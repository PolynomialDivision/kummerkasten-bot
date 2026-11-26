//! Time: UTC everywhere in storage, the configured timezone for everything
//! people see and every "which day is it". The zone is passed around as a
//! [`Zone`] instead of being looked up in many places.

use anyhow::{Context, Result};
use chrono::{
    DateTime, Datelike, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat, TimeZone,
    Utc,
};
use chrono_tz::Tz;

/// Stored timestamp text: RFC 3339, UTC, whole seconds (`…Z`), so text
/// order is time order in SQL.
pub fn to_text(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn now_text() -> String {
    to_text(Utc::now())
}

pub fn parse_text(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("Bad timestamp {s:?}"))?
        .with_timezone(&Utc))
}

pub fn date_text(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("Bad date {s:?}"))
}

pub fn parse_hm(s: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").with_context(|| format!("Bad time {s:?} (HH:MM)"))
}

/// The configured timezone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Zone(pub Tz);

impl Zone {
    pub fn parse(name: &str) -> Result<Self> {
        name.parse::<Tz>()
            .map(Zone)
            .map_err(|e| anyhow::anyhow!("Unknown timezone {name:?}: {e}"))
    }

    pub fn local(&self, t: DateTime<Utc>) -> NaiveDateTime {
        t.with_timezone(&self.0).naive_local()
    }

    pub fn date(&self, t: DateTime<Utc>) -> NaiveDate {
        self.local(t).date()
    }

    /// The instant of local `date` `time`. In a DST gap the first valid
    /// minute after it; when ambiguous, the earlier one.
    pub fn at(&self, date: NaiveDate, time: NaiveTime) -> DateTime<Utc> {
        let naive = date.and_time(time);
        match self.0.from_local_datetime(&naive) {
            LocalResult::Single(t) => t.with_timezone(&Utc),
            LocalResult::Ambiguous(early, _) => early.with_timezone(&Utc),
            LocalResult::None => (1..=180)
                .find_map(|m| {
                    self.0
                        .from_local_datetime(&(naive + chrono::Duration::minutes(m)))
                        .earliest()
                })
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or_else(|| Utc.from_utc_datetime(&naive)),
        }
    }

    /// "Sunday, 18 October" (with the year when it isn't `today`'s).
    pub fn day_label(&self, t: DateTime<Utc>, today: NaiveDate) -> String {
        let d = self.date(t);
        if d.year() == today.year() {
            d.format("%A, %-d %B").to_string()
        } else {
            d.format("%A, %-d %B %Y").to_string()
        }
    }

    /// "Sunday, 18 October · 19:00".
    pub fn day_time_label(&self, t: DateTime<Utc>, today: NaiveDate) -> String {
        format!(
            "{} · {}",
            self.day_label(t, today),
            self.local(t).format("%H:%M")
        )
    }
}

/// "today", "tomorrow", "in 5 days", "in 3 weeks".
pub fn in_days(today: NaiveDate, day: NaiveDate) -> String {
    match (day - today).num_days() {
        i64::MIN..=-1 => "already past".to_owned(),
        0 => "today".to_owned(),
        1 => "tomorrow".to_owned(),
        n @ 2..=13 => format!("in {n} days"),
        n => format!("in {} weeks", n / 7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_times_survive_dst_changes() {
        let zone = Zone::parse("Europe/Berlin").unwrap();
        // 29 March 2026: 02:00–03:00 doesn't exist in Berlin.
        let d = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        let t = zone.at(d, parse_hm("02:30").unwrap());
        assert_eq!(zone.local(t).format("%H:%M").to_string(), "03:00");
        // Summer: 19:00 local is 17:00 UTC.
        let t = zone.at(
            NaiveDate::from_ymd_opt(2026, 7, 5).unwrap(),
            parse_hm("19:00").unwrap(),
        );
        assert_eq!(to_text(t), "2026-07-05T17:00:00Z");
        assert_eq!(parse_text(&to_text(t)).unwrap(), t);
    }

    #[test]
    fn relative_days_read_naturally() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let later = |n| today + chrono::Duration::days(n);
        assert_eq!(in_days(today, today), "today");
        assert_eq!(in_days(today, later(1)), "tomorrow");
        assert_eq!(in_days(today, later(13)), "in 13 days");
        assert_eq!(in_days(today, later(21)), "in 3 weeks");
    }
}
