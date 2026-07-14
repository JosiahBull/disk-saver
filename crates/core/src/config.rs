//! Configuration model and loading pipeline (§7).
//!
//! The single config file (`~/.disk-saver.toml`) is parsed into a fully typed
//! [`Config`]. The engine reads exactly one reserved key per adapter section
//! (`enabled`); everything else in a `[adapters.<name>]` table is preserved
//! verbatim as an opaque [`toml::Value`] and handed to the adapter's factory.
//!
//! Sizes are parsed as [`Threshold`]s (`"40GB"` absolute or `"10%"` of the
//! disk); durations use humantime syntax (`"7d"`, `"36h"`). Tilde expansion is
//! done by the caller via [`expand_tilde`].

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// A free-space threshold: either an absolute number of bytes or a percentage of
/// the total disk. Deserialized from strings like `"40GB"` or `"10%"`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Threshold {
    /// An absolute number of bytes.
    Absolute(u64),
    /// A percentage (0–100) of total disk capacity.
    Percent(f64),
}

impl Threshold {
    /// Resolve this threshold to an absolute number of bytes against `total`.
    pub fn bytes(&self, total: u64) -> u64 {
        match self {
            Threshold::Absolute(b) => *b,
            Threshold::Percent(p) => ((total as f64) * p / 100.0) as u64,
        }
    }

    /// Parse a threshold string (`"40GB"` | `"512MiB"` | `"10%"`).
    fn parse(s: &str) -> Result<Threshold, String> {
        let s = s.trim();
        if let Some(pct) = s.strip_suffix('%') {
            let v: f64 = pct
                .trim()
                .parse()
                .map_err(|_| format!("invalid percentage: {s:?}"))?;
            if !(0.0..=100.0).contains(&v) {
                return Err(format!("percentage out of range (0-100): {s:?}"));
            }
            Ok(Threshold::Percent(v))
        } else {
            let bytes = s
                .parse::<bytesize::ByteSize>()
                .map_err(|e| format!("invalid size {s:?}: {e}"))?;
            Ok(Threshold::Absolute(bytes.0))
        }
    }
}

impl<'de> Deserialize<'de> for Threshold {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Threshold::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// The `[global]` section.
#[derive(Debug, Clone)]
pub struct GlobalConfig {
    /// Filesystem whose free space drives the pressure model. `None` → the
    /// filesystem containing `$HOME`.
    pub disk: Option<PathBuf>,
    /// Above this much free → observe only (default 20%).
    pub start_cleaning_below: Threshold,
    /// Below this much free → warn + faster cadence (default 12%).
    pub warn_below: Threshold,
    /// Below this much free → scavenge mode (default 8%).
    pub scavenge_below: Threshold,
    /// Scavenge until this much is free, then stop (default 15%).
    pub scavenge_target: Threshold,
    /// Default hard timeout for adapter subprocesses (default 60s).
    pub command_timeout: Duration,
    /// Optional override for the sqlite/logs/lock location.
    pub state_dir: Option<PathBuf>,
}

impl GlobalConfig {
    /// Validate threshold ordering, resolved against `total` bytes:
    /// `scavenge_below < warn_below <= start_cleaning_below` and
    /// `scavenge_below < scavenge_target <= start_cleaning_below`.
    pub fn validate_thresholds(&self, total: u64) -> Result<(), ConfigError> {
        let scavenge = self.scavenge_below.bytes(total);
        let warn = self.warn_below.bytes(total);
        let start = self.start_cleaning_below.bytes(total);
        let target = self.scavenge_target.bytes(total);
        if !(scavenge < warn && warn <= start) {
            return Err(ConfigError::invalid(format!(
                "thresholds must satisfy scavenge_below < warn_below <= start_cleaning_below \
                 (resolved to {scavenge} < {warn} <= {start} bytes)"
            )));
        }
        if !(scavenge < target && target <= start) {
            return Err(ConfigError::invalid(format!(
                "thresholds must satisfy scavenge_below < scavenge_target <= start_cleaning_below \
                 (resolved to {scavenge} < {target} <= {start} bytes)"
            )));
        }
        Ok(())
    }
}

/// The `[schedule]` section (§14). Cadence lives in config, not the timer.
#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    /// How often the OS timer fires (a cheap probe; usually exits). Default 1h.
    pub check_every: Duration,
    /// Minimum interval between full runs normally. Default 12h.
    pub run_every: Duration,
    /// Minimum interval between full runs when free space is low. Default 1h.
    pub pressure_run_every: Duration,
}

/// A notification event the engine can emit (§15.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationEvent {
    /// Free space crossed below `warn_below`.
    ScavengeWarning,
    /// A scavenge-mode run executed.
    ScavengeRan,
    /// The approvals queue gained items / is still non-empty.
    ApprovalsPending,
    /// An adapter failed on several consecutive runs.
    AdapterFailing,
}

/// The `[notifications]` section (§15.3).
#[derive(Debug, Clone)]
pub struct NotificationConfig {
    /// Master switch; `false` silences everything.
    pub enabled: bool,
    /// Don't repeat the same event more often than this. Default 12h.
    pub min_gap: Duration,
    /// Which events may fire.
    pub events: Vec<NotificationEvent>,
}

/// One `[adapters.<name>]` section, split into the reserved `enabled` key and
/// the opaque remainder handed to the adapter.
#[derive(Debug, Clone)]
pub struct AdapterSection {
    /// The one reserved key (default `true`).
    pub enabled: bool,
    /// Everything else in the table, verbatim; `None` if the table was empty.
    pub rest: Option<toml::Value>,
}

/// The fully typed configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The `[global]` section.
    pub global: GlobalConfig,
    /// The `[schedule]` section.
    pub schedule: ScheduleConfig,
    /// The `[notifications]` section.
    pub notifications: NotificationConfig,
    /// Per-adapter sections, keyed by adapter name.
    pub adapters: BTreeMap<String, AdapterSection>,
}

impl Config {
    /// The built-in defaults (equivalent to an empty config file).
    pub fn defaults() -> Config {
        Config {
            global: RawGlobal::default().into(),
            schedule: RawSchedule::default().into(),
            notifications: RawNotifications::default().into(),
            adapters: BTreeMap::new(),
        }
    }

    /// Parse config from TOML text. Missing sections fall back to defaults. Each
    /// `[adapters.<name>]` table is split into its reserved `enabled` key and an
    /// opaque remainder ([`AdapterSection::rest`]).
    pub fn parse(toml_text: &str) -> Result<Config, ConfigError> {
        let raw: RawConfig =
            toml::from_str(toml_text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let mut adapters = BTreeMap::new();
        for (name, value) in raw.adapters {
            adapters.insert(name.clone(), split_adapter_section(&name, value)?);
        }
        Ok(Config {
            global: raw.global.into(),
            schedule: raw.schedule.into(),
            notifications: raw.notifications.into(),
            adapters,
        })
    }

    /// Read the config file via the platform (so tests use the fake filesystem),
    /// then parse. A missing file yields [`Config::defaults`]; other read errors
    /// become [`ConfigError::Read`]. Does not expand `~` (the engine/CLI do).
    pub fn load(path: &Path, platform: &dyn crate::Platform) -> Result<Config, ConfigError> {
        match platform.read_to_string(path) {
            Ok(text) => Config::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::defaults()),
            Err(e) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Structural validation independent of disk size: all durations must be
    /// non-zero. (Event names and threshold syntax are validated during
    /// deserialization; threshold *ordering* needs a disk size, see
    /// [`GlobalConfig::validate_thresholds`].)
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.global.command_timeout.is_zero() {
            return Err(ConfigError::invalid(
                "global.command_timeout must be greater than zero",
            ));
        }
        // `schedule install` renders check_every via whole seconds
        // (`as_secs()`), so a sub-second value would produce a degenerate `0s`
        // launchd StartInterval / systemd OnUnitActiveSec that still reports
        // success. Require at least one whole second.
        if self.schedule.check_every < Duration::from_secs(1) {
            return Err(ConfigError::invalid(
                "schedule.check_every must be at least 1s (the OS scheduler interval is whole seconds)",
            ));
        }
        if self.schedule.run_every.is_zero() {
            return Err(ConfigError::invalid(
                "schedule.run_every must be greater than zero",
            ));
        }
        if self.schedule.pressure_run_every.is_zero() {
            return Err(ConfigError::invalid(
                "schedule.pressure_run_every must be greater than zero",
            ));
        }
        if self.notifications.enabled && self.notifications.min_gap.is_zero() {
            return Err(ConfigError::invalid(
                "notifications.min_gap must be greater than zero",
            ));
        }
        Ok(())
    }

    /// A fully-commented default config file, for `disk-saver config init`.
    pub fn default_toml() -> &'static str {
        DEFAULT_TOML
    }
}

/// Split a `[adapters.<name>]` table into its reserved `enabled` key and the
/// opaque remainder.
fn split_adapter_section(name: &str, value: toml::Value) -> Result<AdapterSection, ConfigError> {
    match value {
        toml::Value::Table(mut table) => {
            let enabled = match table.remove("enabled") {
                None => true,
                Some(toml::Value::Boolean(b)) => b,
                Some(other) => {
                    return Err(ConfigError::Adapter {
                        adapter: name.to_owned(),
                        message: format!("'enabled' must be a boolean, got {}", other.type_str()),
                    });
                }
            };
            let rest = if table.is_empty() {
                None
            } else {
                Some(toml::Value::Table(table))
            };
            Ok(AdapterSection { enabled, rest })
        }
        other => Err(ConfigError::Adapter {
            adapter: name.to_owned(),
            message: format!("section must be a table, got {}", other.type_str()),
        }),
    }
}

/// Expand a leading `~` component of `path` against `home`. Any other path is
/// returned unchanged. Adapters and the CLI use this for `roots`/`disk`/etc.
pub fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    let mut comps = path.components();
    if let Some(Component::Normal(first)) = comps.next()
        && first == "~"
    {
        return home.join(comps.as_path());
    }
    path.to_path_buf()
}

// ── internal raw (serde) representations ────────────────────────────────────
//
// The public types intentionally do not derive `Deserialize` (to keep their
// surface exactly as the contract specifies), so parsing goes through these
// private mirrors, each carrying the documented defaults.

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    global: RawGlobal,
    schedule: RawSchedule,
    notifications: RawNotifications,
    adapters: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawGlobal {
    disk: Option<PathBuf>,
    start_cleaning_below: Threshold,
    warn_below: Threshold,
    scavenge_below: Threshold,
    scavenge_target: Threshold,
    #[serde(with = "humantime_serde")]
    command_timeout: Duration,
    state_dir: Option<PathBuf>,
}

impl Default for RawGlobal {
    fn default() -> Self {
        Self {
            disk: None,
            start_cleaning_below: Threshold::Percent(20.0),
            warn_below: Threshold::Percent(12.0),
            scavenge_below: Threshold::Percent(8.0),
            scavenge_target: Threshold::Percent(15.0),
            command_timeout: Duration::from_secs(60),
            state_dir: None,
        }
    }
}

impl From<RawGlobal> for GlobalConfig {
    fn from(r: RawGlobal) -> Self {
        GlobalConfig {
            disk: r.disk,
            start_cleaning_below: r.start_cleaning_below,
            warn_below: r.warn_below,
            scavenge_below: r.scavenge_below,
            scavenge_target: r.scavenge_target,
            command_timeout: r.command_timeout,
            state_dir: r.state_dir,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawSchedule {
    #[serde(with = "humantime_serde")]
    check_every: Duration,
    #[serde(with = "humantime_serde")]
    run_every: Duration,
    #[serde(with = "humantime_serde")]
    pressure_run_every: Duration,
}

impl Default for RawSchedule {
    fn default() -> Self {
        Self {
            check_every: Duration::from_secs(3600),
            run_every: Duration::from_secs(12 * 3600),
            pressure_run_every: Duration::from_secs(3600),
        }
    }
}

impl From<RawSchedule> for ScheduleConfig {
    fn from(r: RawSchedule) -> Self {
        ScheduleConfig {
            check_every: r.check_every,
            run_every: r.run_every,
            pressure_run_every: r.pressure_run_every,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawNotifications {
    enabled: bool,
    #[serde(with = "humantime_serde")]
    min_gap: Duration,
    events: Vec<NotificationEvent>,
}

impl Default for RawNotifications {
    fn default() -> Self {
        Self {
            enabled: true,
            min_gap: Duration::from_secs(12 * 3600),
            events: vec![
                NotificationEvent::ScavengeWarning,
                NotificationEvent::ScavengeRan,
                NotificationEvent::ApprovalsPending,
                NotificationEvent::AdapterFailing,
            ],
        }
    }
}

impl From<RawNotifications> for NotificationConfig {
    fn from(r: RawNotifications) -> Self {
        NotificationConfig {
            enabled: r.enabled,
            min_gap: r.min_gap,
            events: r.events,
        }
    }
}

/// The fully-commented default config text (§7).
const DEFAULT_TOML: &str = r#"# disk-saver configuration.
# All settings are optional; anything omitted uses the built-in default.

[global]
# Filesystem whose free space drives the pressure model.
# Default: the filesystem containing your home directory.
# disk = "/"

# Thresholds: absolute ("40GB") or percentage of total disk ("10%").
start_cleaning_below = "20%"   # more free than this -> observe only, delete nothing
warn_below           = "12%"   # less free than this -> notify + faster cadence
scavenge_below       = "8%"    # less free than this -> scavenge mode
scavenge_target      = "15%"   # scavenge until this much is free, then stop

command_timeout = "60s"        # default hard timeout for adapter subprocesses
# state_dir = "..."            # optional override for sqlite/logs/lock location

[schedule]
check_every        = "1h"      # how often the OS timer fires (cheap probe)
run_every          = "12h"     # min interval between full runs normally
pressure_run_every = "1h"      # min interval once free space is below warn_below

[notifications]
enabled = true
min_gap = "12h"                # don't repeat the same event more often than this
events  = ["scavenge_warning", "scavenge_ran", "approvals_pending", "adapter_failing"]

# Everything below is opaque to the engine except the reserved `enabled` key.

[adapters.docker]
max_age = "7d"
min_age = "2d"
protect = ["postgres:*"]

[adapters.node-modules]
roots   = ["~/dev", "~/work"]
max_age = "30d"
min_age = "7d"

[adapters.rust-target]
roots   = ["~/dev"]
max_age = "30d"
min_age = "7d"

[adapters.python-cache]
roots         = ["~/dev"]
max_age       = "30d"
min_age       = "7d"
include_venvs = false

[adapters.trash]
max_age = "60d"
min_age = "7d"
confirm = true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_parse_percent_and_absolute() {
        assert_eq!(Threshold::parse("10%").unwrap(), Threshold::Percent(10.0));
        assert_eq!(
            Threshold::parse(" 12 % ").unwrap(),
            Threshold::Percent(12.0)
        );
        match Threshold::parse("40GB").unwrap() {
            Threshold::Absolute(b) => assert_eq!(b, 40_000_000_000),
            other => panic!("expected absolute, got {other:?}"),
        }
    }

    #[test]
    fn threshold_parse_rejects_bad_input() {
        assert!(Threshold::parse("150%").is_err());
        assert!(Threshold::parse("-1%").is_err());
        assert!(Threshold::parse("abc%").is_err());
        assert!(Threshold::parse("notasize").is_err());
    }

    #[test]
    fn threshold_bytes_resolution() {
        assert_eq!(Threshold::Absolute(1000).bytes(50_000), 1000);
        assert_eq!(Threshold::Percent(10.0).bytes(1000), 100);
        assert_eq!(Threshold::Percent(0.0).bytes(1000), 0);
    }

    #[test]
    fn defaults_are_as_documented() {
        let c = Config::defaults();
        assert_eq!(c.global.start_cleaning_below, Threshold::Percent(20.0));
        assert_eq!(c.global.warn_below, Threshold::Percent(12.0));
        assert_eq!(c.global.scavenge_below, Threshold::Percent(8.0));
        assert_eq!(c.global.scavenge_target, Threshold::Percent(15.0));
        assert_eq!(c.global.command_timeout, Duration::from_secs(60));
        assert!(c.global.disk.is_none());
        assert_eq!(c.schedule.check_every, Duration::from_secs(3600));
        assert_eq!(c.schedule.run_every, Duration::from_secs(12 * 3600));
        assert_eq!(c.schedule.pressure_run_every, Duration::from_secs(3600));
        assert!(c.notifications.enabled);
        assert_eq!(c.notifications.min_gap, Duration::from_secs(12 * 3600));
        assert_eq!(c.notifications.events.len(), 4);
        assert!(c.adapters.is_empty());
    }

    #[test]
    fn parse_empty_is_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.global.warn_below, Threshold::Percent(12.0));
        assert!(c.adapters.is_empty());
    }

    #[test]
    fn parse_partial_global_keeps_other_defaults() {
        let c = Config::parse("[global]\nwarn_below = \"5GB\"\n").unwrap();
        assert_eq!(c.global.warn_below, Threshold::Absolute(5_000_000_000));
        // Untouched fields keep defaults.
        assert_eq!(c.global.start_cleaning_below, Threshold::Percent(20.0));
        assert_eq!(c.global.command_timeout, Duration::from_secs(60));
    }

    #[test]
    fn parse_splits_enabled_from_adapter_rest() {
        let text = r#"
[adapters.docker]
enabled = false
max_age = "7d"
protect = ["postgres:*"]
"#;
        let c = Config::parse(text).unwrap();
        let docker = c.adapters.get("docker").unwrap();
        assert!(!docker.enabled);
        let rest = docker.rest.as_ref().unwrap();
        // `enabled` was stripped; the rest is preserved verbatim.
        let table = rest.as_table().unwrap();
        assert!(!table.contains_key("enabled"));
        assert!(table.contains_key("max_age"));
        assert!(table.contains_key("protect"));
    }

    #[test]
    fn adapter_without_enabled_defaults_true_and_empty_rest_is_none() {
        let text = "[adapters.trash]\n[adapters.docker]\nmax_age = \"1d\"\n";
        let c = Config::parse(text).unwrap();
        let trash = c.adapters.get("trash").unwrap();
        assert!(trash.enabled);
        assert!(trash.rest.is_none()); // empty table -> None
        let docker = c.adapters.get("docker").unwrap();
        assert!(docker.enabled);
        assert!(docker.rest.is_some());
    }

    #[test]
    fn unknown_adapter_section_is_preserved_for_caller() {
        // The engine has no registry; a typo'd section is simply retained so the
        // CLI can warn. This is the observable "unknown section" behaviour.
        let c = Config::parse("[adapters.dokcer]\nmax_age = \"1d\"\n").unwrap();
        assert!(c.adapters.contains_key("dokcer"));
    }

    #[test]
    fn parse_rejects_non_boolean_enabled() {
        let err = Config::parse("[adapters.docker]\nenabled = \"yes\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Adapter { .. }));
    }

    #[test]
    fn parse_rejects_invalid_toml() {
        assert!(matches!(
            Config::parse("this is = = not toml").unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(Config::defaults().validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_durations() {
        let c = Config::parse("[schedule]\nrun_every = \"0s\"\n").unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_sub_second_check_every() {
        // Non-zero but < 1s would render as a degenerate `0s` scheduler interval.
        let c = Config::parse("[schedule]\ncheck_every = \"500ms\"\n").unwrap();
        assert!(c.validate().is_err());
        // Exactly 1s is fine.
        let c = Config::parse("[schedule]\ncheck_every = \"1s\"\n").unwrap();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_thresholds_ordering() {
        let g = Config::defaults().global;
        // 8% < 12% <= 20% and 8% < 15% <= 20% for total 1000.
        assert!(g.validate_thresholds(1000).is_ok());

        let bad = Config::parse(
            "[global]\nscavenge_below = \"30%\"\nwarn_below = \"12%\"\nstart_cleaning_below = \"20%\"\nscavenge_target = \"15%\"\n",
        )
        .unwrap()
        .global;
        assert!(bad.validate_thresholds(1000).is_err());
    }

    #[test]
    fn default_toml_parses_and_validates() {
        let c = Config::parse(Config::default_toml()).unwrap();
        c.validate().unwrap();
        c.global.validate_thresholds(1_000_000).unwrap();
        // The five v1 adapters are present as sections.
        for name in [
            "docker",
            "node-modules",
            "rust-target",
            "python-cache",
            "trash",
        ] {
            assert!(c.adapters.contains_key(name), "missing {name}");
        }
        assert!(c.adapters.get("trash").unwrap().enabled);
    }

    #[test]
    fn expand_tilde_expands_leading_tilde_only() {
        let home = Path::new("/home/alice");
        assert_eq!(
            expand_tilde(Path::new("~/dev"), home),
            PathBuf::from("/home/alice/dev")
        );
        assert_eq!(
            expand_tilde(Path::new("~"), home),
            PathBuf::from("/home/alice")
        );
        // No leading tilde -> unchanged.
        assert_eq!(
            expand_tilde(Path::new("/abs/x"), home),
            PathBuf::from("/abs/x")
        );
        assert_eq!(
            expand_tilde(Path::new("rel/x"), home),
            PathBuf::from("rel/x")
        );
        // A tilde that is not the first component is left alone.
        assert_eq!(
            expand_tilde(Path::new("/a/~/b"), home),
            PathBuf::from("/a/~/b")
        );
    }

    #[test]
    fn load_missing_file_is_defaults() {
        // A fake platform whose read fails with NotFound.
        struct P;
        impl crate::Platform for P {
            fn now(&self) -> std::time::SystemTime {
                std::time::UNIX_EPOCH
            }
            fn home_dir(&self) -> PathBuf {
                PathBuf::from("/home/x")
            }
            fn trash_dirs(&self) -> Vec<PathBuf> {
                vec![]
            }
            fn user_cache_dir(&self) -> PathBuf {
                PathBuf::from("/home/x/.cache")
            }
            fn metadata(&self, _p: &Path) -> std::io::Result<crate::FileMeta> {
                Err(std::io::ErrorKind::NotFound.into())
            }
            fn read_dir(&self, _p: &Path) -> std::io::Result<Vec<crate::DirEntry>> {
                Ok(vec![])
            }
            fn read_to_string(&self, _p: &Path) -> std::io::Result<String> {
                Err(std::io::ErrorKind::NotFound.into())
            }
            fn dir_size(&self, _p: &Path) -> std::io::Result<u64> {
                Ok(0)
            }
            fn remove_file(&self, _p: &Path) -> std::io::Result<()> {
                Ok(())
            }
            fn remove_dir_all(&self, _p: &Path) -> std::io::Result<()> {
                Ok(())
            }
            fn disk_usage(&self, _p: &Path) -> std::io::Result<crate::DiskUsage> {
                Ok(crate::DiskUsage {
                    total: 0,
                    available: 0,
                })
            }
            fn notify(&self, _n: &crate::Notification) -> std::io::Result<()> {
                Ok(())
            }
            fn run_command(
                &self,
                _c: &crate::CommandSpec,
            ) -> std::io::Result<crate::CommandOutput> {
                Ok(crate::CommandOutput::ok(""))
            }
        }
        let c = Config::load(Path::new("/nope.toml"), &P).unwrap();
        assert_eq!(c.global.warn_below, Threshold::Percent(12.0));
    }
}
