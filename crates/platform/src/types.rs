//! Shared value types for the [`Platform`](crate::Platform) abstraction.
//!
//! These are plain data types with no OS knowledge; both `RealPlatform` and
//! `FakePlatform` produce and consume them.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// The kind of a filesystem node, derived from *symlink* metadata (never
/// followed): a symlink is reported as [`FileKind::Symlink`], not as whatever
/// it points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (the link itself, target never inspected).
    Symlink,
    /// Anything else (socket, fifo, block/char device, …).
    Other,
}

/// Metadata for a single filesystem node. Obtained without following symlinks.
#[derive(Debug, Clone)]
pub struct FileMeta {
    /// What kind of node this is.
    pub kind: FileKind,
    /// Apparent length in bytes (file size; `0` for directories by convention).
    pub len: u64,
    /// Last-modification time.
    pub modified: SystemTime,
    /// The id of the device (filesystem) this node lives on (`st_dev`). Used to
    /// keep walks and recursive deletion from crossing filesystem boundaries
    /// (ARCHITECTURE.md §11.8) — a mounted volume under a scanned root must not
    /// be swept or deleted by accident.
    pub dev: u64,
}

/// One entry produced by [`Platform::read_dir`](crate::Platform::read_dir).
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Absolute path of the entry.
    pub path: PathBuf,
    /// The final path component (the entry's own name).
    pub file_name: String,
    /// Node kind from symlink metadata (symlinks are *not* followed).
    pub kind: FileKind,
}

/// Filesystem free/total space, in bytes, for one filesystem.
#[derive(Debug, Clone, Copy)]
pub struct DiskUsage {
    /// Total capacity of the filesystem, in bytes.
    pub total: u64,
    /// Space currently available to an unprivileged process, in bytes.
    pub available: u64,
}

/// How prominently a desktop notification should be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Low priority; may be shown quietly.
    Low,
    /// Normal priority (the default).
    Normal,
    /// Critical priority; shown prominently and may persist.
    Critical,
}

/// A desktop notification to deliver via [`Platform::notify`](crate::Platform::notify).
#[derive(Debug, Clone)]
pub struct Notification {
    /// Short bold headline.
    pub title: String,
    /// Body text.
    pub body: String,
    /// How urgent the notification is.
    pub urgency: Urgency,
}

/// A subprocess to run via [`Platform::run_command`](crate::Platform::run_command).
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Program name or path (resolved against `PATH` by the platform).
    pub program: String,
    /// Arguments, not including the program itself.
    pub args: Vec<String>,
    /// Hard timeout; `None` defers to the caller/global default (60s).
    pub timeout: Option<Duration>,
}

impl CommandSpec {
    /// Build a spec from a program name and its arguments. Timeout defaults to
    /// `None` (the platform applies its 60s default).
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout: None,
        }
    }

    /// Set a hard timeout for this command, consuming and returning `self`.
    #[must_use]
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }
}

/// The result of running a [`CommandSpec`] to completion.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Process exit code (or `128 + signal` if terminated by a signal on Unix).
    pub status: i32,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// `true` iff the process exited with status `0`.
    pub fn success(&self) -> bool {
        self.status == 0
    }

    /// Standard output decoded as UTF-8, lossily.
    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Standard error decoded as UTF-8, lossily.
    pub fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Convenience constructor for tests and fakes: status `0`, the given
    /// stdout, and empty stderr.
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 0,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_spec_new_collects_args() {
        let spec = CommandSpec::new("docker", ["ps", "-a"]);
        assert_eq!(spec.program, "docker");
        assert_eq!(spec.args, vec!["ps".to_string(), "-a".to_string()]);
        assert!(spec.timeout.is_none());
    }

    #[test]
    fn command_spec_new_accepts_string_iter() {
        let owned = vec!["a".to_string(), "b".to_string()];
        let spec = CommandSpec::new(String::from("prog"), owned);
        assert_eq!(spec.args, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn command_spec_with_timeout_sets_field() {
        let spec = CommandSpec::new("x", Vec::<String>::new()).with_timeout(Duration::from_secs(5));
        assert_eq!(spec.timeout, Some(Duration::from_secs(5)));
    }

    #[test]
    fn command_output_ok_is_success() {
        let out = CommandOutput::ok("hello");
        assert!(out.success());
        assert_eq!(out.stdout_string(), "hello");
        assert!(out.stderr.is_empty());
        assert_eq!(out.stderr_string(), "");
    }

    #[test]
    fn command_output_nonzero_not_success() {
        let out = CommandOutput {
            status: 3,
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        };
        assert!(!out.success());
        assert_eq!(out.stdout_string(), "out");
        assert_eq!(out.stderr_string(), "err");
    }

    #[test]
    fn command_output_string_is_lossy() {
        let out = CommandOutput {
            status: 0,
            stdout: vec![0xff, 0xfe],
            stderr: Vec::new(),
        };
        // Invalid UTF-8 must not panic; it is replaced with U+FFFD.
        assert!(out.stdout_string().contains('\u{FFFD}'));
    }
}
