//! Residents' private chats on the Matrix side: opening them, sending what
//! the engine says (and tracking every event for redaction), and turning
//! messages and taps into engine input: each only after checking, fresh
//! from the server, that the chat is still just the bot and that resident,
//! encrypted, and that they are still in an authorized room.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use mxbot_common::{
    matrix_sdk::{
        self,
        deserialized_responses::EncryptionInfo,
        room::{IncludeRelations, RelationsOptions},
        ruma::{
            api::error::ErrorKind,
            events::{
                reaction::{OriginalSyncReactionEvent, ReactionEventContent, SyncReactionEvent},
                relation::{Annotation, RelationType},
                room::{
                    member::{MembershipState, OriginalSyncRoomMemberEvent},
                    message::{
                        MessageType, OriginalSyncRoomMessageEvent, Relation, ReplacementMetadata,
                    },
                },
                AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            },
            OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId, TransactionId, UInt, UserId,
        },
        Client, Room, RoomState,
    },
    rooms::DynamicRooms,
    send::{send_with_retry, send_with_retry_txn},
    settings::Settings,
};
use tokio::sync::{Mutex, Notify};

use crate::{
    access,
    checkins::{self, Batch},
    config::Tunables,
    engine::{CheckinText, Engine, Input, Kind, Msg, Who},
    flow::MenuItem,
    markup,
    reconcile::{self, Failure, Homeserver},
    screens, store,
};

/// Room-version policy for new chats: the versions this implementation
/// explicitly supports, in our order of preference (a deliberate list, not
/// a sort: room version IDs are opaque). A new chat gets the first one the
/// homeserver lists as *stable*; when none matches, the server's default.
/// Supporting a newer version means testing it and putting it in front.
pub const PREFERRED_SUPPORTED_ROOM_VERSIONS: [RoomVersionId; 3] =
    mxbot_common::private::PREFERRED_ROOM_VERSIONS;

/// The 📮 on entry messages (and 📈 / 💡 under a plenum summary).
pub const ENTRY: &str = "📮";
pub const RESULTS: &str = "📈";
pub const IDEAS: &str = "💡";

/// Everything the handlers share. Cheap to clone.
#[derive(Clone)]
pub struct Kk {
    pub client: Client,
    pub bot: OwnedUserId,
    pub engine: Engine,
    pub settings: Settings<Tunables>,
    pub rooms: DynamicRooms,
    /// Serializes everything that changes conversations.
    pub ops: Arc<Mutex<()>>,
    /// Wakes the job runner (redactions, notices).
    pub wake: Arc<Notify>,
    /// Check-in starts admins planned and haven't confirmed yet.
    pub pending_starts: Arc<std::sync::Mutex<PendingStarts>>,
    /// Test mode: only these (the admins) have access at all, and check-ins
    /// go only to the admin who starts them. `None`: live.
    pub test_users: Option<Arc<Vec<String>>>,
    /// The rooms from the config (`[[kummerkasten.rooms]]`); `None`: admins
    /// manage them with `!kk rooms …`.
    pub managed_rooms: Option<Arc<Vec<crate::config::RoomConfig>>>,
    /// Where the plenum's dates come from, and how the last read went.
    pub calendar: Arc<std::sync::Mutex<crate::calendar::Status>>,
}

pub type PendingStarts =
    std::collections::HashMap<String, (crate::checkins::Plan, std::time::Instant)>;

impl Kk {
    pub fn is_test(&self) -> bool {
        self.test_users.is_some()
    }

    /// Whether `user` may use the Kummerkasten in this mode at all.
    pub fn may_use(&self, user: &str) -> bool {
        self.test_users
            .as_ref()
            .is_none_or(|users| users.iter().any(|u| u == user))
    }

    pub async fn authorized_rooms(&self) -> Vec<OwnedRoomId> {
        let rooms = self
            .engine
            .db
            .run(|c| store::authorized_rooms(c))
            .await
            .unwrap_or_default();
        rooms
            .into_iter()
            .filter_map(|(id, _)| OwnedRoomId::try_from(id).ok())
            .collect()
    }

    /// `user` with the authorized rooms they are in right now (asked of
    /// the server); `None` without access.
    pub async fn who(&self, user: &UserId) -> Option<Who> {
        self.who_checked(user).await.ok().flatten()
    }

    /// [`Kk::who`], with "the server couldn't say" (`Err`) apart from "not
    /// a member" (`None`): only the second locks a chat and drops what
    /// they were answering.
    pub async fn who_checked(&self, user: &UserId) -> Result<Option<Who>> {
        // Test mode: nobody but the admins, whichever room they are in.
        if !self.may_use(user.as_str()) {
            return Ok(None);
        }
        let rooms = self.authorized_rooms().await;
        let joined = access::joined_rooms(&self.client, &rooms, user).await?;
        Ok((!joined.is_empty()).then(|| Who {
            mxid: user.to_string(),
            rooms: joined,
        }))
    }

    /// The chat is still exactly the bot and `user`, joined and encrypted.
    pub async fn chat_is_private(&self, room: &Room, user: &UserId) -> bool {
        mxbot_common::private::is_private_dm(&self.client, room.room_id(), user).await
    }

    async fn resident_of(&self, room: &Room) -> Option<store::Resident> {
        let room_id = room.room_id().to_string();
        self.engine
            .db
            .run(move |c| store::resident_by_room(c, &room_id))
            .await
            .ok()
            .flatten()
    }

    pub async fn is_chat(&self, room: &Room) -> bool {
        self.resident_of(room).await.is_some()
    }
}

/// Send `msgs` to `mxid`'s chat `room`, tracking every event (edits too:
/// they carry content).
///
/// A chat has one screen, the message whose buttons work. A new screen
/// edits it in place while it is still the last message in the chat and
/// part of the current flow ([`store::screen_is_editable`]); its buttons are
/// brought in line with the new view. Otherwise (something was said after
/// it, the flow ended, an older screen) a fresh screen is sent and the old
/// one is redacted right away. Notes and receipts are messages of their
/// own; a receipt with buttons (the deletion code) becomes the screen but
/// is never edited.
pub async fn deliver(kk: &Kk, room: &Room, mxid: &str, msgs: Vec<Msg>) {
    deliver_txn(kk, room, mxid, msgs, None).await;
}

/// [`deliver`], the first new message sent with `txn` (the outbox's, so a
/// retry after a crash isn't posted twice). Returns whether that first
/// message went out (nothing to send counts as out).
pub async fn deliver_txn(
    kk: &Kk,
    room: &Room,
    mxid: &str,
    msgs: Vec<Msg>,
    mut txn: Option<&TransactionId>,
) -> bool {
    let mut first = None;
    for msg in msgs {
        if msg.live && msg.kind != Kind::Receipt {
            let shown = show_screen(kk, room, mxid, &msg, txn.take()).await;
            first.get_or_insert(shown);
            continue;
        }
        let content = markup::notice(&msg.text);
        let sent = match txn.take() {
            Some(txn) => send_with_retry_txn(room, content, txn).await,
            None => send_with_retry(room, content).await,
        };
        let event_id = match sent {
            Ok(id) => id,
            Err(error) => {
                // Carry on: a screen after a lost note is what keeps the
                // chat usable; the flow has moved on already.
                tracing::warn!(%error, "Could not send to a Kummerkasten chat");
                first.get_or_insert(false);
                continue;
            }
        };
        first.get_or_insert(true);
        record(kk, room, &event_id, msg.in_session()).await;
        if msg.live {
            // The code receipt: its buttons work, it isn't edited.
            let buttons = add_buttons(kk, room, &event_id, &msg.buttons, false).await;
            let complete = buttons.len() == msg.buttons.len();
            if let Err(error) = kk
                .engine
                .screen_shown(mxid, event_id.as_str(), complete)
                .await
            {
                tracing::error!(%error, "Could not remember the live screen");
            }
        }
    }
    first.unwrap_or(true)
}

/// Said when handling a tap or message failed on the bot's side. Nothing
/// about what went wrong: the chat holds private text, logs hold the error.
const SORRY: &str = "😕 Something went wrong on my side. Please try that again, or send `!menu`.";

/// The server couldn't confirm they're still in the house: nothing done,
/// nothing lost.
const UNSURE: &str =
    "📡 I couldn't check that with the server just now.\n⏳ Nothing is lost: please try again in a moment.";

/// Show `msg` as the chat's screen: edit the current one, or send a fresh
/// one (with `txn` if given). Returns whether it is shown.
async fn show_screen(
    kk: &Kk,
    room: &Room,
    mxid: &str,
    msg: &Msg,
    txn: Option<&TransactionId>,
) -> bool {
    let room_id = room.room_id().to_string();
    let current = kk
        .engine
        .screen_event(mxid)
        .await
        .ok()
        .flatten()
        .filter(|(r, _)| *r == room_id)
        .map(|(_, e)| e);
    if let Some(event) = &current {
        let (r, e) = (room_id.clone(), event.clone());
        let (editable, had) = kk
            .engine
            .db
            .run(move |c| {
                Ok((
                    store::screen_is_editable(c, &r, &e)?,
                    store::screen_buttons(c, &r, &e)?.unwrap_or_default(),
                ))
            })
            .await
            .unwrap_or_default();
        // Element orders a message's reactions by when each key first came
        // under it, even if it was taken away since: an edit to other
        // buttons puts the ones used before (⬅️) at their old place. Harmless
        // while all are shown; with more than Element shows, a fresh screen,
        // so the step buttons it puts first are the ones in sight.
        let editable = editable && screens::can_edit_reaction_order(&had, &msg.buttons);
        if editable {
            match edit_screen(kk, room, event, msg).await {
                Ok(complete) => {
                    if let Err(error) = kk.engine.screen_shown(mxid, event, complete).await {
                        tracing::error!(%error, "Could not confirm the screen");
                    }
                    return true;
                }
                // Gone, or the server refused: a fresh screen instead, so
                // nobody is left with a dead one.
                Err(error) => {
                    tracing::warn!(%error, "Could not edit the screen; sending a fresh one")
                }
            }
        }
    }
    let content = markup::notice(&msg.text);
    let sent = match txn {
        Some(txn) => send_with_retry_txn(room, content, txn).await,
        None => send_with_retry(room, content).await,
    };
    let event_id = match sent {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, "Could not send to a Kummerkasten chat");
            return false;
        }
    };
    record(kk, room, &event_id, true).await;
    // Known as the screen before its buttons arrive: a tap meanwhile is
    // handled, and the screen stays pending until they are all there.
    if let Err(error) = kk.engine.screen_shown(mxid, event_id.as_str(), false).await {
        tracing::error!(%error, "Could not remember the live screen");
    }
    let buttons = add_buttons(kk, room, &event_id, &msg.buttons, true).await;
    if buttons.len() == msg.buttons.len() {
        if let Err(error) = kk.engine.screen_shown(mxid, event_id.as_str(), true).await {
            tracing::error!(%error, "Could not confirm the screen");
        }
    }
    let (r, e, old) = (room_id, event_id.to_string(), current);
    let saved = kk
        .engine
        .db
        .run(move |c| {
            store::set_screen_buttons(c, &r, &e, &buttons)?;
            // The screen it replaces goes now, not at the end of the flow.
            Ok(match old {
                Some(old) if old != e => store::replace_screen(c, &r, &old)?,
                _ => 0,
            })
        })
        .await;
    match saved {
        Ok(replaced) if replaced > 0 => kk.wake.notify_one(),
        Ok(_) => {}
        Err(error) => tracing::error!(%error, "Could not remember the screen's buttons"),
    }
    true
}

/// Edit the screen `event` to show `msg`, then bring its buttons in line:
/// the bot's reactions that still fit (a common start) stay, the rest are
/// redacted and the new ones added in order, so every button shown works
/// and they keep the order of [`screens::View::keys`] (the text's, but on
/// a crowded screen the step buttons first: the caller sends those as a
/// fresh screen when their order would change).
///
/// An error means the edit itself did not go out (the caller sends a fresh
/// screen then). Once it is out, trouble with the bookkeeping is logged:
/// a second screen would be worse than a button ledger that is behind.
/// Returns whether every button of the new view is in place.
async fn edit_screen(kk: &Kk, room: &Room, event: &str, msg: &Msg) -> Result<bool> {
    let original = OwnedEventId::try_from(event).context("not an event ID")?;
    let edit = markup::notice(&msg.text)
        .make_replacement(ReplacementMetadata::new(original.clone(), None));
    let edit_id = send_with_retry(room, edit).await?;
    let room_id = room.room_id().to_string();
    let ledger = {
        let (r, e, x) = (room_id.clone(), event.to_owned(), edit_id.to_string());
        kk.engine
            .db
            .run(move |c| {
                store::record_edit(c, &r, &x, &e)?;
                Ok(store::screen_buttons(c, &r, &e)?.unwrap_or_default())
            })
            .await
            .unwrap_or_else(|error| {
                tracing::error!(%error, "Could not record a screen edit");
                Vec::new()
            })
    };
    // What is really under the message, asked of the server: a reaction
    // whose redaction is still waiting, or one a crash kept out of the
    // ledger, is seen (and tracked) instead of fought with.
    let old = match server_buttons(kk, room, &original).await {
        Ok(found) => {
            for (_, reaction) in &found {
                let (r, x, e) = (room_id.clone(), reaction.clone(), event.to_owned());
                let _ = kk
                    .engine
                    .db
                    .run(move |c| store::record_button(c, &r, &x, &e, true))
                    .await;
            }
            found
        }
        Err(error) => {
            tracing::debug!(%error, "Could not list the buttons; going by the ledger");
            ledger
        }
    };
    let keep = screens::buttons_kept(&old, &msg.buttons);
    for (_, reaction) in &old[keep..] {
        // Taken away now; when that fails, it is marked for the redaction
        // worker (retried with backoff) instead of being forgotten.
        let gone = match OwnedEventId::try_from(reaction.as_str()) {
            Ok(id) => room.redact(&id, None, None).await.is_ok(),
            Err(_) => false,
        };
        let (r, x) = (room_id.clone(), reaction.clone());
        let saved = kk
            .engine
            .db
            .run(move |c| {
                if gone {
                    store::forget_event(c, &r, &x)
                } else {
                    store::doom_event(c, &r, &x).map(drop)
                }
            })
            .await;
        if let Err(error) = saved {
            tracing::error!(%error, "Could not note a removed button");
        }
        if !gone {
            tracing::warn!("Could not take a button away; it is queued for redaction");
            kk.wake.notify_one();
        }
    }
    let mut buttons = old[..keep].to_vec();
    buttons.extend(add_buttons(kk, room, &original, &msg.buttons[keep..], true).await);
    let complete = buttons.len() == msg.buttons.len();
    let (r, e) = (room_id, event.to_owned());
    if let Err(error) = kk
        .engine
        .db
        .run(move |c| store::set_screen_buttons(c, &r, &e, &buttons))
        .await
    {
        tracing::error!(%error, "Could not remember the screen's buttons");
    }
    Ok(complete)
}

/// The bot's reactions under `event_id`, one per key: `(key, reaction)` of
/// those that could be added. Each is tracked with the message (it goes,
/// and is redacted, when the message is); a missing one is logged, and the
/// caller leaves the screen pending so it is brought in line later.
async fn add_buttons(
    kk: &Kk,
    room: &Room,
    event_id: &OwnedEventId,
    keys: &[String],
    in_session: bool,
) -> Vec<(String, String)> {
    let mut added = Vec::new();
    for key in keys {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), key.clone()));
        let id = match send_with_retry(room, reaction).await {
            Ok(id) => Some(id),
            // Already there (its redaction still waiting, or a crash kept
            // it out of the ledger): use that one.
            Err(error) if is_duplicate(&error) => server_buttons(kk, room, event_id)
                .await
                .ok()
                .and_then(|found| {
                    found
                        .into_iter()
                        .find(|(k, _)| screens::same_key(k, key))
                        .and_then(|(_, id)| OwnedEventId::try_from(id).ok())
                }),
            Err(error) => {
                tracing::warn!(%error, "Could not add a button");
                None
            }
        };
        let Some(id) = id else {
            continue;
        };
        let (r, x, e) = (
            room.room_id().to_string(),
            id.to_string(),
            event_id.to_string(),
        );
        if let Err(error) = kk
            .engine
            .db
            .run(move |c| store::record_button(c, &r, &x, &e, in_session))
            .await
        {
            tracing::error!(%error, "Could not record a button");
        }
        added.push((key.clone(), id.to_string()));
    }
    added
}

/// The homeserver refused a reaction because the same one is already there.
fn is_duplicate(error: &matrix_sdk::Error) -> bool {
    matches!(
        error,
        matrix_sdk::Error::Http(http)
            if matches!(http.client_api_error_kind(), Some(ErrorKind::DuplicateAnnotation))
    )
}

/// The bot's own reactions under `event`, as the server has them, oldest
/// first: `(key, reaction event)`.
async fn server_buttons(
    kk: &Kk,
    room: &Room,
    event: &OwnedEventId,
) -> Result<Vec<(String, String)>> {
    let mut opts = RelationsOptions {
        include_relations: IncludeRelations::RelationsOfType(RelationType::Annotation),
        limit: UInt::new(100),
        ..Default::default()
    };
    let mut found: Vec<(u64, String, String)> = Vec::new();
    loop {
        let relations = room.relations(event.clone(), opts.clone()).await?;
        for item in relations.chunk {
            let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Reaction(
                SyncReactionEvent::Original(reaction),
            ))) = item.raw().deserialize()
            else {
                continue;
            };
            if reaction.sender != kk.bot {
                continue;
            }
            found.push((
                u64::from(reaction.origin_server_ts.get()),
                reaction.content.relates_to.key,
                reaction.event_id.to_string(),
            ));
        }
        match relations.next_batch_token {
            Some(next) => opts.from = Some(next),
            None => break,
        }
    }
    found.sort();
    Ok(found.into_iter().map(|(_, key, id)| (key, id)).collect())
}

/// One of the bot's own messages in a chat came back through sync: the
/// screen it shows (the message, or the one it edits) is in sight now.
pub async fn on_own_message(kk: &Kk, room: &Room, ev: &OriginalSyncRoomMessageEvent) {
    if !kk.is_chat(room).await {
        return;
    }
    let screen = match &ev.content.relates_to {
        Some(Relation::Replacement(replacement)) => replacement.event_id.to_string(),
        _ => ev.event_id.to_string(),
    };
    if let Err(error) = kk
        .engine
        .screen_echoed(room.room_id().as_str(), &screen)
        .await
    {
        tracing::error!(%error, "Could not note an echoed screen");
    }
}

/// Screens whose last view wasn't confirmed as shown (a crash between
/// saving a step and showing it, a button that could not be added): show
/// them again. At startup; between starts the next tap does the same.
pub async fn resume_screens(kk: &Kk) {
    let pending = match kk
        .engine
        .db
        .run(|c| crate::engine::pending_screens(c))
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(%error, "Reading pending screens failed");
            return;
        }
    };
    let ttl = kk.settings.get().session_ttl();
    for (mxid, room_id) in pending {
        let Some(room) = OwnedRoomId::try_from(room_id.as_str())
            .ok()
            .and_then(|id| kk.client.get_room(&id))
            .filter(|r| r.state() == RoomState::Joined)
        else {
            continue;
        };
        let Ok(user) = OwnedUserId::try_from(mxid.as_str()) else {
            continue;
        };
        let _op = kk.ops.lock().await;
        let Some(who) = kk.who(&user).await else {
            continue;
        };
        if !kk.chat_is_private(&room, &user).await {
            continue;
        }
        match kk
            .engine
            .current_screen(&who, &room_id, Utc::now(), ttl)
            .await
        {
            Ok(msgs) => deliver(kk, &room, &mxid, msgs).await,
            Err(error) => tracing::error!(%error, "Could not show a pending screen"),
        }
    }
    kk.wake.notify_one();
}

/// Track an event in a resident's chat (see [`store::record_event`]).
pub async fn track(kk: &Kk, room: &Room, event_id: &OwnedEventId, in_session: bool) {
    record(kk, room, event_id, in_session).await;
}

async fn record(kk: &Kk, room: &Room, event_id: &OwnedEventId, in_session: bool) {
    let (room_id, id) = (room.room_id().to_string(), event_id.to_string());
    if let Err(error) = kk
        .engine
        .db
        .run(move |c| store::record_event(c, &room_id, &id, in_session))
        .await
    {
        tracing::error!(%error, "Could not record a chat event");
    }
}

/// Buttons under a message in a house room (📮 and friends): not tracked,
/// the bot redacts nothing there.
async fn seed(room: &Room, event_id: &OwnedEventId, keys: &[String]) {
    for key in keys {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), key.clone()));
        if let Err(error) = send_with_retry(room, reaction).await {
            tracing::warn!(%error, "Could not add a button");
        }
    }
}

/// Take a tap away again, so the same button can be pressed next time.
async fn consume(kk: &Kk, room: &Room, reaction: &OwnedEventId) {
    let allowed = room
        .power_levels()
        .await
        .is_ok_and(|levels| levels.user_can_redact_event_of_other(&kk.bot));
    if allowed {
        if let Err(error) = room.redact(reaction, None, None).await {
            tracing::debug!(%error, "Could not clear a tap");
        }
    }
}

/// Lost access: drop the conversation, say so, no house data.
async fn lock(kk: &Kk, room: &Room, mxid: &str) {
    let (m, room_id) = (mxid.to_owned(), room.room_id().to_string());
    let _ = kk
        .engine
        .db
        .run(move |c| {
            c.execute("DELETE FROM sessions WHERE mxid = ?1", [&m])?;
            crate::engine::tidy(c, &room_id)?;
            Ok(())
        })
        .await;
    deliver(kk, room, mxid, vec![Msg::note(&screens::locked())]).await;
    kk.wake.notify_one();
}

/// A message in a resident's chat.
pub async fn on_message(
    kk: &Kk,
    room: &Room,
    ev: &OriginalSyncRoomMessageEvent,
    encryption: Option<&EncryptionInfo>,
) {
    let MessageType::Text(text) = &ev.content.msgtype else {
        return;
    };
    if matches!(ev.content.relates_to, Some(Relation::Replacement(_))) {
        return;
    }
    let Some(resident) = kk.resident_of(room).await else {
        return;
    };
    if resident.mxid != ev.sender.as_str() || encryption.is_none() {
        return;
    }
    let _op = kk.ops.lock().await;
    let event_id = ev.event_id.to_string();
    let first = {
        let id = event_id.clone();
        kk.engine.db.run(move |c| store::first_time(c, &id)).await
    };
    if !first.unwrap_or(false) || !kk.chat_is_private(room, &ev.sender).await {
        return;
    }
    let who = match kk.who_checked(&ev.sender).await {
        Ok(Some(who)) => who,
        Ok(None) => {
            lock(kk, room, &resident.mxid).await;
            return;
        }
        Err(_) => {
            // Their message is kept as a chat event (cleared with the rest).
            let (room_id, id) = (room.room_id().to_string(), event_id.clone());
            let _ = kk
                .engine
                .db
                .run(move |c| store::record_event(c, &room_id, &id, true))
                .await;
            deliver(kk, room, &resident.mxid, vec![Msg::note(UNSURE)]).await;
            return;
        }
    };
    {
        let (room_id, id) = (room.room_id().to_string(), event_id.clone());
        let _ = kk
            .engine
            .db
            .run(move |c| store::record_event(c, &room_id, &id, true))
            .await;
    }
    let body = text.body.trim().to_owned();
    let now = Utc::now();
    let msgs = if matches!(body.to_lowercase().as_str(), "!menu" | "!start" | "menu") {
        if kk.engine.is_survey() {
            // No menu: where they are in the check-in, again at the bottom.
            let ttl = kk.settings.get().session_ttl();
            kk.engine
                .current_screen(&who, room.room_id().as_str(), now, ttl)
                .await
                .map(|msgs| {
                    if msgs.is_empty() {
                        vec![Msg::note(screens::nothing_open())]
                    } else {
                        msgs
                    }
                })
        } else {
            kk.engine
                .fresh_menu(&who, room.room_id().as_str(), now)
                .await
        }
    } else if screens::asks_for_help(&body) {
        // Before the flow sees it: on a writing step it would become text.
        let ttl = kk.settings.get().session_ttl();
        kk.engine
            .help(&who, room.room_id().as_str(), now, ttl)
            .await
    } else {
        let ttl = kk.settings.get().session_ttl();
        kk.engine
            .handle(
                &who,
                room.room_id().as_str(),
                Input::Text { body, event_id },
                now,
                ttl,
            )
            .await
    };
    match msgs {
        Ok(msgs) => deliver(kk, room, &resident.mxid, msgs).await,
        Err(error) => {
            tracing::error!(%error, "A Kummerkasten message failed");
            deliver(kk, room, &resident.mxid, vec![Msg::note(SORRY)]).await;
        }
    }
    kk.wake.notify_one();
}

/// A tap in a resident's chat: counted only on their live screen (or 📊 on
/// a check-in notice).
///
/// Unlike messages, reactions needn't arrive encrypted: Element Web and
/// FluffyChat send them in the clear even in encrypted rooms (an emoji and
/// the event it points at: public anyway). They carry no text; the chat
/// itself is still checked to be private and encrypted before anything
/// happens.
///
/// The tap is taken away again (so the button can be pressed once more)
/// only *after* the screen has changed. While their reaction is still
/// there, a second tap on the same button is, to the client, taking it
/// back: a redaction the bot ignores. Taking it away first would open a
/// moment in which the old screen still shows but a tap lands on the new
/// one (1️⃣ on the topic list choosing location 1️⃣).
pub async fn on_reaction(kk: &Kk, room: &Room, ev: &OriginalSyncReactionEvent) {
    let Some(resident) = kk.resident_of(room).await else {
        return;
    };
    if resident.mxid != ev.sender.as_str() {
        return;
    }
    let _op = kk.ops.lock().await;
    let target = ev.content.relates_to.event_id.to_string();
    let live = kk.engine.screen_event(&resident.mxid).await.ok().flatten();
    let on_screen = live.is_some_and(|(room_id, event_id)| {
        room_id == room.room_id().as_str() && event_id == target
    });
    // 📊 on a check-in notice or reminder.
    let checkin =
        if on_screen || !screens::same_key(&ev.content.relates_to.key, screens::START_CHECKIN) {
            None
        } else {
            let room_id = room.room_id().to_string();
            kk.engine
                .db
                .run(move |c| {
                    let _ = &room_id;
                    checkins::message_run(c, &target)
                })
                .await
                .ok()
                .flatten()
        };
    if !on_screen && checkin.is_none() {
        return;
    }
    let first = {
        let id = ev.event_id.to_string();
        kk.engine.db.run(move |c| store::first_time(c, &id)).await
    };
    if !first.unwrap_or(false) || !kk.chat_is_private(room, &ev.sender).await {
        return;
    }
    let who = match kk.who_checked(&ev.sender).await {
        Ok(Some(who)) => who,
        Ok(None) => {
            lock(kk, room, &resident.mxid).await;
            return;
        }
        Err(_) => {
            deliver(kk, room, &resident.mxid, vec![Msg::note(UNSURE)]).await;
            consume(kk, room, &ev.event_id).await;
            return;
        }
    };
    let ttl = kk.settings.get().session_ttl();
    let key = ev.content.relates_to.key.clone();
    let room_id = room.room_id().as_str();
    let result = match checkin {
        Some(run) => {
            kk.engine
                .start_run(&who, room_id, run, Utc::now(), ttl)
                .await
        }
        None => {
            kk.engine
                .handle(&who, room_id, Input::Tap(key), Utc::now(), ttl)
                .await
        }
    };
    // A reaction that isn't a button of this screen (a 🦄 for the bot) is
    // left alone: it did nothing and is theirs.
    let pressed = match result {
        Ok(msgs) if msgs.is_empty() => false,
        Ok(msgs) => {
            deliver(kk, room, &resident.mxid, msgs).await;
            true
        }
        Err(error) => {
            tracing::error!(%error, "A Kummerkasten tap failed");
            deliver(kk, room, &resident.mxid, vec![Msg::note(SORRY)]).await;
            true
        }
    };
    if pressed {
        consume(kk, room, &ev.event_id).await;
    }
    kk.wake.notify_one();
}

/// How long the bot waits after someone from another homeserver accepted
/// their chat before it writes there. Sent in the same second as the join,
/// the check-in reached the bot's own server but never matrix.org: 11 of 11
/// residents from there had an empty chat (2026-10-20, `get-remote-pdu`:
/// "event not found"), while a later message in the same chat brought it
/// along. Their server is apparently still busy with the join and drops
/// what arrives meanwhile.
pub const FEDERATED_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether a membership event is someone from another homeserver joining:
/// that one waits [`FEDERATED_JOIN_GRACE`] before it is handled.
pub fn waits_after_join(bot: &UserId, ev: &OriginalSyncRoomMemberEvent) -> bool {
    ev.content.membership == MembershipState::Join
        && ev.state_key.server_name() != bot.server_name()
}

/// `!kk checkin remind <n>`: check-in `run_id`'s reminder now, into every
/// accepted chat whose resident hasn't answered (checked per person, as for
/// the planned reminder, which still comes). A new message in a chat also
/// makes a federated server fetch what it missed there (a check-in sent
/// before [`FEDERATED_JOIN_GRACE`] existed). Returns
/// `(sent, not needed, failed)`; counts only, never who.
pub async fn remind_now(kk: &Kk, run_id: i64) -> Result<(usize, usize, usize)> {
    let chats = kk.engine.db.run(|c| store::open_chats(c)).await?;
    let (mut sent, mut not_needed, mut failed) = (0, 0, 0);
    for (mxid, room_id) in chats {
        let (Ok(user), Ok(room_id)) = (
            OwnedUserId::try_from(mxid.as_str()),
            OwnedRoomId::try_from(room_id.as_str()),
        ) else {
            not_needed += 1;
            continue;
        };
        let Some(room) = kk.client.get_room(&room_id) else {
            not_needed += 1;
            continue;
        };
        let who = match kk.who_checked(&user).await {
            Ok(Some(who)) => who,
            Ok(None) => {
                not_needed += 1;
                continue;
            }
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        match tell_txn(
            kk,
            &room,
            &who,
            run_id,
            CheckinText::Reminder,
            &TransactionId::new(),
        )
        .await
        {
            Told::Sent => sent += 1,
            Told::NotNeeded => not_needed += 1,
            Told::Failed => failed += 1,
        }
    }
    if sent > 0 {
        kk.engine
            .db
            .run(move |c| checkins::count_batch(c, run_id, Batch::Reminder, 0, sent))
            .await?;
    }
    tracing::info!(
        run = run_id,
        sent,
        not_needed,
        failed,
        "Check-in reminder sent by an admin"
    );
    Ok((sent, not_needed, failed))
}

/// Someone joined or left a resident's chat.
pub async fn on_member(kk: &Kk, room: &Room, ev: &OriginalSyncRoomMemberEvent) {
    let Some(resident) = kk.resident_of(room).await else {
        return;
    };
    if ev.state_key.as_str() != resident.mxid {
        return;
    }
    let _op = kk.ops.lock().await;
    let mxid = resident.mxid.clone();
    match ev.content.membership {
        MembershipState::Join if !resident.dm_joined => {
            let m = mxid.clone();
            let _ = kk.engine.db.run(move |c| store::set_joined(c, &m)).await;
            let who = match kk.who_checked(&ev.state_key).await {
                Ok(Some(who)) => who,
                Ok(None) => {
                    lock(kk, room, &mxid).await;
                    return;
                }
                // The server couldn't say: nothing now, their next tap or
                // message (or the next notice) tries again.
                Err(_) => return,
            };
            if kk.engine.is_survey() {
                // No introduction, no menu: the check-in waiting for them.
                join_survey(kk, room, &who).await;
                tracing::info!("A resident opened their Kummerkasten chat");
                return;
            }
            deliver(kk, room, &mxid, vec![Msg::receipt(screens::intro())]).await;
            if let Ok(menu) = kk
                .engine
                .fresh_menu(&who, room.room_id().as_str(), Utc::now())
                .await
            {
                deliver(kk, room, &mxid, menu).await;
            }
            // Only now are they told about open check-ins meant for them
            // (an invitation alone isn't being told).
            let open = kk
                .engine
                .db
                .run(|c| checkins::announced_open_runs(c, Utc::now()))
                .await
                .unwrap_or_default();
            for run_id in open {
                if tell(kk, room, &who, run_id, CheckinText::Notice).await {
                    let _ = kk
                        .engine
                        .db
                        .run(move |c| checkins::count_batch(c, run_id, Batch::Notice, 0, 1))
                        .await;
                }
            }
            tracing::info!("A resident opened their Kummerkasten chat");
        }
        MembershipState::Leave | MembershipState::Ban => {
            let room_id = room.room_id().to_string();
            let _ = kk
                .engine
                .db
                .run(move |c| store::drop_dm(c, &mxid, &room_id))
                .await;
            // The bot leaves once what it sent there is redacted (scheduler).
            kk.wake.notify_one();
        }
        _ => {}
    }
}

/// Where to open a chat: the menu, or 📈 / 💡.
fn destination(key: &str) -> Option<Option<MenuItem>> {
    if screens::same_key(key, ENTRY) {
        Some(None)
    } else if screens::same_key(key, RESULTS) {
        Some(Some(MenuItem::Results))
    } else if screens::same_key(key, IDEAS) {
        Some(Some(MenuItem::Ideas))
    } else {
        None
    }
}

#[derive(Debug, PartialEq)]
pub enum Opened {
    /// Their chat was there: a fresh menu is in it.
    Existing,
    /// Invited (again) to their chat.
    Invited,
    /// Not in an authorized room.
    Locked,
    /// Survey-only mode: no check-in waiting for them, so no chat to open.
    Nothing,
}

/// Open (or bring up) `user`'s Kummerkasten chat.
pub async fn open_chat(kk: &Kk, user: &UserId) -> Result<Opened> {
    open_chat_at(kk, user, None).await
}

/// Open (or bring up) `user`'s chat, at `at` if they have one already.
pub async fn open_chat_at(kk: &Kk, user: &UserId, at: Option<MenuItem>) -> Result<Opened> {
    let Some(who) = kk.who_checked(user).await? else {
        return Ok(Opened::Locked);
    };
    if kk.engine.is_survey() && !kk.engine.has_survey_for(&who, Utc::now()).await? {
        return Ok(Opened::Nothing);
    }
    let mxid = user.to_string();
    let resident = {
        let m = mxid.clone();
        kk.engine
            .db
            .run(move |c| store::resident_by_mxid(c, &m))
            .await?
    };
    if let Some(resident) = resident {
        let room = resident
            .dm_room_id
            .as_deref()
            .and_then(|id| OwnedRoomId::try_from(id).ok())
            .and_then(|id| kk.client.get_room(&id))
            .filter(|r| r.state() == RoomState::Joined);
        if let Some(room) = room {
            if resident.dm_joined && kk.chat_is_private(&room, user).await {
                let _op = kk.ops.lock().await;
                let menu = match at {
                    Some(item) => {
                        kk.engine
                            .open_at(&who, room.room_id().as_str(), item, Utc::now())
                            .await?
                    }
                    None => {
                        kk.engine
                            .fresh_menu(&who, room.room_id().as_str(), Utc::now())
                            .await?
                    }
                };
                deliver(kk, &room, &mxid, menu).await;
                kk.wake.notify_one();
                return Ok(Opened::Existing);
            }
            if !resident.dm_joined && room.invite_user_by_id(user).await.is_ok() {
                return Ok(Opened::Invited);
            }
        }
    }
    let room = mxbot_common::private::create_private_dm(
        &kk.client,
        user,
        &PREFERRED_SUPPORTED_ROOM_VERSIONS,
    )
    .await?;
    let room_id = room.room_id().to_string();
    kk.engine
        .db
        .run(move |c| store::set_dm(c, &mxid, &room_id))
        .await?;
    tracing::info!("Opened a Kummerkasten chat");
    Ok(Opened::Invited)
}

/// `!kummerkasten` in an authorized room: open their chat, nod with 📬.
pub async fn on_room_message(kk: &Kk, room: &Room, ev: &OriginalSyncRoomMessageEvent) {
    let MessageType::Text(text) = &ev.content.msgtype else {
        return;
    };
    if !text.body.trim().eq_ignore_ascii_case("!kummerkasten") {
        return;
    }
    if !kk
        .authorized_rooms()
        .await
        .iter()
        .any(|r| r == room.room_id())
    {
        return;
    }
    let first = {
        let id = ev.event_id.to_string();
        kk.engine.db.run(move |c| store::first_time(c, &id)).await
    };
    if !first.unwrap_or(false) {
        return;
    }
    match open_chat(kk, &ev.sender).await {
        Ok(Opened::Locked | Opened::Nothing) => {}
        Ok(_) => {
            let nod =
                ReactionEventContent::new(Annotation::new(ev.event_id.clone(), "📬".to_owned()));
            let _ = room.send(nod).await;
        }
        Err(error) => tracing::warn!(%error, "Could not open a Kummerkasten chat"),
    }
}

/// 📮 on an entry message (📈 / 💡 too, under a plenum summary).
pub async fn on_entry_reaction(kk: &Kk, room: &Room, ev: &OriginalSyncReactionEvent) {
    let Some(at) = destination(&ev.content.relates_to.key) else {
        return;
    };
    let (room_id, target) = (
        room.room_id().to_string(),
        ev.content.relates_to.event_id.to_string(),
    );
    let is_entry = kk
        .engine
        .db
        .run(move |c| store::is_entry(c, &room_id, &target))
        .await
        .unwrap_or(false);
    if !is_entry {
        return;
    }
    let first = {
        let id = ev.event_id.to_string();
        kk.engine.db.run(move |c| store::first_time(c, &id)).await
    };
    if !first.unwrap_or(false) {
        return;
    }
    if let Err(error) = open_chat_at(kk, &ev.sender, at).await {
        tracing::warn!(%error, "Could not open a Kummerkasten chat");
    }
    consume(kk, room, &ev.event_id).await;
}

/// Post an entry message with its 📮 into an authorized room.
pub async fn post_entry(kk: &Kk, room: &Room) -> Result<()> {
    let text = "📮 **Kummerkasten**\n\n\
                Something on your mind? A problem, an idea, a thank-you? 🌱\n\
                Tap 📮 (or write `!kummerkasten`) and I'll open a private chat with you.\n\n\
                🥷 anonymous if you like · 🔐 private · 🧹 tidied up after every plenum";
    let event_id = post_with_keys(kk, room, text, &[ENTRY]).await?;
    let (room_id, id) = (room.room_id().to_string(), event_id.to_string());
    kk.engine
        .db
        .run(move |c| store::mark_entry_posted(c, &room_id, &id))
        .await
}

/// `entry = true` rooms of the config: post the 📮 entry message where
/// none was posted yet and the bot is in the room (it may be invited later).
pub async fn post_missing_entries(kk: &Kk) {
    // Survey-only mode: residents are asked; there is nothing to open.
    if kk.engine.is_survey() {
        return;
    }
    let Some(rooms) = &kk.managed_rooms else {
        return;
    };
    for config in rooms.iter().filter(|r| r.entry) {
        let id = config.id.clone();
        if kk
            .engine
            .db
            .run(move |c| store::entry_posted(c, &id))
            .await
            .unwrap_or(true)
        {
            continue;
        }
        let Some(room) = OwnedRoomId::try_from(config.id.as_str())
            .ok()
            .and_then(|id| kk.client.get_room(&id))
            .filter(|r| r.state() == RoomState::Joined)
        else {
            continue;
        };
        match post_entry(kk, &room).await {
            Ok(()) => tracing::info!(room = %config.label, "Posted the entry message"),
            Err(error) => {
                tracing::warn!(%error, room = %config.label, "Could not post the entry message")
            }
        }
    }
}

/// `text` with buttons that open a private chat: 📮 (and 📈 / 💡).
pub async fn post_with_keys(
    kk: &Kk,
    room: &Room,
    text: &str,
    keys: &[&str],
) -> Result<OwnedEventId> {
    post_with_keys_txn(kk, room, text, keys, &TransactionId::new()).await
}

/// [`post_with_keys`] with the transaction ID `txn` (the outbox's).
pub async fn post_with_keys_txn(
    kk: &Kk,
    room: &Room,
    text: &str,
    keys: &[&str],
    txn: &TransactionId,
) -> Result<OwnedEventId> {
    let event_id = send_with_retry_txn(room, markup::notice(text), txn)
        .await
        .context("Posting into a house room")?;
    seed(
        room,
        &event_id,
        &keys.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
    )
    .await;
    let (room_id, id) = (room.room_id().to_string(), event_id.to_string());
    kk.engine
        .db
        .run(move |c| store::add_entry(c, &room_id, &id))
        .await?;
    Ok(event_id)
}

/// Change a message the bot posted in a house room, in place.
pub async fn edit_notice(room: &Room, event_id: &str, text: &str) -> Result<()> {
    let original = OwnedEventId::try_from(event_id).context("not an event ID")?;
    let edit = markup::notice(text).make_replacement(ReplacementMetadata::new(original, None));
    send_with_retry(room, edit).await?;
    Ok(())
}

/// The homeserver as [`reconcile`] needs it. `only`: just this user
/// (someone who just joined an authorized room).
pub struct Matrix<'a> {
    pub kk: &'a Kk,
    pub only: Option<OwnedUserId>,
}

impl Homeserver for Matrix<'_> {
    async fn members(&self) -> Result<Vec<String>> {
        if let Some(user) = &self.only {
            return Ok(vec![user.to_string()]);
        }
        let mut users = Vec::new();
        for room_id in self.kk.authorized_rooms().await {
            let Some(room) = self.kk.client.get_room(&room_id) else {
                continue;
            };
            for member in room
                .members(mxbot_common::matrix_sdk::RoomMemberships::JOIN)
                .await?
            {
                if member.user_id() != self.kk.bot {
                    users.push(member.user_id().to_string());
                }
            }
        }
        Ok(users)
    }

    async fn rooms_of(&self, user: &str) -> Vec<String> {
        match OwnedUserId::try_from(user) {
            Ok(user) => self
                .kk
                .who(&user)
                .await
                .map(|w| w.rooms)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    async fn create_chat(&self, user: &str) -> Result<String, Failure> {
        let user = OwnedUserId::try_from(user).map_err(|e| Failure::Failed(e.to_string()))?;
        match mxbot_common::private::create_private_dm(
            &self.kk.client,
            &user,
            &PREFERRED_SUPPORTED_ROOM_VERSIONS,
        )
        .await
        {
            Ok(room) => Ok(room.room_id().to_string()),
            Err(error) => {
                // Rate-limited (or a transient server error): wait and retry.
                let limited = error
                    .downcast_ref::<mxbot_common::matrix_sdk::Error>()
                    .map(mxbot_common::send::classify_error)
                    .filter(|(retryable, _)| *retryable);
                Err(match limited {
                    Some((_, hint)) => Failure::RateLimited(hint),
                    None => Failure::Failed(error.to_string()),
                })
            }
        }
    }

    fn pause(&self, how_long: std::time::Duration) -> impl std::future::Future<Output = ()> + Send {
        tokio::time::sleep(how_long)
    }
}

/// Someone joined an authorized room: if a check-in meant for them is
/// open and they have no chat yet, open one (they get its notice when they
/// accept the invitation).
pub async fn on_authorized_join(kk: &Kk, room: &Room, ev: &OriginalSyncRoomMemberEvent) {
    if ev.content.membership != MembershipState::Join
        || !kk
            .authorized_rooms()
            .await
            .iter()
            .any(|r| r == room.room_id())
    {
        return;
    }
    let targets = kk
        .engine
        .db
        .run(|c| {
            let mut targets = Vec::new();
            for id in checkins::announced_open_runs(c, Utc::now())? {
                targets.push(checkins::run(c, id)?.target);
            }
            Ok(targets)
        })
        .await
        .unwrap_or_default();
    let hs = Matrix {
        kk,
        only: Some(ev.state_key.clone()),
    };
    for target in targets {
        match reconcile::reconcile(&kk.engine.db, &hs, &target).await {
            // One chat is enough: the first target that includes them made it.
            Ok(out) if out.eligible > 0 => break,
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "Could not reconcile a new member"),
        }
    }
}

/// Send `run`'s notice or reminder into `who`'s chat `room` if they should
/// hear about it now (open, theirs, unanswered; chat still private).
/// Returns whether it went out.
/// What became of one message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Told {
    Sent,
    /// Not needed (any more): answered, closed, no chat, not private.
    NotNeeded,
    /// Sending failed; it may work later.
    Failed,
}

async fn tell(kk: &Kk, room: &Room, who: &Who, run_id: i64, which: CheckinText) -> bool {
    tell_txn(kk, room, who, run_id, which, &TransactionId::new()).await == Told::Sent
}

/// Tell `who` about `run` (notice or reminder) in their chat `room`, with
/// the transaction ID `txn` (the outbox's, so a retry isn't posted twice).
pub async fn tell_txn(
    kk: &Kk,
    room: &Room,
    who: &Who,
    run_id: i64,
    which: CheckinText,
    txn: &TransactionId,
) -> Told {
    let now = Utc::now();
    let Ok((run, text)) = kk.engine.checkin_text(run_id, which, now).await else {
        return Told::Failed;
    };
    let Ok(user) = OwnedUserId::try_from(who.mxid.as_str()) else {
        return Told::NotNeeded;
    };
    if !kk.engine.should_tell(&run, who, now).await.unwrap_or(false)
        || !kk.chat_is_private(room, &user).await
    {
        return Told::NotNeeded;
    }
    if kk.engine.is_survey() {
        return tell_survey(kk, room, who, run_id, which, txn).await;
    }
    let event_id = match send_with_retry_txn(room, markup::notice(&text), txn).await {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, "Could not send a check-in message");
            return Told::Failed;
        }
    };
    record(kk, room, &event_id, false).await;
    let e = event_id.to_string();
    let _ = kk
        .engine
        .db
        .run(move |c| checkins::add_message(c, &e, run_id))
        .await;
    add_buttons(
        kk,
        room,
        &event_id,
        &[screens::START_CHECKIN.to_owned()],
        false,
    )
    .await;
    Told::Sent
}

/// Survey-only mode: the notice is the check-in itself (its intro, a new
/// message, so it notifies); the reminder a note with their screen below.
/// Nothing while they answer another one: this one comes up after it.
async fn tell_survey(
    kk: &Kk,
    room: &Room,
    who: &Who,
    run_id: i64,
    which: CheckinText,
    txn: &TransactionId,
) -> Told {
    // The outbox doesn't hold `ops`; this changes the conversation.
    let _op = kk.ops.lock().await;
    let ttl = kk.settings.get().session_ttl();
    let msgs = match kk
        .engine
        .survey_message(who, room.room_id().as_str(), run_id, which, Utc::now(), ttl)
        .await
    {
        Ok(Some(msgs)) => msgs,
        Ok(None) => return Told::NotNeeded,
        Err(error) => {
            tracing::warn!(%error, "Could not prepare a check-in message");
            return Told::Failed;
        }
    };
    let sent = deliver_txn(kk, room, &who.mxid, msgs, Some(txn)).await;
    kk.wake.notify_one();
    if sent {
        Told::Sent
    } else {
        Told::Failed
    }
}

/// Survey-only mode: someone accepted their chat. The check-in waiting for
/// them, if any (counted as its notice); else nothing at all. The caller
/// holds `ops`.
async fn join_survey(kk: &Kk, room: &Room, who: &Who) {
    let now = Utc::now();
    let msgs = match kk
        .engine
        .fresh_menu(who, room.room_id().as_str(), now)
        .await
    {
        Ok(msgs) => msgs,
        Err(error) => {
            tracing::error!(%error, "Could not open a check-in for a new chat");
            return;
        }
    };
    let run = kk
        .engine
        .db
        .run({
            let mxid = who.mxid.clone();
            move |c| crate::engine::session_run(c, &mxid)
        })
        .await
        .ok()
        .flatten();
    let shown = !msgs.is_empty();
    deliver(kk, room, &who.mxid, msgs).await;
    if let (true, Some(run_id)) = (shown, run) {
        let _ = kk
            .engine
            .db
            .run(move |c| checkins::count_batch(c, run_id, Batch::Notice, 0, 1))
            .await;
    }
    kk.wake.notify_one();
}

/// Survey-only mode, at startup: what the full Kummerkasten left in the
/// chats (menus, drafts, notes) is tidied away, and buttons under a
/// deletion-code receipt are taken off (the receipt stays: it holds their
/// code). Old 📊 notices keep working: they open their check-in.
pub async fn survey_cleanup(kk: &Kk) {
    let ttl = kk.settings.get().session_ttl();
    let receipts = match kk.engine.survey_cleanup(Utc::now(), ttl).await {
        Ok(receipts) => receipts,
        Err(error) => {
            tracing::error!(%error, "Survey-only cleanup failed");
            return;
        }
    };
    for (room_id, receipt) in receipts {
        let (Some(room), Ok(receipt)) = (
            OwnedRoomId::try_from(room_id.as_str())
                .ok()
                .and_then(|id| kk.client.get_room(&id)),
            OwnedEventId::try_from(receipt.as_str()),
        ) else {
            continue;
        };
        // Also buttons the ledger never knew (from before it tracked them).
        if let Ok(buttons) = server_buttons(kk, &room, &receipt).await {
            for (_, reaction) in buttons {
                if let Ok(reaction) = OwnedEventId::try_from(reaction.as_str()) {
                    let _ = room.redact(&reaction, None, None).await;
                }
            }
        }
    }
    kk.wake.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxbot_common::matrix_sdk::ruma::api::client::discovery::get_capabilities::v3::{
        RoomVersionStability, RoomVersionsCapability,
    };
    use mxbot_common::private::choose_room_version;

    fn member(user: &str, membership: &str) -> OriginalSyncRoomMemberEvent {
        serde_json::from_value(serde_json::json!({
            "type": "m.room.member",
            "event_id": "$e:example.org",
            "sender": user,
            "state_key": user,
            "origin_server_ts": 1,
            "content": {"membership": membership},
        }))
        .unwrap()
    }

    #[test]
    fn only_a_join_from_another_server_waits() {
        let bot = UserId::parse("@kummerkasten-bot:scharni.org").unwrap();
        assert!(waits_after_join(
            &bot,
            &member("@schnick:matrix.org", "join")
        ));
        assert!(!waits_after_join(
            &bot,
            &member("@nick:scharni.org", "join")
        ));
        assert!(!waits_after_join(
            &bot,
            &member("@schnick:matrix.org", "leave")
        ));
        assert!(!waits_after_join(
            &bot,
            &member("@schnick:matrix.org", "invite")
        ));
    }

    fn server(stable: &[&str], unstable: &[&str]) -> RoomVersionsCapability {
        let mut available = std::collections::BTreeMap::new();
        for v in stable {
            available.insert(
                RoomVersionId::try_from(*v).unwrap(),
                RoomVersionStability::Stable,
            );
        }
        for v in unstable {
            available.insert(
                RoomVersionId::try_from(*v).unwrap(),
                RoomVersionStability::Unstable,
            );
        }
        RoomVersionsCapability::new(RoomVersionId::V10, available)
    }

    #[test]
    fn room_versions_follow_our_policy_not_numbers() {
        let pick = |cap| choose_room_version(&cap, &PREFERRED_SUPPORTED_ROOM_VERSIONS);
        assert_eq!(
            pick(server(&["10", "11", "12"], &[])),
            Some(RoomVersionId::V12)
        );
        // A version we don't support yet is never chosen, however "new".
        assert_eq!(
            pick(server(&["11", "12", "13"], &[])),
            Some(RoomVersionId::V12)
        );
        // Unstable doesn't count.
        assert_eq!(
            pick(server(&["10", "11"], &["12"])),
            Some(RoomVersionId::V11)
        );
        // Nothing in common: the server's default (None).
        assert_eq!(pick(server(&["9"], &[])), None);
    }
}
