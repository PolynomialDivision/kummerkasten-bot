//! `!kk survey …`, `!kk metric …`, `!kk checkin …`: editing check-in
//! templates and starting check-ins, from an admin's verified chat.

use std::time::{Duration as StdDuration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use mxbot_common::matrix_sdk::ruma::OwnedRoomId;

use crate::{
    checkins::{self, StartOptions, Target},
    dm::Kk,
    markup::literal,
    screens,
    surveys::{self, Edit, NewQuestion, QKind},
    time,
};

/// How long a planned start waits for `!kk checkin go`.
const PLAN_VALID: StdDuration = StdDuration::from_secs(30 * 60);

pub const HELP: &str = "📊 **Check-ins**
`!kk surveys` · `!kk survey new <key> <emoji> <title> [topic <topic>]`
`!kk survey <key>`: the questions, numbered
`!kk survey <key> intro <text>` · `!kk survey <key> title [emoji] <title>`
`!kk survey <key> add <scale|yesno|single|multiple|text> [optional] [suggestion] [metric <key>] [problem <key>] <question>`
`!kk survey <key> edit <n> <question>` · `required <n>` · `optional <n>` · `comment <n> on|off`
`!kk survey <key> metric <n> <key|none>` · `problem <n> <key|none>` · `scale <n> <min> <max>`
`!kk survey <key> choice <n> add [problem <key>] [other] [emoji] <label>`
`!kk survey <key> choice <n> rename <c> <label>` · `choice <n> problem <c> <key|none>` · `choice <n> remove <c>`
`!kk survey <key> move <n> <to>` · `disable <n>` · `enable <n>` · `preview`
`!kk metrics` · `!kk metric add <key> <topic> <label>` · `!kk metric rename <key> <label>`
`!kk checkin start <key> [house | room <!room> | floor <location>] [closes 2026-11-14 [18:00] | closes 1d-before] [no-reminder] [announce] [no-plenum] [only anonymous,floor]`
   without `closes`: until the plenum starts; `1d-before`/`12h-before` follow the plenum when it moves, a date doesn't
`!kk checkin go` · `!kk checkin drop` · `!kk checkins` · `!kk checkin close <n>`
`!kk rooms central <!room> [off]`: where `announce` posts";

/// Handles the check-in commands; `None` for anything else.
pub async fn dispatch(kk: &Kk, actor: &str, args: &[&str]) -> Option<Result<String>> {
    let reply = match args {
        ["surveys"] => templates(kk).await,
        ["survey", "new", key, emoji, rest @ ..] => {
            let (title, topic) = split_option(rest, "topic");
            let (key, emoji, topic) = (key.to_string(), emoji.to_string(), topic);
            if title.is_empty() {
                return Some(Err(anyhow::anyhow!("A title, please.")));
            }
            kk.engine
                .db
                .run(move |c| surveys::create_template(c, &key, &emoji, &title, topic.as_deref()))
                .await
                .map(|()| {
                    "✅ Template created. Add questions with `!kk survey <key> add …`.".to_owned()
                })
        }
        ["survey", key] => overview(kk, key).await,
        ["survey", key, "preview"] => preview(kk, key).await,
        ["survey", key, "intro", text @ ..] if !text.is_empty() => {
            let (key, text) = (key.to_string(), text.join(" "));
            kk.engine
                .db
                .run(move |c| surveys::set_intro(c, &key, &text))
                .await
                .map(|()| {
                    "✏️ Intro updated. New check-ins show it; started ones keep theirs.".to_owned()
                })
        }
        ["survey", key, "title", first, more @ ..] => {
            let (emoji, title) = if looks_like_emoji(first) && !more.is_empty() {
                (Some(first.to_string()), more.join(" "))
            } else {
                (None, [&[*first], more].concat().join(" "))
            };
            let key = key.to_string();
            kk.engine
                .db
                .run(move |c| surveys::rename_template(c, &key, emoji.as_deref(), &title))
                .await
                .map(|()| {
                    "✏️ Title updated. New check-ins show it; started ones keep theirs.".to_owned()
                })
        }
        ["survey", key, "add", kind, rest @ ..] => add(kk, key, kind, rest).await,
        ["survey", key, "move", n, to] => {
            let (key, n, to) = (key.to_string(), number(n), number(to));
            match (n, to) {
                (Ok(n), Ok(to)) => kk
                    .engine
                    .db
                    .run(move |c| surveys::move_question(c, &key, n, to))
                    .await
                    .map(|()| "↕️ Moved. New check-ins ask in this order.".to_owned()),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        ["survey", key, verb @ ("disable" | "enable"), n] => {
            let active = *verb == "enable";
            let key = key.to_string();
            match number(n) {
                Ok(n) => kk
                    .engine
                    .db
                    .run(move |c| surveys::set_active(c, &key, n, active))
                    .await
                    .map(|()| {
                        if active {
                            "✅ Asked again in new check-ins.".to_owned()
                        } else {
                            "🚫 Not asked in new check-ins any more. Its history stays.".to_owned()
                        }
                    }),
                Err(e) => Err(e),
            }
        }
        ["survey", key, "choice", n, verb, rest @ ..] => match choice_edit(verb, rest) {
            Some(edit) => run_edit(kk, key, n, edit).await,
            None => Err(anyhow::anyhow!("See `!kk help` for the choice commands.")),
        },
        ["survey", key, verb, n, rest @ ..] => match edit_of(verb, rest) {
            Some(edit) => run_edit(kk, key, n, edit).await,
            None => return None,
        },
        ["metrics"] => kk
            .engine
            .db
            .run(|c| surveys::metrics(c))
            .await
            .map(|list| format!("🧠 **Canonical metrics**\n{list}")),
        ["metric", "add", key, topic, label @ ..] if !label.is_empty() => {
            let (key, topic, label) = (key.to_string(), topic.to_string(), label.join(" "));
            kk.engine
                .db
                .run(move |c| surveys::add_metric(c, &key, &topic, &label))
                .await
                .map(|()| "🧠 Metric added. Point questions at it with `!kk survey <key> metric <n> <metric>`.".to_owned())
        }
        ["metric", "rename", key, label @ ..] if !label.is_empty() => {
            let (key, label) = (key.to_string(), label.join(" "));
            kk.engine
                .db
                .run(move |c| surveys::rename_metric(c, &key, &label))
                .await
                .map(|()| "✏️ Metric renamed. Its key and history stay.".to_owned())
        }
        ["checkin", "start", key, rest @ ..] => plan_start(kk, actor, key, rest).await,
        ["checkin", "go"] => go(kk, actor).await,
        ["checkin", "drop"] => {
            kk.pending_starts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(actor);
            Ok("❌ Dropped. Nothing was started.".to_owned())
        }
        ["checkins"] => runs(kk).await,
        ["checkin", "close", n] => match n.trim_start_matches('#').parse::<i64>() {
            Ok(id) => kk
                .engine
                .db
                .run(move |c| checkins::close(c, id, Utc::now()))
                .await
                .map(|closed| {
                    if closed {
                        format!("🔒 Check-in #{id} closed. Answers so far are kept.")
                    } else {
                        format!("🤔 Check-in #{id} isn't open.")
                    }
                }),
            Err(_) => Err(anyhow::anyhow!(
                "Which check-in? A number from `!kk checkins`."
            )),
        },
        ["rooms", "central", ..] if kk.managed_rooms.is_some() => Err(crate::admin::managed()),
        ["rooms", "central", room, rest @ ..] => {
            let on = rest.first() != Some(&"off");
            let room = room.to_string();
            kk.engine
                .db
                .run(move |c| checkins::set_central(c, &room, on))
                .await
                .and_then(|done| {
                    if !done {
                        bail!("Authorize the room first: `!kk rooms add …`");
                    }
                    Ok(if on {
                        "📣 Check-ins started with `announce` are announced there.".to_owned()
                    } else {
                        "🔕 No more announcements there.".to_owned()
                    })
                })
        }
        _ => return None,
    };
    Some(reply)
}

fn number(s: &str) -> Result<usize> {
    s.parse()
        .with_context(|| format!("{s:?} isn't a question number"))
}

/// `words…` and the value after `name` (removed from the words).
fn split_option(rest: &[&str], name: &str) -> (String, Option<String>) {
    let mut words = Vec::new();
    let mut value = None;
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == name && i + 1 < rest.len() {
            value = Some(rest[i + 1].to_owned());
            i += 2;
        } else {
            words.push(rest[i]);
            i += 1;
        }
    }
    (words.join(" "), value)
}

fn none_or(s: &str) -> Option<String> {
    (s != "none" && s != "-").then(|| s.to_owned())
}

fn edit_of(verb: &str, rest: &[&str]) -> Option<Edit> {
    Some(match (verb, rest) {
        ("edit", text) if !text.is_empty() => Edit::Text(text.join(" ")),
        ("required", []) => Edit::Required(true),
        ("optional", []) => Edit::Required(false),
        ("comment", ["on"]) => Edit::Comment(true),
        ("comment", ["off"]) => Edit::Comment(false),
        ("metric", [key]) => Edit::Metric(none_or(key)),
        ("problem", [key]) => Edit::Problem(none_or(key)),
        ("scale", [min, max]) => Edit::Scale {
            min: min.parse().ok()?,
            max: max.parse().ok()?,
        },
        _ => return None,
    })
}

fn looks_like_emoji(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| !c.is_alphanumeric() && !c.is_ascii_punctuation())
}

fn choice_edit(verb: &str, rest: &[&str]) -> Option<Edit> {
    Some(match (verb, rest) {
        ("add", rest) => {
            let (words, problem) = split_option(rest, "problem");
            let mut words: Vec<&str> = words.split_whitespace().collect();
            let is_other = words.first() == Some(&"other");
            if is_other {
                words.remove(0);
            }
            let emoji = match words.first() {
                Some(w) if looks_like_emoji(w) && words.len() > 1 => words.remove(0).to_owned(),
                _ => String::new(),
            };
            if words.is_empty() {
                return None;
            }
            Edit::AddChoice {
                emoji,
                label: words.join(" "),
                problem,
                is_other,
            }
        }
        ("rename", [c, label @ ..]) if !label.is_empty() => Edit::RenameChoice {
            n: c.parse().ok()?,
            label: label.join(" "),
        },
        ("problem", [c, key]) => Edit::ChoiceProblem {
            n: c.parse().ok()?,
            problem: none_or(key),
        },
        ("remove", [c]) => Edit::RemoveChoice { n: c.parse().ok()? },
        _ => return None,
    })
}

async fn run_edit(kk: &Kk, key: &str, n: &str, edit: Edit) -> Result<String> {
    let n = number(n)?;
    let key = key.to_owned();
    let k = key.clone();
    let edited = kk
        .engine
        .db
        .run(move |c| surveys::edit(c, &k, n, edit))
        .await?;
    Ok(match edited.new_version {
        Some(_) => format!(
            "⚠️ **This question was already asked in a check-in.**\n\
             So I created a new version for future check-ins. Previous check-ins \
             (and their answers) stay exactly as they were.\n\
             🧠 If the meaning changed, point it at a different metric too: \
             `!kk survey {key} metric {n} <metric>`."
        ),
        None => "✏️ Done.".to_owned(),
    })
}

async fn templates(kk: &Kk) -> Result<String> {
    let list = kk.engine.db.run(|c| surveys::templates(c)).await?;
    let mut out = vec!["📊 **Check-in templates**".to_owned()];
    for (key, emoji, title, n) in list {
        out.push(format!(
            "{emoji} **{}** `{key}` · {n} question(s)",
            literal(&title)
        ));
    }
    out.push(String::new());
    out.push(HELP.to_owned());
    Ok(out.join("\n"))
}

async fn overview(kk: &Kk, key: &str) -> Result<String> {
    let key = key.to_owned();
    kk.engine
        .db
        .run(move |c| {
            let (emoji, title, intro): (String, String, String) = c.query_row(
                "SELECT emoji, title, intro FROM survey_templates WHERE id = ?1",
                [surveys::template_id(c, &key)?],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let mut out = vec![format!("📊 **{emoji} {}** `{key}`", literal(&title))];
            if !intro.is_empty() {
                out.push(literal(&intro));
            }
            out.push(String::new());
            for (i, q) in surveys::questions(c, &key)?.iter().enumerate() {
                let v = &q.current;
                let off = if q.active { "" } else { "🚫 " };
                out.push(format!("**{}.** {off}{}", i + 1, literal(&v.text)));
                let mut facts = vec![match v.kind {
                    QKind::Scale => {
                        let steps = v.config.steps();
                        format!("Scale {}–{}", steps[0], steps[steps.len() - 1])
                    }
                    kind => kind.label().to_owned(),
                }];
                if !v.required {
                    facts.push("optional".into());
                }
                if v.allow_comment {
                    facts.push("💬 comment".into());
                }
                if v.asks_suggestion {
                    facts.push("💡 asks for ideas".into());
                }
                if let Some(m) = &v.metric {
                    facts.push(format!("🧠 `{m}`"));
                }
                if let Some(p) = &v.problem {
                    facts.push(format!("⚠️ `{p}`"));
                }
                facts.push(format!("v{}", v.version));
                if q.used {
                    facts.push("🔒 asked before".into());
                }
                out.push(format!("     {}", facts.join(" · ")));
                for (j, ch) in v.choices.iter().enumerate() {
                    let problem = ch
                        .problem
                        .as_ref()
                        .map(|p| format!(" → `{p}`"))
                        .unwrap_or_default();
                    let other = if ch.is_other { " (other)" } else { "" };
                    out.push(format!(
                        "     {}. {} {}{other}{problem}",
                        j + 1,
                        ch.emoji,
                        literal(&ch.label)
                    ));
                }
            }
            out.push(String::new());
            out.push(format!(
                "➕ `!kk survey {key} add …` · ✏️ `edit <n> …` · ↕️ `move <n> <to>` · \
                 🚫 `disable <n>` · 👀 `!kk survey {key} preview` · 🚀 `!kk checkin start {key}`"
            ));
            Ok(out.join("\n"))
        })
        .await
}

async fn preview(kk: &Kk, key: &str) -> Result<String> {
    let key = key.to_owned();
    let zone = kk.engine.zone;
    kk.engine
        .db
        .run(move |c| {
            let plan = checkins::plan(
                c,
                &key,
                &StartOptions {
                    reminder: false,
                    ..Default::default()
                },
                Utc::now(),
                chrono::Duration::hours(1),
            );
            let plan = plan?;
            let run = checkins::Run {
                id: 0,
                emoji: plan.emoji.clone(),
                title: plan.title.clone(),
                intro: plan.intro.clone(),
                target: plan.target.clone(),
                identities: plan.identities.clone(),
                plenum_id: plan.plenum.map(|p| p.0),
                closes_at: plan.closes_at,
                reminder_at: None,
                announce: false,
                open: true,
                questions: plan
                    .versions
                    .iter()
                    .map(|v| surveys::version(c, *v))
                    .collect::<Result<_>>()?,
            };
            let ctx = screens::Ctx {
                zone: Some(zone),
                today: Some(zone.date(Utc::now())),
                ..Default::default()
            };
            let mut out = vec![
                "👀 **Preview**: what residents will see".to_owned(),
                String::new(),
                screens::checkin_notice(&run, &ctx),
            ];
            for i in 0..run.questions.len() {
                out.push("· · ·".to_owned());
                out.push(screens::question_preview(&run, i));
            }
            Ok(out.join("\n"))
        })
        .await
}

async fn add(kk: &Kk, key: &str, kind: &str, rest: &[&str]) -> Result<String> {
    let kind = QKind::parse(kind)?;
    let mut words = Vec::new();
    let (mut required, mut suggestion) = (true, false);
    let (mut metric, mut problem) = (None, None);
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "optional" if words.is_empty() => required = false,
            "suggestion" if words.is_empty() => suggestion = true,
            "metric" if words.is_empty() && i + 1 < rest.len() => {
                metric = Some(rest[i + 1].to_owned());
                i += 1;
            }
            "problem" if words.is_empty() && i + 1 < rest.len() => {
                problem = Some(rest[i + 1].to_owned());
                i += 1;
            }
            word => words.push(word),
        }
        i += 1;
    }
    let text = words.join(" ");
    let key = key.to_owned();
    let k = key.clone();
    let n = kk
        .engine
        .db
        .run(move |c| {
            surveys::add_question(
                c,
                &k,
                NewQuestion {
                    kind,
                    text: &text,
                    required,
                    asks_suggestion: suggestion,
                    metric: metric.as_deref(),
                    problem: problem.as_deref(),
                },
            )
        })
        .await?;
    let next = if kind.has_choices() {
        format!(
            "\nNow its choices: `!kk survey {key} choice {n} add [problem <key>] [emoji] <label>`"
        )
    } else {
        String::new()
    };
    Ok(format!("➕ Question {n} added.{next}"))
}

async fn plan_start(kk: &Kk, actor: &str, key: &str, rest: &[&str]) -> Result<String> {
    let zone = kk.engine.zone;
    let now = Utc::now();
    let mut opts = StartOptions::default();
    let mut i = 0;
    let rooms = kk
        .engine
        .db
        .run(|c| crate::store::authorized_rooms(c))
        .await?;
    while i < rest.len() {
        match rest[i] {
            "house" => opts.target = Target::House,
            "room" => {
                let id = rest.get(i + 1).context("`room` needs a room ID")?;
                let room = OwnedRoomId::try_from(*id).context("That isn't a room ID.")?;
                let (_, label) = rooms
                    .iter()
                    .find(|(r, _)| r == room.as_str())
                    .context("That room isn't authorized (`!kk rooms`).")?;
                opts.target = Target::Room {
                    room_id: room.to_string(),
                    label: label.clone(),
                };
                i += 1;
            }
            "floor" => {
                let loc = rest
                    .get(i + 1)
                    .context("`floor` needs a location key")?
                    .to_string();
                let found: Option<(i64, String)> = kk
                    .engine
                    .db
                    .run(move |c| {
                        use rusqlite::OptionalExtension;
                        Ok(c.query_row(
                            "SELECT id, label FROM locations WHERE key = ?1 AND is_floor = 1",
                            [&loc],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?)
                    })
                    .await?;
                let (location_id, label) =
                    found.context("No floor with that key (`!kk locations`).")?;
                opts.target = Target::Floor { location_id, label };
                i += 1;
            }
            "closes" if rest.get(i + 1).is_some_and(|a| a.ends_with("-before")) => {
                // `closes 1d-before` / `12h-before`: relative to the plenum.
                let spec = rest[i + 1].trim_end_matches("-before");
                let (n, unit) = spec.split_at(spec.len().saturating_sub(1));
                let n: i64 = n
                    .parse()
                    .context("Like `closes 1d-before` or `closes 12h-before`.")?;
                opts.deadline = checkins::Deadline::BeforePlenum(match unit {
                    "d" => chrono::Duration::days(n),
                    "h" => chrono::Duration::hours(n),
                    _ => bail!("Like `closes 1d-before` or `closes 12h-before`."),
                });
                i += 1;
            }
            "closes" => {
                let date = time::parse_date(rest.get(i + 1).context("`closes` needs a date")?)?;
                let at = match rest.get(i + 2).and_then(|t| time::parse_hm(t).ok()) {
                    Some(t) => {
                        i += 1;
                        t
                    }
                    None => chrono::NaiveTime::from_hms_opt(20, 0, 0).unwrap(),
                };
                opts.deadline = checkins::Deadline::Fixed(zone.at(date, at));
                i += 1;
            }
            "no-reminder" => opts.reminder = false,
            "announce" => opts.announce = true,
            "no-plenum" => opts.for_plenum = false,
            "only" => {
                opts.identities = checkins::parse_identities(
                    rest.get(i + 1)
                        .context("`only` needs e.g. anonymous,floor")?,
                )?;
                i += 1;
            }
            other => bail!("I don't understand {other:?}. See `!kk help`."),
        }
        i += 1;
    }
    // Test mode: whatever the target, only the admin who starts it.
    let asked_for = kk.is_test().then(|| opts.target.describe());
    if kk.is_test() {
        opts.target = Target::Person {
            mxid: actor.to_owned(),
        };
    }
    let remind = kk.settings.get().remind_before();
    let key = key.to_owned();
    let o = opts.clone();
    let plan = kk
        .engine
        .db
        .run(move |c| checkins::plan(c, &key, &o, now, remind))
        .await?;
    let today = zone.date(now);
    let mut out = vec![
        "🚀 **Start check-in?**".to_owned(),
        String::new(),
        format!("{} {}", plan.emoji, literal(&plan.title)),
        plan.target.describe(),
    ];
    if let Some(asked_for) = asked_for {
        out.push(format!("   (live it would go to: {asked_for})"));
    }
    out.extend([
        format!("📝 {} question(s)", plan.versions.len()),
        format!(
            "🗓️ Closes {}{}",
            zone.day_time_label(plan.closes_at, today),
            match plan.plenum_offset {
                Some(o) if o.is_zero() => " · when the plenum starts (moves with it)".to_owned(),
                Some(o) => format!(" · {}h before the plenum (moves with it)", -o.num_hours()),
                None => " · fixed".to_owned(),
            }
        ),
    ]);
    if let Some((_, _, at)) = plan.plenum {
        out.push(format!("🗣️ Next plenum ({})", zone.day_label(at, today)));
    }
    out.push(match plan.reminder_at {
        Some(at) => format!("🔔 One reminder · {}", zone.day_time_label(at, today)),
        None => "🔕 No reminder".to_owned(),
    });
    out.push(if plan.announce {
        "📣 DM + central house room".to_owned()
    } else {
        "📣 DM only".to_owned()
    });
    out.push(format!(
        "👀 {}",
        plan.identities
            .iter()
            .map(|i| i.title())
            .collect::<Vec<_>>()
            .join(" · ")
    ));
    out.push(String::new());
    out.push("✅ `!kk checkin go` · ❌ `!kk checkin drop`".to_owned());
    kk.pending_starts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(actor.to_owned(), (plan, Instant::now()));
    Ok(out.join("\n"))
}

async fn go(kk: &Kk, actor: &str) -> Result<String> {
    let plan = kk
        .pending_starts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(actor)
        .filter(|(_, at)| at.elapsed() < PLAN_VALID)
        .map(|(plan, _)| plan)
        .context("Nothing to start. Plan it first with `!kk checkin start <key> …`.")?;
    let actor = actor.to_owned();
    let id = kk
        .engine
        .db
        .run(move |c| checkins::start(c, &plan, &actor, Utc::now()))
        .await?;
    kk.wake.notify_one();
    Ok(if kk.is_test() {
        format!("🚀 Check-in #{id} started, 🧪 only for you: the message comes into your Kummerkasten chat. 📬")
    } else {
        format!(
            "🚀 Check-in #{id} started. Everyone it's for gets one message in their Kummerkasten chat. 📬"
        )
    })
}

async fn runs(kk: &Kk) -> Result<String> {
    let zone = kk.engine.zone;
    let list = kk.engine.db.run(|c| checkins::overview(c)).await?;
    if list.is_empty() {
        return Ok("📊 No check-ins yet. `!kk checkin start cleaning`".to_owned());
    }
    let today = zone.date(Utc::now());
    let mut out = vec!["📊 **Check-ins**".to_owned()];
    for checkins::RunRow {
        id,
        emoji,
        title,
        open,
        closes_at,
        responses,
    } in list
    {
        let state = if open {
            format!("🟢 open until {}", zone.day_time_label(closes_at, today))
        } else {
            "🔒 closed".to_owned()
        };
        out.push(format!(
            "#{id} {emoji} {} · {state} · 💬 {responses} answer(s)",
            literal(&title)
        ));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_commands_parse() {
        assert_eq!(
            edit_of("edit", &["How", "satisfied?"]),
            Some(Edit::Text("How satisfied?".into()))
        );
        assert_eq!(edit_of("optional", &[]), Some(Edit::Required(false)));
        assert_eq!(edit_of("metric", &["none"]), Some(Edit::Metric(None)));
        assert_eq!(
            edit_of("scale", &["0", "10"]),
            Some(Edit::Scale { min: 0, max: 10 })
        );
        assert_eq!(edit_of("frobnicate", &[]), None);
        assert_eq!(
            choice_edit(
                "add",
                &["problem", "cleaning.reminders", "🔔", "Reminders", "fail"]
            ),
            Some(Edit::AddChoice {
                emoji: "🔔".into(),
                label: "Reminders fail".into(),
                problem: Some("cleaning.reminders".into()),
                is_other: false
            })
        );
        assert_eq!(
            choice_edit("add", &["other", "Something", "else"]),
            Some(Edit::AddChoice {
                emoji: String::new(),
                label: "Something else".into(),
                problem: None,
                is_other: true
            })
        );
        assert_eq!(
            choice_edit("remove", &["2"]),
            Some(Edit::RemoveChoice { n: 2 })
        );
    }
}
