//! `!kk …`: admin commands, only reachable from an admin's verified,
//! encrypted direct chat (`mxbot_common::admin::Dispatch::AdminDm`).

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveTime, Utc};
use mxbot_common::matrix_sdk::{
    ruma::{OwnedRoomId, OwnedUserId},
    RoomState,
};

use crate::{
    catalog::{self, Kind},
    dm::{self, Kk},
    lifecycle,
    plenum::Recurrence,
    plenums::{self, Plenum},
    store, time,
};

/// For `!admin`'s list.
pub const SHORT_HELP: &str =
    "!kk · Kummerkasten: rooms, plenums, rhythm, topics, check-ins, ideas, measures (`!kk help`)";

pub const HELP: &str = "📮 **Kummerkasten admin**

🔐 **Rooms**: members of these may use the Kummerkasten
`!kk rooms` · `!kk rooms add <!room:server> [label]` · `!kk rooms remove <!room:server>`
`!kk entry <!room:server>`: post a 📮 entry message there
`!kk invite-all`: open a chat for everyone in the rooms who has none

🗓️ **Plenums**
`!kk plenums`: the next ones, with their numbers
`!kk plenum add 2026-11-14 [18:00]` · `!kk plenum move 12 2026-11-14 [18:00] [because <reason>]`
   a move keeps everything attached (round, feedback, check-ins); at any notice
`!kk plenum cancel 12` · `!kk plenum remove 12`
`!kk plenum complete 12`: it took place, so close the round, delete what was to be
   deleted, clear the chats (a date passing alone never does that)
`!kk plenum reopen 12`: undo a completion by mistake
`!kk rule` · `!kk rule set sun 1,3,5 19:00 [starting 2026-12-01]`
   also `sun 19:00` (weekly), `every 2 sun 19:00 from 2026-11-01`
`!kk round`: the current round in numbers

🗂️ **Catalog** (`topic` · `location` · `problem`)
`!kk topics` · `!kk locations` · `!kk problems`
`!kk topic add <key> <emoji> <label> [under <parent>] [problem <key>]`
`!kk location add <key> <emoji> <label> [under <parent>] [floor]`
`!kk problem add <key> <topic> <label>`
`!kk topic rename <key> [emoji] <label>` · `!kk topic retire <key>` · `!kk topic restore <key>`

⚙️ `!admin settings`: privacy_threshold, session_minutes, summary_hours_before,
   auto_complete (off: plenums are completed by an admin), complete_at";

/// No such command.
#[derive(Debug)]
struct Unknown;

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("I don't know that one. See `!kk help`.")
    }
}

impl std::error::Error for Unknown {}

/// Most commands one message may carry.
const MAX_COMMANDS: usize = 30;

/// One message, one or more commands: every line starting with `!kk` begins
/// a command; other lines continue the one before (a long text may wrap).
fn split_commands(body: &str) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    for line in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let starts = line
            .split_whitespace()
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("!kk"));
        match commands.last_mut() {
            Some(last) if !starts => {
                last.push(' ');
                last.push_str(line);
            }
            _ => commands.push(line.to_owned()),
        }
    }
    commands
}

/// The command as a short heading for its answer.
fn heading(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().collect();
    let shown = words.len().min(5);
    let more = if words.len() > shown { " …" } else { "" };
    format!("`{}`{more}", words[..shown].join(" "))
}

/// Runs the commands of a message one after the other and answers once.
/// Stops at the first that fails: later ones often build on it (a question,
/// then its choices).
pub async fn run(kk: &Kk, sender: &OwnedUserId, body: &str) -> String {
    let commands = split_commands(body);
    if commands.len() > MAX_COMMANDS {
        return format!(
            "😕 At most {MAX_COMMANDS} commands per message. This one has {}. Nothing was run.",
            commands.len()
        );
    }
    if let [command] = commands.as_slice() {
        return match dispatch(kk, sender, command).await {
            Ok(reply) => reply,
            Err(error) if error.is::<Unknown>() => format!("🤔 I don't know that one.\n\n{HELP}"),
            Err(error) => format!("😕 {error:#}"),
        };
    }
    let mut out = Vec::new();
    for (i, command) in commands.iter().enumerate() {
        let head = format!("**{}.** {}", i + 1, heading(command));
        match dispatch(kk, sender, command).await {
            Ok(reply) => out.push(format!("{head}\n{reply}")),
            Err(error) => {
                out.push(format!("{head}\n😕 {error:#}"));
                let rest = commands.len() - i - 1;
                if rest > 0 {
                    out.push(format!(
                        "⏸️ Stopped here. The {rest} command(s) after it were not run. Fix this one, then send it and the rest again."
                    ));
                }
                break;
            }
        }
    }
    out.join("\n\n")
}

async fn dispatch(kk: &Kk, sender: &OwnedUserId, line: &str) -> Result<String> {
    let args: Vec<&str> = line.split_whitespace().skip(1).collect();
    let actor = sender.to_string();
    if let Some(reply) = crate::admin_checkins::dispatch(kk, &actor, &args).await {
        return reply;
    }
    if let Some(reply) = crate::admin_results::dispatch(kk, &actor, &args).await {
        return reply;
    }
    let zone = kk.engine.zone;
    let now = Utc::now();
    match args.as_slice() {
        [] | ["help"] => Ok(format!(
            "{HELP}\n\n{}\n\n{}",
            crate::admin_checkins::HELP,
            crate::admin_results::HELP
        )),
        ["rooms"] => {
            let rooms = kk.engine.db.run(|c| store::authorized_rooms(c)).await?;
            if rooms.is_empty() {
                return Ok("🔐 No rooms yet. `!kk rooms add <!room:server> House`".to_owned());
            }
            let mut out = vec!["🔐 **Authorized rooms**".to_owned()];
            for (id, label) in rooms {
                let joined = OwnedRoomId::try_from(id.as_str())
                    .ok()
                    .and_then(|r| kk.client.get_room(&r))
                    .is_some_and(|r| r.state() == RoomState::Joined);
                out.push(format!(
                    "• {label} `{id}`{}",
                    if joined {
                        ""
                    } else {
                        " · ⚠️ I'm not in it. Invite me"
                    }
                ));
            }
            Ok(out.join("\n"))
        }
        ["rooms", "add", room, label @ ..] => {
            let room_id =
                OwnedRoomId::try_from(*room).context("That isn't a room ID (!…:server).")?;
            let label = if label.is_empty() {
                room.to_string()
            } else {
                label.join(" ")
            };
            let (r, l, a) = (room_id.to_string(), label.clone(), actor.clone());
            kk.engine
                .db
                .run(move |c| store::add_room(c, &r, &l, &a))
                .await?;
            kk.rooms.insert(room_id.clone());
            let joined = kk
                .client
                .get_room(&room_id)
                .is_some_and(|r| r.state() == RoomState::Joined);
            let joined = joined
                || mxbot_common::rooms::join_with_retry(&kk.client, &room_id, Some(sender)).await;
            Ok(format!(
                "✅ Members of **{label}** may use the Kummerkasten now.{}",
                if joined {
                    ""
                } else {
                    "\n📨 Invite me to the room, so I can see who's in it."
                }
            ))
        }
        ["rooms", "remove", room] => {
            let r = room.to_string();
            let removed = kk.engine.db.run(move |c| store::remove_room(c, &r)).await?;
            if let Ok(id) = OwnedRoomId::try_from(*room) {
                kk.rooms.remove(&id);
            }
            Ok(if removed {
                "✅ Removed. Its members keep access only through other authorized rooms."
                    .to_owned()
            } else {
                "🤔 That room wasn't authorized.".to_owned()
            })
        }
        ["entry", room] => {
            let room_id = OwnedRoomId::try_from(*room).context("That isn't a room ID.")?;
            if !kk.authorized_rooms().await.contains(&room_id) {
                bail!("Authorize the room first: `!kk rooms add {room}`");
            }
            let room = kk
                .client
                .get_room(&room_id)
                .filter(|r| r.state() == RoomState::Joined)
                .context("I'm not in that room. Invite me first.")?;
            dm::post_entry(kk, &room).await?;
            Ok("📮 Posted. Tapping 📮 there opens a private chat.".to_owned())
        }
        ["invite-all"] => invite_all(kk).await,
        ["plenums"] => {
            let (list, previous) = kk
                .engine
                .db
                .run(move |c| {
                    plenums::materialize(c, zone, now)?;
                    Ok((
                        plenums::listing(c, now - chrono::Duration::days(1), 10)?,
                        plenums::previous(c)?,
                    ))
                })
                .await?;
            let mut out = vec!["🗓️ **Plenums**".to_owned()];
            if let Some(p) = previous {
                out.push(format!("⏮️ Previous: {}", line_of(kk, &p, now)));
            }
            out.extend(list.iter().map(|p| line_of(kk, p, now)));
            Ok(out.join("\n"))
        }
        ["plenum", "add", date, rest @ ..] => {
            let at = when(kk, date, rest.first().copied(), now).await?;
            let id = kk
                .engine
                .db
                .run(move |c| plenums::add(c, at, &actor, zone, now))
                .await?;
            Ok(format!(
                "➕ Plenum #{id}: {}",
                zone.day_time_label(at, zone.date(now))
            ))
        }
        ["plenum", "move", id, date, rest @ ..] => {
            let id = number(id)?;
            let (time_arg, reason) = match rest {
                [t, more @ ..] if time::parse_hm(t).is_ok() => (Some(*t), more),
                more => (None, more),
            };
            let reason = match reason {
                ["because" | "reason", words @ ..] | words => words.join(" "),
            };
            let at = when(kk, date, time_arg, now).await?;
            let announced = kk
                .engine
                .db
                .run(move |c| {
                    let r = (!reason.is_empty()).then_some(reason.as_str());
                    plenums::reschedule(c, id, at, r, &actor, zone, now)?;
                    Ok(plenums::get(c, id)?.and_then(|p| p.announced_at).is_some())
                })
                .await?;
            kk.wake.notify_one();
            Ok(format!(
                "🔄 Plenum #{id} moved to {}. Only this one; the rhythm stays.\n\
                 🌱 Its round stays open until it is completed; feedback to delete after \
                 it, its check-ins and everything else move with it.{}",
                zone.day_time_label(at, zone.date(now)),
                if announced {
                    "\n📮 Its summary went out already. A short update follows."
                } else {
                    ""
                }
            ))
        }
        ["plenum", "cancel", id] => {
            let id = number(id)?;
            kk.engine
                .db
                .run(move |c| plenums::cancel(c, id, &actor, now))
                .await?;
            Ok(format!(
                "⏭️ Plenum #{id} is cancelled. Its round carries on to the next plenum."
            ))
        }
        ["plenum", "remove", id] => {
            let id = number(id)?;
            let removed = kk
                .engine
                .db
                .run(move |c| plenums::remove(c, id, &actor, now))
                .await?;
            Ok(match removed {
                plenums::Removed::Deleted => format!("🗑️ Plenum #{id} is gone."),
                plenums::Removed::Cancelled => {
                    format!(
                        "⏭️ Plenum #{id} comes from the rhythm, so it's cancelled (and stays so)."
                    )
                }
            })
        }
        ["plenum", "complete", id] => {
            let id = number(id)?;
            let done = kk
                .engine
                .db
                .run(move |c| lifecycle::complete(c, id, &actor, now))
                .await?;
            kk.wake.notify_one();
            Ok(format!(
                "✅ Plenum #{id} completed: {} submission(s) deleted as asked, {} kept. \
                 Chats are being cleared, and a new round has started. 🌱",
                done.deleted, done.kept
            ))
        }
        ["plenum", "reopen", id] => {
            let id = number(id)?;
            kk.engine
                .db
                .run(move |c| lifecycle::reopen(c, id, &actor, now))
                .await?;
            Ok(format!(
                "↩️ Plenum #{id} is open again. What was already deleted or cleared stays gone."
            ))
        }
        ["rule"] => {
            let rules = kk.engine.db.run(|c| plenums::rules(c)).await?;
            let today = zone.date(now);
            let mut out = vec!["🔁 **Plenum rhythm**".to_owned()];
            for rule in rules {
                let marker = if rule.effective_from > today {
                    "from"
                } else {
                    "since"
                };
                out.push(format!(
                    "• {marker} {}: {}",
                    rule.effective_from.format("%-d %b %Y"),
                    rule.recurrence.describe()
                ));
            }
            Ok(out.join("\n"))
        }
        ["rule", "set", spec @ ..] => {
            let mut spec: Vec<&str> = spec.to_vec();
            let today = zone.date(now);
            let mut starting = today + chrono::Duration::days(1);
            if let Some(pos) = spec.iter().position(|a| *a == "starting") {
                starting = time::parse_date(spec.get(pos + 1).context("`starting` needs a date")?)?;
                spec.drain(pos..pos + 2);
            }
            let rule = Recurrence::parse(&spec, today)?;
            let describe = rule.describe();
            let change = kk
                .engine
                .db
                .run(move |c| plenums::set_rule(c, &rule, starting, &actor, zone, now))
                .await?;
            Ok(format!(
                "🔁 From {}: {describe}.\n{} upcoming plenum(s) replaced; {} moved by hand kept.",
                starting.format("%-d %B %Y"),
                change.replaced,
                change.kept
            ))
        }
        ["round"] => {
            let summary = kk
                .engine
                .db
                .run(move |c| {
                    let round = plenums::open_round(c)?;
                    let Some(round) = round else {
                        return Ok("🌱 No round is open. No plenum is scheduled.".to_owned());
                    };
                    let plenum =
                        plenums::get(c, round.plenum_id)?.context("round without plenum")?;
                    let (total, delete_after): (i64, i64) = c.query_row(
                        "SELECT count(*), coalesce(sum(retention = 'delete_after_plenum'), 0)
                         FROM submissions WHERE round_id = ?1",
                        [round.id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    Ok(format!(
                        "🌱 **Round #{}** · for plenum #{} on {}\n💬 {total} submission(s), \
                         {delete_after} to be deleted after it",
                        round.id,
                        plenum.id,
                        zone.day_time_label(plenum.starts_at, zone.date(now))
                    ))
                })
                .await?;
            Ok(summary)
        }
        ["topics"] => list(kk, Kind::Topic, "🗂️ **Topics**").await,
        ["locations"] => list(kk, Kind::Location, "📍 **Locations**").await,
        ["problems"] => list(kk, Kind::Problem, "⚠️ **Canonical problems**").await,
        [kind @ ("topic" | "location" | "problem"), verb, rest @ ..] => {
            let kind = match *kind {
                "topic" => Kind::Topic,
                "location" => Kind::Location,
                _ => Kind::Problem,
            };
            catalog_edit(kk, kind, verb, rest).await
        }
        _ => Err(Unknown.into()),
    }
}

fn number(s: &str) -> Result<i64> {
    s.trim_start_matches('#')
        .parse()
        .with_context(|| format!("{s:?} isn't a plenum number"))
}

/// `date` at `time`, or at the current rule's time.
async fn when(
    kk: &Kk,
    date: &str,
    time_arg: Option<&str>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let zone = kk.engine.zone;
    let date = time::parse_date(date)?;
    let at: NaiveTime = match time_arg {
        Some(t) => time::parse_hm(t)?,
        None => kk
            .engine
            .db
            .run(move |c| plenums::rule_on(c, zone.date(now)))
            .await?
            .map(|r| r.recurrence.time())
            .unwrap_or_else(|| NaiveTime::from_hms_opt(19, 0, 0).unwrap()),
    };
    Ok(zone.at(date, at))
}

fn line_of(kk: &Kk, p: &Plenum, now: DateTime<Utc>) -> String {
    let zone = kk.engine.zone;
    let today = zone.date(now);
    let mut notes = Vec::new();
    if p.rule_id.is_none() {
        notes.push("➕ extra".to_owned());
    }
    if p.rescheduled {
        notes.push(format!(
            "🔄 moved from {}{}",
            zone.day_time_label(p.original_starts_at, today),
            p.reschedule_reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ));
    }
    if p.awaiting_completion(now) {
        notes.push(format!(
            "⏳ awaiting completion. `!kk plenum complete {}` once it took place",
            p.id
        ));
    }
    format!(
        "#{} · {} · {}{}",
        p.id,
        zone.day_time_label(p.starts_at, zone.date(now)),
        p.status.label(),
        if notes.is_empty() {
            String::new()
        } else {
            format!(" · {}", notes.join(" · "))
        }
    )
}

async fn list(kk: &Kk, kind: Kind, title: &'static str) -> Result<String> {
    let body = kk.engine.db.run(move |c| catalog::listing(c, kind)).await?;
    Ok(format!("{title}\n{body}"))
}

/// Pull `under <x>`, `problem <x>` and `floor` out of `rest`.
fn options<'a>(rest: &[&'a str]) -> (Vec<&'a str>, Option<&'a str>, Option<&'a str>, bool) {
    let mut words = Vec::new();
    let (mut under, mut problem, mut floor) = (None, None, false);
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "under" if i + 1 < rest.len() => {
                under = Some(rest[i + 1]);
                i += 2;
            }
            "problem" if i + 1 < rest.len() => {
                problem = Some(rest[i + 1]);
                i += 2;
            }
            "floor" => {
                floor = true;
                i += 1;
            }
            word => {
                words.push(word);
                i += 1;
            }
        }
    }
    (words, under, problem, floor)
}

fn looks_like_emoji(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| !c.is_alphanumeric() && !c.is_ascii_punctuation())
}

async fn catalog_edit(kk: &Kk, kind: Kind, verb: &str, rest: &[&str]) -> Result<String> {
    let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
    let verb = verb.to_owned();
    kk.engine
        .db
        .run(move |c| {
            let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
            match (verb.as_str(), kind, rest.as_slice()) {
                ("add", Kind::Problem, [key, topic, label @ ..]) if !label.is_empty() => {
                    catalog::add_problem(c, key, topic, &label.join(" "))?;
                    Ok(format!(
                        "⚠️ Problem `{key}` added. Feedback and check-ins can point at it."
                    ))
                }
                ("add", Kind::Topic | Kind::Location, [key, emoji, more @ ..]) => {
                    let (label, under, problem, floor) = options(more);
                    if label.is_empty() {
                        bail!("A label, please.");
                    }
                    let label = label.join(" ");
                    if kind == Kind::Topic {
                        catalog::add_topic(c, under, key, emoji, &label, problem)?;
                    } else {
                        catalog::add_location(c, under, key, emoji, &label, floor)?;
                    }
                    Ok(format!("✅ {emoji} {label} added."))
                }
                ("rename", _, [key, first, more @ ..]) => {
                    let (emoji, label) = if looks_like_emoji(first) && !more.is_empty() {
                        (Some(*first), more.join(" "))
                    } else {
                        (
                            None,
                            std::iter::once(*first)
                                .chain(more.iter().copied())
                                .collect::<Vec<_>>()
                                .join(" "),
                        )
                    };
                    catalog::rename(c, kind, key, emoji, &label)?;
                    Ok(format!(
                        "✏️ `{key}` now reads “{label}”. History keeps pointing at it."
                    ))
                }
                ("retire", _, [key]) => {
                    catalog::set_active(c, kind, key, false)?;
                    Ok(format!(
                        "🗄️ `{key}` retired. No longer offered, kept in history."
                    ))
                }
                ("restore", _, [key]) => {
                    catalog::set_active(c, kind, key, true)?;
                    Ok(format!("♻️ `{key}` is back."))
                }
                _ => bail!("See `!kk help` for the catalog commands."),
            }
        })
        .await
}

/// Open a chat for everyone in the authorized rooms who has none yet.
async fn invite_all(kk: &Kk) -> Result<String> {
    let hs = dm::Matrix { kk, only: None };
    let out =
        crate::reconcile::reconcile(&kk.engine.db, &hs, &crate::checkins::Target::House).await?;
    Ok(format!(
        "📨 {} eligible · {} with a chat · {} invited earlier, not joined yet · {} newly invited{}",
        out.eligible,
        out.ready.len(),
        out.pending,
        out.created,
        if out.failed > 0 {
            format!(" · ⚠️ {} failed (see the log)", out.failed)
        } else {
            String::new()
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_with_several_lines_carries_several_commands() {
        let body = "!kk metric add cleaning.toilets cleaning How clean (1–5)
!kk problem add cleaning.paper cleaning Paper missing

  !KK survey toilets intro How is it
going for you?
";
        assert_eq!(
            split_commands(body),
            [
                "!kk metric add cleaning.toilets cleaning How clean (1–5)",
                "!kk problem add cleaning.paper cleaning Paper missing",
                "!KK survey toilets intro How is it going for you?",
            ]
        );
        // One command, as before.
        assert_eq!(split_commands("  !kk round  "), ["!kk round"]);
        assert_eq!(
            heading("!kk survey toilets choice 2 add problem x"),
            "`!kk survey toilets choice 2` …"
        );
        assert_eq!(heading("!kk round"), "`!kk round`");
    }

    #[test]
    fn catalog_options_are_picked_out() {
        let (words, under, problem, floor) = options(&[
            "Dishwasher",
            "duty",
            "under",
            "kitchen",
            "problem",
            "kitchen.dishes",
        ]);
        assert_eq!(words, ["Dishwasher", "duty"]);
        assert_eq!(under, Some("kitchen"));
        assert_eq!(problem, Some("kitchen.dishes"));
        assert!(!floor);
        assert!(options(&["5th", "floor", "floor"]).3);
        assert!(looks_like_emoji("🍽️"));
        assert!(!looks_like_emoji("Dishes"));
    }
}
