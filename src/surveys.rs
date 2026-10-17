//! Check-in templates: questions, their versions, choices and metrics.
//!
//! The rule: a question version that a check-in has used is history. Any
//! edit of a used question creates the next version (choices copied over,
//! keeping their identity); an unused version is simply edited. The
//! database enforces it too (triggers in `002_checkins.sql`). What a
//! question measures is its canonical metric; what a choice stands for, its
//! canonical problem: both stable keys, whatever the wording.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::time;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QKind {
    Single,
    Multiple,
    YesNo,
    Scale,
    Text,
}

impl QKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QKind::Single => "single",
            QKind::Multiple => "multiple",
            QKind::YesNo => "yes_no",
            QKind::Scale => "scale",
            QKind::Text => "text",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "single" => QKind::Single,
            "multiple" | "multi" => QKind::Multiple,
            "yes_no" | "yesno" => QKind::YesNo,
            "scale" | "rating" => QKind::Scale,
            "text" => QKind::Text,
            other => {
                bail!("{other:?} isn't a question type (scale, yesno, single, multiple, text)")
            }
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            QKind::Single => "Single choice",
            QKind::Multiple => "Multiple choice",
            QKind::YesNo => "Yes / no",
            QKind::Scale => "Scale",
            QKind::Text => "Free text",
        }
    }

    pub fn has_choices(self) -> bool {
        matches!(self, QKind::Single | QKind::Multiple)
    }
}

/// Kind-specific details; JSON in the database.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    /// One face per scale step, lowest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faces: Vec<String>,
}

impl Config {
    pub fn scale() -> Self {
        Config {
            min: Some(1),
            max: Some(5),
            faces: ["😩", "😕", "😐", "🙂", "😄"].map(String::from).to_vec(),
        }
    }

    /// The scale's steps, `min..=max` (1–5 if unset; at most 10 steps).
    pub fn steps(&self) -> Vec<i64> {
        let min = self.min.unwrap_or(1);
        let max = self.max.unwrap_or(5).clamp(min, min + 9);
        (min..=max).collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub choice_id: i64,
    pub version_id: i64,
    pub emoji: String,
    pub label: String,
    pub problem: Option<String>,
    pub is_other: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub id: i64,
    pub version: i64,
    pub kind: QKind,
    pub text: String,
    pub config: Config,
    pub required: bool,
    pub allow_comment: bool,
    pub asks_suggestion: bool,
    pub metric: Option<String>,
    pub problem: Option<String>,
    pub choices: Vec<Choice>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub id: i64,
    pub position: i64,
    pub active: bool,
    /// The current (latest) version.
    pub current: Version,
    /// Some check-in has asked the current version.
    pub used: bool,
}

pub fn template_id(conn: &Connection, key: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM survey_templates WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| format!("There's no check-in template «{key}» (`!kk surveys`)."))
}

pub fn create_template(
    conn: &Connection,
    key: &str,
    emoji: &str,
    title: &str,
    topic: Option<&str>,
) -> Result<()> {
    crate::catalog::check_key(key)?;
    let topic_id = topic
        .map(|t| {
            conn.query_row("SELECT id FROM topics WHERE key = ?1", [t], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .with_context(|| format!("There's no topic «{t}»."))
        })
        .transpose()?;
    conn.execute(
        "INSERT INTO survey_templates (key, topic_id, emoji, title, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![key, topic_id, emoji, title, time::now_text()],
    )
    .with_context(|| format!("«{key}» exists already."))?;
    Ok(())
}

pub fn set_intro(conn: &Connection, key: &str, intro: &str) -> Result<()> {
    let id = template_id(conn, key)?;
    conn.execute(
        "UPDATE survey_templates SET intro = ?2 WHERE id = ?1",
        params![id, intro],
    )?;
    Ok(())
}

/// A template's emoji and title. Started check-ins keep theirs.
pub fn rename_template(
    conn: &Connection,
    key: &str,
    emoji: Option<&str>,
    title: &str,
) -> Result<()> {
    let id = template_id(conn, key)?;
    if title.trim().is_empty() {
        bail!("A title, please.");
    }
    conn.execute(
        "UPDATE survey_templates SET emoji = coalesce(?2, emoji), title = ?3 WHERE id = ?1",
        params![id, emoji, title.trim()],
    )?;
    Ok(())
}

/// `(key, emoji, title, active questions)` of every template.
pub fn templates(conn: &Connection) -> Result<Vec<(String, String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.key, t.emoji, t.title,
                (SELECT count(*) FROM questions q WHERE q.template_id = t.id AND q.active = 1)
         FROM survey_templates t WHERE t.active = 1 ORDER BY t.title",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn key_of(conn: &Connection, table: &str, id: Option<i64>) -> Result<Option<String>> {
    Ok(match id {
        Some(id) => conn
            .query_row(
                &format!("SELECT key FROM {table} WHERE id = ?1"),
                [id],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    })
}

fn id_of(conn: &Connection, table: &str, key: &str) -> Result<i64> {
    conn.query_row(
        &format!("SELECT id FROM {table} WHERE key = ?1"),
        [key],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| format!("There's no {} «{key}».", table.trim_end_matches('s')))
}

pub fn version(conn: &Connection, id: i64) -> Result<Version> {
    let (version, kind, text, config, required, allow_comment, asks, metric, problem): (
        i64,
        String,
        String,
        String,
        bool,
        bool,
        bool,
        Option<i64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT version, kind, text, config, required, allow_comment, asks_suggestion,
                metric_id, problem_id
         FROM question_versions WHERE id = ?1",
        [id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        },
    )?;
    let mut stmt = conn.prepare(
        "SELECT cv.choice_id, cv.id, cv.emoji, cv.label, p.key, cv.is_other
         FROM choice_versions cv LEFT JOIN problems p ON p.id = cv.problem_id
         WHERE cv.question_version_id = ?1 ORDER BY cv.position, cv.id",
    )?;
    let choices = stmt
        .query_map([id], |r| {
            Ok(Choice {
                choice_id: r.get(0)?,
                version_id: r.get(1)?,
                emoji: r.get(2)?,
                label: r.get(3)?,
                problem: r.get(4)?,
                is_other: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Version {
        id,
        version,
        kind: QKind::parse(&kind)?,
        text,
        config: serde_json::from_str(&config).unwrap_or_default(),
        required,
        allow_comment,
        asks_suggestion: asks,
        metric: key_of(conn, "metrics", metric)?,
        problem: key_of(conn, "problems", problem)?,
        choices,
    })
}

fn is_used(conn: &Connection, version_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM survey_run_questions WHERE question_version_id = ?1 LIMIT 1",
            [version_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The template's questions in order, disabled ones included.
pub fn questions(conn: &Connection, key: &str) -> Result<Vec<Question>> {
    let template = template_id(conn, key)?;
    let rows: Vec<(i64, i64, bool, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT q.id, q.position, q.active,
                    (SELECT id FROM question_versions v WHERE v.question_id = q.id
                     ORDER BY v.version DESC LIMIT 1)
             FROM questions q WHERE q.template_id = ?1 ORDER BY q.position, q.id",
        )?;
        let rows = stmt.query_map([template], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    rows.into_iter()
        .map(|(id, position, active, current)| {
            Ok(Question {
                id,
                position,
                active,
                used: is_used(conn, current)?,
                current: version(conn, current)?,
            })
        })
        .collect()
}

/// Question `n` (1-based, as listed) of template `key`.
fn nth(conn: &Connection, key: &str, n: usize) -> Result<Question> {
    questions(conn, key)?
        .into_iter()
        .nth(n.checked_sub(1).context("Questions count from 1.")?)
        .with_context(|| format!("«{key}» has no question {n}."))
}

pub struct NewQuestion<'a> {
    pub kind: QKind,
    pub text: &'a str,
    pub required: bool,
    pub asks_suggestion: bool,
    pub metric: Option<&'a str>,
    pub problem: Option<&'a str>,
}

/// Add a question at the end. Returns its number.
pub fn add_question(conn: &Connection, key: &str, q: NewQuestion) -> Result<usize> {
    if q.text.trim().is_empty() {
        bail!("The question needs a text.");
    }
    let template = template_id(conn, key)?;
    let metric = q.metric.map(|m| id_of(conn, "metrics", m)).transpose()?;
    let problem = q.problem.map(|p| id_of(conn, "problems", p)).transpose()?;
    let position: i64 = conn.query_row(
        "SELECT coalesce(max(position), 0) + 1 FROM questions WHERE template_id = ?1",
        [template],
        |r| r.get(0),
    )?;
    let now = time::now_text();
    conn.execute(
        "INSERT INTO questions (template_id, position, created_at) VALUES (?1, ?2, ?3)",
        params![template, position, now],
    )?;
    let question = conn.last_insert_rowid();
    let config = if q.kind == QKind::Scale {
        Config::scale()
    } else {
        Config::default()
    };
    conn.execute(
        "INSERT INTO question_versions
           (question_id, version, kind, text, config, required, asks_suggestion,
            metric_id, problem_id, created_at)
         VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            question,
            q.kind.as_str(),
            q.text.trim(),
            serde_json::to_string(&config)?,
            q.required,
            q.asks_suggestion,
            metric,
            problem,
            now
        ],
    )?;
    Ok(questions(conn, key)?.len())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    Text(String),
    Required(bool),
    Comment(bool),
    Metric(Option<String>),
    Problem(Option<String>),
    Scale {
        min: i64,
        max: i64,
    },
    AddChoice {
        emoji: String,
        label: String,
        problem: Option<String>,
        is_other: bool,
    },
    RenameChoice {
        n: usize,
        label: String,
    },
    /// Its emoji (its button); empty: none (the choices are numbered).
    ChoiceEmoji {
        n: usize,
        emoji: String,
    },
    ChoiceProblem {
        n: usize,
        problem: Option<String>,
    },
    RemoveChoice {
        n: usize,
    },
}

#[derive(Debug, PartialEq)]
pub struct Edited {
    /// A new version was created (the old one was used).
    pub new_version: Option<i64>,
}

/// Change question `n` of `key`. If its current version was asked in a
/// check-in, the change goes into a new version; that history stays.
pub fn edit(conn: &mut Connection, key: &str, n: usize, edit: Edit) -> Result<Edited> {
    let tx = conn.transaction()?;
    let question = nth(&tx, key, n)?;
    let mut v = question.current.clone();
    // Validate (and resolve keys) before touching anything.
    let metric_id = match &edit {
        Edit::Metric(Some(m)) => Some(id_of(&tx, "metrics", m)?),
        _ => None,
    };
    let problem_id = match &edit {
        Edit::Problem(Some(p))
        | Edit::AddChoice {
            problem: Some(p), ..
        }
        | Edit::ChoiceProblem {
            problem: Some(p), ..
        } => Some(id_of(&tx, "problems", p)?),
        _ => None,
    };
    match &edit {
        Edit::AddChoice { .. }
        | Edit::RenameChoice { .. }
        | Edit::ChoiceEmoji { .. }
        | Edit::RemoveChoice { .. }
        | Edit::ChoiceProblem { .. }
            if !v.kind.has_choices() =>
        {
            bail!("Only single and multiple choice questions have choices.")
        }
        Edit::Scale { .. } if v.kind != QKind::Scale => bail!("That's not a scale question."),
        Edit::Scale { min, max } if !(0..=10).contains(min) || max <= min || max - min > 9 => {
            bail!("A scale goes from 0–10, with 2 to 10 steps.")
        }
        Edit::Text(t) if t.trim().is_empty() => bail!("The question needs a text."),
        Edit::RenameChoice { n, .. }
        | Edit::ChoiceEmoji { n, .. }
        | Edit::RemoveChoice { n }
        | Edit::ChoiceProblem { n, .. }
            if *n == 0 || *n > v.choices.len() =>
        {
            bail!("There's no choice {n}.")
        }
        _ => {}
    }
    let new_version = if question.used {
        let next = v.version + 1;
        tx.execute(
            "INSERT INTO question_versions
               (question_id, version, kind, text, config, required, allow_comment,
                asks_suggestion, metric_id, problem_id, created_at)
             SELECT question_id, ?2, kind, text, config, required, allow_comment,
                    asks_suggestion, metric_id, problem_id, ?3
             FROM question_versions WHERE id = ?1",
            params![v.id, next, time::now_text()],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO choice_versions
               (choice_id, question_version_id, position, emoji, label, problem_id, is_other)
             SELECT choice_id, ?2, position, emoji, label, problem_id, is_other
             FROM choice_versions WHERE question_version_id = ?1",
            params![v.id, id],
        )?;
        v = version(&tx, id)?;
        Some(id)
    } else {
        None
    };
    match edit {
        Edit::Text(t) => {
            tx.execute(
                "UPDATE question_versions SET text = ?2 WHERE id = ?1",
                params![v.id, t.trim()],
            )?;
        }
        Edit::Required(required) => {
            tx.execute(
                "UPDATE question_versions SET required = ?2 WHERE id = ?1",
                params![v.id, required],
            )?;
        }
        Edit::Comment(on) => {
            tx.execute(
                "UPDATE question_versions SET allow_comment = ?2 WHERE id = ?1",
                params![v.id, on],
            )?;
        }
        Edit::Metric(_) => {
            tx.execute(
                "UPDATE question_versions SET metric_id = ?2 WHERE id = ?1",
                params![v.id, metric_id],
            )?;
        }
        Edit::Problem(_) => {
            tx.execute(
                "UPDATE question_versions SET problem_id = ?2 WHERE id = ?1",
                params![v.id, problem_id],
            )?;
        }
        Edit::Scale { min, max } => {
            let mut config = Config::scale();
            config.min = Some(min);
            config.max = Some(max);
            if (max - min + 1) as usize != config.faces.len() {
                config.faces.clear();
            }
            tx.execute(
                "UPDATE question_versions SET config = ?2 WHERE id = ?1",
                params![v.id, serde_json::to_string(&config)?],
            )?;
        }
        Edit::AddChoice {
            emoji,
            label,
            is_other,
            ..
        } => {
            if label.trim().is_empty() {
                bail!("The choice needs a label.");
            }
            tx.execute(
                "INSERT INTO choices (question_id, created_at) VALUES (?1, ?2)",
                params![question.id, time::now_text()],
            )?;
            let choice = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO choice_versions
                   (choice_id, question_version_id, position, emoji, label, problem_id, is_other)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    choice,
                    v.id,
                    v.choices.len() as i64 + 1,
                    emoji,
                    label.trim(),
                    problem_id,
                    is_other
                ],
            )?;
        }
        Edit::RenameChoice { n, label } => {
            tx.execute(
                "UPDATE choice_versions SET label = ?2 WHERE id = ?1",
                params![v.choices[n - 1].version_id, label.trim()],
            )?;
        }
        Edit::ChoiceEmoji { n, emoji } => {
            tx.execute(
                "UPDATE choice_versions SET emoji = ?2 WHERE id = ?1",
                params![v.choices[n - 1].version_id, emoji.trim()],
            )?;
        }
        Edit::ChoiceProblem { n, .. } => {
            tx.execute(
                "UPDATE choice_versions SET problem_id = ?2 WHERE id = ?1",
                params![v.choices[n - 1].version_id, problem_id],
            )?;
        }
        Edit::RemoveChoice { n } => {
            // Only this version drops it; the choice and its history stay.
            tx.execute(
                "DELETE FROM choice_versions WHERE id = ?1",
                [v.choices[n - 1].version_id],
            )?;
        }
    }
    tx.commit()?;
    Ok(Edited { new_version })
}

/// The emojis of question `n`'s choices (its current version).
pub fn choice_emojis(conn: &Connection, key: &str, n: usize) -> Result<Vec<String>> {
    Ok(nth(conn, key, n)?
        .current
        .choices
        .into_iter()
        .map(|c| c.emoji)
        .collect())
}

/// Stop or start asking question `n` in new check-ins.
pub fn set_active(conn: &Connection, key: &str, n: usize, active: bool) -> Result<()> {
    let q = nth(conn, key, n)?;
    conn.execute(
        "UPDATE questions SET active = ?2 WHERE id = ?1",
        params![q.id, active],
    )?;
    Ok(())
}

/// Move question `n` to place `to` (both 1-based).
pub fn move_question(conn: &mut Connection, key: &str, n: usize, to: usize) -> Result<()> {
    let tx = conn.transaction()?;
    let mut list = questions(&tx, key)?;
    if n == 0 || n > list.len() || to == 0 || to > list.len() {
        bail!("Positions go from 1 to {}.", list.len());
    }
    let q = list.remove(n - 1);
    list.insert(to - 1, q);
    for (i, q) in list.iter().enumerate() {
        tx.execute(
            "UPDATE questions SET position = ?2 WHERE id = ?1",
            params![q.id, i as i64 + 1],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn add_metric(conn: &Connection, key: &str, topic: &str, label: &str) -> Result<()> {
    crate::catalog::check_key(key)?;
    let topic_id = id_of(conn, "topics", topic)?;
    conn.execute(
        "INSERT INTO metrics (key, topic_id, label, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![key, topic_id, label, time::now_text()],
    )
    .with_context(|| format!("«{key}» exists already."))?;
    Ok(())
}

/// A metric's label. Its key: what comparisons go by, stays.
pub fn rename_metric(conn: &Connection, key: &str, label: &str) -> Result<()> {
    if label.trim().is_empty() {
        bail!("A label, please.");
    }
    let changed = conn.execute(
        "UPDATE metrics SET label = ?2 WHERE key = ?1",
        params![key, label.trim()],
    )?;
    if changed == 0 {
        bail!("There's no metric «{key}» (`!kk metrics`).");
    }
    Ok(())
}

pub fn metrics(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT m.key, m.label, coalesce(t.key, ''), m.active
         FROM metrics m LEFT JOIN topics t ON t.id = m.topic_id ORDER BY m.key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, bool>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, label, topic, active) = row?;
        out.push(format!(
            "`{key}`: {label} · {topic}{}",
            if active { "" } else { " · 🗄️ retired" }
        ));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::memory_connection;

    #[test]
    fn the_starting_cleaning_template_is_complete() {
        let conn = memory_connection();
        let qs = questions(&conn, "cleaning").unwrap();
        assert_eq!(qs.len(), 3);
        assert_eq!(qs[0].current.kind, QKind::Scale);
        assert_eq!(
            qs[0].current.metric.as_deref(),
            Some("cleaning.overall_satisfaction")
        );
        assert_eq!(qs[0].current.config.steps(), [1, 2, 3, 4, 5]);
        assert_eq!(qs[1].current.choices.len(), 5);
        assert_eq!(
            qs[1].current.choices[0].problem.as_deref(),
            Some("cleaning.skipped_turns")
        );
        assert!(qs[1].current.choices[4].is_other);
        assert!(qs[2].current.asks_suggestion);
        assert!(!qs[2].current.required);
        assert!(qs.iter().all(|q| !q.used));
    }

    #[test]
    fn templates_and_metrics_can_be_renamed() {
        let conn = memory_connection();
        rename_template(&conn, "cleaning", Some("🚽"), "Toilet cleaning").unwrap();
        rename_template(&conn, "cleaning", None, "Toilets").unwrap();
        let (emoji, title): (String, String) = conn
            .query_row(
                "SELECT emoji, title FROM survey_templates WHERE key = 'cleaning'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((emoji.as_str(), title.as_str()), ("🚽", "Toilets"));
        assert!(rename_template(&conn, "cleaning", None, " ").is_err());
        assert!(rename_template(&conn, "nonsense", None, "x").is_err());

        rename_metric(&conn, "cleaning.overall_satisfaction", "Cleaning (1–5)").unwrap();
        assert!(metrics(&conn).unwrap().contains("Cleaning (1–5)"));
        assert!(rename_metric(&conn, "nonsense", "x").is_err());
    }

    #[test]
    fn templates_are_edited_in_place_until_used() {
        let mut conn = memory_connection();
        create_template(
            &conn,
            "kitchen",
            "🍳",
            "Kitchen organisation",
            Some("kitchen"),
        )
        .unwrap();
        let n = add_question(
            &conn,
            "kitchen",
            NewQuestion {
                kind: QKind::Single,
                text: "Who empties the dishwasher?",
                required: true,
                asks_suggestion: false,
                metric: None,
                problem: None,
            },
        )
        .unwrap();
        assert_eq!(n, 1);
        for label in ["Whoever sees it", "A rota", "Nobody 😬"] {
            edit(
                &mut conn,
                "kitchen",
                1,
                Edit::AddChoice {
                    emoji: String::new(),
                    label: label.into(),
                    problem: None,
                    is_other: false,
                },
            )
            .unwrap();
        }
        let e = edit(
            &mut conn,
            "kitchen",
            1,
            Edit::Text("Who empties it, mostly?".into()),
        )
        .unwrap();
        assert_eq!(e.new_version, None);
        let q = &questions(&conn, "kitchen").unwrap()[0];
        assert_eq!(q.current.version, 1);
        assert_eq!(q.current.text, "Who empties it, mostly?");
        assert_eq!(q.current.choices.len(), 3);
        // Kind-specific edits are checked.
        assert!(edit(&mut conn, "kitchen", 1, Edit::Scale { min: 1, max: 5 }).is_err());
        assert!(edit(&mut conn, "kitchen", 1, Edit::RemoveChoice { n: 9 }).is_err());
        assert!(edit(
            &mut conn,
            "kitchen",
            1,
            Edit::Metric(Some("no.such".into()))
        )
        .is_err());
    }

    #[test]
    fn reorder_and_disable() {
        let mut conn = memory_connection();
        move_question(&mut conn, "cleaning", 3, 1).unwrap();
        let qs = questions(&conn, "cleaning").unwrap();
        assert_eq!(qs[0].current.kind, QKind::Text);
        assert_eq!(qs.iter().map(|q| q.position).collect::<Vec<_>>(), [1, 2, 3]);
        set_active(&conn, "cleaning", 2, false).unwrap();
        assert!(!questions(&conn, "cleaning").unwrap()[1].active);
        assert!(move_question(&mut conn, "cleaning", 1, 7).is_err());
    }
}
