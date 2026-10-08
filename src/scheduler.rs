//! The clock: every minute (and whenever a handler wakes it) plenums come
//! into view, rounds open, finished plenums are completed, check-ins open,
//! remind and close, stale drafts are tidied — and the Matrix side catches
//! up with the database: marked events are redacted, cleared chats get
//! their notice and a fresh menu.
//!
//! All of it is driven by state in the database, so a restart in the middle
//! simply carries on. Batches (check-in notices, reminders) are marked as
//! sent before they go out: a crash means one missed message, never two.

use std::time::Duration;

use chrono::Utc;
use mxbot_common::matrix_sdk::{
    ruma::{OwnedEventId, OwnedRoomId, OwnedUserId},
    RoomState,
};

use crate::{
    checkins,
    dm::{self, Kk},
    engine::{CheckinText, Msg},
    lifecycle, plenums, screens, store, summary,
};

const TICK: Duration = Duration::from_secs(60);
/// Between two redactions, to stay friendly with the homeserver.
const REDACT_PAUSE: Duration = Duration::from_millis(250);
const MAX_REDACT_ATTEMPTS: i64 = 5;

pub async fn run(kk: Kk) {
    loop {
        tick(&kk).await;
        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            _ = kk.wake.notified() => {}
        }
    }
}

async fn tick(kk: &Kk) {
    plenum_clock(kk).await;
    summary_clock(kk).await;
    checkin_clock(kk).await;
    expire_drafts(kk).await;
    redact_marked(kk).await;
    post_notices(kk).await;
}

/// Plenums into view, the round invariant, completions that are due.
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
            plenums::materialize(conn, zone, now)?;
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
        Err(error) => tracing::error!(%error, "Plenum clock failed"),
    }
}

/// 📮 The day-before summary of the next plenum (once, following its
/// current date), and the short update for an announced plenum that moved
/// or was cancelled — into the central room(s). Marked before sending: a
/// crash means a missed post, never a second one.
async fn summary_clock(kk: &Kk) {
    let before = kk.settings.get().summary_before();
    let (zone, k) = (kk.engine.zone, kk.engine.k());
    let due = kk
        .engine
        .db
        .run(move |conn| {
            let now = Utc::now();
            if checkins::central_rooms(conn)?.is_empty() {
                // Nowhere to post: nothing is marked as announced either.
                return Ok((None, Vec::new()));
            }
            let cat = crate::catalog::Catalog::load(conn)?;
            let summary = match summary::take_summary(conn, now, before)? {
                Some(id) => {
                    let p = plenums::get(conn, id)?.map(|p| p.starts_at);
                    Some((id, summary::render(conn, &cat, id, k, zone, now)?, p))
                }
                None => None,
            };
            let mut updates = Vec::new();
            for (id, what) in summary::take_updates(conn)? {
                let Some(plenum) = plenums::get(conn, id)? else {
                    continue;
                };
                let text = summary::update_text(conn, id, what, zone, now)?;
                let edits: Vec<(String, String, String)> = summary::summaries(conn, id)?
                    .into_iter()
                    .map(|(room, event, body)| {
                        let amended = summary::amended(&body, what, &plenum, zone, now);
                        (room, event, amended)
                    })
                    .collect();
                updates.push((id, plenum.starts_at, text, edits));
            }
            Ok((summary, updates))
        })
        .await;
    let (summary, updates) = match due {
        Ok(due) => due,
        Err(error) => {
            tracing::error!(%error, "Summary clock failed");
            return;
        }
    };
    let rooms = kk
        .engine
        .db
        .run(|c| checkins::central_rooms(c))
        .await
        .unwrap_or_default();
    if let Some((id, text, Some(starts_at))) = summary {
        for room in rooms.iter().filter_map(|r| room_of_house(kk, r)) {
            match dm::post_with_keys(kk, &room, &text, &[dm::ENTRY, dm::RESULTS, dm::IDEAS]).await {
                Ok(event_id) => {
                    let (r, e, t) = (
                        room.room_id().to_string(),
                        event_id.to_string(),
                        text.clone(),
                    );
                    let _ = kk
                        .engine
                        .db
                        .run(move |c| {
                            summary::record(c, id, &r, &e, summary::Kind::Summary, &t, starts_at)
                        })
                        .await;
                }
                Err(error) => tracing::warn!(%error, "Could not post a plenum summary"),
            }
        }
        tracing::info!(plenum = id, "Plenum summary posted");
    }
    for (id, starts_at, text, edits) in updates {
        // Where its summary went (else: the central rooms).
        let mut targets: Vec<String> = edits.iter().map(|(r, _, _)| r.clone()).collect();
        targets.dedup();
        if targets.is_empty() {
            targets = rooms.clone();
        }
        for room in targets.iter().filter_map(|r| room_of_house(kk, r)) {
            match dm::post_with_keys(kk, &room, &text, &[dm::ENTRY]).await {
                Ok(event_id) => {
                    let (r, e, t) = (
                        room.room_id().to_string(),
                        event_id.to_string(),
                        text.clone(),
                    );
                    let _ = kk
                        .engine
                        .db
                        .run(move |c| {
                            summary::record(c, id, &r, &e, summary::Kind::Update, &t, starts_at)
                        })
                        .await;
                }
                Err(error) => tracing::warn!(%error, "Could not post a plenum update"),
            }
        }
        for (room_id, event_id, amended) in edits {
            let Some(room) = room_of_house(kk, &room_id) else {
                continue;
            };
            if let Err(error) = dm::edit_notice(&room, &event_id, &amended).await {
                tracing::debug!(%error, "Could not update a posted summary");
            }
        }
        tracing::info!(plenum = id, "Plenum update posted");
    }
}

fn room_of_house(kk: &Kk, room_id: &str) -> Option<mxbot_common::matrix_sdk::Room> {
    room_of(kk, room_id)
}

/// Close what's over; send what's due (each batch once).
async fn checkin_clock(kk: &Kk) {
    let due = kk
        .engine
        .db
        .run(|conn| {
            let now = Utc::now();
            checkins::close_due(conn, now)?;
            Ok((
                checkins::take_notices(conn, now)?,
                checkins::take_reminders(conn, now)?,
                checkins::take_announcements(conn, now)?,
            ))
        })
        .await;
    let (notices, reminders, announcements) = match due {
        Ok(due) => due,
        Err(error) => {
            tracing::error!(%error, "Check-in clock failed");
            return;
        }
    };
    for run in announcements {
        dm::announce_checkin(kk, run).await;
    }
    for run in notices {
        dm::send_checkin(kk, run, CheckinText::Notice).await;
    }
    for run in reminders {
        dm::send_checkin(kk, run, CheckinText::Reminder).await;
    }
}

async fn expire_drafts(kk: &Kk) {
    let _op = kk.ops.lock().await;
    let ttl = kk.settings.get().session_ttl();
    let stale = match kk.engine.expire(Utc::now(), ttl).await {
        Ok(stale) => stale,
        Err(error) => {
            tracing::error!(%error, "Expiring drafts failed");
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
        let mut msgs = vec![Msg::note(screens::expired())];
        match kk.engine.fresh_menu(&who, &room_id, Utc::now()).await {
            Ok(menu) => msgs.extend(menu),
            Err(error) => tracing::error!(%error, "No fresh menu after expiry"),
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
/// failing is given up after a few tries (and logged — without content).
async fn redact_marked(kk: &Kk) {
    loop {
        let batch = match kk.engine.db.run(|c| store::doomed(c, 50)).await {
            Ok(batch) if !batch.is_empty() => batch,
            Ok(_) => return,
            Err(error) => {
                tracing::error!(%error, "Reading events to redact failed");
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
                    .map(drop)
                    .map_err(|e| e.to_string()),
                // Not in that room any more, or not an event ID: nothing to do.
                _ => Ok(()),
            };
            let (r, e) = (room_id.clone(), event_id.clone());
            let forget = match outcome {
                Ok(()) => true,
                Err(error) if attempts + 1 >= MAX_REDACT_ATTEMPTS => {
                    tracing::warn!(%error, "Giving up redacting an event");
                    true
                }
                Err(error) => {
                    tracing::debug!(%error, "Redaction failed; will retry");
                    false
                }
            };
            let saved = kk
                .engine
                .db
                .run(move |c| {
                    if forget {
                        store::forget_event(c, &r, &e)
                    } else {
                        store::retry_event(c, &r, &e)
                    }
                })
                .await;
            if let Err(error) = saved {
                tracing::error!(%error, "Could not update a redaction");
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

/// Chats that were cleared get the notice and a fresh menu — once.
async fn post_notices(kk: &Kk) {
    let pending = match kk.engine.db.run(|c| store::pending_notices(c)).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(%error, "Reading pending notices failed");
            return;
        }
    };
    for resident in pending {
        let _op = kk.ops.lock().await;
        let m = resident.mxid.clone();
        // Cleared first: a crash below means no notice, never two.
        if let Err(error) = kk.engine.db.run(move |c| store::clear_notice(c, &m)).await {
            tracing::error!(%error, "Could not clear a notice");
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
        let mut msgs = vec![Msg::receipt(screens::round_cleared())];
        match kk
            .engine
            .fresh_menu(&who, room.room_id().as_str(), Utc::now())
            .await
        {
            Ok(menu) => msgs.extend(menu),
            Err(error) => tracing::error!(%error, "No fresh menu after clearing"),
        }
        dm::deliver(kk, &room, &resident.mxid, msgs).await;
    }
}
