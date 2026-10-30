//! The plenum's dates, from Nextcloud: plenum-bot owns the house plenum's
//! calendar (`plenum-house`) and shares it read-only with this bot's own
//! account, where it appears as `plenum-house_shared_by_plenum-bot`. Only
//! that share is read ([`pick`]): any Nextcloud user can make a calendar of
//! that name and share it with the bot. That calendar is the only place the dates are kept (decided 2026-10-22):
//! people add, move and cancel plenums there, and every few minutes the bot
//! reads it over CalDAV and follows it ([`plenums::sync_calendar`]).
//!
//! A read that fails changes nothing; so does one that finds no plenum
//! ahead (an emptied or re-made calendar must not cancel the plenum the open
//! round is collecting for).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use mxbot_common::{
    calendar::{
        caldav,
        ics::{self, EntryTime, IcsEvent},
    },
    supervise::Alive,
};

use crate::{
    config::CalendarConfig,
    dm::Kk,
    plenums::{self, Occurrence},
    time::Zone,
};

/// What `!kk plenums` says about the calendar.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// The calendar the dates come from; `None`: none is configured (or its
    /// password is missing), so the plenums stay as they are.
    pub calendar: Option<String>,
    /// The last read that worked, and how many plenums it had in view.
    pub read_at: Option<DateTime<Utc>>,
    pub in_view: usize,
    /// Why the reads fail since then.
    pub error: Option<String>,
}

/// The timed occurrences from `from` to `to` (local days), each once, as
/// instants in `zone`. All-day events are no plenums and are left out.
/// Each keeps the ID the calendar gives it: the UID of a single event, the
/// UID and original date of one of a series (`UID#date`).
pub fn occurrences(
    events: &[IcsEvent],
    from: NaiveDate,
    to: NaiveDate,
    zone: Zone,
) -> Vec<Occurrence> {
    let days = (to - from).num_days() + 1;
    let mut found: Vec<Occurrence> = Vec::new();
    for (_, entries) in ics::expand(events, from, days.max(1)) {
        for e in entries {
            if !matches!(e.time, EntryTime::Timed { .. }) || found.iter().any(|o| o.id == e.key) {
                continue;
            }
            found.push(Occurrence {
                id: e.key,
                starts_at: zone.at(e.starts_at.date(), e.starts_at.time()),
            });
        }
    }
    found.sort_by(|a, b| (a.starts_at, &a.id).cmp(&(b.starts_at, &b.id)));
    found
}

/// The collection of `calendar` that `owner` shares with the account:
/// exactly `<calendar>_shared_by_<owner>`. Never a calendar of the account's
/// own (anyone can make one of that name and share it with the bot), never
/// one another account shares under the same name.
pub fn pick(urls: &[String], calendar: &str, owner: &str) -> Option<String> {
    let share = format!("{calendar}_shared_by_{owner}");
    urls.iter()
        .find(|u| u.trim_end_matches('/').rsplit('/').next() == Some(share.as_str()))
        .cloned()
}

/// Reads the calendar over CalDAV with the bot's own account.
pub struct Reader {
    http: reqwest::Client,
    config: CalendarConfig,
    password: String,
    /// The calendar's collection, once found (found again after a failure).
    url: Option<String>,
}

impl Reader {
    /// The password comes from the environment (`password_env`), never the config.
    pub fn from_config(config: &CalendarConfig) -> Result<Self> {
        let password = std::env::var(&config.password_env)
            .ok()
            .filter(|p| !p.trim().is_empty())
            .with_context(|| {
                format!(
                    "[kummerkasten.calendar] needs the app password in ${}",
                    config.password_env
                )
            })?;
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("kummerkasten-bot/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .build()?,
            config: config.clone(),
            password,
            url: None,
        })
    }

    pub fn every(&self) -> Duration {
        Duration::from_secs(self.config.refresh_minutes.max(1) * 60)
    }

    async fn url(&mut self) -> Result<String> {
        if let Some(url) = &self.url {
            return Ok(url.clone());
        }
        let source = caldav::Source {
            caldav_account_url: Some(self.config.account_url.clone()),
            username: Some(self.config.username.clone()),
            password: Some(self.password.clone()),
            ..Default::default()
        };
        let urls = caldav::discover_calendars(&self.http, &source)
            .await
            .context("listing the account's calendars")?;
        let Some(url) = pick(&urls, &self.config.calendar, &self.config.owner) else {
            bail!(
                "{} doesn't share the calendar {:?} with {} (it has {} calendar(s))",
                self.config.owner,
                self.config.calendar,
                self.config.username,
                urls.len()
            );
        };
        tracing::info!(%url, "Plenum calendar found");
        self.url = Some(url.clone());
        Ok(url)
    }

    /// The plenums the calendar has in [`plenums::window`].
    pub async fn read(&mut self, zone: Zone, now: DateTime<Utc>) -> Result<Vec<Occurrence>> {
        let (from, to) = plenums::window(zone, now);
        let url = self.url().await?;
        let events = caldav::fetch_calendar(
            &self.http,
            &url,
            &self.config.username,
            &self.password,
            from,
            to + chrono::Duration::days(1),
            &zone.0,
        )
        .await;
        match events {
            Ok(events) => Ok(occurrences(&events, from, to, zone)),
            Err(error) => {
                // Shared anew under another name, say: look again next time.
                self.url = None;
                Err(error.context(format!("reading the calendar {:?}", self.config.calendar)))
            }
        }
    }
}

/// One read, followed. A failure changes nothing.
pub async fn sync(kk: &Kk, reader: &mut Reader) {
    let zone = kk.engine.zone;
    let now = Utc::now();
    let read = reader.read(zone, now).await;
    let outcome = match read {
        Ok(occurrences) => {
            let in_view = occurrences.len();
            let until = plenums::window_end(zone, now);
            let _op = kk.ops.lock().await;
            kk.engine
                .db
                .run(move |c| plenums::sync_calendar(c, &occurrences, zone, until, now))
                .await
                .map(|synced| (in_view, synced))
        }
        Err(error) => Err(error),
    };
    let mut status = kk.calendar.lock().unwrap_or_else(|e| e.into_inner());
    match outcome {
        Ok((in_view, synced)) => {
            if synced.changed() {
                tracing::info!(in_view, ?synced, "Plenums follow the calendar");
                kk.wake.notify_one();
            }
            status.read_at = Some(now);
            status.in_view = in_view;
            status.error = None;
        }
        Err(error) => {
            tracing::warn!("Calendar not read ({error:#}); the plenums stay as they are");
            status.error = Some(format!("{error:#}"));
        }
    }
}

/// The longest a read normally takes, for the health check of [`run`].
pub const SLOW_READ: Duration = Duration::from_secs(5 * 60);

/// Every `refresh_minutes`, after the first read at startup; `alive` ticks
/// once per read.
pub async fn run(kk: Kk, mut reader: Reader, alive: Alive) {
    loop {
        tokio::time::sleep(reader.every()).await;
        alive.tick();
        sync(&kk, &mut reader).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plenums::tests::{at, day, zone};

    const SERIES: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
BEGIN:VEVENT\r\nUID:house\r\nSUMMARY:Plenum\r\n\
DTSTART;TZID=Europe/Berlin:20261101T190000\r\nDTEND;TZID=Europe/Berlin:20261101T210000\r\n\
RRULE:FREQ=MONTHLY;BYDAY=1SU,3SU,5SU\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:house\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261115T190000\r\nSUMMARY:Plenum\r\n\
DTSTART;TZID=Europe/Berlin:20261114T180000\r\nDTEND;TZID=Europe/Berlin:20261114T200000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:extra\r\nSUMMARY:Extra plenum\r\n\
DTSTART;TZID=Europe/Berlin:20261122T190000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:allday\r\nSUMMARY:Cleaning day\r\nDTSTART;VALUE=DATE:20261110\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn a_series_with_a_moved_occurrence_and_an_extra_one() {
        let events = ics::parse_ics_in(SERIES, &zone().0);
        let occ = occurrences(&events, day(2026, 10, 31), day(2026, 12, 6), zone());
        let got: Vec<(&str, DateTime<Utc>)> =
            occ.iter().map(|o| (o.id.as_str(), o.starts_at)).collect();
        assert_eq!(got.len(), 5, "{got:?}");
        assert_eq!(got[0].1, at(2026, 11, 1, 19));
        // Moved from the 15th to Saturday 18:00: the same occurrence.
        assert_eq!(got[1].1, at(2026, 11, 14, 18));
        assert!(got[1].0.starts_with("house") && got[1].0.contains("2026-11-15"));
        assert_eq!(got[2], ("extra", at(2026, 11, 22, 19)));
        assert_eq!(got[3].1, at(2026, 11, 29, 19));
        assert_eq!(got[4].1, at(2026, 12, 6, 19));
        // Every ID once; the all-day event is no plenum.
        let mut ids: Vec<&str> = got.iter().map(|g| g.0).collect();
        ids.dedup();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn only_the_calendar_its_owner_shares_with_the_bot() {
        let base = "https://cloud.example.org/remote.php/dav/calendars/kummerkasten-bot";
        let pick = |urls: &[String]| pick(urls, "plenum-house", "plenum-bot");
        let share = format!("{base}/plenum-house_shared_by_plenum-bot/");
        let urls = [
            format!("{base}/personal/"),
            // Made by someone else under the same name, shared with the bot
            // (sorted before the real one).
            format!("{base}/plenum-house_shared_by_eve/"),
            format!("{base}/plenum-house_shared_by_plenum-bot_shared_by_eve/"),
            // The bot's own of that name.
            format!("{base}/plenum-house/"),
            share.clone(),
            format!("{base}/plenum-house-old_shared_by_plenum-bot/"),
        ];
        assert_eq!(pick(&urls).as_deref(), Some(share.as_str()));
        // Without the share: none, however alike the others are named.
        let without: Vec<String> = urls.iter().filter(|u| **u != share).cloned().collect();
        assert_eq!(pick(&without), None);
        assert_eq!(
            pick(&[share.trim_end_matches('/').to_owned()]).as_deref(),
            Some(share.trim_end_matches('/'))
        );
        assert_eq!(super::pick(&urls, "plenum-kitchen", "plenum-bot"), None);
        assert_eq!(super::pick(&urls, "plenum-house", "plenum"), None);
    }
}
