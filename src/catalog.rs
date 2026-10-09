//! Topics, locations and canonical problems: database rows, not code.
//! Admins add, rename and retire them; retired rows stay so history keeps
//! its labels.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: i64,
    pub key: String,
    pub parent_id: Option<i64>,
    pub emoji: String,
    pub label: String,
    /// Topics: the canonical problem a subtopic names.
    pub problem_id: Option<i64>,
    /// Topics: "💡 Idea / suggestion".
    pub is_idea: bool,
    /// Locations: residents may say they live there.
    pub is_floor: bool,
}

impl Node {
    pub fn title(&self) -> String {
        format!("{} {}", self.emoji, self.label)
    }
}

/// The active topics and locations.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub topics: Vec<Node>,
    pub locations: Vec<Node>,
}

impl Catalog {
    pub fn load(conn: &Connection) -> Result<Self> {
        let topics = nodes(
            conn,
            "SELECT id, key, parent_id, emoji, label, problem_id, is_idea, 0
             FROM topics WHERE active = 1 ORDER BY sort, id",
        )?;
        let locations = nodes(
            conn,
            "SELECT id, key, parent_id, emoji, label, NULL, 0, is_floor
             FROM locations WHERE active = 1 ORDER BY sort, id",
        )?;
        Ok(Self { topics, locations })
    }

    pub fn topic(&self, id: i64) -> Option<&Node> {
        self.topics.iter().find(|n| n.id == id)
    }

    pub fn location(&self, id: i64) -> Option<&Node> {
        self.locations.iter().find(|n| n.id == id)
    }

    pub fn topic_children(&self, parent: Option<i64>) -> Vec<&Node> {
        children(&self.topics, parent)
    }

    pub fn location_children(&self, parent: Option<i64>) -> Vec<&Node> {
        children(&self.locations, parent)
    }

    pub fn floors(&self) -> Vec<&Node> {
        self.locations.iter().filter(|n| n.is_floor).collect()
    }

    /// "🧹 Cleaning › 🙋 People skipping their turn".
    pub fn topic_path(&self, id: i64) -> String {
        path(&self.topics, id)
    }

    pub fn location_path(&self, id: i64) -> String {
        path(&self.locations, id)
    }
}

fn children(nodes: &[Node], parent: Option<i64>) -> Vec<&Node> {
    nodes.iter().filter(|n| n.parent_id == parent).collect()
}

fn path(nodes: &[Node], id: i64) -> String {
    let mut parts = Vec::new();
    let mut current = nodes.iter().find(|n| n.id == id);
    while let Some(node) = current {
        parts.push(node.title());
        current = node
            .parent_id
            .and_then(|p| nodes.iter().find(|n| n.id == p));
        if parts.len() > 8 {
            break;
        }
    }
    parts.reverse();
    parts.join(" › ")
}

fn nodes(conn: &Connection, sql: &str) -> Result<Vec<Node>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(Node {
            id: r.get(0)?,
            key: r.get(1)?,
            parent_id: r.get(2)?,
            emoji: r.get(3)?,
            label: r.get(4)?,
            problem_id: r.get(5)?,
            is_idea: r.get(6)?,
            is_floor: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ── Admin edits ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Topic,
    Location,
    Problem,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Kind::Topic => "topics",
            Kind::Location => "locations",
            Kind::Problem => "problems",
        }
    }
}

fn id_of(conn: &Connection, kind: Kind, key: &str) -> Result<i64> {
    conn.query_row(
        &format!("SELECT id FROM {} WHERE key = ?1", kind.table()),
        [key],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| {
        format!(
            "There is no {} «{key}».",
            kind.table().trim_end_matches('s')
        )
    })
}

pub(crate) fn check_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 64
        || !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        bail!("Keys are short and lower case, like `kitchen.dishwasher`.");
    }
    Ok(())
}

/// Sort position after the last sibling (topics and locations).
fn next_sort(conn: &Connection, kind: Kind, parent: Option<i64>) -> Result<i64> {
    let sql = format!(
        "SELECT coalesce(max(sort), 0) + 10 FROM {} WHERE parent_id IS ?1",
        kind.table()
    );
    Ok(conn.query_row(&sql, [parent], |r| r.get(0))?)
}

/// `parent`: a topic key or `None` for a new top-level topic.
pub fn add_topic(
    conn: &Connection,
    parent: Option<&str>,
    key: &str,
    emoji: &str,
    label: &str,
    problem: Option<&str>,
) -> Result<()> {
    check_key(key)?;
    let parent_id = parent.map(|p| id_of(conn, Kind::Topic, p)).transpose()?;
    let problem_id = problem.map(|p| id_of(conn, Kind::Problem, p)).transpose()?;
    let sort = next_sort(conn, Kind::Topic, parent_id)?;
    conn.execute(
        "INSERT INTO topics (key, parent_id, emoji, label, problem_id, sort)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key, parent_id, emoji, label, problem_id, sort],
    )
    .with_context(|| format!("«{key}» exists already."))?;
    Ok(())
}

pub fn add_location(
    conn: &Connection,
    parent: Option<&str>,
    key: &str,
    emoji: &str,
    label: &str,
    is_floor: bool,
) -> Result<()> {
    check_key(key)?;
    let parent_id = parent.map(|p| id_of(conn, Kind::Location, p)).transpose()?;
    let sort = next_sort(conn, Kind::Location, parent_id)?;
    conn.execute(
        "INSERT INTO locations (key, parent_id, emoji, label, is_floor, sort)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key, parent_id, emoji, label, is_floor, sort],
    )
    .with_context(|| format!("«{key}» exists already."))?;
    Ok(())
}

pub fn add_problem(conn: &Connection, key: &str, topic: &str, label: &str) -> Result<()> {
    check_key(key)?;
    let topic_id = id_of(conn, Kind::Topic, topic)?;
    conn.execute(
        "INSERT INTO problems (key, topic_id, label, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![key, topic_id, label, crate::time::now_text()],
    )
    .with_context(|| format!("«{key}» exists already."))?;
    Ok(())
}

/// New wording; the key (and everything pointing at the row) stays.
pub fn rename(
    conn: &Connection,
    kind: Kind,
    key: &str,
    emoji: Option<&str>,
    label: &str,
) -> Result<()> {
    let id = id_of(conn, kind, key)?;
    conn.execute(
        &format!("UPDATE {} SET label = ?2 WHERE id = ?1", kind.table()),
        params![id, label],
    )?;
    if let (Some(emoji), true) = (emoji, kind != Kind::Problem) {
        conn.execute(
            &format!("UPDATE {} SET emoji = ?2 WHERE id = ?1", kind.table()),
            params![id, emoji],
        )?;
    }
    Ok(())
}

/// Retire or bring back. Never deletes: history points at these rows.
pub fn set_active(conn: &Connection, kind: Kind, key: &str, active: bool) -> Result<()> {
    let id = id_of(conn, kind, key)?;
    conn.execute(
        &format!("UPDATE {} SET active = ?2 WHERE id = ?1", kind.table()),
        params![id, active],
    )?;
    Ok(())
}

/// "key: emoji label" lines, retired ones marked.
pub fn listing(conn: &Connection, kind: Kind) -> Result<String> {
    let sql = match kind {
        Kind::Problem => "SELECT p.key, '', p.label, p.active, coalesce(t.key, '')
                          FROM problems p LEFT JOIN topics t ON t.id = p.topic_id
                          ORDER BY p.key"
            .to_owned(),
        _ => format!(
            "SELECT n.key, n.emoji, n.label, n.active, coalesce(p.key, '')
             FROM {0} n LEFT JOIN {0} p ON p.id = n.parent_id
             ORDER BY coalesce(p.sort, n.sort), p.id IS NOT NULL, n.sort, n.id",
            kind.table()
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, bool>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, emoji, label, active, parent) = row?;
        let indent = if parent.is_empty() || kind == Kind::Problem {
            ""
        } else {
            "    "
        };
        let retired = if active { "" } else { " · 🗄️ retired" };
        let topic = if kind == Kind::Problem && !parent.is_empty() {
            format!(" · {parent}")
        } else {
            String::new()
        };
        out.push(format!("{indent}`{key}`: {emoji} {label}{topic}{retired}").replace(":  ", ": "));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_connection;

    #[test]
    fn catalog_is_data_and_retiring_keeps_history() {
        let conn = memory_connection();
        let cat = Catalog::load(&conn).unwrap();
        let cleaning = cat.topics.iter().find(|t| t.key == "cleaning").unwrap();
        let skipped = cat
            .topics
            .iter()
            .find(|t| t.key == "cleaning.skipped_turns")
            .unwrap();
        assert_eq!(skipped.parent_id, Some(cleaning.id));
        assert!(skipped.problem_id.is_some());
        assert_eq!(
            cat.topic_path(skipped.id),
            "🧹 Cleaning › 🙋 People skipping their turn"
        );
        assert_eq!(cat.floors().len(), 4);
        let kitchens = cat.locations.iter().find(|l| l.key == "kitchens").unwrap();
        assert_eq!(cat.location_children(Some(kitchens.id)).len(), 4);

        // A new subtopic naming a new problem: no code involved.
        add_problem(
            &conn,
            "kitchen.dishwasher",
            "kitchen",
            "Dishwasher not emptied",
        )
        .unwrap();
        add_topic(
            &conn,
            Some("kitchen"),
            "kitchen.dishwasher",
            "🍽️",
            "Dishwasher",
            Some("kitchen.dishwasher"),
        )
        .unwrap();
        assert!(add_topic(&conn, None, "kitchen", "🍳", "Again", None).is_err());
        assert!(add_topic(&conn, None, "Bad Key", "🍳", "x", None).is_err());
        let cat = Catalog::load(&conn).unwrap();
        let kitchen = cat.topics.iter().find(|t| t.key == "kitchen").unwrap();
        assert_eq!(cat.topic_children(Some(kitchen.id)).len(), 1);

        // Rewording keeps the row; retiring hides it but keeps it.
        rename(&conn, Kind::Topic, "kitchen.dishwasher", None, "Dishes").unwrap();
        set_active(&conn, Kind::Topic, "kitchen.dishwasher", false).unwrap();
        let cat = Catalog::load(&conn).unwrap();
        assert!(cat.topic_children(Some(kitchen.id)).is_empty());
        let label: String = conn
            .query_row(
                "SELECT label FROM topics WHERE key = 'kitchen.dishwasher'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(label, "Dishes");
        assert!(listing(&conn, Kind::Topic).unwrap().contains("retired"));
        assert!(listing(&conn, Kind::Problem)
            .unwrap()
            .contains("`kitchen.dishwasher`: Dishwasher not emptied · kitchen"));
    }
}
