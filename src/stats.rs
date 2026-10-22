//! 📈 Results: computed from what is stored *now*, in SQL, through one
//! privacy policy.
//!
//! * **Comparable only by meaning.** A longitudinal series is one canonical
//!   metric (scale questions) or one canonical problem within one metric
//!   (choice questions): whatever the wording of each version. Different
//!   metrics are never merged; each point keeps the exact question version.
//! * **Two signals, never mixed.** Check-in shares ("38% selected this")
//!   and spontaneous feedback counts ("9 submissions mentioned this") are
//!   returned separately and never combined into one number.
//! * **Privacy threshold.** Every value describing a group needs at least
//!   `k` responses behind it. Breakdowns (by floor) hide small groups *and*
//!   then more, until what is hidden adds up to at least `k`: otherwise
//!   the whole minus the visible groups would give the hidden one away.
//! * **No live differencing.** Check-in results appear once a run is
//!   closed; spontaneous counts of the running round leave out today. So
//!   watching numbers change can't tell what one person just answered.
//! * **Deletion means gone.** Nothing here caches: a deleted submission is
//!   in no statistic computed after it.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::NaiveDate;
use rusqlite::{params, Connection};

use crate::time;

/// "Every day so far" for the counts that leave out today: a finished
/// round, a one-off summary. (Not `NaiveDate::MAX`: as text, "+262142-…"
/// sorts before every real date.)
pub fn all_days() -> NaiveDate {
    NaiveDate::from_ymd_opt(9999, 12, 31).expect("a valid date")
}

/// Which groups of a breakdown may be shown, given each group's number of
/// responses (`counts`, all groups: they add up to the whole) and the
/// threshold `k`: none if the whole is under `k`; otherwise every group
/// with at least `k`, minus the smallest visible ones as long as the
/// hidden ones together have fewer than `k` (complementary suppression:
/// whole − visible would reveal them).
pub fn visible(counts: &[i64], k: i64) -> Vec<bool> {
    let total: i64 = counts.iter().sum();
    if total < k {
        return vec![false; counts.len()];
    }
    let mut shown: Vec<bool> = counts.iter().map(|n| *n >= k).collect();
    loop {
        let hidden: i64 = counts
            .iter()
            .zip(&shown)
            .filter(|(_, s)| !**s)
            .map(|(n, _)| n)
            .sum();
        let any_hidden = shown.iter().any(|s| !s);
        if !any_hidden || hidden >= k {
            return shown;
        }
        // Hide the smallest group still shown (of equal ones, the last).
        match counts
            .iter()
            .enumerate()
            .rev()
            .filter(|(i, _)| shown[*i])
            .min_by_key(|(_, n)| **n)
        {
            Some((i, _)) => shown[i] = false,
            None => return shown,
        }
    }
}

// ── Check-in metrics (scale) ────────────────────────────────────────────────

/// One closed run's value of a metric.
#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub run_id: i64,
    pub closed_on: NaiveDate,
    /// The exact question version asked (for drill-down).
    pub question_version_id: i64,
    /// Answers behind it.
    pub n: i64,
    /// The mean: `None` with fewer than `k` answers.
    pub value: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub metric_key: String,
    pub metric_label: String,
    pub points: Vec<Point>,
}

/// The scale metrics of topic `topic_root` (a top-level topic), one point
/// per closed run, oldest first.
pub fn metric_series(conn: &Connection, topic_root: i64, k: i64) -> Result<Vec<Series>> {
    let mut stmt = conn.prepare(
        "SELECT m.key, m.label, r.id, r.closes_at, qv.id,
                count(a.value), avg(a.value)
         FROM survey_runs r
         JOIN survey_run_questions srq ON srq.run_id = r.id
         JOIN question_versions qv ON qv.id = srq.question_version_id
         JOIN metrics m ON m.id = qv.metric_id
         JOIN topics t ON t.id = m.topic_id
         LEFT JOIN responses s ON s.run_id = r.id
         LEFT JOIN answers a ON a.response_id = s.id AND a.question_version_id = qv.id
                            AND a.skipped = 0
         WHERE r.status = 'closed' AND qv.kind = 'scale'
           AND coalesce(t.parent_id, t.id) = ?1
         GROUP BY m.id, r.id, qv.id
         ORDER BY m.key, r.closes_at, r.id",
    )?;
    let rows = stmt.query_map([topic_root], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<f64>>(6)?,
        ))
    })?;
    let mut out: Vec<Series> = Vec::new();
    for row in rows {
        let (key, label, run, closes, qv, n, mean) = row?;
        let point = Point {
            run_id: run,
            closed_on: time::parse_text(&closes)?.date_naive(),
            question_version_id: qv,
            n,
            value: mean.filter(|_| n >= k),
        };
        match out.last_mut() {
            Some(s) if s.metric_key == key => s.points.push(point),
            _ => out.push(Series {
                metric_key: key,
                metric_label: label,
                points: vec![point],
            }),
        }
    }
    Ok(out)
}

// ── Check-in problems (choice questions) ────────────────────────────────────

/// One closed run's share of respondents who picked a canonical problem.
#[derive(Clone, Debug, PartialEq)]
pub struct Share {
    pub run_id: i64,
    pub closed_on: NaiveDate,
    pub question_version_id: i64,
    /// Respondents who answered the question.
    pub n: i64,
    /// Percent of them who picked this problem: `None` with `n < k`.
    pub percent: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProblemSeries {
    pub problem_id: i64,
    pub problem_key: String,
    pub problem_label: String,
    /// The metric it was asked under (series never cross metrics).
    pub metric_key: String,
    pub shares: Vec<Share>,
}

/// The canonical problems asked about in topic `topic_root`, per metric,
/// one share per closed run, oldest first.
pub fn problem_series(conn: &Connection, topic_root: i64, k: i64) -> Result<Vec<ProblemSeries>> {
    let mut stmt = conn.prepare(
        "WITH asked AS (
             SELECT DISTINCT r.id AS run, r.closes_at, qv.id AS qv, m.key AS metric, cv.problem_id
             FROM survey_runs r
             JOIN survey_run_questions srq ON srq.run_id = r.id
             JOIN question_versions qv ON qv.id = srq.question_version_id
             JOIN metrics m ON m.id = qv.metric_id
             JOIN choice_versions cv ON cv.question_version_id = qv.id
             JOIN problems p ON p.id = cv.problem_id
             JOIN topics t ON t.id = p.topic_id
             WHERE r.status = 'closed' AND qv.kind IN ('single', 'multiple')
               AND coalesce(t.parent_id, t.id) = ?1)
         SELECT asked.problem_id, p.key, p.label, asked.metric, asked.run, asked.closes_at, asked.qv,
                (SELECT count(*) FROM answers a JOIN responses s ON s.id = a.response_id
                 WHERE s.run_id = asked.run AND a.question_version_id = asked.qv
                   AND a.skipped = 0),
                (SELECT count(DISTINCT ac.response_id)
                 FROM answer_choices ac
                 JOIN responses s ON s.id = ac.response_id
                 JOIN choice_versions cv ON cv.id = ac.choice_version_id
                 WHERE s.run_id = asked.run AND ac.question_version_id = asked.qv
                   AND cv.problem_id = asked.problem_id)
         FROM asked JOIN problems p ON p.id = asked.problem_id
         ORDER BY asked.metric, p.key, asked.closes_at, asked.run",
    )?;
    let rows = stmt.query_map([topic_root], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;
    let mut out: Vec<ProblemSeries> = Vec::new();
    for row in rows {
        let (pid, pkey, plabel, metric, run, closes, qv, n, chose) = row?;
        let share = Share {
            run_id: run,
            closed_on: time::parse_text(&closes)?.date_naive(),
            question_version_id: qv,
            n,
            percent: (n >= k && n > 0).then(|| (chose * 100 + n / 2) / n),
        };
        match out.last_mut() {
            Some(s) if s.problem_id == pid && s.metric_key == metric => s.shares.push(share),
            _ => out.push(ProblemSeries {
                problem_id: pid,
                problem_key: pkey,
                problem_label: plabel,
                metric_key: metric,
                shares: vec![share],
            }),
        }
    }
    Ok(out)
}

// ── Spontaneous feedback ────────────────────────────────────────────────────

/// Submissions of round `round_id` (as stored now) before `today`, by
/// canonical problem, in topic `topic_root` (or all topics): counts of
/// submissions, not of people.
pub fn mentions(
    conn: &Connection,
    round_id: i64,
    topic_root: Option<i64>,
    today: NaiveDate,
) -> Result<BTreeMap<i64, i64>> {
    let mut stmt = conn.prepare(
        "SELECT s.problem_id, count(*) FROM submissions s JOIN topics t ON t.id = s.topic_id
         WHERE s.round_id = ?1 AND s.problem_id IS NOT NULL AND s.submitted_on < ?2
           AND (?3 IS NULL OR coalesce(t.parent_id, t.id) = ?3)
         GROUP BY s.problem_id",
    )?;
    let rows = stmt.query_map(params![round_id, time::date_text(today), topic_root], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Submissions of round `round_id` before `today`, by top-level topic.
pub fn by_topic(conn: &Connection, round_id: i64, today: NaiveDate) -> Result<BTreeMap<i64, i64>> {
    let mut stmt = conn.prepare(
        "SELECT coalesce(t.parent_id, t.id), count(*) FROM submissions s
         JOIN topics t ON t.id = s.topic_id
         WHERE s.round_id = ?1 AND s.submitted_on < ?2
         GROUP BY 1",
    )?;
    let rows = stmt.query_map(params![round_id, time::date_text(today)], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ── Rounds and runs ─────────────────────────────────────────────────────────

/// A round in numbers (what is stored now).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoundNumbers {
    /// Feedback submissions (before `today`, for a running round).
    pub feedback: i64,
    /// Responses to its *closed* check-ins.
    pub responses: i64,
    /// Its check-ins still open (no numbers until they close).
    pub open_checkins: i64,
    /// Ideas shared during it.
    pub ideas: i64,
}

pub fn round_numbers(conn: &Connection, round_id: i64, today: NaiveDate) -> Result<RoundNumbers> {
    Ok(conn.query_row(
        "SELECT
            (SELECT count(*) FROM submissions WHERE round_id = ?1 AND submitted_on < ?2),
            (SELECT count(*) FROM responses s JOIN survey_runs r ON r.id = s.run_id
             WHERE r.round_id = ?1 AND r.status = 'closed'),
            (SELECT count(*) FROM survey_runs WHERE round_id = ?1 AND status = 'open'),
            (SELECT count(*) FROM ideas WHERE round_id = ?1)",
        params![round_id, time::date_text(today)],
        |r| {
            Ok(RoundNumbers {
                feedback: r.get(0)?,
                responses: r.get(1)?,
                open_checkins: r.get(2)?,
                ideas: r.get(3)?,
            })
        },
    )?)
}

/// A closed check-in's participation: responses, and the denominator:
/// residents eligible when it started (counted then, as a number only).
#[derive(Clone, Debug, PartialEq)]
pub struct Participation {
    pub run_id: i64,
    pub emoji: String,
    pub title: String,
    pub closed_on: NaiveDate,
    pub responses: i64,
    pub eligible_at_launch: Option<i64>,
}

/// The closed runs of topic `topic_root` (by their template), newest first.
pub fn participation(conn: &Connection, topic_root: Option<i64>) -> Result<Vec<Participation>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.emoji, r.title, r.closes_at,
                (SELECT count(*) FROM responses s WHERE s.run_id = r.id), r.eligible_at_launch
         FROM survey_runs r JOIN survey_templates st ON st.id = r.template_id
         LEFT JOIN topics t ON t.id = st.topic_id
         WHERE r.status = 'closed' AND (?1 IS NULL OR coalesce(t.parent_id, t.id) = ?1)
         ORDER BY r.closes_at DESC, r.id DESC",
    )?;
    let rows = stmt.query_map([topic_root], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<i64>>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (run_id, emoji, title, closes, responses, eligible) = row?;
        out.push(Participation {
            run_id,
            emoji,
            title,
            closed_on: time::parse_text(&closes)?.date_naive(),
            responses,
            eligible_at_launch: eligible,
        });
    }
    Ok(out)
}

// ── Breakdown by floor ──────────────────────────────────────────────────────

/// A scale question's mean by floor: only floors stated as the identity
/// (🏠); everyone else is "not stated". Hidden groups are `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorRow {
    pub label: String,
    /// `None`: hidden (too few, or would give a hidden group away).
    pub mean: Option<f64>,
}

pub fn by_floor(
    conn: &Connection,
    run_id: i64,
    question_version_id: i64,
    k: i64,
) -> Result<Vec<FloorRow>> {
    let mut stmt = conn.prepare(
        "SELECT l.label, count(a.value), avg(a.value)
         FROM locations l
         LEFT JOIN responses s ON s.respondent_floor_id = l.id AND s.run_id = ?1
         LEFT JOIN answers a ON a.response_id = s.id AND a.question_version_id = ?2
                            AND a.skipped = 0
         WHERE l.is_floor = 1 AND l.active = 1
         GROUP BY l.id ORDER BY l.sort, l.id",
    )?;
    let mut groups: Vec<(String, i64, Option<f64>)> = stmt
        .query_map(params![run_id, question_version_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (n, mean): (i64, Option<f64>) = conn.query_row(
        "SELECT count(a.value), avg(a.value) FROM responses s
         JOIN answers a ON a.response_id = s.id AND a.question_version_id = ?2 AND a.skipped = 0
         WHERE s.run_id = ?1 AND s.respondent_floor_id IS NULL",
        params![run_id, question_version_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    groups.push(("Not stated".to_owned(), n, mean));
    let counts: Vec<i64> = groups.iter().map(|g| g.1).collect();
    let shown = visible(&counts, k);
    Ok(groups
        .into_iter()
        .zip(shown)
        .map(|((label, _, mean), show)| FloorRow {
            label,
            mean: mean.filter(|_| show),
        })
        .collect())
}

/// Completed plenums with what is still stored of their round, newest
/// first: `(plenum_id, starts_at text, round_id)`.
pub fn previous_plenums(conn: &Connection, limit: usize) -> Result<Vec<(i64, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.starts_at, r.id FROM plenums p JOIN rounds r ON r.plenum_id = p.id
         WHERE p.status = 'completed' ORDER BY p.starts_at DESC, p.id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        checkins::{self, tests::started, Answer, StartOptions},
        lifecycle,
        plenums::{
            self,
            tests::{at, with_house_calendar, zone},
        },
        secrets::Secrets,
        submissions::{self, Identity, NewSubmission, Retention},
        surveys::{self, Edit},
    };
    use chrono::{DateTime, Duration, Utc};
    use std::collections::BTreeMap as Map;

    pub fn cleaning(conn: &Connection) -> i64 {
        conn.query_row("SELECT id FROM topics WHERE key = 'cleaning'", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// Start a cleaning check-in at `now`, have `people` answer `value` on
    /// the scale and pick the first problem (or not), then close it.
    #[allow(clippy::type_complexity)]
    pub fn closed_run(
        conn: &mut Connection,
        secrets: &Secrets,
        now: DateTime<Utc>,
        people: &[(&str, i64, bool, Option<i64>)],
    ) -> i64 {
        let run = started(
            conn,
            now,
            StartOptions {
                deadline: checkins::Deadline::Fixed(now + Duration::days(7)),
                ..Default::default()
            },
        );
        for (who, value, picks, floor) in people {
            let mut answers = Map::new();
            answers.insert(
                0,
                Answer {
                    value: Some(*value),
                    ..Default::default()
                },
            );
            answers.insert(
                1,
                if *picks {
                    Answer {
                        choices: vec![run.questions[1].choices[0].version_id],
                        ..Default::default()
                    }
                } else {
                    Answer {
                        choices: vec![run.questions[1].choices[1].version_id],
                        ..Default::default()
                    }
                },
            );
            let identity = if floor.is_some() {
                Identity::Floor
            } else {
                Identity::Anonymous
            };
            let s = checkins::Submission {
                mxid: who,
                rooms: &[checkins::tests::HOUSE.to_owned()],
                floor: *floor,
                identity,
                answers: &answers,
            };
            checkins::submit(conn, secrets, &run, &s, now)
                .unwrap()
                .unwrap();
        }
        checkins::close(conn, run.id, now + Duration::minutes(5)).unwrap();
        run.id
    }

    fn floor(conn: &Connection, key: &str) -> i64 {
        conn.query_row("SELECT id FROM locations WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn suppression_hides_small_groups_and_whatever_would_give_them_away() {
        // Fewer than k in all: nothing.
        assert_eq!(visible(&[1, 1], 3), [false, false]);
        // All big enough: all.
        assert_eq!(visible(&[4, 3, 5], 3), [true, true, true]);
        // House = 10; floors 4 + 4 + 0 visible would leave "2" = 10 − 8:
        // the next smallest goes too, so what's hidden is ≥ k together.
        let shown = visible(&[4, 4, 2, 0], 3);
        assert_eq!(shown, [true, false, false, false]);
        let hidden: i64 = [4, 4, 2, 0]
            .iter()
            .zip(&shown)
            .filter(|(_, s)| !**s)
            .map(|(n, _)| n)
            .sum();
        assert!(hidden >= 3);
        // One small group next to one big: both hidden (whole − big = small).
        assert_eq!(visible(&[9, 1], 3), [false, false]);
        // Two small ones that together reach k may stay hidden alone.
        assert_eq!(visible(&[5, 2, 1], 3), [true, false, false]);
        // For every split, no hidden total under k can be derived.
        for a in 0..6 {
            for b in 0..6 {
                for c in 0..6 {
                    let counts = [a, b, c];
                    let shown = visible(&counts, 3);
                    let hidden: i64 = counts
                        .iter()
                        .zip(&shown)
                        .filter(|(_, s)| !**s)
                        .map(|(n, _)| n)
                        .sum();
                    assert!(
                        shown.iter().all(|s| *s) || hidden >= 3 || shown.iter().all(|s| !*s),
                        "{counts:?} → {shown:?}"
                    );
                    for (n, s) in counts.iter().zip(&shown) {
                        assert!(!*s || *n >= 3, "{counts:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn metrics_compare_across_question_versions_but_never_across_metrics() {
        let now = at(2026, 3, 2, 12);
        let mut conn = with_house_calendar(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let people = |v| {
            vec![
                ("@a:x", v, true, None),
                ("@b:x", v, false, None),
                ("@c:x", v, true, None),
            ]
        };
        let first = closed_run(&mut conn, &secrets, now, &people(2));
        // The question is reworded: a new version, the same metric.
        surveys::edit(
            &mut conn,
            "cleaning",
            1,
            Edit::Text("How well does cleaning work for you?".into()),
        )
        .unwrap();
        let second = closed_run(&mut conn, &secrets, now + Duration::days(30), &people(4));
        let series = metric_series(&conn, cleaning(&conn), 3).unwrap();
        assert_eq!(series.len(), 1);
        let s = &series[0];
        assert_eq!(s.metric_key, "cleaning.overall_satisfaction");
        assert_eq!(
            s.points
                .iter()
                .map(|p| (p.run_id, p.value))
                .collect::<Vec<_>>(),
            [(first, Some(2.0)), (second, Some(4.0))]
        );
        // The exact versions are kept.
        assert_ne!(
            s.points[0].question_version_id,
            s.points[1].question_version_id
        );
        // Pointed at another metric, the next run starts a series of its own.
        conn.execute(
            "INSERT INTO metrics (key, topic_id, label, created_at)
             VALUES ('cleaning.fairness', ?1, 'How fair cleaning feels', 'now')",
            [cleaning(&conn)],
        )
        .unwrap();
        surveys::edit(
            &mut conn,
            "cleaning",
            1,
            Edit::Metric(Some("cleaning.fairness".into())),
        )
        .unwrap();
        closed_run(&mut conn, &secrets, now + Duration::days(60), &people(5));
        let series = metric_series(&conn, cleaning(&conn), 3).unwrap();
        let keys: Vec<(&str, usize)> = series
            .iter()
            .map(|s| (s.metric_key.as_str(), s.points.len()))
            .collect();
        assert_eq!(
            keys,
            [
                ("cleaning.fairness", 1),
                ("cleaning.overall_satisfaction", 2)
            ]
        );
    }

    #[test]
    fn problems_aggregate_canonically_and_small_runs_show_no_value() {
        let now = at(2026, 3, 2, 12);
        let mut conn = with_house_calendar(now);
        let secrets = Secrets::from_master(&[1; 32]);
        closed_run(
            &mut conn,
            &secrets,
            now,
            &[
                ("@a:x", 3, true, None),
                ("@b:x", 3, true, None),
                ("@c:x", 3, false, None),
                ("@d:x", 3, true, None),
            ],
        );
        // The choice is reworded (new question version), same problem.
        surveys::edit(
            &mut conn,
            "cleaning",
            2,
            Edit::RenameChoice {
                n: 1,
                label: "Cleaning turns are missed".into(),
            },
        )
        .unwrap();
        // Only two answers: under the threshold.
        closed_run(
            &mut conn,
            &secrets,
            now + Duration::days(30),
            &[("@a:x", 3, true, None), ("@b:x", 3, false, None)],
        );
        let series = problem_series(&conn, cleaning(&conn), 3).unwrap();
        let skipped = series
            .iter()
            .find(|s| s.problem_key == "cleaning.skipped_turns")
            .unwrap();
        assert_eq!(skipped.metric_key, "cleaning.problems_experienced");
        assert_eq!(
            skipped
                .shares
                .iter()
                .map(|s| (s.n, s.percent))
                .collect::<Vec<_>>(),
            [(4, Some(75)), (2, None)]
        );
        // Scale values follow the same threshold.
        let m = &metric_series(&conn, cleaning(&conn), 3).unwrap()[0];
        assert_eq!(m.points[1].value, None);
    }

    #[test]
    fn floor_breakdowns_suppress_small_and_derivable_groups() {
        let now = at(2026, 3, 2, 12);
        let mut conn = with_house_calendar(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let (f1, f2, f3) = (
            floor(&conn, "floor.1"),
            floor(&conn, "floor.2"),
            floor(&conn, "floor.3"),
        );
        // 4 on the 1st floor, 4 on the 2nd, 2 on the 3rd, nobody else.
        let mut people = Vec::new();
        let names: Vec<String> = (0..10).map(|i| format!("@p{i}:x")).collect();
        for (i, name) in names.iter().enumerate() {
            let f = if i < 4 {
                f1
            } else if i < 8 {
                f2
            } else {
                f3
            };
            people.push((name.as_str(), 2 + (i % 3) as i64, true, Some(f)));
        }
        let run = closed_run(&mut conn, &secrets, now, &people);
        let qv = checkins::run(&conn, run).unwrap().questions[0].id;
        let rows = by_floor(&conn, run, qv, 3).unwrap();
        let shown: Vec<(&str, bool)> = rows
            .iter()
            .map(|r| (r.label.as_str(), r.mean.is_some()))
            .collect();
        // The 3rd floor (2) is too small; showing both big floors would
        // reveal it (house mean × 10 − the two floors), so one of them goes
        // too. The empty 4th floor and "not stated" are hidden as well.
        assert_eq!(shown.iter().filter(|(_, s)| *s).count(), 1, "{shown:?}");
        assert!(!rows
            .iter()
            .find(|r| r.label == "3rd floor")
            .unwrap()
            .mean
            .is_some());
        assert!(!rows
            .iter()
            .find(|r| r.label == "Not stated")
            .unwrap()
            .mean
            .is_some());
    }

    #[test]
    fn spontaneous_feedback_counts_what_is_stored_and_leaves_out_today() {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_house_calendar(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let round = plenums::open_round(&conn).unwrap().unwrap();
        let skipped: (i64, i64) = conn
            .query_row(
                "SELECT t.id, t.problem_id FROM topics t WHERE t.key = 'cleaning.skipped_turns'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let submit = |conn: &Connection, retention, day: NaiveDate| {
            submissions::submit(
                conn,
                &secrets,
                &NewSubmission {
                    topic_id: skipped.0,
                    problem_id: Some(skipped.1),
                    location_id: None,
                    body: "Turns get skipped".into(),
                    suggestion: None,
                    identity: Identity::Anonymous,
                    retention,
                    for_plenum: true,
                },
                "@a:x",
                None,
                day,
            )
            .unwrap()
        };
        let today = zone().date(now);
        let yesterday = today - Duration::days(1);
        let keep = submit(&conn, Retention::Keep, yesterday);
        let gone = submit(&conn, Retention::DeleteAfterPlenum, yesterday);
        submit(&conn, Retention::Keep, today);
        // Today's isn't counted yet (no live differencing).
        assert_eq!(
            mentions(&conn, round.id, Some(cleaning(&conn)), today).unwrap()[&skipped.1],
            2
        );
        // Deleted by its author: gone from the count at once.
        submissions::delete(&conn, gone.id).unwrap();
        assert_eq!(
            mentions(&conn, round.id, None, today).unwrap()[&skipped.1],
            1
        );
        assert_eq!(round_numbers(&conn, round.id, today).unwrap().feedback, 1);
        let _ = keep;
        // After the plenum: what was to be deleted is gone; kept stays.
        submit(&conn, Retention::DeleteAfterPlenum, yesterday);
        lifecycle::complete(&mut conn, round.plenum_id, "@admin:x", now).unwrap();
        let later = today + Duration::days(1);
        assert_eq!(
            mentions(&conn, round.id, None, later).unwrap()[&skipped.1],
            2,
            "the two kept ones (yesterday's and today's)"
        );
        assert_eq!(
            previous_plenums(&conn, 5).unwrap()[0].2,
            round.id,
            "the round is browsable under its plenum"
        );
    }

    #[test]
    fn participation_counts_responses_against_eligible_at_launch() {
        let now = at(2026, 3, 2, 12);
        let mut conn = with_house_calendar(now);
        let secrets = Secrets::from_master(&[1; 32]);
        let run = closed_run(
            &mut conn,
            &secrets,
            now,
            &[
                ("@a:x", 3, true, None),
                ("@b:x", 3, true, None),
                ("@c:x", 3, true, None),
            ],
        );
        checkins::count_batch(&conn, run, checkins::Batch::Notice, 27, 20).unwrap();
        // A later joiner's notice adds to "sent", not to the denominator.
        checkins::count_batch(&conn, run, checkins::Batch::Notice, 0, 1).unwrap();
        let p = &participation(&conn, Some(cleaning(&conn))).unwrap()[0];
        assert_eq!((p.responses, p.eligible_at_launch), (3, Some(27)));
        let sent: i64 = conn
            .query_row(
                "SELECT notices_sent FROM survey_runs WHERE id = ?1",
                [run],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sent, 21);
        // Open check-ins have no numbers yet.
        let open = started(&mut conn, now, StartOptions::default());
        assert!(participation(&conn, None)
            .unwrap()
            .iter()
            .all(|p| p.run_id != open.id));
        let _ = params![1];
    }
}
