//! The end of a round: a plenum is completed — by an admin, when it has
//! actually taken place (or, only if the `auto_complete` policy is on,
//! automatically at `complete_at` on its day) — and in one transaction
//!
//! 1. the plenum is marked completed and its round closed,
//! 2. submissions to delete after this plenum are deleted (with their codes),
//! 3. every chat event of the round is marked for redaction,
//! 4. drafts and conversations are dropped,
//! 5. every resident gets a "round cleared" notice + fresh menu queued,
//! 6. the next round opens.
//!
//! The Matrix side (redacting, posting the fresh menus) runs afterwards from
//! that state (`jobs`), so a crash in between loses nothing.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveTime, Utc};
use rusqlite::{params, Connection};

use crate::{
    plenums::{self, Status},
    time::{self, Zone},
};

#[derive(Debug, PartialEq)]
pub struct Completion {
    pub plenum_id: i64,
    pub round_id: Option<i64>,
    /// Submissions deleted as their authors asked.
    pub deleted: usize,
    /// Submissions of the round that stay.
    pub kept: usize,
    pub next_round: Option<i64>,
}

/// Complete the next plenum (`plenum_id` must be it: rounds go in order).
pub fn complete(
    conn: &mut Connection,
    plenum_id: i64,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<Completion> {
    let tx = conn.transaction()?;
    let plenum = plenums::get(&tx, plenum_id)?
        .with_context(|| format!("There is no plenum #{plenum_id}."))?;
    if plenum.status != Status::Scheduled {
        bail!("Plenum #{plenum_id} is {}.", plenum.status.label());
    }
    let next = plenums::next(&tx)?.context("No plenum is scheduled.")?;
    if next.id != plenum_id {
        bail!(
            "Plenum #{} comes first — complete or cancel it before #{plenum_id}.",
            next.id
        );
    }
    // The round belongs to this plenum (`ensure_round` keeps it so).
    let round = plenums::ensure_round(&tx, now)?.filter(|r| r.plenum_id == plenum_id);
    tx.execute(
        "UPDATE plenums SET status = 'completed', completed_at = ?2 WHERE id = ?1",
        params![plenum_id, time::to_text(now)],
    )?;
    let mut kept = 0;
    if let Some(round) = round {
        tx.execute(
            "UPDATE rounds SET status = 'closed', closed_at = ?2 WHERE id = ?1",
            params![round.id, time::to_text(now)],
        )?;
        kept = tx.query_row(
            "SELECT count(*) FROM submissions WHERE round_id = ?1 AND retention = 'keep'",
            [round.id],
            |r| r.get::<_, i64>(0),
        )? as usize;
    }
    // Deleting a submission deletes its code (ON DELETE CASCADE).
    let deleted = tx.execute(
        "DELETE FROM submissions WHERE delete_after_plenum_id = ?1",
        [plenum_id],
    )?;
    // Everything in the chats so far belongs to rounds that are over now.
    tx.execute("UPDATE dm_events SET doomed = 1", [])?;
    tx.execute("DELETE FROM sessions", [])?;
    tx.execute(
        "UPDATE residents SET pending_notice = 'round_cleared'
         WHERE dm_room_id IS NOT NULL AND dm_joined = 1",
        [],
    )?;
    tx.execute(
        "INSERT INTO plenum_changes (plenum_id, action, detail, actor, at)
         VALUES (?1, 'complete', ?2, ?3, ?4)",
        params![
            plenum_id,
            format!("deleted {deleted}, kept {kept}"),
            actor,
            time::to_text(now)
        ],
    )?;
    let next_round = plenums::ensure_round(&tx, now)?.map(|r| r.id);
    tx.commit()?;
    tracing::info!(
        plenum = plenum_id,
        round = ?round.map(|r| r.id),
        deleted,
        kept,
        "Plenum completed"
    );
    Ok(Completion {
        plenum_id,
        round_id: round.map(|r| r.id),
        deleted,
        kept,
        next_round,
    })
}

/// Scheduled plenums whose day is over (local time past `complete_at` on
/// their day, or a later day), oldest first — for the auto-complete
/// policy only. Without it, such a plenum just awaits completion.
pub fn due(
    conn: &Connection,
    zone: Zone,
    now: DateTime<Utc>,
    complete_at: NaiveTime,
) -> Result<Vec<i64>> {
    let today = zone.date(now);
    let local_now = zone.local(now).time();
    Ok(plenums::upcoming(conn, 50)?
        .into_iter()
        .take_while(|p| {
            let day = zone.date(p.starts_at);
            day < today || (day == today && local_now >= complete_at && now >= p.starts_at)
        })
        .map(|p| p.id)
        .collect())
}

/// Undo a completion that was a mistake: the latest completed plenum is
/// scheduled again and its round reopened, taking back what was submitted
/// since. What was already deleted or redacted stays gone.
pub fn reopen(
    conn: &mut Connection,
    plenum_id: i64,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    let tx = conn.transaction()?;
    let latest = plenums::previous(&tx)?.context("No plenum was completed yet.")?;
    if latest.id != plenum_id {
        bail!(
            "Only the latest completed plenum (#{}) can be reopened.",
            latest.id
        );
    }
    let closed: i64 = tx
        .query_row(
            "SELECT id FROM rounds WHERE plenum_id = ?1",
            [plenum_id],
            |r| r.get(0),
        )
        .context("That plenum had no round.")?;
    if let Some(open) = plenums::open_round(&tx)? {
        tx.execute(
            "UPDATE submissions SET round_id = ?2,
                 delete_after_plenum_id = CASE WHEN retention = 'delete_after_plenum'
                                               THEN ?3 ELSE NULL END
             WHERE round_id = ?1",
            params![open.id, closed, plenum_id],
        )?;
        tx.execute(
            "UPDATE dm_events SET round_id = ?2 WHERE round_id = ?1",
            params![open.id, closed],
        )?;
        tx.execute("DELETE FROM rounds WHERE id = ?1", [open.id])?;
    }
    tx.execute(
        "UPDATE plenums SET status = 'scheduled', completed_at = NULL WHERE id = ?1",
        [plenum_id],
    )?;
    tx.execute(
        "UPDATE rounds SET status = 'open', closed_at = NULL WHERE id = ?1",
        [closed],
    )?;
    tx.execute(
        "INSERT INTO plenum_changes (plenum_id, action, detail, actor, at)
         VALUES (?1, 'reopen', '', ?2, ?3)",
        params![plenum_id, actor, time::to_text(now)],
    )?;
    plenums::ensure_round(&tx, now)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plenums::tests::{at, with_default_rule, zone},
        secrets::Secrets,
        submissions::{self, tests::new, Identity, Retention},
    };

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn completing_a_plenum_runs_the_whole_lifecycle() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[9; 32]);
        let today = zone().date(now);
        let plenum = plenums::next(&conn).unwrap().unwrap();
        let round = plenums::open_round(&conn).unwrap().unwrap();
        let keep = submissions::submit(
            &conn,
            &secrets,
            &new(&conn, Identity::Anonymous, Retention::Keep),
            "@a:x",
            None,
            today,
        )
        .unwrap();
        let gone = submissions::submit(
            &conn,
            &secrets,
            &new(&conn, Identity::Anonymous, Retention::DeleteAfterPlenum),
            "@a:x",
            None,
            today,
        )
        .unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO residents (mxid, dm_room_id, dm_joined, created_at) VALUES ('@a:x', '!dm:x', 1, 'now');
             INSERT INTO dm_events (room_id, event_id, round_id, sent_at) VALUES ('!dm:x', '$1', {}, 'now');
             INSERT INTO sessions (mxid, room_id, flow, updated_at) VALUES ('@a:x', '!dm:x', '{{}}', 'now');",
            round.id
        ))
        .unwrap();

        // Not yet: the plenum is on the 15th.
        let seven = NaiveTime::from_hms_opt(23, 59, 0).unwrap();
        assert!(due(&conn, zone(), now, seven).unwrap().is_empty());
        let later = at(2026, 11, 16, 9);
        assert_eq!(due(&conn, zone(), later, seven).unwrap(), [plenum.id]);

        let done = complete(&mut conn, plenum.id, "system", later).unwrap();
        assert_eq!(done.deleted, 1);
        assert_eq!(done.kept, 1);
        assert_eq!(done.round_id, Some(round.id));
        // Deleted for real (with its code), the other stays.
        assert_eq!(
            count(
                &conn,
                &format!("SELECT count(*) FROM submissions WHERE id = {}", gone.id)
            ),
            0
        );
        assert_eq!(
            count(
                &conn,
                &format!("SELECT count(*) FROM submissions WHERE id = {}", keep.id)
            ),
            1
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM deletion_credentials"), 1);
        // The chat is to be cleared, drafts are gone, a fresh menu is due.
        assert_eq!(
            count(&conn, "SELECT count(*) FROM dm_events WHERE doomed = 0"),
            0
        );
        assert_eq!(count(&conn, "SELECT count(*) FROM sessions"), 0);
        assert_eq!(
            count(
                &conn,
                "SELECT count(*) FROM residents WHERE pending_notice = 'round_cleared'"
            ),
            1
        );
        // The next round is open, for the next plenum.
        let next_round = plenums::open_round(&conn).unwrap().unwrap();
        assert_eq!(Some(next_round.id), done.next_round);
        assert_eq!(
            next_round.plenum_id,
            plenums::next(&conn).unwrap().unwrap().id
        );
        assert_eq!(plenums::previous(&conn).unwrap().unwrap().id, plenum.id);
        // Twice is refused; so is skipping ahead.
        assert!(complete(&mut conn, plenum.id, "system", later).is_err());
        let after_next = plenums::upcoming(&conn, 2).unwrap()[1].id;
        assert!(complete(&mut conn, after_next, "@admin:x", later).is_err());
    }

    #[test]
    fn a_cancelled_plenum_hands_its_deletions_to_the_next_one() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[9; 32]);
        let first = plenums::next(&conn).unwrap().unwrap();
        let s = submissions::submit(
            &conn,
            &secrets,
            &new(&conn, Identity::Anonymous, Retention::DeleteAfterPlenum),
            "@a:x",
            None,
            zone().date(now),
        )
        .unwrap();
        plenums::cancel(&mut conn, first.id, "@admin:x", now).unwrap();
        let second = plenums::next(&conn).unwrap().unwrap();
        let points_at: i64 = conn
            .query_row(
                "SELECT delete_after_plenum_id FROM submissions WHERE id = ?1",
                [s.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(points_at, second.id);
        // A move of that plenum keeps the link (it's the same row).
        plenums::reschedule(
            &mut conn,
            second.id,
            at(2026, 11, 28, 18),
            None,
            "@admin:x",
            zone(),
            now,
        )
        .unwrap();
        let done = complete(&mut conn, second.id, "system", at(2026, 11, 29, 9)).unwrap();
        assert_eq!(done.deleted, 1);
    }

    #[test]
    fn reopening_a_mistaken_completion_merges_the_rounds_back() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let secrets = Secrets::from_master(&[9; 32]);
        let first = plenums::next(&conn).unwrap().unwrap();
        let old_round = plenums::open_round(&conn).unwrap().unwrap();
        complete(&mut conn, first.id, "@admin:x", now).unwrap();
        let s = submissions::submit(
            &conn,
            &secrets,
            &new(&conn, Identity::Anonymous, Retention::DeleteAfterPlenum),
            "@a:x",
            None,
            zone().date(now),
        )
        .unwrap();
        reopen(&mut conn, first.id, "@admin:x", now).unwrap();
        let round = plenums::open_round(&conn).unwrap().unwrap();
        assert_eq!(
            round,
            plenums::Round {
                id: old_round.id,
                plenum_id: first.id
            }
        );
        let (r, d): (i64, i64) = conn
            .query_row(
                "SELECT round_id, delete_after_plenum_id FROM submissions WHERE id = ?1",
                [s.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((r, d), (old_round.id, first.id));
        assert_eq!(count(&conn, "SELECT count(*) FROM rounds"), 1);
    }
}
