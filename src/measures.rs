//! 🧪 Measures: what the plenum decided to try.
//!
//! Stored with what they are about (topic, optionally a canonical problem
//! or metric), the concrete plenum that decided them, start and end dates
//! and a status. Results show them on a topic's timeline as *context* —
//! the bot never says a measure caused a change; interpreting is up to the
//! residents and the plenum.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{plenums, time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Proposed,
    Decided,
    Trying,
    Adopted,
    Stopped,
}

impl Status {
    pub const ALL: [Status; 5] = [
        Status::Proposed,
        Status::Decided,
        Status::Trying,
        Status::Adopted,
        Status::Stopped,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Proposed => "proposed",
            Status::Decided => "decided",
            Status::Trying => "trying",
            Status::Adopted => "adopted",
            Status::Stopped => "stopped",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Status::ALL
            .into_iter()
            .find(|st| st.as_str() == s.to_lowercase())
            .with_context(|| format!("{s:?} isn't proposed, decided, trying, adopted or stopped"))
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Proposed => "💭 Proposed",
            Status::Decided => "🗣️ Decided",
            Status::Trying => "🧪 Trying",
            Status::Adopted => "✅ Adopted",
            Status::Stopped => "⏹️ Stopped",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Measure {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub topic_id: Option<i64>,
    pub problem_id: Option<i64>,
    pub metric_id: Option<i64>,
    pub plenum_id: Option<i64>,
    pub starts_on: Option<NaiveDate>,
    pub ends_on: Option<NaiveDate>,
    pub status: Status,
}

const SELECT: &str = "SELECT id, title, description, topic_id, problem_id, metric_id, plenum_id,
        starts_on, ends_on, status FROM measures";

fn measure(r: &rusqlite::Row) -> rusqlite::Result<Measure> {
    let date = |i: usize| -> rusqlite::Result<Option<NaiveDate>> {
        Ok(r.get::<_, Option<String>>(i)?
            .and_then(|s| time::parse_date(&s).ok()))
    };
    let status: String = r.get(9)?;
    Ok(Measure {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        topic_id: r.get(3)?,
        problem_id: r.get(4)?,
        metric_id: r.get(5)?,
        plenum_id: r.get(6)?,
        starts_on: date(7)?,
        ends_on: date(8)?,
        status: Status::parse(&status).unwrap_or(Status::Proposed),
    })
}

#[cfg(test)]
pub fn get(conn: &Connection, id: i64) -> Result<Option<Measure>> {
    Ok(conn
        .query_row(&format!("{SELECT} WHERE id = ?1"), [id], measure)
        .optional()?)
}

/// Every measure, newest first.
pub fn all(conn: &Connection) -> Result<Vec<Measure>> {
    let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY id DESC"))?;
    let rows = stmt.query_map([], measure)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The measures of top-level topic `topic_root` that started, oldest first.
pub fn of_topic(conn: &Connection, topic_root: i64) -> Result<Vec<Measure>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE starts_on IS NOT NULL AND status <> 'proposed'
           AND topic_id IN (SELECT id FROM topics WHERE coalesce(parent_id, id) = ?1)
         ORDER BY starts_on, id"
    ))?;
    let rows = stmt.query_map([topic_root], measure)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub struct NewMeasure<'a> {
    pub title: &'a str,
    pub topic: &'a str,
    pub problem: Option<&'a str>,
    pub metric: Option<&'a str>,
    /// Decided at this plenum; default: the latest completed one.
    pub plenum_id: Option<i64>,
    pub starts_on: Option<NaiveDate>,
    pub status: Status,
}

fn id_of(conn: &Connection, table: &str, key: &str) -> Result<i64> {
    conn.query_row(
        &format!("SELECT id FROM {table} WHERE key = ?1"),
        [key],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| format!("No {} {key:?} (`!kk {table}`).", &table[..table.len() - 1]))
}

pub fn add(conn: &Connection, new: &NewMeasure, actor: &str, now: DateTime<Utc>) -> Result<i64> {
    if new.title.trim().is_empty() {
        bail!("A title, please.");
    }
    let topic = id_of(conn, "topics", new.topic)?;
    let problem = new
        .problem
        .map(|p| id_of(conn, "problems", p))
        .transpose()?;
    let metric = new.metric.map(|m| id_of(conn, "metrics", m)).transpose()?;
    let plenum = match new.plenum_id {
        Some(id) => Some(
            plenums::get(conn, id)?
                .with_context(|| format!("There is no plenum #{id}."))?
                .id,
        ),
        None => plenums::previous(conn)?.map(|p| p.id),
    };
    conn.execute(
        "INSERT INTO measures (title, topic_id, problem_id, metric_id, plenum_id, starts_on,
                               status, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            new.title.trim(),
            topic,
            problem,
            metric,
            plenum,
            new.starts_on.map(time::date_text),
            new.status.as_str(),
            actor,
            time::to_text(now)
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// One change to a measure.
pub enum Change<'a> {
    Status(Status),
    Title(&'a str),
    Description(&'a str),
    Starts(NaiveDate),
    Ends(Option<NaiveDate>),
    Plenum(i64),
}

pub fn change(conn: &Connection, id: i64, change: Change) -> Result<()> {
    let changed = match change {
        Change::Status(s) => conn.execute(
            "UPDATE measures SET status = ?2 WHERE id = ?1",
            params![id, s.as_str()],
        )?,
        Change::Title(t) if !t.trim().is_empty() => conn.execute(
            "UPDATE measures SET title = ?2 WHERE id = ?1",
            params![id, t.trim()],
        )?,
        Change::Title(_) => bail!("A title, please."),
        Change::Description(d) => conn.execute(
            "UPDATE measures SET description = ?2 WHERE id = ?1",
            params![id, d.trim()],
        )?,
        Change::Starts(d) => conn.execute(
            "UPDATE measures SET starts_on = ?2 WHERE id = ?1",
            params![id, time::date_text(d)],
        )?,
        Change::Ends(d) => conn.execute(
            "UPDATE measures SET ends_on = ?2 WHERE id = ?1",
            params![id, d.map(time::date_text)],
        )?,
        Change::Plenum(p) => {
            plenums::get(conn, p)?.with_context(|| format!("There is no plenum #{p}."))?;
            conn.execute(
                "UPDATE measures SET plenum_id = ?2 WHERE id = ?1",
                params![id, p],
            )?
        }
    };
    if changed == 0 {
        bail!("There's no measure #{id}.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lifecycle,
        plenums::tests::{at, with_default_rule},
    };

    #[test]
    fn measures_are_created_edited_and_belong_to_a_concrete_plenum() {
        let now = at(2026, 10, 5, 12);
        let mut conn = with_default_rule(now);
        let first = plenums::next(&conn).unwrap().unwrap();
        lifecycle::complete(&mut conn, first.id, "@admin:x", now).unwrap();
        let id = add(
            &conn,
            &NewMeasure {
                title: "Send cleaning reminders 24h earlier",
                topic: "cleaning",
                problem: Some("cleaning.skipped_turns"),
                metric: None,
                plenum_id: None,
                starts_on: Some(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()),
                status: Status::Trying,
            },
            "@admin:x",
            now,
        )
        .unwrap();
        let m = get(&conn, id).unwrap().unwrap();
        // Decided at the plenum that took place last.
        assert_eq!(m.plenum_id, Some(first.id));
        assert_eq!(m.status, Status::Trying);
        change(&conn, id, Change::Status(Status::Adopted)).unwrap();
        change(
            &conn,
            id,
            Change::Description("Reminder on Saturday morning"),
        )
        .unwrap();
        change(
            &conn,
            id,
            Change::Ends(Some(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap())),
        )
        .unwrap();
        let m = get(&conn, id).unwrap().unwrap();
        assert_eq!(m.status, Status::Adopted);
        assert_eq!(m.description, "Reminder on Saturday morning");
        let cleaning: i64 = conn
            .query_row("SELECT id FROM topics WHERE key = 'cleaning'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(of_topic(&conn, cleaning).unwrap().len(), 1);
        // Bad input is refused.
        assert!(change(&conn, 999, Change::Status(Status::Stopped)).is_err());
        assert!(change(&conn, id, Change::Plenum(999)).is_err());
        assert!(add(
            &conn,
            &NewMeasure {
                title: "x",
                topic: "nonsense",
                problem: None,
                metric: None,
                plenum_id: None,
                starts_on: None,
                status: Status::Proposed,
            },
            "@admin:x",
            now
        )
        .is_err());
    }
}
