use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

const DAY: u64 = 24 * 60 * 60;

/// `[adapters.rustup]`: where to find rustup and the projects that pin
/// toolchains, and how long a toolchain must sit unused before it is offered.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The rustup executable (`~` is expanded). When unset, `$CARGO_HOME/bin/rustup`
    /// and then `~/.cargo/bin/rustup`. `PATH` is never searched: the scheduled
    /// job's `PATH` does not include `~/.cargo/bin`, so a lookup there would
    /// make the adapter work from a terminal and silently do nothing hourly.
    pub rustup: Option<PathBuf>,
    /// Project trees searched for `rust-toolchain` / `rust-toolchain.toml`
    /// pins. Defaults to `["~"]`; the built-in denylist (`~/Library`, the
    /// trash, the user cache dir) is always skipped.
    pub roots: Vec<PathBuf>,
    /// Absolute-path globs excluded from the pin search.
    pub exclude: Vec<String>,
    /// Directory levels searched below each root. Defaults to `8`. Pins
    /// deeper than this are not looked for, and so protect nothing: raise it
    /// if a project lives deeper. Reaching the limit does not mark the scan
    /// incomplete (only an unreadable directory does), since it is hit
    /// routinely in trees no project lives in.
    pub max_depth: usize,
    /// Normal pressure: offer toolchains unused for at least this long.
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
    /// Scavenge floor: never offer a toolchain used more recently than this.
    #[serde(with = "humantime_serde")]
    pub min_age: Duration,
    /// A pin protects its toolchain only while its project was active within
    /// this window. A project nobody has touched for longer will not be built
    /// with that toolchain again soon, and rustup reinstalls it on demand if it
    /// is.
    #[serde(with = "humantime_serde")]
    pub pin_max_idle: Duration,
    /// Queue toolchains for `disk-saver review` instead of uninstalling them
    /// unattended. On by default.
    pub confirm: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rustup: None,
            roots: vec![PathBuf::from("~")],
            exclude: Vec::new(),
            max_depth: 8,
            max_age: Duration::from_secs(30 * DAY),
            min_age: Duration::from_secs(7 * DAY),
            pin_max_idle: Duration::from_secs(30 * DAY),
            confirm: true,
        }
    }
}
