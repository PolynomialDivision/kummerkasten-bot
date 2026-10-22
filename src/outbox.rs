//! 📤 Messages that must go out even across a crash or a homeserver
//! hiccup: check-in notices, reminders and announcements, plenum summaries
//! and the update when an announced plenum moves or is cancelled.
//!
//! A batch is queued in the same transaction that marks it as due (the
//! run's `notified_at`, the plenum's `announced_at` …), as one row with an
//! empty target. When it goes out, that row becomes one row per chat
//! (notices, reminders) or room (the rest), decided against who is there
//! *then*. Each row is sent with its own Matrix transaction ID (`kk-out-<id>`,
//! ids are never reused), so a send that succeeded just before a crash is
//! not posted twice when it is retried. A failed send is tried again with a
//! growing wait (up to six hours between tries) and never given up: after
//! [`STUCK_AFTER`] tries it counts as stuck and admins see it in `!kk round`.
//! What isn't needed any more (a check-in closed meanwhile, no chat) is
//! dropped on purpose, by the sender, not by the retry count. Once nothing of
//! a batch is left to send, all its rows are deleted: who was told is kept
//! only while a batch is going out (the counts on the run stay).

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};

use crate::time;

/// After this many failed tries a message counts as stuck: still tried
/// (every six hours), and shown to admins.
pub const STUCK_AFTER: i64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A check-in's start notice, into each resident's chat (`ref`: run).
    Notice,
    /// A check-in's one reminder (`ref`: run).
    Reminder,
    /// A check-in announced in the central room(s) (`ref`: run).
    Announcement,
    /// The day-before summary (`ref`: plenum).
    Summary,
    /// The short update: an announced plenum moved (`ref`: plenum).
    Moved,
    /// … or was cancelled.
    Cancelled,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Notice => "notice",
            Kind::Reminder => "reminder",
            Kind::Announcement => "announcement",
            Kind::Summary => "summary",
            Kind::Moved => "moved",
            Kind::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "notice" => Kind::Notice,
            "reminder" => Kind::Reminder,
            "announcement" => Kind::Announcement,
            "summary" => Kind::Summary,
            "moved" => Kind::Moved,
            "cancelled" => Kind::Cancelled,
            other => bail!("unknown outbox kind {other:?}"),
        })
    }

    /// Into residents' chats (targets are Matrix IDs), else house rooms.
    pub fn to_chats(self) -> bool {
        matches!(self, Kind::Notice | Kind::Reminder)
    }
}

/// One message to send.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: i64,
    pub kind: Kind,
    pub ref_id: i64,
    /// A resident's Matrix ID or a room ID.
    pub target: String,
    /// The Matrix transaction ID it is sent with, every time.
    pub txn: String,
}

/// Queue a batch (who gets it is decided when it goes out). Queuing the
/// same batch again while it is waiting changes nothing.
pub fn queue(conn: &Connection, kind: Kind, ref_id: i64, now: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO outbox (kind, ref_id, target, created_at) VALUES (?1, ?2, '', ?3)",
        params![kind.as_str(), ref_id, time::to_text(now)],
    )?;
    Ok(())
}

/// Batches whose recipients are still to be decided: `(row, kind, ref)`.
pub fn batches(conn: &Connection) -> Result<Vec<(i64, Kind, i64)>> {
    let mut stmt =
        conn.prepare("SELECT id, kind, ref_id FROM outbox WHERE target = '' ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, ref_id) = row?;
        out.push((id, Kind::parse(&kind)?, ref_id));
    }
    Ok(out)
}

/// The batch `row` goes to `targets`: one row each instead of the batch
/// row, in one transaction. No targets: the batch is done.
pub fn expand(conn: &Connection, row: i64, targets: &[String], now: DateTime<Utc>) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let (kind, ref_id): (String, i64) = tx.query_row(
        "SELECT kind, ref_id FROM outbox WHERE id = ?1 AND target = ''",
        [row],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    for target in targets.iter().filter(|t| !t.is_empty()) {
        tx.execute(
            "INSERT OR IGNORE INTO outbox (kind, ref_id, target, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind, ref_id, target, time::to_text(now)],
        )?;
    }
    tx.execute("DELETE FROM outbox WHERE id = ?1", [row])?;
    finish(&tx, &kind, ref_id)?;
    tx.commit()?;
    Ok(())
}

/// Messages due now, oldest first.
pub fn due(conn: &Connection, now: DateTime<Utc>, limit: usize) -> Result<Vec<Item>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, ref_id, target, created_at FROM outbox
         WHERE target != '' AND sent = 0 AND (retry_at IS NULL OR retry_at <= ?1)
         ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![time::to_text(now), limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, ref_id, target, created) = row?;
        out.push(Item {
            id,
            kind: Kind::parse(&kind)?,
            ref_id,
            target,
            txn: format!("kk-out-{id}-{}", created.replace([':', '-'], "")),
        });
    }
    Ok(out)
}

/// It went out.
pub fn sent(conn: &Connection, item: &Item) -> Result<()> {
    conn.execute("UPDATE outbox SET sent = 1 WHERE id = ?1", [item.id])?;
    finish(conn, item.kind.as_str(), item.ref_id)
}

/// Not needed any more (answered meanwhile, no chat, the plenum began).
pub fn skipped(conn: &Connection, item: &Item) -> Result<()> {
    conn.execute("DELETE FROM outbox WHERE id = ?1", [item.id])?;
    finish(conn, item.kind.as_str(), item.ref_id)
}

/// Sending failed: try again after 1, 2, 4 … minutes, at most six hours,
/// for as long as it takes. Returns `true` when the message just became
/// stuck (see [`STUCK_AFTER`]), so it is logged once.
pub fn failed(conn: &Connection, item: &Item, now: DateTime<Utc>) -> Result<bool> {
    let attempts: i64 = conn.query_row(
        "SELECT attempts FROM outbox WHERE id = ?1",
        [item.id],
        |r| r.get(0),
    )?;
    let wait = Duration::minutes((1i64 << attempts.clamp(0, 9)).min(360));
    conn.execute(
        "UPDATE outbox SET attempts = attempts + 1, retry_at = ?2 WHERE id = ?1",
        params![item.id, time::to_text(now + wait)],
    )?;
    Ok(attempts + 1 == STUCK_AFTER)
}

/// Messages still waiting, and how many of them are stuck (for `!kk round`).
pub fn waiting(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*), coalesce(sum(attempts >= ?1), 0) FROM outbox WHERE sent = 0",
        [STUCK_AFTER],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// Nothing of the batch left to send: forget it, and with it who was told.
fn finish(conn: &Connection, kind: &str, ref_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM outbox WHERE kind = ?1 AND ref_id = ?2
           AND NOT EXISTS (SELECT 1 FROM outbox WHERE kind = ?1 AND ref_id = ?2 AND sent = 0)",
        params![kind, ref_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plenums::tests::{at, with_house_calendar};

    fn rows(conn: &Connection) -> Vec<(String, i64)> {
        let mut stmt = conn
            .prepare("SELECT target, sent FROM outbox ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_batch_goes_to_each_target_and_is_forgotten_once_sent() {
        let now = at(2026, 11, 2, 12);
        let conn = with_house_calendar(now);
        queue(&conn, Kind::Notice, 7, now).unwrap();
        queue(&conn, Kind::Notice, 7, now).unwrap();
        let batches = batches(&conn).unwrap();
        assert_eq!(batches.len(), 1, "queued twice: one batch");
        expand(&conn, batches[0].0, &["@a:x".into(), "@b:x".into()], now).unwrap();
        assert!(super::batches(&conn).unwrap().is_empty());
        let items = due(&conn, now, 10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].target, "@a:x");
        assert_ne!(items[0].txn, items[1].txn);
        // The same message keeps its transaction ID.
        assert_eq!(due(&conn, now, 10).unwrap()[0].txn, items[0].txn);
        sent(&conn, &items[0]).unwrap();
        // While the batch goes out, who was told is known…
        assert_eq!(rows(&conn), [("@a:x".into(), 1), ("@b:x".into(), 0)]);
        skipped(&conn, &items[1]).unwrap();
        // …and forgotten once it is done.
        assert!(rows(&conn).is_empty());
        assert_eq!(waiting(&conn).unwrap(), (0, 0));
    }

    #[test]
    fn a_failed_message_waits_longer_each_time_and_is_never_given_up() {
        let now = at(2026, 11, 2, 12);
        let conn = with_house_calendar(now);
        queue(&conn, Kind::Summary, 3, now).unwrap();
        let row = batches(&conn).unwrap()[0].0;
        expand(&conn, row, &["!house:x".into()], now).unwrap();
        let item = due(&conn, now, 10).unwrap().remove(0);
        assert!(!failed(&conn, &item, now).unwrap());
        // Not due again right away; a minute later it is.
        assert!(due(&conn, now, 10).unwrap().is_empty());
        assert_eq!(due(&conn, now + Duration::minutes(1), 10).unwrap().len(), 1);
        // Stuck after STUCK_AFTER tries (said once), still waiting, still due
        // later: six hours at most between tries.
        let mut became_stuck = 0;
        for i in 1..=STUCK_AFTER + 2 {
            if failed(&conn, &item, now + Duration::hours(i)).unwrap() {
                became_stuck += 1;
            }
        }
        assert_eq!(became_stuck, 1);
        assert_eq!(rows(&conn), [("!house:x".into(), 0)]);
        assert_eq!(waiting(&conn).unwrap(), (1, 1));
        let last = now + Duration::hours(STUCK_AFTER + 2);
        assert!(due(&conn, last + Duration::hours(5), 10)
            .unwrap()
            .is_empty());
        assert_eq!(due(&conn, last + Duration::hours(6), 10).unwrap().len(), 1);
        // Sent in the end: forgotten like any other.
        sent(&conn, &item).unwrap();
        assert!(rows(&conn).is_empty());
    }

    #[test]
    fn a_batch_without_targets_is_simply_done_and_ids_are_never_reused() {
        let now = at(2026, 11, 2, 12);
        let conn = with_house_calendar(now);
        queue(&conn, Kind::Announcement, 1, now).unwrap();
        let first = batches(&conn).unwrap()[0].0;
        expand(&conn, first, &[], now).unwrap();
        assert!(rows(&conn).is_empty());
        queue(&conn, Kind::Announcement, 2, now).unwrap();
        assert!(batches(&conn).unwrap()[0].0 > first);
    }
}
