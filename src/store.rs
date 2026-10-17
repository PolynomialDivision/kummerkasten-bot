//! Bookkeeping for the Matrix side: authorized rooms, residents' chats,
//! chat events (for precise redaction) and handled event IDs.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{plenums, time};

// ── Authorized rooms ────────────────────────────────────────────────────────

pub fn authorized_rooms(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn
        .prepare("SELECT room_id, label FROM authorized_rooms WHERE active = 1 ORDER BY label")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn add_room(conn: &Connection, room_id: &str, label: &str, actor: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO authorized_rooms (room_id, label, added_by, added_at, active)
         VALUES (?1, ?2, ?3, ?4, 1)
         ON CONFLICT (room_id) DO UPDATE SET label = ?2, added_by = ?3, added_at = ?4, active = 1",
        params![room_id, label, actor, time::now_text()],
    )?;
    Ok(())
}

pub fn remove_room(conn: &Connection, room_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE authorized_rooms SET active = 0 WHERE room_id = ?1 AND active = 1",
        [room_id],
    )? == 1)
}

/// The config's rooms (`[[kummerkasten.rooms]]`) become exactly the active
/// ones: missing ones added, labels and `central` set, the others
/// deactivated (kept, like `!kk rooms remove`). Returns what changed.
pub fn sync_rooms(conn: &Connection, rooms: &[crate::config::RoomConfig]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut changed = 0;
    for room in rooms {
        changed += tx.execute(
            "INSERT INTO authorized_rooms (room_id, label, added_by, added_at, active, central)
             VALUES (?1, ?2, 'config', ?3, 1, ?4)
             ON CONFLICT (room_id) DO UPDATE SET label = ?2, active = 1, central = ?4
             WHERE label != ?2 OR active != 1 OR central != ?4",
            params![room.id, room.label, time::now_text(), room.central],
        )?;
    }
    let listed: Vec<&str> = rooms.iter().map(|r| r.id.as_str()).collect();
    let active: Vec<String> = {
        let mut stmt = tx.prepare("SELECT room_id FROM authorized_rooms WHERE active = 1")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for room in active.iter().filter(|r| !listed.contains(&r.as_str())) {
        changed += tx.execute(
            "UPDATE authorized_rooms SET active = 0, central = 0 WHERE room_id = ?1",
            [room],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Whether a 📮 entry message was ever posted into `room_id`.
pub fn entry_posted(conn: &Connection, room_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM posted_entries WHERE room_id = ?1",
            [room_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn mark_entry_posted(conn: &Connection, room_id: &str, event_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO posted_entries (room_id, event_id, posted_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (room_id) DO UPDATE SET event_id = ?2, posted_at = ?3",
        params![room_id, event_id, time::now_text()],
    )?;
    Ok(())
}

// ── Residents ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Resident {
    pub mxid: String,
    pub dm_room_id: Option<String>,
    pub dm_joined: bool,
    pub pending_notice: Option<String>,
}

fn resident(r: &rusqlite::Row) -> rusqlite::Result<Resident> {
    Ok(Resident {
        mxid: r.get(0)?,
        dm_room_id: r.get(1)?,
        dm_joined: r.get(2)?,
        pending_notice: r.get(3)?,
    })
}

const RESIDENT: &str = "SELECT mxid, dm_room_id, dm_joined, pending_notice FROM residents";

pub fn resident_by_room(conn: &Connection, room_id: &str) -> Result<Option<Resident>> {
    Ok(conn
        .query_row(
            &format!("{RESIDENT} WHERE dm_room_id = ?1"),
            [room_id],
            resident,
        )
        .optional()?)
}

pub fn resident_by_mxid(conn: &Connection, mxid: &str) -> Result<Option<Resident>> {
    Ok(conn
        .query_row(&format!("{RESIDENT} WHERE mxid = ?1"), [mxid], resident)
        .optional()?)
}

/// Residents with a notice to post (once their chat has been cleared).
pub fn pending_notices(conn: &Connection) -> Result<Vec<Resident>> {
    let mut stmt = conn.prepare(&format!(
        "{RESIDENT} r WHERE pending_notice IS NOT NULL AND dm_room_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM dm_events e
                           WHERE e.room_id = r.dm_room_id AND e.doomed = 1)"
    ))?;
    let rows = stmt.query_map([], resident)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn clear_notice(conn: &Connection, mxid: &str) -> Result<()> {
    conn.execute(
        "UPDATE residents SET pending_notice = NULL WHERE mxid = ?1",
        [mxid],
    )?;
    Ok(())
}

/// The bot created `room_id` as `mxid`'s chat (they're invited).
pub fn set_dm(conn: &Connection, mxid: &str, room_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO residents (mxid, dm_room_id, dm_joined, created_at) VALUES (?1, ?2, 0, ?3)
         ON CONFLICT (mxid) DO UPDATE SET dm_room_id = ?2, dm_joined = 0",
        params![mxid, room_id, time::now_text()],
    )?;
    Ok(())
}

pub fn set_joined(conn: &Connection, mxid: &str) -> Result<()> {
    conn.execute("UPDATE residents SET dm_joined = 1 WHERE mxid = ?1", [mxid])?;
    Ok(())
}

/// They left their chat: forget it (and what was tracked in it, the bot
/// can't redact there any more). Their floor stays for a next chat.
/// They left their chat: it is theirs no more, everything the bot tracked
/// in it is marked for redaction, and the bot leaves once that is done
/// ([`rooms_to_leave`]); leaving first would leave it all on the server.
pub fn drop_dm(conn: &Connection, mxid: &str, room_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE residents SET dm_room_id = NULL, dm_joined = 0, pending_notice = NULL
         WHERE mxid = ?1 AND dm_room_id = ?2",
        params![mxid, room_id],
    )?;
    conn.execute("DELETE FROM sessions WHERE mxid = ?1", [mxid])?;
    conn.execute(
        "UPDATE dm_events SET doomed = 1 WHERE room_id = ?1",
        [room_id],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO leaving_rooms (room_id, since) VALUES (?1, ?2)",
        params![room_id, time::now_text()],
    )?;
    Ok(())
}

/// Left chats with nothing left to redact: the bot may leave them now.
pub fn rooms_to_leave(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT room_id FROM leaving_rooms l
         WHERE NOT EXISTS (SELECT 1 FROM dm_events e WHERE e.room_id = l.room_id)
         ORDER BY since",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn left_room(conn: &Connection, room_id: &str) -> Result<()> {
    conn.execute("DELETE FROM leaving_rooms WHERE room_id = ?1", [room_id])?;
    Ok(())
}

/// `(mxid, room)` of every chat the resident has joined.
pub fn open_chats(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT mxid, dm_room_id FROM residents WHERE dm_room_id IS NOT NULL AND dm_joined = 1",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ── Chat events ─────────────────────────────────────────────────────────────

/// Track an event in a resident's chat, under the open round. `in_session`
/// ones go when the current flow ends; all go when the round is cleared.
pub fn record_event(
    conn: &Connection,
    room_id: &str,
    event_id: &str,
    in_session: bool,
) -> Result<()> {
    let round = plenums::open_round(conn)?.map(|r| r.id);
    // The day only (see migration 3): enough to order redactions.
    conn.execute(
        "INSERT OR IGNORE INTO dm_events (room_id, event_id, round_id, in_session, sent_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            room_id,
            event_id,
            round,
            in_session,
            time::date_text(chrono::Utc::now().date_naive())
        ],
    )?;
    Ok(())
}

/// Track an edit (`m.replace`) of `replaces`: it carries the new content,
/// so it goes with the screen; it is no message of its own in the timeline.
pub fn record_edit(conn: &Connection, room_id: &str, event_id: &str, replaces: &str) -> Result<()> {
    record_event(conn, room_id, event_id, true)?;
    conn.execute(
        "UPDATE dm_events SET replaces = ?3 WHERE room_id = ?1 AND event_id = ?2",
        params![room_id, event_id, replaces],
    )?;
    Ok(())
}

/// Events marked for redaction and due: `(room_id, event_id, attempts)`.
pub fn doomed(conn: &Connection, limit: usize) -> Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT room_id, event_id, attempts FROM dm_events
         WHERE doomed = 1 AND (retry_at IS NULL OR retry_at <= ?2)
         ORDER BY room_id, sent_at LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64, time::now_text()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Redacted (or nothing left to redact): no longer tracked.
pub fn forget_event(conn: &Connection, room_id: &str, event_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM dm_events WHERE room_id = ?1 AND event_id = ?2",
        params![room_id, event_id],
    )?;
    conn.execute(
        "DELETE FROM screens WHERE room_id = ?1 AND event_id = ?2",
        params![room_id, event_id],
    )?;
    Ok(())
}

/// After this many failed attempts a redaction counts as stuck: it is still
/// tried (less often), and admins see it in `!kk round`.
pub const STUCK_AFTER: i64 = 5;

/// A redaction failed: try again later, waiting longer each time (1, 2, 4
/// … minutes, at most 6 hours). Never given up while it might still work.
pub fn retry_event(
    conn: &Connection,
    room_id: &str,
    event_id: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    let attempts: i64 = conn
        .query_row(
            "SELECT attempts FROM dm_events WHERE room_id = ?1 AND event_id = ?2",
            params![room_id, event_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let wait = chrono::Duration::minutes((1i64 << attempts.clamp(0, 9)).min(360));
    conn.execute(
        "UPDATE dm_events SET attempts = attempts + 1, retry_at = ?3
         WHERE room_id = ?1 AND event_id = ?2",
        params![room_id, event_id, time::to_text(now + wait)],
    )?;
    Ok(())
}

/// Redactions that keep failing (see [`STUCK_AFTER`]).
pub fn stuck_redactions(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM dm_events WHERE doomed = 1 AND attempts >= ?1",
        [STUCK_AFTER],
        |r| r.get(0),
    )?)
}

// ── Screens ─────────────────────────────────────────────────────────────────

/// The bot's buttons under `event_id`, in order: `(key, reaction event)`.
/// `None`: not a screen the bot can edit.
pub fn screen_buttons(
    conn: &Connection,
    room_id: &str,
    event_id: &str,
) -> Result<Option<Vec<(String, String)>>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT buttons FROM screens WHERE room_id = ?1 AND event_id = ?2",
            params![room_id, event_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json.map(|j| serde_json::from_str(&j).unwrap_or_default()))
}

pub fn set_screen_buttons(
    conn: &Connection,
    room_id: &str,
    event_id: &str,
    buttons: &[(String, String)],
) -> Result<()> {
    conn.execute(
        "INSERT INTO screens (room_id, event_id, buttons) VALUES (?1, ?2, ?3)
         ON CONFLICT (room_id, event_id) DO UPDATE SET buttons = ?3",
        params![room_id, event_id, serde_json::to_string(buttons)?],
    )?;
    Ok(())
}

/// Whether the screen `event_id` can be edited in place: the bot knows its
/// buttons, it isn't going away (marked for redaction), it goes with the
/// flow (not a receipt), and nothing was said after it, so it is still the
/// last message in the chat (edits don't count; reactions aren't tracked).
pub fn screen_is_editable(conn: &Connection, room_id: &str, event_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM dm_events e JOIN screens s
                ON s.room_id = e.room_id AND s.event_id = e.event_id
             WHERE e.room_id = ?1 AND e.event_id = ?2 AND e.doomed = 0 AND e.in_session = 1
               AND NOT EXISTS (SELECT 1 FROM dm_events later
                               WHERE later.room_id = e.room_id AND later.rowid > e.rowid
                                 AND later.replaces IS NULL)",
            params![room_id, event_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A fresh screen replaces `event_id`: it goes now (with its edits), not at
/// the end of the flow. A receipt (not part of the flow) stays.
pub fn replace_screen(conn: &Connection, room_id: &str, event_id: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE dm_events SET doomed = 1
         WHERE room_id = ?1 AND in_session = 1 AND (event_id = ?2 OR replaces = ?2)",
        params![room_id, event_id],
    )?)
}

// ── Handled events ──────────────────────────────────────────────────────────

/// `true` the first time `event_id` is seen; `false` for a re-delivery.
pub fn first_time(conn: &Connection, event_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO processed_events (event_id, seen_at) VALUES (?1, ?2)",
        params![event_id, time::now_text()],
    )? == 1)
}

pub fn prune_processed(conn: &Connection, before: DateTime<Utc>) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM processed_events WHERE seen_at < ?1",
        [time::to_text(before)],
    )?)
}

// ── Entry messages ──────────────────────────────────────────────────────────

pub fn add_entry(conn: &Connection, room_id: &str, event_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO entry_messages (room_id, event_id) VALUES (?1, ?2)",
        params![room_id, event_id],
    )?;
    Ok(())
}

pub fn is_entry(conn: &Connection, room_id: &str, event_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM entry_messages WHERE room_id = ?1 AND event_id = ?2",
            params![room_id, event_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plenums::tests::{at, with_default_rule};

    #[test]
    fn the_config_rooms_become_exactly_the_active_rooms() {
        use crate::config::RoomConfig;
        let conn = with_default_rule(at(2026, 11, 2, 12));
        add_room(&conn, "!old:x", "Old", "@a:x").unwrap();
        let room = |id: &str, label: &str, central| RoomConfig {
            id: id.into(),
            label: label.into(),
            central,
            entry: false,
        };
        let rooms = [room("!h:x", "House", true), room("!t:x", "Test", false)];
        assert_eq!(sync_rooms(&conn, &rooms).unwrap(), 3);
        let active = authorized_rooms(&conn).unwrap();
        assert_eq!(
            active,
            [
                ("!h:x".to_owned(), "House".to_owned()),
                ("!t:x".to_owned(), "Test".to_owned())
            ]
        );
        assert_eq!(crate::checkins::central_rooms(&conn).unwrap(), ["!h:x"]);
        // Unchanged: nothing to do. Changed: only what changed.
        assert_eq!(sync_rooms(&conn, &rooms).unwrap(), 0);
        let moved = [room("!h:x", "House", false), room("!t:x", "Test", true)];
        assert_eq!(sync_rooms(&conn, &moved).unwrap(), 2);
        assert_eq!(crate::checkins::central_rooms(&conn).unwrap(), ["!t:x"]);
        // An empty list: no room at all.
        sync_rooms(&conn, &[]).unwrap();
        assert!(authorized_rooms(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_left_chat_is_redacted_before_the_bot_leaves() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        conn.execute(
            "INSERT INTO residents (mxid, dm_room_id, dm_joined, created_at) VALUES ('@a:x', '!a:x', 1, 'x')",
            [],
        )
        .unwrap();
        record_event(&conn, "!a:x", "$receipt", false).unwrap();
        record_event(&conn, "!a:x", "$screen", true).unwrap();
        drop_dm(&conn, "@a:x", "!a:x").unwrap();
        // Everything, receipts too, is to be redacted; nothing is forgotten.
        let mut doomed: Vec<String> = doomed(&conn, 10)
            .unwrap()
            .into_iter()
            .map(|(_, e, _)| e)
            .collect();
        doomed.sort();
        assert_eq!(doomed, ["$receipt", "$screen"]);
        // Not before both are gone.
        assert!(rooms_to_leave(&conn).unwrap().is_empty());
        forget_event(&conn, "!a:x", "$receipt").unwrap();
        assert!(rooms_to_leave(&conn).unwrap().is_empty());
        forget_event(&conn, "!a:x", "$screen").unwrap();
        assert_eq!(rooms_to_leave(&conn).unwrap(), ["!a:x"]);
        left_room(&conn, "!a:x").unwrap();
        assert!(rooms_to_leave(&conn).unwrap().is_empty());
    }

    #[test]
    fn failed_redactions_back_off_and_are_never_given_up() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        record_event(&conn, "!a:x", "$1", true).unwrap();
        crate::engine::tidy(&conn, "!a:x").unwrap();
        let now = chrono::Utc::now();
        for _ in 0..STUCK_AFTER {
            retry_event(&conn, "!a:x", "$1", now).unwrap();
        }
        // Waiting for its next attempt: not due now.
        assert!(doomed(&conn, 10).unwrap().is_empty());
        assert_eq!(stuck_redactions(&conn).unwrap(), 1);
        // Due again later, still tracked.
        conn.execute("UPDATE dm_events SET retry_at = '2000-01-01T00:00:00Z'", [])
            .unwrap();
        assert_eq!(doomed(&conn, 10).unwrap().len(), 1);
        let attempts: i64 = conn
            .query_row("SELECT attempts FROM dm_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(attempts, STUCK_AFTER);
    }

    #[test]
    fn a_screen_is_edited_only_while_it_is_the_last_message() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        let editable = |e: &str| screen_is_editable(&conn, "!a:x", e).unwrap();
        record_event(&conn, "!a:x", "$screen", true).unwrap();
        // Without known buttons (an older screen): never edited.
        assert!(!editable("$screen"));
        set_screen_buttons(&conn, "!a:x", "$screen", &[("📝".into(), "$r1".into())]).unwrap();
        assert!(editable("$screen"));
        // Its own edits don't count as later messages.
        record_edit(&conn, "!a:x", "$edit", "$screen").unwrap();
        assert!(editable("$screen"));
        assert_eq!(
            screen_buttons(&conn, "!a:x", "$screen").unwrap().unwrap(),
            [("📝".to_owned(), "$r1".to_owned())]
        );
        // Something said after it (their text, a note): not any more.
        record_event(&conn, "!a:x", "$their_text", true).unwrap();
        assert!(!editable("$screen"));
        // Replacing it dooms it and its edits, not their text.
        assert_eq!(replace_screen(&conn, "!a:x", "$screen").unwrap(), 2);
        let doomed: Vec<String> = doomed(&conn, 10)
            .unwrap()
            .into_iter()
            .map(|(_, e, _)| e)
            .collect();
        assert_eq!(doomed.len(), 2);
        assert!(!doomed.contains(&"$their_text".to_owned()));
        // A receipt is never edited or replaced.
        record_event(&conn, "!a:x", "$receipt", false).unwrap();
        set_screen_buttons(&conn, "!a:x", "$receipt", &[]).unwrap();
        assert!(!editable("$receipt"));
        assert_eq!(replace_screen(&conn, "!a:x", "$receipt").unwrap(), 0);
        // A doomed screen isn't edited either.
        record_event(&conn, "!a:x", "$s2", true).unwrap();
        set_screen_buttons(&conn, "!a:x", "$s2", &[]).unwrap();
        assert!(editable("$s2"));
        crate::engine::tidy(&conn, "!a:x").unwrap();
        assert!(!editable("$s2"));
        // Forgetting it drops its buttons.
        forget_event(&conn, "!a:x", "$s2").unwrap();
        assert!(screen_buttons(&conn, "!a:x", "$s2").unwrap().is_none());
    }

    #[test]
    fn a_posted_entry_is_remembered_per_room() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        assert!(!entry_posted(&conn, "!h:x").unwrap());
        mark_entry_posted(&conn, "!h:x", "$1").unwrap();
        mark_entry_posted(&conn, "!h:x", "$2").unwrap();
        assert!(entry_posted(&conn, "!h:x").unwrap());
        assert!(!entry_posted(&conn, "!t:x").unwrap());
    }

    #[test]
    fn events_are_tracked_per_round_and_redelivery_is_noticed() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        let round = plenums::open_round(&conn).unwrap().unwrap();
        record_event(&conn, "!a:x", "$1", true).unwrap();
        record_event(&conn, "!a:x", "$1", true).unwrap();
        record_event(&conn, "!b:x", "$2", false).unwrap();
        let rounds: Vec<i64> = conn
            .prepare("SELECT round_id FROM dm_events")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rounds, [round.id, round.id]);
        assert!(first_time(&conn, "$x").unwrap());
        assert!(!first_time(&conn, "$x").unwrap());
    }

    #[test]
    fn tidying_a_flow_touches_only_that_chat_and_that_flow() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        record_event(&conn, "!a:x", "$screen", true).unwrap();
        record_event(&conn, "!a:x", "$receipt", false).unwrap();
        record_event(&conn, "!b:x", "$other", true).unwrap();
        crate::engine::tidy(&conn, "!a:x").unwrap();
        let doomed: Vec<String> = doomed(&conn, 10)
            .unwrap()
            .into_iter()
            .map(|(_, e, _)| e)
            .collect();
        assert_eq!(doomed, ["$screen"]);
    }

    #[test]
    fn a_notice_waits_until_the_chat_is_cleared() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        set_dm(&conn, "@a:x", "!a:x").unwrap();
        set_joined(&conn, "@a:x").unwrap();
        record_event(&conn, "!a:x", "$1", false).unwrap();
        conn.execute_batch(
            "UPDATE residents SET pending_notice = 'round_cleared'; UPDATE dm_events SET doomed = 1;",
        )
        .unwrap();
        assert!(pending_notices(&conn).unwrap().is_empty());
        forget_event(&conn, "!a:x", "$1").unwrap();
        assert_eq!(pending_notices(&conn).unwrap().len(), 1);
    }
}
