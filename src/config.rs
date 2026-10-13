use anyhow::{bail, Result};
use mxbot_common::config::{MatrixConfig, SecurityConfig};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Config {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub kummerkasten: KkConfig,
}

#[derive(Deserialize)]
pub struct KkConfig {
    /// IANA timezone for every date residents see and every "which day".
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Time of the plenums of the starting rule (1st, 3rd and 5th Sunday).
    /// Only used on first start; afterwards the rule lives in the database.
    #[serde(default = "default_plenum_time")]
    pub plenum_time: String,
    /// Starting values of the settings admins can change at runtime.
    #[serde(default)]
    pub settings: Tunables,
}

impl Default for KkConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            plenum_time: default_plenum_time(),
            settings: Tunables::default(),
        }
    }
}

fn default_timezone() -> String {
    "Europe/Berlin".to_owned()
}

fn default_plenum_time() -> String {
    "19:00".to_owned()
}

/// Knobs admins can change with `!admin set <key> <value>`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tunables {
    /// Smallest group a result may describe (results, M3).
    pub privacy_threshold: u32,
    /// An unfinished flow is tidied away after this many minutes.
    pub session_minutes: u32,
    /// Only with `auto_complete`: local time on a plenum's day from which it
    /// is completed automatically.
    pub complete_at: String,
    /// Complete a plenum automatically at `complete_at` on its day. Off by
    /// default: a date passing doesn't mean the plenum happened (it may be
    /// delayed, moved or not take place), and completing deletes data:
    /// admins complete it (`!kk plenum complete`).
    #[serde(default)]
    pub auto_complete: bool,
    /// The day-before summary goes this many hours before a plenum starts.
    #[serde(default = "default_summary_hours")]
    pub summary_hours_before: u32,
    /// The one check-in reminder goes this many hours before it closes.
    #[serde(default = "default_reminder_hours")]
    pub reminder_hours: u32,
}

fn default_reminder_hours() -> u32 {
    24
}

fn default_summary_hours() -> u32 {
    24
}

impl Default for Tunables {
    fn default() -> Self {
        Self {
            privacy_threshold: 3,
            session_minutes: 60,
            complete_at: "23:59".to_owned(),
            auto_complete: false,
            summary_hours_before: default_summary_hours(),
            reminder_hours: default_reminder_hours(),
        }
    }
}

impl Tunables {
    pub fn session_ttl(&self) -> chrono::Duration {
        chrono::Duration::minutes(self.session_minutes.clamp(5, 24 * 60) as i64)
    }

    pub fn complete_at(&self) -> chrono::NaiveTime {
        crate::time::parse_hm(&self.complete_at)
            .unwrap_or_else(|_| chrono::NaiveTime::from_hms_opt(23, 59, 0).unwrap())
    }

    pub fn summary_before(&self) -> chrono::Duration {
        chrono::Duration::hours(self.summary_hours_before.clamp(1, 24 * 7) as i64)
    }

    pub fn remind_before(&self) -> chrono::Duration {
        chrono::Duration::hours(self.reminder_hours.clamp(1, 24 * 14) as i64)
    }

    /// Never below 2: a "group" of one is a person.
    pub fn privacy_threshold(&self) -> u32 {
        self.privacy_threshold.max(2)
    }
}

/// The Kummerkasten only goes where admins send it: no "all" in the
/// invite allow-lists (the shared default when they're left out).
pub fn check_security(security: &SecurityConfig) -> Result<()> {
    security.validate()?;
    if security.allowed_rooms.is_all() {
        bail!(
            "[security] allowed_rooms must be a list (usually []): rooms are authorized \
             with `!kk rooms add` instead"
        );
    }
    if security.allowed_inviters.is_all() {
        bail!("[security] allowed_inviters must not be \"all\" (usually []; admins may always invite)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_is_valid() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        check_security(&config.security).unwrap();
        crate::time::Zone::parse(&config.kummerkasten.timezone).unwrap();
        crate::time::parse_hm(&config.kummerkasten.plenum_time).unwrap();
        assert!(config.security.admin_dm.require_verified_device);
    }

    #[test]
    fn open_invite_lists_are_refused() {
        let parse = |s: &str| toml::from_str::<SecurityConfig>(s).unwrap();
        assert!(check_security(&parse("")).is_err());
        assert!(check_security(&parse("allowed_rooms = []\nallowed_inviters = \"all\"")).is_err());
        assert!(check_security(&parse("allowed_rooms = []\nallowed_inviters = []")).is_ok());
    }
}
