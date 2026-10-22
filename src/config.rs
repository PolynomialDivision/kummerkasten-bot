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
    /// No longer used: the plenum's dates come from `[kummerkasten.calendar]`.
    /// Still accepted (and ignored, with a warning) for one release, so an
    /// older config keeps starting the bot.
    #[serde(default)]
    pub plenum_time: Option<String>,
    /// Where the plenum's dates come from. Left out: the bot has no dates
    /// and leaves the plenums it knows as they are.
    #[serde(default)]
    pub calendar: Option<CalendarConfig>,
    /// Starting values of the settings admins can change at runtime.
    #[serde(default)]
    pub settings: Tunables,
    /// `test`: a separate database, nobody but the admins has access, and
    /// a check-in goes only to the admin who starts it.
    #[serde(default)]
    pub mode: Mode,
    /// The authorized rooms, when the config manages them (`[[kummerkasten.rooms]]`):
    /// synced at every start, and `!kk rooms add/remove/central` are refused.
    /// Left out: admins manage them with those commands.
    #[serde(default)]
    pub rooms: Option<Vec<RoomConfig>>,
    /// What residents see: only the check-ins admins start (`survey`, the
    /// default for now), or the whole Kummerkasten (`full`).
    #[serde(default)]
    pub ui: Ui,
}

/// What residents get to see. Everything stays in the code and the
/// database either way; `survey` only keeps it out of their sight.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Ui {
    /// Only the check-ins (surveys) admins start: no menu, no entry
    /// messages, no summaries. A resident gets the survey, answers it, done.
    #[default]
    Survey,
    /// The whole Kummerkasten: menu, feedback, ideas, results, summaries.
    Full,
}

impl Default for KkConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            plenum_time: None,
            calendar: None,
            settings: Tunables::default(),
            mode: Mode::default(),
            rooms: None,
            ui: Ui::default(),
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Live,
    /// Trying it out: everything as live, but in its own database (the
    /// live one is never touched), only admins may use it, and check-ins
    /// go only to the admin who starts them. Messages say 🧪 Test.
    Test,
}

impl Mode {
    /// The bot's own files in the store directory (the Matrix session is
    /// the same in both modes): database, key, settings.
    pub fn files(self) -> [&'static str; 3] {
        match self {
            Mode::Live => ["kummerkasten.sqlite", "kummerkasten.key", "settings.json"],
            Mode::Test => [
                "kummerkasten-test.sqlite",
                "kummerkasten-test.key",
                "settings-test.json",
            ],
        }
    }
}

/// The Nextcloud calendar with the plenum's dates (`[kummerkasten.calendar]`):
/// plenum-bot's, shared read-only with this bot's own account.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    /// The bot's CalDAV calendar home,
    /// `https://cloud.example.org/remote.php/dav/calendars/<username>/`.
    pub account_url: String,
    /// The calendar's name at its owner (`plenum-house`). Shared with this
    /// account, Nextcloud calls it `<calendar>_shared_by_<owner>`.
    pub calendar: String,
    pub username: String,
    /// The environment variable holding the app password (never the config).
    #[serde(default = "default_password_env")]
    pub password_env: String,
    /// How often the calendar is read.
    #[serde(default = "default_refresh_minutes")]
    pub refresh_minutes: u64,
}

fn default_password_env() -> String {
    "CALDAV_PASSWORD".to_owned()
}

fn default_refresh_minutes() -> u64 {
    5
}

/// One authorized room from the config.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoomConfig {
    pub id: String,
    pub label: String,
    /// The central house room: the day-before summary, `announce`.
    #[serde(default)]
    pub central: bool,
    /// Post the 📮 entry message here once (when none was posted yet).
    #[serde(default)]
    pub entry: bool,
}

/// Room IDs that parse, each once, and test mode only with admins.
pub fn check_kummerkasten(kk: &KkConfig, security: &SecurityConfig) -> Result<()> {
    if let Some(rooms) = &kk.rooms {
        let mut seen = std::collections::HashSet::new();
        for room in rooms {
            if mxbot_common::matrix_sdk::ruma::RoomId::parse(&room.id).is_err() {
                bail!(
                    "[[kummerkasten.rooms]] {:?} isn't a room ID (!…:server)",
                    room.id
                );
            }
            if !seen.insert(room.id.as_str()) {
                bail!("[[kummerkasten.rooms]] {:?} is listed twice", room.id);
            }
            if room.label.trim().is_empty() {
                bail!("[[kummerkasten.rooms]] {:?} needs a label", room.id);
            }
        }
    }
    if let Some(calendar) = &kk.calendar {
        if !calendar.account_url.starts_with("https://")
            && !calendar.account_url.starts_with("http://")
        {
            bail!("[kummerkasten.calendar] account_url must be an http(s) URL");
        }
        for (name, value) in [
            ("calendar", &calendar.calendar),
            ("username", &calendar.username),
            ("password_env", &calendar.password_env),
        ] {
            if value.trim().is_empty() {
                bail!("[kummerkasten.calendar] {name} must not be empty");
            }
        }
    }
    if kk.mode == Mode::Test && security.admin_users.is_empty() {
        bail!("mode = \"test\" needs [security] admin_users: only they may use it");
    }
    Ok(())
}

fn default_timezone() -> String {
    "Europe/Berlin".to_owned()
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
        assert!(config.kummerkasten.plenum_time.is_none());
        let calendar = config.kummerkasten.calendar.as_ref().unwrap();
        assert_eq!(calendar.calendar, "plenum-house");
        assert_eq!(calendar.password_env, "CALDAV_PASSWORD");
        assert!(config.security.admin_dm.require_verified_device);
        check_kummerkasten(&config.kummerkasten, &config.security).unwrap();
    }

    #[test]
    fn rooms_from_the_config_and_test_mode_are_checked() {
        let parse = |s: &str| toml::from_str::<Config>(&format!("{MATRIX}{s}")).unwrap();
        let ok = parse(
            "[security]\nadmin_users = [\"@a:x\"]\n[kummerkasten]\nmode = \"test\"\n\
             [[kummerkasten.rooms]]\nid = \"!h:x\"\nlabel = \"House\"\ncentral = true\nentry = true\n",
        );
        check_kummerkasten(&ok.kummerkasten, &ok.security).unwrap();
        assert_eq!(ok.kummerkasten.mode, Mode::Test);
        let rooms = ok.kummerkasten.rooms.unwrap();
        assert!(rooms[0].central && rooms[0].entry);
        // Room version 12 IDs have no server name.
        let v12 = parse(
            "[[kummerkasten.rooms]]\nid = \"!SLkThsL4ZaZ7j9yD75hKGRqdzXOZE8C_wi56tOtnq1M\"\nlabel = \"Testraum\"\n",
        );
        check_kummerkasten(&v12.kummerkasten, &v12.security).unwrap();
        // Not managed unless listed; live by default.
        let plain = parse("");
        assert!(plain.kummerkasten.rooms.is_none());
        assert_eq!(plain.kummerkasten.mode, Mode::Live);
        assert_ne!(Mode::Live.files(), Mode::Test.files());
        // Test mode without admins would lock everyone out.
        let nobody = parse("[kummerkasten]\nmode = \"test\"\n");
        assert!(check_kummerkasten(&nobody.kummerkasten, &nobody.security).is_err());
        let twice = parse(
            "[[kummerkasten.rooms]]\nid = \"!h:x\"\nlabel = \"A\"\n\
             [[kummerkasten.rooms]]\nid = \"!h:x\"\nlabel = \"B\"\n",
        );
        assert!(check_kummerkasten(&twice.kummerkasten, &twice.security).is_err());
        let bad = parse("[[kummerkasten.rooms]]\nid = \"house\"\nlabel = \"A\"\n");
        assert!(check_kummerkasten(&bad.kummerkasten, &bad.security).is_err());
    }

    #[test]
    fn the_calendar_section_and_the_old_plenum_time() {
        let parse = |s: &str| toml::from_str::<Config>(&format!("{MATRIX}{s}"));
        // What house-infra renders: the calendar, and (for one release)
        // still `plenum_time`, accepted and ignored.
        let config = parse(
            "[kummerkasten]\nplenum_time = \"19:00\"\n[kummerkasten.calendar]\n\
             account_url = \"https://cloud.example.org/remote.php/dav/calendars/kummerkasten-bot/\"\n\
             calendar = \"plenum-house\"\nusername = \"kummerkasten-bot\"\n",
        )
        .unwrap();
        let kk = &config.kummerkasten;
        assert_eq!(kk.plenum_time.as_deref(), Some("19:00"));
        let calendar = kk.calendar.as_ref().unwrap();
        assert_eq!(
            (calendar.password_env.as_str(), calendar.refresh_minutes),
            ("CALDAV_PASSWORD", 5)
        );
        check_kummerkasten(kk, &config.security).unwrap();
        // Without it: no dates, but the bot starts.
        let old = parse("[kummerkasten]\nplenum_time = \"19:00\"\n").unwrap();
        assert!(old.kummerkasten.calendar.is_none());
        check_kummerkasten(&old.kummerkasten, &old.security).unwrap();
        // A password in the config is refused: it belongs in the environment.
        assert!(parse(
            "[kummerkasten.calendar]\naccount_url = \"https://c.x/\"\ncalendar = \"p\"\n\
             username = \"u\"\npassword = \"secret\"\n"
        )
        .is_err());
        let empty = parse(
            "[kummerkasten.calendar]\naccount_url = \"https://c.x/\"\ncalendar = \" \"\nusername = \"u\"\n",
        )
        .unwrap();
        assert!(check_kummerkasten(&empty.kummerkasten, &empty.security).is_err());
    }

    #[test]
    fn the_config_house_infra_renders_is_valid_also_the_old_one() {
        let rendered = include_str!("../testdata/house-infra.config.toml");
        let config: Config = toml::from_str(rendered).unwrap();
        check_security(&config.security).unwrap();
        check_kummerkasten(&config.kummerkasten, &config.security).unwrap();
        let calendar = config.kummerkasten.calendar.unwrap();
        assert_eq!(calendar.calendar, "plenum-house");
        assert!(calendar
            .account_url
            .ends_with("/calendars/kummerkasten-bot/"));
        // The one deployed before the calendar: `plenum_time`, no calendar.
        let start = rendered.find("[kummerkasten.calendar]").unwrap();
        let end = rendered.find("[kummerkasten.settings]").unwrap();
        let old = format!(
            "{}{}",
            rendered[..start].replace(
                "[kummerkasten]\n",
                "[kummerkasten]\nplenum_time = \"19:00\"\n"
            ),
            &rendered[end..]
        );
        let config: Config = toml::from_str(&old).unwrap();
        assert_eq!(config.kummerkasten.plenum_time.as_deref(), Some("19:00"));
        assert!(config.kummerkasten.calendar.is_none());
    }

    const MATRIX: &str =
        "[matrix]\nhomeserver = \"https://m.x\"\nuser_id = \"@k:x\"\ndevice_id = \"D\"\n";

    #[test]
    fn open_invite_lists_are_refused() {
        let parse = |s: &str| toml::from_str::<SecurityConfig>(s).unwrap();
        assert!(check_security(&parse("")).is_err());
        assert!(check_security(&parse("allowed_rooms = []\nallowed_inviters = \"all\"")).is_err());
        assert!(check_security(&parse("allowed_rooms = []\nallowed_inviters = []")).is_ok());
    }
}
