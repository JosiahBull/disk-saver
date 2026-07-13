//! Platform abstraction for `disk-saver`.
//!
//! Adapters have **zero** OS knowledge: no `#[cfg(target_os)]`, no `std::fs`,
//! no `std::process`, no `SystemTime::now()`. Everything flows through the
//! [`Platform`] trait, so every adapter is integration-testable against an
//! in-memory fake.
//!
//! Two implementations ship here:
//!
//! * [`RealPlatform`] — std/`rustix`-backed. The *only* place in the workspace
//!   with `#[cfg(target_os = …)]` branches (well-known dirs, `statvfs`,
//!   notification delivery).
//! * [`FakePlatform`] — an in-memory implementation behind the `test-util`
//!   feature, with a fluent builder, recorders, an advanceable clock, scripted
//!   commands, and a no-symlink-follow in-memory filesystem.
//!
//! Deletion goes through the [`Platform`] trait rather than `std::fs` because
//! it is a safety chokepoint: [`RealPlatform`] refuses relative paths, never
//! traverses symlinks, and logs every removal before performing it.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod real;
mod types;

pub use real::RealPlatform;
pub use types::{
    CommandOutput, CommandSpec, DirEntry, DiskUsage, FileKind, FileMeta, Notification, Urgency,
};

#[cfg(feature = "test-util")]
mod fake;
#[cfg(feature = "test-util")]
pub use fake::FakePlatform;

/// The entire surface adapters and the engine use to touch the outside world:
/// the clock, well-known directories, the filesystem (never following
/// symlinks), disk statistics, desktop notifications, and subprocesses.
///
/// Implementations must be `Send + Sync` so the engine can hold one behind a
/// shared reference.
pub trait Platform: Send + Sync {
    /// Current wall-clock time. In tests this is a controllable fake clock, so
    /// nothing else in the workspace should call `SystemTime::now()`.
    fn now(&self) -> SystemTime;

    /// The user's home directory (`$HOME`).
    fn home_dir(&self) -> PathBuf;

    /// Trash directories to sweep. macOS: `[~/.Trash]`; Linux:
    /// `[~/.local/share/Trash]` (honoring `$XDG_DATA_HOME`).
    fn trash_dirs(&self) -> Vec<PathBuf>;

    /// The user cache directory. macOS: `~/Library/Caches`; Linux:
    /// `$XDG_CACHE_HOME` or `~/.cache`.
    fn user_cache_dir(&self) -> PathBuf;

    /// Symlink metadata for `path`. **Never** follows symlinks: a symlink is
    /// reported with [`FileKind::Symlink`].
    fn metadata(&self, path: &Path) -> std::io::Result<FileMeta>;

    /// Directory listing with absolute paths. Entry kinds come from symlink
    /// metadata (symlinks are not followed).
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>>;

    /// Read a file's entire contents into a UTF-8 string.
    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;

    /// Recursive *apparent* size of `path` in bytes. Stays on the starting
    /// device and never follows symlinks.
    fn dir_size(&self, path: &Path) -> std::io::Result<u64>;

    /// Remove a single file (or unlink a single symlink).
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;

    /// Recursively remove a directory tree. Must **not** follow symlinks:
    /// symlinked entries are unlinked, only real directories are descended.
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()>;

    /// Free/total space of the filesystem containing `path`.
    fn disk_usage(&self, path: &Path) -> std::io::Result<DiskUsage>;

    /// Deliver a desktop notification. Best-effort: if the notifier is
    /// unavailable the text is logged and `Ok(())` is returned — notifications
    /// never fail a run.
    fn notify(&self, n: &Notification) -> std::io::Result<()>;

    /// Run a subprocess to completion with a hard timeout (kill on deadline).
    /// A `None` timeout on the spec means a 60s default.
    fn run_command(&self, cmd: &CommandSpec) -> std::io::Result<CommandOutput>;
}
