//! 📮 One private Kummerkasten chat per eligible resident.
//!
//! Before a targeted check-in goes out (and on `!kk invite-all`, and when
//! someone joins an authorized room while a check-in is open), the bot:
//!
//! 1. lists who is in the authorized rooms, and asks the server again, for
//!    each, which of them they are in *now* (no access: nothing happens);
//! 2. keeps those the check-in is for;
//! 3. reuses their chat if they have one (joined, or invited and not
//!    joined yet, never a second one);
//! 4. otherwise creates the encrypted private chat and invites them,
//!    waiting out the homeserver's rate limit when it says so.
//!
//! Idempotent: a second pass creates nothing new. Someone who hasn't
//! accepted the invitation yet is not counted as told; they get the notice
//! of every open check-in for them when they join.

use std::{future::Future, time::Duration};

use anyhow::Result;

use crate::{checkins::Target, db::Db, store};

/// What the homeserver is asked (a seam for tests).
pub trait Homeserver: Sync {
    /// Everyone joined to an authorized room (per the room member lists).
    fn members(&self) -> impl Future<Output = Result<Vec<String>>> + Send;
    /// The authorized rooms `user` is joined to right now, asked fresh.
    fn rooms_of(&self, user: &str) -> impl Future<Output = Vec<String>> + Send;
    /// Create the private, encrypted chat with `user` and invite them.
    fn create_chat(&self, user: &str) -> impl Future<Output = Result<String, Failure>> + Send;
    /// Wait (between creations; when rate-limited).
    fn pause(&self, how_long: Duration) -> impl Future<Output = ()> + Send;
}

#[derive(Debug)]
pub enum Failure {
    /// Slow down: the server says so (with its hint, if any).
    RateLimited(Option<Duration>),
    Failed(String),
}

/// Pause between two new chats, to stay friendly with the homeserver.
pub const PACE: Duration = Duration::from_millis(500);
/// Tries per chat while rate-limited.
const ATTEMPTS: u32 = 4;
const FIRST_BACKOFF: Duration = Duration::from_secs(2);
const MAX_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Eligible for the target (in an authorized room now, and in it).
    pub eligible: usize,
    /// …with a chat they have joined: these can be told now.
    pub ready: Vec<(String, String)>,
    /// …invited earlier and not joined yet (no second invitation).
    pub pending: usize,
    /// New chats created (they are invited).
    pub created: usize,
    pub failed: usize,
}

/// Make sure every eligible resident in `target` has a Kummerkasten chat.
pub async fn reconcile<H: Homeserver>(db: &Db, hs: &H, target: &Target) -> Result<Outcome> {
    let mut out = Outcome::default();
    let mut members = hs.members().await?;
    members.sort();
    members.dedup();
    for user in members {
        let rooms = hs.rooms_of(&user).await;
        if rooms.is_empty() {
            continue;
        }
        let (resident, floor) = {
            let u = user.clone();
            db.run(move |c| {
                let resident = store::resident_by_mxid(c, &u)?;
                let floor: Option<i64> = c
                    .query_row(
                        "SELECT floor_id FROM residents WHERE mxid = ?1",
                        [&u],
                        |r| r.get(0),
                    )
                    .unwrap_or(None);
                Ok((resident, floor))
            })
            .await?
        };
        if !target.matches(&rooms, floor) {
            continue;
        }
        out.eligible += 1;
        match resident.and_then(|r| r.dm_room_id.map(|room| (room, r.dm_joined))) {
            Some((room, true)) => out.ready.push((user, room)),
            Some((_, false)) => out.pending += 1,
            None => match create(hs, &user).await {
                Some(room) => {
                    let (u, r) = (user.clone(), room);
                    db.run(move |c| store::set_dm(c, &u, &r)).await?;
                    out.created += 1;
                    hs.pause(PACE).await;
                }
                None => out.failed += 1,
            },
        }
    }
    tracing::info!(
        eligible = out.eligible,
        ready = out.ready.len(),
        pending = out.pending,
        created = out.created,
        failed = out.failed,
        "Kummerkasten chats reconciled"
    );
    Ok(out)
}

/// Create one chat, waiting out rate limits a few times.
async fn create<H: Homeserver>(hs: &H, user: &str) -> Option<String> {
    let mut backoff = FIRST_BACKOFF;
    for attempt in 1..=ATTEMPTS {
        match hs.create_chat(user).await {
            Ok(room) => return Some(room),
            Err(Failure::RateLimited(hint)) if attempt < ATTEMPTS => {
                let wait = hint.unwrap_or(backoff).min(MAX_WAIT);
                tracing::info!(?wait, "Rate-limited creating a chat; waiting");
                hs.pause(wait).await;
                backoff = (backoff * 2).min(MAX_WAIT);
            }
            Err(Failure::RateLimited(_)) => {
                tracing::warn!("Still rate-limited creating a chat; trying next time");
                return None;
            }
            Err(Failure::Failed(error)) => {
                tracing::warn!(%error, "Could not create a Kummerkasten chat");
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plenums::tests::{at, with_default_rule};
    use std::{
        collections::{HashMap, HashSet},
        sync::Mutex,
    };

    #[derive(Default)]
    struct Server {
        /// user → authorized rooms they're in.
        rooms: Mutex<HashMap<String, Vec<String>>>,
        /// Rate-limited this many times before creating works.
        limited: Mutex<u32>,
        broken: Mutex<HashSet<String>>,
        created: Mutex<Vec<String>>,
        paused: Mutex<Vec<Duration>>,
    }

    impl Server {
        fn member(&self, user: &str, rooms: &[&str]) {
            self.rooms.lock().unwrap().insert(
                user.to_owned(),
                rooms.iter().map(|r| r.to_string()).collect(),
            );
        }
    }

    impl Homeserver for Server {
        fn members(&self) -> impl Future<Output = Result<Vec<String>>> + Send {
            let all: Vec<String> = self.rooms.lock().unwrap().keys().cloned().collect();
            async move { Ok(all) }
        }
        fn rooms_of(&self, user: &str) -> impl Future<Output = Vec<String>> + Send {
            let rooms = self
                .rooms
                .lock()
                .unwrap()
                .get(user)
                .cloned()
                .unwrap_or_default();
            async move { rooms }
        }
        fn create_chat(&self, user: &str) -> impl Future<Output = Result<String, Failure>> + Send {
            let result = {
                let mut limited = self.limited.lock().unwrap();
                if self.broken.lock().unwrap().contains(user) {
                    Err(Failure::Failed("no".into()))
                } else if *limited > 0 {
                    *limited -= 1;
                    Err(Failure::RateLimited(Some(Duration::from_secs(3))))
                } else {
                    let mut created = self.created.lock().unwrap();
                    created.push(user.to_owned());
                    Ok(format!("!chat{}:x", created.len()))
                }
            };
            async move { result }
        }
        fn pause(&self, how_long: Duration) -> impl Future<Output = ()> + Send {
            self.paused.lock().unwrap().push(how_long);
            async {}
        }
    }

    fn db() -> Db {
        Db::from_connection(with_default_rule(at(2026, 11, 2, 12)))
    }

    fn chat_of(db: &Db, user: &str) -> Option<String> {
        let u = user.to_owned();
        db.with(move |c| Ok(store::resident_by_mxid(c, &u)?.and_then(|r| r.dm_room_id)))
            .unwrap()
    }

    #[tokio::test]
    async fn eligible_residents_get_one_chat_each_and_nobody_else_any() {
        let db = db();
        let hs = Server::default();
        hs.member("@new:x", &["!house:x"]);
        hs.member("@has:x", &["!house:x"]);
        hs.member("@invited:x", &["!house:x"]);
        // Listed as a member, but the fresh check says no room any more.
        hs.member("@gone:x", &[]);
        db.with(|c| {
            store::set_dm(c, "@has:x", "!old:x")?;
            store::set_joined(c, "@has:x")?;
            store::set_dm(c, "@invited:x", "!waiting:x")?;
            Ok(())
        })
        .unwrap();
        let out = reconcile(&db, &hs, &Target::House).await.unwrap();
        assert_eq!(out.eligible, 3);
        // The existing chat is reused; the pending invitation not repeated.
        assert_eq!(out.ready, [("@has:x".to_owned(), "!old:x".to_owned())]);
        assert_eq!(out.pending, 1);
        assert_eq!(out.created, 1);
        assert_eq!(*hs.created.lock().unwrap(), ["@new:x"]);
        assert!(chat_of(&db, "@new:x").is_some());
        // No access: no chat, no row.
        assert_eq!(chat_of(&db, "@gone:x"), None);
        // Again: nothing new (idempotent), the new one is pending now.
        let again = reconcile(&db, &hs, &Target::House).await.unwrap();
        assert_eq!(again.created, 0);
        assert_eq!(again.pending, 2);
        assert_eq!(hs.created.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn only_the_target_and_membership_changes_count() {
        let db = db();
        let hs = Server::default();
        hs.member("@kitchen:x", &["!house:x", "!kitchen2:x"]);
        hs.member("@other:x", &["!house:x"]);
        let target = Target::Room {
            room_id: "!kitchen2:x".into(),
            label: "Kitchen 2".into(),
        };
        let out = reconcile(&db, &hs, &target).await.unwrap();
        assert_eq!((out.eligible, out.created), (1, 1));
        assert_eq!(chat_of(&db, "@other:x"), None);
        // @other joins Kitchen 2's room: the next pass includes them.
        hs.member("@other:x", &["!house:x", "!kitchen2:x"]);
        let out = reconcile(&db, &hs, &target).await.unwrap();
        assert_eq!((out.eligible, out.created, out.pending), (2, 1, 1));
        // @kitchen leaves every authorized room: not eligible any more.
        hs.member("@kitchen:x", &[]);
        let out = reconcile(&db, &hs, &target).await.unwrap();
        assert_eq!(out.eligible, 1);
    }

    #[tokio::test]
    async fn rate_limits_are_waited_out_and_failures_skipped() {
        let db = db();
        let hs = Server::default();
        hs.member("@a:x", &["!house:x"]);
        hs.member("@b:x", &["!house:x"]);
        *hs.limited.lock().unwrap() = 2;
        hs.broken.lock().unwrap().insert("@b:x".to_owned());
        let out = reconcile(&db, &hs, &Target::House).await.unwrap();
        assert_eq!((out.created, out.failed), (1, 1));
        // Twice the server's hint (3 s), then the pace after the creation.
        assert_eq!(
            *hs.paused.lock().unwrap(),
            [Duration::from_secs(3), Duration::from_secs(3), PACE]
        );
        // Rate-limited for good: given up after a few tries, nothing stored.
        let hs2 = Server::default();
        hs2.member("@c:x", &["!house:x"]);
        *hs2.limited.lock().unwrap() = 100;
        let out = reconcile(&db, &hs2, &Target::House).await.unwrap();
        assert_eq!((out.created, out.failed), (0, 1));
        assert_eq!(chat_of(&db, "@c:x"), None);
        assert_eq!(hs2.paused.lock().unwrap().len(), (ATTEMPTS - 1) as usize);
    }
}
