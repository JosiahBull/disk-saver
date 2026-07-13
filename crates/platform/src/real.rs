//! The production [`Platform`] implementation, backed by `std` and `rustix`.
//!
//! This module is the **only** place in the workspace allowed to branch on
//! `target_os`.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::{
    CommandOutput, CommandSpec, DirEntry, DiskUsage, FileKind, FileMeta, Notification, Platform,
};

/// Default hard timeout for [`Platform::run_command`] when the spec leaves it
/// unset.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// std/`rustix`-backed [`Platform`]. Zero-sized; construct with
/// [`RealPlatform::new`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RealPlatform;

impl RealPlatform {
    /// Create a new `RealPlatform`.
    pub fn new() -> Self {
        RealPlatform
    }
}

/// Map a std [`fs::FileType`] to our [`FileKind`], reporting symlinks as
/// [`FileKind::Symlink`] (i.e. assuming the type came from symlink metadata).
fn kind_from_file_type(ft: fs::FileType) -> FileKind {
    if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

/// Reject relative paths for destructive operations.
fn ensure_absolute(path: &Path) -> io::Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to operate on relative path: {}", path.display()),
        ))
    }
}

/// Escape a string for embedding inside an AppleScript double-quoted literal:
/// backslash and double-quote must be escaped.
#[cfg(target_os = "macos")]
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

/// Recursively sum apparent file sizes under `path`, staying on device `dev`
/// and never following symlinks. `md` is `path`'s already-fetched symlink
/// metadata.
fn size_on_device(path: &Path, md: &fs::Metadata, dev: u64) -> io::Result<u64> {
    let ft = md.file_type();
    if ft.is_symlink() {
        // Never follow symlinks.
        return Ok(0);
    }
    if md.dev() != dev {
        // Crossed a filesystem boundary; do not descend.
        return Ok(0);
    }
    if ft.is_file() {
        return Ok(md.len());
    }
    if !ft.is_dir() {
        return Ok(0);
    }

    let mut total: u64 = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child_path = entry.path();
        let child_md = fs::symlink_metadata(&child_path)?;
        total = total.saturating_add(size_on_device(&child_path, &child_md, dev)?);
    }
    Ok(total)
}

/// Recursively remove `path`, logging every removal before it happens and
/// never following symlinks (symlinked directories are unlinked, not descended).
fn remove_tree(path: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    let ft = md.file_type();

    if ft.is_symlink() {
        tracing::info!(path = %path.display(), "disk-saver: removing symlink");
        return fs::remove_file(path);
    }

    if ft.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            remove_tree(&entry.path())?;
        }
        tracing::info!(path = %path.display(), "disk-saver: removing directory");
        return fs::remove_dir(path);
    }

    tracing::info!(path = %path.display(), "disk-saver: removing file");
    fs::remove_file(path)
}

impl Platform for RealPlatform {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn home_dir(&self) -> PathBuf {
        // $HOME is the source of truth on both macOS and Linux.
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    }

    fn trash_dirs(&self) -> Vec<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            vec![self.home_dir().join(".Trash")]
        }
        #[cfg(not(target_os = "macos"))]
        {
            // XDG: $XDG_DATA_HOME/Trash, else ~/.local/share/Trash.
            let base = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| self.home_dir().join(".local").join("share"));
            vec![base.join("Trash")]
        }
    }

    fn user_cache_dir(&self) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            self.home_dir().join("Library").join("Caches")
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| self.home_dir().join(".cache"))
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMeta> {
        let md = fs::symlink_metadata(path)?;
        Ok(FileMeta {
            kind: kind_from_file_type(md.file_type()),
            len: md.len(),
            modified: md.modified()?,
        })
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            // `DirEntry::file_type` does not traverse symlinks.
            let ft = entry.file_type()?;
            out.push(DirEntry {
                path: entry.path(),
                file_name: entry.file_name().to_string_lossy().into_owned(),
                kind: kind_from_file_type(ft),
            });
        }
        Ok(out)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn dir_size(&self, path: &Path) -> io::Result<u64> {
        let md = fs::symlink_metadata(path)?;
        if md.file_type().is_symlink() {
            return Ok(0);
        }
        let dev = md.dev();
        size_on_device(path, &md, dev)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        ensure_absolute(path)?;
        tracing::info!(path = %path.display(), "disk-saver: removing file");
        fs::remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        ensure_absolute(path)?;
        remove_tree(path)
    }

    fn disk_usage(&self, path: &Path) -> io::Result<DiskUsage> {
        let s = rustix::fs::statvfs(path).map_err(io::Error::from)?;
        let frsize = s.f_frsize;
        Ok(DiskUsage {
            total: s.f_blocks.saturating_mul(frsize),
            available: s.f_bavail.saturating_mul(frsize),
        })
    }

    fn notify(&self, n: &Notification) -> io::Result<()> {
        let spec = notify_command(n);
        match spec {
            Some(spec) => match self.run_command(&spec) {
                Ok(out) if out.success() => Ok(()),
                Ok(out) => {
                    tracing::info!(
                        title = %n.title,
                        body = %n.body,
                        status = out.status,
                        "disk-saver: notifier exited non-zero; notification not shown"
                    );
                    Ok(())
                }
                Err(e) => {
                    tracing::info!(
                        title = %n.title,
                        body = %n.body,
                        error = %e,
                        "disk-saver: notifier unavailable; notification only logged"
                    );
                    Ok(())
                }
            },
            None => {
                tracing::info!(
                    title = %n.title,
                    body = %n.body,
                    "disk-saver: no notifier for this platform; notification only logged"
                );
                Ok(())
            }
        }
    }

    fn run_command(&self, cmd: &CommandSpec) -> io::Result<CommandOutput> {
        let timeout = cmd.timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT);

        let mut child = Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Drain both pipes on dedicated threads so a chatty child cannot
        // deadlock against a full pipe buffer while we poll for exit.
        let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf);
            buf
        });
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait()? {
                Some(status) => break status,
                None => {
                    if Instant::now() >= deadline {
                        // Hard timeout: kill, reap, and release the reader
                        // threads (the pipes close once the child is gone).
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = out_reader.join();
                        let _ = err_reader.join();
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "command '{}' exceeded timeout of {:?}",
                                cmd.program, timeout
                            ),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };

        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();

        Ok(CommandOutput {
            status: exit_code(&status),
            stdout,
            stderr,
        })
    }
}

/// Extract an integer status: the exit code, or `128 + signal` if the process
/// was terminated by a signal.
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(-1)
}

/// Build the notifier command for the current OS, or `None` if this OS has no
/// known notifier.
fn notify_command(n: &Notification) -> Option<CommandSpec> {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape_applescript(&n.body),
            escape_applescript(&n.title),
        );
        Some(CommandSpec::new("osascript", ["-e".to_string(), script]))
    }
    #[cfg(target_os = "linux")]
    {
        use crate::Urgency;
        let urgency = match n.urgency {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        };
        Some(CommandSpec::new(
            "notify-send",
            [
                "-u".to_string(),
                urgency.to_string(),
                n.title.clone(),
                n.body.clone(),
            ],
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = n;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn escape_applescript_escapes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("plain"), "plain");
        assert_eq!(escape_applescript(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_applescript(r"a\b"), r"a\\b");
        assert_eq!(escape_applescript(r#"\"#), r"\\");
    }

    #[test]
    fn now_is_after_epoch() {
        let p = RealPlatform::new();
        assert!(p.now() > SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn metadata_reports_kinds_without_following_symlinks() {
        let dir = tmp();
        let file_path = dir.path().join("a.txt");
        fs::write(&file_path, b"hello").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();

        let p = RealPlatform::new();

        let fm = p.metadata(&file_path).unwrap();
        assert_eq!(fm.kind, FileKind::File);
        assert_eq!(fm.len, 5);

        let dm = p.metadata(&sub).unwrap();
        assert_eq!(dm.kind, FileKind::Dir);

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&sub, &link).unwrap();
        let lm = p.metadata(&link).unwrap();
        assert_eq!(lm.kind, FileKind::Symlink);
    }

    #[test]
    fn read_dir_returns_absolute_paths_and_kinds() {
        let dir = tmp();
        fs::write(dir.path().join("f.txt"), b"x").unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("f.txt"), dir.path().join("l")).unwrap();

        let p = RealPlatform::new();
        let mut entries = p.read_dir(dir.path()).unwrap();
        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));

        assert_eq!(entries.len(), 3);
        for e in &entries {
            assert!(e.path.is_absolute());
        }
        let by_name: std::collections::HashMap<_, _> = entries
            .iter()
            .map(|e| (e.file_name.as_str(), e.kind))
            .collect();
        assert_eq!(by_name["f.txt"], FileKind::File);
        assert_eq!(by_name["d"], FileKind::Dir);
        assert_eq!(by_name["l"], FileKind::Symlink);
    }

    #[test]
    fn read_to_string_reads_contents() {
        let dir = tmp();
        let f = dir.path().join("c.txt");
        fs::write(&f, b"content").unwrap();
        let p = RealPlatform::new();
        assert_eq!(p.read_to_string(&f).unwrap(), "content");
    }

    #[test]
    fn dir_size_sums_files_and_ignores_symlinks() {
        let dir = tmp();
        fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("b"), vec![0u8; 50]).unwrap();

        // A symlink to a big external file must not be counted.
        let external = dir.path().join("external_big");
        fs::write(&external, vec![0u8; 10_000]).unwrap();
        let inner = dir.path().join("inner");
        fs::create_dir(&inner).unwrap();
        std::os::unix::fs::symlink(&external, inner.join("link")).unwrap();

        let p = RealPlatform::new();
        // Size the `inner` subtree only: it contains one symlink → 0 bytes.
        assert_eq!(p.dir_size(&inner).unwrap(), 0);

        // Whole tree: 100 + 50 + 10_000 (external file itself lives at top) but
        // the symlink under `inner` adds nothing.
        assert_eq!(p.dir_size(dir.path()).unwrap(), 100 + 50 + 10_000);
    }

    #[test]
    fn dir_size_on_symlink_top_is_zero() {
        let dir = tmp();
        let target = dir.path().join("t");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("f"), vec![0u8; 42]).unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let p = RealPlatform::new();
        assert_eq!(p.dir_size(&link).unwrap(), 0);
    }

    #[test]
    fn remove_file_refuses_relative_path() {
        let p = RealPlatform::new();
        let err = p.remove_file(Path::new("relative/x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn remove_dir_all_refuses_relative_path() {
        let p = RealPlatform::new();
        let err = p.remove_dir_all(Path::new("relative")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn remove_file_deletes_file() {
        let dir = tmp();
        let f = dir.path().join("gone.txt");
        fs::write(&f, b"x").unwrap();
        let p = RealPlatform::new();
        p.remove_file(&f).unwrap();
        assert!(!f.exists());
    }

    #[test]
    fn remove_dir_all_removes_tree_without_following_symlinks() {
        let dir = tmp();

        // External data that a symlink inside the tree points at.
        let external = dir.path().join("external");
        fs::create_dir(&external).unwrap();
        let external_file = external.join("keepme");
        fs::write(&external_file, b"precious").unwrap();

        // Tree to delete, containing a symlink to the external dir.
        let victim = dir.path().join("victim");
        fs::create_dir_all(victim.join("nested")).unwrap();
        fs::write(victim.join("nested").join("f"), b"data").unwrap();
        std::os::unix::fs::symlink(&external, victim.join("link_to_external")).unwrap();

        let p = RealPlatform::new();
        p.remove_dir_all(&victim).unwrap();

        assert!(!victim.exists());
        // The symlink target and its contents must survive.
        assert!(external.exists());
        assert!(external_file.exists());
        assert_eq!(fs::read_to_string(&external_file).unwrap(), "precious");
    }

    #[test]
    fn remove_dir_all_on_symlink_unlinks_only_the_link() {
        let dir = tmp();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("f"), b"x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let p = RealPlatform::new();
        p.remove_dir_all(&link).unwrap();

        assert!(!link.exists());
        assert!(target.exists());
        assert!(target.join("f").exists());
    }

    #[test]
    fn disk_usage_reports_sane_values() {
        let dir = tmp();
        let p = RealPlatform::new();
        let du = p.disk_usage(dir.path()).unwrap();
        assert!(du.total > 0);
        assert!(du.available > 0);
        assert!(du.available <= du.total);
    }

    #[test]
    fn run_command_captures_stdout_stderr_and_status() {
        let p = RealPlatform::new();
        let spec = CommandSpec::new("sh", ["-c", "printf hello; printf oops 1>&2; exit 3"]);
        let out = p.run_command(&spec).unwrap();
        assert_eq!(out.status, 3);
        assert!(!out.success());
        assert_eq!(out.stdout_string(), "hello");
        assert_eq!(out.stderr_string(), "oops");
    }

    #[test]
    fn run_command_success_case() {
        let p = RealPlatform::new();
        let out = p
            .run_command(&CommandSpec::new("sh", ["-c", "printf ok"]))
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_string(), "ok");
    }

    #[test]
    fn run_command_handles_large_output_without_deadlock() {
        let p = RealPlatform::new();
        // ~200 KiB of output would overflow a pipe buffer if not drained.
        let out = p
            .run_command(&CommandSpec::new(
                "sh",
                ["-c", "yes ABCDEFGH | head -n 25000"],
            ))
            .unwrap();
        assert!(out.success());
        assert!(out.stdout.len() > 100_000);
    }

    #[test]
    fn run_command_missing_binary_is_not_found() {
        let p = RealPlatform::new();
        let err = p
            .run_command(&CommandSpec::new(
                "disk-saver-nonexistent-binary-xyz",
                Vec::<String>::new(),
            ))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn run_command_enforces_hard_timeout() {
        let p = RealPlatform::new();
        let start = Instant::now();
        let err = p
            .run_command(&CommandSpec::new("sleep", ["5"]).with_timeout(Duration::from_millis(200)))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Must not have waited the full 5s.
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn home_dir_reads_env() {
        // Just assert it returns something on this dev machine; HOME is set.
        let p = RealPlatform::new();
        let _ = p.home_dir();
        // trash/cache derive from home and must be non-empty.
        assert!(!p.trash_dirs().is_empty());
        assert!(!p.user_cache_dir().as_os_str().is_empty());
    }

    #[test]
    #[ignore = "delivers a real desktop notification; run manually"]
    fn notify_is_best_effort_ok() {
        let p = RealPlatform::new();
        let n = Notification {
            title: "disk-saver test".to_string(),
            body: "hello \"world\"".to_string(),
            urgency: crate::Urgency::Low,
        };
        // Always Ok, whether or not a notifier is installed.
        p.notify(&n).unwrap();
    }
}
