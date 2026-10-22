//! Concrete plenums and rounds in the database.
//!
//! The plenum's dates are kept in one place only: the Nextcloud calendar
//! plenum-bot owns (`plenum-house`), shared read-only with this bot.
//! [`sync_calendar`] follows it:
//!
//!   calendar occurrence (stable ID: UID, or UID#original-date in a series)
//!   → plenum row (`calendar_id`, stable row ID)
//!
//! A moved event moves its row (same ID), a deleted one cancels it, one that
//! comes back schedules it again. Past plenums are never rewritten, and a
//! read without any occurrence changes nothing (an emptied or re-made
//! calendar must not cancel the plenum a round is collecting for).
//!
//! Invariant kept by [`ensure_round`]: the one open round belongs to the
//! earliest scheduled plenum, and the "delete after the next plenum"
//! submissions of that round point at the same plenum. The calendar may
//! change the order of plenums (a move past another one): the round then
//! goes with the earliest one, with its submissions and check-ins.
//!
//! A moved plenum keeps its ID, so its round, its "delete after"
//! submissions, check-ins, measures and announcements stay with it;
//! `original_starts_at` keeps the first date. A plenum whose time has
//! passed is *awaiting completion*, not completed: only an admin (or the
//! explicit auto-complete policy) completes it.

use std::collections::HashSet;

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::time::{self, Zone};

/// How far ahead the calendar is read (and plenums exist as rows).
pub const HORIZON_DAYS: i64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Scheduled,
    Cancelled,
    Completed,
}

impl Status {
    fn parse(s: &str) -> Self {
        match s {
            "cancelled" => Status::Cancelled,
            "completed" => Status::Completed,
            _ => Status::Scheduled,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Status::Scheduled => "🗓️ scheduled",
            Status::Cancelled => "⏭️ cancelled",
            Status::Completed => "✅ completed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plenum {
    pub id: i64,
    /// The calendar occurrence it follows; `None`: from before the calendar
    /// (the old rules), not (yet) found there.
    pub calendar_id: Option<String>,
    /// When it is (now): what everything follows.
    pub starts_at: DateTime<Utc>,
    pub status: Status,
    pub rescheduled: bool,
    pub completed_at: Option<DateTime<Utc>>,
    /// When it was first planned; never changes.
    pub original_starts_at: DateTime<Utc>,
    pub reschedule_reason: Option<String>,
    /// When its day-before summary went out.
    pub announced_at: Option<DateTime<Utc>>,
}

impl Plenum {
    /// Its time has come, but nobody said it happened: the round stays
    /// open and nothing is cleaned up until it is completed.
    pub fn awaiting_completion(&self, now: DateTime<Utc>) -> bool {
        self.status == Status::Scheduled && self.starts_at <= now
    }
}

const COLUMNS: &str = "id, calendar_id, starts_at, status, rescheduled, completed_at,
                       coalesce(original_starts_at, starts_at), reschedule_reason, announced_at";

fn plenum(r: &Row) -> rusqlite::Result<Plenum> {
    let text = |i: usize| -> rusqlite::Result<Option<String>> { r.get(i) };
    let bad = |i: usize, e: anyhow::Error| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, e.into())
    };
    Ok(Plenum {
        id: r.get(0)?,
        calendar_id: r.get(1)?,
        starts_at: time::parse_text(&r.get::<_, String>(2)?).map_err(|e| bad(2, e))?,
        status: Status::parse(&r.get::<_, String>(3)?),
        rescheduled: r.get(4)?,
        completed_at: text(5)?
            .map(|s| time::parse_text(&s))
            .transpose()
            .map_err(|e| bad(5, e))?,
        original_starts_at: time::parse_text(&r.get::<_, String>(6)?).map_err(|e| bad(6, e))?,
        reschedule_reason: r.get(7)?,
        announced_at: text(8)?
            .map(|s| time::parse_text(&s))
            .transpose()
            .map_err(|e| bad(8, e))?,
    })
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Plenum>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM plenums WHERE id = ?1"),
            [id],
            plenum,
        )
        .optional()?)
}

fn query(conn: &Connection, sql_tail: &str, args: impl rusqlite::Params) -> Result<Vec<Plenum>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM plenums {sql_tail}"))?;
    let rows = stmt.query_map(args, plenum)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The next plenum: the earliest one still scheduled (also one whose day
/// is today, or already past while it waits to be completed).
pub fn next(conn: &Connection) -> Result<Option<Plenum>> {
    Ok(upcoming(conn, 1)?.into_iter().next())
}

/// The next `limit` scheduled plenums, in order: `[1]` is the one after next.
pub fn upcoming(conn: &Connection, limit: usize) -> Result<Vec<Plenum>> {
    query(
        conn,
        "WHERE status = 'scheduled' ORDER BY starts_at, id LIMIT ?1",
        [limit as i64],
    )
}

/// The latest completed plenum.
pub fn previous(conn: &Connection) -> Result<Option<Plenum>> {
    Ok(query(
        conn,
        "WHERE status = 'completed' ORDER BY starts_at DESC, id DESC LIMIT 1",
        [],
    )?
    .into_iter()
    .next())
}

/// Plenums from `from` on (any status), for the admin overview.
pub fn listing(conn: &Connection, from: DateTime<Utc>, limit: usize) -> Result<Vec<Plenum>> {
    query(
        conn,
        "WHERE starts_at >= ?1 ORDER BY starts_at, id LIMIT ?2",
        params![time::to_text(from), limit as i64],
    )
}

// ── The calendar ────────────────────────────────────────────────────────────

/// One plenum as the calendar has it now.
#[derive(Clone, Debug, PartialEq)]
pub struct Occurrence {
    /// Stable when the event moves: its UID, for one of a series UID and
    /// the date it was first due.
    pub id: String,
    pub starts_at: DateTime<Utc>,
}

/// The local days the calendar is read for: from yesterday (a plenum that
/// is running or just over) to [`HORIZON_DAYS`] ahead.
pub fn window(zone: Zone, now: DateTime<Utc>) -> (NaiveDate, NaiveDate) {
    let today = zone.date(now);
    (
        today - Duration::days(1),
        today + Duration::days(HORIZON_DAYS),
    )
}

/// The end of [`window`]: rows from then on are out of view, not gone.
pub fn window_end(zone: Zone, now: DateTime<Utc>) -> DateTime<Utc> {
    zone.at(window(zone, now).1 + Duration::days(1), NaiveTime::MIN)
}

/// What one read of the calendar changed.
#[derive(Debug, Default, PartialEq)]
pub struct Synced {
    /// Rows from before the calendar (the old rules) that an occurrence at
    /// the same time (else on the same day) took over.
    pub adopted: usize,
    pub added: usize,
    pub moved: usize,
    pub cancelled: usize,
    /// Cancelled ones back in the calendar.
    pub restored: usize,
    /// Future rows from before the calendar no occurrence took over (the old
    /// schedule): cancelled, and deleted when nothing points at them.
    pub retired: usize,
}

impl Synced {
    pub fn changed(&self) -> bool {
        *self != Synced::default()
    }
}

const CALENDAR: &str = "calendar";

/// Follow one read of the calendar: `occurrences` are the plenums it has
/// in [`window`], `until` that window's end ([`window_end`]).
///
/// Each occurrence goes to the row linked to it; else it adopts a row from
/// before the calendar at the same time (else on the same day); else it is
/// a new row (not for one already past). A linked row follows its
/// occurrence (moved: same row; back: scheduled again); a linked future row
/// whose occurrence is gone is cancelled, and so is every future row from
/// before the calendar that no occurrence took over. Completed plenums, and
/// past ones that stay past, are never rewritten. Without any occurrence
/// ahead, nothing changes at all.
pub fn sync_calendar(
    conn: &mut Connection,
    occurrences: &[Occurrence],
    zone: Zone,
    until: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Synced> {
    let mut synced = Synced::default();
    // Nothing ahead (an emptied calendar, one re-made, only yesterday's
    // left): no read to cancel the plenum the round collects for on.
    if !occurrences.iter().any(|o| o.starts_at > now) {
        let waiting: i64 = conn.query_row(
            "SELECT count(*) FROM plenums WHERE status = 'scheduled' AND starts_at > ?1",
            [time::to_text(now)],
            |r| r.get(0),
        )?;
        if waiting > 0 {
            tracing::warn!(
                plenums = waiting,
                "The calendar has no plenum ahead: nothing is cancelled, the plenums stay as they are"
            );
        }
        return Ok(synced);
    }
    let mut occurrences = occurrences.to_vec();
    occurrences.sort_by(|a, b| (a.starts_at, &a.id).cmp(&(b.starts_at, &b.id)));
    let mut ids = HashSet::new();
    occurrences.retain(|o| ids.insert(o.id.clone()));

    let tx = conn.transaction()?;
    let mut rows: Vec<Option<Plenum>> = occurrences
        .iter()
        .map(|o| linked(&tx, &o.id))
        .collect::<Result<_>>()?;

    // Rows from before the calendar: the one at the same time first, then
    // one on the same day (the time changed with the move to the calendar).
    let unlinked = query(
        &tx,
        "WHERE calendar_id IS NULL AND status = 'scheduled' ORDER BY starts_at, id",
        [],
    )?;
    let mut taken = HashSet::new();
    for same_time in [true, false] {
        for (o, row) in occurrences.iter().zip(rows.iter_mut()) {
            if row.is_some() {
                continue;
            }
            let found = unlinked.iter().find(|p| {
                !taken.contains(&p.id)
                    && if same_time {
                        p.starts_at == o.starts_at
                    } else {
                        zone.date(p.starts_at) == zone.date(o.starts_at)
                            && !(p.starts_at <= now && o.starts_at <= now)
                    }
            });
            let Some(p) = found else { continue };
            taken.insert(p.id);
            tx.execute(
                "UPDATE plenums SET calendar_id = ?2 WHERE id = ?1",
                params![p.id, o.id],
            )?;
            audit(&tx, Some(p.id), "adopt", &o.id, CALENDAR, now)?;
            *row = Some(Plenum {
                calendar_id: Some(o.id.clone()),
                ..p.clone()
            });
            synced.adopted += 1;
        }
    }

    for (o, row) in occurrences.iter().zip(&rows) {
        let Some(p) = row else {
            // Not created after the fact: a plenum the bot never knew of
            // must not take over the round once it is over.
            if o.starts_at > now {
                tx.execute(
                    "INSERT INTO plenums
                       (calendar_id, starts_at, original_starts_at, status, created_at)
                     VALUES (?1, ?2, ?2, 'scheduled', ?3)",
                    params![o.id, time::to_text(o.starts_at), time::to_text(now)],
                )?;
                let id = tx.last_insert_rowid();
                audit(
                    &tx,
                    Some(id),
                    "add",
                    &time::to_text(o.starts_at),
                    CALENDAR,
                    now,
                )?;
                synced.added += 1;
            }
            continue;
        };
        if p.status == Status::Completed || (p.starts_at <= now && o.starts_at <= now) {
            continue;
        }
        if p.status == Status::Cancelled {
            // A cancellation not announced yet is simply off again.
            tx.execute(
                "UPDATE plenums SET status = 'scheduled', update_pending = NULL WHERE id = ?1",
                [p.id],
            )?;
            audit(&tx, Some(p.id), "restore", &o.id, CALENDAR, now)?;
            synced.restored += 1;
        }
        if p.starts_at != o.starts_at {
            move_row(&tx, p, o.starts_at, now)?;
            synced.moved += 1;
        }
    }

    // Gone from the calendar: future linked plenums in view.
    let gone: Vec<i64> = query(
        &tx,
        "WHERE calendar_id IS NOT NULL AND status = 'scheduled'
           AND starts_at > ?1 AND starts_at < ?2 ORDER BY starts_at, id",
        params![time::to_text(now), time::to_text(until)],
    )?
    .into_iter()
    .filter(|p| p.calendar_id.as_ref().is_some_and(|id| !ids.contains(id)))
    .map(|p| p.id)
    .collect();
    for id in gone {
        cancel_row(&tx, id, "gone from the calendar", now)?;
        synced.cancelled += 1;
    }

    // The old schedule: future rows no occurrence took over.
    let old: Vec<i64> = query(
        &tx,
        "WHERE calendar_id IS NULL AND status = 'scheduled' AND starts_at > ?1
         ORDER BY starts_at, id",
        [time::to_text(now)],
    )?
    .into_iter()
    .map(|p| p.id)
    .collect();
    for &id in &old {
        cancel_row(&tx, id, "not in the calendar", now)?;
        synced.retired += 1;
    }

    // The round goes with the earliest scheduled plenum first, then what is
    // left of the old schedule with nothing pointing at it goes.
    ensure_round(&tx, now)?;
    for id in old {
        tx.execute(
            "DELETE FROM plenums WHERE id = ?1 AND calendar_id IS NULL AND status = 'cancelled'
               AND announced_at IS NULL AND update_pending IS NULL
               AND NOT EXISTS (SELECT 1 FROM rounds WHERE plenum_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM submissions WHERE delete_after_plenum_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM survey_runs WHERE plenum_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM measures WHERE plenum_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM plenum_announcements WHERE plenum_id = ?1)",
            [id],
        )?;
    }
    tx.commit()?;
    Ok(synced)
}

fn linked(conn: &Connection, calendar_id: &str) -> Result<Option<Plenum>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM plenums WHERE calendar_id = ?1"),
            [calendar_id],
            plenum,
        )
        .optional()?)
}

fn audit(
    conn: &Connection,
    plenum_id: Option<i64>,
    action: &str,
    detail: &str,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO plenum_changes (plenum_id, action, detail, actor, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![plenum_id, action, detail, actor, time::to_text(now)],
    )?;
    Ok(())
}

/// A move later than this after the day-before summary went out gets a
/// fresh summary the day before its new date (besides the short update);
/// a move by a day or two only the update.
pub const FRESH_SUMMARY_AFTER: Duration = Duration::hours(72);

/// Move a plenum, at any notice, even on its day or after its time passed
/// without it happening. Same row, same ID: its round, "delete after"
/// submissions, check-ins and everything else stay with it, and what
/// follows its date (check-in deadlines set relative to it, the day-before
/// summary, completion) follows the new one. The caller keeps the round
/// invariant ([`ensure_round`]).
fn move_row(
    conn: &Connection,
    before: &Plenum,
    starts_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<()> {
    // Announced already: say that it moved; and if it moved far, announce
    // it again the day before its new date.
    let announced = before.announced_at.is_some();
    let fresh_summary = announced && starts_at - before.starts_at > FRESH_SUMMARY_AFTER;
    conn.execute(
        "UPDATE plenums SET starts_at = ?2, rescheduled = 1,
                original_starts_at = coalesce(original_starts_at, ?3),
                reschedule_reason = NULL, rescheduled_at = ?4,
                update_pending = CASE WHEN announced_at IS NOT NULL THEN 'moved'
                                      ELSE update_pending END,
                announced_at = CASE WHEN ?5 THEN NULL ELSE announced_at END
         WHERE id = ?1",
        params![
            before.id,
            time::to_text(starts_at),
            time::to_text(before.starts_at),
            time::to_text(now),
            fresh_summary
        ],
    )?;
    audit(
        conn,
        Some(before.id),
        "move",
        &format!(
            "{} → {}",
            time::to_text(before.starts_at),
            time::to_text(starts_at)
        ),
        CALENDAR,
        now,
    )?;
    crate::checkins::follow_plenum(conn, before.id, now)?;
    tracing::info!(
        plenum = before.id,
        announced,
        "Plenum moved in the calendar"
    );
    Ok(())
}

/// Skip one plenum. Its round (and what was to be deleted after it) moves
/// on to the next plenum with the caller's [`ensure_round`].
fn cancel_row(conn: &Connection, id: i64, why: &str, now: DateTime<Utc>) -> Result<()> {
    // Announced already: say it's off (its summary is not sent again).
    conn.execute(
        "UPDATE plenums SET status = 'cancelled',
                update_pending = CASE WHEN announced_at IS NOT NULL THEN 'cancelled' END
         WHERE id = ?1",
        [id],
    )?;
    audit(conn, Some(id), "cancel", why, CALENDAR, now)?;
    Ok(())
}

// ── Rounds ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Round {
    pub id: i64,
    pub plenum_id: i64,
}

pub fn open_round(conn: &Connection) -> Result<Option<Round>> {
    Ok(conn
        .query_row(
            "SELECT id, plenum_id FROM rounds WHERE status = 'open'",
            [],
            |r| {
                Ok(Round {
                    id: r.get(0)?,
                    plenum_id: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// Keep the invariant: an open round exists (once any plenum is scheduled)
/// and belongs to the earliest scheduled plenum; its "delete after the next
/// plenum" submissions follow it there. Returns the open round.
pub fn ensure_round(conn: &Connection, now: DateTime<Utc>) -> Result<Option<Round>> {
    let next = next(conn)?;
    match (open_round(conn)?, next) {
        (None, Some(p)) => {
            conn.execute(
                "INSERT INTO rounds (plenum_id, opened_at, status) VALUES (?1, ?2, 'open')",
                params![p.id, time::to_text(now)],
            )?;
            let id = conn.last_insert_rowid();
            tracing::info!(round = id, plenum = p.id, "Opened a Kummerkasten round");
            Ok(Some(Round {
                id,
                plenum_id: p.id,
            }))
        }
        (Some(round), Some(p)) if round.plenum_id != p.id => {
            conn.execute(
                "UPDATE rounds SET plenum_id = ?2 WHERE id = ?1",
                params![round.id, p.id],
            )?;
            conn.execute(
                "UPDATE submissions SET delete_after_plenum_id = ?2
                 WHERE round_id = ?1 AND retention = 'delete_after_plenum'",
                params![round.id, p.id],
            )?;
            // Its check-ins too (deadlines set relative to the plenum follow).
            crate::checkins::follow_round(conn, round.id, p.id, now)?;
            tracing::info!(
                round = round.id,
                plenum = p.id,
                "Round moved to another plenum"
            );
            Ok(Some(Round {
                id: round.id,
                plenum_id: p.id,
            }))
        }
        (round, _) => Ok(round),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::memory_connection;
    use chrono::{Datelike, Weekday};

    pub fn zone() -> Zone {
        Zone::parse("Europe/Berlin").unwrap()
    }

    pub fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        zone().at(
            NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            NaiveTime::from_hms_opt(h, 0, 0).unwrap(),
        )
    }

    pub fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// The house's rhythm: the 1st, 3rd and (where a month has one) 5th
    /// Sunday in `from..=to`.
    pub fn house_sundays(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
        from.iter_days()
            .take_while(|d| *d <= to)
            .filter(|d| d.weekday() == Weekday::Sun && matches!((d.day() - 1) / 7, 0 | 2 | 4))
            .collect()
    }

    /// plenum-bot's calendar as of `now`: the house's Sundays at 19:00 in
    /// view, each its own event.
    pub fn house_calendar(now: DateTime<Utc>) -> Vec<Occurrence> {
        let (from, to) = window(zone(), now);
        house_sundays(from, to)
            .into_iter()
            .map(|d| Occurrence {
                id: format!("house-{d}"),
                starts_at: zone().at(d, NaiveTime::from_hms_opt(19, 0, 0).unwrap()),
            })
            .collect()
    }

    /// One read of the calendar as of `now`.
    pub fn read(conn: &mut Connection, calendar: &[Occurrence], now: DateTime<Utc>) -> Synced {
        sync_calendar(conn, calendar, zone(), window_end(zone(), now), now).unwrap()
    }

    /// A database whose plenums came from the house's calendar, as of `now`.
    pub fn with_house_calendar(now: DateTime<Utc>) -> Connection {
        let mut conn = memory_connection();
        read(&mut conn, &house_calendar(now), now);
        conn
    }

    /// The calendar as the rows know it (every event not cancelled).
    pub fn calendar_of(conn: &Connection) -> Vec<Occurrence> {
        query(
            conn,
            "WHERE calendar_id IS NOT NULL AND status <> 'cancelled' ORDER BY starts_at, id",
            [],
        )
        .unwrap()
        .into_iter()
        .map(|p| Occurrence {
            id: p.calendar_id.unwrap(),
            starts_at: p.starts_at,
        })
        .collect()
    }

    fn event_of(conn: &Connection, id: i64) -> String {
        get(conn, id).unwrap().unwrap().calendar_id.unwrap()
    }

    /// Someone moves plenum `id`'s event to `to`; the bot reads it at `now`.
    pub fn move_in_calendar(conn: &mut Connection, id: i64, to: DateTime<Utc>, now: DateTime<Utc>) {
        let event = event_of(conn, id);
        let mut calendar = calendar_of(conn);
        calendar
            .iter_mut()
            .filter(|o| o.id == event)
            .for_each(|o| o.starts_at = to);
        read(conn, &calendar, now);
    }

    /// Someone deletes plenum `id`'s event; the bot reads it at `now`.
    pub fn cancel_in_calendar(conn: &mut Connection, id: i64, now: DateTime<Utc>) {
        let event = event_of(conn, id);
        let mut calendar = calendar_of(conn);
        calendar.retain(|o| o.id != event);
        read(conn, &calendar, now);
    }

    fn dates(conn: &Connection) -> Vec<NaiveDate> {
        upcoming(conn, 20)
            .unwrap()
            .iter()
            .map(|p| zone().date(p.starts_at))
            .collect()
    }

    fn ids(conn: &Connection) -> Vec<i64> {
        upcoming(conn, 50).unwrap().iter().map(|p| p.id).collect()
    }

    #[test]
    fn plenums_come_from_the_calendar_with_stable_ids() {
        let now = at(2026, 3, 2, 12);
        let mut conn = with_house_calendar(now);
        let first = dates(&conn);
        assert_eq!(
            first[..4],
            [
                day(2026, 3, 15),
                day(2026, 3, 29),
                day(2026, 4, 5),
                day(2026, 4, 19)
            ]
        );
        // March 1st (yesterday, in view) is past: not created after the fact.
        assert!(!first.contains(&day(2026, 3, 1)));
        let before = ids(&conn);
        // Reading the same calendar again changes nothing.
        assert!(!read(&mut conn, &house_calendar(now), now).changed());
        assert_eq!(ids(&conn), before);
        // 19:00 Berlin, stored in UTC.
        let next = next(&conn).unwrap().unwrap();
        assert_eq!(time::to_text(next.starts_at), "2026-03-15T18:00:00Z");
        assert_eq!(next.calendar_id.as_deref(), Some("house-2026-03-15"));
        assert!(previous(&conn).unwrap().is_none());
        // The open round belongs to the next plenum.
        assert_eq!(open_round(&conn).unwrap().unwrap().plenum_id, next.id);
        // A week later the window reaches further: new ones are added, the
        // ones there keep their IDs.
        let later = at(2026, 3, 9, 12);
        let synced = read(&mut conn, &house_calendar(later), later);
        assert_eq!((synced.added, synced.cancelled), (1, 0));
        assert_eq!(ids(&conn)[..before.len()], before[..]);
    }

    #[test]
    fn a_moved_event_moves_only_its_row() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_house_calendar(now);
        let before = upcoming(&conn, 4).unwrap();
        // Sunday 15 Nov → Saturday 14 Nov, 18:00.
        let target = before[0].clone();
        assert_eq!(zone().date(target.starts_at), day(2026, 11, 15));
        move_in_calendar(&mut conn, target.id, at(2026, 11, 14, 18), now);
        let after = upcoming(&conn, 4).unwrap();
        assert_eq!(after.len(), before.len());
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(b.id, a.id);
            if a.id == target.id {
                assert_eq!(a.starts_at, at(2026, 11, 14, 18));
                assert_eq!(a.original_starts_at, target.starts_at);
                assert!(a.rescheduled);
            } else {
                assert_eq!(a.starts_at, b.starts_at);
            }
        }
        assert_eq!(open_round(&conn).unwrap().unwrap().plenum_id, target.id);
        // Reading it again changes nothing.
        let calendar = calendar_of(&conn);
        assert!(!read(&mut conn, &calendar, now).changed());
    }

    #[test]
    fn a_deleted_event_cancels_its_plenum_and_one_back_schedules_it_again() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_house_calendar(now);
        let first = next(&conn).unwrap().unwrap();
        let round = open_round(&conn).unwrap().unwrap();
        let mut calendar = calendar_of(&conn);
        let event = calendar.remove(0);
        let synced = read(&mut conn, &calendar, now);
        assert_eq!(synced.cancelled, 1);
        assert_eq!(
            get(&conn, first.id).unwrap().unwrap().status,
            Status::Cancelled
        );
        // The round carries on to the next plenum …
        let second = next(&conn).unwrap().unwrap();
        assert_eq!(
            open_round(&conn).unwrap().unwrap(),
            Round {
                id: round.id,
                plenum_id: second.id
            }
        );
        // … and back with the event (same row).
        calendar.insert(0, event);
        let synced = read(&mut conn, &calendar, now);
        assert_eq!((synced.restored, synced.added), (1, 0));
        assert_eq!(next(&conn).unwrap().unwrap().id, first.id);
        assert_eq!(open_round(&conn).unwrap().unwrap().plenum_id, first.id);
        // A new event is a new plenum; it takes the round when it comes first.
        calendar.insert(
            0,
            Occurrence {
                id: "extra".to_owned(),
                starts_at: at(2026, 11, 8, 18),
            },
        );
        assert_eq!(read(&mut conn, &calendar, now).added, 1);
        let extra = next(&conn).unwrap().unwrap();
        assert_eq!(extra.calendar_id.as_deref(), Some("extra"));
        assert_eq!(open_round(&conn).unwrap().unwrap().plenum_id, extra.id);
        // Every change is in the audit trail.
        let changes: i64 = conn
            .query_row(
                "SELECT count(*) FROM plenum_changes WHERE actor = 'calendar'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(changes as usize, ids(&conn).len() + 2);
    }

    #[test]
    fn a_move_past_another_plenum_hands_the_round_to_the_one_now_first() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_house_calendar(now);
        let [first, second] = <[Plenum; 2]>::try_from(upcoming(&conn, 2).unwrap()).unwrap();
        let round = open_round(&conn).unwrap().unwrap();
        // 15 Nov moves to 6 Dec, after 29 Nov.
        move_in_calendar(&mut conn, first.id, at(2026, 12, 6, 20), now);
        assert_eq!(next(&conn).unwrap().unwrap().id, second.id);
        assert_eq!(
            open_round(&conn).unwrap().unwrap(),
            Round {
                id: round.id,
                plenum_id: second.id
            }
        );
        assert_eq!(
            get(&conn, first.id).unwrap().unwrap().status,
            Status::Scheduled
        );
    }

    #[test]
    fn an_empty_calendar_cancels_nothing() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_house_calendar(now);
        let before = upcoming(&conn, 50).unwrap();
        let round = open_round(&conn).unwrap();
        assert!(!read(&mut conn, &[], now).changed());
        // Only yesterday's left is no different.
        let yesterday = [Occurrence {
            id: "yesterday".to_owned(),
            starts_at: at(2026, 11, 1, 19),
        }];
        assert!(!read(&mut conn, &yesterday, now).changed());
        assert_eq!(upcoming(&conn, 50).unwrap(), before);
        assert_eq!(open_round(&conn).unwrap(), round);
    }

    #[test]
    fn past_plenums_are_never_rewritten() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_house_calendar(now);
        let first = next(&conn).unwrap().unwrap();
        // Its evening is over and it waits to be completed. Gone from the
        // calendar, or moved to earlier that day: it stays as it is.
        let evening = at(2026, 11, 15, 22);
        let mut calendar = calendar_of(&conn);
        calendar[0].starts_at = at(2026, 11, 15, 17);
        read(&mut conn, &calendar, evening);
        calendar.remove(0);
        read(&mut conn, &calendar, evening);
        let p = get(&conn, first.id).unwrap().unwrap();
        assert_eq!(
            (p.starts_at, p.status),
            (first.starts_at, Status::Scheduled)
        );
        assert!(p.awaiting_completion(evening));
        assert_eq!(open_round(&conn).unwrap().unwrap().plenum_id, first.id);
        // Postponed to next week (it didn't happen): that one follows.
        calendar.insert(
            0,
            Occurrence {
                id: first.calendar_id.clone().unwrap(),
                starts_at: at(2026, 11, 22, 19),
            },
        );
        assert_eq!(read(&mut conn, &calendar, evening).moved, 1);
        assert_eq!(
            get(&conn, first.id).unwrap().unwrap().starts_at,
            at(2026, 11, 22, 19)
        );
        // An event the bot never knew of that is over already isn't added.
        calendar.push(Occurrence {
            id: "missed".to_owned(),
            starts_at: at(2026, 11, 15, 20),
        });
        assert_eq!(read(&mut conn, &calendar, evening).added, 0);
    }

    #[test]
    fn a_round_waiting_for_a_past_plenum_of_the_old_rules_stays_with_it() {
        // After the migration: rows of the old rule, none linked; the 18 Oct
        // plenum is over but nobody completed it, so the round is still its.
        let mut conn = memory_connection();
        conn.execute(
            "INSERT INTO plenum_rules (recurrence, effective_from, created_at)
             VALUES ('{}', '2026-10-14', 'then')",
            [],
        )
        .unwrap();
        for d in house_sundays(day(2026, 10, 14), day(2027, 2, 19)) {
            conn.execute(
                "INSERT INTO plenums (rule_id, rule_date, starts_at, original_starts_at, created_at)
                 VALUES (1, ?1, ?2, ?2, 'then')",
                params![
                    time::date_text(d),
                    time::to_text(zone().at(d, NaiveTime::from_hms_opt(19, 0, 0).unwrap()))
                ],
            )
            .unwrap();
        }
        let now = at(2026, 10, 22, 12);
        let round = ensure_round(&conn, at(2026, 10, 14, 10)).unwrap().unwrap();
        let oct18 = get(&conn, round.plenum_id).unwrap().unwrap();
        assert_eq!(oct18.starts_at, at(2026, 10, 18, 19));
        // The calendar moved every plenum to 20:00.
        let calendar: Vec<Occurrence> = house_calendar(now)
            .into_iter()
            .map(|o| Occurrence {
                starts_at: o.starts_at + Duration::hours(1),
                ..o
            })
            .collect();
        let synced = read(&mut conn, &calendar, now);
        assert_eq!(synced.adopted, calendar.len());
        assert_eq!(synced.moved, calendar.len());
        assert_eq!((synced.added, synced.retired), (0, 0));
        assert_eq!(open_round(&conn).unwrap().unwrap(), round);
        assert_eq!(get(&conn, oct18.id).unwrap().unwrap(), oct18);
        assert_eq!(next(&conn).unwrap().unwrap().id, oct18.id);
    }
}
