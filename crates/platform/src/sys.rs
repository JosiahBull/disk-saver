//! OS-agnostic building blocks shared by every real [`Platform`] implementation.
//!
//! Filesystem walking, sizing, safe deletion, `statvfs`, and subprocess handling
//! are byte-for-byte identical on macOS and Linux, so they live here as free
//! functions. The per-OS platform types ([`crate::MacOsPlatform`] /
//! [`crate::LinuxPlatform`]) delegate their shared trait methods straight to
//! these, and implement only the genuinely OS-specific surface (well-known
//! directories and notification delivery) themselves — so no method body in
//! this crate branches on `target_os`.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::{CommandOutput, CommandSpec, DirEntry, DiskUsage, FileKind, FileMeta, Notification};

/// Default hard timeout for [`run_command`] when the spec leaves it unset.
pub(crate) const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Current wall-clock time.
pub(crate) fn now() -> SystemTime {
    SystemTime::now()
}

/// The user's home directory (`$HOME`) — the source of truth on both macOS and
/// Linux.
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
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

/// Symlink metadata for `path`. Never follows symlinks.
pub(crate) fn metadata(path: &Path) -> io::Result<FileMeta> {
    let md = fs::symlink_metadata(path)?;
    Ok(FileMeta {
        kind: kind_from_file_type(md.file_type()),
        len: md.len(),
        modified: md.modified()?,
        dev: md.dev(),
    })
}

/// Directory listing with absolute paths; entry kinds come from symlink
/// metadata (symlinks are not followed).
pub(crate) fn read_dir(path: &Path) -> io::Result<Vec<DirEntry>> {
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

/// Read a file's entire contents into a UTF-8 string.
pub(crate) fn read_to_string(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
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

/// Recursive apparent size of `path`. Stays on the starting device and never
/// follows symlinks.
pub(crate) fn dir_size(path: &Path) -> io::Result<u64> {
    let md = fs::symlink_metadata(path)?;
    if md.file_type().is_symlink() {
        return Ok(0);
    }
    let dev = md.dev();
    size_on_device(path, &md, dev)
}

/// Recursively remove `path`, logging every removal before it happens, never
/// following symlinks (symlinked directories are unlinked, not descended), and
/// never crossing a filesystem boundary: a child directory on a *different*
/// device (a mount point) is left completely untouched. `dev` is the device of
/// the deletion root — descending into a mount and unlinking its contents would
/// be catastrophic data loss (ARCHITECTURE.md §11.8), so we skip it. Leaving a
/// cross-device child in place makes the parent `remove_dir` fail with `ENOTEMPTY`,
/// which the caller reports as a failure — the safe outcome.
fn remove_tree(path: &Path, dev: u64) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    let ft = md.file_type();

    if ft.is_symlink() {
        tracing::info!(path = %path.display(), "disk-saver: removing symlink");
        return fs::remove_file(path);
    }

    if ft.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            let child_md = fs::symlink_metadata(&child)?;
            // A real (non-symlink) directory on another device is a mount point:
            // never descend into or unlink it.
            if child_md.file_type().is_dir() && child_md.dev() != dev {
                tracing::warn!(path = %child.display(),
                    "disk-saver: refusing to remove across a filesystem boundary; leaving in place");
                continue;
            }
            remove_tree(&child, dev)?;
        }
        tracing::info!(path = %path.display(), "disk-saver: removing directory");
        return fs::remove_dir(path);
    }

    tracing::info!(path = %path.display(), "disk-saver: removing file");
    fs::remove_file(path)
}

/// Remove a single file (or unlink a single symlink). Refuses relative paths.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    ensure_absolute(path)?;
    tracing::info!(path = %path.display(), "disk-saver: removing file");
    fs::remove_file(path)
}

/// Recursively remove a directory tree. Refuses relative paths; never follows
/// symlinks; never crosses a filesystem boundary (a mounted volume nested inside
/// the tree is left intact, §11.8).
pub(crate) fn remove_dir_all(path: &Path) -> io::Result<()> {
    ensure_absolute(path)?;
    let dev = fs::symlink_metadata(path)?.dev();
    remove_tree(path, dev)
}

/// Free/total space of the filesystem containing `path`, via `statvfs`.
pub(crate) fn disk_usage(path: &Path) -> io::Result<DiskUsage> {
    let s = rustix::fs::statvfs(path).map_err(io::Error::from)?;
    let frsize = s.f_frsize;
    Ok(DiskUsage {
        total: s.f_blocks.saturating_mul(frsize),
        available: s.f_bavail.saturating_mul(frsize),
    })
}

/// Run a subprocess to completion with a hard timeout (kill on deadline).
pub(crate) fn run_command(cmd: &CommandSpec) -> io::Result<CommandOutput> {
    let timeout = cmd.timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT);

    let mut child = Command::new(&cmd.program)
        .args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Drain both pipes on dedicated threads so a chatty child cannot deadlock
    // against a full pipe buffer while we poll for exit.
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
                    // Hard timeout: kill, reap, and release the reader threads
                    // (the pipes close once the child is gone).
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

/// Extract an integer status: the exit code, or `128 + signal` if the process
/// was terminated by a signal.
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(-1)
}

/// Best-effort notification delivery shared by both OSes: run the OS-specific
/// notifier command if one was built, otherwise just log. Never returns an
/// error — notifications must never fail a run (ARCHITECTURE.md §10, §15.3).
pub(crate) fn notify(n: &Notification, spec: Option<CommandSpec>) -> io::Result<()> {
    match spec {
        Some(spec) => match run_command(&spec) {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => {
                tracing::info!(
                    title = %n.title, body = %n.body, status = out.status,
                    "disk-saver: notifier exited non-zero; notification not shown"
                );
                Ok(())
            }
            Err(e) => {
                tracing::info!(
                    title = %n.title, body = %n.body, error = %e,
                    "disk-saver: notifier unavailable; notification only logged"
                );
                Ok(())
            }
        },
        None => {
            tracing::info!(
                title = %n.title, body = %n.body,
                "disk-saver: no notifier for this platform; notification only logged"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    #[test]
    fn now_is_after_epoch() {
        assert!(now() > SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn home_dir_reads_env() {
        // HOME is set on the dev machine and CI.
        assert!(!home_dir().as_os_str().is_empty());
    }

    #[test]
    fn metadata_reports_kinds_without_following_symlinks() {
        let dir = tmp();
        let file_path = dir.path().join("a.txt");
        fs::write(&file_path, b"hello").unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();

        let fm = metadata(&file_path).unwrap();
        assert_eq!(fm.kind, FileKind::File);
        assert_eq!(fm.len, 5);

        let dm = metadata(&sub).unwrap();
        assert_eq!(dm.kind, FileKind::Dir);

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&sub, &link).unwrap();
        let lm = metadata(&link).unwrap();
        assert_eq!(lm.kind, FileKind::Symlink);
    }

    #[test]
    fn read_dir_returns_absolute_paths_and_kinds() {
        let dir = tmp();
        fs::write(dir.path().join("f.txt"), b"x").unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("f.txt"), dir.path().join("l")).unwrap();

        let mut entries = read_dir(dir.path()).unwrap();
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
        assert_eq!(read_to_string(&f).unwrap(), "content");
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

        // Size the `inner` subtree only: it contains one symlink → 0 bytes.
        assert_eq!(dir_size(&inner).unwrap(), 0);

        // Whole tree: 100 + 50 + 10_000; the symlink under `inner` adds nothing.
        assert_eq!(dir_size(dir.path()).unwrap(), 100 + 50 + 10_000);
    }

    #[test]
    fn dir_size_on_symlink_top_is_zero() {
        let dir = tmp();
        let target = dir.path().join("t");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("f"), vec![0u8; 42]).unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(dir_size(&link).unwrap(), 0);
    }

    #[test]
    fn remove_file_refuses_relative_path() {
        let err = remove_file(Path::new("relative/x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn remove_dir_all_refuses_relative_path() {
        let err = remove_dir_all(Path::new("relative")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn remove_file_deletes_file() {
        let dir = tmp();
        let f = dir.path().join("gone.txt");
        fs::write(&f, b"x").unwrap();
        remove_file(&f).unwrap();
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

        remove_dir_all(&victim).unwrap();

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

        remove_dir_all(&link).unwrap();

        assert!(!link.exists());
        assert!(target.exists());
        assert!(target.join("f").exists());
    }

    #[test]
    fn disk_usage_reports_sane_values() {
        let dir = tmp();
        let du = disk_usage(dir.path()).unwrap();
        assert!(du.total > 0);
        assert!(du.available > 0);
        assert!(du.available <= du.total);
    }

    #[test]
    fn run_command_captures_stdout_stderr_and_status() {
        let spec = CommandSpec::new("sh", ["-c", "printf hello; printf oops 1>&2; exit 3"]);
        let out = run_command(&spec).unwrap();
        assert_eq!(out.status, 3);
        assert!(!out.success());
        assert_eq!(out.stdout_string(), "hello");
        assert_eq!(out.stderr_string(), "oops");
    }

    #[test]
    fn run_command_success_case() {
        let out = run_command(&CommandSpec::new("sh", ["-c", "printf ok"])).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_string(), "ok");
    }

    #[test]
    fn run_command_handles_large_output_without_deadlock() {
        // ~200 KiB of output would overflow a pipe buffer if not drained.
        let out = run_command(&CommandSpec::new(
            "sh",
            ["-c", "yes ABCDEFGH | head -n 25000"],
        ))
        .unwrap();
        assert!(out.success());
        assert!(out.stdout.len() > 100_000);
    }

    #[test]
    fn run_command_missing_binary_is_not_found() {
        let err = run_command(&CommandSpec::new(
            "disk-saver-nonexistent-binary-xyz",
            Vec::<String>::new(),
        ))
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn run_command_enforces_hard_timeout() {
        let start = Instant::now();
        let err =
            run_command(&CommandSpec::new("sleep", ["5"]).with_timeout(Duration::from_millis(200)))
                .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Must not have waited the full 5s.
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
