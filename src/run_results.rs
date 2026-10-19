//! `!kk checkin results <n>`: one check-in's answers, for admins. Numbers
//! for scales, yes/no and choices; the written answers too, because a
//! survey asks "what would you change?" to have it read. Two safeguards
//! keep a single answer from standing out:
//!
//! * nothing but the count while the check-in is open: watching answers
//!   arrive one by one, next to the homeserver's record of who wrote to the
//!   bot when, could tell whose answer is whose;
//! * nothing at all below the privacy threshold (`k` answers), and written
//!   answers only from `k` on, sorted, never in the order they came.
//!
//! Who answered is never shown, whichever way they submitted.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::{
    checkins,
    markup::literal,
    surveys::{QKind, Version},
    time::Zone,
};

/// The answers to `run_id`, as admins read them.
pub fn render(
    conn: &Connection,
    run_id: i64,
    k: i64,
    zone: Zone,
    now: DateTime<Utc>,
) -> Result<String> {
    let run = checkins::run(conn, run_id)?;
    let today = zone.date(now);
    let (responses, eligible): (i64, Option<i64>) = conn.query_row(
        "SELECT (SELECT count(*) FROM responses WHERE run_id = ?1), eligible_at_launch
         FROM survey_runs WHERE id = ?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let open = run.is_open(now);
    let mut lines = vec![format!(
        "📊 **#{run_id} {} {}** · {}",
        run.emoji,
        literal(&run.title),
        if open {
            format!(
                "🟢 open until {}",
                zone.day_time_label(run.closes_at, today)
            )
        } else {
            "🔒 closed".to_owned()
        }
    )];
    lines.push(match eligible.filter(|e| *e > 0) {
        Some(e) => format!(
            "💬 {responses} of {e} asked answered ({} %)",
            (responses * 100 + e / 2) / e
        ),
        None => format!("💬 {responses} answered"),
    });
    if open {
        lines.push(String::new());
        lines.push(format!(
            "⏳ The answers show once it has closed, so nobody's answer can be matched to when \
             they sent it. `!kk checkin close {run_id}` ends it now."
        ));
        return Ok(lines.join("\n"));
    }
    if responses < k {
        lines.push(String::new());
        lines.push(format!(
            "🔒 Fewer than {k} answers: showing them could give someone away."
        ));
        return Ok(lines.join("\n"));
    }
    for (i, q) in run.questions.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!("**{}. {}**", i + 1, literal(&q.text)));
        lines.extend(question(conn, run_id, q, k)?);
    }
    Ok(lines.join("\n"))
}

/// One stored answer: value, text, comment, skipped.
type Row = (Option<f64>, Option<String>, Option<String>, bool);

/// One question's lines.
fn question(conn: &Connection, run_id: i64, q: &Version, k: i64) -> Result<Vec<String>> {
    let rows: Vec<Row> = {
        let mut stmt = conn.prepare(
            "SELECT a.value, a.text, a.comment, a.skipped
             FROM answers a JOIN responses r ON r.id = a.response_id
             WHERE r.run_id = ?1 AND a.question_version_id = ?2",
        )?;
        let rows = stmt.query_map(params![run_id, q.id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let skipped = rows.iter().filter(|r| r.3).count();
    let given: Vec<_> = rows.iter().filter(|r| !r.3).collect();
    let mut out = Vec::new();
    let tally = |n: usize| {
        if skipped > 0 {
            format!("{n} answered · {skipped} skipped")
        } else {
            format!("{n} answered")
        }
    };
    match q.kind {
        QKind::Scale => {
            let values: Vec<i64> = given
                .iter()
                .filter_map(|r| r.0)
                .map(|v| v.round() as i64)
                .collect();
            let steps = q.config.steps();
            if let Some(mean) = mean(&values) {
                out.push(format!(
                    "⌀ **{mean:.1}** of {} · {}",
                    steps.last().copied().unwrap_or(5),
                    tally(values.len())
                ));
            } else {
                out.push(tally(0));
            }
            let faces = q.config.faces.len() == steps.len();
            for (i, step) in steps.iter().enumerate() {
                let n = values.iter().filter(|v| *v == step).count();
                let face = if faces {
                    format!("{} ", q.config.faces[i])
                } else {
                    String::new()
                };
                out.push(format!("{face}{step}: {n}×"));
            }
        }
        QKind::YesNo => {
            let yes = given.iter().filter(|r| r.0 == Some(1.0)).count();
            let no = given.iter().filter(|r| r.0 == Some(0.0)).count();
            out.push(tally(yes + no));
            out.push(format!("👍 Yes: {yes}×"));
            out.push(format!("👎 No: {no}×"));
        }
        QKind::Single | QKind::Multiple => {
            let counts: Vec<(i64, i64)> = {
                let mut stmt = conn.prepare(
                    "SELECT ac.choice_version_id, count(*)
                     FROM answer_choices ac JOIN responses r ON r.id = ac.response_id
                     WHERE r.run_id = ?1 AND ac.question_version_id = ?2
                     GROUP BY ac.choice_version_id",
                )?;
                let rows = stmt.query_map(params![run_id, q.id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            out.push(tally(given.len()));
            let mut choices: Vec<(i64, String)> = q
                .choices
                .iter()
                .map(|c| {
                    let n = counts
                        .iter()
                        .find(|(id, _)| *id == c.version_id)
                        .map_or(0, |(_, n)| *n);
                    let emoji = if c.emoji.is_empty() {
                        "▫️".to_owned()
                    } else {
                        c.emoji.clone()
                    };
                    (n, format!("{emoji} {}: {n}×", literal(&c.label)))
                })
                .collect();
            // Most picked first.
            choices.sort_by_key(|(n, _)| -n);
            out.extend(choices.into_iter().map(|(_, line)| line));
        }
        QKind::Text => {
            let texts: Vec<&str> = given.iter().filter_map(|r| r.1.as_deref()).collect();
            out.push(tally(texts.len()));
            out.extend(written(&texts, k));
        }
    }
    let comments: Vec<&str> = given.iter().filter_map(|r| r.2.as_deref()).collect();
    if !comments.is_empty() {
        out.push(format!("💬 {} comment(s)", comments.len()));
        out.extend(written(&comments, k));
    }
    Ok(out)
}

/// Written answers, sorted, from `k` of them on.
fn written(texts: &[&str], k: i64) -> Vec<String> {
    if (texts.len() as i64) < k {
        if texts.is_empty() {
            return Vec::new();
        }
        return vec![format!("🔒 Fewer than {k}: not shown.")];
    }
    let mut sorted: Vec<String> = texts.iter().map(|t| t.trim().to_owned()).collect();
    sorted.sort_by_key(|t| t.to_lowercase());
    sorted
        .into_iter()
        .map(|t| format!("• “{}”", literal(&t)))
        .collect()
}

fn mean(values: &[i64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<i64>() as f64 / values.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_answers_are_sorted_and_held_back_below_k() {
        assert!(written(&[], 3).is_empty());
        assert_eq!(written(&["b", "a"], 3), ["🔒 Fewer than 3: not shown."]);
        assert_eq!(
            written(&["Zebra", "apple", " mango "], 3),
            ["• “apple”", "• “mango”", "• “Zebra”"]
        );
        assert_eq!(written(&["*bold*", "b", "c"], 3)[0], "• “\\*bold\\*”");
    }

    #[test]
    fn the_mean_of_nothing_is_nothing() {
        assert_eq!(mean(&[]), None);
        assert_eq!(mean(&[2, 3]), Some(2.5));
    }
}
