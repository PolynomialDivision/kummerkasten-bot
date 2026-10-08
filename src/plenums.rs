//! Concrete plenums and rounds in the database.
//!
//! recurrence rules (`plenum_rules`, versioned)
//!   → generated plenums (`plenums`, one row per date, stable ID)
//!   → per-occurrence changes on that row (moved, cancelled, completed)
//!
//! A generated row is never generated twice (`UNIQUE (rule_id, rule_date)`),
//! so a moved or cancelled occurrence stays that way. Past plenums are never
//! recomputed: a rule change only replaces future occurrences nobody moved.
//!
//! Invariant kept by [`ensure_round`]: the one open round belongs to the
//! earliest scheduled plenum, and the "delete after the next plenum"
//! submissions of that round point at the same plenum.
//!
//! Moving a plenum (even at short notice) is a normal operation on that
//! row: same ID, so its round, its "delete after" submissions, check-ins,
//! measures and announcements stay with it; `original_starts_at` keeps the
//! first date. A move never changes the order of scheduled plenums (that
//! would hand the round to another one) — cancel instead. A plenum whose
//! time has passed is *awaiting completion*, not completed: only an admin
//! (or the explicit auto-complete policy) completes it.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    plenum::Recurrence,
    time::{self, Zone},
};

/// How far ahead generated plenums exist as rows.
pub const HORIZON_DAYS: i64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Scheduled,
    Cancelled,
    Completed,
}

impl Status {
    fn parse(s: &str) -> Self {
        match s {
            "cancelled" => Status::Cancelled,
            "completed" => Status::Completed,
            _ => Status::Scheduled,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Status::Scheduled => "🗓️ scheduled",
            Status::Cancelled => "⏭️ cancelled",
            Status::Completed => "✅ completed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plenum {
    pub id: i64,
    pub rule_id: Option<i64>,
    pub rule_date: Option<NaiveDate>,
    /// When it is (now): what everything follows.
    pub starts_at: DateTime<Utc>,
    pub status: Status,
    pub rescheduled: bool,
    pub completed_at: Option<DateTime<Utc>>,
    /// When it was first planned; never changes.
    pub original_starts_at: DateTime<Utc>,
    pub reschedule_reason: Option<String>,
    /// When its day-before summary went out.
    pub announced_at: Option<DateTime<Utc>>,
}

impl Plenum {
    /// Its time has come, but nobody said it happened: the round stays
    /// open and nothing is cleaned up until it is completed.
    pub fn awaiting_completion(&self, now: DateTime<Utc>) -> bool {
        self.status == Status::Scheduled && self.starts_at <= now
    }
}

const COLUMNS: &str = "id, rule_id, rule_date, starts_at, status, rescheduled, completed_at,
                       coalesce(original_starts_at, starts_at), reschedule_reason, announced_at";

fn plenum(r: &Row) -> rusqlite::Result<Plenum> {
    let text = |i: usize| -> rusqlite::Result<Option<String>> { r.get(i) };
    let bad = |i: usize, e: anyhow::Error| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, e.into())
    };
    Ok(Plenum {
        id: r.get(0)?,
        rule_id: r.get(1)?,
        rule_date: text(2)?
            .map(|s| time::parse_date(&s))
            .transpose()
            .map_err(|e| bad(2, e))?,
        starts_at: time::parse_text(&r.get::<_, String>(3)?).map_err(|e| bad(3, e))?,
        status: Status::parse(&r.get::<_, String>(4)?),
        rescheduled: r.get(5)?,
        completed_at: text(6)?
            .map(|s| time::parse_text(&s))
            .transpose()
            .map_err(|e| bad(6, e))?,
        original_starts_at: time::parse_text(&r.get::<_, String>(7)?).map_err(|e| bad(7, e))?,
        reschedule_reason: r.get(8)?,
        announced_at: text(9)?
            .map(|s| time::parse_text(&s))
            .transpose()
            .map_err(|e| bad(9, e))?,
    })
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Plenum>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM plenums WHERE id = ?1"),
            [id],
            plenum,
        )
        .optional()?)
}

fn query(conn: &Connection, sql_tail: &str, args: impl rusqlite::Params) -> Result<Vec<Plenum>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM plenums {sql_tail}"))?;
    let rows = stmt.query_map(args, plenum)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The next plenum: the earliest one still scheduled (also one whose day
/// is today, or already past while it waits to be completed).
pub fn next(conn: &Connection) -> Result<Option<Plenum>> {
    Ok(upcoming(conn, 1)?.into_iter().next())
}

/// The next `limit` scheduled plenums, in order — `[1]` is the one after next.
pub fn upcoming(conn: &Connection, limit: usize) -> Result<Vec<Plenum>> {
    query(
        conn,
        "WHERE status = 'scheduled' ORDER BY starts_at, id LIMIT ?1",
        [limit as i64],
    )
}

/// The latest completed plenum.
pub fn previous(conn: &Connection) -> Result<Option<Plenum>> {
    Ok(query(
        conn,
        "WHERE status = 'completed' ORDER BY starts_at DESC, id DESC LIMIT 1",
        [],
    )?
    .into_iter()
    .next())
}

/// Plenums from `from` on (any status), for the admin overview.
pub fn listing(conn: &Connection, from: DateTime<Utc>, limit: usize) -> Result<Vec<Plenum>> {
    query(
        conn,
        "WHERE starts_at >= ?1 ORDER BY starts_at, id LIMIT ?2",
        params![time::to_text(from), limit as i64],
    )
}

// ── Rules ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Rule {
    pub id: i64,
    pub recurrence: Recurrence,
    pub effective_from: NaiveDate,
}

pub fn rules(conn: &Connection) -> Result<Vec<Rule>> {
    let mut stmt = conn.prepare(
        "SELECT id, recurrence, effective_from FROM plenum_rules ORDER BY effective_from, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, json, from) = row?;
        out.push(Rule {
            id,
            recurrence: serde_json::from_str(&json)
                .with_context(|| format!("Reading plenum rule {id}"))?,
            effective_from: time::parse_date(&from)?,
        });
    }
    Ok(out)
}

/// The rule version in force on `day`.
pub fn rule_on(conn: &Connection, day: NaiveDate) -> Result<Option<Rule>> {
    Ok(rules(conn)?
        .into_iter()
        .rev()
        .find(|r| r.effective_from <= day))
}

/// Store `default` as the rule when there is none yet (first start).
pub fn ensure_rule(conn: &Connection, default: &Recurrence, today: NaiveDate) -> Result<bool> {
    let any: i64 = conn.query_row("SELECT count(*) FROM plenum_rules", [], |r| r.get(0))?;
    if any > 0 {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO plenum_rules (recurrence, effective_from, created_by, created_at)
         VALUES (?1, ?2, NULL, ?3)",
        params![
            serde_json::to_string(default)?,
            time::date_text(today),
            time::now_text()
        ],
    )?;
    Ok(true)
}

/// Create the rows of every date the rules propose from today until
/// [`HORIZON_DAYS`] ahead. Never touches existing rows, never creates past
/// ones. Returns how many were created.
pub fn materialize(conn: &Connection, zone: Zone, now: DateTime<Utc>) -> Result<usize> {
    let today = zone.date(now);
    let end = today + Duration::days(HORIZON_DAYS);
    let rules = rules(conn)?;
    let mut created = 0;
    for (i, rule) in rules.iter().enumerate() {
        let until = rules.get(i + 1).map_or(end, |next| {
            (next.effective_from - Duration::days(1)).min(end)
        });
        let from = rule.effective_from.max(today);
        for date in rule.recurrence.dates(from, until) {
            let starts_at = zone.at(date, rule.recurrence.time());
            created += conn.execute(
                "INSERT OR IGNORE INTO plenums
                   (rule_id, rule_date, starts_at, original_starts_at, status, created_at)
                 VALUES (?1, ?2, ?3, ?3, 'scheduled', ?4)",
                params![
                    rule.id,
                    time::date_text(date),
                    time::to_text(starts_at),
                    time::to_text(now)
                ],
            )?;
        }
    }
    Ok(created)
}

#[derive(Debug, PartialEq)]
pub struct RuleChange {
    pub rule_id: i64,
    /// Future occurrences of the old rule that gave way.
    pub replaced: usize,
    /// Occurrences an admin moved by hand, kept as they are.
    pub kept: usize,
}

/// A new rule version from `from` on (not before tomorrow, so today's
/// plenum and the past stay put). Generated occurrences from that day on
/// that nobody moved are replaced by the new rule's; moved ones stay.
pub fn set_rule(
    conn: &mut Connection,
    recurrence: &Recurrence,
    from: NaiveDate,
    actor: &str,
    zone: Zone,
    now: DateTime<Utc>,
) -> Result<RuleChange> {
    if from <= zone.date(now) {
        bail!("A new rhythm can start tomorrow at the earliest.");
    }
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO plenum_rules (recurrence, effective_from, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            serde_json::to_string(recurrence)?,
            time::date_text(from),
            actor,
            time::to_text(now)
        ],
    )?;
    let rule_id = tx.last_insert_rowid();
    // Step aside first (so the open round can move on), delete afterwards.
    let replaced = tx.execute(
        "UPDATE plenums SET status = 'cancelled'
         WHERE rule_id IS NOT NULL AND rule_id <> ?1 AND rule_date >= ?2
           AND rescheduled = 0 AND status IN ('scheduled', 'cancelled')",
        params![rule_id, time::date_text(from)],
    )?;
    let ids: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM plenums
             WHERE rule_id IS NOT NULL AND rule_id <> ?1 AND rule_date >= ?2
               AND rescheduled = 0 AND status = 'cancelled'",
        )?;
        let rows = stmt.query_map(params![rule_id, time::date_text(from)], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let kept: i64 = tx.query_row(
        "SELECT count(*) FROM plenums
         WHERE rule_id IS NOT NULL AND rule_id <> ?1 AND rule_date >= ?2
           AND rescheduled = 1 AND status = 'scheduled'",
        params![rule_id, time::date_text(from)],
        |r| r.get(0),
    )?;
    materialize(&tx, zone, now)?;
    ensure_round(&tx, now)?;
    for id in ids {
        // Only if nothing points at it any more.
        tx.execute(
            "DELETE FROM plenums WHERE id = ?1
               AND NOT EXISTS (SELECT 1 FROM rounds WHERE plenum_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM submissions WHERE delete_after_plenum_id = ?1)",
            [id],
        )?;
    }
    audit(
        &tx,
        None,
        "rule",
        &format!(
            "from {}: {} (replaced {replaced}, kept {kept} moved)",
            time::date_text(from),
            recurrence.describe()
        ),
        actor,
        now,
    )?;
    tx.commit()?;
    Ok(RuleChange {
        rule_id,
        replaced,
        kept: kept as usize,
    })
}

// ── Single occurrences ──────────────────────────────────────────────────────

fn audit(
    conn: &Connection,
    plenum_id: Option<i64>,
    action: &str,
    detail: &str,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO plenum_changes (plenum_id, action, detail, actor, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![plenum_id, action, detail, actor, time::to_text(now)],
    )?;
    Ok(())
}

fn scheduled(conn: &Connection, id: i64) -> Result<Plenum> {
    let p = get(conn, id)?.with_context(|| format!("There is no plenum #{id}."))?;
    if p.status != Status::Scheduled {
        bail!("Plenum #{id} is {}.", p.status.label());
    }
    Ok(p)
}

fn not_in_past(zone: Zone, now: DateTime<Utc>, at: DateTime<Utc>) -> Result<()> {
    if zone.date(at) < zone.date(now) {
        bail!("That day is already over.");
    }
    Ok(())
}

/// An extra plenum besides the rule's.
pub fn add(
    conn: &mut Connection,
    starts_at: DateTime<Utc>,
    actor: &str,
    zone: Zone,
    now: DateTime<Utc>,
) -> Result<i64> {
    not_in_past(zone, now, starts_at)?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO plenums (rule_id, rule_date, starts_at, original_starts_at, status, created_at)
         VALUES (NULL, NULL, ?1, ?1, 'scheduled', ?2)",
        params![time::to_text(starts_at), time::to_text(now)],
    )?;
    let id = tx.last_insert_rowid();
    audit(&tx, Some(id), "add", &time::to_text(starts_at), actor, now)?;
    ensure_round(&tx, now)?;
    tx.commit()?;
    Ok(id)
}

/// A move later than this after the day-before summary went out gets a
/// fresh summary the day before its new date (besides the short update);
/// a move by a day or two only the update.
pub const FRESH_SUMMARY_AFTER: Duration = Duration::hours(72);

/// Move one occurrence — at any notice, even on its day or after its time
/// passed without it happening. Same row, same ID: its round, "delete
/// after" submissions, check-ins and everything else stay with it, and
/// what follows its date (check-in deadlines set relative to it, the
/// day-before summary, completion) follows the new one. The rule and every
/// other plenum stay as they are. A move may not pass another scheduled
/// plenum (cancel one instead).
pub fn reschedule(
    conn: &mut Connection,
    id: i64,
    starts_at: DateTime<Utc>,
    reason: Option<&str>,
    actor: &str,
    zone: Zone,
    now: DateTime<Utc>,
) -> Result<()> {
    not_in_past(zone, now, starts_at)?;
    let tx = conn.transaction()?;
    let before = scheduled(&tx, id)?;
    let others: Vec<Plenum> = upcoming(&tx, 1000)?
        .into_iter()
        .filter(|p| p.id != id)
        .collect();
    let earlier = others.iter().rfind(|p| comes_before(p, &before));
    let later = others.iter().find(|p| !comes_before(p, &before));
    if let Some(p) = earlier.filter(|p| starts_at <= p.starts_at) {
        bail!(
            "That would put it before plenum #{} on {} — move or cancel that one instead.",
            p.id,
            zone.day_time_label(p.starts_at, zone.date(now))
        );
    }
    if let Some(p) = later.filter(|p| starts_at >= p.starts_at) {
        bail!(
            "That would put it after plenum #{} on {} — cancel this one instead, \
             so its round carries on to that one.",
            p.id,
            zone.day_time_label(p.starts_at, zone.date(now))
        );
    }
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    // Announced already: say that it moved; and if it moved far, announce
    // it again the day before its new date.
    let announced = before.announced_at.is_some();
    let fresh_summary = announced && starts_at - before.starts_at > FRESH_SUMMARY_AFTER;
    tx.execute(
        "UPDATE plenums SET starts_at = ?2, rescheduled = 1,
                original_starts_at = coalesce(original_starts_at, ?3),
                reschedule_reason = ?4, rescheduled_at = ?5,
                update_pending = CASE WHEN announced_at IS NOT NULL THEN 'moved'
                                      ELSE update_pending END,
                announced_at = CASE WHEN ?6 THEN NULL ELSE announced_at END
         WHERE id = ?1",
        params![
            id,
            time::to_text(starts_at),
            time::to_text(before.starts_at),
            reason,
            time::to_text(now),
            fresh_summary
        ],
    )?;
    audit(
        &tx,
        Some(id),
        "move",
        &format!(
            "{} → {}{}",
            time::to_text(before.starts_at),
            time::to_text(starts_at),
            reason.map(|r| format!(" ({r})")).unwrap_or_default()
        ),
        actor,
        now,
    )?;
    crate::checkins::follow_plenum(&tx, id, now)?;
    ensure_round(&tx, now)?;
    tx.commit()?;
    tracing::info!(plenum = id, announced, "Plenum moved");
    Ok(())
}

/// `a` comes before `b` in the order of scheduled plenums.
fn comes_before(a: &Plenum, b: &Plenum) -> bool {
    (a.starts_at, a.id) < (b.starts_at, b.id)
}

/// Skip one occurrence. Its round (and what was to be deleted after it)
/// moves on to the next plenum.
pub fn cancel(conn: &mut Connection, id: i64, actor: &str, now: DateTime<Utc>) -> Result<()> {
    let tx = conn.transaction()?;
    scheduled(&tx, id)?;
    // Announced already: say it's off (its summary is not sent again).
    tx.execute(
        "UPDATE plenums SET status = 'cancelled',
                update_pending = CASE WHEN announced_at IS NOT NULL THEN 'cancelled' END
         WHERE id = ?1",
        [id],
    )?;
    audit(&tx, Some(id), "cancel", "", actor, now)?;
    ensure_round(&tx, now)?;
    tx.commit()?;
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum Removed {
    /// An extra plenum, gone.
    Deleted,
    /// A generated one: deleting it would bring it back, so it's cancelled.
    Cancelled,
}

pub fn remove(conn: &mut Connection, id: i64, actor: &str, now: DateTime<Utc>) -> Result<Removed> {
    let p = get(conn, id)?.with_context(|| format!("There is no plenum #{id}."))?;
    if p.status == Status::Completed {
        bail!("Plenum #{id} already took place — it stays in the history.");
    }
    if p.status == Status::Scheduled {
        cancel(conn, id, actor, now)?;
    }
    if p.rule_id.is_some() {
        return Ok(Removed::Cancelled);
    }
    let tx = conn.transaction()?;
    let deleted = tx.execute(
        "DELETE FROM plenums WHERE id = ?1
           AND NOT EXISTS (SELECT 1 FROM rounds WHERE plenum_id = ?1)
           AND NOT EXISTS (SELECT 1 FROM submissions WHERE delete_after_plenum_id = ?1)",
        [id],
    )?;
    if deleted == 1 {
        audit(&tx, None, "remove", &format!("#{id}"), actor, now)?;
    }
    tx.commit()?;
    Ok(if deleted == 1 {
        Removed::Deleted
    } else {
        Removed::Cancelled
    })
}

// ── Rounds ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Round {
    pub id: i64,
    pub plenum_id: i64,
}

pub fn open_round(conn: &Connection) -> Result<Option<Round>> {
    Ok(conn
        .query_row(
            "SELECT id, plenum_id FROM rounds WHERE status = 'open'",
            [],
            |r| {
                Ok(Round {
                    id: r.get(0)?,
                    plenum_id: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// Keep the invariant: an open round exists (once any plenum is scheduled)
/// and belongs to the earliest scheduled plenum; its "delete after the next
/// plenum" submissions follow it there. Returns the open round.
pub fn ensure_round(conn: &Connection, now: DateTime<Utc>) -> Result<Option<Round>> {
    let next = next(conn)?;
    match (open_round(conn)?, next) {
        (None, Some(p)) => {
            conn.execute(
                "INSERT INTO rounds (plenum_id, opened_at, status) VALUES (?1, ?2, 'open')",
                params![p.id, time::to_text(now)],
            )?;
            let id = conn.last_insert_rowid();
            tracing::info!(round = id, plenum = p.id, "Opened a Kummerkasten round");
            Ok(Some(Round {
                id,
                plenum_id: p.id,
            }))
        }
        (Some(round), Some(p)) if round.plenum_id != p.id => {
            conn.execute(
                "UPDATE rounds SET plenum_id = ?2 WHERE id = ?1",
                params![round.id, p.id],
            )?;
            conn.execute(
                "UPDATE submissions SET delete_after_plenum_id = ?2
                 WHERE round_id = ?1 AND retention = 'delete_after_plenum'",
                params![round.id, p.id],
            )?;
            // Its check-ins too (deadlines set relative to the plenum follow).
            crate::checkins::follow_round(conn, round.id, p.id, now)?;
            tracing::info!(
                round = round.id,
                plenum = p.id,
                "Round moved to another plenum"
            );
            Ok(Some(Round {
                id: round.id,
                plenum_id: p.id,
            }))
        }
        (round, _) => Ok(round),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::memory_connection;
    use chrono::{NaiveTime, TimeZone};

    pub fn zone() -> Zone {
        Zone::parse("Europe/Berlin").unwrap()
    }

    pub fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        zone().at(
            NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            NaiveTime::from_hms_opt(h, 0, 0).unwrap(),
        )
    }

    pub fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// A database with the default rule (1st/3rd/5th Sunday, 19:00) from
    /// 1 March 2026 and its plenums, as of `now`.
    pub fn with_default_rule(now: DateTime<Utc>) -> Connection {
        let conn = memory_connection();
        let rule = Recurrence::default_rule(NaiveTime::from_hms_opt(19, 0, 0).unwrap());
        ensure_rule(&conn, &rule, day(2026, 3, 1)).unwrap();
        materialize(&conn, zone(), now).unwrap();
        ensure_round(&conn, now).unwrap();
        conn
    }

    fn dates(conn: &Connection) -> Vec<NaiveDate> {
        upcoming(conn, 20)
            .unwrap()
            .iter()
            .map(|p| zone().date(p.starts_at))
            .collect()
    }

    #[test]
    fn plenums_come_from_the_rule_with_stable_ids() {
        let now = at(2026, 3, 2, 12);
        let mut conn = with_default_rule(now);
        let first = dates(&conn);
        assert_eq!(
            first[..4],
            [
                day(2026, 3, 15),
                day(2026, 3, 29),
                day(2026, 4, 5),
                day(2026, 4, 19)
            ]
        );
        // March 1st is past: not created after the fact.
        assert!(!first.contains(&day(2026, 3, 1)));
        let ids: Vec<i64> = upcoming(&conn, 5).unwrap().iter().map(|p| p.id).collect();
        // Materializing again (every tick) changes nothing.
        assert_eq!(materialize(&conn, zone(), now).unwrap(), 0);
        let again: Vec<i64> = upcoming(&conn, 5).unwrap().iter().map(|p| p.id).collect();
        assert_eq!(ids, again);
        // 19:00 Berlin, stored in UTC.
        let next = next(&conn).unwrap().unwrap();
        assert_eq!(time::to_text(next.starts_at), "2026-03-15T18:00:00Z");
        // The plenum after next.
        assert_eq!(
            zone().date(upcoming(&conn, 2).unwrap()[1].starts_at),
            day(2026, 3, 29)
        );
        assert!(previous(&conn).unwrap().is_none());
        // The open round belongs to the next plenum.
        assert_eq!(open_round(&conn).unwrap().unwrap().plenum_id, next.id);
        let _ = &mut conn;
    }

    #[test]
    fn moving_one_plenum_changes_only_that_one() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let before = upcoming(&conn, 4).unwrap();
        // Sunday 15 Nov → Saturday 14 Nov, 18:00.
        let target = before
            .iter()
            .find(|p| zone().date(p.starts_at) == day(2026, 11, 15))
            .unwrap();
        reschedule(
            &mut conn,
            target.id,
            at(2026, 11, 14, 18),
            None,
            "@admin:x",
            zone(),
            now,
        )
        .unwrap();
        let after = upcoming(&conn, 4).unwrap();
        assert_eq!(after.len(), before.len());
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(b.id, a.id);
            if a.id == target.id {
                assert_eq!(a.starts_at, at(2026, 11, 14, 18));
                assert!(a.rescheduled);
            } else {
                assert_eq!(a.starts_at, b.starts_at);
            }
        }
        // Materializing doesn't bring Sunday back.
        assert_eq!(materialize(&conn, zone(), now).unwrap(), 0);
        assert!(!dates(&conn).contains(&day(2026, 11, 15)));
        // The rule itself is unchanged.
        assert_eq!(rules(&conn).unwrap().len(), 1);
    }

    #[test]
    fn extra_and_cancelled_plenums() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let first = next(&conn).unwrap().unwrap();
        assert_eq!(zone().date(first.starts_at), day(2026, 11, 15));
        let round = open_round(&conn).unwrap().unwrap();
        // An extra plenum before it takes over the round.
        let extra = add(&mut conn, at(2026, 11, 8, 18), "@admin:x", zone(), now).unwrap();
        assert_eq!(next(&conn).unwrap().unwrap().id, extra);
        assert_eq!(
            open_round(&conn).unwrap().unwrap(),
            Round {
                id: round.id,
                plenum_id: extra
            }
        );
        // Cancelling it hands the round back; it stays cancelled.
        cancel(&mut conn, extra, "@admin:x", now).unwrap();
        assert_eq!(open_round(&conn).unwrap().unwrap().plenum_id, first.id);
        assert_eq!(
            get(&conn, extra).unwrap().unwrap().status,
            Status::Cancelled
        );
        // Removing the extra one now deletes it; a generated one is only
        // cancelled (deleting would make the rule bring it back).
        assert_eq!(
            remove(&mut conn, extra, "@admin:x", now).unwrap(),
            Removed::Deleted
        );
        assert!(get(&conn, extra).unwrap().is_none());
        assert_eq!(
            remove(&mut conn, first.id, "@admin:x", now).unwrap(),
            Removed::Cancelled
        );
        materialize(&conn, zone(), now).unwrap();
        assert_eq!(
            get(&conn, first.id).unwrap().unwrap().status,
            Status::Cancelled
        );
        assert_eq!(
            zone().date(next(&conn).unwrap().unwrap().starts_at),
            day(2026, 11, 29)
        );
        // Not in the past.
        assert!(add(&mut conn, at(2026, 11, 1, 18), "@admin:x", zone(), now).is_err());
        // Every change is in the audit trail.
        let changes: i64 = conn
            .query_row("SELECT count(*) FROM plenum_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(changes, 4);
    }

    #[test]
    fn a_new_rule_replaces_only_untouched_future_plenums() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let moved = upcoming(&conn, 3).unwrap()[1].clone(); // 29 Nov
        reschedule(
            &mut conn,
            moved.id,
            at(2026, 11, 28, 18),
            None,
            "@admin:x",
            zone(),
            now,
        )
        .unwrap();
        // From 20 Nov on: every Sunday.
        let rule = Recurrence::parse(&["sun", "19:00"], day(2026, 11, 2)).unwrap();
        let change =
            set_rule(&mut conn, &rule, day(2026, 11, 20), "@admin:x", zone(), now).unwrap();
        assert_eq!(change.kept, 1);
        let ds = dates(&conn);
        // 15 Nov stays (before the change), 28 Nov stays (moved by hand),
        // and the new rule adds 22 and 29 Nov, 6 Dec, …
        assert_eq!(
            ds[..5],
            [
                day(2026, 11, 15),
                day(2026, 11, 22),
                day(2026, 11, 28),
                day(2026, 11, 29),
                day(2026, 12, 6)
            ]
        );
        // A past day or today can't be the start of a new rule.
        assert!(set_rule(&mut conn, &rule, day(2026, 11, 2), "@admin:x", zone(), now).is_err());
        // The open round still belongs to the next plenum.
        assert_eq!(
            open_round(&conn).unwrap().unwrap().plenum_id,
            next(&conn).unwrap().unwrap().id
        );
        let _ = Utc.timestamp_opt(0, 0);
    }
}
