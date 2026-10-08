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

/// They left their chat: forget it (and what was tracked in it — the bot
/// can't redact there any more). Their floor stays for a next chat.
pub fn drop_dm(conn: &Connection, mxid: &str, room_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE residents SET dm_room_id = NULL, dm_joined = 0, pending_notice = NULL
         WHERE mxid = ?1 AND dm_room_id = ?2",
        params![mxid, room_id],
    )?;
    conn.execute("DELETE FROM sessions WHERE mxid = ?1", [mxid])?;
    conn.execute("DELETE FROM dm_events WHERE room_id = ?1", [room_id])?;
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

/// Events marked for redaction: `(room_id, event_id, attempts)`.
pub fn doomed(conn: &Connection, limit: usize) -> Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT room_id, event_id, attempts FROM dm_events WHERE doomed = 1
         ORDER BY room_id, sent_at LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn forget_event(conn: &Connection, room_id: &str, event_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM dm_events WHERE room_id = ?1 AND event_id = ?2",
        params![room_id, event_id],
    )?;
    Ok(())
}

pub fn retry_event(conn: &Connection, room_id: &str, event_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE dm_events SET attempts = attempts + 1 WHERE room_id = ?1 AND event_id = ?2",
        params![room_id, event_id],
    )?;
    Ok(())
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
