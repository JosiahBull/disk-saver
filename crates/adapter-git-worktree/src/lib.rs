//! `disk-saver-adapter-git-worktree` — linked git worktrees whose repository
//! has forgotten them, or whose branch merged.
//!
//! `git worktree add` makes a checkout whose `.git` is a one-line *file*,
//! `gitdir: <repo>/.git/worktrees/<name>`, pointing at an admin directory inside
//! the repository. When that admin directory goes — `git worktree prune`, a
//! re-clone, a deleted repository — the checkout is stranded: every git command
//! in it fails with "gitfile does not point to a valid repository", nothing
//! lists it, and nothing will ever clean it up. Coding agents create worktrees
//! constantly, so these pile up; one sweep of `~/work` found 14 of them holding
//! 43 GB, several dot-named (`.wt-prepare-latency`).
//!
//! A worktree its repository still lists is done with once its branch merges,
//! and nothing removes it then either: the same sweep found 54 holding 78 GB.
//! Whether a branch merged comes from its forge, asked by [`disk_saver_forge`];
//! git alone cannot tell after a squash merge. A merged worktree is proposed
//! once idle for `merged_age`, one whose MR or PR is open never, and any other
//! once idle past the retention policy.
//!
//! A checkout can still hold the only copy of someone's work, so this
//! adapter proposes and never disposes on its own say-so. Each candidate is
//! [`Class::UserData`] and carries *evidence* of what removing it would lose.
//! Without `confirm = false` every candidate waits for `disk-saver review`;
//! with it, only forgotten or merged checkouts whose evidence came back clean
//! are removed automatically.
//!
//! # Phases
//!
//! * **check** — one forge lookup per host, for `disk-saver doctor`.
//! * **observe** — no-op. Age comes from the filesystem.
//! * **plan** — only under pressure. Walk the roots with
//!   [`disk_saver_scan::find_dirs`], which offers dot-named directories too.
//!   A directory matches when its `.git` file names a `<common>/worktrees/<name>`
//!   admin directory that does not exist. A `.git` file pointing elsewhere (a
//!   submodule's `.git/modules/<name>`) is not a worktree; one whose admin
//!   directory exists names its repository, whose `worktrees/*/gitdir` list its
//!   checkouts, even ones in `.claude/worktrees` the walk never enters. Every
//!   orphan found is [claimed](Ctx::claim), so `git-ignored` leaves it alone;
//!   a listed worktree is claimed when proposed. Worktrees idle past their
//!   threshold get the evidence check, and its result is stored in KV against
//!   the path.
//! * **execute** — repeat every check: still a real directory, still forgotten
//!   or listed as proposed, still not protected, and evidence no worse than
//!   what was proposed (see `regression`). Then remove it, with a listed one's
//!   admin directory, and report the bytes measured just before. The branch
//!   stays.
//!
//! # The evidence check
//!
//! A listed worktree's evidence is `git status`: every staged, unstaged or
//! untracked path, fingerprinted so `execute` can see one that changed after it
//! was proposed. Git runs without optional locks, so the index is never
//! rewritten; a rewritten index would read as activity. A detached HEAD on
//! commits no branch, remote-tracking branch or tag holds keeps the worktree.
//!
//! A forgotten worktree has no index and no `HEAD`, so "what is uncommitted"
//! has no answer. What can be answered is whether each file's *content* is in
//! the repository's object store, which is what losing the checkout would cost:
//!
//! 1. Point git at the repository with an index file that does not exist. A
//!    missing index reads as empty and these commands never write one, so every
//!    file is untracked and `git ls-files --others --exclude-standard` lists
//!    every file the repository does not ignore.
//! 2. Keep non-empty regular files. Symlinks are skipped (`hash-object` aborts
//!    on a symlinked directory); a nested repository, which git lists as one
//!    directory entry, is reported as such, since its files were not checked.
//! 3. Hash each file's raw bytes (`--no-filters`) and look the hashes up with
//!    `git cat-file --batch-check`.
//! 4. Re-check the misses the way git would store them. Files under a Git LFS
//!    filter are hashed as their LFS pointer, computed here: running git-lfs's
//!    clean filter would copy every untracked file matching an LFS pattern into
//!    `.git/lfs/objects`, gigabytes of writes in a run that exists because the
//!    disk is full. Other files go through `hash-object` with the repository's
//!    filters, which covers line-ending conversion.
//!
//! Whatever is still missing is unsaved work. Either check fails closed: any
//! failing step, a count that does not line up, or a file name holding a
//! newline makes the result unknown, which always needs a person.
//!
//! Checkouts of other repositories inside a candidate, such as an import a
//! vendoring tool keeps there, go with it: each gets the status check, and its
//! paths are reported under where it sits.
//!
//! What the check cannot see: ignored files (a local `.env` goes with the
//! checkout), whether a blob is still *reachable* from a ref, and files inside a
//! nested repository that the checkout ignores.
//!
//! # Safety guardrails
//!
//! Never proposed or removed, whatever the config says:
//!
//! * anything under a `.codex` directory, or under `~/Documents/Codex` (the
//!   Codex app's task workspace), and anything under a `.claude` directory
//!   whose branch has not merged — agent-managed worktrees;
//! * a worktree locked with `git worktree lock`;
//! * a checkout inside another repository's work tree, such as an import a
//!   vendoring tool checks out from a bare cache;
//! * a checkout holding a non-empty `.claude/worktrees` or `.codex/worktrees`;
//! * a forgotten checkout the repository still lists under another admin
//!   directory (`git worktree repair` can relink it), or a checkout containing a
//!   worktree the repository still lists;
//! * anything another adapter claimed first.
//!
//! The shared walker supplies the `.disk-saver-keep` sentinel, the denylist, no
//! symlink following, and staying on one filesystem.
//!
//! # Config (`[adapters.git-worktree]`)
//!
//! | Key | Default | |
//! |---|---|---|
//! | `roots` | `["~"]` | directories to walk |
//! | `exclude` | `[]` | absolute-path globs to skip |
//! | `max_depth` | `8` | levels below each root |
//! | `merged_age` | `"1d"` | idle time before a worktree whose branch merged is proposed |
//! | `max_age` | `"14d"` | idle time before any other worktree is proposed |
//! | `min_age` | `"3d"` | floor under scavenge; lowers itself to a shorter `max_age` |
//! | `confirm` | `true` | `false` removes forgotten checkouts with clean evidence and merged ones with nothing uncommitted without asking |
//! | `forges` | `{}` | host → `"github"`, `"gitlab"` or `"none"`; `github.com`, `gitlab.com`, `github.*` and `gitlab.*` need no entry |

#![forbid(unsafe_code)]

mod checks;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigCx, ConfigError, Configured,
    Ctx, Decision, DecisionKind, DecisionLog, FileKind, Outcome, Platform, Pressure,
    RetentionPolicy,
};
use disk_saver_forge::{self as forge, ForgeKind, ForgeState};
use disk_saver_scan::{FsConfig, KEEP_SENTINEL, ScanOptions, find_dirs};
use globset::GlobSet;
use serde::{Deserialize, Serialize};

use checks::{Check, Status, UNREACHABLE, embedded, evidence};

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "git-worktree";

/// Every candidate is somebody's checkout: gone is gone.
const CLASS: Class = Class::UserData;

const DAY: u64 = 24 * 60 * 60;

/// Threshold for a worktree whose branch merged when the config names none.
const DEFAULT_MERGED_AGE: Duration = Duration::from_secs(DAY);

/// Normal-mode threshold for every other worktree when the config names none.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(14 * DAY);

/// Scavenge floor when the config names none. Days rather than the hour the
/// build-artifact adapters use: what goes here is a whole checkout, not
/// something a rebuild restores.
const DEFAULT_MIN_AGE: Duration = Duration::from_secs(3 * DAY);

/// Directories where coding agents keep worktrees they manage. A checkout
/// under `.claude` is proposed only once its branch has merged, one under
/// `.codex` never, and a checkout holding either is never proposed.
const AGENT_DIRS: &[&str] = &[".claude", ".codex"];

/// Agent workspaces under the home directory, relative to it. The Codex app
/// runs each task in `~/Documents/Codex/<date>/<task>`, and a checkout it made
/// there is as much its own as one under `.codex`.
const AGENT_WORKSPACES: &[&str] = &["Documents/Codex"];

/// Top-level entries whose mtimes say nothing about whether a person used the
/// checkout: git's own file, build output and caches that tools rewrite, and
/// Finder metadata.
const NOT_ACTIVITY: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    ".DS_Store",
];

/// How long the evidence check may run. It reads every file the repository
/// does not ignore, which on a large monorepo checkout is tens of seconds.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How many unsaved paths a label names.
const SAMPLES: usize = 3;

/// Above this many unsaved paths, the KV baseline keeps a digest instead of
/// the list, and execute then requires the set to be unchanged.
const MAX_RECORDED_LOST: usize = 2_000;

/// KV key prefix for the evidence a checkout was proposed with.
const BASELINE_PREFIX: &str = "planned:";

/// A baseline nobody has re-planned for this long is dropped. Every plan run
/// refreshes the baseline of everything it proposes, so only checkouts that
/// stopped being proposed age out.
const BASELINE_TTL: Duration = Duration::from_secs(30 * DAY);

// ── configuration ────────────────────────────────────────────────────────────

/// `[adapters.git-worktree]`. The keys are [`FsConfig`]'s plus `merged_age`
/// and `forges`, with this adapter's own defaults for `max_age` and `confirm`;
/// a flattened `FsConfig` would impose its 30 days and `confirm = false`.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct WorktreeConfig {
    /// Directories to walk. Defaults to `["~"]`.
    roots: Vec<PathBuf>,
    /// Absolute-path globs never walked, proposed or removed.
    exclude: Vec<String>,
    /// Levels below each root to descend. Defaults to `8`.
    max_depth: usize,
    /// Idle time after which a worktree whose branch merged is proposed.
    #[serde(with = "humantime_serde")]
    merged_age: Duration,
    /// Idle time after which any other worktree is proposed under normal
    /// pressure.
    #[serde(with = "humantime_serde")]
    max_age: Duration,
    /// Floor under scavenge; `None` is [`DEFAULT_MIN_AGE`].
    #[serde(with = "humantime_serde")]
    min_age: Option<Duration>,
    /// Ask before removing even a worktree with nothing unsaved.
    confirm: bool,
    /// Which forge serves each remote host, beyond the hosts recognised by
    /// name.
    forges: BTreeMap<String, ForgeKind>,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        WorktreeConfig {
            roots: vec![PathBuf::from("~")],
            exclude: Vec::new(),
            max_depth: 8,
            merged_age: DEFAULT_MERGED_AGE,
            max_age: DEFAULT_MAX_AGE,
            min_age: None,
            confirm: true,
            forges: BTreeMap::new(),
        }
    }
}

impl WorktreeConfig {
    /// The shared walker config, for root and denylist resolution.
    fn as_fs(&self) -> FsConfig {
        FsConfig {
            roots: self.roots.clone(),
            exclude: self.exclude.clone(),
            max_depth: self.max_depth,
            max_age: self.max_age,
            min_age: self.min_age,
            confirm: self.confirm,
        }
    }
}

/// The factory the CLI registry uses to build the `git-worktree` adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, |cfg, cx| build(cfg, cx, checks::check))
}

/// The adapter with `check` gathering each checkout's evidence.
#[cfg(test)]
fn factory_with(check: Check) -> AdapterFactory {
    AdapterFactory::typed(NAME, move |cfg, cx| build(cfg, cx, check))
}

/// Resolve the scavenge floor, validate the thresholds, and compile `exclude`
/// up front so a bad glob is a startup error rather than a failed run.
fn build(
    mut cfg: WorktreeConfig,
    cx: &ConfigCx,
    check: Check,
) -> Result<GitWorktrees, ConfigError> {
    // Our 3-day floor gives way to a shorter `max_age` the user wrote; a
    // `min_age` they wrote above `max_age` is reported.
    let min_age = Configured::from(cfg.min_age)
        .with_default(DEFAULT_MIN_AGE)
        .capped_at(cfg.max_age, cx, "min_age", "max_age")?
        .unwrap_or(DEFAULT_MIN_AGE);
    if cfg.merged_age > cfg.max_age {
        return Err(cx.err(format!(
            "merged_age ({:?}) must not exceed max_age ({:?}): a merged worktree is the safer one to remove",
            cfg.merged_age, cfg.max_age
        )));
    }
    cfg.forges = std::mem::take(&mut cfg.forges)
        .into_iter()
        .map(|(host, kind)| (host.to_ascii_lowercase(), kind))
        .collect();
    Ok(GitWorktrees {
        policy: cx.retention(cfg.max_age, min_age)?,
        exclude: cx.globs("exclude", &cfg.exclude)?,
        check,
        cfg,
    })
}

/// The `git-worktree` adapter.
struct GitWorktrees {
    cfg: WorktreeConfig,
    /// The policy for every worktree whose branch has not merged.
    policy: RetentionPolicy,
    /// The user's `exclude` globs, re-checked in `execute` so an approval that
    /// predates a new exclude cannot remove what it now covers.
    exclude: GlobSet,
    /// What gathers each checkout's evidence.
    check: Check,
}

// ── identification ───────────────────────────────────────────────────────────

/// Where a linked worktree's `.git` file points.
#[derive(Debug, Clone)]
struct Link {
    /// `<common>/worktrees/<name>`: the admin directory the repository keeps
    /// for this worktree.
    admin: PathBuf,
    /// `<common>`: the repository's git directory.
    common: PathBuf,
}

impl Link {
    /// The repository, for people: the work tree holding `.git`, or a bare
    /// repository's own directory.
    fn repo(&self) -> &Path {
        match self.common.parent() {
            Some(parent) if self.common.file_name().is_some_and(|n| n == ".git") => parent,
            _ => &self.common,
        }
    }
}

/// Read `dir/.git` as a linked worktree's gitfile.
///
/// `None` unless it is a regular file holding `gitdir: <path>` (parsed the way
/// git does: trailing line ends dropped, a relative path resolved against
/// `dir`) whose parent directory is named `worktrees`. That last condition is
/// what separates a linked worktree from a submodule, whose gitfile points into
/// `.git/modules`.
fn read_link(p: &dyn Platform, dir: &Path) -> Option<Link> {
    let dotgit = dir.join(".git");
    let meta = p.metadata(&dotgit).ok()?;
    // A gitfile is one short line; anything large is something else.
    if meta.kind != FileKind::File || meta.len > 4096 {
        return None;
    }
    let text = p.read_to_string(&dotgit).ok()?;
    let target = text
        .trim_end_matches(['\n', '\r'])
        .strip_prefix("gitdir: ")?;
    if target.is_empty() {
        return None;
    }
    let admin = normalize(&dir.join(target));
    let worktrees = admin.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    Some(Link {
        common: worktrees.parent()?.to_path_buf(),
        admin,
    })
}

/// Whether the admin directory is gone, i.e. the worktree is orphaned.
///
/// Only "no such file" (or a path component that is not a directory) counts.
/// Any other failure — permission denied on the way to it — reads as present,
/// because a worktree wrongly judged orphaned is held to the weaker evidence.
fn admin_missing(p: &dyn Platform, link: &Link) -> bool {
    match p.metadata(&link.admin) {
        Ok(_) => false,
        Err(e) => matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory),
    }
}

/// Resolve `.` and `..` lexically, as git does with a relative gitdir.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The `.claude` or `.codex` component of `path`, if it has one.
fn agent_component(path: &Path) -> Option<&'static str> {
    path.components().find_map(|c| {
        AGENT_DIRS
            .iter()
            .copied()
            .find(|name| c.as_os_str() == *name)
    })
}

/// Checkouts each repository still lists, keyed by `<common>`, so a plan run
/// reads each repository's admin directories once.
#[derive(Default)]
struct Listed(HashMap<PathBuf, Vec<(PathBuf, PathBuf)>>);

impl Listed {
    /// The worktree checkouts `common` lists, after their admin directories:
    /// each `<common>/worktrees/*/gitdir` holds the path of a checkout's `.git` file.
    fn of(&mut self, p: &dyn Platform, common: &Path) -> &[(PathBuf, PathBuf)] {
        self.0.entry(common.to_path_buf()).or_insert_with(|| {
            let Ok(admins) = p.read_dir(&common.join("worktrees")) else {
                return Vec::new();
            };
            admins
                .iter()
                .filter(|a| a.kind == FileKind::Dir)
                .filter_map(|a| {
                    let text = p.read_to_string(&a.path.join("gitdir")).ok()?;
                    let dotgit = text.trim_end_matches(['\n', '\r']);
                    if dotgit.is_empty() {
                        return None;
                    }
                    let checkout = normalize(&a.path.join(dotgit)).parent()?.to_path_buf();
                    Some((a.path.clone(), checkout))
                })
                .collect()
        })
    }
}

/// A linked worktree's checkout.
#[derive(Debug, Clone)]
struct Worktree {
    dir: PathBuf,
    link: Link,
    /// Its repository no longer lists it: the admin directory is gone.
    forgotten: bool,
}

impl Worktree {
    /// The worktree checked out at `dir`, if `dir` is one.
    fn at(p: &dyn Platform, dir: &Path) -> Option<Worktree> {
        let link = read_link(p, dir)?;
        Some(Worktree {
            dir: dir.to_path_buf(),
            forgotten: admin_missing(p, &link),
            link,
        })
    }
}

/// Every worktree under the roots, forgotten or listed.
///
/// The walk finds a forgotten worktree by its `.git` file, and does not descend
/// into it. It finds repositories too: a listed worktree's `.git` file names
/// one, and so does any `*.git` directory holding `worktrees/`. Each
/// repository's admin directories then say where its checkouts are. A listing
/// whose checkout no longer names it back (moved, copied or gone) is skipped:
/// `git worktree prune` and `repair` own those.
fn discover(
    p: &dyn Platform,
    roots: &[PathBuf],
    opts: &ScanOptions,
    listed: &mut Listed,
) -> Vec<Worktree> {
    let commons = RefCell::new(BTreeSet::new());
    let forgotten = find_dirs(p, roots, opts, &|p, entry| {
        match Worktree::at(p, &entry.path) {
            Some(wt) if wt.forgotten => return true,
            Some(wt) => {
                commons.borrow_mut().insert(wt.link.common);
            }
            None => {
                if entry.file_name.ends_with(".git")
                    && p.metadata(&entry.path.join("worktrees"))
                        .is_ok_and(|m| m.kind == FileKind::Dir)
                {
                    commons.borrow_mut().insert(entry.path.clone());
                }
            }
        }
        false
    });

    let mut found: BTreeMap<PathBuf, Worktree> = forgotten
        .into_iter()
        .filter_map(|dir| Some((dir.clone(), Worktree::at(p, &dir)?)))
        .collect();
    for common in commons.into_inner() {
        for (admin, dir) in listed.of(p, &common) {
            if read_link(p, dir).is_some_and(|l| l.admin == *admin) {
                found.entry(dir.clone()).or_insert_with(|| Worktree {
                    dir: dir.clone(),
                    link: Link {
                        admin: admin.clone(),
                        common: common.clone(),
                    },
                    forgotten: false,
                });
            }
        }
    }
    found.into_values().collect()
}

/// Whether `dir` is somewhere the adapter may look: under a root, outside the
/// denylist, and neither excluded nor exempted by a `.disk-saver-keep` between
/// it and its root. The walk applies the same rules to what it enters; a
/// checkout found through its repository was never walked.
fn in_scope(
    p: &dyn Platform,
    dir: &Path,
    roots: &[PathBuf],
    exclude: &GlobSet,
    denylist: &[PathBuf],
) -> bool {
    let Some(root) = roots.iter().find(|r| dir.starts_with(r)) else {
        return false;
    };
    if denylist.iter().any(|d| dir.starts_with(d)) {
        return false;
    }
    dir.ancestors()
        .take_while(|a| a.starts_with(root))
        .all(|a| !exclude.is_match(a) && p.metadata(&a.join(KEEP_SENTINEL)).is_err())
}

/// Why a worktree must be left alone whatever its branch did, if it must.
///
/// Checked in `plan` and again in `execute`, so an approval cannot outlive the
/// reason it would now be refused. The `.claude` rule depends on the branch,
/// so `plan` applies it.
fn protected(p: &dyn Platform, wt: &Worktree, listed: &mut Listed) -> Option<String> {
    let dir = &wt.dir;
    if agent_component(dir) == Some(".codex") {
        return Some("agent-managed: it is under a .codex directory".to_string());
    }
    let home = p.home_dir();
    if let Some(workspace) = AGENT_WORKSPACES
        .iter()
        .find(|ws| dir.starts_with(home.join(ws)))
    {
        return Some(format!("agent-managed: it is under ~/{workspace}"));
    }
    if !wt.forgotten && p.metadata(&wt.link.admin.join("locked")).is_ok() {
        return Some("it is locked with `git worktree lock`".to_string());
    }
    // The nearest work tree enclosing it. Another repository's means a tool
    // vendored this checkout there, and the enclosing project needs it; agent
    // worktrees in `<repo>/.claude/worktrees` belong to the enclosing
    // repository itself. The search stops below the home directory, which may
    // be a dotfiles work tree.
    let enclosing = dir
        .ancestors()
        .skip(1)
        .take_while(|a| *a != home && a.parent().is_some())
        .find_map(|host| {
            let dotgit = host.join(".git");
            let common = match p.metadata(&dotgit).ok()?.kind {
                FileKind::Dir => Some(dotgit),
                _ => read_link(p, host).map(|l| l.common),
            };
            Some((host, common))
        });
    if let Some((host, common)) = enclosing
        && common.as_deref() != Some(wt.link.common.as_path())
    {
        return Some(format!(
            "it is checked out inside {}, another repository's work tree",
            tilde(&home, host)
        ));
    }
    for agent in AGENT_DIRS {
        let worktrees = dir.join(agent).join("worktrees");
        if p.read_dir(&worktrees).is_ok_and(|e| !e.is_empty()) {
            return Some(format!(
                "it holds agent-managed worktrees in {agent}/worktrees"
            ));
        }
    }
    for (_, checkout) in listed.of(p, &wt.link.common) {
        if checkout == dir && wt.forgotten {
            return Some(format!(
                "{} still lists a worktree here; `git worktree repair` can relink it",
                tilde(&home, wt.link.repo())
            ));
        }
        if checkout != dir && checkout.starts_with(dir) {
            return Some(format!(
                "it holds {}, a worktree {} still lists",
                tilde(&home, checkout),
                tilde(&home, wt.link.repo())
            ));
        }
    }
    None
}

/// What a listed worktree has checked out, from its admin directory's `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Head {
    Branch(String),
    Detached,
}

fn read_head(p: &dyn Platform, admin: &Path) -> Option<Head> {
    let text = p.read_to_string(&admin.join("HEAD")).ok()?;
    let text = text.trim_end_matches(['\n', '\r']);
    match text.strip_prefix("ref: refs/heads/") {
        Some("") => None,
        Some(branch) => Some(Head::Branch(branch.to_string())),
        None => is_object_id(text).then_some(Head::Detached),
    }
}

/// When the worktree was last used: the newest mtime among the checkout's
/// top-level entries, ignoring [`NOT_ACTIVITY`] (failing that, the directory's
/// own), and, while its repository lists it, its admin directory's `HEAD` and
/// `index`, which git rewrites on every commit, checkout and `git add`. The
/// reflog is left out: `git gc` rewrites it when it expires entries.
///
/// `now` if nothing can be read: activity we cannot see is treated as recent.
fn last_used(p: &dyn Platform, wt: &Worktree, now: SystemTime) -> SystemTime {
    let newest = p
        .read_dir(&wt.dir)
        .ok()
        .into_iter()
        .flatten()
        .filter(|e| !NOT_ACTIVITY.contains(&e.file_name.as_str()))
        .filter_map(|e| p.metadata(&e.path).ok().map(|m| m.modified))
        .max()
        .or_else(|| p.metadata(&wt.dir).ok().map(|m| m.modified));
    let admin = ["HEAD", "index"]
        .iter()
        .filter(|_| !wt.forgotten)
        .filter_map(|f| p.metadata(&wt.link.admin.join(f)).ok().map(|m| m.modified));
    newest.into_iter().chain(admin).max().unwrap_or(now)
}

// ── evidence ─────────────────────────────────────────────────────────────────

/// What the evidence check found in one checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    /// Nothing would be lost.
    Clean {
        /// Paths checked.
        checked: u64,
    },
    /// Something would.
    Unsaved {
        /// Paths checked.
        checked: u64,
        /// Every path that would be lost, sorted.
        lost: Vec<Lost>,
    },
    /// The check could not say.
    Unknown {
        /// Why, for the label.
        reason: String,
    },
}

/// One path whose content would be lost.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Lost {
    /// A nested repository git lists as a single entry; its files were not
    /// checked.
    Repo { path: String },
    /// A file whose content, hashed as git would store it, is `blob`.
    File { path: String, blob: String },
}

impl Lost {
    /// The path, relative to the checkout.
    fn path(&self) -> &str {
        match self {
            Lost::Repo { path } | Lost::File { path, .. } => path,
        }
    }

    /// One line naming the path and its content, so an edit to an
    /// already-unsaved file shows up as a change.
    fn key(&self) -> String {
        match self {
            Lost::Repo { path } => format!("nested {path}"),
            Lost::File { path, blob } => format!("missing {blob} {path}"),
        }
    }
}

impl Evidence {
    /// Clean, with at least one file actually checked. A forgotten checkout in
    /// which the check found nothing to hash (everything ignored) proved
    /// nothing, so it is not enough to remove one without asking.
    fn is_clean(&self) -> bool {
        matches!(self, Evidence::Clean { checked } if *checked > 0)
    }

    /// The label's account of the evidence: for a forgotten checkout, what is
    /// not in the object store; for a listed one, what is uncommitted.
    fn summary(&self, forgotten: bool) -> String {
        match self {
            Evidence::Clean { .. } if !forgotten => "nothing uncommitted".to_string(),
            Evidence::Clean { checked: 0 } => {
                "no files to check: everything in it is ignored, empty or a symlink".to_string()
            }
            Evidence::Clean { checked } => {
                format!("every non-ignored file's content is in git ({checked} checked)")
            }
            Evidence::Unsaved { lost, .. } => {
                let repos = lost.iter().filter(|l| matches!(l, Lost::Repo { .. }));
                let files = lost.iter().filter(|l| matches!(l, Lost::File { .. }));
                let (n_repos, n_files) = (repos.clone().count(), files.clone().count());
                let mut parts = Vec::new();
                if n_files > 0 {
                    let noun = plural(n_files, "file");
                    parts.push(if forgotten {
                        format!("{n_files} {noun} not in git")
                    } else {
                        format!("{n_files} uncommitted {noun}")
                    });
                }
                if n_repos > 0 {
                    let noun = if n_repos == 1 {
                        "repository"
                    } else {
                        "repositories"
                    };
                    parts.push(format!("{n_repos} nested {noun} not checked"));
                }
                // Nested repositories first: the less obvious loss.
                let shown: Vec<&str> = repos.chain(files).take(SAMPLES).map(Lost::path).collect();
                let more = if lost.len() > SAMPLES { ", …" } else { "" };
                format!("{} ({}{more})", parts.join(" and "), shown.join(", "))
            }
            Evidence::Unknown { reason } => {
                let work = if forgotten { "unsaved" } else { "uncommitted" };
                format!("couldn't check for {work} work: {reason}")
            }
        }
    }
}

/// `"file"` or `"files"`.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
}

/// Parse evidence written as a `checked <n>` line, then one [`Lost::key`] line
/// per path, the form the policy tests script it in. `None` if any line is not
/// one of those.
#[cfg(test)]
fn parse_report(text: &str) -> Option<Evidence> {
    // Split on `\n` only: a path may end in `\r`.
    let mut lines = text.split('\n');
    let checked: u64 = lines.next()?.strip_prefix("checked ")?.parse().ok()?;
    let mut lost = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (tag, rest) = line.split_once(' ')?;
        let entry = match tag {
            "missing" => {
                let (blob, path) = rest.split_once(' ')?;
                (is_object_id(blob) && !path.is_empty()).then(|| Lost::File {
                    path: path.to_string(),
                    blob: blob.to_string(),
                })
            }
            "nested" => (!rest.is_empty()).then(|| Lost::Repo {
                path: rest.to_string(),
            }),
            _ => None,
        };
        lost.push(entry?);
    }
    if lost.is_empty() {
        return Some(Evidence::Clean { checked });
    }
    lost.sort();
    Some(Evidence::Unsaved { checked, lost })
}

/// A SHA-1 or SHA-256 object id in lowercase hex.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

// ── the baseline a proposal was made with ────────────────────────────────────

/// What `plan` stored in KV for a checkout it proposed: the evidence its label
/// showed, so `execute` can tell whether things got worse since.
#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    /// The label proposed. A plan run whose result never reached the approvals
    /// queue (a dry run) can overwrite the baseline, so `execute` only trusts
    /// one whose label matches the candidate it was handed.
    label: String,
    /// Unix seconds when stored, for expiry.
    recorded_at: u64,
    /// What the checkout was proposed as; `execute` refuses one that is
    /// something else now.
    proposed_as: ProposedAs,
    evidence: Recorded,
}

/// The state a checkout was proposed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProposedAs {
    /// Its repository had forgotten it.
    Forgotten,
    /// Listed, and its branch had merged.
    Merged,
    /// Listed, and its branch had not merged (or nobody could say).
    Unmerged,
}

/// [`Evidence`] reduced to what `execute` compares against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Recorded {
    Clean,
    Unsaved {
        /// FNV-1a of the sorted [`Lost::key`]s.
        digest: String,
        /// The keys themselves, unless there were more than
        /// [`MAX_RECORDED_LOST`].
        lost: Option<Vec<String>>,
    },
    Unknown,
}

impl Recorded {
    fn of(evidence: &Evidence) -> Self {
        match evidence {
            Evidence::Clean { .. } => Recorded::Clean,
            Evidence::Unsaved { lost, .. } => {
                let keys = keys(lost);
                Recorded::Unsaved {
                    digest: digest(&keys),
                    lost: (keys.len() <= MAX_RECORDED_LOST).then_some(keys),
                }
            }
            Evidence::Unknown { .. } => Recorded::Unknown,
        }
    }
}

/// The sorted keys of `lost`.
fn keys(lost: &[Lost]) -> Vec<String> {
    let mut keys: Vec<String> = lost.iter().map(Lost::key).collect();
    keys.sort();
    keys
}

/// A stable 64-bit FNV-1a digest of `keys`, one per line.
fn digest(keys: &[String]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in keys.iter().flat_map(|k| k.bytes().chain(*b"\n")) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Why the evidence now is worse than what was proposed, if it is.
///
/// Clean now is always fine. Otherwise the person who approved must have seen
/// everything that would now be lost:
///
/// * proposed clean — anything else is new;
/// * proposed with unsaved files — every path now unsaved must have been
///   unsaved then *with the same content* (a subset check, or an unchanged
///   digest when the set was too large to store);
/// * proposed as unknown — still unknown is what was approved, but unsaved
///   files are news the label never showed.
fn regression(before: &Recorded, now: &Evidence) -> Option<String> {
    let lost = match (before, now) {
        (_, Evidence::Clean { .. }) | (Recorded::Unknown, Evidence::Unknown { .. }) => return None,
        (_, Evidence::Unknown { reason }) => {
            return Some(format!("couldn't re-check it for unsaved work: {reason}"));
        }
        (_, Evidence::Unsaved { lost, .. }) => lost,
    };
    let count = |n: usize| format!("{n} {}", plural(n, "path"));
    match before {
        Recorded::Clean => Some(format!(
            "{} not in git now, but every file was when it was proposed",
            count(lost.len())
        )),
        Recorded::Unknown => Some(format!(
            "{} not in git now, which its proposal couldn't show",
            count(lost.len())
        )),
        Recorded::Unsaved {
            lost: Some(proposed),
            ..
        } => {
            let proposed: BTreeSet<&str> = proposed.iter().map(String::as_str).collect();
            let fresh: Vec<&Lost> = lost
                .iter()
                .filter(|l| !proposed.contains(l.key().as_str()))
                .collect();
            let first = fresh.first()?;
            Some(format!(
                "{} not in git changed or appeared since it was proposed ({})",
                count(fresh.len()),
                first.path()
            ))
        }
        Recorded::Unsaved {
            lost: None,
            digest: d,
        } => (digest(&keys(lost)) != *d)
            .then(|| "the files not in git changed since it was proposed".to_string()),
    }
}

fn baseline_key(id: &str) -> String {
    format!("{BASELINE_PREFIX}{id}")
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Drop baselines nothing has re-planned within [`BASELINE_TTL`], and any that
/// no longer parse. Best-effort.
fn expire_baselines(ctx: &Ctx, now: SystemTime) {
    let Ok(keys) = ctx.kv.keys(BASELINE_PREFIX) else {
        return;
    };
    let cutoff = unix(now).saturating_sub(BASELINE_TTL.as_secs());
    for key in keys {
        let stale = match ctx.kv.get::<Baseline>(&key) {
            Ok(Some(b)) => b.recorded_at < cutoff,
            Ok(None) => false,
            Err(_) => true,
        };
        if stale {
            let _ = ctx.kv.delete(&key);
        }
    }
}

// ── the adapter ──────────────────────────────────────────────────────────────

impl GitWorktrees {
    /// Re-check a candidate immediately before removing it, returning it and
    /// the checkouts embedded in it, whose admin directories go with it. `Err`
    /// is the reason to skip it.
    fn revalidate(&self, ctx: &Ctx, c: &Candidate) -> Result<(Worktree, Vec<Worktree>), String> {
        let p = ctx.platform;
        let dir = Path::new(&c.id);
        if !dir.is_absolute() {
            return Err(format!("{} is not an absolute path", c.id));
        }
        let baseline = ctx.kv.get::<Baseline>(&baseline_key(&c.id)).ok().flatten();
        if let Some(agent) = agent_component(dir) {
            let merged = baseline
                .as_ref()
                .is_some_and(|b| b.proposed_as == ProposedAs::Merged);
            if agent != ".claude" || !merged {
                return Err(format!("agent-managed: it is under a {agent} directory"));
            }
        }
        if self.exclude.is_match(dir) {
            return Err("an `exclude` glob covers it".to_string());
        }
        if FsConfig::denylist(p)
            .iter()
            .any(|d| dir == d || dir.starts_with(d))
        {
            return Err("it is in a directory disk-saver never cleans".to_string());
        }
        match p.metadata(dir) {
            Ok(m) if m.kind == FileKind::Dir => {}
            Ok(m) if m.kind == FileKind::Symlink => return Err("it is now a symlink".to_string()),
            Ok(_) => return Err("it is no longer a directory".to_string()),
            Err(_) => return Err("it no longer exists".to_string()),
        }
        if let Some(keep) = dir
            .ancestors()
            .map(|a| a.join(KEEP_SENTINEL))
            .find(|k| p.metadata(k).is_ok())
        {
            return Err(format!("{} exempts it", keep.display()));
        }
        let wt = Worktree::at(p, dir)
            .ok_or_else(|| "its .git no longer points at a worktree admin directory".to_string())?;
        let mut listed = Listed::default();
        if let Some(reason) = protected(p, &wt, &mut listed) {
            return Err(reason);
        }
        let Some(baseline) = baseline else {
            return Err(
                "no record of the evidence it was proposed with; the next run re-checks it"
                    .to_string(),
            );
        };
        match (baseline.proposed_as, wt.forgotten) {
            (ProposedAs::Forgotten, false) => {
                return Err("it is registered with its repository again".to_string());
            }
            (ProposedAs::Merged | ProposedAs::Unmerged, true) => {
                return Err("its repository no longer lists it".to_string());
            }
            _ => {}
        }
        if !wt.forgotten
            && !listed
                .of(p, &wt.link.common)
                .iter()
                .any(|(admin, checkout)| *admin == wt.link.admin && checkout == dir)
        {
            return Err("its repository lists another checkout under that name".to_string());
        }
        if last_used(p, &wt, ctx.now()) > c.last_used {
            return Err("it has been used since it was proposed".to_string());
        }
        if baseline.label != c.label {
            return Err(
                "its evidence changed after it was proposed; the next run proposes it again"
                    .to_string(),
            );
        }
        let embedded = embedded(p, &wt);
        match evidence(p, &wt, &embedded, self.check) {
            Status::Unreachable => Err(UNREACHABLE.to_string()),
            Status::Checked(now) => match regression(&baseline.evidence, &now) {
                Some(reason) => Err(reason),
                None => Ok((wt, embedded)),
            },
        }
    }
}

impl Adapter for GitWorktrees {
    fn name(&self) -> &'static str {
        NAME
    }

    /// One real lookup per forge host that a listed worktree under the roots
    /// pushes to, so `disk-saver doctor` shows a missing CLI, a lapsed login or
    /// an answer the adapter no longer understands. Runs never call this: with
    /// no answer, a merged worktree just waits for `max_age` and asks.
    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        let p = ctx.platform;
        let (roots, opts) =
            self.cfg.as_fs().resolve_walk(p).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}"))
            })?;
        let denylist = FsConfig::denylist(p);
        let mut hosts: BTreeMap<String, Option<String>> = BTreeMap::new();
        for wt in discover(p, &roots, &opts, &mut Listed::default()) {
            if wt.forgotten || !in_scope(p, &wt.dir, &roots, &opts.exclude, &denylist) {
                continue;
            }
            let Some(Head::Branch(branch)) = read_head(p, &wt.link.admin) else {
                continue;
            };
            let Ok(remote) = forge::remote_of(p, &wt.dir, &branch) else {
                continue;
            };
            if hosts.contains_key(&remote.host) {
                continue;
            }
            let problem = match forge::kind_for(&remote.host, &self.cfg.forges) {
                ForgeKind::None if self.cfg.forges.contains_key(&remote.host) => None,
                ForgeKind::None => Some(
                    "not a known forge; name it in `forges` (or map it to \"none\")".to_string(),
                ),
                kind => match forge::query(p, kind, &remote, &branch) {
                    ForgeState::Unknown(reason) => Some(reason),
                    _ => None,
                },
            };
            hosts.insert(remote.host, problem);
        }
        let problems: Vec<String> = hosts
            .into_iter()
            .filter_map(|(host, problem)| problem.map(|why| format!("{host}: {why}")))
            .collect();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(AdapterError::unavailable(format!(
                "can't tell which worktrees merged ({})",
                problems.join("; ")
            )))
        }
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        // No-op: age comes from the filesystem.
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        if !ctx.pressure.deletes() {
            return Ok(Vec::new());
        }
        let p = ctx.platform;
        let now = ctx.now();
        let home = p.home_dir();
        let (roots, opts) =
            self.cfg.as_fs().resolve_walk(p).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}"))
            })?;
        let denylist = FsConfig::denylist(p);
        let threshold = match ctx.pressure {
            Pressure::Scavenge { .. } => self.policy.min_age,
            Pressure::Normal | Pressure::Comfortable => self.policy.max_age,
        };
        let mut listed = Listed::default();
        let mut warned = BTreeSet::new();

        let mut candidates = Vec::new();
        for wt in discover(p, &roots, &opts, &mut listed) {
            let dir = &wt.dir;
            if !in_scope(p, dir, &roots, &opts.exclude, &denylist) {
                continue;
            }
            if let Some(owner) = ctx.claims.owner(dir) {
                record_kept(ctx, dir, None, format!("claimed by {owner}"));
                continue;
            }
            // A forgotten checkout is ours whether or not it is proposed:
            // `git-ignored` must not offer up its contents piecemeal.
            if wt.forgotten {
                ctx.claim(dir.clone());
            }
            if let Some(reason) = protected(p, &wt, &mut listed) {
                record_kept(ctx, dir, None, reason);
                continue;
            }
            let used = last_used(p, &wt, now);
            let age = now.duration_since(used).unwrap_or_default();
            let idle = |limit: Duration| {
                format!(
                    "idle {}, under the {} threshold",
                    humanize(age),
                    humanize(limit)
                )
            };

            let (state, on, proposed_as) = if wt.forgotten {
                if age < threshold {
                    record_kept(ctx, dir, Some(age), idle(threshold));
                    continue;
                }
                if agent_component(dir) == Some(".claude") {
                    let reason = "agent-managed: it is under a .claude directory";
                    record_kept(ctx, dir, Some(age), reason.to_string());
                    continue;
                }
                let state = "forgotten by its repository".to_string();
                (state, String::new(), ProposedAs::Forgotten)
            } else {
                let Some(head) = read_head(p, &wt.link.admin) else {
                    let reason = "couldn't read what it has checked out";
                    record_kept(ctx, dir, None, reason.to_string());
                    continue;
                };
                // Nothing younger than both thresholds can be proposed, so it
                // is kept before the forge is asked.
                let floor = self.cfg.merged_age.min(threshold);
                if age < floor {
                    record_kept(ctx, dir, Some(age), idle(floor));
                    continue;
                }
                let (forge, on) = match &head {
                    Head::Branch(branch) => (
                        forge::lookup(p, dir, branch, &self.cfg.forges),
                        format!(" on {branch}"),
                    ),
                    Head::Detached => (ForgeState::Detached, " at a detached HEAD".to_string()),
                };
                if let ForgeState::Unknown(reason) = &forge
                    && warned.insert(reason.clone())
                {
                    tracing::warn!(
                        target: NAME,
                        %reason,
                        "couldn't look up a worktree's MR or PR; it waits for max_age"
                    );
                }
                let merged = matches!(forge, ForgeState::Merged(_));
                let limit = if merged {
                    self.cfg.merged_age
                } else {
                    threshold
                };
                if let ForgeState::Open(request) = &forge {
                    record_kept(ctx, dir, Some(age), format!("{request} is open"));
                    continue;
                }
                if !merged && agent_component(dir) == Some(".claude") {
                    let reason = format!("agent-managed, so proposed only once merged: {forge}");
                    record_kept(ctx, dir, Some(age), reason);
                    continue;
                }
                if age < limit {
                    record_kept(ctx, dir, Some(age), format!("{}: {forge}", idle(limit)));
                    continue;
                }
                let proposed_as = if merged {
                    ProposedAs::Merged
                } else {
                    ProposedAs::Unmerged
                };
                (forge.to_string(), on, proposed_as)
            };

            let evidence = match evidence(p, &wt, &embedded(p, &wt), self.check) {
                Status::Unreachable => {
                    record_kept(ctx, dir, Some(age), UNREACHABLE.to_string());
                    continue;
                }
                Status::Checked(evidence) => evidence,
            };
            let bytes = p.dir_size(dir).unwrap_or(0);
            let id = dir.to_string_lossy().into_owned();
            let label = format!(
                "worktree of {}: {}{on} — {state} — {}",
                tilde(&home, wt.link.repo()),
                tilde(&home, dir),
                evidence.summary(wt.forgotten)
            );
            let baseline = Baseline {
                label: label.clone(),
                recorded_at: unix(now),
                proposed_as,
                evidence: Recorded::of(&evidence),
            };
            if let Err(e) = ctx.kv.set(&baseline_key(&id), &baseline, now) {
                // Without a baseline `execute` refuses it, so this costs a
                // removal, never a wrong one.
                tracing::warn!(target: NAME, error = %e, path = %id, "couldn't store evidence");
            }
            // Unsaved or unknown work, and anything not done with, waits for a
            // person.
            let clean = if wt.forgotten {
                evidence.is_clean()
            } else {
                matches!(evidence, Evidence::Clean { .. })
            };
            let confirm = self.cfg.confirm || proposed_as == ProposedAs::Unmerged || !clean;
            if !wt.forgotten {
                ctx.claim(dir.clone());
            }
            candidates.push(
                Candidate::new(id.clone(), label.clone(), bytes, used, CLASS).confirm(confirm),
            );
            if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Planned, NAME, id)
                        .label(label)
                        .bytes(bytes)
                        .age(age),
                );
            }
        }
        expire_baselines(ctx, now);
        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let mut outcomes = Vec::with_capacity(batch.len());
        for c in batch {
            let outcome = match self.revalidate(ctx, c) {
                Err(reason) => Outcome::Skipped {
                    id: c.id.clone(),
                    reason,
                },
                Ok((wt, embedded)) => {
                    let p = ctx.platform;
                    // Measured now: another adapter may have removed a
                    // `target/` inside it since it was planned.
                    let bytes = p.dir_size(&wt.dir).unwrap_or(c.bytes);
                    match p.remove_dir_all(&wt.dir) {
                        Ok(()) => {
                            // The checkouts are gone either way; a leftover
                            // admin directory only lists a missing worktree,
                            // which `git worktree prune` clears.
                            let admins = (!wt.forgotten)
                                .then_some(&wt)
                                .into_iter()
                                .chain(&embedded)
                                .map(|w| &w.link.admin);
                            for admin in admins {
                                if let Err(e) = p.remove_dir_all(admin) {
                                    tracing::warn!(
                                        target: NAME,
                                        error = %e,
                                        admin = %admin.display(),
                                        "removed the checkout but not its admin directory"
                                    );
                                }
                            }
                            let _ = ctx.kv.delete(&baseline_key(&c.id));
                            Outcome::Removed {
                                id: c.id.clone(),
                                bytes,
                            }
                        }
                        Err(e) => Outcome::Failed {
                            id: c.id.clone(),
                            error: e.to_string(),
                        },
                    }
                }
            };
            record_outcome(ctx, c, &outcome);
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// `path` with the home directory written as `~`.
fn tilde(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A duration in whole days, or hours under a day.
fn humanize(d: Duration) -> String {
    let hours = d.as_secs() / 3600;
    if hours >= 24 {
        format!("{}d", hours / 24)
    } else {
        format!("{hours}h")
    }
}

/// Record a `Kept` decision (guarded so lean builds pay nothing).
fn record_kept(ctx: &Ctx, dir: &Path, age: Option<Duration>, reason: String) {
    if DecisionLog::compiled() {
        let mut d = Decision::new(DecisionKind::Kept, NAME, dir.to_string_lossy()).reason(reason);
        if let Some(age) = age {
            d = d.age(age);
        }
        ctx.decide(d);
    }
}

/// Record the terminal decision for an execute outcome.
fn record_outcome(ctx: &Ctx, c: &Candidate, outcome: &Outcome) {
    if !DecisionLog::compiled() {
        return;
    }
    let (kind, bytes, reason) = match outcome {
        Outcome::Removed { bytes, .. } => (DecisionKind::Deleted, *bytes, None),
        Outcome::Skipped { reason, .. } => (DecisionKind::Skipped, c.bytes, Some(reason.clone())),
        Outcome::Failed { error, .. } => (DecisionKind::Failed, c.bytes, Some(error.clone())),
    };
    let mut d = Decision::new(kind, NAME, c.id.clone())
        .label(c.label.clone())
        .bytes(bytes)
        .age(c.age(ctx.now()));
    if let Some(r) = reason {
        d = d.reason(r);
    }
    ctx.decide(d);
}

#[cfg(test)]
mod tests;
