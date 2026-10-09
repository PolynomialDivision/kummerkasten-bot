//! SQLite access, as in the rest of the fleet: one connection behind a
//! mutex, used from `spawn_blocking`, and numbered migrations applied once
//! each, in order, every one in its own transaction.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// Migrations in order. Never edit one that has shipped: add the next.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/001_initial.sql")),
    (2, include_str!("../migrations/002_checkins.sql")),
    (3, include_str!("../migrations/003_plenum_changes.sql")),
    (
        4,
        include_str!("../migrations/004_results_ideas_measures.sql"),
    ),
];

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let mut conn = Connection::open(&path)
                .with_context(|| format!("Opening SQLite at {}", path.display()))?;
            configure(&conn)?;
            migrate(&mut conn)?;
            Ok(conn)
        })
        .await??;
        Ok(Self::from_connection(conn))
    }

    pub fn from_connection(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Run `f` on the connection, off the async runtime.
    pub async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut conn)
        })
        .await?
    }

    /// Run `f` synchronously (tests and code already off the runtime).
    #[cfg(test)]
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }
}

/// `secure_delete` asks SQLite to overwrite deleted content in the main
/// database file instead of leaving it in free pages. An extra measure,
/// not a guarantee of physical erasure: the WAL (until its next
/// checkpoint), the filesystem, the storage hardware, snapshots and backups
/// may still hold older data: see the README.
fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA secure_delete = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )
    .context("Configuring SQLite")
}

/// Apply every migration not applied yet. Returns how many ran.
pub fn migrate(conn: &mut Connection) -> Result<usize> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version     INTEGER PRIMARY KEY,
             applied_at  TEXT NOT NULL
         );",
    )?;
    let mut applied = 0;
    for (version, sql) in MIGRATIONS {
        let done: Option<i64> = conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE version = ?1",
                [version],
                |r| r.get(0),
            )
            .optional()?;
        if done.is_some() {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("Applying migration {version}"))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, crate::time::now_text()],
        )?;
        tx.commit()?;
        tracing::info!(version, "Applied database migration");
        applied += 1;
    }
    Ok(applied)
}

#[cfg(test)]
pub fn memory_connection() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    migrate(&mut conn).unwrap();
    conn
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
}
