//! The evidence checks: the git commands that say what removing a checkout
//! would lose.

use std::io::ErrorKind;
use std::path::Path;

use disk_saver_core::{CommandOutput, CommandSpec, FileKind, Platform};
use disk_saver_scan::{ScanOptions, find_dirs};

use crate::{CHECK_TIMEOUT, Evidence, Head, Link, Lost, Worktree, is_object_id, read_head, tilde};

/// Where a forgotten checkout's check points `GIT_INDEX_FILE`: a file that
/// does not exist, which git reads as an empty index and never writes, so
/// every file in the checkout reads as untracked.
pub(crate) const EMPTY_INDEX: &str = "/nonexistent/disk-saver-empty-index";

/// Variables a git hook exports that would point git at some other
/// repository than the checkout's. The pre-commit hook runs this crate's
/// tests, and a hook could run disk-saver too.
pub(crate) const HOOK_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// What the evidence check found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Checked(Evidence),
    /// HEAD is detached on commits nothing else holds.
    Unreachable,
}

/// Why a worktree with [`Status::Unreachable`] is kept.
pub(crate) const UNREACHABLE: &str = "its detached HEAD is on commits no branch or tag holds";

/// Gathers one checkout's evidence: [`check`], or canned evidence in the
/// policy tests, so they need not script every git call.
pub(crate) type Check = fn(&dyn Platform, &Worktree) -> Status;

/// The object-store check for a forgotten checkout, the status check for a
/// listed one. Any failing step, or an answer that does not line up with what
/// was asked, makes the evidence unknown: a short list would read as less
/// unsaved work, the one direction this must never err in.
pub(crate) fn check(p: &dyn Platform, wt: &Worktree) -> Status {
    let found = if wt.forgotten {
        forgotten(p, &wt.dir, &wt.link).map(Status::Checked)
    } else {
        listed(p, wt)
    };
    found.unwrap_or_else(|reason| Status::Checked(Evidence::Unknown { reason }))
}

/// The evidence for `wt` and every checkout embedded in it, as one report. An
/// embedded checkout's paths are prefixed with where it sits; its detached
/// HEAD on commits nothing holds makes the whole report unknown rather than
/// keeping `wt`, since a vendoring tool pins such commits itself.
pub(crate) fn evidence(
    p: &dyn Platform,
    wt: &Worktree,
    embedded: &[Worktree],
    check: Check,
) -> Status {
    let (mut checked, mut lost) = match check(p, wt) {
        Status::Checked(Evidence::Clean { checked }) => (checked, Vec::new()),
        Status::Checked(Evidence::Unsaved { checked, lost }) => (checked, lost),
        other => return other,
    };
    for inner in embedded {
        let at = inner.dir.strip_prefix(&wt.dir).unwrap_or(&inner.dir);
        let at = at.to_string_lossy();
        let (n, more) = match check(p, inner) {
            Status::Checked(Evidence::Clean { checked }) => (checked, Vec::new()),
            Status::Checked(Evidence::Unsaved { checked, lost }) => (checked, lost),
            Status::Checked(Evidence::Unknown { reason }) => {
                let reason = format!("{at}: {reason}");
                return Status::Checked(Evidence::Unknown { reason });
            }
            Status::Unreachable => {
                let reason = format!("{at}: {UNREACHABLE}");
                return Status::Checked(Evidence::Unknown { reason });
            }
        };
        checked += n;
        lost.extend(more.into_iter().map(|l| match l {
            Lost::File { path, blob } => Lost::File {
                path: format!("{at}/{path}"),
                blob,
            },
            Lost::Repo { path } => Lost::Repo {
                path: format!("{at}/{path}"),
            },
        }));
    }
    lost.sort();
    Status::Checked(if lost.is_empty() {
        Evidence::Clean { checked }
    } else {
        Evidence::Unsaved { checked, lost }
    })
}

/// Checkouts of other repositories inside `wt`, such as an import a vendoring
/// tool keeps there. They go with it. Only ones their repository still lists
/// count; one of `wt`'s own repository makes [`protected`] keep `wt`.
pub(crate) fn embedded(p: &dyn Platform, wt: &Worktree) -> Vec<Worktree> {
    let roots = std::slice::from_ref(&wt.dir);
    find_dirs(p, roots, &ScanOptions::default(), &|p, entry| {
        Worktree::at(p, &entry.path).is_some_and(|inner| !inner.forgotten)
    })
    .into_iter()
    .filter_map(|dir| Worktree::at(p, &dir))
    .filter(|inner| inner.link.common != wt.link.common)
    .collect()
}

/// `git <args>` in `dir`, reading the repository `dir` belongs to. It never
/// takes an optional lock, so `git status` never writes the index it
/// refreshes (a rewritten index would read as the checkout having been used),
/// and it ignores any fsmonitor daemon.
fn git(dir: &Path, args: &[&str]) -> CommandSpec {
    let mut all = vec!["--no-optional-locks", "-c", "core.fsmonitor=false"];
    all.extend_from_slice(args);
    HOOK_VARIABLES
        .iter()
        .fold(CommandSpec::new("git", all), |spec, var| {
            spec.env_remove(*var)
        })
        .in_dir(dir)
        .with_timeout(CHECK_TIMEOUT)
}

/// Run `spec` and return its stdout, or why it failed. `what` names the step.
fn run(p: &dyn Platform, what: &str, spec: &CommandSpec) -> Result<Vec<u8>, String> {
    let out = p.run_command(spec).map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!("{} is not installed", spec.program),
        ErrorKind::TimedOut => format!("{what} timed out"),
        _ => format!("couldn't run {what}: {e}"),
    })?;
    if out.success() {
        Ok(out.stdout)
    } else {
        Err(failed(what, &out))
    }
}

/// The step and the last line it wrote to stderr (git's `fatal:` line,
/// usually).
fn failed(what: &str, out: &CommandOutput) -> String {
    let stderr = out.stderr_string();
    match stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(line) => format!(
            "{what} failed: {}",
            line.chars().take(200).collect::<String>()
        ),
        None => format!("{what} failed with status {}", out.status),
    }
}

/// A NUL-terminated list of paths, as `-z` prints it. A path that is not UTF-8
/// or holds a newline cannot travel through `--stdin-paths`, so it fails the
/// check rather than going unhashed.
fn nul_separated(bytes: &[u8]) -> Result<Vec<String>, String> {
    bytes
        .split(|b| *b == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let path =
                std::str::from_utf8(record).map_err(|_| "a file name is not UTF-8".to_string())?;
            if path.contains('\n') {
                return Err("a file name contains a newline".to_string());
            }
            Ok(path.to_string())
        })
        .collect()
}

/// The object id of each of `paths`: their raw bytes with `raw`, otherwise
/// as the repository's filters would store them.
fn hash_paths(
    p: &dyn Platform,
    spec: impl Fn(&[&str]) -> CommandSpec,
    paths: &[String],
    raw: bool,
) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let args: &[&str] = if raw {
        &["hash-object", "--no-filters", "--stdin-paths"]
    } else {
        &["hash-object", "--stdin-paths"]
    };
    let out = run(
        p,
        "git hash-object",
        &spec(args).with_stdin(paths.join("\n") + "\n"),
    )?;
    let ids: Vec<String> = String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_string)
        .collect();
    if ids.len() != paths.len() || !ids.iter().all(|id| is_object_id(id)) {
        return Err("git hash-object returned a different number of ids".to_string());
    }
    Ok(ids)
}

/// Whether each of `ids` is in the object store.
fn in_store(
    p: &dyn Platform,
    spec: impl Fn(&[&str]) -> CommandSpec,
    ids: &[String],
) -> Result<Vec<bool>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let args: &[&str] = &["cat-file", "--batch-check=%(objectname)"];
    let out = run(
        p,
        "git cat-file",
        &spec(args).with_stdin(ids.join("\n") + "\n"),
    )?;
    let found: Vec<bool> = String::from_utf8_lossy(&out)
        .lines()
        .zip(ids)
        .map(|(line, id)| match line.strip_prefix(id.as_str()) {
            Some("") => Ok(true),
            Some(" missing") => Ok(false),
            _ => Err(()),
        })
        .collect::<Result<_, _>>()
        .map_err(|()| "git cat-file answered for a different object".to_string())?;
    if found.len() != ids.len() {
        return Err("git cat-file returned a different number of answers".to_string());
    }
    Ok(found)
}

/// The object-store check for `dir`, a forgotten checkout (see the crate
/// docs for its steps).
fn forgotten(p: &dyn Platform, dir: &Path, link: &Link) -> Result<Evidence, String> {
    let home = p.home_dir();
    match p.metadata(&link.common) {
        Ok(m) if m.kind == FileKind::Dir => {}
        Ok(_) => return Err(format!("{} is not a directory", tilde(&home, &link.common))),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(format!(
                "its repository {} is gone",
                tilde(&home, link.repo())
            ));
        }
        Err(e) => return Err(format!("couldn't read {}: {e}", tilde(&home, &link.common))),
    }
    if !p
        .metadata(&link.common.join("objects"))
        .is_ok_and(|m| m.kind == FileKind::Dir)
    {
        return Err(format!(
            "{} is not a git repository",
            tilde(&home, &link.common)
        ));
    }

    let common = link.common.to_string_lossy();
    let dir_text = dir.to_string_lossy();
    let spec = |args: &[&str]| {
        git(dir, args)
            .env("GIT_DIR", common.as_ref())
            .env("GIT_WORK_TREE", dir_text.as_ref())
            .env("GIT_INDEX_FILE", EMPTY_INDEX)
    };

    let listing = run(
        p,
        "git ls-files",
        &spec(&["ls-files", "-z", "--others", "--exclude-standard"]),
    )?;
    let mut files = Vec::new();
    let mut lost = Vec::new();
    for path in nul_separated(&listing)? {
        match p.metadata(&dir.join(&path)) {
            Ok(m) if m.kind == FileKind::File && m.len > 0 => files.push(path),
            Ok(m) if m.kind == FileKind::Dir => lost.push(Lost::Repo { path }),
            // Empty files, symlinks, sockets, and files gone since the listing.
            _ => {}
        }
    }

    let raw = hash_paths(p, spec, &files, true)?;
    let misses: Vec<String> = files
        .iter()
        .zip(in_store(p, spec, &raw)?)
        .filter(|(_, found)| !found)
        .map(|(path, _)| path.clone())
        .collect();

    // Split the misses by filter: `check-attr -z` answers `path NUL filter
    // NUL value NUL` for each.
    let (mut lfs, mut other) = (Vec::new(), Vec::new());
    if !misses.is_empty() {
        let stdin: String = misses.iter().map(|m| format!("{m}\0")).collect();
        let out = run(
            p,
            "git check-attr",
            &spec(&["check-attr", "-z", "--stdin", "filter"]).with_stdin(stdin),
        )?;
        let fields: Vec<&[u8]> = out.split(|b| *b == 0).collect();
        let answers = fields.as_chunks::<3>().0;
        if answers.len() != misses.len()
            || answers
                .iter()
                .zip(&misses)
                .any(|(answer, path)| answer[0] != path.as_bytes())
        {
            return Err("git check-attr answered for different paths".to_string());
        }
        for (answer, path) in answers.iter().zip(misses) {
            if answer[2] == b"lfs" {
                lfs.push(path);
            } else {
                other.push(path);
            }
        }
    }

    let filtered = hash_paths(p, spec, &other, false)?;
    let pointers = lfs_pointer_ids(p, dir, spec, &lfs)?;
    let paths: Vec<String> = other.into_iter().chain(lfs).collect();
    let ids: Vec<String> = filtered.into_iter().chain(pointers).collect();
    for ((path, blob), found) in paths
        .into_iter()
        .zip(ids.clone())
        .zip(in_store(p, spec, &ids)?)
    {
        if !found {
            lost.push(Lost::File { path, blob });
        }
    }

    let checked = files.len() as u64;
    if lost.is_empty() {
        return Ok(Evidence::Clean { checked });
    }
    lost.sort();
    Ok(Evidence::Unsaved { checked, lost })
}

/// The id of the LFS pointer git would store for each of `paths`: the pointer
/// text for the file's SHA-256 and size, hashed as a blob. The digests come
/// from `sha256sum`, or `shasum -a 256` where that is all there is (macOS).
fn lfs_pointer_ids(
    p: &dyn Platform,
    dir: &Path,
    spec: impl Fn(&[&str]) -> CommandSpec,
    paths: &[String],
) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let tool = |program: &str, flags: &[&str]| {
        let mut args: Vec<&str> = flags.to_vec();
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
        CommandSpec::new(program, args)
            .in_dir(dir)
            .with_timeout(CHECK_TIMEOUT)
    };
    let out = match p.run_command(&tool("sha256sum", &[])) {
        Err(e) if e.kind() == ErrorKind::NotFound => p.run_command(&tool("shasum", &["-a", "256"])),
        other => other,
    }
    .map_err(|e| format!("couldn't hash an LFS file: {e}"))?;
    if !out.success() {
        return Err(failed("hashing an LFS file", &out));
    }
    let digests: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        // GNU tools prefix a line with `\` when the name needed escaping.
        .map(|line| line.trim_start_matches('\\'))
        .filter_map(|line| line.get(..64))
        .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .collect();
    if digests.len() != paths.len() {
        return Err("hashing an LFS file returned a different number of digests".to_string());
    }

    let mut ids = Vec::with_capacity(paths.len());
    for (path, digest) in paths.iter().zip(digests) {
        let size = p
            .metadata(&dir.join(path))
            .map_err(|e| format!("couldn't read {path}: {e}"))?
            .len;
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{digest}\nsize {size}\n"
        );
        let out = run(
            p,
            "git hash-object",
            &spec(&["hash-object", "--stdin"]).with_stdin(pointer),
        )?;
        let id = String::from_utf8_lossy(&out).trim().to_string();
        if !is_object_id(&id) {
            return Err("git hash-object returned something other than an id".to_string());
        }
        ids.push(id);
    }
    Ok(ids)
}

/// The status check for a listed worktree: every path `git status` reports —
/// staged, unstaged, and untracked files the repository does not ignore —
/// fingerprinted so `execute` can see one that changed after it was proposed:
/// a file by its raw blob id, a symlink by its target's, a deleted or special
/// file by an all-zero id. Whether that content is in the object store does not
/// matter: a blob only the index or some other branch holds is no copy of this
/// checkout's work. A detached HEAD on commits no branch, remote-tracking
/// branch or tag holds is [`Status::Unreachable`].
fn listed(p: &dyn Platform, wt: &Worktree) -> Result<Status, String> {
    let dir = &wt.dir;
    let spec = |args: &[&str]| git(dir, args);
    match read_head(p, &wt.link.admin) {
        Some(Head::Branch(_)) => {}
        Some(Head::Detached) => {
            let holders = run(
                p,
                "git for-each-ref",
                &spec(&[
                    "for-each-ref",
                    "--count=1",
                    "--contains=HEAD",
                    "--format=%(refname)",
                    "refs/heads",
                    "refs/remotes",
                    "refs/tags",
                ]),
            )?;
            if holders.iter().all(u8::is_ascii_whitespace) {
                return Ok(Status::Unreachable);
            }
        }
        None => return Err("couldn't read what it has checked out".to_string()),
    }

    // `--no-renames` keeps one path per record, after the two status letters
    // and a space.
    let status = run(
        p,
        "git status",
        &spec(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
        ]),
    )?;
    let paths: Vec<String> = nul_separated(&status)?
        .into_iter()
        .map(|record| match record.get(..3) {
            Some(prefix) if prefix.ends_with(' ') => Ok(record.get(3..).unwrap_or("").to_string()),
            _ => Err("git status printed a record it does not document".to_string()),
        })
        .collect::<Result<_, _>>()?;

    let mut lost = Vec::new();
    let mut files = Vec::new();
    for path in &paths {
        let full = dir.join(path);
        match p.metadata(&full) {
            Ok(m) if m.kind == FileKind::File => files.push(path.clone()),
            Ok(m) if m.kind == FileKind::Dir => lost.push(Lost::Repo { path: path.clone() }),
            Ok(m) if m.kind == FileKind::Symlink => {
                let target = p
                    .read_link(&full)
                    .map_err(|e| format!("couldn't read the link {path}: {e}"))?;
                let out = run(
                    p,
                    "git hash-object",
                    &spec(&["hash-object", "--stdin"])
                        .with_stdin(target.to_string_lossy().into_owned()),
                )?;
                let blob = String::from_utf8_lossy(&out).trim().to_string();
                if !is_object_id(&blob) {
                    return Err("git hash-object returned something other than an id".to_string());
                }
                lost.push(Lost::File {
                    path: path.clone(),
                    blob,
                });
            }
            // Deleted, or a socket or fifo: nothing to hash.
            _ => lost.push(Lost::File {
                path: path.clone(),
                blob: "0".repeat(40),
            }),
        }
    }
    let ids = hash_paths(p, spec, &files, true)?;
    lost.extend(
        files
            .into_iter()
            .zip(ids)
            .map(|(path, blob)| Lost::File { path, blob }),
    );
    lost.sort();
    lost.dedup();

    let checked = paths.len() as u64;
    Ok(Status::Checked(if lost.is_empty() {
        Evidence::Clean { checked }
    } else {
        Evidence::Unsaved { checked, lost }
    }))
}
