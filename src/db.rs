//! SQLite access through `mxbot_common::sqlite`, as in the rest of the
//! fleet: one connection behind a mutex, used from `spawn_blocking`, and
//! numbered migrations applied once each, in order, every one in its own
//! transaction. A database from a newer image is refused.

use std::path::Path;

use anyhow::Result;
#[cfg(test)]
use mxbot_common::sqlite;
use mxbot_common::sqlite::Options;
#[cfg(test)]
use rusqlite::Connection;

pub use mxbot_common::sqlite::Db;

/// Migrations in order. Never edit one that has shipped: add the next.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/001_initial.sql")),
    (2, include_str!("../migrations/002_checkins.sql")),
    (3, include_str!("../migrations/003_plenum_changes.sql")),
    (
        4,
        include_str!("../migrations/004_results_ideas_measures.sql"),
    ),
    (5, include_str!("../migrations/005_posted_entries.sql")),
    (6, include_str!("../migrations/006_screens_outbox.sql")),
    (7, include_str!("../migrations/007_pending_screens.sql")),
    (8, include_str!("../migrations/008_echoed_screens.sql")),
    (9, include_str!("../migrations/009_calendar_plenums.sql")),
];

/// `secure_delete` asks SQLite to overwrite deleted content in the main
/// database file instead of leaving it in free pages. An extra measure,
/// not a guarantee of physical erasure: the WAL (until its next
/// checkpoint), the filesystem, the storage hardware, snapshots and backups
/// may still hold older data: see the README.
const OPTIONS: Options = Options {
    secure_delete: true,
};

pub async fn open(path: &Path) -> Result<Db> {
    Db::open(path, MIGRATIONS, OPTIONS).await
}

#[cfg(test)]
fn configure(conn: &Connection) -> Result<()> {
    sqlite::configure(conn, OPTIONS)
}

/// Apply every migration not applied yet. Returns how many ran.
#[cfg(test)]
pub fn migrate(conn: &mut Connection) -> Result<usize> {
    sqlite::migrate(conn, MIGRATIONS)
}

#[cfg(test)]
pub fn memory_connection() -> Connection {
    sqlite::memory_connection(MIGRATIONS).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_leave_a_consistent_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kk.sqlite");
        let mut conn = Connection::open(&path).unwrap();
        configure(&conn).unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        drop(conn);
        // Reopening (a restart) applies nothing again.
        let mut conn = Connection::open(&path).unwrap();
        configure(&conn).unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        let problems: Vec<String> = conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        let ok: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        let secure: i64 = conn
            .query_row("PRAGMA secure_delete", [], |r| r.get(0))
            .unwrap();
        assert_eq!(secure, 1);
    }

    #[test]
    fn an_older_image_refuses_a_database_a_newer_one_migrated() {
        let mut conn = memory_connection();
        let next = MIGRATIONS.len() as i64 + 1;
        conn.execute("INSERT INTO schema_migrations VALUES (?1, 'later')", [next])
            .unwrap();
        let error = migrate(&mut conn).unwrap_err().to_string();
        assert!(error.contains("newer version"), "{error}");
    }

    #[test]
    fn an_m1_database_upgrades_with_its_data() {
        // A database as M1 left it: only migration 1, with data in it.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute_batch(MIGRATIONS[0].1).unwrap();
        conn.execute("INSERT INTO schema_migrations VALUES (1, 'then')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO authorized_rooms (room_id, label, added_by, added_at) VALUES ('!h:x', 'House', '@a:x', 'then')",
            [],
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 1);
        // The room is still there, now with the new column's default.
        let central: i64 = conn
            .query_row(
                "SELECT central FROM authorized_rooms WHERE room_id = '!h:x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(central, 0);
        let templates: i64 = conn
            .query_row("SELECT count(*) FROM survey_templates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(templates, 1);
        assert_eq!(migrate(&mut conn).unwrap(), 0);
    }

    #[test]
    fn an_m2_database_with_a_moved_plenum_upgrades() {
        // Migrations 1 and 2, with a plenum moved once and a check-in
        // running until it.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
        )
        .unwrap();
        for (version, sql) in &MIGRATIONS[..2] {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations VALUES (?1, 'then')",
                [version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO plenums (id, starts_at, rescheduled, created_at)
                 VALUES (7, '2026-10-19T17:00:00Z', 1, 'then');
             INSERT INTO plenum_changes (plenum_id, action, detail, actor, at)
                 VALUES (7, 'move', '2026-10-18T17:00:00Z → 2026-10-19T17:00:00Z', '@a:x', 'then');
             INSERT INTO plenums (id, starts_at, created_at)
                 VALUES (8, '2026-11-01T18:00:00Z', 'then');
             INSERT INTO rounds (id, plenum_id, opened_at, status) VALUES (1, 7, 'then', 'open');
             INSERT INTO survey_runs (id, template_id, emoji, title, intro, target, plenum_id,
                 round_id, opens_at, closes_at, reminder_at, created_by, created_at)
                 VALUES (3, 1, '🧹', 'Cleaning', '', '{\"kind\":\"house\"}', 7, 1,
                 '2026-10-10T10:00:00Z', '2026-10-19T17:00:00Z', '2026-10-18T17:00:00Z', '@a:x', 'then');
             INSERT INTO checkin_messages (room_id, event_id, run_id) VALUES ('!dm:x', '$n', 3);",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len() - 2);
        let original = |id: i64| -> String {
            conn.query_row(
                "SELECT original_starts_at FROM plenums WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        // The first date, from the audit trail; unmoved ones: their date.
        assert_eq!(original(7), "2026-10-18T17:00:00Z");
        assert_eq!(original(8), "2026-11-01T18:00:00Z");
        let (mode, offset, remind): (String, i64, i64) = conn
            .query_row(
                "SELECT deadline_mode, deadline_offset_minutes, reminder_offset_minutes
                 FROM survey_runs WHERE id = 3",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (mode.as_str(), offset, remind),
            ("relative_to_plenum", 0, 1440)
        );
        // The 📊 of the notice still works; the room is gone from the table.
        let run: i64 = conn
            .query_row(
                "SELECT run_id FROM checkin_messages WHERE event_id = '$n'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(run, 3);
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('checkin_messages')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(columns, ["event_id", "run_id"]);
    }

    /// The live database as commit 64a767d leaves it while a survey runs: the
    /// starting rule, the plenums it generated, the 18 Oct one completed, an
    /// open round for 1 Nov with feedback to delete after it, a check-in
    /// closing when it starts, answers, an announcement. The new code's
    /// migration and first calendar reads must keep all of it on its rows.
    #[test]
    fn the_running_survey_survives_the_move_to_the_calendar() {
        use crate::plenums::{
            self,
            tests::{at, day, house_sundays, read, zone},
            Occurrence, Status,
        };
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
        )
        .unwrap();
        for (version, sql) in &MIGRATIONS[..8] {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations VALUES (?1, 'then')",
                [version],
            )
            .unwrap();
        }
        // ensure_rule + materialize on first start (14 Oct), and again today.
        conn.execute(
            "INSERT INTO plenum_rules (recurrence, effective_from, created_by, created_at)
             VALUES ('{\"kind\":\"monthly_nth\",\"weekday\":6,\"nths\":[1,3,5],\"time\":\"19:00:00\"}',
                     '2026-10-14', NULL, '2026-10-14T10:00:00Z')",
            [],
        )
        .unwrap();
        let seven = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
        for d in house_sundays(
            day(2026, 10, 14),
            day(2026, 10, 22) + chrono::Duration::days(120),
        ) {
            conn.execute(
                "INSERT OR IGNORE INTO plenums
                   (rule_id, rule_date, starts_at, original_starts_at, status, created_at)
                 VALUES (1, ?1, ?2, ?2, 'scheduled', '2026-10-14T10:00:00Z')",
                rusqlite::params![
                    crate::time::date_text(d),
                    crate::time::to_text(zone().at(d, seven))
                ],
            )
            .unwrap();
        }
        let row = |d: chrono::NaiveDate| -> i64 {
            conn.query_row(
                "SELECT id FROM plenums WHERE rule_date = ?1",
                [crate::time::date_text(d)],
                |r| r.get(0),
            )
            .unwrap()
        };
        let (oct18, nov1, nov15, nov29) = (
            row(day(2026, 10, 18)),
            row(day(2026, 11, 1)),
            row(day(2026, 11, 15)),
            row(day(2026, 11, 29)),
        );
        conn.execute_batch(&format!(
            "INSERT INTO rounds (id, plenum_id, opened_at, closed_at, status)
                 VALUES (1, {oct18}, '2026-10-14T10:00:00Z', '2026-10-18T20:00:00Z', 'closed');
             UPDATE plenums SET status = 'completed', completed_at = '2026-10-18T20:00:00Z'
                 WHERE id = {oct18};
             INSERT INTO rounds (id, plenum_id, opened_at, status)
                 VALUES (2, {nov1}, '2026-10-18T20:00:00Z', 'open');
             INSERT INTO submissions (id, round_id, topic_id, body, identity, retention,
                     delete_after_plenum_id, submitted_on)
                 VALUES (77, 2, (SELECT min(id) FROM topics), 'too loud', 'anonymous',
                         'delete_after_plenum', {nov1}, '2026-10-20'),
                        (78, 2, (SELECT min(id) FROM topics), 'thanks', 'anonymous',
                         'keep', NULL, '2026-10-21');
             INSERT INTO survey_runs (id, template_id, emoji, title, intro, target, plenum_id,
                     round_id, opens_at, closes_at, reminder_at, created_by, created_at,
                     deadline_mode, deadline_offset_minutes, reminder_offset_minutes, notified_at)
                 VALUES (5, 1, '🧹', 'Cleaning', '', '{{\"kind\":\"house\"}}', {nov1}, 2,
                         '2026-10-20T10:00:00Z', '2026-11-01T18:00:00Z', '2026-10-31T18:00:00Z',
                         '@a:x', '2026-10-20T10:00:00Z', 'relative_to_plenum', 0, 1440,
                         '2026-10-20T10:00:00Z');
             INSERT INTO responses (id, run_id, identity) VALUES ('r1', 5, 'anonymous'),
                                                                 ('r2', 5, 'anonymous');
             INSERT INTO checkin_messages (event_id, run_id) VALUES ('$notice', 5);
             UPDATE plenums SET announced_at = '2026-10-17T18:00:00Z' WHERE id = {oct18};
             INSERT INTO plenum_announcements (room_id, event_id, plenum_id, kind, body, for_starts_at)
                 VALUES ('!house:x', '$summary', {oct18}, 'summary', '📮', '2026-10-18T17:00:00Z');"
        ))
        .unwrap();
        let rows_before: i64 = conn
            .query_row("SELECT count(*) FROM plenums", [], |r| r.get(0))
            .unwrap();

        // The new image: migration 9, then the calendar.
        assert_eq!(migrate(&mut conn).unwrap(), 1);
        let now = at(2026, 10, 22, 12);
        // An empty (or not yet filled) calendar changes nothing.
        assert!(!read(&mut conn, &[], now).changed());
        // plenum-bot's calendar: 1 Nov as before; 15 Nov half an hour later;
        // an extra one on 22 Nov; 29 Nov not there; the rest as before.
        let calendar: Vec<Occurrence> = house_sundays(day(2026, 10, 21), day(2027, 2, 19))
            .into_iter()
            .filter(|d| *d != day(2026, 11, 29))
            .map(|d| Occurrence {
                id: format!("house#{d}"),
                starts_at: if d == day(2026, 11, 15) {
                    zone().at(d, chrono::NaiveTime::from_hms_opt(19, 30, 0).unwrap())
                } else {
                    zone().at(d, seven)
                },
            })
            .chain([Occurrence {
                id: "extra".to_owned(),
                starts_at: at(2026, 11, 22, 19),
            }])
            .collect();
        let synced = read(&mut conn, &calendar, now);
        assert_eq!(
            (synced.added, synced.moved, synced.retired, synced.cancelled),
            (1, 1, 1, 0),
            "{synced:?}"
        );
        assert_eq!(synced.adopted, calendar.len() - 1);
        // Read again: nothing to do.
        assert!(!read(&mut conn, &calendar, now).changed());

        // The open round, its plenum and everything attached: unchanged.
        let round = plenums::open_round(&conn).unwrap().unwrap();
        assert_eq!((round.id, round.plenum_id), (2, nov1));
        let p = plenums::get(&conn, nov1).unwrap().unwrap();
        assert_eq!(p.calendar_id.as_deref(), Some("house#2026-11-01"));
        assert_eq!(
            (p.status, p.starts_at),
            (Status::Scheduled, at(2026, 11, 1, 19))
        );
        assert!(!p.rescheduled);
        assert_eq!(plenums::next(&conn).unwrap().unwrap().id, nov1);
        let one = |sql: &str| -> String {
            conn.query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0))
                .map(|v| format!("{v:?}"))
                .unwrap()
        };
        assert_eq!(
            one("SELECT group_concat(id || ':' || round_id || ':' || coalesce(delete_after_plenum_id, '-')) FROM submissions"),
            format!("Text(\"77:2:{nov1},78:2:-\")")
        );
        assert_eq!(
            one("SELECT plenum_id || ' ' || round_id || ' ' || status || ' ' || closes_at || ' ' || reminder_at FROM survey_runs WHERE id = 5"),
            format!("Text(\"{nov1} 2 open 2026-11-01T18:00:00Z 2026-10-31T18:00:00Z\")")
        );
        assert_eq!(
            one("SELECT count(*) FROM responses WHERE run_id = 5"),
            "Integer(2)"
        );
        assert_eq!(
            one("SELECT run_id FROM checkin_messages WHERE event_id = '$notice'"),
            "Integer(5)"
        );
        assert_eq!(
            one("SELECT plenum_id FROM plenum_announcements WHERE event_id = '$summary'"),
            format!("Integer({oct18})")
        );
        // The past: untouched.
        let past = plenums::get(&conn, oct18).unwrap().unwrap();
        assert_eq!((past.status, past.calendar_id), (Status::Completed, None));
        // 15 Nov: the same row, moved to the calendar's time.
        let moved = plenums::get(&conn, nov15).unwrap().unwrap();
        assert_eq!(moved.calendar_id.as_deref(), Some("house#2026-11-15"));
        assert_eq!(
            moved.starts_at,
            zone().at(
                day(2026, 11, 15),
                chrono::NaiveTime::from_hms_opt(19, 30, 0).unwrap()
            )
        );
        assert_eq!(moved.original_starts_at, at(2026, 11, 15, 19));
        // 29 Nov was only the old schedule: gone. 22 Nov is new.
        assert!(plenums::get(&conn, nov29).unwrap().is_none());
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM plenums"), rows_before);
        assert_eq!(
            count("SELECT count(*) FROM plenums WHERE calendar_id IS NULL"),
            1
        );
        assert_eq!(count("SELECT count(*) FROM plenum_rules"), 1);
        assert_eq!(count("SELECT count(*) FROM pragma_foreign_key_check"), 0);
    }

    #[test]
    fn the_starting_catalog_is_there() {
        let conn = memory_connection();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count("SELECT count(*) FROM locations WHERE is_floor = 1"),
            4
        );
        assert_eq!(
            count("SELECT count(*) FROM topics WHERE parent_id IS NULL"),
            8
        );
        assert_eq!(
            count(
                "SELECT count(*) FROM topics t JOIN topics p ON t.parent_id = p.id \
                 WHERE p.key = 'cleaning'"
            ),
            7
        );
        assert_eq!(
            count(
                "SELECT count(*) FROM topics t JOIN problems p ON t.problem_id = p.id \
                 WHERE p.key = 'cleaning.skipped_turns'"
            ),
            1
        );
    }

    /// The code this module had before `mxbot_common::sqlite`, kept as it
    /// was so the test below builds databases exactly as production has them.
    fn old_open(path: &Path, upto: usize) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA secure_delete = ON;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )
        .unwrap();
        // Its indentation is part of the stored schema.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (\n             \
             version     INTEGER PRIMARY KEY,\n             \
             applied_at  TEXT NOT NULL\n         );",
        )
        .unwrap();
        let mut conn = conn;
        for (version, sql) in &MIGRATIONS[..upto] {
            let tx = conn.transaction().unwrap();
            tx.execute_batch(sql).unwrap();
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                rusqlite::params![version, crate::time::now_text()],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        conn
    }

    /// Everything that makes up the schema, and the applied migrations.
    fn shape(conn: &Connection) -> (Vec<String>, Vec<i64>, i64) {
        let schema = conn
            .prepare("SELECT type || ' ' || name || ': ' || ifnull(sql, '') FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let versions = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let user_version = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        (schema, versions, user_version)
    }

    #[tokio::test]
    async fn databases_the_old_code_made_open_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let reference = shape(&old_open(&dir.path().join("old.sqlite"), MIGRATIONS.len()));
        assert_eq!(reference.1, (1..=9).collect::<Vec<i64>>());
        assert_eq!(reference.2, 0);
        // A fresh database ends up as the old code made it.
        let db = open(&dir.path().join("fresh.sqlite")).await.unwrap();
        assert_eq!(db.with(|c| Ok(shape(c))).unwrap(), reference);
        // One at every older version upgrades to it, with its data and its
        // migration history; one already current is not touched.
        for upto in 1..=MIGRATIONS.len() {
            let path = dir.path().join(format!("v{upto}.sqlite"));
            let old = old_open(&path, upto);
            old.execute(
                "INSERT INTO authorized_rooms (room_id, label, added_by, added_at) \
                 VALUES ('!h:x', 'House', '@a:x', 'then')",
                [],
            )
            .unwrap();
            let history = |c: &Connection| -> Vec<(i64, String)> {
                c.prepare("SELECT version, applied_at FROM schema_migrations ORDER BY version")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            };
            let before = history(&old);
            drop(old);
            let db = open(&path).await.unwrap();
            db.with(|c| {
                assert_eq!(shape(c), reference, "from version {upto}");
                assert_eq!(history(c)[..upto], before[..]);
                let label: String = c.query_row(
                    "SELECT label FROM authorized_rooms WHERE room_id = '!h:x'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(label, "House");
                Ok(())
            })
            .unwrap();
        }
    }
}
