//! A check-in template as a file (TOML), instead of a long list of `!kk`
//! commands: the questions one below the other, no numbers to keep track
//! of. Admins upload one in their chat with the bot, or keep them in
//! `store/surveys/` (`!kk survey load <name>`); the bot ships the ones in
//! the repo's `surveys/` too. `!kk survey <key> export` gives one back.
//!
//! ```toml
//! key = "house_cleaning"
//! emoji = "🧹"
//! title = "House cleaning"
//! topic = "cleaning"            # optional
//! intro = "We'd like to know …" # optional
//!
//! [[question]]
//! single = "🧼 How happy are you with how clean the house is?"
//! choices = ["😍 Very happy", "🙂 Fairly happy"]
//! other = "✏️ Something else"   # optional, they write their own
//!
//! [[question]]
//! text = "🌟 If you could wish for one thing …"
//! optional = true
//! ```
//!
//! A question is one of `single`, `multiple`, `yes_no`, `scale` or `text`,
//! with its wording. Optional per question: `optional`, `comment` (a
//! comment field), `suggestion` (may become an idea), `metric`, `problem`,
//! `min`/`max` (scales). A choice is `"emoji label"`, or
//! `{ label = "emoji label", problem = "key" }`.
//!
//! A file is applied as a whole or not at all. A template that was never
//! started is replaced by it; one that was keeps its history (results stay
//! comparable): the file needs another key then.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{
    admin_checkins::choice_edit,
    surveys::{self, Edit, NewQuestion, QKind},
};

/// Largest file accepted (an upload or one on disk).
pub const MAX_BYTES: usize = 128 * 1024;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SurveyFile {
    pub key: String,
    pub emoji: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub intro: String,
    #[serde(default, rename = "question")]
    pub questions: Vec<FileQuestion>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct FileQuestion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<String>,
    #[serde(default, alias = "yesno", skip_serializing_if = "Option::is_none")]
    pub yes_no: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<FileChoice>,
    /// The last choice: "something else", they write their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub comment: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub suggestion: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(untagged)]
pub enum FileChoice {
    Plain(String),
    Full {
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        problem: Option<String>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl FileQuestion {
    /// Its kind and wording: exactly one of `single`, `multiple`, …
    fn kind(&self) -> Result<(QKind, &str)> {
        let given: Vec<(QKind, &str)> = [
            (QKind::Single, &self.single),
            (QKind::Multiple, &self.multiple),
            (QKind::YesNo, &self.yes_no),
            (QKind::Scale, &self.scale),
            (QKind::Text, &self.text),
        ]
        .into_iter()
        .filter_map(|(kind, text)| text.as_deref().map(|t| (kind, t)))
        .collect();
        match given.as_slice() {
            [one] => Ok(*one),
            [] => bail!("needs one of single, multiple, yes_no, scale or text, with the question"),
            _ => bail!(
                "is two kinds at once: give only one of single, multiple, yes_no, scale, text"
            ),
        }
    }
}

impl SurveyFile {
    pub fn parse(text: &str) -> Result<Self> {
        let file: SurveyFile = toml::from_str(text).context("The file doesn't read as a survey")?;
        file.check()?;
        Ok(file)
    }

    /// What can be told without the database.
    fn check(&self) -> Result<()> {
        crate::catalog::check_key(&self.key)?;
        if self.title.trim().is_empty() {
            bail!("A title, please.");
        }
        if self.questions.is_empty() {
            bail!("The survey has no questions ([[question]]).");
        }
        for (i, q) in self.questions.iter().enumerate() {
            let n = i + 1;
            let (kind, _) = q.kind().with_context(|| format!("Question {n}"))?;
            let choices = q.choices.len() + usize::from(q.other.is_some());
            if kind.has_choices() && choices < 2 {
                bail!("Question {n} needs at least two choices.");
            }
            if !kind.has_choices() && choices > 0 {
                bail!("Question {n}: only single and multiple choice questions have choices.");
            }
            if kind != QKind::Scale && (q.min.is_some() || q.max.is_some()) {
                bail!("Question {n}: min and max are for scales.");
            }
        }
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }
}

/// What applying a file did.
#[derive(Debug, PartialEq)]
pub struct Applied {
    pub key: String,
    pub questions: usize,
    /// A template with this key (never started) was replaced.
    pub replaced: bool,
}

/// Store `file` as its template, all or nothing.
pub fn apply(conn: &mut Connection, file: &SurveyFile) -> Result<Applied> {
    let tx = conn.transaction()?;
    let key = file.key.as_str();
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM survey_templates WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        let started: Option<i64> = tx
            .query_row(
                "SELECT id FROM survey_runs WHERE template_id = ?1 ORDER BY id LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(run) = started {
            bail!(
                "«{key}» was started already (check-in #{run}), so it stays as it is and its \
                 results stay comparable. Give the file another key (like `{key}_2`), or change \
                 single questions with `!kk survey {key} edit …`."
            );
        }
        delete_template(&tx, id)?;
    }
    surveys::create_template(&tx, key, &file.emoji, &file.title, file.topic.as_deref())?;
    if !file.intro.trim().is_empty() {
        surveys::set_intro(&tx, key, file.intro.trim())?;
    }
    for (i, q) in file.questions.iter().enumerate() {
        let n = i + 1;
        add(&tx, key, n, q).with_context(|| format!("Question {n}"))?;
    }
    tx.commit()?;
    Ok(Applied {
        key: key.to_owned(),
        questions: file.questions.len(),
        replaced: existing.is_some(),
    })
}

fn add(tx: &Connection, key: &str, n: usize, q: &FileQuestion) -> Result<()> {
    let (kind, text) = q.kind()?;
    surveys::add_question(
        tx,
        key,
        NewQuestion {
            kind,
            text,
            required: !q.optional,
            asks_suggestion: q.suggestion,
            metric: q.metric.as_deref(),
            problem: q.problem.as_deref(),
        },
    )?;
    let choices = q
        .choices
        .iter()
        .map(|c| match c {
            FileChoice::Plain(label) => (label.as_str(), None, false),
            FileChoice::Full { label, problem } => (label.as_str(), problem.as_deref(), false),
        })
        .chain(q.other.as_deref().map(|label| (label, None, true)));
    for (c, (label, problem, other)) in choices.enumerate() {
        let mut words: Vec<&str> = Vec::new();
        if let Some(problem) = problem {
            words.extend(["problem", problem]);
        }
        if other {
            words.push("other");
        }
        words.extend(label.split_whitespace());
        let edit = choice_edit("add", &words)
            .with_context(|| format!("choice {}: a label, please", c + 1))?;
        surveys::edit_in(tx, key, n, edit).with_context(|| format!("choice {}", c + 1))?;
    }
    if q.min.is_some() || q.max.is_some() {
        let min = q.min.unwrap_or(1);
        let max = q.max.unwrap_or(5);
        surveys::edit_in(tx, key, n, Edit::Scale { min, max })?;
    }
    if q.comment {
        surveys::edit_in(tx, key, n, Edit::Comment(true))?;
    }
    Ok(())
}

/// A template no check-in ever used, with everything under it.
fn delete_template(tx: &Connection, id: i64) -> Result<()> {
    tx.execute_batch(&format!(
        "DELETE FROM choice_versions WHERE question_version_id IN
             (SELECT v.id FROM question_versions v JOIN questions q ON q.id = v.question_id
              WHERE q.template_id = {id});
         DELETE FROM choices WHERE question_id IN
             (SELECT id FROM questions WHERE template_id = {id});
         DELETE FROM question_versions WHERE question_id IN
             (SELECT id FROM questions WHERE template_id = {id});
         DELETE FROM questions WHERE template_id = {id};
         DELETE FROM survey_templates WHERE id = {id};"
    ))?;
    Ok(())
}

/// Template `key` as a file: its active questions, as new check-ins ask
/// them.
pub fn export(conn: &Connection, key: &str) -> Result<SurveyFile> {
    let (emoji, title, intro, topic): (String, String, String, Option<String>) = conn.query_row(
        "SELECT t.emoji, t.title, t.intro, tp.key
         FROM survey_templates t LEFT JOIN topics tp ON tp.id = t.topic_id
         WHERE t.id = ?1",
        params![surveys::template_id(conn, key)?],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let questions = surveys::questions(conn, key)?
        .into_iter()
        .filter(|q| q.active)
        .map(|q| {
            let v = q.current;
            let mut out = FileQuestion {
                optional: !v.required,
                comment: v.allow_comment,
                suggestion: v.asks_suggestion,
                metric: v.metric,
                problem: v.problem,
                ..FileQuestion::default()
            };
            let text = Some(v.text);
            match v.kind {
                QKind::Single => out.single = text,
                QKind::Multiple => out.multiple = text,
                QKind::YesNo => out.yes_no = text,
                QKind::Scale => out.scale = text,
                QKind::Text => out.text = text,
            }
            if v.kind == QKind::Scale && (v.config.min, v.config.max) != (Some(1), Some(5)) {
                out.min = v.config.min;
                out.max = v.config.max;
            }
            for c in v.choices {
                let label = if c.emoji.is_empty() {
                    c.label
                } else {
                    format!("{} {}", c.emoji, c.label)
                };
                if c.is_other && out.other.is_none() && c.problem.is_none() {
                    out.other = Some(label);
                } else if let Some(problem) = c.problem {
                    out.choices.push(FileChoice::Full {
                        label,
                        problem: Some(problem),
                    });
                } else {
                    out.choices.push(FileChoice::Plain(label));
                }
            }
            out
        })
        .collect();
    Ok(SurveyFile {
        key: key.to_owned(),
        emoji,
        title,
        topic,
        intro,
        questions,
    })
}

/// A name of a file in `store/surveys/` (or shipped): `house_cleaning`,
/// with or without `.toml`.
pub fn file_name(name: &str) -> Result<String> {
    let stem = name.strip_suffix(".toml").unwrap_or(name);
    crate::catalog::check_key(stem).context("Like `!kk survey load house_cleaning`.")?;
    Ok(format!("{stem}.toml"))
}

/// The surveys that come with the bot (the repo's `surveys/`).
pub const SHIPPED: &[(&str, &str)] = &[(
    "house_cleaning.toml",
    include_str!("../surveys/house_cleaning.toml"),
)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{checkins, screens};

    fn shipped() -> SurveyFile {
        SurveyFile::parse(SHIPPED[0].1).unwrap()
    }

    /// The ready-made house cleaning survey: it must apply on a fresh
    /// database, start, and show its emojis as buttons.
    #[test]
    fn the_house_cleaning_survey_applies_and_starts() {
        let now = crate::plenums::tests::at(2026, 10, 19, 12);
        let mut conn = crate::plenums::tests::with_default_rule(now);
        let file = shipped();
        let applied = apply(&mut conn, &file).unwrap();
        assert_eq!(applied.questions, 12);
        assert!(!applied.replaced);
        let questions = surveys::questions(&conn, "house_cleaning").unwrap();
        for (i, q) in questions.iter().enumerate() {
            let n = i + 1;
            if q.current.kind.has_choices() {
                let emojis = surveys::choice_emojis(&conn, "house_cleaning", n).unwrap();
                assert!((2..=10).contains(&emojis.len()), "question {n}");
                assert!(emojis.iter().all(|e| !e.is_empty()), "question {n}");
                let warnings = screens::choice_key_warnings(&emojis);
                assert!(warnings.is_empty(), "question {n}: {warnings:?}");
                assert!(q.current.required, "question {n}");
            } else {
                assert!(!q.current.required, "question {n}");
            }
        }
        assert!(questions[6].current.allow_comment);
        assert!(questions[8].current.asks_suggestion);
        let opts = checkins::StartOptions {
            identities: vec![crate::submissions::Identity::Anonymous],
            ..Default::default()
        };
        let plan = checkins::plan(
            &conn,
            "house_cleaning",
            &opts,
            now,
            chrono::Duration::hours(24),
        )
        .unwrap();
        let id = checkins::start(&mut conn, &plan, "@admin:x", now).unwrap();
        let run = checkins::run(&conn, id).unwrap();
        assert_eq!(run.title, "House cleaning");
        for i in 0..run.questions.len() {
            let text = screens::question_preview(&run, i);
            assert!(!text.contains("1️⃣"), "question {}: {text}", i + 1);
        }
    }

    #[test]
    fn what_is_exported_applies_to_the_same_survey() {
        let now = crate::plenums::tests::at(2026, 10, 19, 12);
        let mut conn = crate::plenums::tests::with_default_rule(now);
        let file = shipped();
        apply(&mut conn, &file).unwrap();
        let exported = export(&conn, "house_cleaning").unwrap();
        assert_eq!(exported, file);
        // And as text, read back.
        let text = exported.to_toml().unwrap();
        assert_eq!(SurveyFile::parse(&text).unwrap(), file);
    }

    #[test]
    fn a_survey_never_started_is_replaced_one_started_is_kept() {
        let now = crate::plenums::tests::at(2026, 10, 19, 12);
        let mut conn = crate::plenums::tests::with_default_rule(now);
        let mut file = shipped();
        apply(&mut conn, &file).unwrap();
        file.questions.truncate(2);
        let again = apply(&mut conn, &file).unwrap();
        assert!(again.replaced);
        assert_eq!(
            surveys::questions(&conn, "house_cleaning").unwrap().len(),
            2
        );
        // The starting "cleaning" template was started in no test yet:
        // start it, then a file for it is refused, and nothing changes.
        let plan = checkins::plan(
            &conn,
            "cleaning",
            &checkins::StartOptions::default(),
            now,
            chrono::Duration::hours(24),
        )
        .unwrap();
        checkins::start(&mut conn, &plan, "@admin:x", now).unwrap();
        file.key = "cleaning".into();
        let error = apply(&mut conn, &file).unwrap_err().to_string();
        assert!(error.contains("started already"), "{error}");
        assert_eq!(surveys::questions(&conn, "cleaning").unwrap().len(), 3);
    }

    #[test]
    fn a_bad_file_changes_nothing_and_says_where() {
        let now = crate::plenums::tests::at(2026, 10, 19, 12);
        let mut conn = crate::plenums::tests::with_default_rule(now);
        let unknown_problem = r#"
key = "trial"
emoji = "🧪"
title = "Trial"

[[question]]
single = "🙂 Fine?"
choices = ["👍 Yes", { label = "👎 No", problem = "no.such.problem" }]
"#;
        let file = SurveyFile::parse(unknown_problem).unwrap();
        let error = format!("{:#}", apply(&mut conn, &file).unwrap_err());
        assert!(
            error.contains("Question 1") && error.contains("choice 2"),
            "{error}"
        );
        assert!(surveys::template_id(&conn, "trial").is_err(), "rolled back");
        for (text, says) in [
            ("key = \"x\"\nemoji = \"🧪\"\ntitle = \"X\"\n", "no questions"),
            (
                "key = \"x\"\nemoji = \"🧪\"\ntitle = \"X\"\n[[question]]\noptional = true\n",
                "Question 1",
            ),
            (
                "key = \"x\"\nemoji = \"🧪\"\ntitle = \"X\"\n[[question]]\nsingle = \"?\"\nchoices = [\"A\"]\n",
                "two choices",
            ),
            (
                "key = \"x\"\nemoji = \"🧪\"\ntitle = \"X\"\n[[question]]\ntext = \"?\"\nlable = \"typo\"\n",
                "doesn't read",
            ),
            ("key = \"Not A Key\"\nemoji = \"🧪\"\ntitle = \"X\"\n", "lower case"),
        ] {
            let error = format!("{:#}", SurveyFile::parse(text).unwrap_err());
            assert!(error.contains(says), "{says}: {error}");
        }
    }

    #[test]
    fn file_names_are_keys() {
        assert_eq!(file_name("house_cleaning").unwrap(), "house_cleaning.toml");
        assert_eq!(
            file_name("house_cleaning.toml").unwrap(),
            "house_cleaning.toml"
        );
        assert!(file_name("../secrets").is_err());
    }
}
