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
    /// The bot's own screen events seen coming back through sync, newest
    /// last: an echo can arrive before the sender has noted the event as
    /// the screen (a send from the scheduler runs beside sync).
    echoes: std::collections::VecDeque<String>,
}

/// How many recent echoes are remembered.
const ECHOES: usize = 64;

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
    /// The saved flow's view isn't confirmed on Matrix (see migration 7).
    pending: bool,
    /// The bot's event showing this view came back through sync (migration 8).
    echoed: bool,
}

impl Session {
    /// The chat may not be showing this flow's view yet: a tap now may be
    /// meant for the view before, and must not act on this one.
    fn stale(&self) -> bool {
        self.pending || !self.echoed
    }
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
    /// Survey-only mode (`ui = "survey"`): residents see nothing but the
    /// check-ins admins start. See [`Engine::survey_home`].
    survey: bool,
}

impl Engine {
    pub fn new(db: Db, secrets: Secrets, zone: Zone) -> Self {
        Self {
            db,
            secrets,
            zone,
            memory: Arc::default(),
            threshold: Arc::new(|| 3),
            survey: false,
        }
    }

    /// Survey-only mode on or off.
    pub fn survey_only(mut self, on: bool) -> Self {
        self.survey = on;
        self
    }

    pub fn is_survey(&self) -> bool {
        self.survey
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
                if this.survey {
                    let msgs = this.survey_home(conn, &who, &room_id, now, false)?;
                    return finish(conn, &who.mxid, msgs);
                }
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
                        survey: false,
                    },
                );
                save(conn, &who.mxid, &room_id, &flow, now)?;
                let msgs = vec![Msg::screen(this.render(conn, &who, &flow, now)?)];
                finish(conn, &who.mxid, msgs)
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
                let was_pending =
                    load(conn, &who.mxid)?.is_some_and(|s| s.room == room_id && s.stale());
                let msgs = this.handle_sync(conn, &who, &room_id, input, now, ttl)?;
                let msgs = this.with_screen(conn, &who, &room_id, now, msgs, was_pending)?;
                finish(conn, &who.mxid, msgs)
            })
            .await
    }

    /// The screen the chat should be showing right now, to show it again:
    /// after a restart, or when the last one wasn't confirmed as shown. An
    /// unfinished flow older than `ttl` is thrown away first, as a tap
    /// would.
    pub async fn current_screen(
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
                let session = load(conn, &who.mxid)?.filter(|s| s.room == room_id);
                if this.survey {
                    let msgs = match session {
                        Some(s)
                            if in_checkin(&s.flow)
                                && !this.draft_over(conn, &s.flow, s.updated, now, ttl)? =>
                        {
                            vec![Msg::screen(this.render(conn, &who, &s.flow, now)?)]
                        }
                        _ => this.survey_home(conn, &who, &room_id, now, false)?,
                    };
                    return finish(conn, &who.mxid, msgs);
                }
                let flow = match session {
                    Some(s) if this.draft_over(conn, &s.flow, s.updated, now, ttl)? => {
                        return this.reset(conn, &who, &room_id, screens::expired(), now);
                    }
                    Some(s) => s.flow,
                    _ => {
                        tidy(conn, &room_id)?;
                        save(conn, &who.mxid, &room_id, &Flow::default(), now)?;
                        Flow::default()
                    }
                };
                let msgs = vec![Msg::screen(this.render(conn, &who, &flow, now)?)];
                finish(conn, &who.mxid, msgs)
            })
            .await
    }

    /// `msgs` with a screen added when they hold none and the chat has no
    /// live screen to tap on (never had one, or it was redacted: the flow
    /// ended, a restart in the middle of a step), or when `stale`: the
    /// screen shown may not be the one the flow is at. A note alone would
    /// leave them with nothing but `!menu`, or with a screen that lies.
    fn with_screen(
        &self,
        conn: &Connection,
        who: &Who,
        room_id: &str,
        now: DateTime<Utc>,
        mut msgs: Vec<Msg>,
        stale: bool,
    ) -> Result<Vec<Msg>> {
        if msgs.iter().any(|m| m.live) {
            return Ok(msgs);
        }
        let session = load(conn, &who.mxid)?.filter(|s| s.room == room_id);
        let alive = match session.as_ref().and_then(|s| s.screen_event.as_deref()) {
            Some(event) => screen_alive(conn, room_id, event)?,
            None => false,
        };
        if alive && !stale {
            return Ok(msgs);
        }
        // Survey-only mode: a screen only while answering a check-in; with
        // none waiting, the chat stays quiet.
        if self.survey && !session.as_ref().is_some_and(|s| in_checkin(&s.flow)) {
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
                if this.survey {
                    let msgs = this.survey_home(conn, &who, &room_id, now, false)?;
                    return finish(conn, &who.mxid, msgs);
                }
                this.forget(&who.mxid);
                tidy(conn, &room_id)?;
                save(conn, &who.mxid, &room_id, &Flow::default(), now)?;
                let msgs = vec![Msg::screen(this.render(
                    conn,
                    &who,
                    &Flow::default(),
                    now,
                )?)];
                finish(conn, &who.mxid, msgs)
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
                let session = load(conn, &who.mxid)?.filter(|s| s.room == room_id);
                if this.survey {
                    let msgs = match session {
                        Some(s)
                            if in_checkin(&s.flow)
                                && !this.draft_over(conn, &s.flow, s.updated, now, ttl)? =>
                        {
                            vec![
                                Msg::note(&screens::survey_help()),
                                Msg::screen(this.render(conn, &who, &s.flow, now)?),
                            ]
                        }
                        _ => match this.survey_home(conn, &who, &room_id, now, false)? {
                            msgs if msgs.is_empty() => vec![Msg::note(screens::nothing_open())],
                            msgs => {
                                let mut out = vec![Msg::note(&screens::survey_help())];
                                out.extend(msgs);
                                out
                            }
                        },
                    };
                    return finish(conn, &who.mxid, msgs);
                }
                let flow = match session {
                    Some(s) if !this.draft_over(conn, &s.flow, s.updated, now, ttl)? => s.flow,
                    _ => {
                        tidy(conn, &room_id)?;
                        save(conn, &who.mxid, &room_id, &Flow::default(), now)?;
                        Flow::default()
                    }
                };
                let msgs = vec![
                    Msg::note(&screens::help()),
                    Msg::screen(this.render(conn, &who, &flow, now)?),
                ];
                finish(conn, &who.mxid, msgs)
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
                let msgs = this.with_screen(conn, &who, &room_id, now, msgs, false)?;
                finish(conn, &who.mxid, msgs)
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

    /// The bot saw its own event showing `screen_event` (or an edit of it)
    /// come back through sync: taps from here on were made with the view
    /// in sight.
    pub async fn screen_echoed(&self, room_id: &str, screen_event: &str) -> Result<()> {
        {
            let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.echoes.push_back(screen_event.to_owned());
            while memory.echoes.len() > ECHOES {
                memory.echoes.pop_front();
            }
        }
        let (room_id, screen_event) = (room_id.to_owned(), screen_event.to_owned());
        self.db
            .run(move |conn| {
                conn.execute(
                    "UPDATE sessions SET echoed = 1 WHERE room_id = ?1 AND screen_event_id = ?2",
                    params![room_id, screen_event],
                )?;
                Ok(())
            })
            .await
    }

    /// The screen `event_id` shows the saved flow now. `complete`: with all
    /// its buttons; if not, it stays pending and is brought in line at the
    /// next tap (or start).
    pub async fn screen_shown(&self, mxid: &str, event_id: &str, complete: bool) -> Result<()> {
        let echoed_already = self
            .memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .echoes
            .iter()
            .any(|e| e == event_id);
        let (mxid, event_id) = (mxid.to_owned(), event_id.to_owned());
        self.db
            .run(move |conn| {
                conn.execute(
                    "UPDATE sessions SET screen_event_id = ?2, pending = ?3 WHERE mxid = ?1",
                    params![mxid, event_id, !complete],
                )?;
                if echoed_already {
                    conn.execute("UPDATE sessions SET echoed = 1 WHERE mxid = ?1", [&mxid])?;
                }
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
                let sessions: Vec<(String, String, String, String)> = {
                    let mut stmt =
                        conn.prepare("SELECT mxid, room_id, flow, updated_at FROM sessions")?;
                    let rows =
                        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                let mut out = Vec::new();
                for (mxid, room_id, json, updated) in sessions {
                    let flow = Flow::restore(&json);
                    let updated = time::parse_text(&updated)?;
                    if !this.draft_over(conn, &flow, updated, now, ttl)? {
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
        let view = screens::view(flow, &ctx);
        Ok(if self.survey && in_checkin(flow) {
            screens::survey_view(view, flow.screen())
        } else {
            view
        })
    }

    /// Whether an unfinished flow is to be thrown away: one older than
    /// `ttl`. In survey-only mode a check-in waits for them until it
    /// closes, however long they pause.
    fn draft_over(
        &self,
        conn: &Connection,
        flow: &Flow,
        updated: DateTime<Utc>,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<bool> {
        if !flow.in_progress() {
            return Ok(false);
        }
        if let (true, Some(id)) = (self.survey, flow.run()) {
            // Gone altogether counts as closed.
            return Ok(checkins::run(conn, id).map_or(true, |run| !run.is_open(now)));
        }
        Ok(now - updated > ttl)
    }

    /// Survey-only mode: the check-in waiting for `who` (open, theirs, not
    /// answered yet), the one closing first.
    fn next_survey(&self, conn: &Connection, who: &Who, now: DateTime<Utc>) -> Result<Option<i64>> {
        let floor = own_floor(conn, &who.mxid)?;
        Ok(
            checkins::listing(conn, &self.secrets, &who.mxid, &who.rooms, floor, now)?
                .into_iter()
                .find(|l| l.open && !l.completed)
                .map(|l| l.id),
        )
    }

    /// Survey-only mode: whether a check-in is waiting for `who`.
    pub async fn has_survey_for(&self, who: &Who, now: DateTime<Utc>) -> Result<bool> {
        let (this, who) = (self.clone(), who.clone());
        self.db
            .run(move |conn| Ok(this.next_survey(conn, &who, now)?.is_some()))
            .await
    }

    /// Survey-only mode: check-in `run_id` from its intro, whatever was in
    /// the chat before tidied away. Its flow is `[Menu, CheckinIntro]`: the
    /// menu underneath is never shown (see [`flow::SURVEY_DEPTH`]).
    fn begin_survey(
        &self,
        conn: &Connection,
        who: &Who,
        room_id: &str,
        run_id: i64,
        now: DateTime<Utc>,
    ) -> Result<Vec<Msg>> {
        self.forget(&who.mxid);
        tidy(conn, room_id)?;
        let mut flow = Flow::default();
        flow.draft.checkin = Some(CheckinDraft {
            run: run_id,
            answers: Default::default(),
            identity: None,
            ideas: Default::default(),
        });
        flow.push(Screen::CheckinIntro);
        save(conn, &who.mxid, room_id, &flow, now)?;
        Ok(vec![Msg::screen(self.render(conn, who, &flow, now)?)])
    }

    /// Survey-only mode, where residents are when not answering one: the
    /// next check-in waiting for them, from its intro. With none, the chat
    /// is tidied and stays quiet, or says so (`say_nothing`) when they
    /// wrote something.
    fn survey_home(
        &self,
        conn: &Connection,
        who: &Who,
        room_id: &str,
        now: DateTime<Utc>,
        say_nothing: bool,
    ) -> Result<Vec<Msg>> {
        if let Some(run_id) = self.next_survey(conn, who, now)? {
            return self.begin_survey(conn, who, room_id, run_id, now);
        }
        self.forget(&who.mxid);
        tidy(conn, room_id)?;
        save(conn, &who.mxid, room_id, &Flow::default(), now)?;
        // No screen: nothing to tap on any more.
        conn.execute(
            "UPDATE sessions SET screen_event_id = NULL, pending = 0, echoed = 1 WHERE mxid = ?1",
            [&who.mxid],
        )?;
        Ok(if say_nothing {
            vec![Msg::note(screens::nothing_open())]
        } else {
            Vec::new()
        })
    }

    /// Survey-only mode: what a check-in's notice or reminder puts into
    /// `who`'s chat (they were checked to be asked and not done). `None`:
    /// nothing now. Either they are answering another check-in (this one
    /// comes up when that is done or closes), or the notice is on their
    /// screen already.
    ///
    /// The notice is the check-in itself: its intro, a message of its own
    /// (so it notifies). The reminder is a short note with the screen they
    /// are on right below it, fresh at the bottom.
    pub async fn survey_message(
        &self,
        who: &Who,
        room_id: &str,
        run_id: i64,
        which: CheckinText,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Option<Vec<Msg>>> {
        let this = self.clone();
        let (who, room_id) = (who.clone(), room_id.to_owned());
        self.db
            .run(move |conn| {
                let session = load(conn, &who.mxid)?.filter(|s| s.room == room_id);
                let current = match session {
                    Some(s)
                        if in_checkin(&s.flow)
                            && !this.draft_over(conn, &s.flow, s.updated, now, ttl)? =>
                    {
                        Some(s)
                    }
                    _ => None,
                };
                let run = checkins::run(conn, run_id)?;
                // Answered (or closed) meanwhile: never asked again.
                if !run.is_open(now) || checkins::completed(conn, &this.secrets, run_id, &who.mxid)?
                {
                    return Ok(None);
                }
                let reminder = || {
                    let ctx = screens::Ctx {
                        zone: Some(this.zone),
                        today: Some(this.zone.date(now)),
                        ..Default::default()
                    };
                    Msg::note(&screens::survey_reminder(&run, &ctx))
                };
                let msgs = match (current, which) {
                    (_, CheckinText::Announcement) => return Ok(None),
                    (Some(s), _) if s.flow.run() != Some(run_id) => return Ok(None),
                    // Shown already; only re-sent if that never got through.
                    (Some(s), CheckinText::Notice) if !s.pending => return Ok(None),
                    (Some(s), CheckinText::Notice) => {
                        vec![Msg::screen(this.render(conn, &who, &s.flow, now)?)]
                    }
                    (Some(s), CheckinText::Reminder) => vec![
                        reminder(),
                        Msg::screen(this.render(conn, &who, &s.flow, now)?),
                    ],
                    (None, CheckinText::Notice) => {
                        this.begin_survey(conn, &who, &room_id, run_id, now)?
                    }
                    (None, CheckinText::Reminder) => {
                        let mut msgs = vec![reminder()];
                        msgs.extend(this.begin_survey(conn, &who, &room_id, run_id, now)?);
                        msgs
                    }
                };
                Ok(Some(finish(conn, &who.mxid, msgs)?))
            })
            .await
    }

    /// Survey-only mode, at startup: every chat not answering a check-in
    /// loses what the full Kummerkasten left there (a menu, an unfinished
    /// feedback draft, notes): tidied, its session dropped. Returns
    /// `(room, receipt)` of deletion-code receipts that were the live
    /// screen: the message stays (it holds their code), its buttons go.
    pub async fn survey_cleanup(
        &self,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<Vec<(String, String)>> {
        let this = self.clone();
        self.db
            .run(move |conn| {
                let sessions: Vec<(String, String, String, String, Option<String>)> = {
                    let mut stmt = conn.prepare(
                        "SELECT mxid, room_id, flow, updated_at, screen_event_id FROM sessions",
                    )?;
                    let rows = stmt.query_map([], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                let mut receipts = Vec::new();
                for (mxid, room_id, json, updated, screen) in sessions {
                    let flow = Flow::restore(&json);
                    let updated = time::parse_text(&updated)?;
                    if in_checkin(&flow) && !this.draft_over(conn, &flow, updated, now, ttl)? {
                        continue;
                    }
                    if let (Screen::CodeSaved, Some(receipt)) = (flow.screen(), screen) {
                        // Its buttons only (tracked with `replaces`).
                        conn.execute(
                            "UPDATE dm_events SET doomed = 1 WHERE room_id = ?1 AND replaces = ?2",
                            params![room_id, receipt],
                        )?;
                        receipts.push((room_id.clone(), receipt));
                    }
                    this.forget(&mxid);
                    tidy(conn, &room_id)?;
                    conn.execute("DELETE FROM sessions WHERE mxid = ?1", [&mxid])?;
                }
                Ok(receipts)
            })
            .await
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
        if self.survey {
            let mut msgs = vec![Msg::note(note)];
            msgs.extend(self.survey_home(conn, who, room_id, now, false)?);
            return Ok(msgs);
        }
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
                if self.draft_over(conn, &session.flow, session.updated, now, ttl)? {
                    // Stale: tidy up and start over, ignoring the input. (In
                    // survey-only mode only a check-in that closed meanwhile.)
                    let note = match session.flow.run() {
                        Some(_) if self.survey => Refused::Closed.message(),
                        _ => screens::expired(),
                    };
                    return self.reset(conn, who, room_id, note, now);
                }
                if session.stale() {
                    // The chat may still show the view before this one (a
                    // restart or a failed edit in between; the edit not yet
                    // delivered when a second device tapped): a tap or an
                    // answer meant for that view must not act on this one
                    // (a typed "3" would pick 3 here). Show this one.
                    let view = Msg::screen(self.render(conn, who, &session.flow, now)?);
                    return Ok(match input {
                        Input::Tap(_) => vec![view],
                        Input::Text { .. } => vec![Msg::note(screens::shown_again()), view],
                    });
                }
                (session.flow, session.screen_event)
            }
            _ => (Flow::default(), None),
        };
        if self.survey && !in_checkin(&flow) {
            // Nothing being answered: a message brings up the check-in
            // waiting for them (or says there is none); a tap on whatever
            // is left over does nothing.
            return match input {
                Input::Text { .. } => self.survey_home(conn, who, room_id, now, true),
                Input::Tap(_) => Ok(Vec::new()),
            };
        }
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
                None => {
                    // Not a button of this view. One the bot had put there
                    // (an option retired meanwhile) is cleared by showing
                    // the view again; anything else (a 🦄) is left alone.
                    let was_button = match &screen_event {
                        Some(event) => crate::store::screen_buttons(conn, room_id, event)?
                            .unwrap_or_default()
                            .iter()
                            .any(|(k, _)| screens::same_key(k, &key)),
                        None => false,
                    };
                    return Ok(if was_button {
                        vec![Msg::edit(view)]
                    } else {
                        Vec::new()
                    });
                }
            },
            Input::Text { body, event_id } => {
                // A deletion code they type is a secret: off the screen now.
                let secret = (flow.screen() == &Screen::ManageEnter).then_some(event_id);
                // Survey-only mode has no ❌: "stop" or "cancel" is an
                // answer there (or a hint on a button step), never a reset.
                let typed =
                    screens::typed(&view, &body).filter(|a| !(self.survey && *a == Action::Cancel));
                if flow.wants_text(run.as_ref()) {
                    match typed {
                        // "cancel"/"back" typed, but never on a text step
                        // where it might be the actual answer.
                        Some(a @ (Action::Back | Action::Cancel)) if body.trim().len() < 12 => {
                            (a, secret)
                        }
                        Some(a @ Action::Named(_)) => (a, secret),
                        _ => (Action::Text(body), secret),
                    }
                } else {
                    (typed.unwrap_or(Action::Text(body)), secret)
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
            survey: self.survey,
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
        if self.survey && !in_checkin(&flow) {
            // Done (submitted): the receipt, then the next check-in waiting
            // for them if there is one. Never a menu.
            out.extend(self.survey_home(conn, who, room_id, now, false)?);
            return Ok(out);
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
        if let Some(s) = &session {
            let open =
                in_checkin(&s.flow) && !self.draft_over(conn, &s.flow, s.updated, now, ttl)?;
            // Already answering it: show where they are.
            if open && s.flow.run() == Some(run_id) {
                return Ok(vec![Msg::screen(self.render(conn, who, &s.flow, now)?)]);
            }
            // Survey-only mode: one at a time, the half-done one first.
            if open && self.survey {
                if let Some(other) = s.flow.run() {
                    let other = checkins::run(conn, other)?;
                    return Ok(vec![
                        Msg::note(&screens::finish_first(&other)),
                        Msg::screen(self.render(conn, who, &s.flow, now)?),
                    ]);
                }
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
        if self.survey {
            return self.begin_survey(conn, who, room_id, run_id, now);
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

/// The check-in `mxid`'s chat is answering, if any.
pub fn session_run(conn: &Connection, mxid: &str) -> Result<Option<i64>> {
    Ok(load(conn, mxid)?
        .filter(|s| in_checkin(&s.flow))
        .and_then(|s| s.flow.run()))
}

/// The flow is answering a check-in (somewhere past the menu).
fn in_checkin(flow: &Flow) -> bool {
    flow.run().is_some() && flow.in_progress()
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

/// Mark one message in `room_id` for redaction, with its edits and buttons.
fn doom(conn: &Connection, room_id: &str, event_id: &str) -> Result<()> {
    crate::store::doom_event(conn, room_id, event_id).map(drop)
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
    let row: Option<(String, String, String, Option<String>, bool, bool)> = conn
        .query_row(
            "SELECT flow, updated_at, room_id, screen_event_id, pending, echoed FROM sessions
             WHERE mxid = ?1",
            [mxid],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    Ok(match row {
        Some((json, updated, room, screen_event, pending, echoed)) => Some(Session {
            flow: Flow::restore(&json),
            updated: time::parse_text(&updated)?,
            room,
            screen_event,
            pending,
            echoed,
        }),
        None => None,
    })
}

/// `msgs` as the answer to an interaction: when they hold a screen, the
/// session is marked pending until `dm` confirms it was shown, and not
/// echoed until the bot sees its own event showing it. (`save` already did
/// so for a changed step, in the same statement; this covers a screen shown
/// again unchanged.)
fn finish(conn: &Connection, mxid: &str, msgs: Vec<Msg>) -> Result<Vec<Msg>> {
    if msgs.iter().any(|m| m.live) {
        conn.execute(
            "UPDATE sessions SET pending = 1, echoed = 0 WHERE mxid = ?1",
            [mxid],
        )?;
    }
    Ok(msgs)
}

/// Chats whose screen is not confirmed as shown: `(mxid, room_id)`.
pub fn pending_screens(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT mxid, room_id FROM sessions WHERE pending = 1")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn save(
    conn: &Connection,
    mxid: &str,
    room_id: &str,
    flow: &Flow,
    now: DateTime<Utc>,
) -> Result<()> {
    // A step that changed isn't shown yet: pending (and not echoed) in the
    // same statement, so a crash right after saving can't leave the chat
    // showing the step before as if it were this one (see `finish`).
    conn.execute(
        "INSERT INTO sessions (mxid, room_id, flow, updated_at, pending, echoed)
         VALUES (?1, ?2, ?3, ?4, 1, 0)
         ON CONFLICT (mxid) DO UPDATE SET room_id = ?2, flow = ?3, updated_at = ?4,
             pending = CASE WHEN sessions.flow IS ?3 THEN sessions.pending ELSE 1 END,
             echoed = CASE WHEN sessions.flow IS ?3 THEN sessions.echoed ELSE 0 END",
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
    use crate::plenums::tests::{at, with_house_calendar};

    const ME: &str = "@alice:example.org";
    const ROOM: &str = "!dm:example.org";

    fn engine(now: DateTime<Utc>) -> Engine {
        let db = Db::from_connection(with_house_calendar(now));
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
        /// The live screen's event, as `dm` would know it.
        screen: Option<String>,
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
                screen: None,
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

        /// As `dm::deliver` does: an edit confirms the screen it changes,
        /// anything else is a new event, a live one becoming the screen.
        fn deliver(&mut self, msgs: Vec<Msg>) -> Vec<Msg> {
            for m in &msgs {
                let id = match (m.kind, &self.screen) {
                    (Kind::Edit, Some(screen)) => screen.clone(),
                    _ => self.record(m.in_session()),
                };
                if m.live {
                    let e = self.engine.clone();
                    tokio_block(e.screen_shown(&self.who.mxid, &id, true)).unwrap();
                    tokio_block(e.screen_echoed(ROOM, &id)).unwrap();
                    self.screen = Some(id);
                }
            }
            msgs
        }

        /// Like `deliver`, but the bot's own event hasn't come back through
        /// sync yet (what the room sees before the edit arrives).
        fn deliver_unechoed(&mut self, msgs: Vec<Msg>) -> Vec<Msg> {
            for m in &msgs {
                let id = match (m.kind, &self.screen) {
                    (Kind::Edit, Some(screen)) => screen.clone(),
                    _ => self.record(m.in_session()),
                };
                if m.live {
                    let e = self.engine.clone();
                    tokio_block(e.screen_shown(&self.who.mxid, &id, true)).unwrap();
                    self.screen = Some(id);
                }
            }
            msgs
        }

        fn echo(&self) {
            let e = self.engine.clone();
            tokio_block(e.screen_echoed(ROOM, self.screen.as_deref().unwrap())).unwrap();
        }

        /// A tap whose answer never reaches Matrix (a crash in between).
        fn tap_undelivered(&mut self, key: &str) -> Vec<Msg> {
            tokio_block(self.engine.handle(
                &self.who,
                ROOM,
                Input::Tap(key.into()),
                self.now,
                ttl(),
            ))
            .unwrap()
        }

        fn pending(&self) -> bool {
            self.count(&format!(
                "SELECT pending FROM sessions WHERE mxid = '{}'",
                self.who.mxid
            )) == 1
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
    fn a_screen_not_confirmed_as_shown_is_shown_again_before_a_tap_counts() {
        let mut chat = Chat::new();
        chat.menu();
        assert!(!chat.pending());
        // 📝 is handled, the flow is at the topics, but the chat never saw
        // the topic list (the bot crashed before the edit).
        let out = chat.tap_undelivered("📝");
        assert!(screen(&out).text.contains("What is this about?"));
        assert!(chat.pending());
        // The person still sees the menu and taps 💡 (Ideas there, but a
        // topic in the list they can't see): nothing is chosen, the topic
        // list is shown instead.
        let out = chat.tap("💡");
        assert_eq!(out.len(), 1);
        assert!(screen(&out).text.contains("What is this about?"));
        assert_eq!(
            chat.count(r#"SELECT count(*) FROM sessions WHERE flow LIKE '%"screen":"location"%'"#),
            0
        );
        // Delivered now: taps count again.
        assert!(!chat.pending());
        let out = chat.tap("💡");
        assert!(screen(&out).text.contains("📍 **Where?**"));
        let out = chat.tap("⏭️");
        assert!(screen(&out).text.contains("What's your idea"));
        // ⬅️ handled but not shown: the chat still asks for the idea, the
        // flow is back at the place. Their text was meant for the idea, so
        // it isn't used here: a note, and the screen the flow is at, so the
        // chat stops lying.
        chat.tap_undelivered("⬅️");
        assert!(chat.pending());
        let out = chat.say("A quiet hour after 22:00");
        assert_eq!(out[0].kind, Kind::Note);
        assert_eq!(out[0].text, screens::shown_again());
        assert!(screen(&out).text.contains("📍 **Where?**"));
        assert!(!chat.pending());
        assert_eq!(chat.count("SELECT count(*) FROM submissions"), 0);
    }

    #[test]
    fn a_tap_from_before_the_view_was_in_sight_only_shows_it_again() {
        let mut chat = Chat::new();
        chat.menu();
        // 📝 handled and sent, but the edit hasn't come back through sync:
        // whatever arrives before it was tapped on the menu.
        let msgs = chat.tap_undelivered("📝");
        chat.deliver_unechoed(msgs);
        let out = chat.tap("💡");
        assert_eq!(out.len(), 1);
        assert!(screen(&out).text.contains("What is this about?"));
        // The echo came (the re-show's, in the real room): taps act.
        chat.echo();
        let out = chat.tap("💡");
        assert!(screen(&out).text.contains("📍 **Where?**"));
    }

    #[test]
    fn a_button_retired_meanwhile_refreshes_the_screen_instead_of_acting() {
        let mut chat = Chat::new();
        chat.menu();
        chat.tap("📝");
        let screen_event = chat.screen.clone().unwrap();
        // The bot's buttons under the topic list, as the ledger knows them.
        chat.engine
            .db
            .with(|c| {
                crate::store::set_screen_buttons(
                    c,
                    ROOM,
                    &screen_event,
                    &[("🧹".into(), "$r1".into()), ("🍳".into(), "$r2".into())],
                )?;
                crate::catalog::set_active(c, crate::catalog::Kind::Topic, "cleaning", false)
            })
            .unwrap();
        // 🧹 is still under the message, but no longer a topic.
        let out = chat.tap("🧹");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, Kind::Edit);
        assert!(out[0].text.contains("What is this about?"));
        assert!(!out[0].buttons.contains(&"🧹".to_owned()));
        assert_eq!(
            chat.count(r#"SELECT count(*) FROM sessions WHERE flow LIKE '%"screen":"location"%'"#),
            0
        );
        // A reaction that never was a button does nothing at all.
        let out = chat.tap("🦄");
        assert!(out.is_empty(), "{:?}", out);
    }

    #[test]
    fn pending_screens_are_listed_and_shown_again_at_start() {
        let mut chat = Chat::new();
        chat.menu();
        chat.tap_undelivered("📝");
        let listed = chat.engine.db.with(|c| pending_screens(c)).unwrap();
        assert_eq!(listed, [(ME.to_owned(), ROOM.to_owned())]);
        // What the chat should show: the same step, nothing else.
        let out =
            tokio_block(chat.engine.current_screen(&chat.who, ROOM, chat.now, ttl())).unwrap();
        assert_eq!(out.len(), 1);
        assert!(screen(&out).text.contains("What is this about?"));
        chat.deliver(out);
        assert!(chat
            .engine
            .db
            .with(|c| pending_screens(c))
            .unwrap()
            .is_empty());
        // A screen shown without all its buttons stays pending.
        tokio_block(chat.engine.screen_shown(ME, "$e9", false)).unwrap();
        assert!(chat.pending());
        // An old unfinished draft isn't resumed at start: tidied, menu.
        chat.now += chrono::Duration::hours(2);
        let out =
            tokio_block(chat.engine.current_screen(&chat.who, ROOM, chat.now, ttl())).unwrap();
        assert_eq!(out[0].text, screens::expired());
        assert!(screen(&out).text.starts_with("📮 **Kummerkasten**"));
    }

    // ── Survey-only mode ───────────────────────────────────────────────────

    impl Chat {
        fn survey() -> Self {
            let mut chat = Chat::new();
            chat.engine = chat.engine.clone().survey_only(true);
            chat
        }

        /// The check-in's notice (or reminder) as the outbox would send it.
        fn told(&mut self, run: i64, which: CheckinText) -> Vec<Msg> {
            let msgs = tokio_block(self.engine.survey_message(
                &self.who,
                ROOM,
                run,
                which,
                self.now,
                ttl(),
            ))
            .unwrap();
            match msgs {
                Some(msgs) => self.deliver(msgs),
                None => Vec::new(),
            }
        }
    }

    fn keys(msg: &Msg) -> Vec<&str> {
        msg.buttons.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_survey_is_a_message_answered_step_by_step_and_then_done() {
        let mut chat = Chat::survey();
        let run = with_run(&mut chat, Default::default());
        // The notice is the check-in itself: no menu, no introduction.
        let intro = chat.told(run.id, CheckinText::Notice);
        assert_eq!(intro.len(), 1);
        assert!(
            intro[0].text.contains("Cleaning check-in"),
            "{}",
            intro[0].text
        );
        assert!(intro[0].text.contains("🔐 Just you and me here"));
        // …above ▶️ Start, the last line.
        assert!(
            intro[0].text.ends_with("tap ✅.\n\n▶️ **Start**"),
            "{}",
            intro[0].text
        );
        assert_eq!(keys(&intro[0]), ["▶️"]);
        assert!(!intro[0].text.contains("Cancel"));
        // Questions: answers, ⬅️ and (optional ones) ⏭️, never ❌.
        let q1 = chat.tap("▶️");
        assert!(screen(&q1).text.contains("1/3"));
        assert!(keys(screen(&q1)).contains(&"⬅️"));
        assert!(!keys(screen(&q1)).contains(&"❌"));
        assert!(screen(&q1).text.contains("⬅️ Back") && !screen(&q1).text.contains("Cancel"));
        let q2 = chat.tap("😐");
        assert!(screen(&q2).text.contains("2/3"));
        let ticked = chat.tap("🙋");
        assert_eq!(ticked[0].kind, Kind::Edit);
        let q3 = chat.tap("➡️");
        assert!(screen(&q3).text.contains("What would you change?"));
        // No offer to share it as an idea: straight on.
        let identity = chat.say("Fewer, clearer tasks per person.");
        assert!(
            screen(&identity).text.contains("submit your answers"),
            "{}",
            screen(&identity).text
        );
        let preview = chat.tap("🥷");
        assert!(screen(&preview).text.contains("📊 **Ready to submit**"));
        assert_eq!(keys(screen(&preview)), ["✅", "⬅️"]);
        // ✅: the short receipt and nothing else (no menu).
        let done = chat.tap("✅");
        assert_eq!(done.len(), 1, "{done:?}");
        assert_eq!(done[0].kind, Kind::Receipt);
        assert!(done[0]
            .text
            .contains("Thanks! Your answers were submitted."));
        assert_eq!(
            chat.count("SELECT count(*) FROM responses WHERE identity = 'anonymous' AND respondent_mxid IS NULL"),
            1
        );
        assert_eq!(chat.count("SELECT count(*) FROM ideas"), 0);
        // Afterwards: a message gets a short word, taps do nothing.
        let later = chat.say("hello?");
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].text, screens::nothing_open());
        assert!(chat.tap("📝").is_empty());
        assert!(chat.tap("✅").is_empty());
        // Never asked again: no notice, no reminder, 📊 says so.
        assert!(chat.told(run.id, CheckinText::Notice).is_empty());
        assert!(chat.told(run.id, CheckinText::Reminder).is_empty());
        let again = tokio_block(
            chat.engine
                .start_run(&chat.who, ROOM, run.id, chat.now, ttl()),
        )
        .unwrap();
        assert_eq!(again.len(), 1);
        assert!(again[0].text.contains("already answered"));
        assert_eq!(chat.count("SELECT count(*) FROM responses"), 1);
    }

    #[test]
    fn an_anonymous_only_survey_says_so_and_skips_the_choice() {
        let mut chat = Chat::survey();
        let run = with_run(
            &mut chat,
            crate::checkins::StartOptions {
                identities: vec![crate::submissions::Identity::Anonymous],
                ..Default::default()
            },
        );
        let intro = chat.told(run.id, CheckinText::Notice);
        assert!(intro[0]
            .text
            .contains("🥷 Anonymous: your answers aren't stored"));
        assert!(!intro[0].text.contains("you choose"));
        chat.tap("▶️");
        chat.tap("😐");
        chat.tap("➡️");
        // Straight from the last question to the preview: nothing to choose.
        let preview = chat.tap("⏭️");
        assert!(screen(&preview).text.contains("📊 **Ready to submit**"));
        assert!(screen(&preview).text.contains("🥷 Anonymous"));
    }

    #[test]
    fn back_skip_and_typed_words_behave_in_a_survey() {
        let mut chat = Chat::survey();
        let run = with_run(&mut chat, Default::default());
        chat.told(run.id, CheckinText::Notice);
        // ⬅️ on the intro (typed: there is no button) stays on the intro.
        let intro = chat.say("back");
        assert!(screen(&intro).text.contains("questions"), "{intro:?}");
        chat.tap("▶️");
        chat.tap("🙂");
        // Typed "back" goes to question 1, the answer still there.
        let q1 = chat.say("back");
        assert!(screen(&q1).text.contains("Your answer so far: 🙂 4"));
        // "cancel" / "stop" on a button step is no reset: a hint.
        let hint = chat.say("stop");
        assert_eq!(hint[0].kind, Kind::Note);
        assert!(hint[0].text.contains("Tap one of the buttons"));
        assert!(chat.count("SELECT count(*) FROM sessions WHERE flow LIKE '%\"value\":4%'") == 1);
        // On a written step it is the answer.
        chat.tap("🙂");
        chat.tap("⏭️");
        let identity = chat.say("stop");
        assert!(screen(&identity).text.contains("submit your answers"));
        let preview = chat.tap("🥷");
        assert!(screen(&preview).text.contains("“stop”"));
        assert!(screen(&preview).text.contains("⏭️ skipped"));
    }

    #[test]
    fn a_paused_survey_waits_until_it_closes() {
        let mut chat = Chat::survey();
        let run = with_run(&mut chat, Default::default());
        chat.told(run.id, CheckinText::Notice);
        chat.tap("▶️");
        chat.tap("😄");
        // Two days later (far past the draft timeout) it carries on.
        chat.now += chrono::Duration::days(2);
        assert!(chat.now < run.closes_at);
        assert!(tokio_block(chat.engine.expire(chat.now, ttl()))
            .unwrap()
            .is_empty());
        let q2 = chat.tap("➡️");
        assert!(screen(&q2).text.contains("3/3"), "{}", screen(&q2).text);
        // `!menu` / restart: the same step again.
        let again =
            tokio_block(chat.engine.current_screen(&chat.who, ROOM, chat.now, ttl())).unwrap();
        assert!(screen(&again).text.contains("3/3"));
        // A reminder: a note, the step they're on right below.
        let reminder = chat.told(run.id, CheckinText::Reminder);
        assert_eq!(reminder.len(), 2);
        assert!(reminder[0].text.contains("Quick reminder"));
        assert!(screen(&reminder).text.contains("3/3"));
        // Closed: the draft goes; a tap says so, without a menu.
        chat.now = run.closes_at + chrono::Duration::minutes(1);
        let closed = chat.tap("⏭️");
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert!(closed[0].text.contains("closed"));
        assert_eq!(chat.count("SELECT count(*) FROM responses"), 0);
        assert_eq!(
            chat.count(r#"SELECT count(*) FROM sessions WHERE flow LIKE '%"checkin":{%'"#),
            0
        );
    }

    #[test]
    fn the_scheduler_tidies_a_survey_that_closed_unanswered() {
        let mut chat = Chat::survey();
        let run = with_run(&mut chat, Default::default());
        chat.told(run.id, CheckinText::Notice);
        chat.tap("▶️");
        let later = run.closes_at + chrono::Duration::minutes(1);
        let swept = tokio_block(chat.engine.expire(later, ttl())).unwrap();
        assert_eq!(swept, [(ME.to_owned(), ROOM.to_owned())]);
        // What follows is the next survey, or (none) nothing at all.
        let next = tokio_block(chat.engine.fresh_menu(&chat.who, ROOM, later)).unwrap();
        assert!(next.is_empty(), "{next:?}");
        assert_eq!(
            chat.count("SELECT count(*) FROM dm_events WHERE doomed = 0 AND in_session = 1"),
            0
        );
    }

    #[test]
    fn one_survey_at_a_time_the_next_comes_after() {
        let mut chat = Chat::survey();
        let first = with_run(&mut chat, Default::default());
        let second = with_run(&mut chat, Default::default());
        chat.told(first.id, CheckinText::Notice);
        chat.tap("▶️");
        // The second one's notice waits while the first is half done…
        assert!(chat.told(second.id, CheckinText::Notice).is_empty());
        // …and 📊 on an old notice of it points back to the first.
        let busy = chat.deliver(
            tokio_block(
                chat.engine
                    .start_run(&chat.who, ROOM, second.id, chat.now, ttl()),
            )
            .unwrap(),
        );
        assert!(busy[0].text.contains("First the"));
        assert!(screen(&busy).text.contains("1/3"));
        chat.tap("😐");
        chat.tap("➡️");
        chat.tap("⏭️");
        chat.tap("🥷");
        // Done with the first: its receipt, then the second, from its intro.
        let done = chat.tap("✅");
        assert_eq!(done.len(), 2, "{done:?}");
        assert_eq!(done[0].kind, Kind::Receipt);
        assert!(screen(&done).text.contains("questions"));
        assert_eq!(keys(screen(&done)), ["▶️"]);
        let q1 = chat.tap("▶️");
        assert!(screen(&q1).text.contains("1/3"));
    }

    #[test]
    fn writing_while_idle_brings_up_a_waiting_survey() {
        let mut chat = Chat::survey();
        assert_eq!(chat.say("hi")[0].text, screens::nothing_open());
        with_run(&mut chat, Default::default());
        // Missed the notice (no chat back then): a message brings it up.
        let out = chat.say("hi");
        assert_eq!(out.len(), 1);
        assert_eq!(keys(screen(&out)), ["▶️"]);
        // `!help` there: the short survey help and the same step.
        let help =
            chat.deliver(tokio_block(chat.engine.help(&chat.who, ROOM, chat.now, ttl())).unwrap());
        assert!(help[0].text.contains("How this works"));
        assert!(!help[0].text.contains("!menu"));
        assert_eq!(keys(screen(&help)), ["▶️"]);
    }

    #[test]
    fn switching_to_survey_only_clears_what_the_full_bot_left() {
        let mut chat = Chat::new();
        chat.menu();
        chat.tap("📝");
        chat.tap("🧹");
        // Restarted in survey-only mode.
        chat.engine = chat.engine.clone().survey_only(true);
        let receipts = tokio_block(chat.engine.survey_cleanup(chat.now, ttl())).unwrap();
        assert!(receipts.is_empty());
        assert_eq!(chat.count("SELECT count(*) FROM sessions"), 0);
        assert_eq!(
            chat.count("SELECT count(*) FROM dm_events WHERE doomed = 0 AND in_session = 1"),
            0
        );
        // A tap on the old menu does nothing now.
        assert!(chat.tap("📝").is_empty());
        // A deletion code's receipt stays; its buttons go.
        let mut full = Chat::new();
        share(&mut full, "🥷", "♾️");
        let receipt = full.screen.clone().unwrap();
        full.engine
            .db
            .with(|c| crate::store::record_button(c, ROOM, "$btn", &receipt, false))
            .unwrap();
        full.engine = full.engine.clone().survey_only(true);
        let receipts = tokio_block(full.engine.survey_cleanup(full.now, ttl())).unwrap();
        assert_eq!(receipts, [(ROOM.to_owned(), receipt.clone())]);
        assert_eq!(
            full.count(&format!(
                "SELECT doomed FROM dm_events WHERE event_id = '{receipt}'"
            )),
            0
        );
        assert_eq!(
            full.count("SELECT doomed FROM dm_events WHERE event_id = '$btn'"),
            1
        );
    }

    #[test]
    fn a_survey_in_progress_survives_the_switch_and_a_restart() {
        let mut chat = Chat::survey();
        let run = with_run(&mut chat, Default::default());
        chat.told(run.id, CheckinText::Notice);
        chat.tap("▶️");
        chat.tap("🙂");
        assert!(tokio_block(chat.engine.survey_cleanup(chat.now, ttl()))
            .unwrap()
            .is_empty());
        chat.engine = Engine::new(
            chat.engine.db.clone(),
            Secrets::from_master(&[5; 32]),
            chat.engine.zone,
        )
        .survey_only(true);
        let q2 = chat.tap("➡️");
        assert!(screen(&q2).text.contains("3/3"));
    }

    #[test]
    fn results_show_numbers_and_sorted_words_only_once_closed() {
        let mut chat = Chat::survey();
        let run = with_run(&mut chat, Default::default());
        for (who, face, text) in [
            ("@alice:example.org", "😐", "Zebra stripes on the bins"),
            ("@bob:example.org", "😄", "a clearer plan"),
        ] {
            if who != ME {
                chat.engine
                    .db
                    .with(|c| {
                        c.execute(
                            "INSERT OR IGNORE INTO residents (mxid, created_at) VALUES (?1, 'x')",
                            [who],
                        )?;
                        Ok(())
                    })
                    .unwrap();
            }
            chat.who.mxid = who.into();
            chat.screen = None;
            chat.told(run.id, CheckinText::Notice);
            chat.tap("▶️");
            chat.tap(face);
            chat.tap("🙋");
            chat.tap("➡️");
            chat.say(text);
            chat.tap("🥷");
            chat.tap("✅");
        }
        let (k, zone, id, now) = (2, chat.engine.zone, run.id, chat.now);
        let open = chat
            .engine
            .db
            .with(|c| crate::run_results::render(c, id, k, zone, now))
            .unwrap();
        assert!(open.contains("2 answered"), "{open}");
        assert!(open.contains("once it has closed"));
        assert!(!open.contains("Zebra"));
        chat.engine
            .db
            .with(|c| crate::checkins::close(c, id, now).map(drop))
            .unwrap();
        let closed = chat
            .engine
            .db
            .with(|c| crate::run_results::render(c, id, k, zone, now))
            .unwrap();
        assert!(closed.contains("⌀ **4.0**"), "{closed}");
        assert!(
            closed.contains("🙋 Cleaning turns are skipped: 2×"),
            "{closed}"
        );
        let (a, z) = (
            closed.find("a clearer plan").unwrap(),
            closed.find("Zebra stripes").unwrap(),
        );
        assert!(a < z, "sorted");
        assert!(!closed.contains("alice") && !closed.contains("bob"));
        // Below the threshold: nothing but the count.
        let few = chat
            .engine
            .db
            .with(|c| crate::run_results::render(c, id, 3, zone, now))
            .unwrap();
        assert!(few.contains("Fewer than 3 answers"));
        assert!(!few.contains("⌀"));
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
