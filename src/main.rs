//! 📮 kummerkasten-bot: feedback, check-ins and ideas for the house, in
//! private Matrix chats. See README.md.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use mxbot_common::{
    admin::Dispatch,
    matrix_sdk::{
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::{
                reaction::OriginalSyncReactionEvent,
                room::{
                    member::OriginalSyncRoomMemberEvent,
                    message::{MessageType, OriginalSyncRoomMessageEvent},
                },
            },
            OwnedEventId, OwnedRoomId,
        },
        Room, RoomState,
    },
    rooms::DynamicRooms,
    send::send_with_retry,
    settings::Settings,
    Bot,
};
use tokio::sync::{Mutex, Notify};

mod access;
mod admin;
mod admin_checkins;
mod admin_results;
mod catalog;
mod checkins;
mod config;
mod db;
mod dm;
mod engine;
mod flow;
mod ideas;
mod lifecycle;
mod markup;
mod measures;
mod outbox;
mod plenum;
mod plenums;
mod reconcile;
mod results;
mod run_results;
mod scheduler;
mod screens;
mod secrets;
mod stats;
mod store;
mod submissions;
mod summary;
mod survey_file;
mod surveys;
mod time;

/// Where a message goes.
#[derive(Debug, PartialEq)]
enum Route {
    /// `!kk …` from an admin in their verified direct chat.
    Admin,
    /// Anything else: a resident's chat or an authorized room.
    Normal,
}

fn route(dispatch: Dispatch, body: &str) -> Option<Route> {
    match dispatch {
        Dispatch::Handled => None,
        Dispatch::AdminDm if is_admin_command(body) => Some(Route::Admin),
        Dispatch::AdminDm | Dispatch::Continue => Some(Route::Normal),
    }
}

fn is_admin_command(body: &str) -> bool {
    let first = body.split_whitespace().next().unwrap_or("");
    first.eq_ignore_ascii_case("!kk")
}

#[tokio::main]
async fn main() -> Result<()> {
    let config: config::Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
    mxbot_common::logging::init("kummerkasten_bot");
    config::check_security(&config.security)?;
    config::check_kummerkasten(&config.kummerkasten, &config.security)?;
    let zone = time::Zone::parse(&config.kummerkasten.timezone)?;
    let mode = config.kummerkasten.mode;
    let test = mode == config::Mode::Test;
    let survey = config.kummerkasten.ui == config::Ui::Survey;
    if survey {
        tracing::info!(
            "Survey-only mode: residents see only the check-ins admins start (ui = \"full\" for everything)"
        );
    }
    if test {
        tracing::warn!(
            admins = ?config.security.admin_users,
            "Test mode: own database, only admins have access, check-ins go only to who starts them"
        );
    }
    markup::set_test_mode(test);

    // The Matrix session (crypto store) is the same in both modes; the
    // Kummerkasten's own files are not: test mode never touches live data.
    let store_path = mxbot_common::config::store_path_from_env();
    std::fs::create_dir_all(&store_path)
        .with_context(|| format!("Creating {}", store_path.display()))?;
    let [db_file, key_file, settings_file] = mode.files();
    let db = db::Db::open(&store_path.join(db_file)).await?;
    let keyed = db.run(|c| store::holds_keyed_data(c)).await?;
    let secrets = secrets::Secrets::load_or_create(&store_path.join(key_file), !keyed)?;
    let settings = Settings::load(
        store_path.join(settings_file),
        config.kummerkasten.settings.clone(),
    )
    .await?;
    if let Some(rooms) = config.kummerkasten.rooms.clone() {
        let count = rooms.len();
        let changed = db.run(move |c| store::sync_rooms(c, &rooms)).await?;
        tracing::info!(rooms = count, changed, "Authorized rooms from the config");
    }

    // First start: the house's rhythm as the starting rule. Then plenums
    // into view and a round open.
    let plenum_time = time::parse_hm(&config.kummerkasten.plenum_time)?;
    db.run(move |conn| {
        let now = Utc::now();
        let rule = plenum::Recurrence::default_rule(plenum_time);
        if plenums::ensure_rule(conn, &rule, zone.date(now))? {
            tracing::info!(rule = %rule.describe(), "Stored the starting plenum rhythm");
        }
        plenums::materialize(conn, zone, now)?;
        plenums::ensure_round(conn, now)?;
        Ok(())
    })
    .await?;

    let authorized = db.run(|c| store::authorized_rooms(c)).await?;
    let rooms = DynamicRooms::new(
        authorized
            .iter()
            .filter_map(|(id, _)| OwnedRoomId::try_from(id.as_str()).ok()),
    );
    // Residents' chats are created by the bot, never joined on invitation:
    // `[security]` lets only admins invite it, into authorized rooms.
    let bot = Bot::builder("kummerkasten-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .settings(settings.store().clone())
        .dynamic_rooms(rooms.clone())
        .admin_help(admin::SHORT_HELP)
        .start(&config.matrix, &config.security)
        .await?;

    let kk = dm::Kk {
        client: bot.client.clone(),
        bot: bot.user_id.clone(),
        engine: engine::Engine::new(db, secrets, zone)
            .with_threshold({
                let settings = settings.clone();
                Arc::new(move || settings.get().privacy_threshold() as i64)
            })
            .survey_only(survey),
        settings,
        rooms,
        ops: Arc::new(Mutex::new(())),
        wake: Arc::new(Notify::new()),
        pending_starts: Arc::default(),
        test_users: test.then(|| Arc::new(config.security.admin_users.clone())),
        managed_rooms: config.kummerkasten.rooms.clone().map(Arc::new),
    };

    bot.client.add_event_handler({
        let kk = kk.clone();
        let bot = bot.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| {
            let kk = kk.clone();
            let bot = bot.clone();
            async move {
                if room.state() != RoomState::Joined {
                    return;
                }
                if ev.sender == kk.bot {
                    dm::on_own_message(&kk, &room, &ev).await;
                    return;
                }
                // A survey file an admin uploads in their chat with the bot.
                if let MessageType::File(file) = &ev.content.msgtype {
                    if bot.admin.is_console(&room, &ev.sender, encryption.as_ref()).await {
                        let reply = admin_checkins::upload(&kk, &ev.sender, file).await;
                        answer_admin(&kk, &room, &ev.event_id, &reply).await;
                        return;
                    }
                }
                let dispatch = bot.admin.handle(&room, &ev, encryption.as_ref()).await;
                let body = match &ev.content.msgtype {
                    MessageType::Text(text) => text.body.trim().to_owned(),
                    _ => String::new(),
                };
                match route(dispatch, &body) {
                    None => {}
                    Some(Route::Admin) => {
                        let reply = match admin_checkins::export_request(&body) {
                            Some(key) => send_survey_file(&kk, &room, &key).await,
                            None => admin::run(&kk, &ev.sender, &body).await,
                        };
                        answer_admin(&kk, &room, &ev.event_id, &reply).await;
                    }
                    Some(Route::Normal) => {
                        if kk.is_chat(&room).await {
                            if is_admin_command(&body) {
                                // Not an admin (or not their verified chat).
                                dm::track(&kk, &room, &ev.event_id, true).await;
                                if let Ok(answer) = send_with_retry(
                                    &room,
                                    markup::notice("🔒 `!kk` is for admins, in their verified direct chat with me."),
                                )
                                .await
                                {
                                    dm::track(&kk, &room, &answer, true).await;
                                }
                                return;
                            }
                            dm::on_message(&kk, &room, &ev, encryption.as_ref()).await;
                        } else {
                            dm::on_room_message(&kk, &room, &ev).await;
                        }
                    }
                }
            }
        }
    });

    bot.client.add_event_handler({
        let kk = kk.clone();
        move |ev: OriginalSyncReactionEvent, room: Room| {
            let kk = kk.clone();
            async move {
                if ev.sender == kk.bot || room.state() != RoomState::Joined {
                    return;
                }
                if kk.is_chat(&room).await {
                    dm::on_reaction(&kk, &room, &ev).await;
                } else {
                    dm::on_entry_reaction(&kk, &room, &ev).await;
                }
            }
        }
    });

    bot.client.add_event_handler({
        let kk = kk.clone();
        move |ev: OriginalSyncRoomMemberEvent, room: Room| {
            let kk = kk.clone();
            async move {
                if ev.state_key == kk.bot {
                    return;
                }
                if kk.is_chat(&room).await {
                    if dm::waits_after_join(&kk.bot, &ev) {
                        // Not in the sync loop: the other events go on meanwhile.
                        tokio::spawn(async move {
                            tokio::time::sleep(dm::FEDERATED_JOIN_GRACE).await;
                            dm::on_member(&kk, &room, &ev).await;
                        });
                    } else {
                        dm::on_member(&kk, &room, &ev).await;
                    }
                } else {
                    dm::on_authorized_join(&kk, &room, &ev).await;
                }
            }
        }
    });

    bot.initial_sync().await;
    tracing::info!("Initial sync complete");
    // Survey-only mode: menus and drafts of the full Kummerkasten go.
    if survey {
        dm::survey_cleanup(&kk).await;
    }
    // Screens a crash left behind, before anyone taps on them.
    dm::resume_screens(&kk).await;
    tokio::spawn(scheduler::run(kk.clone()));
    bot.run().await
}

/// Answer an admin's command (or upload) where it came from. In their
/// Kummerkasten chat the command and the answer are messages after the
/// screen (it isn't edited above them), cleared with the round.
async fn answer_admin(kk: &dm::Kk, room: &Room, asked: &OwnedEventId, reply: &str) {
    match send_with_retry(room, markup::notice(reply)).await {
        Ok(answer) if kk.is_chat(room).await => {
            dm::track(kk, room, asked, false).await;
            dm::track(kk, room, &answer, false).await;
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "Could not answer an admin"),
    }
}

/// `!kk survey <key> export`: the template as a .toml attachment. Returns
/// the answer to go with it.
async fn send_survey_file(kk: &dm::Kk, room: &Room, key: &str) -> String {
    let (name, toml) = match admin_checkins::export_file(kk, key).await {
        Ok(file) => file,
        Err(error) => return format!("😕 {error:#}"),
    };
    let sent = room
        .send_attachment(
            name.as_str(),
            &mime::TEXT_PLAIN_UTF_8,
            toml.into_bytes(),
            mxbot_common::matrix_sdk::attachment::AttachmentConfig::new(),
        )
        .await;
    match sent {
        Ok(response) => {
            if kk.is_chat(room).await {
                dm::track(kk, room, &response.event_id, false).await;
            }
            format!(
                "📎 Here is **{name}**.\n✏️ Change it and upload it again: it replaces the survey while it was never started."
            )
        }
        Err(error) => {
            tracing::warn!(%error, "Could not send a survey file");
            "😕 I couldn't send the file. Please try again.".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_admins_in_their_verified_chat_reach_admin_commands() {
        // The console said: an admin, in their verified, encrypted chat.
        assert_eq!(
            route(Dispatch::AdminDm, "!kk rooms add !r:x"),
            Some(Route::Admin)
        );
        // Anyone else: a resident in their chat, anyone in a room, is
        // routed as a normal message (and told `!kk` is for admins).
        assert_eq!(
            route(Dispatch::Continue, "!kk rooms add !r:x"),
            Some(Route::Normal)
        );
        assert_eq!(route(Dispatch::AdminDm, "!menu"), Some(Route::Normal));
        assert_eq!(route(Dispatch::Handled, "!admin status"), None);
        assert!(!is_admin_command("!kkk"));
    }
}
