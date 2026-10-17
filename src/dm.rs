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
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::{
                reaction::{OriginalSyncReactionEvent, ReactionEventContent},
                relation::Annotation,
                room::{
                    member::{MembershipState, OriginalSyncRoomMemberEvent},
                    message::{
                        MessageType, OriginalSyncRoomMessageEvent, Relation, ReplacementMetadata,
                    },
                },
            },
            OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId, TransactionId, UserId,
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
        // Test mode: nobody but the admins, whichever room they are in.
        if !self.may_use(user.as_str()) {
            return None;
        }
        let rooms = self.authorized_rooms().await;
        let joined = access::joined_rooms(&self.client, &rooms, user).await;
        (!joined.is_empty()).then(|| Who {
            mxid: user.to_string(),
            rooms: joined,
        })
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
    for msg in msgs {
        if msg.live && msg.kind != Kind::Receipt {
            if !show_screen(kk, room, mxid, &msg).await {
                return;
            }
            continue;
        }
        let event_id = match send_with_retry(room, markup::notice(&msg.text)).await {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(%error, "Could not send to a Kummerkasten chat");
                return;
            }
        };
        record(kk, room, &event_id, msg.in_session()).await;
        if msg.live {
            // The code receipt: its buttons work, it isn't edited.
            if let Err(error) = kk.engine.set_screen(mxid, event_id.as_str()).await {
                tracing::error!(%error, "Could not remember the live screen");
            }
            seed(room, &event_id, &msg.buttons).await;
        }
    }
}

/// Show `msg` as the chat's screen: edit the current one, or send a fresh
/// one. `false`: nothing could be sent.
async fn show_screen(kk: &Kk, room: &Room, mxid: &str, msg: &Msg) -> bool {
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
        let editable = kk
            .engine
            .db
            .run(move |c| store::screen_is_editable(c, &r, &e))
            .await
            .unwrap_or(false);
        if editable {
            match edit_screen(kk, room, event, msg).await {
                Ok(()) => return true,
                // Gone, or the server refused: a fresh screen instead, so
                // nobody is left with a dead one.
                Err(error) => {
                    tracing::warn!(%error, "Could not edit the screen; sending a fresh one")
                }
            }
        }
    }
    let event_id = match send_with_retry(room, markup::notice(&msg.text)).await {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, "Could not send to a Kummerkasten chat");
            return false;
        }
    };
    record(kk, room, &event_id, true).await;
    if let Err(error) = kk.engine.set_screen(mxid, event_id.as_str()).await {
        tracing::error!(%error, "Could not remember the live screen");
    }
    let buttons = add_buttons(room, &event_id, &msg.buttons).await;
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
/// and they keep the order of the text.
async fn edit_screen(kk: &Kk, room: &Room, event: &str, msg: &Msg) -> Result<()> {
    let original = OwnedEventId::try_from(event).context("not an event ID")?;
    let edit = markup::notice(&msg.text)
        .make_replacement(ReplacementMetadata::new(original.clone(), None));
    let edit_id = send_with_retry(room, edit).await?;
    let room_id = room.room_id().to_string();
    let old = {
        let (r, e, x) = (room_id.clone(), event.to_owned(), edit_id.to_string());
        kk.engine
            .db
            .run(move |c| {
                store::record_edit(c, &r, &x, &e)?;
                Ok(store::screen_buttons(c, &r, &e)?.unwrap_or_default())
            })
            .await?
    };
    let keep = screens::buttons_kept(&old, &msg.buttons);
    for (_, reaction) in &old[keep..] {
        if let Ok(reaction) = OwnedEventId::try_from(reaction.as_str()) {
            if let Err(error) = room.redact(&reaction, None, None).await {
                tracing::debug!(%error, "Could not take a button away");
            }
        }
    }
    let mut buttons = old[..keep].to_vec();
    buttons.extend(add_buttons(room, &original, &msg.buttons[keep..]).await);
    let (r, e) = (room_id, event.to_owned());
    kk.engine
        .db
        .run(move |c| store::set_screen_buttons(c, &r, &e, &buttons))
        .await?;
    Ok(())
}

/// The bot's reactions under `event_id`, one per key: `(key, reaction)` of
/// those that could be added.
async fn add_buttons(
    room: &Room,
    event_id: &OwnedEventId,
    keys: &[String],
) -> Vec<(String, String)> {
    let mut added = Vec::new();
    for key in keys {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), key.clone()));
        match send_with_retry(room, reaction).await {
            Ok(id) => added.push((key.clone(), id.to_string())),
            Err(error) => tracing::warn!(%error, "Could not add a button"),
        }
    }
    added
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

async fn seed(room: &Room, event_id: &OwnedEventId, keys: &[String]) {
    for key in keys {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), key.clone()));
        if let Err(error) = room.send(reaction).await {
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
    let Some(who) = kk.who(&ev.sender).await else {
        lock(kk, room, &resident.mxid).await;
        return;
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
        kk.engine
            .fresh_menu(&who, room.room_id().as_str(), now)
            .await
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
        Err(error) => tracing::error!(%error, "A Kummerkasten message failed"),
    }
    kk.wake.notify_one();
}

/// A tap in a resident's chat: counted only on their live screen.
/// A tap in a resident's chat. Unlike messages, reactions needn't arrive
/// encrypted: Element Web and FluffyChat send them in the clear even in
/// encrypted rooms (an emoji and the event it points at: public anyway).
/// They carry no text; the chat itself is still checked to be private and
/// encrypted before anything happens.
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
    let Some(who) = kk.who(&ev.sender).await else {
        lock(kk, room, &resident.mxid).await;
        return;
    };
    consume(kk, room, &ev.event_id).await;
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
    match result {
        Ok(msgs) => deliver(kk, room, &resident.mxid, msgs).await,
        Err(error) => tracing::error!(%error, "A Kummerkasten tap failed"),
    }
    kk.wake.notify_one();
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
            let Some(who) = kk.who(&ev.state_key).await else {
                lock(kk, room, &mxid).await;
                return;
            };
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
}

/// Open (or bring up) `user`'s Kummerkasten chat.
pub async fn open_chat(kk: &Kk, user: &UserId) -> Result<Opened> {
    open_chat_at(kk, user, None).await
}

/// Open (or bring up) `user`'s chat, at `at` if they have one already.
pub async fn open_chat_at(kk: &Kk, user: &UserId, at: Option<MenuItem>) -> Result<Opened> {
    let Some(who) = kk.who(user).await else {
        return Ok(Opened::Locked);
    };
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
        Ok(Opened::Locked) => {}
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
                Tap 📮 and I'll open a private chat with you.\n\n\
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
    seed(room, &event_id, &[screens::START_CHECKIN.to_owned()]).await;
    Told::Sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxbot_common::matrix_sdk::ruma::api::client::discovery::get_capabilities::v3::{
        RoomVersionStability, RoomVersionsCapability,
    };
    use mxbot_common::private::choose_room_version;

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
