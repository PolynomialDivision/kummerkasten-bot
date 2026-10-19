//! Who may use the Kummerkasten: whoever is joined to one of the authorized
//! rooms *right now*, as the homeserver says: asked again before every
//! action, never remembered. When the server can't answer, nothing is
//! allowed; callers tell that apart from "not joined" so a hiccup doesn't
//! lock anyone out for good.

use std::future::Future;

use anyhow::Result;
use mxbot_common::matrix_sdk::{
    ruma::{OwnedRoomId, RoomId, UserId},
    Client,
};

/// Membership as the server sees it (a seam for tests).
pub trait Membership: Sync {
    fn is_joined(&self, room: &RoomId, user: &UserId) -> impl Future<Output = Result<bool>> + Send;
}

impl Membership for Client {
    fn is_joined(&self, room: &RoomId, user: &UserId) -> impl Future<Output = Result<bool>> + Send {
        mxbot_common::private::is_joined_now(self, room, user)
    }
}

/// Joined to at least one of `rooms` right now.
#[cfg(test)]
pub async fn authorized<M: Membership>(
    membership: &M,
    rooms: &[OwnedRoomId],
    user: &UserId,
) -> bool {
    joined_rooms(membership, rooms, user)
        .await
        .is_ok_and(|joined| !joined.is_empty())
}

/// Which of `rooms` `user` is joined to right now (for targets like "the
/// members of Kitchen 2"). Empty: no access. `Err`: the server couldn't say
/// for one of them, so the list may be short (and a target missed).
pub async fn joined_rooms<M: Membership>(
    membership: &M,
    rooms: &[OwnedRoomId],
    user: &UserId,
) -> Result<Vec<String>> {
    let mut joined = Vec::new();
    for room in rooms {
        match membership.is_joined(room, user).await {
            Ok(true) => joined.push(room.to_string()),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(room = %room, %error, "Could not check a membership");
                anyhow::bail!("the server could not say whether {user} is in {room}");
            }
        }
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxbot_common::matrix_sdk::ruma::{room_id, user_id, OwnedUserId};
    use std::{collections::HashSet, sync::Mutex};

    #[derive(Default)]
    struct Server {
        joined: Mutex<HashSet<(OwnedRoomId, OwnedUserId)>>,
        broken: Mutex<HashSet<OwnedRoomId>>,
    }

    impl Membership for Server {
        fn is_joined(
            &self,
            room: &RoomId,
            user: &UserId,
        ) -> impl Future<Output = Result<bool>> + Send {
            let broken = self.broken.lock().unwrap().contains(room);
            let joined = self
                .joined
                .lock()
                .unwrap()
                .contains(&(room.to_owned(), user.to_owned()));
            async move {
                if broken {
                    anyhow::bail!("server says no")
                }
                Ok(joined)
            }
        }
    }

    #[tokio::test]
    async fn membership_is_checked_every_time_and_errors_lock() {
        let server = Server::default();
        let house = room_id!("!house:x").to_owned();
        let floor = room_id!("!floor2:x").to_owned();
        let rooms = vec![house.clone(), floor.clone()];
        let alice = user_id!("@alice:x");
        assert!(!authorized(&server, &rooms, alice).await);
        server
            .joined
            .lock()
            .unwrap()
            .insert((floor.clone(), alice.to_owned()));
        // A member of any authorized room may use it.
        assert!(authorized(&server, &rooms, alice).await);
        assert_eq!(
            joined_rooms(&server, &rooms, alice).await.unwrap(),
            [floor.to_string()]
        );
        // A room that isn't authorized doesn't count.
        assert!(!authorized(&server, std::slice::from_ref(&house), alice).await);
        // Leaving locks it again, at the very next check.
        server.joined.lock().unwrap().clear();
        assert!(!authorized(&server, &rooms, alice).await);
        // If the server can't answer, the answer is no.
        server
            .joined
            .lock()
            .unwrap()
            .insert((floor.clone(), alice.to_owned()));
        server.broken.lock().unwrap().insert(floor);
        assert!(!authorized(&server, &rooms, alice).await);
        // ... and the caller can tell it from "not joined".
        assert!(joined_rooms(&server, &rooms, alice).await.is_err());
    }
}
