//! 💡 Ideas: shared on purpose, supported and commented without names.
//!
//! An idea only exists because its author chose 🌍 *Add to Ideas* (from
//! feedback or a check-in answer that asks what to change) — never
//! automatically. It is a separate public object: nothing links it to the
//! author, the submission or the response it came from, and their own
//! identity and retention choices stay as they were.
//!
//! 👍 Support and 💬 comments are keyed per account with
//! `HMAC(secret, idea ‖ Matrix ID)`: one each, no time, no row order. The
//! database can tell *whether* an account supports an idea (to stop doubles
//! and to show "✅ you support this" to that account), never list who does.
//!
//! "A similar idea already exists" is deterministic: normalized keywords
//! (overlap), the same canonical problem. It only suggests — residents
//! decide, nothing is merged.

use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};

use crate::{secrets::Secrets, time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Suggested,
    Discussing,
    Trying,
    Implemented,
    Rejected,
    Archived,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Suggested,
        Status::Discussing,
        Status::Trying,
        Status::Implemented,
        Status::Rejected,
        Status::Archived,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Suggested => "suggested",
            Status::Discussing => "discussing",
            Status::Trying => "trying",
            Status::Implemented => "implemented",
            Status::Rejected => "rejected",
            Status::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Status::ALL
            .into_iter()
            .find(|st| st.as_str() == s.to_lowercase())
            .with_context(|| {
                format!(
                    "{s:?} isn't suggested, discussing, trying, implemented, rejected or archived"
                )
            })
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Suggested => "💭 Suggested",
            Status::Discussing => "🗣️ Discussing",
            Status::Trying => "🧪 Trying",
            Status::Implemented => "✅ Implemented",
            Status::Rejected => "❌ Rejected",
            Status::Archived => "🗄️ Archived",
        }
    }

    /// On the board (rejected and archived ones are only counted there).
    pub fn active(self) -> bool {
        !matches!(self, Status::Rejected | Status::Archived)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Feedback,
    Checkin,
    Admin,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::Feedback => "feedback",
            Origin::Checkin => "checkin",
            Origin::Admin => "admin",
        }
    }
}

/// An idea as the board and its page show it.
#[derive(Clone, Debug, PartialEq)]
pub struct Idea {
    pub id: i64,
    pub title: String,
    pub body: Option<String>,
    pub topic_id: Option<i64>,
    pub problem_id: Option<i64>,
    pub status: Status,
    pub support: i64,
    pub comments: i64,
    pub created_on: NaiveDate,
}

const SELECT: &str = "SELECT i.id, i.title, i.body, i.topic_id, i.problem_id, i.status,
        (SELECT count(*) FROM idea_supports s WHERE s.idea_id = i.id),
        (SELECT count(*) FROM idea_comments c WHERE c.idea_id = i.id),
        i.created_on
     FROM ideas i";

fn idea(r: &rusqlite::Row) -> rusqlite::Result<(Idea, String)> {
    let status: String = r.get(5)?;
    let created: String = r.get(8)?;
    Ok((
        Idea {
            id: r.get(0)?,
            title: r.get(1)?,
            body: r.get(2)?,
            topic_id: r.get(3)?,
            problem_id: r.get(4)?,
            status: Status::parse(&status).unwrap_or(Status::Suggested),
            support: r.get(6)?,
            comments: r.get(7)?,
            created_on: NaiveDate::MIN,
        },
        created,
    ))
}

fn with_date((mut idea, created): (Idea, String)) -> Result<Idea> {
    idea.created_on = time::parse_date(&created)?;
    Ok(idea)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Idea>> {
    conn.query_row(&format!("{SELECT} WHERE i.id = ?1"), [id], idea)
        .optional()?
        .map(with_date)
        .transpose()
}

/// The board: active ideas, most supported first (then newest).
pub fn board(conn: &Connection, limit: usize) -> Result<Vec<Idea>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE i.status NOT IN ('rejected', 'archived')
         ORDER BY 7 DESC, i.created_on DESC, i.id DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map([limit as i64], idea)?;
    rows.map(|r| with_date(r?)).collect()
}

/// Every idea, newest first (for admins).
pub fn all(conn: &Connection) -> Result<Vec<Idea>> {
    let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY i.id DESC"))?;
    let rows = stmt.query_map([], idea)?;
    rows.map(|r| with_date(r?)).collect()
}

/// Rejected or archived (counted under the board).
pub fn inactive_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM ideas WHERE status IN ('rejected', 'archived')",
        [],
        |r| r.get(0),
    )?)
}

/// The latest comments on `id` (no author: there is none).
pub fn comments(conn: &Connection, id: i64, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT body FROM idea_comments WHERE idea_id = ?1
         ORDER BY created_on DESC, body LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![id, limit as i64], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ── Creating ────────────────────────────────────────────────────────────────

pub struct NewIdea<'a> {
    pub text: &'a str,
    pub topic_id: Option<i64>,
    pub problem_id: Option<i64>,
    pub origin: Origin,
    pub round_id: Option<i64>,
}

/// A one-line title from what someone wrote: its first line, at most 80
/// characters (cut at a word).
pub fn title_of(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if first.chars().count() <= 80 {
        return first;
    }
    let mut out = String::new();
    for word in first.split(' ') {
        if out.chars().count() + word.chars().count() + 1 > 78 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        out = first.chars().take(78).collect();
    }
    format!("{out}…")
}

/// Share a new idea. Returns its ID.
pub fn create(conn: &Connection, new: &NewIdea, today: NaiveDate) -> Result<i64> {
    let text = new.text.trim();
    if text.is_empty() {
        bail!("An empty idea");
    }
    let title = title_of(text);
    let body = (text != title).then_some(text);
    conn.execute(
        "INSERT INTO ideas (title, body, topic_id, problem_id, keywords, origin, round_id, created_on)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            title,
            body,
            new.topic_id,
            new.problem_id,
            keywords(text).into_iter().collect::<Vec<_>>().join(" "),
            new.origin.as_str(),
            new.round_id,
            time::date_text(today)
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

// ── Support and comments (one per account, never a list of names) ──────────

/// 👍 — `true` if it wasn't supported by this account yet.
pub fn support(conn: &Connection, secrets: &Secrets, id: i64, mxid: &str) -> Result<bool> {
    usable(conn, id)?;
    Ok(conn.execute(
        "INSERT OR IGNORE INTO idea_supports (idea_id, supporter_key) VALUES (?1, ?2)",
        params![id, secrets.idea_key(id, mxid)],
    )? == 1)
}

/// Take one's 👍 back.
pub fn withdraw(conn: &Connection, secrets: &Secrets, id: i64, mxid: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM idea_supports WHERE idea_id = ?1 AND supporter_key = ?2",
        params![id, secrets.idea_key(id, mxid)],
    )? == 1)
}

/// Whether this account supports `id` (shown to that account only).
pub fn supports(conn: &Connection, secrets: &Secrets, id: i64, mxid: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM idea_supports WHERE idea_id = ?1 AND supporter_key = ?2",
            params![id, secrets.idea_key(id, mxid)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// 💬 — one comment per account and idea; writing again replaces it.
pub fn comment(
    conn: &Connection,
    secrets: &Secrets,
    id: i64,
    mxid: &str,
    text: &str,
    today: NaiveDate,
) -> Result<()> {
    usable(conn, id)?;
    let text = text.trim();
    if text.is_empty() {
        bail!("An empty comment");
    }
    conn.execute(
        "INSERT INTO idea_comments (idea_id, commenter_key, body, created_on)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (idea_id, commenter_key) DO UPDATE SET body = ?3, created_on = ?4",
        params![id, secrets.idea_key(id, mxid), text, time::date_text(today)],
    )?;
    Ok(())
}

fn usable(conn: &Connection, id: i64) -> Result<()> {
    match get(conn, id)? {
        Some(idea) if idea.status.active() => Ok(()),
        Some(_) => bail!("That idea is closed."),
        None => bail!("That idea is gone."),
    }
}

// ── Similar ideas ───────────────────────────────────────────────────────────

/// Words that say little about what an idea is about (English and German).
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "could", "do", "for", "from", "have",
    "i", "if", "in", "is", "it", "maybe", "more", "my", "of", "on", "or", "our", "please",
    "should", "so", "some", "that", "the", "there", "this", "to", "us", "we", "what", "when",
    "with", "would", "you", "all", "every", "per", "than", "then", "also", "just", "very", "der",
    "die", "das", "ein", "eine", "einen", "und", "oder", "mit", "für", "fuer", "von", "zu", "im",
    "in", "ist", "sind", "wir", "ich", "es", "auf", "an", "am", "bei", "dass", "den", "dem", "des",
    "nicht", "mehr", "mal", "auch", "bitte", "sollten", "könnten", "koennten", "jede", "jeden",
    "jedes", "pro", "als", "wie", "wenn",
];

/// The words an idea is about: lower case, letters and digits, no stop
/// words, a plural or German ending trimmed so "bathroom"/"bathrooms" and
/// "Bad"/"Bäder" are closer.
pub fn keywords(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 2 && !STOPWORDS.contains(w))
        .map(|w| {
            let w = w
                .replace('ä', "a")
                .replace('ö', "o")
                .replace('ü', "u")
                .replace('ß', "ss");
            let mut w = w.as_str();
            for ending in ["ies", "es", "s", "en", "er", "e", "n"] {
                if w.chars().count() > ending.len() + 3 {
                    if let Some(stem) = w.strip_suffix(ending) {
                        w = stem;
                        break;
                    }
                }
            }
            w.to_owned()
        })
        .collect()
}

/// How much two keyword sets overlap (Jaccard, 0–1).
pub fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

/// The active idea most like `text` (in topic `topic_root` / about
/// `problem`), if one is close enough to mention: half the words in
/// common, or a third and the same canonical problem.
pub fn similar(
    conn: &Connection,
    text: &str,
    topic_root: Option<i64>,
    problem: Option<i64>,
) -> Result<Option<Idea>> {
    let mine = keywords(text);
    let mut stmt = conn.prepare(
        "SELECT i.id, i.keywords, i.problem_id,
                (SELECT coalesce(t.parent_id, t.id) FROM topics t WHERE t.id = i.topic_id)
         FROM ideas i WHERE i.status NOT IN ('rejected', 'archived')",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let mut best: Option<(f64, i64)> = None;
    for row in rows {
        let (id, words, their_problem, their_root) = row?;
        let theirs: BTreeSet<String> = words.split_whitespace().map(str::to_owned).collect();
        let score = overlap(&mine, &theirs);
        let same_problem = problem.is_some() && problem == their_problem;
        let same_topic = topic_root.is_some() && topic_root == their_root;
        let close = score >= 0.5 || (score >= 0.3 && (same_problem || same_topic));
        if close && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, id));
        }
    }
    match best {
        Some((_, id)) => get(conn, id),
        None => Ok(None),
    }
}

// ── For admins ──────────────────────────────────────────────────────────────

pub fn set_status(conn: &Connection, id: i64, status: Status) -> Result<()> {
    if conn.execute(
        "UPDATE ideas SET status = ?2 WHERE id = ?1",
        params![id, status.as_str()],
    )? == 0
    {
        bail!("There's no idea #{id}.");
    }
    Ok(())
}

pub fn rename(conn: &Connection, id: i64, title: &str) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        bail!("A title, please.");
    }
    if conn.execute(
        "UPDATE ideas SET title = ?2, keywords = ?3 WHERE id = ?1",
        params![
            id,
            title,
            keywords(title).into_iter().collect::<Vec<_>>().join(" ")
        ],
    )? == 0
    {
        bail!("There's no idea #{id}.");
    }
    Ok(())
}

pub fn set_topic(conn: &Connection, id: i64, topic_key: &str) -> Result<()> {
    let topic: i64 = conn
        .query_row("SELECT id FROM topics WHERE key = ?1", [topic_key], |r| {
            r.get(0)
        })
        .optional()?
        .with_context(|| format!("No topic {topic_key:?} (`!kk topics`)."))?;
    if conn.execute(
        "UPDATE ideas SET topic_id = ?2 WHERE id = ?1",
        params![id, topic],
    )? == 0
    {
        bail!("There's no idea #{id}.");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::memory_connection;

    pub fn topic(conn: &Connection, key: &str) -> i64 {
        conn.query_row("SELECT id FROM topics WHERE key = ?1", [key], |r| r.get(0))
            .unwrap()
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()
    }

    pub fn share(conn: &Connection, text: &str, topic_key: &str) -> i64 {
        create(
            conn,
            &NewIdea {
                text,
                topic_id: Some(topic(conn, topic_key)),
                problem_id: None,
                origin: Origin::Feedback,
                round_id: None,
            },
            day(),
        )
        .unwrap()
    }

    #[test]
    fn support_is_once_per_account_and_never_lists_who() {
        let conn = memory_connection();
        let secrets = Secrets::from_master(&[3; 32]);
        let id = share(&conn, "Clean the bathrooms twice per week", "cleaning");
        assert!(support(&conn, &secrets, id, "@a:x").unwrap());
        assert!(
            !support(&conn, &secrets, id, "@a:x").unwrap(),
            "no double support"
        );
        assert!(support(&conn, &secrets, id, "@b:x").unwrap());
        assert_eq!(get(&conn, id).unwrap().unwrap().support, 2);
        assert!(supports(&conn, &secrets, id, "@a:x").unwrap());
        assert!(!supports(&conn, &secrets, id, "@c:x").unwrap());
        // Nothing in the database names a supporter.
        let stored: Vec<Vec<u8>> = conn
            .prepare("SELECT supporter_key FROM idea_supports")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for key in stored {
            assert!(!key.windows(4).any(|w| w == b"@a:x" || w == b"@b:x"));
        }
        let names: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('idea_supports') WHERE name LIKE '%mxid%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(names, 0);
        assert!(withdraw(&conn, &secrets, id, "@a:x").unwrap());
        assert_eq!(get(&conn, id).unwrap().unwrap().support, 1);
        // A closed idea takes no support.
        set_status(&conn, id, Status::Archived).unwrap();
        assert!(support(&conn, &secrets, id, "@c:x").is_err());
    }

    #[test]
    fn one_comment_per_account_replaced_when_written_again() {
        let conn = memory_connection();
        let secrets = Secrets::from_master(&[3; 32]);
        let id = share(&conn, "Earlier cleaning reminders", "cleaning");
        comment(&conn, &secrets, id, "@a:x", "Yes please", day()).unwrap();
        comment(
            &conn,
            &secrets,
            id,
            "@a:x",
            "Yes please — Saturday morning",
            day(),
        )
        .unwrap();
        comment(&conn, &secrets, id, "@b:x", "Fine by me", day()).unwrap();
        assert_eq!(get(&conn, id).unwrap().unwrap().comments, 2);
        assert!(comments(&conn, id, 5)
            .unwrap()
            .contains(&"Yes please — Saturday morning".to_owned()));
    }

    #[test]
    fn similar_ideas_are_found_deterministically_and_never_merged() {
        let conn = memory_connection();
        let existing = share(&conn, "Clean the bathrooms twice per week", "cleaning");
        share(&conn, "A new sofa for the common room", "community");
        let cleaning = Some(topic(&conn, "cleaning"));
        let found = similar(
            &conn,
            "Bathroom cleaning twice a week please",
            cleaning,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(found.id, existing);
        // Unrelated: nothing.
        assert!(similar(&conn, "Buy a table tennis table", cleaning, None)
            .unwrap()
            .is_none());
        // German, with umlauts and plural.
        let de = share(&conn, "Die Bäder zweimal pro Woche putzen", "cleaning");
        assert_eq!(
            similar(&conn, "Bad zweimal die Woche putzen", cleaning, None)
                .unwrap()
                .map(|i| i.id),
            Some(de)
        );
        // Finding one creates or changes nothing.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM ideas", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        // Archived ones aren't suggested.
        set_status(&conn, existing, Status::Archived).unwrap();
        assert!(
            similar(&conn, "Clean the bathrooms twice per week", cleaning, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(
            title_of("  Clean the bathrooms\nmore often  "),
            "Clean the bathrooms"
        );
        let long = "word ".repeat(40);
        let t = title_of(&long);
        assert!(t.chars().count() <= 80 && t.ends_with('…'), "{t}");
        assert_eq!(Status::parse("Trying").unwrap(), Status::Trying);
        assert!(Status::parse("done").is_err());
    }
}
