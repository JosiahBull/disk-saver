//! The macOS [`Platform`] implementation.
//!
//! All the heavy lifting (filesystem, sizing, deletion, `statvfs`, subprocess)
//! is shared and lives in [`crate::sys`]; this file holds only what is genuinely
//! macOS-specific: the Trash and cache locations, and notification delivery via
//! `osascript`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{
    CommandOutput, CommandSpec, DirEntry, DiskUsage, FileMeta, Notification, Platform, sys,
};

/// The production [`Platform`] on macOS. Zero-sized; construct with
/// [`MacOsPlatform::new`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MacOsPlatform;

impl MacOsPlatform {
    /// Create a new `MacOsPlatform`.
    #[must_use]
    pub fn new() -> Self {
        MacOsPlatform
    }
}

/// Escape a string for embedding inside an AppleScript double-quoted literal:
/// backslash and double-quote must be escaped.
fn escape_applescript(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out
}

/// Build the `osascript` command that shows `n` as a desktop notification.
fn notify_command(n: &Notification) -> CommandSpec {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape_applescript(&n.body),
        escape_applescript(&n.title),
    );
    CommandSpec::new("osascript", ["-e".to_string(), script])
}

impl Platform for MacOsPlatform {
    fn now(&self) -> SystemTime {
        sys::now()
    }

    fn home_dir(&self) -> PathBuf {
        sys::home_dir()
    }

    fn trash_dirs(&self) -> Vec<PathBuf> {
        vec![self.home_dir().join(".Trash")]
    }

    fn user_cache_dir(&self) -> PathBuf {
        self.home_dir().join("Library").join("Caches")
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
    use crate::Urgency;

    #[test]
    fn escape_applescript_escapes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("plain"), "plain");
        assert_eq!(escape_applescript(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_applescript(r"a\b"), r"a\\b");
        assert_eq!(escape_applescript(r"\"), r"\\");
    }

    #[test]
    fn notify_command_builds_osascript() {
        let n = Notification {
            title: "T".into(),
            body: "B".into(),
            urgency: Urgency::Normal,
        };
        let spec = notify_command(&n);
        assert_eq!(spec.program, "osascript");
        assert_eq!(spec.args[0], "-e");
        assert!(spec.args[1].contains("display notification \"B\" with title \"T\""));
    }

    #[test]
    fn well_known_dirs_are_macos_shaped() {
        let p = MacOsPlatform::new();
        assert!(p.trash_dirs()[0].ends_with(".Trash"));
        assert!(p.user_cache_dir().ends_with("Library/Caches"));
    }

    #[test]
    #[ignore = "delivers a real desktop notification; run manually"]
    fn notify_is_best_effort_ok() {
        let p = MacOsPlatform::new();
        let n = Notification {
            title: "disk-saver test".to_string(),
            body: "hello \"world\"".to_string(),
            urgency: Urgency::Low,
        };
        p.notify(&n).unwrap();
    }
}
