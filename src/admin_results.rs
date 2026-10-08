//! `!kk ideas …`, `!kk measure …`, `!kk summary …` — M3's admin side,
//! from an admin's verified chat.

use anyhow::{Context, Result};
use chrono::Utc;

use crate::{
    catalog::Catalog,
    dm::Kk,
    ideas::{self, Status as IdeaStatus},
    markup::literal,
    measures::{self, Change, NewMeasure, Status as MeasureStatus},
    plenums, summary, time,
};

pub const HELP: &str = "💡 **Ideas**
`!kk ideas` — all, with numbers · `!kk idea <n>` — one, with its comments
`!kk idea <n> status <suggested|discussing|trying|implemented|rejected|archived>`
`!kk idea <n> rename <title>` · `!kk idea <n> topic <topic key>`
`!kk idea add <topic key> <title>` — e.g. one raised at the plenum
   (supporters are never listed — only counted)

🧪 **Measures** (what the plenum decided to try)
`!kk measures` · `!kk measure add <topic key> <title> [problem <key>] [metric <key>] [plenum <n>] [from 2026-10-19] [status trying]`
`!kk measure <n> status <proposed|decided|trying|adopted|stopped>`
`!kk measure <n> describe <text>` · `rename <title>` · `from <date>` · `until <date|none>` · `plenum <n>`

📮 **Plenum summary** (the day before, into the central room(s))
`!kk summary preview [plenum n]` — what it would say now";

/// Handles the M3 commands; `None` for anything else.
pub async fn dispatch(kk: &Kk, actor: &str, args: &[&str]) -> Option<Result<String>> {
    Some(match args {
        ["ideas"] => list_ideas(kk).await,
        ["idea", "add", topic, title @ ..] if !title.is_empty() => {
            let (topic, title) = (topic.to_string(), title.join(" "));
            let today = kk.engine.zone.date(Utc::now());
            kk.engine
                .db
                .run(move |c| {
                    let topic_id: i64 = c
                        .query_row("SELECT id FROM topics WHERE key = ?1", [&topic], |r| {
                            r.get(0)
                        })
                        .with_context(|| format!("No topic {topic:?} (`!kk topics`)."))?;
                    let round = plenums::open_round(c)?.map(|r| r.id);
                    ideas::create(
                        c,
                        &ideas::NewIdea {
                            text: &title,
                            topic_id: Some(topic_id),
                            problem_id: None,
                            origin: ideas::Origin::Admin,
                            round_id: round,
                        },
                        today,
                    )
                })
                .await
                .map(|id| format!("💡 Idea #{id} is on the board."))
        }
        ["idea", n] => show_idea(kk, n).await,
        ["idea", n, "status", status] => {
            let (n, status) = (number(n), IdeaStatus::parse(status));
            match (n, status) {
                (Ok(n), Ok(status)) => kk
                    .engine
                    .db
                    .run(move |c| ideas::set_status(c, n, status))
                    .await
                    .map(|()| format!("💡 Idea #{n}: {}", status.label())),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        ["idea", n, "rename", title @ ..] if !title.is_empty() => match number(n) {
            Ok(n) => {
                let title = title.join(" ");
                kk.engine
                    .db
                    .run(move |c| ideas::rename(c, n, &title))
                    .await
                    .map(|()| format!("✏️ Idea #{n} renamed."))
            }
            Err(e) => Err(e),
        },
        ["idea", n, "topic", key] => match number(n) {
            Ok(n) => {
                let key = key.to_string();
                kk.engine
                    .db
                    .run(move |c| ideas::set_topic(c, n, &key))
                    .await
                    .map(|()| format!("🗂️ Idea #{n} moved."))
            }
            Err(e) => Err(e),
        },
        ["measures"] => list_measures(kk).await,
        ["measure", "add", topic, rest @ ..] => add_measure(kk, actor, topic, rest).await,
        ["measure", n, verb, rest @ ..] => edit_measure(kk, n, verb, rest).await,
        ["summary", "preview", rest @ ..] => preview(kk, rest).await,
        _ => return None,
    })
}

fn number(s: &str) -> Result<i64> {
    s.trim_start_matches('#')
        .parse()
        .with_context(|| format!("{s:?} isn't a number"))
}

async fn list_ideas(kk: &Kk) -> Result<String> {
    kk.engine
        .db
        .run(|c| {
            let all = ideas::all(c)?;
            if all.is_empty() {
                return Ok("💡 No ideas yet.".to_owned());
            }
            let mut out = vec!["💡 **Ideas**".to_owned()];
            for i in all.iter().take(30) {
                out.push(format!(
                    "#{} · {} · 👍 {} · 💬 {} · {}",
                    i.id,
                    literal(&i.title),
                    i.support,
                    i.comments,
                    i.status.label()
                ));
            }
            Ok(out.join("\n"))
        })
        .await
}

async fn show_idea(kk: &Kk, n: &str) -> Result<String> {
    let n = number(n)?;
    kk.engine
        .db
        .run(move |c| {
            let idea = ideas::get(c, n)?.with_context(|| format!("There's no idea #{n}."))?;
            let cat = Catalog::load(c)?;
            let mut out = vec![
                format!("💡 **#{} · {}**", idea.id, literal(&idea.title)),
                format!(
                    "{} · 👍 {} · 💬 {} · since {}",
                    idea.status.label(),
                    idea.support,
                    idea.comments,
                    idea.created_on.format("%-d %b %Y")
                ),
            ];
            if let Some(t) = idea.topic_id {
                out.push(cat.topic_path(t));
            }
            if let Some(body) = &idea.body {
                out.push(format!("“{}”", literal(body)));
            }
            for comment in ideas::comments(c, n, 10)? {
                out.push(format!("💬 “{}”", literal(&comment)));
            }
            Ok(out.join("\n"))
        })
        .await
}

async fn list_measures(kk: &Kk) -> Result<String> {
    let zone = kk.engine.zone;
    kk.engine
        .db
        .run(move |c| {
            let all = measures::all(c)?;
            if all.is_empty() {
                return Ok("🧪 No measures yet — `!kk measure add …`".to_owned());
            }
            let cat = Catalog::load(c)?;
            let today = zone.date(Utc::now());
            let mut out = vec!["🧪 **Measures**".to_owned()];
            for m in all.iter().take(30) {
                let plenum = match m.plenum_id {
                    Some(p) => plenums::get(c, p)?
                        .map(|p| {
                            format!(
                                " · decided at #{} ({})",
                                p.id,
                                zone.day_label(p.starts_at, today)
                            )
                        })
                        .unwrap_or_default(),
                    None => String::new(),
                };
                out.push(format!(
                    "#{} · {} · {} · {}{}{}",
                    m.id,
                    literal(&m.title),
                    m.topic_id.map(|t| cat.topic_path(t)).unwrap_or_default(),
                    m.status.label(),
                    m.starts_on
                        .map(|d| format!(" · from {}", d.format("%-d %b %Y")))
                        .unwrap_or_default(),
                    plenum
                ));
            }
            Ok(out.join("\n"))
        })
        .await
}

async fn add_measure(kk: &Kk, actor: &str, topic: &str, rest: &[&str]) -> Result<String> {
    let mut title = Vec::new();
    let (mut problem, mut metric, mut plenum, mut from) = (None, None, None, None);
    let mut status = MeasureStatus::Decided;
    let mut i = 0;
    while i < rest.len() {
        match (rest[i], rest.get(i + 1)) {
            ("problem", Some(v)) => problem = Some(v.to_string()),
            ("metric", Some(v)) => metric = Some(v.to_string()),
            ("plenum", Some(v)) => plenum = Some(number(v)?),
            ("from", Some(v)) => from = Some(time::parse_date(v)?),
            ("status", Some(v)) => status = MeasureStatus::parse(v)?,
            (word, _) => {
                title.push(word.to_owned());
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    let (title, topic, actor) = (title.join(" "), topic.to_owned(), actor.to_owned());
    let id = kk
        .engine
        .db
        .run(move |c| {
            measures::add(
                c,
                &NewMeasure {
                    title: &title,
                    topic: &topic,
                    problem: problem.as_deref(),
                    metric: metric.as_deref(),
                    plenum_id: plenum,
                    starts_on: from,
                    status,
                },
                &actor,
                Utc::now(),
            )
        })
        .await?;
    Ok(format!(
        "🧪 Measure #{id} recorded. It shows on the topic's timeline in 📈 Results once it \
         has a start date — as context, never as a cause."
    ))
}

async fn edit_measure(kk: &Kk, n: &str, verb: &str, rest: &[&str]) -> Result<String> {
    let n = number(n)?;
    let text = rest.join(" ");
    let date = |s: &str| time::parse_date(s);
    let change_of = |verb: &str| -> Result<Change<'static>> {
        Ok(match verb {
            "status" => Change::Status(MeasureStatus::parse(&text)?),
            "from" => Change::Starts(date(&text)?),
            "until" if text == "none" => Change::Ends(None),
            "until" => Change::Ends(Some(date(&text)?)),
            "plenum" => Change::Plenum(number(&text)?),
            _ => anyhow::bail!("Try status, describe, rename, from, until or plenum."),
        })
    };
    let owned = text.clone();
    let verb = verb.to_owned();
    let change = match verb.as_str() {
        "describe" | "rename" => None,
        other => Some(change_of(other)?),
    };
    kk.engine
        .db
        .run(move |c| match (verb.as_str(), change) {
            ("describe", _) => measures::change(c, n, Change::Description(&owned)),
            ("rename", _) => measures::change(c, n, Change::Title(&owned)),
            (_, Some(change)) => measures::change(c, n, change),
            _ => Ok(()),
        })
        .await?;
    Ok(format!("🧪 Measure #{n} updated."))
}

async fn preview(kk: &Kk, rest: &[&str]) -> Result<String> {
    let id = match rest {
        ["plenum", n] => Some(number(n)?),
        _ => None,
    };
    let zone = kk.engine.zone;
    let k = kk.engine.k();
    kk.engine
        .db
        .run(move |c| {
            let id = match id {
                Some(id) => id,
                None => plenums::next(c)?.context("No plenum is scheduled.")?.id,
            };
            let cat = Catalog::load(c)?;
            let text = summary::render(c, &cat, id, k, zone, Utc::now())?;
            Ok(format!("👀 **Preview** — nothing was posted.\n\n{text}"))
        })
        .await
}
