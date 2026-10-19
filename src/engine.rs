//! Runs a resident's conversation: loads their session, maps a tap or a
//! message to an action, applies it (`flow`), carries out the effect in the
//! database and says which messages to send (`screens`). No Matrix here:
//! `dm` sends what comes out and records the events.
//!
//! Privacy notes:
//! * the session row (Matrix ID + draft) is deleted in the same transaction
//!   that stores a submission or a check-in response, so a stored anonymous
//!   one is never next to its author's ID;
//! * which submission someone is managing (after entering a code) is kept
//!   in memory only, for a few minutes;
//! * nothing here logs content or Matrix IDs, and log lines about stored
//!   answers carry nothing that identifies the row;
//! * an idea shared along with feedback or an answer is created in the same
//!   transaction but linked to neither (and has no author).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    catalog::Catalog,
    checkins::{self, Refused, Run},
    db::Db,
    flow::{self, Action, CheckinDraft, Effect, Env, Flow, IdeaPlan, MenuItem, Screen},
    ideas, plenums, results,
    screens::{self, Prepared},
    secrets::Secrets,
    submissions::{self, NewSubmission},
    time::{self, Zone},
};

/// How long a looked-up submission stays open for 🗑️.
const MANAGE_FOR: Duration = Duration::from_secs(15 * 60);
/// Wrong codes allowed per person within [`ATTEMPT_WINDOW`].
const MAX_ATTEMPTS: usize = 5;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Who is talking, as checked against the server just now: their Matrix ID
/// and the authorized rooms they are in (never empty for someone allowed).
#[derive(Clone, Debug, PartialEq)]
pub struct Who {
    pub mxid: String,
    pub rooms: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Tap(String),
    Text { body: String, event_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// The live screen: its buttons work, it goes when the flow ends.
    Screen,
    /// The live screen, changed: edit it in place (a box ticked).
    Edit,
    /// A short note within the flow; goes with it.
    Note,
    /// A result that stays until the round is cleared.
    Receipt,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub kind: Kind,
    pub text: String,
    pub buttons: Vec<String>,
    /// Its buttons are the ones that work now (screens, the code receipt).
    pub live: bool,
}

impl Msg {
    pub fn note(text: &str) -> Self {
        Msg {
            kind: Kind::Note,
            text: text.to_owned(),
            buttons: Vec::new(),
            live: false,
        }
    }

    pub fn receipt(text: String) -> Self {
        Msg {
            kind: Kind::Receipt,
            text,
            buttons: Vec::new(),
            live: false,
        }
    }

    fn screen(view: screens::View) -> Self {
        Msg {
            kind: Kind::Screen,
            buttons: view.keys(),
            text: view.text,
            live: true,
        }
    }

    fn edit(view: screens::View) -> Self {
        Msg {
            kind: Kind::Edit,
            ..Msg::screen(view)
        }
    }

    /// The receipt with a deletion code: it stays until the round is
    /// cleared, or until they say they saved the code.
    fn code(text: String, view: screens::View) -> Self {
        Msg {
            kind: Kind::Receipt,
            text,
            buttons: view.keys(),
            live: true,
        }
    }

    /// Whether it goes when the current flow ends.
    pub fn in_session(&self) -> bool {
        matches!(self.kind, Kind::Screen | Kind::Edit | Kind::Note)
    }
}

#[derive(Default)]
struct Memory {
    managing: HashMap<String, (i64, Instant)>,
    attempts: HashMap<String, Vec<Instant>>,
}

/// Which check-in message to write.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CheckinText {
    Notice,
    Reminder,
    Announcement,
}

struct Session {
    flow: Flow,
    updated: DateTime<Utc>,
    room: String,
    screen_event: Option<String>,
}

/// The privacy threshold in force (admins can change it at runtime).
pub type Threshold = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone)]
pub struct Engine {
    pub db: Db,
    secrets: Secrets,
    pub zone: Zone,
    memory: Arc<Mutex<Memory>>,
    threshold: Threshold,
}

impl Engine {
    pub fn new(db: Db, secrets: Secrets, zone: Zone) -> Self {
        Self {
            db,
            secrets,
            zone,
            memory: Arc::default(),
            threshold: Arc::new(|| 3),
        }
    }

    /// Read the privacy threshold from `threshold` (the live settings).
    pub fn with_threshold(mut self, threshold: Threshold) -> Self {
        self.threshold = threshold;
        self
    }

    /// The smallest group a result may describe, never below 2.
    pub fn k(&self) -> i64 {
        (self.threshold)().max(2)
    }

    /// Open `who`'s chat at `item` (📈 / 💡 under a summary in the house
    /// room), whatever was going on before.
    pub async fn open_at(
        &self,
        who: &Who,
        room_id: &str,
        item: MenuItem,
        now: DateTime<Utc>,
    ) -> Result<Vec<Msg>> {
        let this = self.clone();
        let (who, room_id) = (who.clone(), room_id.to_owned());
        self.db
            .run(move |conn| {
                this.forget(&who.mxid);
                tidy(conn, &room_id)?;
                let mut flow = Flow::default();
                let cat = Catalog::load(conn)?;
                flow::apply_in(
                    &mut flow,
                    Action::Open(item),
                    &Env {
                        cat: &cat,
                        own_floor: None,
                        run: None,
                        similar: None,
                        pages: 0,
                    },
                );
                save(conn, &who.mxid, &room_id, &flow, now)?;
                Ok(vec![Msg::screen(this.render(conn, &who, &flow, now)?)])
            })
            .await
    }

    /// Handle one tap or message in `who`'s chat `room_id`. `ttl`: an
    /// unfinished flow older than this is thrown away.
    pub async fn handle(
        &self,
        who: &Who,
        room_id: &str,
        input: Input,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<Msg>> {
        let this = self.clone();
        let (who, room_id) = (who.clone(), room_id.to_owned());
        self.db
            .run(move |conn| {
                let msgs = this.handle_sync(conn, &who, &room_id, input, now, ttl)?;
                this.with_screen(conn, &who, &room_id, now, msgs)
            })
            .await
    }

    /// `msgs` with a screen added when they hold none and the chat has no
    /// live screen to tap on (never had one, or it was redacted: the flow
    /// ended, a restart in the middle of a step). A note alone would leave
    /// them with nothing but `!menu`.
    fn with_screen(
        &self,
        conn: &Connection,
        who: &Who,
        room_id: &str,
        now: DateTime<Utc>,
        mut msgs: Vec<Msg>,
    ) -> Result<Vec<Msg>> {
        if msgs.iter().any(|m| m.live) {
            return Ok(msgs);
        }
        let session = load(conn, &who.mxid)?.filter(|s| s.room == room_id);
        let alive = match session.as_ref().and_then(|s| s.screen_event.as_deref()) {
            Some(event) => screen_alive(conn, room_id, event)?,
            None => false,
        };
        if alive {
            return Ok(msgs);
        }
        let flow = match session {
            Some(s) => s.flow,
            None => {
                save(conn, &who.mxid, room_id, &Flow::default(), now)?;
                Flow::default()
            }
        };
        msgs.push(Msg::screen(self.render(conn, who, &flow, now)?));
        Ok(msgs)
    }

    /// Throw away whatever was going on and show a fresh menu.
    pub async fn fresh_menu(
        &self,
        who: &Who,
        room_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<Msg>> {
        let this = self.clone();
        let (who, room_id) = (who.clone(), room_id.to_owned());
        self.db
            .run(move |conn| {
                this.forget(&who.mxid);
                tidy(conn, &room_id)?;
                save(conn, &who.mxid, &room_id, &Flow::default(), now)?;
                Ok(vec![Msg::screen(this.render(
                    conn,
                    &who,
                    &Flow::default(),
                    now,
                )?)])
            })
            .await
    }

    /// `!help`: the short help, then the screen they were on again (below
    /// it, so they carry on where they were), or a fresh menu.
    pub async fn help(
        &self,
        who: &Who,
        room_id: &str,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<Msg>> {
        let this = self.clone();
        let (who, room_id) = (who.clone(), room_id.to_owned());
        self.db
            .run(move |conn| {
                let flow = match load(conn, &who.mxid)? {
                    Some(s)
                        if s.room == room_id
                            && !(s.flow.in_progress() && now - s.updated > ttl) =>
                    {
                        s.flow
                    }
                    _ => {
                        tidy(conn, &room_id)?;
                        save(conn, &who.mxid, &room_id, &Flow::default(), now)?;
                        Flow::default()
                    }
                };
                Ok(vec![
                    Msg::note(&screens::help()),
                    Msg::screen(this.render(conn, &who, &flow, now)?),
                ])
            })
            .await
    }

    /// 📊 on a check-in notice or reminder: open that check-in (or carry on
    /// where they were in it).
    pub async fn start_run(
        &self,
        who: &Who,
        room_id: &str,
        run_id: i64,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<Msg>> {
        let this = self.clone();
        let (who, room_id) = (who.clone(), room_id.to_owned());
        self.db
            .run(move |conn| {
                let msgs = this.start_run_sync(conn, &who, &room_id, run_id, now, ttl)?;
                this.with_screen(conn, &who, &room_id, now, msgs)
            })
            .await
    }

    /// The live screen message of `mxid`, if any: `(room_id, event_id)`.
    pub async fn screen_event(&self, mxid: &str) -> Result<Option<(String, String)>> {
        let mxid = mxid.to_owned();
        self.db
            .run(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT room_id, screen_event_id FROM sessions
                         WHERE mxid = ?1 AND screen_event_id IS NOT NULL",
                        [mxid],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?)
            })
            .await
    }

    pub async fn set_screen(&self, mxid: &str, event_id: &str) -> Result<()> {
        let (mxid, event_id) = (mxid.to_owned(), event_id.to_owned());
        self.db
            .run(move |conn| {
                conn.execute(
                    "UPDATE sessions SET screen_event_id = ?2 WHERE mxid = ?1",
                    params![mxid, event_id],
                )?;
                Ok(())
            })
            .await
    }

    /// Unfinished flows older than `ttl`: their events are marked for
    /// redaction and the flow reset. Returns `(mxid, room_id)` of each, to
    /// get a fresh menu.
    pub async fn expire(
        &self,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<(String, String)>> {
        let this = self.clone();
        self.db
            .run(move |conn| {
                let stale: Vec<(String, String, String)> = {
                    let mut stmt = conn.prepare(
                        "SELECT mxid, room_id, flow FROM sessions WHERE updated_at < ?1",
                    )?;
                    let rows = stmt.query_map([time::to_text(now - ttl)], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                let mut out = Vec::new();
                for (mxid, room_id, json) in stale {
                    let flow: Flow = serde_json::from_str(&json).unwrap_or_default();
                    if !flow.in_progress() {
                        continue;
                    }
                    this.forget(&mxid);
                    tidy(conn, &room_id)?;
                    conn.execute("DELETE FROM sessions WHERE mxid = ?1", [&mxid])?;
                    out.push((mxid, room_id));
                }
                Ok(out)
            })
            .await
    }

    /// A check-in message (notice, reminder, announcement) for `run_id`.
    pub async fn checkin_text(
        &self,
        run_id: i64,
        which: CheckinText,
        now: DateTime<Utc>,
    ) -> Result<(Run, String)> {
        let zone = self.zone;
        self.db
            .run(move |conn| {
                let run = checkins::run(conn, run_id)?;
                let ctx = screens::Ctx {
                    zone: Some(zone),
                    today: Some(zone.date(now)),
                    ..Default::default()
                };
                let text = match which {
                    CheckinText::Notice => screens::checkin_notice(&run, &ctx),
                    CheckinText::Reminder => screens::checkin_reminder(&run, &ctx),
                    CheckinText::Announcement => screens::checkin_announcement(&run, &ctx),
                };
                Ok((run, text))
            })
            .await
    }

    /// Whether `who` should hear about `run` now (open, theirs, unanswered).
    pub async fn should_tell(&self, run: &Run, who: &Who, now: DateTime<Utc>) -> Result<bool> {
        let (this, run, who) = (self.clone(), run.clone(), who.clone());
        self.db
            .run(move |conn| {
                let floor = own_floor(conn, &who.mxid)?;
                checkins::should_tell(conn, &this.secrets, &run, &who.mxid, &who.rooms, floor, now)
            })
            .await
    }

    fn forget(&self, mxid: &str) {
        self.memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .managing
            .remove(mxid);
    }

    fn managing(&self, mxid: &str) -> Option<i64> {
        let memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        memory
            .managing
            .get(mxid)
            .filter(|(_, at)| at.elapsed() < MANAGE_FOR)
            .map(|(id, _)| *id)
    }

    fn render(
        &self,
        conn: &Connection,
        who: &Who,
        flow: &Flow,
        now: DateTime<Utc>,
    ) -> Result<screens::View> {
        let mxid = who.mxid.as_str();
        let cat = Catalog::load(conn)?;
        let own_floor = own_floor(conn, mxid)?;
        let named = if flow.screen() == &Screen::ManageEnter {
            submissions::named_by(conn, mxid)?
        } else {
            Vec::new()
        };
        let managed = match (flow.screen(), self.managing(mxid)) {
            (Screen::Manage, Some(id)) => manageable(conn, mxid, id)?,
            _ => None,
        };
        let listing = if matches!(flow.screen(), Screen::Menu | Screen::Checkins) {
            checkins::listing(conn, &self.secrets, mxid, &who.rooms, own_floor, now)?
        } else {
            Vec::new()
        };
        let run = match flow.run() {
            Some(id) => Some(checkins::run(conn, id)?),
            None => None,
        };
        let today = self.zone.date(now);
        let prepared = self.prepare(conn, &cat, who, flow, run.as_ref(), today)?;
        let next_plenum = plenums::next(conn)?.map(|p| {
            // Its time passed but it isn't completed: the round goes on.
            let when = if p.awaiting_completion(now) {
                "🌱 round still open".to_owned()
            } else {
                time::in_days(today, self.zone.date(p.starts_at))
            };
            let moved = p.rescheduled
                && self.zone.date(p.original_starts_at) != self.zone.date(p.starts_at);
            (
                self.zone.day_label(p.starts_at, today),
                format!(
                    "{} · {when}{}",
                    self.zone.local(p.starts_at).format("%H:%M"),
                    if moved { " · 🔄 moved" } else { "" }
                ),
            )
        });
        let ctx = screens::Ctx {
            cat: Some(&cat),
            next_plenum,
            own_floor,
            named: &named,
            managed: managed.as_ref(),
            checkins: &listing,
            run: run.as_ref(),
            zone: Some(self.zone),
            today: Some(today),
            prepared,
        };
        Ok(screens::view(flow, &ctx))
    }

    /// What a results, ideas or sharing screen shows (through the privacy
    /// policy, never raw feedback).
    fn prepare(
        &self,
        conn: &Connection,
        cat: &Catalog,
        who: &Who,
        flow: &Flow,
        run: Option<&Run>,
        today: chrono::NaiveDate,
    ) -> Result<Option<Prepared>> {
        let k = self.k();
        Ok(match flow.screen() {
            Screen::Results => {
                let (text, topics) = results::home(conn, cat, k, today)?;
                Some(Prepared::Results { text, topics })
            }
            Screen::ResultsTopic { topic, page } => Some(Prepared::Topic(results::topic(
                conn, cat, *topic, *page, k, self.zone, today,
            )?)),
            Screen::ResultsFloors { topic, page } => {
                let t = results::topic(conn, cat, *topic, *page, k, self.zone, today)?;
                Some(Prepared::Text(match t.floors {
                    Some((run, qv)) => results::floors(conn, run, qv, k)?,
                    None => "🔒 Not enough responses to show this breakdown.".to_owned(),
                }))
            }
            Screen::ResultsHistory => {
                Some(Prepared::History(results::history(conn, self.zone, today)?))
            }
            Screen::ResultsPlenum { plenum } => Some(Prepared::Text(results::plenum(
                conn, cat, *plenum, k, self.zone, today,
            )?)),
            Screen::Ideas => {
                let (text, items) = results::board(conn, cat)?;
                Some(Prepared::Board { text, items })
            }
            Screen::Idea { id } => match ideas::get(conn, *id)? {
                Some(idea) => Some(Prepared::Idea {
                    topic: idea
                        .topic_id
                        .and_then(|t| cat.topic(t))
                        .map(|_| cat.topic_path(idea.topic_id.unwrap_or_default()))
                        .unwrap_or_default(),
                    comments: ideas::comments(conn, *id, 3)?,
                    supported: ideas::supports(conn, &self.secrets, *id, &who.mxid)?,
                    idea,
                }),
                None => None,
            },
            Screen::ShareIdea => flow.idea_text(cat).map(|t| Prepared::Share(t.to_owned())),
            Screen::CheckinShareIdea { index } => flow
                .draft
                .checkin
                .as_ref()
                .and_then(|c| c.answers.get(index))
                .and_then(|a| a.text.clone())
                .map(Prepared::Share),
            Screen::SimilarIdea { id } | Screen::CheckinSimilarIdea { id, .. } => {
                ideas::get(conn, *id)?.map(Prepared::Similar)
            }
            _ => {
                let _ = run;
                None
            }
        })
    }

    /// A similar existing idea, for the "share as an idea?" step.
    fn similar_for(
        &self,
        conn: &Connection,
        cat: &Catalog,
        flow: &Flow,
        run: Option<&Run>,
    ) -> Result<Option<i64>> {
        let (text, topic, problem) = match flow.screen() {
            Screen::ShareIdea => {
                let topic = flow.draft.topic;
                (
                    flow.idea_text(cat).map(str::to_owned),
                    topic.map(|t| root(cat, t)),
                    topic.and_then(|t| cat.topic(t)).and_then(|t| t.problem_id),
                )
            }
            Screen::CheckinShareIdea { index } => {
                let text = flow
                    .draft
                    .checkin
                    .as_ref()
                    .and_then(|c| c.answers.get(index))
                    .and_then(|a| a.text.clone());
                let (topic, problem) = match run {
                    Some(run) => run_context(conn, run, *index)?,
                    None => (None, None),
                };
                (text, topic.map(|t| root(cat, t)), problem)
            }
            _ => return Ok(None),
        };
        Ok(match text {
            Some(text) => ideas::similar(conn, &text, topic, problem)?.map(|i| i.id),
            None => None,
        })
    }

    /// Start over with a note (and a fresh menu).
    fn reset(
        &self,
        conn: &Connection,
        who: &Who,
        room_id: &str,
        note: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<Msg>> {
        self.forget(&who.mxid);
        tidy(conn, room_id)?;
        save(conn, &who.mxid, room_id, &Flow::default(), now)?;
        Ok(vec![
            Msg::note(note),
            Msg::screen(self.render(conn, who, &Flow::default(), now)?),
        ])
    }

    fn handle_sync(
        &self,
        conn: &mut Connection,
        who: &Who,
        room_id: &str,
        input: Input,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<Msg>> {
        let mxid = who.mxid.as_str();
        let (mut flow, screen_event) = match load(conn, mxid)? {
            Some(session) if session.room == room_id => {
                if session.flow.in_progress() && now - session.updated > ttl {
                    // Stale: tidy up and start over, ignoring the input.
                    return self.reset(conn, who, room_id, screens::expired(), now);
                }
                (session.flow, session.screen_event)
            }
            _ => (Flow::default(), None),
        };
        let own_floor = own_floor(conn, mxid)?;
        // A check-in in progress: still open, and still meant for them?
        let run = match flow.run() {
            Some(id) if flow.in_progress() => {
                let run = checkins::run(conn, id)?;
                if !run.is_open(now) {
                    return self.reset(conn, who, room_id, Refused::Closed.message(), now);
                }
                if !run
                    .target
                    .matches(mxid, &who.rooms, flow.draft.floor.or(own_floor))
                {
                    return self.reset(conn, who, room_id, Refused::NotAsked.message(), now);
                }
                Some(run)
            }
            _ => None,
        };
        let view = self.render(conn, who, &flow, now)?;
        let (action, secret_event) = match input {
            Input::Tap(key) => match screens::resolve(&view, &key) {
                Some(action) => (action, None),
                None => return Ok(Vec::new()),
            },
            Input::Text { body, event_id } => {
                // A deletion code they type is a secret: off the screen now.
                let secret = (flow.screen() == &Screen::ManageEnter).then_some(event_id);
                if flow.wants_text(run.as_ref()) {
                    match screens::typed(&view, &body) {
                        // "cancel"/"back" typed, but never on a text step
                        // where it might be the actual answer.
                        Some(a @ (Action::Back | Action::Cancel)) if body.trim().len() < 12 => {
                            (a, secret)
                        }
                        Some(a @ Action::Named(_)) => (a, secret),
                        _ => (Action::Text(body), secret),
                    }
                } else {
                    (
                        screens::typed(&view, &body).unwrap_or(Action::Text(body)),
                        secret,
                    )
                }
            }
        };
        if let Some(event_id) = secret_event {
            doom(conn, room_id, &event_id)?;
        }
        let cat = Catalog::load(conn)?;
        let similar = self.similar_for(conn, &cat, &flow, run.as_ref())?;
        let pages = match flow.screen() {
            Screen::ResultsTopic { topic, .. } => {
                crate::stats::participation(conn, Some(*topic))?.len()
            }
            _ => 0,
        };
        let step_env = Env {
            cat: &cat,
            own_floor,
            run: run.as_ref(),
            similar,
            pages,
        };
        let mut out = Vec::new();
        match flow::apply_in(&mut flow, action, &step_env) {
            Effect::Show => {}
            Effect::Refresh => {
                save(conn, mxid, room_id, &flow, now)?;
                return Ok(vec![Msg::edit(self.render(conn, who, &flow, now)?)]);
            }
            Effect::Hint(hint) => {
                save(conn, mxid, room_id, &flow, now)?;
                return Ok(vec![Msg::note(hint)]);
            }
            effect @ (Effect::Support(_) | Effect::Withdraw(_)) => {
                let (id, support) = match effect {
                    Effect::Support(id) => (id, true),
                    Effect::Withdraw(id) => (id, false),
                    _ => unreachable!(),
                };
                let done = if support {
                    ideas::support(conn, &self.secrets, id, mxid).map(drop)
                } else {
                    ideas::withdraw(conn, &self.secrets, id, mxid).map(drop)
                };
                save(conn, mxid, room_id, &flow, now)?;
                let screen = Msg::edit(self.render(conn, who, &flow, now)?);
                // The screen says it (✅ You support this idea / 👍 again); a
                // note only when nothing changed, and then after the screen,
                // so the screen is edited in place.
                return Ok(match done {
                    Ok(()) => vec![screen],
                    Err(_) => vec![
                        screen,
                        Msg::note("🗄️ That idea is closed. Nothing changed."),
                    ],
                });
            }
            Effect::Comment(id, text) => {
                let today = self.zone.date(now);
                out.push(Msg::note(
                    match ideas::comment(conn, &self.secrets, id, mxid, &text, today) {
                        Ok(()) => "💬 Thanks. Your comment is there, without your name.",
                        Err(_) => "🗄️ That idea is closed. The comment wasn't added.",
                    },
                ));
            }
            Effect::Cancel => {
                self.forget(mxid);
                tidy(conn, room_id)?;
            }
            Effect::Submit => match self.submit(conn, mxid, room_id, &flow, own_floor, &cat, now) {
                Ok((receipt, has_code)) => {
                    flow = Flow::default();
                    if has_code {
                        // The code stays on screen until they've saved it.
                        flow.push(Screen::CodeSaved);
                        save(conn, mxid, room_id, &flow, now)?;
                        let buttons = self.render(conn, who, &flow, now)?;
                        return Ok(vec![Msg::code(receipt, buttons)]);
                    }
                    out.push(Msg::receipt(receipt));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "A submission could not be stored");
                    save(conn, mxid, room_id, &flow, now)?;
                    return Ok(vec![Msg::note(
                        "😕 Something went wrong storing that. Nothing was saved. Try ✅ again in a moment.",
                    )]);
                }
            },
            Effect::ForgetCode => {
                if let Some(event_id) = screen_event {
                    doom(conn, room_id, &event_id)?;
                }
                out.push(Msg::note("🔐 Removed the code from this chat."));
            }
            Effect::SubmitCheckin => {
                let Some(run) = run.as_ref() else {
                    return self.reset(conn, who, room_id, Refused::Closed.message(), now);
                };
                match self.submit_checkin(conn, who, room_id, &flow, run, own_floor, now)? {
                    Ok(identity) => {
                        out.push(Msg::receipt(screens::checkin_thanks(run, identity)));
                        flow = Flow::default();
                    }
                    Err(Refused::Incomplete) => {
                        save(conn, mxid, room_id, &flow, now)?;
                        return Ok(vec![Msg::note(Refused::Incomplete.message())]);
                    }
                    Err(refused) => return self.reset(conn, who, room_id, refused.message(), now),
                }
            }
            Effect::LookupToken(token) => {
                if self.too_many_attempts(mxid) {
                    save(conn, mxid, room_id, &flow, now)?;
                    return Ok(vec![Msg::note(
                        "⏳ That's a lot of tries. Please wait a few minutes.",
                    )]);
                }
                match submissions::by_token(conn, &self.secrets, &token)? {
                    Some(found) => self.open(mxid, found.id, &mut flow),
                    None => {
                        self.count_attempt(mxid);
                        save(conn, mxid, room_id, &flow, now)?;
                        return Ok(vec![Msg::note(
                            "🤔 That code doesn't match any submission. Check it and try again.",
                        )]);
                    }
                }
            }
            Effect::OpenNamed(id) => match submissions::named_one(conn, mxid, id)? {
                Some(found) => self.open(mxid, found.id, &mut flow),
                None => return Ok(vec![Msg::note("🤔 I can't find that one any more.")]),
            },
            Effect::Delete => {
                let target = self.managing(mxid);
                let still_theirs = match target {
                    Some(id) => manageable(conn, mxid, id)?.is_some(),
                    None => false,
                };
                match target.filter(|_| still_theirs) {
                    Some(id) => {
                        let tx = conn.transaction()?;
                        submissions::delete(&tx, id)?;
                        tidy(&tx, room_id)?;
                        tx.commit()?;
                        self.forget(mxid);
                        tracing::info!("submission deleted by its author");
                        out.push(Msg::receipt(screens::deleted()));
                        flow = Flow::default();
                    }
                    None => {
                        return Ok(vec![Msg::note(
                            "⏳ I've forgotten which one that was. Send the code again.",
                        )])
                    }
                }
            }
        }
        save(conn, mxid, room_id, &flow, now)?;
        out.push(Msg::screen(self.render(conn, who, &flow, now)?));
        Ok(out)
    }

    fn start_run_sync(
        &self,
        conn: &mut Connection,
        who: &Who,
        room_id: &str,
        run_id: i64,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<Msg>> {
        let mxid = who.mxid.as_str();
        let session = load(conn, mxid)?.filter(|s| s.room == room_id);
        // Already answering it: show where they are.
        if let Some(s) = &session {
            if s.flow.run() == Some(run_id) && s.flow.in_progress() && now - s.updated <= ttl {
                return Ok(vec![Msg::screen(self.render(conn, who, &s.flow, now)?)]);
            }
        }
        let run = checkins::run(conn, run_id)?;
        let floor = own_floor(conn, mxid)?;
        if !run.is_open(now) {
            return Ok(vec![Msg::note(Refused::Closed.message())]);
        }
        if !run.target.matches(mxid, &who.rooms, floor) {
            return Ok(vec![Msg::note(Refused::NotAsked.message())]);
        }
        if checkins::completed(conn, &self.secrets, run_id, mxid)? {
            return Ok(vec![Msg::note(Refused::AlreadyAnswered.message())]);
        }
        self.forget(mxid);
        tidy(conn, room_id)?;
        let mut flow = Flow::default();
        flow.push(Screen::Checkins);
        flow.draft.checkin = Some(CheckinDraft {
            run: run_id,
            answers: Default::default(),
            identity: None,
            ideas: Default::default(),
        });
        flow.push(Screen::CheckinIntro);
        save(conn, mxid, room_id, &flow, now)?;
        Ok(vec![Msg::screen(self.render(conn, who, &flow, now)?)])
    }

    fn open(&self, mxid: &str, id: i64, flow: &mut Flow) {
        self.memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .managing
            .insert(mxid.to_owned(), (id, Instant::now()));
        flow.push(Screen::Manage);
    }

    fn too_many_attempts(&self, mxid: &str) -> bool {
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        let tries = memory.attempts.entry(mxid.to_owned()).or_default();
        tries.retain(|t| t.elapsed() < ATTEMPT_WINDOW);
        tries.len() >= MAX_ATTEMPTS
    }

    fn count_attempt(&self, mxid: &str) {
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        memory
            .attempts
            .entry(mxid.to_owned())
            .or_default()
            .push(Instant::now());
    }

    /// Store the draft, in one transaction with deleting the session and
    /// marking the flow's chat events for redaction. Returns the receipt
    /// and whether it holds a deletion code.
    #[allow(clippy::too_many_arguments)]
    fn submit(
        &self,
        conn: &mut Connection,
        mxid: &str,
        room_id: &str,
        flow: &Flow,
        own_floor: Option<i64>,
        cat: &Catalog,
        now: DateTime<Utc>,
    ) -> Result<(String, bool)> {
        let d = &flow.draft;
        let topic_id = d.topic.context("no topic")?;
        let new = NewSubmission {
            topic_id,
            problem_id: cat.topic(topic_id).and_then(|t| t.problem_id),
            location_id: d.location,
            body: d.body.clone().context("no text")?,
            suggestion: d.suggestion.clone(),
            identity: d.identity.context("no identity")?,
            retention: d.retention.context("no retention")?,
            for_plenum: d.for_plenum,
        };
        let tx = conn.transaction()?;
        if let Some(floor) = d.floor {
            tx.execute(
                "UPDATE residents SET floor_id = ?2 WHERE mxid = ?1",
                params![mxid, floor],
            )?;
        }
        let today = self.zone.date(now);
        let done =
            submissions::submit(&tx, &self.secrets, &new, mxid, d.floor.or(own_floor), today)?;
        // The idea, if they chose to share one, not linked to the submission.
        let shared = match (d.idea, flow.idea_text(cat)) {
            (Some(plan), Some(text)) => Some(self.share(
                &tx,
                plan,
                text,
                Some(topic_id),
                new.problem_id,
                ideas::Origin::Feedback,
                Some(done.round_id),
                mxid,
                today,
            )?),
            _ => None,
        };
        tx.execute("DELETE FROM sessions WHERE mxid = ?1", [mxid])?;
        tidy(&tx, room_id)?;
        let plenum = plenums::get(&tx, done.plenum_id)?;
        tx.commit()?;
        // Deliberately nothing that tells which row this was (no ID, topic or
        // identity): with the time of this line and the homeserver's record
        // of who wrote to the bot when, it could undo anonymity.
        tracing::info!(round = done.round_id, "submission created");
        let mut receipt = screens::submitted(
            new.identity,
            done.token.as_deref(),
            new.retention,
            plenum
                .map(|p| self.zone.day_label(p.starts_at, today))
                .as_deref(),
        );
        if let Some(shared) = shared {
            receipt.push_str(&format!("\n{shared}"));
        }
        Ok((receipt, done.token.is_some()))
    }

    /// Store the check-in answers: participation key and response in one
    /// transaction with dropping the session and its chat events.
    #[allow(clippy::too_many_arguments)]
    fn submit_checkin(
        &self,
        conn: &mut Connection,
        who: &Who,
        room_id: &str,
        flow: &Flow,
        run: &Run,
        own_floor: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<Result<submissions::Identity, Refused>> {
        let draft = flow.draft.checkin.as_ref().context("no check-in draft")?;
        let identity = draft.identity.context("no identity")?;
        let tx = conn.transaction()?;
        if let Some(floor) = flow.draft.floor {
            tx.execute(
                "UPDATE residents SET floor_id = ?2 WHERE mxid = ?1",
                params![who.mxid, floor],
            )?;
        }
        let submission = checkins::Submission {
            mxid: &who.mxid,
            rooms: &who.rooms,
            floor: flow.draft.floor.or(own_floor),
            identity,
            answers: &draft.answers,
        };
        if let Err(refused) = checkins::submit(&tx, &self.secrets, run, &submission, now)? {
            // Dropping `tx` rolls everything back.
            return Ok(Err(refused));
        }
        // Answers they chose to share as ideas: linked to no response.
        let today = self.zone.date(now);
        let round = plenums::open_round(&tx)?.map(|r| r.id);
        for (index, plan) in &draft.ideas {
            let Some(text) = draft.answers.get(index).and_then(|a| a.text.clone()) else {
                continue;
            };
            let (topic, problem) = run_context(&tx, run, *index)?;
            self.share(
                &tx,
                *plan,
                &text,
                topic,
                problem,
                ideas::Origin::Checkin,
                round,
                &who.mxid,
                today,
            )?;
        }
        tx.execute("DELETE FROM sessions WHERE mxid = ?1", [&who.mxid])?;
        tidy(&tx, room_id)?;
        tx.commit()?;
        // As for submissions: nothing that identifies the response.
        tracing::info!(run = run.id, "check-in response stored");
        Ok(Ok(identity))
    }
}

impl Engine {
    /// Carry out `plan` for an idea text; the line for the receipt.
    #[allow(clippy::too_many_arguments)]
    fn share(
        &self,
        conn: &Connection,
        plan: IdeaPlan,
        text: &str,
        topic_id: Option<i64>,
        problem_id: Option<i64>,
        origin: ideas::Origin,
        round_id: Option<i64>,
        mxid: &str,
        today: chrono::NaiveDate,
    ) -> Result<String> {
        Ok(match plan {
            IdeaPlan::New => {
                ideas::create(
                    conn,
                    &ideas::NewIdea {
                        text,
                        topic_id,
                        problem_id,
                        origin,
                        round_id,
                    },
                    today,
                )?;
                "🌍 Shared on the 💡 Ideas board, without your name.".to_owned()
            }
            IdeaPlan::Support { id } => match ideas::support(conn, &self.secrets, id, mxid) {
                Ok(_) => "👍 Added your support to the similar idea.".to_owned(),
                Err(_) => "🗄️ The similar idea was closed in the meantime.".to_owned(),
            },
            IdeaPlan::Comment { id } => {
                match ideas::comment(conn, &self.secrets, id, mxid, text, today) {
                    Ok(()) => "💬 Added as a comment to the similar idea.".to_owned(),
                    Err(_) => "🗄️ The similar idea was closed in the meantime.".to_owned(),
                }
            }
        })
    }
}

/// The top-level topic of `topic`.
fn root(cat: &Catalog, topic: i64) -> i64 {
    cat.topic(topic).and_then(|t| t.parent_id).unwrap_or(topic)
}

/// A check-in question's topic (its template's) and canonical problem.
fn run_context(conn: &Connection, run: &Run, index: usize) -> Result<(Option<i64>, Option<i64>)> {
    let topic: Option<i64> = conn
        .query_row(
            "SELECT st.topic_id FROM survey_runs r JOIN survey_templates st ON st.id = r.template_id
             WHERE r.id = ?1",
            [run.id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let problem = match run.questions.get(index).and_then(|q| q.problem.clone()) {
        Some(key) => conn
            .query_row("SELECT id FROM problems WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?,
        None => None,
    };
    Ok((topic, problem))
}

/// Whether the screen `event_id` is still in the chat: tracked and not
/// marked for redaction.
fn screen_alive(conn: &Connection, room_id: &str, event_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM dm_events WHERE room_id = ?1 AND event_id = ?2 AND doomed = 0",
            params![room_id, event_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Mark one event in `room_id` for redaction.
fn doom(conn: &Connection, room_id: &str, event_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE dm_events SET doomed = 1 WHERE room_id = ?1 AND event_id = ?2",
        params![room_id, event_id],
    )?;
    Ok(())
}

/// A submission `mxid` may manage right now: one they named, or one with
/// a deletion code (reached only by entering that code, which put its ID
/// into memory).
fn manageable(conn: &Connection, mxid: &str, id: i64) -> Result<Option<submissions::View>> {
    Ok(match submissions::named_one(conn, mxid, id)? {
        Some(named) => Some(named),
        None => submissions::coded_one(conn, id)?,
    })
}

fn own_floor(conn: &Connection, mxid: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT floor_id FROM residents WHERE mxid = ?1",
            [mxid],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

fn load(conn: &Connection, mxid: &str) -> Result<Option<Session>> {
    let row: Option<(String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT flow, updated_at, room_id, screen_event_id FROM sessions WHERE mxid = ?1",
            [mxid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(match row {
        Some((json, updated, room, screen_event)) => Some(Session {
            flow: serde_json::from_str(&json).unwrap_or_default(),
            updated: time::parse_text(&updated)?,
            room,
            screen_event,
        }),
        None => None,
    })
}

fn save(
    conn: &Connection,
    mxid: &str,
    room_id: &str,
    flow: &Flow,
    now: DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (mxid, room_id, flow, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (mxid) DO UPDATE SET room_id = ?2, flow = ?3, updated_at = ?4",
        params![
            mxid,
            room_id,
            serde_json::to_string(flow)?,
            time::to_text(now)
        ],
    )?;
    Ok(())
}

/// Mark the current conversation's events in `room_id` for redaction.
pub fn tidy(conn: &Connection, room_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE dm_events SET doomed = 1 WHERE room_id = ?1 AND in_session = 1",
        [room_id],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plenums::tests::{at, with_default_rule};

    const ME: &str = "@alice:example.org";
    const ROOM: &str = "!dm:example.org";

    fn engine(now: DateTime<Utc>) -> Engine {
        let db = Db::from_connection(with_default_rule(now));
        db.with(|c| {
            c.execute(
                "INSERT INTO residents (mxid, dm_room_id, dm_joined, created_at) VALUES (?1, ?2, 1, 'x')",
                params![ME, ROOM],
            )?;
            Ok(())
        })
        .unwrap();
        Engine::new(
            db,
            Secrets::from_master(&[5; 32]),
            crate::plenums::tests::zone(),
        )
    }

    fn ttl() -> chrono::Duration {
        chrono::Duration::minutes(60)
    }

    /// Feed inputs like the chat would, recording every screen and message
    /// as an in-session event (as `dm` does). Returns the last messages.
    struct Chat {
        engine: Engine,
        now: DateTime<Utc>,
        n: usize,
        who: Who,
    }

    impl Chat {
        fn new() -> Self {
            let now = at(2026, 11, 2, 12);
            Chat {
                engine: engine(now),
                now,
                n: 0,
                who: Who {
                    mxid: ME.to_owned(),
                    rooms: vec![crate::checkins::tests::HOUSE.to_owned()],
                },
            }
        }

        fn record(&mut self, in_session: bool) -> String {
            self.n += 1;
            let id = format!("$e{}", self.n);
            let id2 = id.clone();
            self.engine
                .db
                .with(|c| {
                    c.execute(
                        "INSERT INTO dm_events (room_id, event_id, in_session, sent_at) VALUES (?1, ?2, ?3, 'x')",
                        params![ROOM, id2, in_session],
                    )?;
                    Ok(())
                })
                .unwrap();
            id
        }

        fn deliver(&mut self, msgs: Vec<Msg>) -> Vec<Msg> {
            for m in msgs.iter().filter(|m| m.kind != Kind::Edit) {
                let id = self.record(m.in_session());
                if m.live {
                    let e = self.engine.clone();
                    tokio_block(e.set_screen(ME, &id)).unwrap();
                }
            }
            msgs
        }

        fn tap(&mut self, key: &str) -> Vec<Msg> {
            let msgs = tokio_block(self.engine.handle(
                &self.who,
                ROOM,
                Input::Tap(key.into()),
                self.now,
                ttl(),
            ))
            .unwrap();
            self.deliver(msgs)
        }

        fn say(&mut self, text: &str) -> Vec<Msg> {
            let id = self.record(true);
            let msgs = tokio_block(self.engine.handle(
                &self.who,
                ROOM,
                Input::Text {
                    body: text.into(),
                    event_id: id,
                },
                self.now,
                ttl(),
            ))
            .unwrap();
            self.deliver(msgs)
        }

        fn menu(&mut self) -> Vec<Msg> {
            let msgs = tokio_block(self.engine.fresh_menu(&self.who, ROOM, self.now)).unwrap();
            self.deliver(msgs)
        }

        fn count(&self, sql: &str) -> i64 {
            self.engine
                .db
                .with(|c| Ok(c.query_row(sql, [], |r| r.get(0))?))
                .unwrap()
        }
    }

    fn tokio_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn screen(msgs: &[Msg]) -> &Msg {
        msgs.iter()
            .find(|m| m.kind == Kind::Screen)
            .expect("a screen")
    }

    /// Menu → submitted anonymous cleaning feedback for Kitchen 2.
    fn share(chat: &mut Chat, identity: &str, retention: &str) -> Vec<Msg> {
        chat.menu();
        chat.tap("📝");
        chat.tap("🧹");
        chat.tap("🙋");
        chat.tap("🍳");
        chat.tap("2️⃣");
        chat.say("The bathroom often isn't cleaned when it is someone's turn.");
        chat.tap("⏭️");
        chat.tap(identity);
        if identity == "🏠" {
            chat.tap("3️⃣");
        }
        let preview = chat.tap(retention);
        assert!(
            screen(&preview).text.contains("📮 **Ready to submit**"),
            "{:?}",
            preview
        );
        chat.tap("✅")
    }

    #[test]
    fn submitting_anonymously_end_to_end() {
        let mut chat = Chat::new();
        let out = share(&mut chat, "🥷", "♾️");
        let code_event = format!("$e{}", chat.n);
        // Only the receipt with the code: it is the live screen now, with
        // "I've saved the code" and "Main menu".
        assert_eq!(out.len(), 1);
        let receipt = &out[0];
        assert!(receipt.text.contains("Submitted anonymously"));
        assert!(receipt.live && receipt.kind == Kind::Receipt);
        assert_eq!(receipt.buttons, ["✅", "🏠"]);
        // Stored without the author; the draft session is gone with it.
        assert_eq!(chat.count("SELECT count(*) FROM submissions WHERE submitter_mxid IS NULL AND identity = 'anonymous'"), 1);
        // The whole conversation (screens, the typed feedback) is marked
        // for redaction; the receipt with the code is not.
        assert_eq!(
            chat.count("SELECT count(*) FROM dm_events WHERE doomed = 0"),
            1
        );
        assert_eq!(
            chat.count("SELECT count(*) FROM dm_events WHERE doomed = 0 AND in_session = 0"),
            1
        );
        let code = receipt.text.split('`').nth(1).unwrap().to_owned();
        // "I've saved the code": only now is that message removed.
        let out = chat.tap("✅");
        assert!(out[0].text.contains("Removed the code"));
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
        assert_eq!(
            chat.count(&format!(
                "SELECT doomed FROM dm_events WHERE event_id = '{code_event}'"
            )),
            1
        );
        // The code from the receipt manages it.
        chat.tap("🔑");
        let shown = chat.say(&code);
        assert!(screen(&shown).text.contains("isn't cleaned"));
        // The message with the code is off the screen right away.
        assert_eq!(
            chat.count(&format!(
                "SELECT doomed FROM dm_events WHERE event_id = '$e{}'",
                chat.n - 1
            )),
            1
        );
        let gone = chat.tap("🗑️");
        assert!(gone[0].text.contains("Deleted"));
        assert_eq!(chat.count("SELECT count(*) FROM submissions"), 0);
        assert_eq!(chat.count("SELECT count(*) FROM deletion_credentials"), 0);
    }

    #[test]
    fn floor_and_named_submissions() {
        let mut chat = Chat::new();
        let out = share(&mut chat, "🏠", "🧹");
        assert!(out[0].text.contains("Submitted with your floor"));
        assert_eq!(chat.count("SELECT count(*) FROM submissions WHERE submitter_mxid IS NULL AND submitter_floor_id IS NOT NULL"), 1);
        // The floor is remembered (with the person, not the submission)…
        assert_eq!(
            chat.count("SELECT count(*) FROM residents WHERE floor_id IS NOT NULL"),
            1
        );
        // …and not asked again.
        let out = share(&mut chat, "👤", "♾️");
        assert!(out[0].text.contains("Submitted with your name"));
        assert!(!out[0].text.contains('`'));
        chat.tap("🔑");
        let listed = chat.tap("1️⃣");
        assert!(screen(&listed).text.contains("👤 With my name"));
    }

    #[test]
    fn a_wrong_code_reveals_nothing_and_guessing_is_slowed() {
        let mut chat = Chat::new();
        share(&mut chat, "🥷", "♾️");
        // 🏠 Main menu keeps the code message.
        let out = chat.tap("🏠");
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
        assert_eq!(
            chat.count("SELECT count(*) FROM dm_events WHERE doomed = 0 AND in_session = 0"),
            1
        );
        chat.tap("🔑");
        for _ in 0..MAX_ATTEMPTS {
            let out = chat.say("AAAA-BBBB-CCCC");
            assert!(out[0].text.contains("doesn't match"));
        }
        let out = chat.say("AAAA-BBBB-CCCC");
        assert!(out[0].text.contains("wait a few minutes"));
    }

    #[test]
    fn cancel_and_back_work_everywhere_and_leave_no_draft() {
        let mut chat = Chat::new();
        chat.menu();
        chat.tap("📝");
        chat.tap("🧹");
        let back = chat.tap("⬅️");
        assert!(screen(&back).text.contains("What is this about?"));
        chat.tap("🧹");
        chat.tap("🙋");
        chat.tap("⏭️");
        chat.say("Something private.");
        let out = chat.tap("❌");
        // Straight back to a fresh menu, no question asked.
        assert_eq!(out.len(), 1);
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
        assert_eq!(chat.count("SELECT count(*) FROM submissions"), 0);
        assert_eq!(
            chat.count("SELECT count(*) FROM sessions WHERE flow LIKE '%private%'"),
            0
        );
        // Everything of that flow goes; only the new menu stays.
        assert_eq!(
            chat.count("SELECT count(*) FROM dm_events WHERE doomed = 0"),
            1
        );
        // Typed "cancel" works too, on a button step.
        chat.tap("📝");
        let out = chat.say("cancel");
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
    }

    #[test]
    fn help_shows_the_same_step_again_and_loses_nothing() {
        let mut chat = Chat::new();
        chat.menu();
        let step = screen(&chat.tap("📝")).text.clone();
        let out =
            chat.deliver(tokio_block(chat.engine.help(&chat.who, ROOM, chat.now, ttl())).unwrap());
        assert_eq!(out[0].kind, Kind::Note);
        assert!(out[0].text.contains("!menu"));
        // The same step, below the help, its buttons working.
        assert_eq!(screen(&out).text, step);
        assert!(chat.tap("🧹").iter().any(|m| m.live));
    }

    #[test]
    fn taps_of_another_screen_are_ignored_and_text_on_buttons_gets_a_hint() {
        let mut chat = Chat::new();
        chat.menu();
        assert!(chat.tap("🍳").is_empty());
        // The menu is there to tap on: the hint is all that is needed.
        let out = chat.say("hello?");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, Kind::Note);
    }

    #[test]
    fn a_hint_brings_the_screen_back_when_the_live_one_is_gone() {
        let mut chat = Chat::new();
        chat.menu();
        chat.tap("📝");
        let screen_event = format!("$e{}", chat.n);
        // The screen was redacted (a restart between tidying and showing,
        // a client, an admin): the session still points at it.
        chat.engine
            .db
            .with(|c| {
                c.execute("DELETE FROM dm_events WHERE event_id = ?1", [&screen_event])?;
                Ok(())
            })
            .unwrap();
        let out = chat.say("hello?");
        assert_eq!(out[0].kind, Kind::Note);
        // The same step again, below the hint, so there is something to tap.
        assert!(screen(&out).text.contains("What is this about?"));
        assert!(screen(&out).live);
        // Marked for redaction counts as gone too.
        let screen_event = format!("$e{}", chat.n);
        chat.engine
            .db
            .with(|c| {
                c.execute(
                    "UPDATE dm_events SET doomed = 1 WHERE event_id = ?1",
                    [&screen_event],
                )?;
                Ok(())
            })
            .unwrap();
        let out = chat.say("hello?");
        assert!(screen(&out).text.contains("What is this about?"));
        // With the screen in place: the hint alone, nothing is re-sent.
        let out = chat.say("hello?");
        assert_eq!(out.len(), 1);
        // No session at all (locked out, then back): a fresh menu.
        chat.engine
            .db
            .with(|c| {
                c.execute("DELETE FROM sessions", [])?;
                Ok(())
            })
            .unwrap();
        let out = chat.say("hello?");
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
    }

    #[test]
    fn a_check_in_notice_tapped_too_late_still_leaves_a_screen() {
        let mut chat = Chat::new();
        let run = with_run(&mut chat, Default::default());
        // 📊 on the notice, weeks later, no screen ever shown in this chat.
        let out = tokio_block(chat.engine.start_run(
            &chat.who,
            ROOM,
            run.id,
            run.closes_at + chrono::Duration::days(1),
            ttl(),
        ))
        .unwrap();
        assert!(out[0].text.contains("closed"), "{}", out[0].text);
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
    }

    #[test]
    fn edits_go_with_the_flow_like_screens() {
        let view = screens::View {
            text: String::new(),
            buttons: Vec::new(),
        };
        assert!(Msg::edit(view).in_session());
        assert!(!Msg::receipt(String::new()).in_session());
    }

    #[test]
    fn a_stale_draft_is_thrown_away_and_a_restart_resumes_a_fresh_one() {
        let mut chat = Chat::new();
        chat.menu();
        chat.tap("📝");
        chat.tap("🧹");
        // A restart: a new engine on the same database continues.
        chat.engine = Engine::new(
            chat.engine.db.clone(),
            Secrets::from_master(&[5; 32]),
            chat.engine.zone,
        );
        let out = chat.tap("🙋");
        assert!(screen(&out).text.contains("Where?"));
        // Two hours later the next tap finds it expired.
        chat.now += chrono::Duration::hours(2);
        let out = chat.tap("🍳");
        assert_eq!(out[0].text, screens::expired());
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
        // The scheduler's sweep does the same without any input.
        chat.tap("📝");
        chat.now += chrono::Duration::hours(2);
        let swept = tokio_block(chat.engine.expire(chat.now, ttl())).unwrap();
        assert_eq!(swept, [(ME.to_owned(), ROOM.to_owned())]);
        assert_eq!(chat.count("SELECT count(*) FROM sessions"), 0);
    }

    // ── Check-ins ──────────────────────────────────────────────────────────

    fn with_run(chat: &mut Chat, opts: crate::checkins::StartOptions) -> Run {
        let now = chat.now;
        chat.engine
            .db
            .with(|c| Ok(crate::checkins::tests::started(c, now, opts)))
            .unwrap()
    }

    /// Menu → 📊 → the run → ▶️ → 😐 3, tick the first problem, a text,
    /// 🥷 → preview.
    fn answer_all(chat: &mut Chat) -> Vec<Msg> {
        let menu = chat.menu();
        assert!(screen(&menu).text.contains("**Check-ins · 1 open**"));
        let list = chat.tap("📊");
        assert!(screen(&list).text.contains("🟢 🧹 **Cleaning**"));
        let intro = chat.tap("1️⃣");
        assert!(screen(&intro).text.contains("📝 3 questions"));
        let q1 = chat.tap("▶️");
        assert!(screen(&q1).text.contains("Cleaning check-in · 1/3"));
        let q2 = chat.tap("😐");
        assert!(screen(&q2).text.contains("⬜ Cleaning turns are skipped"));
        // Ticking edits the question in place.
        let ticked = chat.tap("🙋");
        assert_eq!(ticked.len(), 1);
        assert_eq!(ticked[0].kind, Kind::Edit);
        assert!(ticked[0].text.contains("☑️ Cleaning turns are skipped"));
        let q3 = chat.tap("➡️");
        assert!(screen(&q3).text.contains("What would you change?"));
        let share = chat.say("Fewer, clearer tasks per person.");
        assert!(screen(&share).text.contains("share this as an idea"));
        let identity = chat.tap("🔒");
        assert!(screen(&identity).text.contains("submit your answers"));
        let preview = chat.tap("🥷");
        assert!(screen(&preview).text.contains("📊 **Ready to submit**"));
        assert!(screen(&preview).text.contains("→ 😐 3"));
        preview
    }

    #[test]
    fn answering_a_check_in_end_to_end() {
        let mut chat = Chat::new();
        let run = with_run(&mut chat, Default::default());
        answer_all(&mut chat);
        // Nothing stored before ✅.
        assert_eq!(chat.count("SELECT count(*) FROM responses"), 0);
        let out = chat.tap("✅");
        assert!(out[0].text.contains("Thanks! Your answers were submitted."));
        assert!(out[0].text.contains("🥷 Anonymous"));
        assert!(
            screen(&out).text.contains("📊 Check-ins\n"),
            "{}",
            screen(&out).text
        );
        assert_eq!(
            chat.count("SELECT count(*) FROM responses WHERE identity = 'anonymous' AND respondent_mxid IS NULL"),
            1
        );
        assert_eq!(
            chat.count(
                "SELECT count(*) FROM answers WHERE text = 'Fewer, clearer tasks per person.'"
            ),
            1
        );
        assert_eq!(chat.count("SELECT count(*) FROM answer_choices"), 1);
        assert_eq!(chat.count("SELECT count(*) FROM participations"), 1);
        // The draft is gone with it, and so is the conversation in the chat.
        assert_eq!(
            chat.count("SELECT count(*) FROM sessions WHERE flow LIKE '%Fewer%'"),
            0
        );
        assert_eq!(
            chat.count("SELECT count(*) FROM dm_events WHERE doomed = 0 AND in_session = 1"),
            1
        );
        // Completed: listed so, not offered again, no reminder.
        let list = chat.tap("📊");
        assert!(screen(&list).text.contains("✅ 🧹 Cleaning · Completed"));
        assert!(screen(&list).text.contains("No open check-ins right now"));
        let again = tokio_block(
            chat.engine
                .start_run(&chat.who, ROOM, run.id, chat.now, ttl()),
        )
        .unwrap();
        assert!(again[0].text.contains("already answered"));
        let later = run.reminder_at.unwrap();
        assert!(!tokio_block(chat.engine.should_tell(&run, &chat.who, later)).unwrap());
    }

    #[test]
    fn a_check_in_survives_a_restart_and_back_and_cancel_work() {
        let mut chat = Chat::new();
        with_run(&mut chat, Default::default());
        chat.menu();
        chat.tap("📊");
        chat.tap("1️⃣");
        chat.tap("▶️");
        chat.tap("🙂");
        chat.engine = Engine::new(
            chat.engine.db.clone(),
            Secrets::from_master(&[5; 32]),
            chat.engine.zone,
        );
        // Back to question 1: the answer is still there.
        let q1 = chat.tap("⬅️");
        assert!(screen(&q1).text.contains("Your answer so far: 🙂 4"));
        let out = chat.tap("❌");
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
        assert_eq!(chat.count("SELECT count(*) FROM responses"), 0);
        assert_eq!(chat.count("SELECT count(*) FROM participations"), 0);
        assert_eq!(
            chat.count("SELECT count(*) FROM sessions WHERE flow LIKE '%checkin%run%'"),
            0
        );
    }

    #[test]
    fn losing_access_mid_check_in_saves_nothing() {
        let mut chat = Chat::new();
        with_run(&mut chat, Default::default());
        answer_all(&mut chat);
        // They left the house room just before ✅.
        chat.who.rooms.clear();
        let out = chat.tap("✅");
        assert!(out[0].text.contains("isn't for you"));
        assert_eq!(chat.count("SELECT count(*) FROM responses"), 0);
        assert_eq!(chat.count("SELECT count(*) FROM participations"), 0);
    }

    #[test]
    fn a_room_target_only_reaches_that_rooms_members() {
        let mut chat = Chat::new();
        let run = with_run(
            &mut chat,
            crate::checkins::StartOptions {
                target: crate::checkins::Target::Room {
                    room_id: "!kitchen2:x".into(),
                    label: "Kitchen 2".into(),
                },
                ..Default::default()
            },
        );
        let menu = chat.menu();
        assert!(!screen(&menu).text.contains("open**"));
        let out = tokio_block(
            chat.engine
                .start_run(&chat.who, ROOM, run.id, chat.now, ttl()),
        )
        .unwrap();
        assert!(out[0].text.contains("isn't for you"));
        chat.who.rooms.push("!kitchen2:x".into());
        let out = tokio_block(
            chat.engine
                .start_run(&chat.who, ROOM, run.id, chat.now, ttl()),
        )
        .unwrap();
        assert!(screen(&out).text.contains("Cleaning check-in"));
    }

    #[test]
    fn a_floor_answer_and_a_named_one() {
        let mut chat = Chat::new();
        let run = with_run(&mut chat, Default::default());
        answer_all(&mut chat);
        // Back to identity, choose the floor (asked once), submit.
        chat.tap("⬅️");
        let floors = chat.tap("🏠");
        assert!(screen(&floors).text.contains("Which floor do you live on?"));
        chat.tap("2️⃣");
        chat.tap("✅");
        assert_eq!(
            chat.count("SELECT count(*) FROM responses WHERE identity = 'floor' AND respondent_mxid IS NULL AND respondent_floor_id IS NOT NULL"),
            1
        );
        // Another person, named.
        chat.who.mxid = "@bob:example.org".into();
        chat.engine
            .db
            .with(|c| {
                c.execute(
                    "INSERT INTO residents (mxid, created_at) VALUES ('@bob:example.org', 'x')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let out = tokio_block(
            chat.engine
                .start_run(&chat.who, ROOM, run.id, chat.now, ttl()),
        )
        .unwrap();
        assert!(screen(&out).text.contains("📝 3 questions"));
        chat.deliver(out);
        chat.tap("▶️");
        chat.tap("😄");
        // Nothing ticked on an optional question: ➡️ skips it.
        chat.tap("➡️");
        chat.tap("⏭️");
        chat.tap("👤");
        let out = chat.tap("✅");
        assert!(out[0].text.contains("👤 With my name"));
        assert_eq!(
            chat.count("SELECT count(*) FROM responses WHERE respondent_mxid = '@bob:example.org'"),
            1
        );
        assert_eq!(chat.count("SELECT count(*) FROM participations"), 2);
        assert_eq!(
            chat.count("SELECT count(*) FROM answers WHERE skipped = 1"),
            2
        );
    }

    // ── Ideas (M3) ─────────────────────────────────────────────────────────

    /// Menu → 📝 → 💡 idea → no place → the idea text → the share screen.
    fn share_screen(chat: &mut Chat, text: &str) -> Vec<Msg> {
        chat.menu();
        chat.tap("📝");
        chat.tap("💡");
        chat.tap("⏭️");
        let share = chat.say(text);
        assert!(
            screen(&share)
                .text
                .contains("Would you like to share this as an idea?"),
            "{}",
            screen(&share).text
        );
        assert!(screen(&share).text.contains(text));
        share
    }

    /// From the share screen: `choice`, then 🥷, ♾️ and ✅.
    fn share_and_submit(chat: &mut Chat, choice: &str) -> Vec<Msg> {
        let next = chat.tap(choice);
        assert!(
            screen(&next).text.contains("How would you like to submit"),
            "{}",
            screen(&next).text
        );
        chat.tap("🥷");
        let preview = chat.tap("♾️");
        assert!(screen(&preview).text.contains("Ready to submit"));
        chat.tap("✅")
    }

    #[test]
    fn a_private_or_plenum_only_idea_is_never_published() {
        let mut chat = Chat::new();
        share_screen(&mut chat, "A quiet hour after 22:00");
        let out = share_and_submit(&mut chat, "🔒");
        assert!(out[0].text.contains("Submitted anonymously"));
        assert_eq!(chat.count("SELECT count(*) FROM ideas"), 0);
        assert_eq!(
            chat.count("SELECT count(*) FROM submissions WHERE for_plenum = 0"),
            1
        );
        share_screen(&mut chat, "Plants for the staircase");
        share_and_submit(&mut chat, "🗣️");
        assert_eq!(chat.count("SELECT count(*) FROM ideas"), 0);
        assert_eq!(
            chat.count("SELECT count(*) FROM submissions WHERE for_plenum = 1"),
            1
        );
    }

    #[test]
    fn sharing_an_idea_takes_consent_and_links_it_to_nobody() {
        let mut chat = Chat::new();
        share_screen(&mut chat, "Clean the bathrooms twice per week");
        let out = share_and_submit(&mut chat, "🌍");
        assert!(
            out[0].text.contains("Shared on the 💡 Ideas board"),
            "{}",
            out[0].text
        );
        assert_eq!(
            chat.count(
                "SELECT count(*) FROM ideas WHERE title = 'Clean the bathrooms twice per week'"
            ),
            1
        );
        // Nothing ties the idea to its author or to the submission.
        let columns: Vec<String> = chat
            .engine
            .db
            .with(|c| {
                Ok(c.prepare("SELECT name FROM pragma_table_info('ideas')")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        for col in &columns {
            assert!(
                !col.contains("mxid") && !col.contains("submission") && !col.contains("author"),
                "{col}"
            );
        }
        // The submission keeps its own (anonymous) identity.
        assert_eq!(
            chat.count("SELECT count(*) FROM submissions WHERE identity = 'anonymous' AND submitter_mxid IS NULL"),
            1
        );
        // On the board, supported without names.
        chat.menu();
        let board = chat.tap("💡");
        assert!(screen(&board)
            .text
            .contains("Clean the bathrooms twice per week"));
        let page = chat.tap("1️⃣");
        assert!(screen(&page).text.contains("👍 0"));
        let supported = chat.tap("👍");
        // One message: the screen, changed in place, says it (no extra note).
        assert_eq!(supported.len(), 1);
        assert_eq!(supported[0].kind, Kind::Edit);
        assert!(supported[0].text.contains("✅ You support this idea."));
        assert!(supported[0].text.contains("👍 1"));
        // A second 👍 isn't offered (and wouldn't count): only take it back,
        // and that button is there to tap (deliver adds it to the edit).
        assert!(!supported[0].buttons.contains(&"👍".to_owned()));
        assert!(supported[0].buttons.contains(&"↩️".to_owned()));
        for msg in &supported {
            assert!(!msg.text.contains(ME));
        }
        // A comment, shown without a name.
        chat.tap("💬");
        let out = chat.say("Saturday mornings would suit me");
        assert!(out[0].text.contains("without your name"));
        assert!(screen(&out)
            .text
            .contains("Saturday mornings would suit me"));
        assert!(!screen(&out).text.contains(ME));
    }

    #[test]
    fn a_similar_idea_is_offered_and_the_resident_decides() {
        let mut chat = Chat::new();
        chat.engine
            .db
            .with(|c| {
                Ok(crate::ideas::tests::share(
                    c,
                    "Clean the bathrooms twice per week",
                    "cleaning",
                ))
            })
            .unwrap();
        share_screen(&mut chat, "Bathroom cleaning twice a week");
        let similar = chat.tap("🌍");
        assert!(screen(&similar)
            .text
            .contains("A similar idea already exists"));
        assert!(screen(&similar)
            .text
            .contains("Clean the bathrooms twice per week"));
        let out = share_and_submit_from(&mut chat, "👍");
        assert!(
            out[0].text.contains("Added your support"),
            "{}",
            out[0].text
        );
        // Supported, not merged and not duplicated.
        assert_eq!(chat.count("SELECT count(*) FROM ideas"), 1);
        assert_eq!(chat.count("SELECT count(*) FROM idea_supports"), 1);
        // Or separately, if they want.
        share_screen(&mut chat, "Bathroom cleaning twice a week");
        chat.tap("🌍");
        share_and_submit_from(&mut chat, "➕");
        assert_eq!(chat.count("SELECT count(*) FROM ideas"), 2);
    }

    /// From the similar-idea screen: `choice`, then 🥷, ♾️ and ✅.
    fn share_and_submit_from(chat: &mut Chat, choice: &str) -> Vec<Msg> {
        share_and_submit(chat, choice)
    }

    #[test]
    fn a_check_in_suggestion_becomes_an_idea_only_with_consent() {
        let mut chat = Chat::new();
        with_run(&mut chat, Default::default());
        chat.menu();
        chat.tap("📊");
        chat.tap("1️⃣");
        chat.tap("▶️");
        chat.tap("😐");
        chat.tap("➡️");
        chat.say("Fewer, clearer tasks per person.");
        let identity = chat.tap("🌍");
        assert!(screen(&identity).text.contains("submit your answers"));
        let preview = chat.tap("🥷");
        assert!(screen(&preview)
            .text
            .contains("Also shared on the Ideas board"));
        chat.tap("✅");
        assert_eq!(
            chat.count("SELECT count(*) FROM ideas WHERE origin = 'checkin' AND title = 'Fewer, clearer tasks per person.'"),
            1
        );
        // The answer itself stays in the response, unchanged.
        assert_eq!(
            chat.count(
                "SELECT count(*) FROM answers WHERE text = 'Fewer, clearer tasks per person.'"
            ),
            1
        );
    }

    #[test]
    fn results_and_ideas_open_from_the_menu() {
        let mut chat = Chat::new();
        chat.menu();
        let results = chat.tap("📈");
        assert!(screen(&results).text.contains("📈 **Results**"));
        assert!(screen(&results).text.contains("📮 **Current round**"));
        let history = chat.tap("📚");
        assert!(screen(&history)
            .text
            .contains("No plenum has been completed yet"));
        chat.menu();
        let ideas = chat.tap("💡");
        assert!(screen(&ideas).text.contains("No ideas yet"));
        // 📈 under a summary opens the chat right there.
        let out = tokio_block(
            chat.engine
                .open_at(&chat.who, ROOM, MenuItem::Results, chat.now),
        )
        .unwrap();
        assert!(out[0].text.contains("📈 **Results**"));
    }
}
