//! When plenums happen: recurrence rules, as data.
//!
//! A rule only *proposes* dates. `plenums` turns them into concrete rows
//! (with stable IDs) once they come into view, and from then on a row is
//! what counts: moving or cancelling one occurrence never touches the
//! rule, and changing the rule never rewrites a plenum something already
//! points at. New kinds of rules are new variants here; stored rules keep
//! deserializing.

use anyhow::{bail, Context, Result};
use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Weekday};
use serde::{Deserialize, Serialize};

pub const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const WEEKDAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recurrence {
    /// The `nths` occurrences of `weekday` (0 = Monday) in every month:
    /// `[1, 3, 5]` is the 1st, 3rd and (where a month has one) 5th.
    MonthlyNth {
        weekday: u8,
        nths: Vec<u8>,
        time: NaiveTime,
    },
    /// On `weekdays` of every `every`-th week, counted from the week of
    /// `anchor` (every = 1: every week).
    Weekly {
        weekdays: Vec<u8>,
        every: u32,
        anchor: NaiveDate,
        time: NaiveTime,
    },
}

impl Recurrence {
    /// The house's current rhythm: 1st, 3rd and 5th Sunday, at `time`.
    pub fn default_rule(time: NaiveTime) -> Self {
        Recurrence::MonthlyNth {
            weekday: 6,
            nths: vec![1, 3, 5],
            time,
        }
    }

    pub fn time(&self) -> NaiveTime {
        match self {
            Recurrence::MonthlyNth { time, .. } | Recurrence::Weekly { time, .. } => *time,
        }
    }

    /// Every date the rule proposes in `from..=to`, in order.
    pub fn dates(&self, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
        let mut out = Vec::new();
        let mut day = from;
        while day <= to {
            if self.matches(day) {
                out.push(day);
            }
            day += Duration::days(1);
        }
        out
    }

    pub fn matches(&self, day: NaiveDate) -> bool {
        let weekday = day.weekday().num_days_from_monday() as u8;
        match self {
            Recurrence::MonthlyNth {
                weekday: w, nths, ..
            } => {
                // The n-th of its weekday this month: days 1–7 are the 1st, …
                *w == weekday && nths.contains(&((day.day0() / 7) as u8 + 1))
            }
            Recurrence::Weekly {
                weekdays,
                every,
                anchor,
                ..
            } => {
                let monday =
                    |d: NaiveDate| d - Duration::days(d.weekday().num_days_from_monday() as i64);
                let weeks = (monday(day) - monday(*anchor)).num_days() / 7;
                weekdays.contains(&weekday) && weeks.rem_euclid((*every).max(1) as i64) == 0
            }
        }
    }

    /// "1st, 3rd and 5th Sunday of each month · 19:00".
    pub fn describe(&self) -> String {
        let time = self.time().format("%H:%M");
        match self {
            Recurrence::MonthlyNth { weekday, nths, .. } => {
                let mut nths = nths.clone();
                nths.sort_unstable();
                let list: Vec<String> = nths.iter().map(|n| ordinal(*n)).collect();
                format!(
                    "{} {} of each month · {time}",
                    join_and(&list),
                    WEEKDAY_NAMES[*weekday as usize % 7]
                )
            }
            Recurrence::Weekly {
                weekdays,
                every,
                anchor,
                ..
            } => {
                let days: Vec<String> = weekdays
                    .iter()
                    .map(|d| WEEKDAY_NAMES[*d as usize % 7].to_owned())
                    .collect();
                match every {
                    1 => format!("every {} · {time}", join_and(&days)),
                    n => format!(
                        "every {} week on {} (counting from {}) · {time}",
                        ordinal(*n as u8),
                        join_and(&days),
                        anchor.format("%-d %b %Y")
                    ),
                }
            }
        }
    }

    /// Admin syntax, after `!kk rule set`:
    ///
    /// * `sun 1,3,5 19:00`, those Sundays of each month
    /// * `sun 19:00`, every Sunday; `sat,sun 19:00`, both days
    /// * `every 2 sun 19:00 from 2026-11-01`, every second week
    pub fn parse(args: &[&str], today: NaiveDate) -> Result<Self> {
        let mut args: Vec<&str> = args.to_vec();
        let mut every = 1;
        let mut anchor = today;
        if args.first() == Some(&"every") {
            every = args
                .get(1)
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| (1..=52).contains(n))
                .context("`every N` needs a number of weeks (1–52)")?;
            args.drain(..2);
        }
        if let Some(pos) = args.iter().position(|a| *a == "from") {
            anchor = crate::time::parse_date(args.get(pos + 1).context("`from` needs a date")?)?;
            args.drain(pos..pos + 2);
        }
        let time = crate::time::parse_hm(args.last().context("Which time? e.g. 19:00")?)?;
        args.pop();
        let weekdays = args
            .first()
            .context("Which weekday? e.g. sun")?
            .split(',')
            .map(parse_weekday)
            .collect::<Result<Vec<u8>>>()?;
        match args.len() {
            1 => Ok(Recurrence::Weekly {
                weekdays,
                every,
                anchor,
                time,
            }),
            2 if every == 1 && weekdays.len() == 1 => {
                let nths = args[1]
                    .split(',')
                    .map(|n| {
                        n.parse::<u8>()
                            .ok()
                            .filter(|n| (1..=5).contains(n))
                            .with_context(|| format!("{n:?} isn't 1–5"))
                    })
                    .collect::<Result<Vec<u8>>>()?;
                Ok(Recurrence::MonthlyNth {
                    weekday: weekdays[0],
                    nths,
                    time,
                })
            }
            _ => bail!("Try `sun 1,3,5 19:00`, `sun 19:00` or `every 2 sun 19:00 from 2026-11-01`"),
        }
    }
}

pub fn parse_weekday(s: &str) -> Result<u8> {
    let s = s.to_ascii_lowercase();
    WEEKDAYS
        .iter()
        .position(|d| s.starts_with(d))
        .map(|i| i as u8)
        .with_context(|| format!("{s:?} isn't a weekday (mon … sun)"))
}

#[allow(dead_code)]
pub fn weekday(n: u8) -> Weekday {
    Weekday::try_from(n % 7).unwrap_or(Weekday::Sun)
}

fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn seven() -> NaiveTime {
        NaiveTime::from_hms_opt(19, 0, 0).unwrap()
    }

    #[test]
    fn default_rule_is_the_first_third_and_fifth_sunday() {
        let rule = Recurrence::default_rule(seven());
        // March 2026 has five Sundays: 1, 8, 15, 22, 29.
        assert_eq!(
            rule.dates(d(2026, 3, 1), d(2026, 3, 31)),
            [d(2026, 3, 1), d(2026, 3, 15), d(2026, 3, 29)]
        );
        // April 2026 has only four (5, 12, 19, 26): no 5th, nothing extra.
        assert_eq!(
            rule.dates(d(2026, 4, 1), d(2026, 4, 30)),
            [d(2026, 4, 5), d(2026, 4, 19)]
        );
        assert_eq!(
            rule.describe(),
            "1st, 3rd and 5th Sunday of each month · 19:00"
        );
    }

    #[test]
    fn other_rhythms_can_be_configured() {
        let today = d(2026, 10, 5);
        let parse =
            |s: &str| Recurrence::parse(&s.split_whitespace().collect::<Vec<_>>(), today).unwrap();
        // Every Sunday.
        let rule = parse("sun 19:00");
        assert_eq!(rule.dates(d(2026, 10, 1), d(2026, 10, 31)).len(), 4);
        // Every second Sunday from 4 Oct.
        let rule = parse("every 2 sun 18:30 from 2026-10-04");
        assert_eq!(
            rule.dates(d(2026, 10, 1), d(2026, 11, 1)),
            [d(2026, 10, 4), d(2026, 10, 18), d(2026, 11, 1)]
        );
        assert_eq!(rule.time(), NaiveTime::from_hms_opt(18, 30, 0).unwrap());
        // 1st and 3rd Sunday only.
        let rule = parse("sun 1,3 19:00");
        assert_eq!(
            rule.dates(d(2026, 3, 1), d(2026, 3, 31)),
            [d(2026, 3, 1), d(2026, 3, 15)]
        );
        // Several weekdays.
        let rule = parse("wed,sun 20:00");
        assert_eq!(rule.dates(d(2026, 10, 5), d(2026, 10, 11)).len(), 2);
        // Nonsense is refused with a hint.
        for bad in ["sun", "sun 6 19:00", "funday 19:00", "every 0 sun 19:00"] {
            assert!(
                Recurrence::parse(&bad.split_whitespace().collect::<Vec<_>>(), today).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn stored_rules_keep_reading() {
        let rule = Recurrence::default_rule(seven());
        let json = serde_json::to_string(&rule).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"monthly_nth","weekday":6,"nths":[1,3,5],"time":"19:00:00"}"#
        );
        assert_eq!(serde_json::from_str::<Recurrence>(&json).unwrap(), rule);
    }
}
