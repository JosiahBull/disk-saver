//! Operational logging setup (§15.1).
//!
//! Interactive (stderr is a TTY): pretty, human-friendly logs to stderr.
//! Non-interactive (scheduled): daily-rotated files in `<state_dir>/logs/` via
//! `tracing-appender`, so a launchd/systemd run leaves an inspectable trail. Only
//! the newest [`LOG_FILES_KEPT`] files are kept; older ones are deleted as each
//! run opens the log.
//!
//! Verbosity: default `WARN`, so a healthy run writes nothing; `--verbose` raises
//! it to `DEBUG`. `RUST_LOG`, if set, overrides both (`RUST_LOG=info` shows one
//! line per deleted item).

use std::io::IsTerminal;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

/// How many daily log files to keep — two weeks for a machine that runs every day.
const LOG_FILES_KEPT: usize = 14;

/// Build the level filter from `RUST_LOG`, or a default keyed off `--verbose`.
fn env_filter(verbose: bool) -> EnvFilter {
    let default = if verbose { "debug" } else { "warn" };
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

/// The daily-rotated appender over `logs_dir`. Building it deletes all but the
/// newest [`LOG_FILES_KEPT`] `disk-saver.log.*` files there (counting the one it
/// opens); anything else in the directory, such as `launchd.err`, is left alone.
fn file_appender(logs_dir: &Path) -> Result<RollingFileAppender, InitError> {
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("disk-saver.log")
        .max_log_files(LOG_FILES_KEPT)
        .build(logs_dir)
}

/// Initialise logging for a normal command.
///
/// When stderr is a TTY, logs go to stderr (pretty). Otherwise they are written
/// to a daily-rotated file in `<state_dir>/logs/`; the returned [`WorkerGuard`]
/// must be kept alive for the duration of the process so buffered lines flush.
/// If the log file cannot be opened, logs fall back to stderr.
#[must_use = "keep the returned guard alive so file logs are flushed"]
pub fn init(state_dir: &Path, verbose: bool) -> Option<WorkerGuard> {
    if std::io::stderr().is_terminal() {
        init_interactive(verbose);
        return None;
    }

    let appender = match file_appender(&state_dir.join("logs")) {
        Ok(appender) => appender,
        Err(e) => {
            init_interactive(verbose);
            tracing::warn!(error = %e, "could not open the log file; logging to stderr");
            return None;
        }
    };
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter(verbose))
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .try_init();
    Some(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_the_log_prunes_old_daily_files_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        for day in 1..=20 {
            std::fs::write(
                dir.path().join(format!("disk-saver.log.2020-01-{day:02}")),
                "x",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("launchd.err"), "").unwrap();

        let _appender = file_appender(dir.path()).unwrap();

        let logs = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("disk-saver.log."))
            .count();
        assert_eq!(
            logs, LOG_FILES_KEPT,
            "the kept files include the one just opened"
        );
        assert!(dir.path().join("launchd.err").exists());
    }
}
