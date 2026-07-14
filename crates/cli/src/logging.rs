//! Operational logging setup (§15.1).
//!
//! Interactive (stderr is a TTY): pretty, human-friendly logs to stderr.
//! Non-interactive (scheduled): daily-rotated files in `<state_dir>/logs/` via
//! `tracing-appender`, so a launchd/systemd run leaves an inspectable trail.
//!
//! Verbosity: default `INFO`; `--verbose` raises it to `DEBUG`. `RUST_LOG`, if
//! set, overrides both.

use std::io::IsTerminal;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Build the level filter from `RUST_LOG`, or a default keyed off `--verbose`.
fn env_filter(verbose: bool) -> EnvFilter {
    let default = if verbose { "debug" } else { "info" };
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
}

/// Initialise logging for an interactive/short-lived command (always to
/// stderr). Returns no worker guard.
///
/// Used before the state directory is known (e.g. `config init`).
pub fn init_interactive(verbose: bool) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter(verbose))
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Initialise logging for a normal command.
///
/// When stderr is a TTY, logs go to stderr (pretty). Otherwise they are written
/// to a daily-rotated file in `<state_dir>/logs/`; the returned [`WorkerGuard`]
/// must be kept alive for the duration of the process so buffered lines flush.
#[must_use = "keep the returned guard alive so file logs are flushed"]
pub fn init(state_dir: &Path, verbose: bool) -> Option<WorkerGuard> {
    if std::io::stderr().is_terminal() {
        init_interactive(verbose);
        return None;
    }

    let appender = tracing_appender::rolling::daily(state_dir.join("logs"), "disk-saver.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter(verbose))
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .try_init();
    Some(guard)
}
