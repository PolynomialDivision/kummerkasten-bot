//! The clock: every minute (and whenever a handler wakes it) rounds open, finished plenums are completed, check-ins open,
//! remind and close, stale drafts are tidied, and the Matrix side catches
//! up with the database: marked events are redacted, cleared chats get
//! their notice and a fresh menu.
//!
//! All of it is driven by state in the database, so a restart in the middle
//! simply carries on. Batches (check-in notices, reminders, announcements,
//! plenum summaries and updates) are marked and queued in the outbox in one
//! transaction; the outbox sends them, retrying, each with its own Matrix
//! transaction ID, so a crash neither loses nor (as a rule) doubles one.
//! Redactions that fail are retried with a growing wait, never forgotten
//! while the message is still there; a chat a resident left is left only
//! once it is redacted.

use std::{sync::Mutex, time::Duration};

use chrono::Utc;
use mxbot_common::{
    matrix_sdk::{
        ruma::{OwnedEventId, OwnedRoomId, OwnedTransactionId, OwnedUserId},
        RoomState,
    },
    status,
    supervise::Alive,
};

use crate::{
    checkins::{self, Batch},
    dm::{self, Kk},
    engine::{CheckinText, Msg},
    lifecycle, outbox, plenums, reconcile, screens, store, summary,
};

const TICK: Duration = Duration::from_secs(60);
/// No tick for this long (a round hangs): the container turns unhealthy. A
/// round may send and redact a lot (a cleared chat), so well above a minute.
pub const STALLED_AFTER: Duration = Duration::from_secs(15 * 60);
/// Between two redactions, to stay friendly with the homeserver.
const REDACT_PAUSE: Duration = Duration::from_millis(250);

pub async fn run(kk: Kk, alive: Alive) {
    alive.expect_every(TICK);
    loop {
        alive.tick();
        // A panic in a tick ends the bot (panic = "abort", Cargo.toml) and
        // Docker restarts it; a tick that hangs stops the ticks above and the
        // container turns unhealthy. Either way the clock does not stop
        // unnoticed (notices, reminders, closing, redactions).
        tick(&kk).await;
        match take_failure() {
            None => alive.ok(),
            Some(reason) => alive.failed(reason),
        }
        report_outbox(&kk).await;
        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            _ = kk.wake.notified() => {}
        }
    }
}

/// The first failure of the running round, for the status file
/// (`mxbot_common::status`): only [`run`] runs the clocks, one round at a time.
static ROUND_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// A clock of this round failed: logged where it happened, noted here.
fn note(what: &str, error: &dyn std::fmt::Display) {
    note_reason(format!("{what}: {error:#}"));
}

fn note_reason(reason: String) {
    let mut failure = ROUND_FAILURE.lock().unwrap_or_else(|e| e.into_inner());
    failure.get_or_insert(reason);
}

fn take_failure() -> Option<String> {
    ROUND_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// The outbox's waiting messages, the oldest one's age and how many are stuck, for the
/// status file. A failed count leaves the last one.
async fn report_outbox(kk: &Kk) {
    match kk.engine.db.run(|c| outbox::backlog(c)).await {
        Ok((pending, stuck, oldest)) => {
            let age = oldest.map(|at| (Utc::now() - at).to_std().unwrap_or_default());
            status::queue("outbox").set(pending.max(0) as u64, age);
            status::gauge("outbox_stuck").set(stuck as f64);
        }
        Err(error) => tracing::debug!(%error, "Could not count the outbox for the status file"),
    }
}

async fn tick(kk: &Kk) {
    plenum_clock(kk).await;
    // Survey-only mode: no plenum summaries (they invite to 📈 and 💡).
    if !kk.engine.is_survey() {
        summary_clock(kk).await;
    }
    checkin_clock(kk).await;
    outbox_clock(kk).await;
    expire_drafts(kk).await;
    redact_marked(kk).await;
    leave_left_chats(kk).await;
    post_notices(kk).await;
    dm::post_missing_entries(kk).await;
}

/// The round invariant, completions that are due. (The plenums themselves
/// come from the calendar: `calendar::run`.)
async fn plenum_clock(kk: &Kk) {
    let _op = kk.ops.lock().await;
    let zone = kk.engine.zone;
    let settings = kk.settings.get();
    let complete_at = settings.complete_at();
    // A date passing doesn't mean the plenum took place: only the explicit
    // policy completes it unasked.
    let auto_complete = settings.auto_complete;
    let result = kk
        .engine
        .db
        .run(move |conn| {
            let now = Utc::now();
            plenums::ensure_round(conn, now)?;
            let mut completed = 0;
            if auto_complete {
                for id in lifecycle::due(conn, zone, now, complete_at)? {
                    lifecycle::complete(conn, id, "system", now)?;
                    completed += 1;
                }
            }
            store::prune_processed(conn, now - chrono::Duration::days(30))?;
            Ok(completed)
        })
        .await;
    match result {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            completed = n,
            "Plenum completed by policy; chats will be cleared"
        ),
        Err(error) => {
            tracing::error!(%error, "Plenum clock failed");
            note("Plenum clock failed", &error);
        }
    }
}

/// 📮 The day-before summary of the next plenum (once, following its
/// current date), and the short update for an announced plenum that moved
/// or was cancelled: queued for the central room(s) in the transaction that
/// marks them, and sent by [`outbox_clock`].
async fn summary_clock(kk: &Kk) {
    let before = kk.settings.get().summary_before();
    let queued = kk
        .engine
        .db
        .run(move |conn| {
            let now = Utc::now();
            if checkins::central_rooms(conn)?.is_empty() {
                // Nowhere to post: nothing is marked as announced either.
                return Ok(0);
            }
            let tx = conn.unchecked_transaction()?;
            let mut queued = 0;
            if let Some(id) = summary::take_summary(&tx, now, before)? {
                outbox::queue(&tx, outbox::Kind::Summary, id, now)?;
                queued += 1;
            }
            for (id, what) in summary::take_updates(&tx)? {
                let kind = match what {
                    summary::Update::Moved => outbox::Kind::Moved,
                    summary::Update::Cancelled => outbox::Kind::Cancelled,
                };
                outbox::queue(&tx, kind, id, now)?;
                queued += 1;
            }
            tx.commit()?;
            Ok(queued)
        })
        .await;
    if let Err(error) = queued {
        tracing::error!(%error, "Summary clock failed");
        note("Summary clock failed", &error);
    }
}

/// Close what's over; queue what's due (each batch once, in the
/// transaction that marks it).
async fn checkin_clock(kk: &Kk) {
    let queued = kk
        .engine
        .db
        .run(|conn| {
            let now = Utc::now();
            let tx = conn.unchecked_transaction()?;
            checkins::close_due(&tx, now)?;
            for run in checkins::take_announcements(&tx, now)? {
                outbox::queue(&tx, outbox::Kind::Announcement, run, now)?;
            }
            for run in checkins::take_notices(&tx, now)? {
                outbox::queue(&tx, outbox::Kind::Notice, run, now)?;
            }
            for run in checkins::take_reminders(&tx, now)? {
                outbox::queue(&tx, outbox::Kind::Reminder, run, now)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await;
    if let Err(error) = queued {
        tracing::error!(%error, "Check-in clock failed");
        note("Check-in clock failed", &error);
    }
}

/// 📤 The outbox: decide who gets each new batch (against who is there
/// now), then send what is due, each with its own transaction ID.
async fn outbox_clock(kk: &Kk) {
    let batches = match kk.engine.db.run(|c| outbox::batches(c)).await {
        Ok(batches) => batches,
        Err(error) => {
            tracing::error!(%error, "Reading the outbox failed");
            note("Reading the outbox failed", &error);
            return;
        }
    };
    for (row, kind, ref_id) in batches {
        let targets = match recipients(kk, kind, ref_id).await {
            Ok(targets) => targets,
            Err(error) => {
                tracing::error!(%error, kind = kind.as_str(), "Could not decide who gets a message");
                note("Could not decide who gets a message", &error);
                continue;
            }
        };
        let now = Utc::now();
        if let Err(error) = kk
            .engine
            .db
            .run(move |c| outbox::expand(c, row, &targets, now))
            .await
        {
            tracing::error!(%error, "Could not queue the messages of a batch");
            note("Could not queue the messages of a batch", &error);
        }
    }
    let due = match kk.engine.db.run(|c| outbox::due(c, Utc::now(), 200)).await {
        Ok(due) => due,
        Err(error) => {
            tracing::error!(%error, "Reading the outbox failed");
            note("Reading the outbox failed", &error);
            return;
        }
    };
    let mut told = std::collections::BTreeMap::<(i64, &str), usize>::new();
    for item in due {
        let outcome = send_item(kk, &item).await;
        if outcome == dm::Told::Failed {
            note_reason(format!(
                "an outbox message ({}) not sent",
                item.kind.as_str()
            ));
        }
        if outcome == dm::Told::Sent && item.kind.to_chats() {
            *told.entry((item.ref_id, item.kind.as_str())).or_default() += 1;
        }
        let (i, now) = (item.clone(), Utc::now());
        let saved = kk
            .engine
            .db
            .run(move |c| match outcome {
                dm::Told::Sent => outbox::sent(c, &i).map(|()| false),
                dm::Told::NotNeeded => outbox::skipped(c, &i).map(|()| false),
                dm::Told::Failed => outbox::failed(c, &i, now),
            })
            .await;
        match saved {
            Ok(true) => tracing::warn!(
                kind = item.kind.as_str(),
                "A message keeps failing to send; trying again every few hours"
            ),
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "Could not update the outbox");
                note("Could not update the outbox", &error);
            }
        }
    }
    // Aggregate counts only (how many were told), never who.
    for ((run, kind), sent) in told {
        let batch = if kind == "reminder" {
            Batch::Reminder
        } else {
            Batch::Notice
        };
        let _ = kk
            .engine
            .db
            .run(move |c| checkins::count_batch(c, run, batch, 0, sent))
            .await;
        tracing::info!(run, kind, sent, "Check-in messages sent");
    }
}

/// Who gets a batch: residents' chats (Matrix IDs) or house rooms.
async fn recipients(kk: &Kk, kind: outbox::Kind, ref_id: i64) -> anyhow::Result<Vec<String>> {
    use outbox::Kind as K;
    Ok(match kind {
        K::Notice | K::Reminder => {
            // Closed meanwhile (the bot was down past its close): nobody,
            // and no chats created for it.
            let open = kk
                .engine
                .db
                .run(move |c| Ok(checkins::run(c, ref_id)?.is_open(Utc::now())))
                .await?;
            if !open {
                return Ok(Vec::new());
            }
            if kind == K::Notice {
                // Before the notice, every eligible resident without a chat
                // gets one (they get the notice when they accept it).
                let target = kk
                    .engine
                    .db
                    .run(move |c| Ok(checkins::run(c, ref_id)?.target))
                    .await?;
                let hs = dm::Matrix { kk, only: None };
                let eligible = match reconcile::reconcile(&kk.engine.db, &hs, &target).await {
                    Ok(out) => out.eligible,
                    Err(error) => {
                        tracing::warn!(%error, "Could not reconcile chats; telling who has one");
                        0
                    }
                };
                kk.engine
                    .db
                    .run(move |c| checkins::count_batch(c, ref_id, Batch::Notice, eligible, 0))
                    .await?;
            }
            // Access, target and "not answered yet" are checked per person
            // right before sending.
            kk.engine
                .db
                .run(|c| store::open_chats(c))
                .await?
                .into_iter()
                .map(|(mxid, _)| mxid)
                .collect()
        }
        K::Announcement | K::Summary => kk.engine.db.run(|c| checkins::central_rooms(c)).await?,
        K::Moved | K::Cancelled => {
            // Where its summary went, else the central rooms.
            kk.engine
                .db
                .run(move |c| {
                    let mut rooms: Vec<String> = summary::summaries(c, ref_id)?
                        .into_iter()
                        .map(|(room, _, _)| room)
                        .collect();
                    rooms.dedup();
                    if rooms.is_empty() {
                        rooms = checkins::central_rooms(c)?;
                    }
                    Ok(rooms)
                })
                .await?
        }
    })
}

/// Send one message of the outbox.
async fn send_item(kk: &Kk, item: &outbox::Item) -> dm::Told {
    use outbox::Kind as K;
    let txn: OwnedTransactionId = item.txn.as_str().into();
    if item.kind.to_chats() {
        let mxid = item.target.clone();
        let resident = kk
            .engine
            .db
            .run(move |c| store::resident_by_mxid(c, &mxid))
            .await;
        let Ok(Some(store::Resident {
            dm_room_id: Some(room_id),
            dm_joined: true,
            ..
        })) = resident
        else {
            return dm::Told::NotNeeded;
        };
        let (Some(room), Ok(user)) = (
            room_of(kk, &room_id),
            OwnedUserId::try_from(item.target.as_str()),
        ) else {
            return dm::Told::NotNeeded;
        };
        // Not a member any more: not needed. The server couldn't say: later.
        let who = match kk.who_checked(&user).await {
            Ok(Some(who)) => who,
            Ok(None) => return dm::Told::NotNeeded,
            Err(_) => return dm::Told::Failed,
        };
        let which = if item.kind == K::Reminder {
            CheckinText::Reminder
        } else {
            CheckinText::Notice
        };
        return dm::tell_txn(kk, &room, &who, item.ref_id, which, &txn).await;
    }
    // Into a house room: not joined (yet) counts as failed, it may come.
    let Some(room) = room_of(kk, &item.target) else {
        return dm::Told::Failed;
    };
    let (zone, k, id, now) = (kk.engine.zone, kk.engine.k(), item.ref_id, Utc::now());
    let kind = item.kind;
    // The text, and the plenum date it tells (None: not needed any more).
    let text = kk
        .engine
        .db
        .run(move |conn| {
            Ok(match kind {
                K::Announcement => None,
                K::Summary => match plenums::get(conn, id)? {
                    // Only before the plenum begins.
                    Some(p) if p.starts_at > now => {
                        let cat = crate::catalog::Catalog::load(conn)?;
                        Some((summary::render(conn, &cat, id, k, zone, now)?, p.starts_at))
                    }
                    _ => None,
                },
                _ => match plenums::get(conn, id)? {
                    Some(p) => {
                        let what = if kind == K::Cancelled {
                            summary::Update::Cancelled
                        } else {
                            summary::Update::Moved
                        };
                        Some((
                            summary::update_text(conn, id, what, zone, now)?,
                            p.starts_at,
                        ))
                    }
                    None => None,
                },
            })
        })
        .await;
    if kind == K::Announcement {
        // Only while it is open: days late (a room unreachable meanwhile)
        // it would invite to a closed check-in.
        let open = kk
            .engine
            .db
            .run(move |c| Ok(checkins::run(c, id)?.is_open(now)))
            .await;
        match open {
            Ok(true) => {}
            Ok(false) => return dm::Told::NotNeeded,
            Err(error) => {
                tracing::error!(%error, "Could not read a check-in to announce");
                note("Could not read a check-in to announce", &error);
                return dm::Told::Failed;
            }
        }
        let text = match kk
            .engine
            .checkin_text(id, CheckinText::Announcement, now)
            .await
        {
            Ok((_, text)) => text,
            Err(error) => {
                tracing::error!(%error, "Could not write an announcement");
                note("Could not write an announcement", &error);
                return dm::Told::Failed;
            }
        };
        return match dm::post_with_keys_txn(kk, &room, &text, &[dm::ENTRY], &txn).await {
            Ok(_) => dm::Told::Sent,
            Err(error) => {
                tracing::warn!(%error, "Could not announce a check-in");
                dm::Told::Failed
            }
        };
    }
    let (text, starts_at) = match text {
        Ok(Some(text)) => text,
        Ok(None) => return dm::Told::NotNeeded,
        Err(error) => {
            tracing::error!(%error, "Could not write a plenum message");
            note("Could not write a plenum message", &error);
            return dm::Told::Failed;
        }
    };
    let keys: &[&str] = if kind == K::Summary {
        &[dm::ENTRY, dm::RESULTS, dm::IDEAS]
    } else {
        &[dm::ENTRY]
    };
    let event_id = match dm::post_with_keys_txn(kk, &room, &text, keys, &txn).await {
        Ok(event_id) => event_id,
        Err(error) => {
            tracing::warn!(%error, "Could not post a plenum message");
            return dm::Told::Failed;
        }
    };
    let (r, e, t) = (room.room_id().to_string(), event_id.to_string(), text);
    let record_kind = if kind == K::Summary {
        summary::Kind::Summary
    } else {
        summary::Kind::Update
    };
    let amend = kk
        .engine
        .db
        .run(move |c| {
            summary::record(c, id, &r, &e, record_kind, &t, starts_at)?;
            if kind == K::Summary {
                return Ok(Vec::new());
            }
            // The update also amends the summary posted in this room.
            let what = if kind == K::Cancelled {
                summary::Update::Cancelled
            } else {
                summary::Update::Moved
            };
            let Some(plenum) = plenums::get(c, id)? else {
                return Ok(Vec::new());
            };
            Ok(summary::summaries(c, id)?
                .into_iter()
                .filter(|(room, _, _)| *room == r)
                .map(|(_, event, body)| (event, summary::amended(&body, what, &plenum, zone, now)))
                .collect())
        })
        .await;
    for (event, amended) in amend.unwrap_or_default() {
        if let Err(error) = dm::edit_notice(&room, &event, &amended).await {
            tracing::debug!(%error, "Could not update a posted summary");
        }
    }
    tracing::info!(plenum = id, kind = kind.as_str(), "Plenum message posted");
    dm::Told::Sent
}

async fn expire_drafts(kk: &Kk) {
    let _op = kk.ops.lock().await;
    let ttl = kk.settings.get().session_ttl();
    let stale = match kk.engine.expire(Utc::now(), ttl).await {
        Ok(stale) => stale,
        Err(error) => {
            tracing::error!(%error, "Expiring drafts failed");
            note("Expiring drafts failed", &error);
            return;
        }
    };
    for (mxid, room_id) in stale {
        let Some(room) = room_of(kk, &room_id) else {
            continue;
        };
        let Ok(user) = OwnedUserId::try_from(mxid.as_str()) else {
            continue;
        };
        let Some(who) = kk.who(&user).await else {
            continue;
        };
        // Survey-only mode: a check-in that closed unanswered just goes
        // (tidied away); the next one waiting comes up, if any.
        let mut msgs = if kk.engine.is_survey() {
            Vec::new()
        } else {
            vec![Msg::note(screens::expired())]
        };
        match kk.engine.fresh_menu(&who, &room_id, Utc::now()).await {
            Ok(menu) => msgs.extend(menu),
            Err(error) => {
                tracing::error!(%error, "No fresh menu after expiry");
                note("No fresh menu after expiry", &error);
            }
        }
        dm::deliver(kk, &room, &mxid, msgs).await;
    }
}

fn room_of(kk: &Kk, room_id: &str) -> Option<mxbot_common::matrix_sdk::Room> {
    OwnedRoomId::try_from(room_id)
        .ok()
        .and_then(|id| kk.client.get_room(&id))
        .filter(|r| r.state() == RoomState::Joined)
}

/// Redact what the database marked, then forget it. Something that keeps
/// failing is given up after a few tries (and logged, without content).
async fn redact_marked(kk: &Kk) {
    loop {
        let batch = match kk.engine.db.run(|c| store::doomed(c, 50)).await {
            Ok(batch) if !batch.is_empty() => batch,
            Ok(_) => return,
            Err(error) => {
                tracing::error!(%error, "Reading events to redact failed");
                note("Reading events to redact failed", &error);
                return;
            }
        };
        let mut progress = false;
        for (room_id, event_id, attempts) in batch {
            let outcome = match (
                room_of(kk, &room_id),
                OwnedEventId::try_from(event_id.as_str()),
            ) {
                (Some(room), Ok(id)) => room
                    .redact(&id, Some("🧹 Kummerkasten: cleared"), None)
                    .await
                    .map(drop),
                // Not in that room any more, or not an event ID: nothing to do.
                _ => Ok(()),
            };
            let (r, e) = (room_id.clone(), event_id.clone());
            let forget = match outcome {
                Ok(()) => true,
                // Not there (any more): nothing is left to redact.
                Err(error) if is_gone(&error) => {
                    tracing::debug!(%error, "Nothing to redact: the event is gone");
                    true
                }
                // Anything else may pass (a hiccup, power levels fixed later):
                // tried again later, never forgotten while it stays on the server.
                Err(error) => {
                    if attempts + 1 >= store::STUCK_AFTER {
                        tracing::warn!(%error, attempts = attempts + 1, "A redaction keeps failing; trying again later");
                        note("A redaction keeps failing", &error);
                    } else {
                        tracing::debug!(%error, "Redaction failed; will retry");
                    }
                    false
                }
            };
            let now = Utc::now();
            let saved = kk
                .engine
                .db
                .run(move |c| {
                    if forget {
                        store::forget_event(c, &r, &e)
                    } else {
                        store::retry_event(c, &r, &e, now)
                    }
                })
                .await;
            if let Err(error) = saved {
                tracing::error!(%error, "Could not update a redaction");
                note("Could not update a redaction", &error);
                return;
            }
            progress |= forget;
            tokio::time::sleep(REDACT_PAUSE).await;
        }
        if !progress {
            // Only retries left in this batch: try again next tick.
            return;
        }
    }
}

/// The homeserver says the event doesn't exist (redacting it is pointless).
fn is_gone(error: &mxbot_common::matrix_sdk::HttpError) -> bool {
    use mxbot_common::matrix_sdk::ruma::api::error::ErrorKind;
    matches!(error.client_api_error_kind(), Some(ErrorKind::NotFound))
}

/// Chats a resident left: once everything tracked there is redacted, the
/// bot leaves too.
async fn leave_left_chats(kk: &Kk) {
    let rooms = match kk.engine.db.run(|c| store::rooms_to_leave(c)).await {
        Ok(rooms) => rooms,
        Err(error) => {
            tracing::error!(%error, "Reading chats to leave failed");
            note("Reading chats to leave failed", &error);
            return;
        }
    };
    for room_id in rooms {
        if let Some(room) = room_of(kk, &room_id) {
            if let Err(error) = room.leave().await {
                tracing::warn!(%error, "Could not leave a closed chat; trying again later");
                note("Could not leave a closed chat", &error);
                continue;
            }
        }
        let r = room_id.clone();
        if let Err(error) = kk.engine.db.run(move |c| store::left_room(c, &r)).await {
            tracing::error!(%error, "Could not note a left chat");
            note("Could not note a left chat", &error);
        }
    }
}

/// Chats that were cleared get the notice and a fresh menu, once.
async fn post_notices(kk: &Kk) {
    let pending = match kk.engine.db.run(|c| store::pending_notices(c)).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(%error, "Reading pending notices failed");
            note("Reading pending notices failed", &error);
            return;
        }
    };
    for resident in pending {
        let _op = kk.ops.lock().await;
        let m = resident.mxid.clone();
        // Cleared first: a crash below means no notice, never two.
        if let Err(error) = kk.engine.db.run(move |c| store::clear_notice(c, &m)).await {
            tracing::error!(%error, "Could not clear a notice");
            note("Could not clear a notice", &error);
            continue;
        }
        let Some(room) = resident.dm_room_id.as_deref().and_then(|r| room_of(kk, r)) else {
            continue;
        };
        let Ok(user) = OwnedUserId::try_from(resident.mxid.as_str()) else {
            continue;
        };
        let Some(who) = kk.who(&user).await else {
            continue;
        };
        if !kk.chat_is_private(&room, &user).await {
            continue;
        }
        // Survey-only mode: cleared quietly; a check-in still waiting for
        // them comes up again.
        let mut msgs = if kk.engine.is_survey() {
            Vec::new()
        } else {
            vec![Msg::receipt(screens::round_cleared())]
        };
        match kk
            .engine
            .fresh_menu(&who, room.room_id().as_str(), Utc::now())
            .await
        {
            Ok(menu) => msgs.extend(menu),
            Err(error) => {
                tracing::error!(%error, "No fresh menu after clearing");
                note("No fresh menu after clearing", &error);
            }
        }
        dm::deliver(kk, &room, &resident.mxid, msgs).await;
    }
}
