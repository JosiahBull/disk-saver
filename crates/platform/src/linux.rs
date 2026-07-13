//! The Linux [`Platform`] implementation (also the fallback for other non-macOS
//! unixes).
//!
//! All the heavy lifting (filesystem, sizing, deletion, `statvfs`, subprocess)
//! is shared and lives in [`crate::sys`]; this file holds only what is genuinely
//! Linux-specific: the XDG Trash and cache locations, and notification delivery
//! via `notify-send`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{
    CommandOutput, CommandSpec, DirEntry, DiskUsage, FileMeta, Notification, Platform, Urgency, sys,
};

/// The production [`Platform`] on Linux. Zero-sized; construct with
/// [`LinuxPlatform::new`].
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxPlatform;

impl LinuxPlatform {
    /// Create a new `LinuxPlatform`.
    #[must_use]
    pub fn new() -> Self {
        LinuxPlatform
    }
}

/// Value of an XDG base-directory environment variable, treating unset/empty as
/// absent and returning `fallback` in that case.
fn xdg_dir(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(fallback)
}

/// Build the `notify-send` command that shows `n` as a desktop notification.
fn notify_command(n: &Notification) -> CommandSpec {
    let urgency = match n.urgency {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    };
    CommandSpec::new(
        "notify-send",
        [
            "-u".to_string(),
            urgency.to_string(),
            n.title.clone(),
            n.body.clone(),
        ],
    )
}

impl Platform for LinuxPlatform {
    fn now(&self) -> SystemTime {
        sys::now()
    }

    fn home_dir(&self) -> PathBuf {
        sys::home_dir()
    }

    fn trash_dirs(&self) -> Vec<PathBuf> {
        // XDG: $XDG_DATA_HOME/Trash, else ~/.local/share/Trash.
        let base = xdg_dir("XDG_DATA_HOME", || {
            self.home_dir().join(".local").join("share")
        });
        vec![base.join("Trash")]
    }

    fn user_cache_dir(&self) -> PathBuf {
        // XDG: $XDG_CACHE_HOME, else ~/.cache.
        xdg_dir("XDG_CACHE_HOME", || self.home_dir().join(".cache"))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMeta> {
        sys::metadata(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        sys::read_dir(path)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        sys::read_to_string(path)
    }

    fn dir_size(&self, path: &Path) -> io::Result<u64> {
        sys::dir_size(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        sys::remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        sys::remove_dir_all(path)
    }

    fn disk_usage(&self, path: &Path) -> io::Result<DiskUsage> {
        sys::disk_usage(path)
    }

    fn notify(&self, n: &Notification) -> io::Result<()> {
        sys::notify(n, Some(notify_command(n)))
    }

    fn run_command(&self, cmd: &CommandSpec) -> io::Result<CommandOutput> {
        sys::run_command(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_command_builds_notify_send() {
        let n = Notification {
            title: "T".into(),
            body: "B".into(),
            urgency: Urgency::Critical,
        };
        let spec = notify_command(&n);
        assert_eq!(spec.program, "notify-send");
        assert_eq!(spec.args, vec!["-u", "critical", "T", "B"]);
    }

    #[test]
    fn well_known_dirs_are_xdg_shaped() {
        let p = LinuxPlatform::new();
        assert!(p.trash_dirs()[0].ends_with("Trash"));
        assert!(!p.user_cache_dir().as_os_str().is_empty());
    }

    #[test]
    #[ignore = "delivers a real desktop notification; run manually"]
    fn notify_is_best_effort_ok() {
        let p = LinuxPlatform::new();
        let n = Notification {
            title: "disk-saver test".to_string(),
            body: "hello \"world\"".to_string(),
            urgency: Urgency::Low,
        };
        p.notify(&n).unwrap();
    }
}
