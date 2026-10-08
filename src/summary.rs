//! 📮 The day-before-plenum summary, and the short update when a plenum
//! that was announced moves or is cancelled.
//!
//! When: the next scheduled plenum, `before` ahead of its *current* start
//! (it follows every move), once — `announced_at` is set before anything is
//! sent, so a crash means a missed summary, never two. A cancelled plenum
//! is never summarized. If an announced plenum moves or is cancelled,
//! `update_pending` asks for one short update (taken the same way); a move
//! by more than [`plenums::FRESH_SUMMARY_AFTER`] also clears `announced_at`,
//! so the new date gets its own summary.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    catalog::Catalog,
    ideas, plenums, stats,
    time::{self, Zone},
};

/// The plenum whose summary is due now, marked as announced — or `None`.
/// Due: the earliest scheduled plenum, not announced yet, starting in at
/// most `before` but not started yet.
pub fn take_summary(
    conn: &Connection,
    now: DateTime<Utc>,
    before: Duration,
) -> Result<Option<i64>> {
    let Some(next) = plenums::next(conn)? else {
        return Ok(None);
    };
    if next.announced_at.is_some() || next.starts_at <= now || next.starts_at - before > now {
        return Ok(None);
    }
    let marked = conn.execute(
        "UPDATE plenums SET announced_at = ?2
         WHERE id = ?1 AND status = 'scheduled' AND announced_at IS NULL",
        params![next.id, time::to_text(now)],
    )?;
    Ok((marked == 1).then_some(next.id))
}

/// What happened to an announced plenum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Update {
    Moved,
    Cancelled,
}

/// Announced plenums that moved or were cancelled since: their one update
/// each, cleared before it is sent.
pub fn take_updates(conn: &Connection) -> Result<Vec<(i64, Update)>> {
    let due: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, update_pending FROM plenums WHERE update_pending IS NOT NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::new();
    for (id, what) in due {
        conn.execute(
            "UPDATE plenums SET update_pending = NULL WHERE id = ?1",
            [id],
        )?;
        out.push((
            id,
            if what == "cancelled" {
                Update::Cancelled
            } else {
                Update::Moved
            },
        ));
    }
    Ok(out)
}

// ── Texts ───────────────────────────────────────────────────────────────────

/// The day-before summary of plenum `id`: playful, compact, aggregate
/// only — counts, topics and problems with at least `k` behind them, idea
/// titles (shared on purpose). Never anyone's words, never small groups.
pub fn render(
    conn: &Connection,
    cat: &Catalog,
    id: i64,
    k: i64,
    zone: Zone,
    now: DateTime<Utc>,
) -> Result<String> {
    let plenum = plenums::get(conn, id)?.with_context(|| format!("no plenum #{id}"))?;
    let today = zone.date(now);
    let when = match time::in_days(today, zone.date(plenum.starts_at)).as_str() {
        "today" => "today".to_owned(),
        "tomorrow" => "tomorrow".to_owned(),
        _ => format!("on {}", zone.day_label(plenum.starts_at, today)),
    };
    let round: Option<i64> = conn
        .query_row("SELECT id FROM rounds WHERE plenum_id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let mut lines = vec![
        format!("📮 **Kummerkasten · Plenum {when}** ✨"),
        format!("🗓️ {}", zone.day_time_label(plenum.starts_at, today)),
        String::new(),
    ];
    let far = stats::all_days();
    if let Some(round) = round {
        let (feedback, responses): (i64, i64) = conn.query_row(
            "SELECT (SELECT count(*) FROM submissions WHERE round_id = ?1 AND for_plenum = 1),
                    (SELECT count(*) FROM responses s JOIN survey_runs r ON r.id = s.run_id
                     WHERE r.round_id = ?1)",
            [round],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let ideas_shared = stats::round_numbers(conn, round, far)?.ideas;
        lines.push(format!("💬 {feedback} feedback submissions"));
        lines.push(format!("📊 {responses} check-in responses"));
        lines.push(format!("💡 {ideas_shared} ideas shared"));

        let mut topics: Vec<(i64, i64)> = stats::by_topic(conn, round, far)?
            .into_iter()
            .filter(|(t, n)| *n >= k && cat.topic(*t).is_some_and(|t| !t.is_idea))
            .collect();
        topics.sort_by_key(|(t, n)| (-n, *t));
        if !topics.is_empty() {
            lines.push(String::new());
            lines.push("🔥 **Most mentioned topics**".to_owned());
            for (t, _) in topics.iter().take(3) {
                lines.push(cat.topic(*t).map(|t| t.title()).unwrap_or_default());
            }
        }
        let issues = recurring(conn, cat, round, k)?;
        if !issues.is_empty() {
            lines.push(String::new());
            lines.push("⚠️ **Recurring issues**".to_owned());
            lines.extend(issues.iter().take(4).cloned());
        }
    }
    let popular: Vec<ideas::Idea> = ideas::board(conn, 10)?
        .into_iter()
        .filter(|i| i.support > 0)
        .take(3)
        .collect();
    if !popular.is_empty() {
        lines.push(String::new());
        lines.push("💡 **Popular ideas**".to_owned());
        for i in &popular {
            lines.push(format!(
                "{} · 👍 {}",
                crate::markup::literal(&i.title),
                i.support
            ));
        }
    }
    let discussing: Vec<String> = ideas::board(conn, 50)?
        .into_iter()
        .filter(|i| i.status == ideas::Status::Discussing)
        .take(3)
        .map(|i| format!("• {}", crate::markup::literal(&i.title)))
        .collect();
    if !discussing.is_empty() {
        lines.push(String::new());
        lines.push("🗣️ **Suggested for discussion**".to_owned());
        lines.extend(discussing);
    }
    lines.push(String::new());
    lines.push("📮 Open the Kummerkasten · 📈 Results · 💡 Ideas — tap below.".to_owned());
    Ok(lines.join("\n"))
}

/// Problems raised this round by at least `k` submissions, or picked by at
/// least a quarter of respondents in the latest closed check-in asking
/// about them (with at least `k` respondents). Two signals, both named.
fn recurring(conn: &Connection, cat: &Catalog, round: i64, k: i64) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mentions = stats::mentions(conn, round, None, stats::all_days())?;
    let label = |pid: i64| {
        cat.topics
            .iter()
            .find(|t| t.problem_id == Some(pid))
            .map(|t| t.title())
    };
    let mut seen = std::collections::BTreeSet::new();
    for root in cat.topic_children(None) {
        for p in stats::problem_series(conn, root.id, k)? {
            let Some(latest) = p.shares.last().and_then(|s| s.percent) else {
                continue;
            };
            if latest >= 25 && seen.insert(p.problem_id) {
                let mut line = label(p.problem_id).unwrap_or(p.problem_label.clone());
                line.push_str(&format!(" · 📊 {latest}% in the latest check-in"));
                if let Some(n) = mentions.get(&p.problem_id).filter(|n| **n >= k) {
                    line.push_str(&format!(" · 📮 {n} submissions"));
                }
                out.push(line);
            }
        }
    }
    let mut spontaneous: Vec<(&i64, &i64)> = mentions
        .iter()
        .filter(|(p, n)| **n >= k && !seen.contains(*p))
        .collect();
    spontaneous.sort_by_key(|(p, n)| (-**n, **p));
    for (pid, n) in spontaneous {
        if let Some(l) = label(*pid) {
            out.push(format!("{l} · 📮 {n} submissions"));
        }
    }
    Ok(out)
}

/// The short update for an announced plenum that moved or was cancelled.
pub fn update_text(
    conn: &Connection,
    id: i64,
    what: Update,
    zone: Zone,
    now: DateTime<Utc>,
) -> Result<String> {
    let plenum = plenums::get(conn, id)?.with_context(|| format!("no plenum #{id}"))?;
    let today = zone.date(now);
    // The date people were last told (however often it moved since).
    let before: Option<String> = conn
        .query_row(
            "SELECT for_starts_at FROM plenum_announcements WHERE plenum_id = ?1
             ORDER BY rowid DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let before = before
        .and_then(|t| time::parse_text(&t).ok())
        .unwrap_or(plenum.original_starts_at);
    Ok(match what {
        Update::Moved => {
            let lead = if time::in_days(today, zone.date(before)) == "tomorrow" {
                "Tomorrow's plenum has been moved."
            } else {
                "The plenum has been moved."
            };
            let mut text = format!(
                "📮 **Plenum update** 🗓️\n\n{lead}\n\n~~{}~~\n➡️ **{}**",
                zone.day_time_label(before, today),
                zone.day_time_label(plenum.starts_at, today)
            );
            if let Some(reason) = &plenum.reschedule_reason {
                text.push_str(&format!("\n💬 {}", crate::markup::literal(reason)));
            }
            text.push_str("\n\nThe Kummerkasten round stays open until the rescheduled plenum. 🌱");
            text
        }
        Update::Cancelled => {
            let next = plenums::next(conn)?;
            let mut text = format!(
                "📮 **Plenum update** ❌\n\nThe plenum on {} won't take place.",
                zone.day_time_label(plenum.starts_at, today)
            );
            match next {
                Some(n) => text.push_str(&format!(
                    "\n\nThe Kummerkasten round carries on until the next plenum on **{}**. 🌱",
                    zone.day_time_label(n.starts_at, today)
                )),
                None => text.push_str("\n\nThe Kummerkasten round stays open. 🌱"),
            }
            text
        }
    })
}

/// The summary as it should read now that `what` happened (for editing
/// the posted one in place): a line on top, the rest unchanged.
pub fn amended(
    body: &str,
    what: Update,
    plenum: &plenums::Plenum,
    zone: Zone,
    now: DateTime<Utc>,
) -> String {
    let note = match what {
        Update::Moved => format!(
            "🗓️ **Moved to {}** — see the update.",
            zone.day_time_label(plenum.starts_at, zone.date(now))
        ),
        Update::Cancelled => "❌ **This plenum won't take place** — see the update.".to_owned(),
    };
    // A second move replaces the first note.
    let rest = body
        .strip_prefix("🗓️ **Moved to ")
        .or_else(|| body.strip_prefix("❌ **This plenum"))
        .and_then(|r| r.split_once("\n\n").map(|(_, rest)| rest))
        .unwrap_or(body);
    format!("{note}\n\n{rest}")
}

// ── Where they were posted ──────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Summary,
    Update,
}

/// A summary or update was posted as `event_id` in `room_id`, telling
/// people the plenum is at `for_starts_at`.
pub fn record(
    conn: &Connection,
    plenum_id: i64,
    room_id: &str,
    event_id: &str,
    kind: Kind,
    body: &str,
    for_starts_at: DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO plenum_announcements
           (room_id, event_id, plenum_id, kind, body, for_starts_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            room_id,
            event_id,
            plenum_id,
            match kind {
                Kind::Summary => "summary",
                Kind::Update => "update",
            },
            body,
            time::to_text(for_starts_at)
        ],
    )?;
    Ok(())
}

/// The posted summaries of plenum `id`: `(room_id, event_id, body)`.
pub fn summaries(conn: &Connection, id: i64) -> Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT room_id, event_id, body FROM plenum_announcements
         WHERE plenum_id = ?1 AND kind = 'summary' ORDER BY room_id",
    )?;
    let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        checkins::{self, Deadline, StartOptions},
        lifecycle,
        plenums::tests::{at, with_default_rule, zone},
        secrets::Secrets,
        submissions::{self, tests::new, Identity, Retention},
    };
    use chrono::NaiveTime;

    fn day() -> Duration {
        Duration::hours(24)
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// The default rule's first plenum after 5 Oct 2026: Sunday 18 Oct, 19:00.
    fn october() -> (Connection, plenums::Plenum) {
        let now = at(2026, 10, 5, 12);
        let conn = with_default_rule(now);
        let p = plenums::next(&conn).unwrap().unwrap();
        assert_eq!(p.starts_at, at(2026, 10, 18, 19));
        (conn, p)
    }

    #[test]
    fn moved_before_any_announcement_the_summary_follows_the_new_date() {
        let (mut conn, p) = october();
        // Saturday: Sunday's plenum moves to Monday 19 Oct.
        let saturday = at(2026, 10, 17, 10);
        plenums::reschedule(
            &mut conn,
            p.id,
            at(2026, 10, 19, 19),
            Some("room is booked"),
            "@admin:x",
            zone(),
            saturday,
        )
        .unwrap();
        let moved = plenums::get(&conn, p.id).unwrap().unwrap();
        assert_eq!(moved.id, p.id);
        assert_eq!(moved.starts_at, at(2026, 10, 19, 19));
        assert_eq!(moved.original_starts_at, at(2026, 10, 18, 19));
        assert_eq!(moved.reschedule_reason.as_deref(), Some("room is booked"));
        // Not on Saturday evening (a day before the old date) …
        assert_eq!(
            take_summary(&conn, at(2026, 10, 17, 20), day()).unwrap(),
            None
        );
        // … but on Sunday evening, a day before the new one — once.
        assert_eq!(
            take_summary(&conn, at(2026, 10, 18, 20), day()).unwrap(),
            Some(p.id)
        );
        assert_eq!(
            take_summary(&conn, at(2026, 10, 18, 21), day()).unwrap(),
            None
        );
        assert!(take_updates(&conn).unwrap().is_empty());
    }

    #[test]
    fn moved_after_the_summary_an_update_goes_out_once() {
        let (mut conn, p) = october();
        assert_eq!(
            take_summary(&conn, at(2026, 10, 17, 20), day()).unwrap(),
            Some(p.id)
        );
        // Later that evening it moves by a day.
        plenums::reschedule(
            &mut conn,
            p.id,
            at(2026, 10, 19, 19),
            None,
            "@admin:x",
            zone(),
            at(2026, 10, 17, 22),
        )
        .unwrap();
        assert_eq!(take_updates(&conn).unwrap(), [(p.id, Update::Moved)]);
        assert!(take_updates(&conn).unwrap().is_empty());
        // No second full summary for a move by a day.
        assert_eq!(
            take_summary(&conn, at(2026, 10, 18, 20), day()).unwrap(),
            None
        );
    }

    #[test]
    fn moved_far_after_the_summary_gets_a_fresh_one_before_the_new_date() {
        let (mut conn, p) = october();
        take_summary(&conn, at(2026, 10, 17, 20), day()).unwrap();
        // A week later (the following plenum is only on 1 November).
        plenums::reschedule(
            &mut conn,
            p.id,
            at(2026, 10, 25, 19),
            None,
            "@admin:x",
            zone(),
            at(2026, 10, 17, 22),
        )
        .unwrap();
        assert_eq!(take_updates(&conn).unwrap(), [(p.id, Update::Moved)]);
        assert_eq!(
            take_summary(&conn, at(2026, 10, 18, 20), day()).unwrap(),
            None
        );
        assert_eq!(
            take_summary(&conn, at(2026, 10, 24, 20), day()).unwrap(),
            Some(p.id)
        );
    }

    #[test]
    fn moved_on_its_own_day_and_several_times() {
        let (mut conn, p) = october();
        // On Sunday morning, to Sunday evening 20:00, then to Monday, then
        // to Tuesday: always the same plenum, the first date kept.
        for (when, to) in [
            (at(2026, 10, 18, 9), at(2026, 10, 18, 20)),
            (at(2026, 10, 18, 15), at(2026, 10, 19, 19)),
            (at(2026, 10, 19, 8), at(2026, 10, 20, 19)),
        ] {
            plenums::reschedule(&mut conn, p.id, to, None, "@admin:x", zone(), when).unwrap();
            let now = plenums::get(&conn, p.id).unwrap().unwrap();
            assert_eq!(now.starts_at, to);
            assert_eq!(now.original_starts_at, at(2026, 10, 18, 19));
            assert_eq!(plenums::next(&conn).unwrap().unwrap().id, p.id);
            assert_eq!(plenums::open_round(&conn).unwrap().unwrap().plenum_id, p.id);
        }
        assert_eq!(
            count(
                &conn,
                &format!(
                    "SELECT count(*) FROM plenum_changes WHERE plenum_id = {} AND action = 'move'",
                    p.id
                )
            ),
            3
        );
    }

    #[test]
    fn a_move_never_passes_another_plenum() {
        let (mut conn, p) = october();
        let following = plenums::upcoming(&conn, 2).unwrap()[1].clone(); // 1 Nov
        let err = plenums::reschedule(
            &mut conn,
            p.id,
            following.starts_at + Duration::hours(1),
            None,
            "@admin:x",
            zone(),
            at(2026, 10, 10, 9),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancel this one instead"), "{err}");
        // Nor can the later one jump before it.
        assert!(plenums::reschedule(
            &mut conn,
            following.id,
            at(2026, 10, 17, 19),
            None,
            "@admin:x",
            zone(),
            at(2026, 10, 10, 9),
        )
        .is_err());
        assert_eq!(
            plenums::get(&conn, p.id).unwrap().unwrap().starts_at,
            p.starts_at
        );
    }

    #[test]
    fn a_cancelled_plenum_is_never_summarized_and_an_announced_one_gets_an_update() {
        let (mut conn, p) = october();
        plenums::cancel(&mut conn, p.id, "@admin:x", at(2026, 10, 16, 9)).unwrap();
        // The summary due on the 17th would be for the *next* plenum (1 Nov),
        // which isn't due yet.
        assert_eq!(
            take_summary(&conn, at(2026, 10, 17, 20), day()).unwrap(),
            None
        );
        assert!(take_updates(&conn).unwrap().is_empty());
        // The next one: announced, then cancelled → one update, no summary.
        let next = plenums::next(&conn).unwrap().unwrap();
        assert_eq!(
            take_summary(&conn, next.starts_at - Duration::hours(2), day()).unwrap(),
            Some(next.id)
        );
        plenums::cancel(
            &mut conn,
            next.id,
            "@admin:x",
            next.starts_at - Duration::hours(1),
        )
        .unwrap();
        assert_eq!(take_updates(&conn).unwrap(), [(next.id, Update::Cancelled)]);
    }

    #[test]
    fn postponing_keeps_the_round_open_and_deletes_nothing_until_it_is_completed() {
        let (mut conn, p) = october();
        let secrets = Secrets::from_master(&[9; 32]);
        let round = plenums::open_round(&conn).unwrap().unwrap();
        let s = submissions::submit(
            &conn,
            &secrets,
            &new(&conn, Identity::Anonymous, Retention::DeleteAfterPlenum),
            "@a:x",
            None,
            zone().date(at(2026, 10, 5, 12)),
        )
        .unwrap();
        plenums::reschedule(
            &mut conn,
            p.id,
            at(2026, 10, 19, 19),
            None,
            "@admin:x",
            zone(),
            at(2026, 10, 17, 10),
        )
        .unwrap();
        // Still pointing at the same plenum.
        let points_at: i64 = conn
            .query_row(
                "SELECT delete_after_plenum_id FROM submissions WHERE id = ?1",
                [s.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(points_at, p.id);
        // The original Sunday night passes: nothing is cleaned up — even
        // under the auto-complete policy, the plenum isn't due on that date.
        let complete_at = NaiveTime::from_hms_opt(23, 59, 0).unwrap();
        let sunday_night = at(2026, 10, 18, 23) + Duration::minutes(59);
        assert!(lifecycle::due(&conn, zone(), sunday_night, complete_at)
            .unwrap()
            .is_empty());
        assert_eq!(
            plenums::open_round(&conn).unwrap().unwrap(),
            round,
            "the round stays open during the postponement"
        );
        // Monday night it is past its new time: awaiting completion — but
        // still nothing deleted, the round still open.
        let monday_night = at(2026, 10, 19, 22);
        let waiting = plenums::get(&conn, p.id).unwrap().unwrap();
        assert!(waiting.awaiting_completion(monday_night));
        assert_eq!(count(&conn, "SELECT count(*) FROM submissions"), 1);
        assert_eq!(plenums::open_round(&conn).unwrap().unwrap(), round);
        // Completed (it took place): now, and only now.
        let done = lifecycle::complete(&mut conn, p.id, "@admin:x", monday_night).unwrap();
        assert_eq!(done.deleted, 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM submissions"), 0);
        assert_ne!(plenums::open_round(&conn).unwrap().unwrap().id, round.id);
    }

    #[test]
    fn check_in_deadlines_follow_the_plenum_unless_fixed() {
        let (mut conn, p) = october();
        let now = at(2026, 10, 5, 12);
        let remind = Duration::hours(24);
        let start = |conn: &mut Connection, deadline| {
            let opts = StartOptions {
                deadline,
                ..Default::default()
            };
            let plan = checkins::plan(conn, "cleaning", &opts, now, remind).unwrap();
            checkins::start(conn, &plan, "@admin:x", now).unwrap()
        };
        let until_plenum = start(&mut conn, Deadline::Default);
        let day_before = start(&mut conn, Deadline::BeforePlenum(Duration::days(1)));
        let fixed = start(&mut conn, Deadline::Fixed(at(2026, 10, 17, 18)));
        let times = |conn: &Connection, id: i64| {
            let r = checkins::run(conn, id).unwrap();
            (r.closes_at, r.reminder_at)
        };
        assert_eq!(
            times(&conn, until_plenum),
            (at(2026, 10, 18, 19), Some(at(2026, 10, 17, 19)))
        );
        assert_eq!(
            times(&conn, day_before),
            (at(2026, 10, 17, 19), Some(at(2026, 10, 16, 19)))
        );
        assert_eq!(
            times(&conn, fixed),
            (at(2026, 10, 17, 18), Some(at(2026, 10, 16, 18)))
        );
        // Moved to Monday: the relative ones move along, the fixed one not.
        for _ in 0..2 {
            // (twice: rescheduling to the same time again changes nothing)
            plenums::reschedule(
                &mut conn,
                p.id,
                at(2026, 10, 19, 19),
                None,
                "@admin:x",
                zone(),
                at(2026, 10, 10, 9),
            )
            .unwrap();
            assert_eq!(
                times(&conn, until_plenum),
                (at(2026, 10, 19, 19), Some(at(2026, 10, 18, 19)))
            );
            assert_eq!(
                times(&conn, day_before),
                (at(2026, 10, 18, 19), Some(at(2026, 10, 17, 19)))
            );
            assert_eq!(
                times(&conn, fixed),
                (at(2026, 10, 17, 18), Some(at(2026, 10, 16, 18)))
            );
        }
        // The old reminder time comes and goes: nothing for the moved ones.
        let old_reminder = at(2026, 10, 17, 19) + Duration::minutes(1);
        let due: Vec<i64> = checkins::take_reminders(&conn, old_reminder).unwrap();
        assert!(!due.contains(&until_plenum), "{due:?}");
        // At their new times, each reminder once.
        assert!(checkins::take_reminders(&conn, at(2026, 10, 18, 19))
            .unwrap()
            .contains(&until_plenum));
        assert!(checkins::take_reminders(&conn, at(2026, 10, 18, 20))
            .unwrap()
            .is_empty());
        // A reminder already sent stays sent when the plenum moves again.
        plenums::reschedule(
            &mut conn,
            p.id,
            at(2026, 10, 20, 19),
            None,
            "@admin:x",
            zone(),
            at(2026, 10, 18, 20),
        )
        .unwrap();
        assert!(checkins::take_reminders(&conn, at(2026, 10, 19, 20))
            .unwrap()
            .is_empty());
        assert_eq!(times(&conn, until_plenum).0, at(2026, 10, 20, 19));
        // Closing follows the new date (Tuesday 20th), not the old ones: on
        // Monday evening the fixed run and the "day before" one close, the
        // one running until the plenum stays open.
        assert_eq!(checkins::close_due(&conn, at(2026, 10, 19, 20)).unwrap(), 2);
        assert!(!checkins::run(&conn, fixed).unwrap().open);
        assert!(!checkins::run(&conn, day_before).unwrap().open);
        assert!(checkins::run(&conn, until_plenum).unwrap().open);
    }

    #[test]
    fn a_cancelled_plenum_hands_its_check_ins_and_deadlines_to_the_next() {
        let (mut conn, p) = october();
        let now = at(2026, 10, 5, 12);
        let plan = checkins::plan(
            &conn,
            "cleaning",
            &StartOptions::default(),
            now,
            Duration::hours(24),
        )
        .unwrap();
        let run = checkins::start(&mut conn, &plan, "@admin:x", now).unwrap();
        plenums::cancel(&mut conn, p.id, "@admin:x", at(2026, 10, 10, 9)).unwrap();
        let next = plenums::next(&conn).unwrap().unwrap();
        let r = checkins::run(&conn, run).unwrap();
        assert_eq!(r.plenum_id, Some(next.id));
        assert_eq!(r.closes_at, next.starts_at);
    }

    #[test]
    fn the_summary_is_aggregate_only_and_respects_the_threshold() {
        let (mut conn, p) = october();
        let secrets = Secrets::from_master(&[9; 32]);
        let cat = Catalog::load(&conn).unwrap();
        let topic = |key: &str| -> (i64, Option<i64>) {
            conn.query_row(
                "SELECT id, problem_id FROM topics WHERE key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let day = zone().date(at(2026, 10, 5, 12));
        let feedback = |key: &str, body: &str| {
            let (topic_id, problem_id) = topic(key);
            submissions::submit(
                &conn,
                &secrets,
                &submissions::NewSubmission {
                    topic_id,
                    problem_id,
                    location_id: None,
                    body: body.into(),
                    suggestion: None,
                    identity: Identity::Anonymous,
                    retention: Retention::Keep,
                    for_plenum: true,
                },
                "@a:x",
                None,
                day,
            )
            .unwrap();
        };
        for i in 0..4 {
            feedback("cleaning.skipped_turns", &format!("SECRET-CLEANING-{i}"));
        }
        // One noise complaint: under the threshold, not named.
        feedback("noise", "SECRET-NOISE");
        let idea =
            crate::ideas::tests::share(&conn, "Clean the bathrooms twice per week", "cleaning");
        crate::ideas::support(&conn, &secrets, idea, "@b:x").unwrap();
        let text = render(&conn, &cat, p.id, 3, zone(), at(2026, 10, 17, 19)).unwrap();
        assert!(
            text.starts_with("📮 **Kummerkasten · Plenum tomorrow** ✨"),
            "{text}"
        );
        assert!(text.contains("💬 5 feedback submissions"));
        assert!(text.contains("🧹 Cleaning"), "{text}");
        assert!(
            text.contains("People skipping their turn · 📮 4 submissions"),
            "{text}"
        );
        assert!(text.contains("Clean the bathrooms twice per week · 👍 1"));
        // No one's words, no small group.
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("Noise"), "{text}");
        let _ = &mut conn;
    }

    #[test]
    fn an_update_strikes_the_announced_date_and_amends_the_summary() {
        let (mut conn, p) = october();
        take_summary(&conn, at(2026, 10, 17, 19), day()).unwrap();
        record(
            &conn,
            p.id,
            "!house:x",
            "$summary",
            Kind::Summary,
            "📮 **Kummerkasten · Plenum tomorrow** ✨\nrest",
            p.starts_at,
        )
        .unwrap();
        let saturday = at(2026, 10, 17, 21);
        plenums::reschedule(
            &mut conn,
            p.id,
            at(2026, 10, 19, 19),
            Some("room booked"),
            "@admin:x",
            zone(),
            saturday,
        )
        .unwrap();
        let (id, what) = take_updates(&conn).unwrap()[0];
        let text = update_text(&conn, id, what, zone(), saturday).unwrap();
        assert!(text.contains("Tomorrow's plenum has been moved."), "{text}");
        assert!(text.contains("~~Sunday, 18 October · 19:00~~"), "{text}");
        assert!(text.contains("➡️ **Monday, 19 October · 19:00**"), "{text}");
        assert!(text.contains("room booked"));
        assert!(text.contains("stays open until the rescheduled plenum"));
        let (_, _, body) = &summaries(&conn, p.id).unwrap()[0];
        let moved = plenums::get(&conn, p.id).unwrap().unwrap();
        let edited = amended(body, what, &moved, zone(), saturday);
        assert!(edited.starts_with("🗓️ **Moved to Monday, 19 October · 19:00**"));
        assert!(edited.ends_with("rest"));
        // Moved again: the note is replaced, not stacked.
        let again = edited.replace("19:00**", "20:00**");
        assert_eq!(
            amended(&again, what, &moved, zone(), saturday)
                .matches("Moved to")
                .count(),
            1
        );
        // The next update says what changed since *that* (the date last told).
        record(
            &conn,
            p.id,
            "!house:x",
            "$update",
            Kind::Update,
            &text,
            moved.starts_at,
        )
        .unwrap();
        plenums::reschedule(
            &mut conn,
            p.id,
            at(2026, 10, 20, 19),
            None,
            "@admin:x",
            zone(),
            at(2026, 10, 18, 9),
        )
        .unwrap();
        let (id, what) = take_updates(&conn).unwrap()[0];
        let text = update_text(&conn, id, what, zone(), at(2026, 10, 18, 9)).unwrap();
        assert!(text.contains("~~Monday, 19 October · 19:00~~"), "{text}");
        // Cancelled: said, with where the round goes.
        plenums::cancel(&mut conn, p.id, "@admin:x", at(2026, 10, 18, 10)).unwrap();
        let (id, what) = take_updates(&conn).unwrap()[0];
        assert_eq!(what, Update::Cancelled);
        let text = update_text(&conn, id, what, zone(), at(2026, 10, 18, 10)).unwrap();
        assert!(text.contains("won't take place"));
        assert!(
            text.contains("carries on until the next plenum on **Sunday, 1 November"),
            "{text}"
        );
    }

    #[test]
    fn delivery_records_go_with_the_run_and_only_counts_stay() {
        let (mut conn, _) = october();
        let now = at(2026, 10, 5, 12);
        let plan = checkins::plan(
            &conn,
            "cleaning",
            &StartOptions::default(),
            now,
            Duration::hours(24),
        )
        .unwrap();
        let run = checkins::start(&mut conn, &plan, "@admin:x", now).unwrap();
        checkins::add_message(&conn, "$notice-a", run).unwrap();
        checkins::add_message(&conn, "$reminder-a", run).unwrap();
        checkins::count_batch(&conn, run, checkins::Batch::Notice, 12, 10).unwrap();
        checkins::count_batch(&conn, run, checkins::Batch::Reminder, 0, 4).unwrap();
        // While open: the 📊 works, but no row says whose chat it was.
        assert_eq!(
            checkins::message_run(&conn, "$reminder-a").unwrap(),
            Some(run)
        );
        let rooms: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('checkin_messages') WHERE name = 'room_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rooms, 0);
        checkins::close(&conn, run, now + Duration::hours(1)).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM checkin_messages"), 0);
        let (eligible, notices, reminders): (i64, i64, i64) = conn
            .query_row(
                "SELECT eligible_at_launch, notices_sent, reminders_sent FROM survey_runs WHERE id = ?1",
                [run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((eligible, notices, reminders), (12, 10, 4));
    }
}
