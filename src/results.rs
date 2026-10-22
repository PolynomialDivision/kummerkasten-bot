//! 📈 What residents see under Results: short, what changed first, then
//! recurring problems, ideas, with drill-down. Text only, built from
//! [`stats`] (which applies the privacy policy); no raw feedback, no free
//! text answers, ever.
//!
//! Wording rule: series and measures are shown side by side on a timeline,
//! never connected by "because", "led to" or the like.

use anyhow::Result;
use chrono::NaiveDate;
use rusqlite::Connection;

use crate::{
    catalog::Catalog,
    ideas, measures, plenums,
    stats::{self, ProblemSeries, Series},
    time::{self, Zone},
};

/// Topics to pick: `(id, emoji, label)`.
pub type Topics = Vec<(i64, String, String)>;

/// A topic page's inputs that need choosing (which check-in it centres on).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TopicPage {
    pub text: String,
    /// Closed check-ins of this topic (for ◀️ / ▶️), newest first.
    pub runs: usize,
    /// The run and scale question a floor breakdown can be shown for.
    pub floors: Option<(i64, i64)>,
}

fn month(d: NaiveDate) -> String {
    d.format("%b").to_string()
}

fn number(v: f64) -> String {
    format!("{v:.1}")
}

/// 📉 / 📈 / ➖ between the last two visible values.
fn trend(values: &[f64]) -> &'static str {
    match values {
        [.., a, b] if b > a => " 📈",
        [.., a, b] if b < a => " 📉",
        [_, _, ..] => " ➖",
        _ => "",
    }
}

/// "fewer than 3" for counts under the threshold (counts of submissions
/// are not people, but a single mention shouldn't stand out either).
pub fn count_text(n: i64, k: i64) -> String {
    if n >= k {
        n.to_string()
    } else {
        format!("fewer than {k}")
    }
}

/// The overview: the running round in numbers, the topics with anything
/// to show. `(text, topics: (id, emoji, label))`.
pub fn home(
    conn: &Connection,
    cat: &Catalog,
    k: i64,
    today: NaiveDate,
) -> Result<(String, Topics)> {
    let mut lines = vec!["📈 **Results**".to_owned(), String::new()];
    if let Some(round) = plenums::open_round(conn)? {
        let n = stats::round_numbers(conn, round.id, today)?;
        lines.push("📮 **Current round**".to_owned());
        lines.push(format!("💬 {} feedback submissions", n.feedback));
        lines.push(format!(
            "📊 {} check-in responses{}",
            n.responses,
            if n.open_checkins > 0 {
                format!(" · {} check-in(s) still open", n.open_checkins)
            } else {
                String::new()
            }
        ));
        lines.push(format!("💡 {} ideas shared", n.ideas));
        lines.push(String::new());
    }
    let mut topics = Vec::new();
    for t in cat.topic_children(None).into_iter().filter(|t| !t.is_idea) {
        let has_data = !stats::metric_series(conn, t.id, k)?.is_empty()
            || !stats::problem_series(conn, t.id, k)?.is_empty()
            || !stats::participation(conn, Some(t.id))?.is_empty();
        if has_data {
            topics.push((t.id, t.emoji.clone(), t.label.clone()));
        }
    }
    if topics.is_empty() {
        lines.push(
            "🌱 No check-in results yet. They appear here once a check-in closes.".to_owned(),
        );
    } else {
        lines.push("👇 Pick a topic for its history:".to_owned());
    }
    Ok((lines.join("\n"), topics))
}

/// A topic's page, centred on its `page`-th latest closed check-in.
pub fn topic(
    conn: &Connection,
    cat: &Catalog,
    topic_root: i64,
    page: usize,
    k: i64,
    zone: Zone,
    today: NaiveDate,
) -> Result<TopicPage> {
    let title = cat.topic(topic_root).map(|t| t.title()).unwrap_or_default();
    let mut lines = vec![format!("**{title}**"), String::new()];
    let runs = stats::participation(conn, Some(topic_root))?;
    let series = stats::metric_series(conn, topic_root, k)?;
    let problems = stats::problem_series(conn, topic_root, k)?;
    let page = page.min(runs.len().saturating_sub(1));
    let centre = runs.get(page);
    let mut floors = None;

    // 1. The check-in in focus: its headline value and participation.
    if let Some(run) = centre {
        lines.push(format!(
            "📊 {} {} check-in · closed {}",
            run.emoji,
            crate::markup::literal(&run.title),
            run.closed_on.format("%-d %b %Y")
        ));
        for s in &series {
            if let Some(p) = s.points.iter().find(|p| p.run_id == run.run_id) {
                match p.value {
                    Some(v) => {
                        lines.push(format!(
                            "   {}: **{} / 5** · {} answers",
                            crate::markup::literal(&s.metric_label),
                            number(v),
                            p.n
                        ));
                        floors.get_or_insert((run.run_id, p.question_version_id));
                    }
                    None => lines.push(format!(
                        "   {}: 🔒 not enough responses to show",
                        crate::markup::literal(&s.metric_label)
                    )),
                }
            }
        }
        lines.push(match run.eligible_at_launch {
            Some(eligible) => format!(
                "   👥 {} of {eligible} eligible residents answered (eligible when it started)",
                run.responses
            ),
            None => format!("   👥 {} answered", run.responses),
        });
        lines.push(String::new());
    }

    // 2. What changed: each metric over time, measures as context.
    let measures = measures::of_topic(conn, topic_root)?;
    for s in &series {
        if let Some(line) = timeline(s, &measures) {
            lines.push(format!("📈 {}", crate::markup::literal(&s.metric_label)));
            lines.push(format!("   {line}"));
        }
    }
    let shown: Vec<&measures::Measure> =
        measures.iter().filter(|m| m.starts_on.is_some()).collect();
    for m in &shown {
        lines.push(format!(
            "   🧪 {}: {} ({})",
            m.starts_on
                .map(|d| d.format("%-d %b").to_string())
                .unwrap_or_default(),
            crate::markup::literal(&m.title),
            m.status.label()
        ));
    }
    if !series.is_empty() || !shown.is_empty() {
        lines.push(String::new());
    }

    // 3. Recurring problems: survey shares and spontaneous counts, apart.
    let mentions = match plenums::open_round(conn)? {
        Some(round) => stats::mentions(conn, round.id, Some(topic_root), today)?,
        None => Default::default(),
    };
    let problem_lines = problem_lines(&problems, &mentions, cat, k);
    if !problem_lines.is_empty() {
        lines.push("⚠️ **Recurring problems**".to_owned());
        lines.extend(problem_lines);
        lines.push(String::new());
    }

    // 4. Ideas.
    let active: i64 = conn.query_row(
        "SELECT count(*) FROM ideas i JOIN topics t ON t.id = i.topic_id
         WHERE coalesce(t.parent_id, t.id) = ?1 AND i.status NOT IN ('rejected', 'archived')",
        [topic_root],
        |r| r.get(0),
    )?;
    lines.push(format!("💡 Ideas: {active} active"));
    let _ = zone;
    Ok(TopicPage {
        text: lines.join("\n"),
        runs: runs.len(),
        floors,
    })
}

/// "3.1 (May) → 2.8 (Jul) → 🧪 19 Oct → 3.5 (Oct)": the visible values in
/// order, with the start of each measure where it falls in time.
fn timeline(s: &Series, measures: &[measures::Measure]) -> Option<String> {
    let mut items: Vec<(NaiveDate, String)> = s
        .points
        .iter()
        .map(|p| {
            (
                p.closed_on,
                match p.value {
                    Some(v) => format!("{} ({})", number(v), month(p.closed_on)),
                    None => format!("🔒 ({})", month(p.closed_on)),
                },
            )
        })
        .collect();
    if items.len() < 2 && measures.is_empty() {
        return None;
    }
    let first = s.points.first()?.closed_on;
    for m in measures {
        if let Some(d) = m.starts_on.filter(|d| *d >= first) {
            items.push((d, format!("🧪 {}", d.format("%-d %b"))));
        }
    }
    items.sort_by_key(|(d, _)| *d);
    let values: Vec<f64> = s.points.iter().filter_map(|p| p.value).collect();
    Some(format!(
        "{}{}",
        items
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join(" → "),
        trend(&values)
    ))
}

fn problem_lines(
    problems: &[ProblemSeries],
    mentions: &std::collections::BTreeMap<i64, i64>,
    cat: &Catalog,
    k: i64,
) -> Vec<String> {
    let mut lines = Vec::new();
    let emoji_of = |pid: i64| {
        cat.topics
            .iter()
            .find(|t| t.problem_id == Some(pid))
            .map(|t| t.emoji.clone())
            .unwrap_or_else(|| "⚠️".to_owned())
    };
    let mut seen = std::collections::BTreeSet::new();
    for p in problems {
        let shares: Vec<i64> = p.shares.iter().filter_map(|s| s.percent).collect();
        if shares.is_empty() {
            continue;
        }
        seen.insert(p.problem_id);
        lines.push(format!(
            "{} {}",
            emoji_of(p.problem_id),
            crate::markup::literal(&p.problem_label)
        ));
        let values: Vec<f64> = shares.iter().map(|v| *v as f64).collect();
        lines.push(format!(
            "   📊 {} of check-in respondents picked it{}",
            shares
                .iter()
                .map(|v| format!("{v}%"))
                .collect::<Vec<_>>()
                .join(" → "),
            trend(&values)
        ));
        if let Some(n) = mentions.get(&p.problem_id).filter(|n| **n > 0) {
            lines.push(format!(
                "   📮 {} Kummerkasten submissions this round",
                count_text(*n, k)
            ));
        }
    }
    // Problems only mentioned spontaneously.
    for (pid, n) in mentions {
        if seen.contains(pid) || *n < k {
            continue;
        }
        let label = cat
            .topics
            .iter()
            .find(|t| t.problem_id == Some(*pid))
            .map(|t| t.label.clone())
            .unwrap_or_default();
        lines.push(format!(
            "{} {}",
            emoji_of(*pid),
            crate::markup::literal(&label)
        ));
        lines.push(format!("   📮 {n} Kummerkasten submissions this round"));
    }
    lines
}

/// 🏠 by floor for one check-in question.
pub fn floors(conn: &Connection, run: i64, qv: i64, k: i64) -> Result<String> {
    let rows = stats::by_floor(conn, run, qv, k)?;
    let mut lines = vec!["🏠 **By floor**".to_owned(), String::new()];
    if rows.iter().all(|r| r.mean.is_none()) {
        lines.push("🔒 Not enough responses to show this breakdown.".to_owned());
    } else {
        for r in rows {
            lines.push(match r.mean {
                Some(m) => format!("{} · **{}**", r.label, number(m)),
                None => format!("{} · 🔒", r.label),
            });
        }
        lines.push(String::new());
        lines.push(format!(
            "🔒 Groups under {k} answers are hidden, and so is anything that would let \
             someone work them out."
        ));
    }
    lines.push(String::new());
    lines.push("🏠 Only answers given “with my floor” count here.".to_owned());
    Ok(lines.join("\n"))
}

/// 📚 Previous plenums: `(plenum_id, label)`.
pub fn history(conn: &Connection, zone: Zone, today: NaiveDate) -> Result<Vec<(i64, String)>> {
    Ok(stats::previous_plenums(conn, 8)?
        .into_iter()
        .map(|(id, at, _)| {
            let label = time::parse_text(&at)
                .map(|t| zone.day_label(t, today))
                .unwrap_or(at);
            (id, label)
        })
        .collect())
}

/// One past plenum's round, as far as it is still stored.
pub fn plenum(
    conn: &Connection,
    cat: &Catalog,
    plenum_id: i64,
    k: i64,
    zone: Zone,
    today: NaiveDate,
) -> Result<String> {
    let Some((_, at, round)) = stats::previous_plenums(conn, 1000)?
        .into_iter()
        .find(|(id, _, _)| *id == plenum_id)
    else {
        return Ok("⏳ That plenum isn't in the history any more.".to_owned());
    };
    let day = time::parse_text(&at)
        .map(|t| zone.day_label(t, today))
        .unwrap_or(at);
    // Everything of a finished round counts (it's in the past).
    let far = stats::all_days();
    let n = stats::round_numbers(conn, round, far)?;
    let mut lines = vec![
        format!("📚 **Plenum · {day}**"),
        String::new(),
        format!("💬 {} feedback submissions still stored", n.feedback),
        format!("📊 {} check-in responses", n.responses),
        format!("💡 {} ideas shared", n.ideas),
    ];
    let mut topics: Vec<(i64, i64)> = stats::by_topic(conn, round, far)?
        .into_iter()
        .filter(|(_, n)| *n >= k)
        .collect();
    topics.sort_by_key(|(_, n)| -n);
    if !topics.is_empty() {
        lines.push(String::new());
        lines.push("🔥 **Most mentioned**".to_owned());
        for (t, n) in topics.into_iter().take(3) {
            lines.push(format!(
                "{} · {n}",
                cat.topic(t).map(|t| t.title()).unwrap_or_default()
            ));
        }
    }
    lines.push(String::new());
    lines.push(
        "🧹 Feedback that was to be deleted after this plenum is gone, and counts nowhere."
            .to_owned(),
    );
    Ok(lines.join("\n"))
}

/// The ideas board text and its items `(id, line)`.
pub fn board(conn: &Connection, cat: &Catalog) -> Result<(String, Vec<(i64, String)>)> {
    let list = ideas::board(conn, 10)?;
    let mut lines = vec!["💡 **Ideas**".to_owned(), String::new()];
    if list.is_empty() {
        lines.push("🌱 No ideas yet. Share one under 📝 Share something → 💡.".to_owned());
    } else {
        lines.push("👆 Tap one to support it or add a comment:".to_owned());
    }
    let items = list
        .into_iter()
        .map(|i| {
            let emoji = i
                .topic_id
                .and_then(|t| cat.topic(t))
                .map(|t| {
                    cat.topic(t.parent_id.unwrap_or(t.id))
                        .map_or(t.emoji.clone(), |root| root.emoji.clone())
                })
                .unwrap_or_else(|| "💡".to_owned());
            (
                i.id,
                format!(
                    "{emoji} **{}** · 👍 {} · 💬 {} · {}",
                    crate::markup::literal(&i.title),
                    i.support,
                    i.comments,
                    i.status.label()
                ),
            )
        })
        .collect();
    let closed = ideas::inactive_count(conn)?;
    if closed > 0 {
        lines.push(format!(
            "🗄️ {closed} closed (archived or rejected) not shown"
        ));
    }
    Ok((lines.join("\n"), items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        measures::{NewMeasure, Status},
        plenums::tests::{at, with_house_calendar, zone},
        secrets::Secrets,
        stats::tests::{cleaning, closed_run},
    };
    use chrono::Duration;

    /// Words that would claim a cause.
    const CAUSAL: &[&str] = &[
        "because",
        "caused",
        "cause",
        "led to",
        "leads to",
        "thanks to",
        "due to",
        "result of",
        "resulted",
        "improved by",
        "worsened by",
        "effect",
        "so that",
        "therefore",
    ];

    #[test]
    fn a_topic_shows_trends_problems_and_measures_without_claiming_causes() {
        let now = at(2026, 5, 4, 12);
        let mut conn = with_house_calendar(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let people = |v: i64, picks: usize| -> Vec<(String, i64, bool, Option<i64>)> {
            (0..5)
                .map(|i| (format!("@p{i}:x"), v, i < picks, None))
                .collect()
        };
        let run = |conn: &mut Connection, when, v, picks| {
            let p = people(v, picks);
            let refs: Vec<(&str, i64, bool, Option<i64>)> = p
                .iter()
                .map(|(a, b, c, d)| (a.as_str(), *b, *c, *d))
                .collect();
            closed_run(conn, &secrets, when, &refs)
        };
        run(&mut conn, at(2026, 5, 4, 12), 3, 3);
        run(&mut conn, at(2026, 7, 6, 12), 2, 4);
        measures::add(
            &conn,
            &NewMeasure {
                title: "Earlier cleaning reminders",
                topic: "cleaning",
                problem: Some("cleaning.skipped_turns"),
                metric: None,
                plenum_id: None,
                starts_on: Some(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()),
                status: Status::Trying,
            },
            "@admin:x",
            now,
        )
        .unwrap();
        run(&mut conn, at(2026, 10, 5, 12), 4, 1);
        let cat = Catalog::load(&conn).unwrap();
        let page = topic(
            &conn,
            &cat,
            cleaning(&conn),
            0,
            3,
            zone(),
            NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
        )
        .unwrap();
        let text = page.text;
        assert!(text.contains("**4.0 / 5** · 5 answers"), "{text}");
        assert!(
            text.contains("3.0 (May) → 2.0 (Jul) → 🧪 1 Aug → 4.0 (Oct) 📈"),
            "{text}"
        );
        assert!(text.contains("60% → 80% → 20%"), "{text}");
        assert!(
            text.contains("🧪 1 Aug: Earlier cleaning reminders (🧪 Trying)"),
            "{text}"
        );
        let lower = text.to_lowercase();
        for word in CAUSAL {
            assert!(!lower.contains(word), "causal wording {word:?} in:\n{text}");
        }
        assert_eq!(page.runs, 3);
        assert!(page.floors.is_some());
        // ◀️ older: centred on the July check-in.
        let older = topic(
            &conn,
            &cat,
            cleaning(&conn),
            1,
            3,
            zone(),
            NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
        )
        .unwrap();
        assert!(older.text.contains("**2.0 / 5**"), "{}", older.text);
        // Never any free text: there is none in it.
        assert!(!text.contains('“'));
        let _ = Duration::zero();
    }

    #[test]
    fn small_counts_read_as_fewer_than_the_threshold() {
        assert_eq!(count_text(2, 3), "fewer than 3");
        assert_eq!(count_text(9, 3), "9");
    }
}
