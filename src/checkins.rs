//! Check-in runs: a snapshot of a template, asked of a target group until
//! it closes.
//!
//! * Starting a run copies nothing but points at the exact question versions
//!   (which then can't change any more) in their order, and stores the
//!   target as a rule: resolved, against fresh Matrix membership, whenever
//!   someone is notified, starts, continues or submits.
//! * Submitting writes, in one transaction, a participation key
//!   `HMAC(secret, run ‖ Matrix ID)` and a response with a random UUID.
//!   Neither has a timestamp or a row order, and nothing links them: the
//!   database can say *whether* someone answered, not *what*.
//! * Notices, reminders and announcements go out per run in one batch whose
//!   time is stored on the run, never per person, so no "reminded" flag
//!   can tell who had answered by then. What is kept is aggregate: how many
//!   were eligible when it started, how many notices and reminders went
//!   out. The messages themselves are remembered (event → run, no room)
//!   only while the run is open, so their 📊 works.
//! * A deadline is either fixed, or relative to the run's plenum: then it
//!   (and the reminder before it) follows that plenum when it moves, or
//!   when the round moves on to another plenum because one was cancelled.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{
    plenums,
    secrets::Secrets,
    submissions::Identity,
    surveys::{self, QKind, Version},
    time,
};

/// Who a run asks: a rule, not a list of people.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// Everyone who may use the Kummerkasten.
    House,
    /// Members of one authorized room (a floor's or a kitchen's room).
    Room { room_id: String, label: String },
    /// Residents who told us they live on this floor.
    Floor { location_id: i64, label: String },
    /// Just one person: the admin who started it, in test mode
    /// (`[kummerkasten] mode = "test"`).
    Person { mxid: String },
}

impl Target {
    /// `mxid`: who is asking. `rooms`: the authorized rooms they are in
    /// right now (empty: no access at all). `floor`: the floor they told us.
    pub fn matches(&self, mxid: &str, rooms: &[String], floor: Option<i64>) -> bool {
        !rooms.is_empty()
            && match self {
                Target::House => true,
                Target::Room { room_id, .. } => rooms.contains(room_id),
                Target::Floor { location_id, .. } => floor == Some(*location_id),
                Target::Person { mxid: only } => only == mxid,
            }
    }

    pub fn describe(&self) -> String {
        match self {
            Target::House => "👥 Whole house".to_owned(),
            Target::Room { label, .. } => format!("👥 Members of {label}"),
            Target::Floor { label, .. } => format!("🏠 {label}"),
            Target::Person { mxid } => format!("🧪 Only {mxid} (test mode)"),
        }
    }
}

/// One answer while answering (part of the flow's draft).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub value: Option<i64>,
    /// Choice version IDs.
    pub choices: Vec<i64>,
    pub text: Option<String>,
    pub comment: Option<String>,
    pub skipped: bool,
}

impl Answer {
    pub fn is_given(&self) -> bool {
        !self.skipped && (self.value.is_some() || !self.choices.is_empty() || self.text.is_some())
    }
}

/// A run as it was started.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub id: i64,
    pub emoji: String,
    pub title: String,
    pub intro: String,
    pub target: Target,
    pub identities: Vec<Identity>,
    pub plenum_id: Option<i64>,
    pub closes_at: DateTime<Utc>,
    pub reminder_at: Option<DateTime<Utc>>,
    pub announce: bool,
    pub open: bool,
    pub questions: Vec<Version>,
}

impl Run {
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        self.open && now < self.closes_at
    }

    /// "about 2 minutes".
    pub fn duration_hint(&self) -> String {
        let minutes = (self.questions.len() as f64 * 0.5).ceil().max(1.0) as i64;
        format!(
            "about {minutes} minute{}",
            if minutes == 1 { "" } else { "s" }
        )
    }
}

fn identities_text(identities: &[Identity]) -> String {
    identities
        .iter()
        .map(|i| i.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

pub fn parse_identities(s: &str) -> Result<Vec<Identity>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        out.push(match part {
            "anonymous" => Identity::Anonymous,
            "floor" => Identity::Floor,
            "named" | "name" => Identity::Named,
            other => bail!("{other:?} isn't anonymous, floor or named"),
        });
    }
    if out.is_empty() {
        bail!("At least one identity choice, please.");
    }
    Ok(out)
}

pub fn run(conn: &Connection, id: i64) -> Result<Run> {
    #[allow(clippy::type_complexity)]
    let (
        emoji,
        title,
        intro,
        target,
        identities,
        plenum_id,
        closes_at,
        reminder_at,
        announce,
        status,
    ): (
        String,
        String,
        String,
        String,
        String,
        Option<i64>,
        String,
        Option<String>,
        bool,
        String,
    ) = conn
        .query_row(
            "SELECT emoji, title, intro, target, identities, plenum_id, closes_at,
                    reminder_at, announce, status
             FROM survey_runs WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()?
        .with_context(|| format!("There's no check-in #{id}."))?;
    let versions: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT question_version_id FROM survey_run_questions WHERE run_id = ?1 ORDER BY position",
        )?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    Ok(Run {
        id,
        emoji,
        title,
        intro,
        target: serde_json::from_str(&target)?,
        identities: parse_identities(&identities)?,
        plenum_id,
        closes_at: time::parse_text(&closes_at)?,
        reminder_at: reminder_at.map(|t| time::parse_text(&t)).transpose()?,
        announce,
        open: status == "open",
        questions: versions
            .into_iter()
            .map(|v| surveys::version(conn, v))
            .collect::<Result<_>>()?,
    })
}

// ── Starting ────────────────────────────────────────────────────────────────

/// When a run closes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Deadline {
    /// When its plenum starts (if it is for one), else in a week.
    Default,
    /// Exactly then, whatever happens to the plenum.
    Fixed(DateTime<Utc>),
    /// This long before its plenum starts, and moves with it.
    BeforePlenum(Duration),
}

#[derive(Clone, Debug)]
pub struct StartOptions {
    pub target: Target,
    pub deadline: Deadline,
    pub reminder: bool,
    pub announce: bool,
    /// Associate with the next plenum (and its round).
    pub for_plenum: bool,
    pub identities: Vec<Identity>,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            target: Target::House,
            deadline: Deadline::Default,
            reminder: true,
            announce: false,
            for_plenum: true,
            identities: vec![Identity::Anonymous, Identity::Floor, Identity::Named],
        }
    }
}

/// What starting would do: shown to the admin before it happens.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub template: String,
    pub emoji: String,
    pub title: String,
    pub intro: String,
    pub versions: Vec<i64>,
    pub target: Target,
    pub identities: Vec<Identity>,
    pub plenum: Option<(i64, i64, DateTime<Utc>)>,
    pub closes_at: DateTime<Utc>,
    /// Set: the deadline is this long before the plenum (negative) or
    /// after it, and follows it.
    pub plenum_offset: Option<Duration>,
    pub reminder_at: Option<DateTime<Utc>>,
    pub announce: bool,
}

/// Work out a start: the template's active questions (current versions),
/// the next plenum, a closing time (default: when that plenum starts, else
/// in a week) and one reminder `remind_before` the close, if that is still
/// at least an hour after the start.
pub fn plan(
    conn: &Connection,
    template: &str,
    opts: &StartOptions,
    now: DateTime<Utc>,
    remind_before: Duration,
) -> Result<Plan> {
    let (emoji, title, intro): (String, String, String) = conn.query_row(
        "SELECT emoji, title, intro FROM survey_templates WHERE id = ?1",
        [surveys::template_id(conn, template)?],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let versions: Vec<i64> = surveys::questions(conn, template)?
        .into_iter()
        .filter(|q| q.active)
        .map(|q| q.current.id)
        .collect();
    if versions.is_empty() {
        bail!("«{template}» has no questions to ask.");
    }
    for v in &versions {
        let v = surveys::version(conn, *v)?;
        if v.kind.has_choices() && v.choices.len() < 2 {
            bail!("“{}” needs at least two choices.", v.text);
        }
    }
    let plenum = if opts.for_plenum {
        let round = plenums::open_round(conn)?;
        match round {
            Some(round) => {
                let p = plenums::get(conn, round.plenum_id)?.context("round without plenum")?;
                Some((p.id, round.id, p.starts_at))
            }
            None => None,
        }
    } else {
        None
    };
    let (closes_at, plenum_offset) = match (opts.deadline, plenum) {
        (Deadline::Fixed(at), _) => (at, None),
        (Deadline::Default, Some((_, _, at))) => (at, Some(Duration::zero())),
        (Deadline::Default, None) => (now + Duration::days(7), None),
        (Deadline::BeforePlenum(before), Some((_, _, at))) => (at - before, Some(-before)),
        (Deadline::BeforePlenum(_), None) => {
            bail!("A deadline before the plenum needs a plenum. Leave out `no-plenum`.")
        }
    };
    if closes_at <= now + Duration::hours(1) {
        bail!("It would close too soon. Give people at least an hour.");
    }
    let reminder_at = Some(closes_at - remind_before)
        .filter(|at| opts.reminder && *at >= now + Duration::hours(1));
    if opts.announce && central_rooms(conn)?.is_empty() {
        bail!("No central room is set for announcements: `!kk rooms central <!room:server>`.");
    }
    if opts.identities.is_empty() {
        bail!("At least one identity choice, please.");
    }
    Ok(Plan {
        template: template.to_owned(),
        emoji,
        title,
        intro,
        versions,
        target: opts.target.clone(),
        identities: opts.identities.clone(),
        plenum,
        closes_at,
        plenum_offset,
        reminder_at,
        announce: opts.announce,
    })
}

/// Start `plan`. Refused if the template changed since it was planned.
pub fn start(conn: &mut Connection, plan: &Plan, actor: &str, now: DateTime<Utc>) -> Result<i64> {
    let tx = conn.transaction()?;
    let current: Vec<i64> = surveys::questions(&tx, &plan.template)?
        .into_iter()
        .filter(|q| q.active)
        .map(|q| q.current.id)
        .collect();
    if current != plan.versions {
        bail!(
            "«{}» changed in the meantime. Plan the start again.",
            plan.template
        );
    }
    tx.execute(
        "INSERT INTO survey_runs
           (template_id, emoji, title, intro, target, identities, plenum_id, round_id,
            opens_at, closes_at, reminder_at, announce, created_by, created_at,
            deadline_mode, deadline_offset_minutes, reminder_offset_minutes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?9, ?14, ?15, ?16)",
        params![
            surveys::template_id(&tx, &plan.template)?,
            plan.emoji,
            plan.title,
            plan.intro,
            serde_json::to_string(&plan.target)?,
            identities_text(&plan.identities),
            plan.plenum.map(|p| p.0),
            plan.plenum.map(|p| p.1),
            time::to_text(now),
            time::to_text(plan.closes_at),
            plan.reminder_at.map(time::to_text),
            plan.announce,
            actor,
            if plan.plenum_offset.is_some() {
                "relative_to_plenum"
            } else {
                "fixed"
            },
            plan.plenum_offset.map(|o| o.num_minutes()),
            plan.reminder_at
                .map(|at| (plan.closes_at - at).num_minutes()),
        ],
    )?;
    let run = tx.last_insert_rowid();
    for (i, version) in plan.versions.iter().enumerate() {
        tx.execute(
            "INSERT INTO survey_run_questions (run_id, position, question_version_id)
             VALUES (?1, ?2, ?3)",
            params![run, i as i64 + 1, version],
        )?;
    }
    tx.commit()?;
    tracing::info!(run, "Check-in started");
    Ok(run)
}

// ── For residents ───────────────────────────────────────────────────────────

pub fn completed(conn: &Connection, secrets: &Secrets, run: i64, mxid: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM participations WHERE run_id = ?1 AND participation_key = ?2",
            params![run, secrets.participation_key(run, mxid)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Listing {
    pub id: i64,
    pub emoji: String,
    pub title: String,
    pub closes_at: DateTime<Utc>,
    pub open: bool,
    pub completed: bool,
}

/// The check-ins `mxid` is asked (open, or answered in the last two weeks).
pub fn listing(
    conn: &Connection,
    secrets: &Secrets,
    mxid: &str,
    rooms: &[String],
    floor: Option<i64>,
    now: DateTime<Utc>,
) -> Result<Vec<Listing>> {
    let recent = time::to_text(now - Duration::days(14));
    let ids: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT id FROM survey_runs WHERE closes_at > ?1 ORDER BY closes_at, id")?;
        let rows = stmt.query_map([recent], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::new();
    for id in ids {
        let run = run(conn, id)?;
        if !run.target.matches(mxid, rooms, floor) {
            continue;
        }
        let completed = completed(conn, secrets, id, mxid)?;
        let open = run.is_open(now);
        if open || completed {
            out.push(Listing {
                id,
                emoji: run.emoji,
                title: run.title,
                closes_at: run.closes_at,
                open,
                completed,
            });
        }
    }
    Ok(out)
}

/// Whether `mxid` should be told about (or reminded of) `run` now: it is
/// open, they are in its target, and they haven't answered yet.
pub fn should_tell(
    conn: &Connection,
    secrets: &Secrets,
    run: &Run,
    mxid: &str,
    rooms: &[String],
    floor: Option<i64>,
    now: DateTime<Utc>,
) -> Result<bool> {
    Ok(run.is_open(now)
        && run.target.matches(mxid, rooms, floor)
        && !completed(conn, secrets, run.id, mxid)?)
}

#[derive(Debug, PartialEq)]
pub enum Refused {
    Closed,
    NotAsked,
    AlreadyAnswered,
    Incomplete,
}

impl Refused {
    pub fn message(&self) -> &'static str {
        match self {
            Refused::Closed => "⏰ This check-in has closed. Nothing was saved.",
            Refused::NotAsked => "🔒 This check-in isn't for you (any more). Nothing was saved.",
            Refused::AlreadyAnswered => "✅ You've already answered this check-in. Thank you!",
            Refused::Incomplete => "✍️ A required question is still open.",
        }
    }
}

/// Everything to store one response.
pub struct Submission<'a> {
    pub mxid: &'a str,
    /// Authorized rooms they're in right now.
    pub rooms: &'a [String],
    /// The floor they told us (for targeting and floor answers).
    pub floor: Option<i64>,
    pub identity: Identity,
    pub answers: &'a BTreeMap<usize, Answer>,
}

/// Store a response, in the caller's transaction, with the participation
/// key. Checks again that the run is open, meant for them, not answered
/// yet, and that the answers fit the questions.
pub fn submit(
    conn: &Connection,
    secrets: &Secrets,
    run: &Run,
    s: &Submission,
    now: DateTime<Utc>,
) -> Result<Result<(), Refused>> {
    if !run.is_open(now) {
        return Ok(Err(Refused::Closed));
    }
    if !run.target.matches(s.mxid, s.rooms, s.floor) {
        return Ok(Err(Refused::NotAsked));
    }
    if !run.identities.contains(&s.identity) {
        bail!("identity not offered by this run");
    }
    for (i, q) in run.questions.iter().enumerate() {
        let answer = s.answers.get(&i);
        if q.required && !answer.is_some_and(Answer::is_given) {
            return Ok(Err(Refused::Incomplete));
        }
        if let Some(a) = answer.filter(|a| a.is_given()) {
            check_answer(q, a)?;
        }
    }
    let key = secrets.participation_key(run.id, s.mxid);
    let fresh = conn.execute(
        "INSERT OR IGNORE INTO participations (run_id, participation_key) VALUES (?1, ?2)",
        params![run.id, key],
    )?;
    if fresh == 0 {
        return Ok(Err(Refused::AlreadyAnswered));
    }
    let (mxid, floor) = match s.identity {
        Identity::Anonymous => (None, None),
        Identity::Floor => (None, Some(s.floor.context("Which floor?")?)),
        Identity::Named => (Some(s.mxid), None),
    };
    let response = uuid();
    conn.execute(
        "INSERT INTO responses (id, run_id, identity, respondent_mxid, respondent_floor_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![response, run.id, s.identity.as_str(), mxid, floor],
    )?;
    for (i, q) in run.questions.iter().enumerate() {
        let a = s.answers.get(&i).cloned().unwrap_or(Answer {
            skipped: true,
            ..Default::default()
        });
        let skipped = !a.is_given();
        conn.execute(
            "INSERT INTO answers (response_id, question_version_id, value, text, comment, skipped)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                response,
                q.id,
                a.value.filter(|_| !skipped),
                a.text.as_deref().filter(|_| !skipped),
                a.comment.as_deref(),
                skipped
            ],
        )?;
        if !skipped {
            for choice in &a.choices {
                conn.execute(
                    "INSERT INTO answer_choices (response_id, question_version_id, choice_version_id)
                     VALUES (?1, ?2, ?3)",
                    params![response, q.id, choice],
                )?;
            }
        }
    }
    Ok(Ok(()))
}

fn check_answer(q: &Version, a: &Answer) -> Result<()> {
    match q.kind {
        QKind::Scale => {
            let v = a.value.context("a scale needs a value")?;
            if !q.config.steps().contains(&v) {
                bail!("value out of range");
            }
        }
        QKind::YesNo => {
            if !matches!(a.value, Some(0 | 1)) {
                bail!("yes/no needs 1 or 0");
            }
        }
        QKind::Single | QKind::Multiple => {
            if a.choices.is_empty() || (q.kind == QKind::Single && a.choices.len() > 1) {
                bail!("wrong number of choices");
            }
            if !a
                .choices
                .iter()
                .all(|c| q.choices.iter().any(|qc| qc.version_id == *c))
            {
                bail!("choice from another question");
            }
        }
        QKind::Text => {
            if a.text.as_deref().is_none_or(|t| t.trim().is_empty()) {
                bail!("text answer is empty");
            }
        }
    }
    Ok(())
}

/// A random (version 4) UUID.
fn uuid() -> String {
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

// ── Deadlines that follow their plenum ──────────────────────────────────────

/// `(id, opens_at, offset minutes, reminder offset, reminder_at, reminded_at)`.
type RelativeRun = (
    i64,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
);

/// Open runs of `plenum_id` whose deadline is relative to it: close (and
/// remind) by its current date. Fixed deadlines don't move. A reminder
/// that already went out stays sent; one that would now fall before the
/// run opened is dropped. Idempotent.
pub fn follow_plenum(conn: &Connection, plenum_id: i64, now: DateTime<Utc>) -> Result<usize> {
    let Some(plenum) = plenums::get(conn, plenum_id)? else {
        return Ok(0);
    };
    let runs: Vec<RelativeRun> = {
        let mut stmt = conn.prepare(
            "SELECT id, opens_at, deadline_offset_minutes, reminder_offset_minutes,
                    reminder_at, reminded_at
             FROM survey_runs
             WHERE plenum_id = ?1 AND status = 'open' AND deadline_mode = 'relative_to_plenum'",
        )?;
        let rows = stmt.query_map([plenum_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut moved = 0;
    for (id, opens, offset, remind_offset, reminder_at, reminded_at) in runs {
        let opens = time::parse_text(&opens)?;
        // Never at or before its own start (the table insists). A deadline
        // that is past now closes the run at the next tick, as it should.
        let closes =
            (plenum.starts_at + Duration::minutes(offset)).max(opens + Duration::minutes(1));
        let _ = now;
        let reminder = match reminded_at {
            // Sent: it stays what it was, if that still fits before the close.
            Some(_) => reminder_at
                .map(|t| time::parse_text(&t))
                .transpose()?
                .filter(|at| *at < closes),
            None => remind_offset
                .map(|m| closes - Duration::minutes(m))
                .filter(|at| *at > opens && *at < closes),
        };
        moved += conn.execute(
            "UPDATE survey_runs SET closes_at = ?2, reminder_at = ?3 WHERE id = ?1",
            params![id, time::to_text(closes), reminder.map(time::to_text)],
        )?;
    }
    Ok(moved)
}

/// The round `round_id` now belongs to `plenum_id` (the one before was
/// cancelled): so do its open check-ins, and their relative deadlines.
pub fn follow_round(
    conn: &Connection,
    round_id: i64,
    plenum_id: i64,
    now: DateTime<Utc>,
) -> Result<usize> {
    conn.execute(
        "UPDATE survey_runs SET plenum_id = ?2 WHERE round_id = ?1 AND status = 'open'",
        params![round_id, plenum_id],
    )?;
    follow_plenum(conn, plenum_id, now)
}

// ── Delivery batches (per run, never per person) ────────────────────────────

/// Close runs whose time is up. Returns how many.
pub fn close_due(conn: &Connection, now: DateTime<Utc>) -> Result<usize> {
    let n = conn.execute(
        "UPDATE survey_runs SET status = 'closed' WHERE status = 'open' AND closes_at <= ?1",
        [time::to_text(now)],
    )?;
    conn.execute(
        "DELETE FROM checkin_messages
         WHERE run_id IN (SELECT id FROM survey_runs WHERE status = 'closed')",
        [],
    )?;
    Ok(n)
}

pub fn close(conn: &Connection, id: i64, now: DateTime<Utc>) -> Result<bool> {
    let closed = conn.execute(
        "UPDATE survey_runs SET status = 'closed', closes_at = min(closes_at, ?2),
                reminder_at = CASE WHEN reminder_at < ?2 THEN reminder_at END
         WHERE id = ?1 AND status = 'open'",
        params![id, time::to_text(now + Duration::seconds(1))],
    )? == 1;
    conn.execute("DELETE FROM checkin_messages WHERE run_id = ?1", [id])?;
    Ok(closed)
}

/// Mark due batches of one kind as sent and return their runs: the time is
/// set *before* sending, so a crash means a missed message, never a second.
fn take(conn: &Connection, column: &str, due: &str, now: DateTime<Utc>) -> Result<Vec<i64>> {
    let now = time::to_text(now);
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT id FROM survey_runs
             WHERE status = 'open' AND closes_at > ?1 AND {column} IS NULL AND {due}
             ORDER BY id"
        ))?;
        let rows = stmt.query_map([&now], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for id in &ids {
        conn.execute(
            &format!("UPDATE survey_runs SET {column} = ?2 WHERE id = ?1"),
            params![id, now],
        )?;
    }
    Ok(ids)
}

/// Open runs whose notice batch went out already (someone joining their
/// chat later gets their notice then).
pub fn announced_open_runs(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM survey_runs
         WHERE status = 'open' AND closes_at > ?1 AND notified_at IS NOT NULL ORDER BY id",
    )?;
    let rows = stmt.query_map([time::to_text(now)], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Runs whose start notice is due (once each).
pub fn take_notices(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<i64>> {
    take(conn, "notified_at", "opens_at <= ?1", now)
}

/// Runs whose one reminder is due (once each; none if not configured).
pub fn take_reminders(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<i64>> {
    take(
        conn,
        "reminded_at",
        "reminder_at IS NOT NULL AND reminder_at <= ?1",
        now,
    )
}

/// Runs to announce in the central room (only those set to).
pub fn take_announcements(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<i64>> {
    take(conn, "announced_at", "announce = 1", now)
}

pub fn central_rooms(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT room_id FROM authorized_rooms WHERE active = 1 AND central = 1")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn set_central(conn: &Connection, room_id: &str, central: bool) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE authorized_rooms SET central = ?2 WHERE room_id = ?1 AND active = 1",
        params![room_id, central],
    )? == 1)
}

/// A notice or reminder went out as `event_id` (no room: see the module).
pub fn add_message(conn: &Connection, event_id: &str, run: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO checkin_messages (event_id, run_id) VALUES (?1, ?2)",
        params![event_id, run],
    )?;
    Ok(())
}

/// The run whose notice or reminder `event_id` is, while that run is open.
pub fn message_run(conn: &Connection, event_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT run_id FROM checkin_messages WHERE event_id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Which batch went out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Batch {
    Notice,
    Reminder,
}

/// Count a batch (aggregate only): how many were told, and for the start
/// notice how many were eligible then (the response-rate denominator).
pub fn count_batch(
    conn: &Connection,
    run: i64,
    batch: Batch,
    eligible: usize,
    sent: usize,
) -> Result<()> {
    match batch {
        Batch::Notice => conn.execute(
            "UPDATE survey_runs SET notices_sent = notices_sent + ?2,
                    eligible_at_launch = coalesce(eligible_at_launch, nullif(?3, 0))
             WHERE id = ?1",
            params![run, sent as i64, eligible as i64],
        )?,
        Batch::Reminder => conn.execute(
            "UPDATE survey_runs SET reminders_sent = reminders_sent + ?2 WHERE id = ?1",
            params![run, sent as i64],
        )?,
    };
    Ok(())
}

/// A run in the admin overview, with a count of answers, nothing more.
pub struct RunRow {
    pub id: i64,
    pub emoji: String,
    pub title: String,
    pub open: bool,
    pub closes_at: DateTime<Utc>,
    pub responses: i64,
}

/// For admins: the latest runs.
pub fn overview(conn: &Connection) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.emoji, r.title, r.status = 'open', r.closes_at,
                (SELECT count(*) FROM responses s WHERE s.run_id = r.id)
         FROM survey_runs r ORDER BY r.id DESC LIMIT 10",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get::<_, String>(4)?,
            r.get(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, emoji, title, open, closes, responses) = row?;
        out.push(RunRow {
            id,
            emoji,
            title,
            open,
            closes_at: time::parse_text(&closes)?,
            responses,
        });
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        plenums::tests::{at, with_default_rule},
        surveys::{edit, Edit},
    };

    pub const HOUSE: &str = "!house:x";

    pub fn rooms() -> Vec<String> {
        vec![HOUSE.to_owned()]
    }

    pub fn started(conn: &mut Connection, now: DateTime<Utc>, opts: StartOptions) -> Run {
        let plan = plan(conn, "cleaning", &opts, now, Duration::hours(24)).unwrap();
        let id = start(conn, &plan, "@admin:x", now).unwrap();
        run(conn, id).unwrap()
    }

    /// Answers for the starting cleaning template: 😐 3, the first problem.
    pub fn answers(run: &Run) -> BTreeMap<usize, Answer> {
        BTreeMap::from([
            (
                0,
                Answer {
                    value: Some(3),
                    ..Default::default()
                },
            ),
            (
                1,
                Answer {
                    choices: vec![run.questions[1].choices[0].version_id],
                    ..Default::default()
                },
            ),
            (
                2,
                Answer {
                    skipped: true,
                    ..Default::default()
                },
            ),
        ])
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_run_is_a_snapshot_the_template_can_no_longer_change() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let run1 = started(&mut conn, now, StartOptions::default());
        assert_eq!(run1.questions.len(), 3);
        // For the next plenum, closing when it starts, one reminder a day before.
        let next = plenums::next(&conn).unwrap().unwrap();
        assert_eq!(run1.plenum_id, Some(next.id));
        assert_eq!(run1.closes_at, next.starts_at);
        assert_eq!(run1.reminder_at, Some(next.starts_at - Duration::hours(24)));
        // Rewording a used question and a used choice: new version, same choice.
        let e = edit(
            &mut conn,
            "cleaning",
            1,
            Edit::Text("How satisfied are you with cleaning?".into()),
        )
        .unwrap();
        assert!(e.new_version.is_some());
        edit(
            &mut conn,
            "cleaning",
            2,
            Edit::RenameChoice {
                n: 1,
                label: "Cleaning turns are frequently missed".into(),
            },
        )
        .unwrap();
        let old = run(&conn, run1.id).unwrap();
        assert_eq!(old, run1, "the old run reads exactly as before");
        assert_eq!(
            old.questions[0].text,
            "How well is cleaning currently working?"
        );
        assert_eq!(
            old.questions[1].choices[0].label,
            "Cleaning turns are skipped"
        );
        // The next run asks the new wording: measuring the same metric,
        // and the reworded choice still stands for the same problem.
        let run2 = started(&mut conn, now, StartOptions::default());
        assert_eq!(
            run2.questions[0].text,
            "How satisfied are you with cleaning?"
        );
        assert_eq!(run2.questions[0].metric, run1.questions[0].metric);
        let (c1, c2) = (&run1.questions[1].choices[0], &run2.questions[1].choices[0]);
        assert_eq!(c1.choice_id, c2.choice_id);
        assert_ne!(c1.version_id, c2.version_id);
        assert_eq!(c2.problem.as_deref(), Some("cleaning.skipped_turns"));
        // The database itself refuses to touch what was asked.
        assert!(conn
            .execute(
                "UPDATE question_versions SET text = 'x' WHERE id = ?1",
                [run1.questions[0].id]
            )
            .is_err());
        assert!(conn
            .execute(
                "UPDATE choice_versions SET label = 'x' WHERE id = ?1",
                [c1.version_id]
            )
            .is_err());
        assert!(conn
            .execute(
                "DELETE FROM survey_run_questions WHERE run_id = ?1",
                [run1.id]
            )
            .is_err());
        // A disabled question isn't asked in new runs.
        surveys::set_active(&conn, "cleaning", 3, false).unwrap();
        assert_eq!(
            started(&mut conn, now, StartOptions::default())
                .questions
                .len(),
            2
        );
    }

    #[test]
    fn targets_are_rules_resolved_against_current_membership() {
        let floor2 = 7;
        let house = Target::House;
        let kitchen = Target::Room {
            room_id: "!kitchen2:x".into(),
            label: "Kitchen 2".into(),
        };
        let floor = Target::Floor {
            location_id: floor2,
            label: "2nd floor".into(),
        };
        let none: Vec<String> = vec![];
        let both = vec![HOUSE.to_owned(), "!kitchen2:x".to_owned()];
        let me = "@a:x";
        assert!(house.matches(me, &rooms(), None));
        assert!(!house.matches(me, &none, None), "no access, nothing");
        assert!(kitchen.matches(me, &both, None));
        assert!(!kitchen.matches(me, &rooms(), None));
        assert!(floor.matches(me, &rooms(), Some(floor2)));
        assert!(!floor.matches(me, &rooms(), Some(floor2 + 1)));
        assert!(!floor.matches(me, &none, Some(floor2)));
        // Test mode: only the admin who started it, and only with access.
        let only_me = Target::Person { mxid: me.into() };
        assert!(only_me.matches(me, &rooms(), None));
        assert!(!only_me.matches("@b:x", &rooms(), None));
        assert!(!only_me.matches(me, &none, None));
    }

    #[test]
    fn a_person_target_round_trips_through_the_database_json() {
        let target = Target::Person {
            mxid: "@a:x".into(),
        };
        let json = serde_json::to_string(&target).unwrap();
        assert_eq!(json, r#"{"kind":"person","mxid":"@a:x"}"#);
        assert_eq!(serde_json::from_str::<Target>(&json).unwrap(), target);
    }

    #[test]
    fn anonymous_floor_and_named_responses() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let run = started(&mut conn, now, StartOptions::default());
        let floor: i64 = conn
            .query_row("SELECT id FROM locations WHERE key = 'floor.2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let a = answers(&run);
        for (who, identity) in [
            ("@anon:x", Identity::Anonymous),
            ("@floor:x", Identity::Floor),
            ("@named:x", Identity::Named),
        ] {
            let s = Submission {
                mxid: who,
                rooms: &rooms(),
                floor: Some(floor),
                identity,
                answers: &a,
            };
            assert_eq!(submit(&conn, &secrets, &run, &s, now).unwrap(), Ok(()));
        }
        assert_eq!(count(&conn, "SELECT count(*) FROM responses WHERE identity = 'anonymous' AND respondent_mxid IS NULL AND respondent_floor_id IS NULL"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM responses WHERE identity = 'floor' AND respondent_mxid IS NULL AND respondent_floor_id IS NOT NULL"), 1);
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM responses WHERE respondent_mxid = '@named:x'"
            ),
            1
        );
        // Neither anonymous nor floor participants appear anywhere in the clear.
        let everything: String = conn
            .query_row(
                "SELECT group_concat(coalesce(respondent_mxid, ''), ',') FROM responses",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!everything.contains("@anon:x") && !everything.contains("@floor:x"));
        // The schema refuses an ID on an anonymous response.
        assert!(conn
            .execute(
                "UPDATE responses SET respondent_mxid = '@anon:x' WHERE identity = 'anonymous'",
                []
            )
            .is_err());
        // Answers: 3 per response, with the chosen problem.
        assert_eq!(count(&conn, "SELECT count(*) FROM answers"), 9);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM answers WHERE skipped = 1"),
            3
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM answer_choices"), 3);
    }

    #[test]
    fn one_answer_per_person_without_a_link_to_the_answer() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let run = started(&mut conn, now, StartOptions::default());
        let a = answers(&run);
        let s = Submission {
            mxid: "@a:x",
            rooms: &rooms(),
            floor: None,
            identity: Identity::Anonymous,
            answers: &a,
        };
        assert!(!completed(&conn, &secrets, run.id, "@a:x").unwrap());
        assert_eq!(submit(&conn, &secrets, &run, &s, now).unwrap(), Ok(()));
        assert!(completed(&conn, &secrets, run.id, "@a:x").unwrap());
        assert_eq!(
            submit(&conn, &secrets, &run, &s, now).unwrap(),
            Err(Refused::AlreadyAnswered)
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM responses"), 1);
        // The participation table knows only run + key; no column, key or
        // foreign key leads from it to a response (or back).
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('participations')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(columns, ["run_id", "participation_key"]);
        let fks: Vec<String> = conn
            .prepare("SELECT \"table\" FROM pragma_foreign_key_list('participations')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(fks, ["survey_runs"]);
        let fks: Vec<String> = conn
            .prepare("SELECT \"table\" FROM pragma_foreign_key_list('responses')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!fks.contains(&"participations".to_owned()));
        // No hidden insertion order: neither table has a rowid.
        assert!(conn
            .query_row("SELECT rowid FROM participations", [], |r| r
                .get::<_, i64>(0))
            .is_err());
        assert!(conn
            .query_row("SELECT rowid FROM responses", [], |r| r.get::<_, i64>(0))
            .is_err());
        let id: String = conn
            .query_row("SELECT id FROM responses", [], |r| r.get(0))
            .unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }

    #[test]
    fn answers_must_fit_and_required_ones_must_be_there() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let run = started(&mut conn, now, StartOptions::default());
        fn sub<'a>(rooms: &'a [String], a: &'a BTreeMap<usize, Answer>) -> Submission<'a> {
            Submission {
                mxid: "@a:x",
                rooms,
                floor: None,
                identity: Identity::Anonymous,
                answers: a,
            }
        }
        let mut a = answers(&run);
        a.remove(&0);
        // Not asked (no access any more): refused before anything else.
        assert_eq!(
            submit(&conn, &secrets, &run, &sub(&[], &a), now).unwrap(),
            Err(Refused::NotAsked)
        );
        let r = rooms();
        assert_eq!(
            submit(&conn, &secrets, &run, &sub(&r, &a), now).unwrap(),
            Err(Refused::Incomplete)
        );
        a.insert(
            0,
            Answer {
                value: Some(9),
                ..Default::default()
            },
        );
        assert!(submit(&conn, &secrets, &run, &sub(&r, &a), now).is_err());
        a.insert(
            0,
            Answer {
                value: Some(5),
                ..Default::default()
            },
        );
        a.insert(
            1,
            Answer {
                choices: vec![999_999],
                ..Default::default()
            },
        );
        assert!(submit(&conn, &secrets, &run, &sub(&r, &a), now).is_err());
        // Closed: refused.
        let fine = answers(&run);
        assert_eq!(
            submit(&conn, &secrets, &run, &sub(&r, &fine), run.closes_at).unwrap(),
            Err(Refused::Closed)
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM participations"), 0);
    }

    #[test]
    fn notices_reminders_and_announcements_go_out_once_and_only_as_configured() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let quiet = started(
            &mut conn,
            now,
            StartOptions {
                reminder: false,
                ..Default::default()
            },
        );
        // Announcing needs a central room.
        assert!(plan(
            &conn,
            "cleaning",
            &StartOptions {
                announce: true,
                ..Default::default()
            },
            now,
            Duration::hours(24)
        )
        .is_err());
        crate::store::add_room(&conn, HOUSE, "House", "@admin:x").unwrap();
        assert!(set_central(&conn, HOUSE, true).unwrap());
        let loud = started(
            &mut conn,
            now,
            StartOptions {
                announce: true,
                ..Default::default()
            },
        );

        assert_eq!(take_notices(&conn, now).unwrap(), [quiet.id, loud.id]);
        assert!(take_notices(&conn, now).unwrap().is_empty());
        assert_eq!(take_announcements(&conn, now).unwrap(), [loud.id]);
        assert!(take_announcements(&conn, now).unwrap().is_empty());
        // The reminder: not before its time, once, and not for the run without one.
        assert!(take_reminders(&conn, now).unwrap().is_empty());
        let later = loud.reminder_at.unwrap();
        assert_eq!(take_reminders(&conn, later).unwrap(), [loud.id]);
        assert!(take_reminders(&conn, later + Duration::hours(1))
            .unwrap()
            .is_empty());
        // Who gets it: in the target and not answered yet.
        let a = answers(&loud);
        let s = Submission {
            mxid: "@done:x",
            rooms: &rooms(),
            floor: None,
            identity: Identity::Anonymous,
            answers: &a,
        };
        submit(&conn, &secrets, &loud, &s, now).unwrap().unwrap();
        assert!(!should_tell(&conn, &secrets, &loud, "@done:x", &rooms(), None, later).unwrap());
        assert!(should_tell(&conn, &secrets, &loud, "@open:x", &rooms(), None, later).unwrap());
        assert!(!should_tell(&conn, &secrets, &loud, "@gone:x", &[], None, later).unwrap());
        // Closing: at its time, and its buttons stop working.
        add_message(&conn, "$notice", loud.id).unwrap();
        assert_eq!(close_due(&conn, loud.closes_at).unwrap(), 2);
        assert_eq!(message_run(&conn, "$notice").unwrap(), None);
        assert!(!run(&conn, loud.id).unwrap().is_open(now));
    }

    #[test]
    fn the_menu_lists_open_and_answered_check_ins() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let run = started(&mut conn, now, StartOptions::default());
        let kitchen_only = started(
            &mut conn,
            now,
            StartOptions {
                target: Target::Room {
                    room_id: "!kitchen:x".into(),
                    label: "Kitchen".into(),
                },
                ..Default::default()
            },
        );
        let list = listing(&conn, &secrets, "@a:x", &rooms(), None, now).unwrap();
        assert_eq!(
            list.iter().map(|l| (l.id, l.completed)).collect::<Vec<_>>(),
            [(run.id, false)]
        );
        let a = answers(&run);
        let s = Submission {
            mxid: "@a:x",
            rooms: &rooms(),
            floor: None,
            identity: Identity::Named,
            answers: &a,
        };
        submit(&conn, &secrets, &run, &s, now).unwrap().unwrap();
        let list = listing(&conn, &secrets, "@a:x", &rooms(), None, now).unwrap();
        assert!(list[0].completed);
        // Closed and answered: still shown as completed for a while.
        close(&conn, run.id, now).unwrap();
        let list = listing(
            &conn,
            &secrets,
            "@a:x",
            &rooms(),
            None,
            now + Duration::hours(1),
        )
        .unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].completed && !list[0].open);
        let _ = kitchen_only;
    }
}
