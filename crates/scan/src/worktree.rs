//! Linked git worktrees, read without running git.
//!
//! A linked worktree's `.git` is a *file* — `gitdir: <repo>/.git/worktrees/<name>`
//! — and that admin directory holds the worktree's `HEAD`, `index` and reflog,
//! plus a `locked` file while `git worktree lock` holds it. Coding agents keep
//! their worktrees in dot-directories the walker never enters
//! (`<repo>/.claude/worktrees`, `~/.codex/worktrees`), so the adapters that
//! report them find them by location and read them here.
//!
//! Nothing in this module writes or deletes.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use disk_saver_platform::{FileKind, Platform};

use crate::bump;

/// Why a worktree adapter skips anything it is ever asked to execute: they only
/// report, so nothing should reach `execute`, and nothing that does is removed.
pub const REPORT_ONLY: &str = "report-only: disk-saver never removes an agent's worktrees";

/// What a worktree has checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// A branch, with `refs/heads/` stripped.
    Branch(String),
    /// A detached commit.
    Detached,
    /// No readable `.git` or `HEAD`: not (or no longer) a git worktree.
    Unknown,
}

/// One worktree checkout, as far as the filesystem can tell.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// The checkout directory.
    pub path: PathBuf,
    /// The admin directory `.git` points at, if it could be read.
    pub gitdir: Option<PathBuf>,
    /// What is checked out.
    pub head: Head,
    /// Whether `git worktree lock` holds it. Claude Code locks the worktree of
    /// an agent that is still running.
    pub locked: bool,
    /// Apparent size from [`Platform::dir_size`] (`0` if it could not be
    /// measured).
    pub size: u64,
    /// Approximate last activity (see [`inspect`]).
    pub last_active: SystemTime,
}

impl Worktree {
    /// `<path> [<branch>]`, noting a detached HEAD, a lock, or a directory git
    /// does not recognise as a worktree.
    pub fn label(&self) -> String {
        let head = match &self.head {
            Head::Branch(branch) => branch.as_str(),
            Head::Detached => "detached",
            Head::Unknown => "not a git worktree",
        };
        let lock = if self.locked { ", locked" } else { "" };
        format!("{} [{head}{lock}]", self.path.display())
    }
}

/// Read the worktree checked out at `path`.
///
/// `last_active` is the newest of the checkout directory's own mtime, the
/// mtimes of its top-level entries, and the admin `HEAD` and `index` — so
/// staging, committing, switching branch, or adding a top-level file each move
/// it. The reflog is left out: `git gc` rewrites every worktree's `logs/HEAD`
/// when it expires entries, so it moves on maintenance, not use. It is
/// approximate on purpose: an edit deep in the tree only shows once git next
/// refreshes the index, but the checkout is about to be sized, and a second
/// whole-tree walk just to date it would double the cost.
pub fn inspect(p: &dyn Platform, path: &Path) -> Worktree {
    let gitdir = gitdir(p, path);
    let head = gitdir.as_deref().map_or(Head::Unknown, |g| head(p, g));
    let locked = gitdir
        .as_deref()
        .is_some_and(|g| p.metadata(&g.join("locked")).is_ok());
    Worktree {
        size: p.dir_size(path).unwrap_or(0),
        last_active: last_active(p, path, gitdir.as_deref()),
        path: path.to_path_buf(),
        gitdir,
        head,
        locked,
    }
}

/// The admin directory `path/.git` points at.
///
/// A `.git` *directory* is its own gitdir (a main checkout rather than a linked
/// worktree). A relative `gitdir:` line, which git writes under
/// `worktree.useRelativePaths`, resolves against the checkout — lexically: the
/// directories it climbs out of are the checkout's own ancestors, which the
/// adapters reach by listing rather than by following links, so the result is
/// what the OS would resolve. A symlinked `.git` is not followed.
fn gitdir(p: &dyn Platform, path: &Path) -> Option<PathBuf> {
    let dot_git = path.join(".git");
    match p.metadata(&dot_git).ok()?.kind {
        FileKind::Dir => Some(dot_git),
        FileKind::File => {
            let text = p.read_to_string(&dot_git).ok()?;
            let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
            if target.is_empty() {
                return None;
            }
            let target = Path::new(target);
            Some(if target.is_absolute() {
                target.to_path_buf()
            } else {
                lexical(&path.join(target))
            })
        }
        FileKind::Symlink | FileKind::Other => None,
    }
}

/// `path` with `.` dropped and each `..` popping the component before it.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// What `gitdir/HEAD` names.
fn head(p: &dyn Platform, gitdir: &Path) -> Head {
    let Ok(text) = p.read_to_string(&gitdir.join("HEAD")) else {
        return Head::Unknown;
    };
    match text.trim().strip_prefix("ref:") {
        Some(reference) => {
            let reference = reference.trim();
            Head::Branch(
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_owned(),
            )
        }
        None => Head::Detached,
    }
}

/// See [`inspect`].
fn last_active(p: &dyn Platform, path: &Path, gitdir: Option<&Path>) -> SystemTime {
    let mut newest = p.metadata(path).map(|m| m.modified).ok();
    for entry in p.read_dir(path).unwrap_or_default() {
        if let Ok(meta) = p.metadata(&entry.path) {
            bump(&mut newest, meta.modified);
        }
    }
    if let Some(gitdir) = gitdir {
        for admin in [gitdir.join("HEAD"), gitdir.join("index")] {
            if let Ok(meta) = p.metadata(&admin) {
                bump(&mut newest, meta.modified);
            }
        }
    }
    newest.unwrap_or(UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_saver_platform::FakePlatform;
    use std::time::Duration;

    fn t(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    const WT: &str = "/home/u/repo/.claude/worktrees/agent-1";
    const ADMIN: &str = "/home/u/repo/.git/worktrees/agent-1";

    fn linked(gitdir_line: &str) -> FakePlatform {
        FakePlatform::new()
            .with_home("/home/u")
            .with_file(format!("{WT}/.git"), gitdir_line, t(10))
            .with_file(format!("{WT}/src/main.rs"), vec![0u8; 100], t(10))
            .with_sized_dir(format!("{WT}/target"), 5_000, t(20))
            .with_file(format!("{ADMIN}/HEAD"), "ref: refs/heads/fix/x\n", t(30))
            .with_file(format!("{ADMIN}/index"), vec![0u8; 8], t(40))
    }

    #[test]
    fn reads_a_linked_worktree() {
        let fake = linked(&format!("gitdir: {ADMIN}\n"));
        let wt = inspect(&fake, Path::new(WT));

        assert_eq!(wt.gitdir.as_deref(), Some(Path::new(ADMIN)));
        assert_eq!(wt.head, Head::Branch("fix/x".into()));
        assert!(!wt.locked);
        assert_eq!(wt.size, fake.dir_size(Path::new(WT)).unwrap());
        assert!(wt.size >= 5_000);
        // The index (t40) is newer than anything in the checkout.
        assert_eq!(wt.last_active, t(40));
        assert_eq!(wt.label(), format!("{WT} [fix/x]"));
    }

    #[test]
    fn a_rewritten_reflog_is_not_activity() {
        // `git gc` expiring the reflog rewrote it today; nobody used the worktree.
        let fake = linked(&format!("gitdir: {ADMIN}\n")).with_file(
            format!("{ADMIN}/logs/HEAD"),
            "",
            t(99),
        );
        assert_eq!(inspect(&fake, Path::new(WT)).last_active, t(40));
    }

    #[test]
    fn a_relative_gitdir_resolves_against_the_checkout() {
        let fake = linked("gitdir: ../../../.git/worktrees/agent-1\n");
        let wt = inspect(&fake, Path::new(WT));
        assert_eq!(wt.gitdir.as_deref(), Some(Path::new(ADMIN)));
        assert_eq!(wt.head, Head::Branch("fix/x".into()));
    }

    #[test]
    fn notes_a_lock_and_a_detached_head() {
        let fake = linked(&format!("gitdir: {ADMIN}\n"))
            .with_file(format!("{ADMIN}/HEAD"), "0123abcd\n", t(30))
            .with_file(format!("{ADMIN}/locked"), "claude agent", t(30));
        let wt = inspect(&fake, Path::new(WT));
        assert_eq!(wt.head, Head::Detached);
        assert!(wt.locked);
        assert_eq!(wt.label(), format!("{WT} [detached, locked]"));
    }

    #[test]
    fn a_directory_without_git_is_still_measured() {
        let fake = FakePlatform::new().with_home("/home/u").with_sized_dir(
            format!("{WT}/target"),
            7_000,
            t(5),
        );
        let wt = inspect(&fake, Path::new(WT));
        assert_eq!(wt.gitdir, None);
        assert_eq!(wt.head, Head::Unknown);
        assert!(wt.size >= 7_000);
        assert_eq!(wt.label(), format!("{WT} [not a git worktree]"));
    }

    #[test]
    fn a_symlinked_git_is_not_followed() {
        let fake = linked(&format!("gitdir: {ADMIN}\n"));
        let fake = fake
            .with_file("/elsewhere/.git-file", format!("gitdir: {ADMIN}\n"), t(1))
            .with_symlink(format!("{WT}/.git"), "/elsewhere/.git-file", t(1));
        let wt = inspect(&fake, Path::new(WT));
        assert_eq!(wt.gitdir, None);
        assert_eq!(wt.head, Head::Unknown);
    }
}
