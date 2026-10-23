//! `disk-saver-adapter-orphaned-worktree` — linked git worktrees whose
//! repository has forgotten them.
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
//! An orphaned checkout can still hold the only copy of someone's work, so this
//! adapter proposes and never disposes on its own say-so. Each candidate is
//! [`Class::UserData`] and carries *evidence*: whether every file in it is
//! already in the repository's object store. Without `confirm = false` every
//! candidate waits for `disk-saver review`; with it, only checkouts whose
//! evidence came back clean are removed automatically.
//!
//! # Phases
//!
//! * **check / observe** — no-op. Age comes from the filesystem.
//! * **plan** — only under pressure. Walk the roots with
//!   [`disk_saver_scan::find_dirs`], which offers dot-named directories too.
//!   A directory matches when its `.git` file names a `<common>/worktrees/<name>`
//!   admin directory that does not exist. A `.git` file pointing elsewhere (a
//!   submodule's `.git/modules/<name>`) is not a worktree; one whose admin
//!   directory exists is still registered and is only recorded as kept. Every
//!   orphan found is [claimed](Ctx::claim), so `git-ignored` leaves it alone.
//!   Orphans idle past the retention policy get the evidence check, and its
//!   result is stored in KV against the path.
//! * **execute** — repeat every check: still a real directory, still orphaned,
//!   still not protected, and evidence no worse than what was proposed (see
//!   `regression`). Then remove it and report the bytes measured just before.
//!
//! # The evidence check
//!
//! With the admin directory gone there is no index and no `HEAD`, so "what is
//! uncommitted" has no answer. What can be answered is whether each file's
//! *content* is in the repository's object store, which is what losing the
//! checkout would cost. The check is one `sh` script (`EVIDENCE_SCRIPT`),
//! given the checkout and `<common>` as positional arguments, never interpolated
//! into the script:
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
//!    filter are hashed as their LFS pointer, which the script computes itself:
//!    running git-lfs's clean filter would copy every untracked file matching
//!    an LFS pattern into `.git/lfs/objects`, gigabytes of writes in a run that
//!    exists because the disk is full. Other files go through `hash-object`
//!    with the repository's filters, which covers line-ending conversion.
//!
//! Whatever is still missing is unsaved work. The script fails closed: any
//! failing step, a count that does not line up, or a file name holding a
//! newline makes the result unknown, which always needs a person.
//!
//! What the check cannot see: ignored files (a local `.env` goes with the
//! checkout), whether a blob is still *reachable* from a ref, and files inside a
//! nested repository that the checkout ignores.
//!
//! # Safety guardrails
//!
//! Never proposed or removed, whatever the config says:
//!
//! * anything under a `.claude` or `.codex` directory, or under `~/Documents/Codex`
//!   (the Codex app's task workspace) — agent-managed worktrees;
//! * a checkout holding a non-empty `.claude/worktrees` or `.codex/worktrees`;
//! * a checkout the repository still lists under another admin directory
//!   (`git worktree repair` can relink it), or one containing a worktree the
//!   repository still lists;
//! * anything another adapter claimed first.
//!
//! The shared walker supplies the `.disk-saver-keep` sentinel, the denylist, no
//! symlink following, and staying on one filesystem.
//!
//! # Config (`[adapters.orphaned-worktree]`)
//!
//! | Key | Default | |
//! |---|---|---|
//! | `roots` | `["~"]` | directories to walk |
//! | `exclude` | `[]` | absolute-path globs to skip |
//! | `max_depth` | `8` | levels below each root |
//! | `max_age` | `"14d"` | idle time before a checkout is proposed |
//! | `min_age` | `"3d"` | floor under scavenge; lowers itself to a shorter `max_age` |
//! | `confirm` | `true` | `false` removes checkouts with clean evidence without asking |

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, CommandOutput, CommandSpec, ConfigCx,
    ConfigError, Configured, Ctx, Decision, DecisionKind, DecisionLog, FileKind, Outcome, Platform,
    Pressure, RetentionPolicy,
};
use disk_saver_scan::{FsConfig, KEEP_SENTINEL, find_dirs};
use globset::GlobSet;
use serde::{Deserialize, Serialize};

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "orphaned-worktree";

/// Every candidate is somebody's checkout: gone is gone.
const CLASS: Class = Class::UserData;

const DAY: u64 = 24 * 60 * 60;

/// Normal-mode threshold when the config names none.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(14 * DAY);

/// Scavenge floor when the config names none. Days rather than the hour the
/// build-artifact adapters use: what goes here is a whole checkout, not
/// something a rebuild restores.
const DEFAULT_MIN_AGE: Duration = Duration::from_secs(3 * DAY);

/// Directories where coding agents keep worktrees they manage. Nothing under
/// one is ever touched, and neither is a checkout that holds one.
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

/// The evidence check, run as `sh -c EVIDENCE_SCRIPT sh <checkout> <common>`.
///
/// Prints `checked <n>` (regular files hashed), then one line per path whose
/// content is not in the object store — `missing <blob> <path>` for a file,
/// `nested <path>/` for a nested repository. Exits 3 if it cannot start, 4 if
/// any step fails. See the module docs for why each step is there.
///
/// Every intermediate result goes to a temporary file whose writer's exit
/// status is checked, rather than through a pipeline whose early stages could
/// fail unseen: a truncated list would read as fewer unsaved files, which is
/// the one direction this must never err in. That matters most when the disk
/// is full, which is when this runs.
const EVIDENCE_SCRIPT: &str = r#"set -u
wt=$1 common=$2
unset GIT_COMMON_DIR GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
export GIT_DIR="$common" GIT_WORK_TREE="$wt" GIT_OPTIONAL_LOCKS=0
export GIT_INDEX_FILE=/nonexistent/disk-saver-empty-index
cd "$wt" || exit 3
tmp=$(mktemp -d) || exit 3
trap 'rm -f "$tmp"/*; rmdir "$tmp"' EXIT
lines() { n=$(wc -l < "$1") || exit 4; echo $((n)); }
git -c core.fsmonitor=false ls-files -z --others --exclude-standard > "$tmp/all" || exit 4
xargs -0 sh -c 'nl="
"
for f do
  case $f in *"$nl"*) echo "a file name contains a newline" >&2; exit 1;; esac
  if [ -L "$f" ]; then :
  elif [ -f "$f" ]; then
    if [ -s "$f" ]; then printf "%s\n" "$f" || exit 1; fi
  elif [ -d "$f" ]; then printf "%s\n" "$f" >&3 || exit 1
  fi
done' sh < "$tmp/all" > "$tmp/files" 3> "$tmp/nested" || exit 4
n=$(lines "$tmp/files")
git hash-object --no-filters --stdin-paths < "$tmp/files" > "$tmp/raw" || exit 4
git cat-file --batch-check='%(objectname)' < "$tmp/raw" > "$tmp/rawfound" || exit 4
[ "$(lines "$tmp/raw")" -eq "$n" ] && [ "$(lines "$tmp/rawfound")" -eq "$n" ] || exit 4
paste "$tmp/rawfound" "$tmp/files" > "$tmp/rawpairs" || exit 4
awk -F'\t' '$1 ~ / missing$/ { sub(/^[^\t]*\t/, ""); print }' "$tmp/rawpairs" > "$tmp/miss" || exit 4
tr '\n' '\0' < "$tmp/miss" > "$tmp/miss0" || exit 4
git check-attr -z --stdin filter < "$tmp/miss0" > "$tmp/attr0" || exit 4
tr '\0' '\n' < "$tmp/attr0" > "$tmp/attr" || exit 4
: > "$tmp/lfs" && : > "$tmp/conv" || exit 4
awk -v lfs="$tmp/lfs" -v conv="$tmp/conv" 'NR % 3 == 1 { p = $0 } NR % 3 == 0 { if ($0 == "lfs") print p > lfs; else print p > conv }' "$tmp/attr" || exit 4
m=$(lines "$tmp/miss")
[ "$(lines "$tmp/attr")" -eq $((3 * m)) ] && [ $(($(lines "$tmp/lfs") + $(lines "$tmp/conv"))) -eq "$m" ] || exit 4
git hash-object --stdin-paths < "$tmp/conv" > "$tmp/convh" || exit 4
git cat-file --batch-check='%(objectname)' < "$tmp/convh" > "$tmp/convfound" || exit 4
c=$(lines "$tmp/conv")
[ "$(lines "$tmp/convh")" -eq "$c" ] && [ "$(lines "$tmp/convfound")" -eq "$c" ] || exit 4
paste "$tmp/convfound" "$tmp/conv" > "$tmp/convpairs" || exit 4
awk -F'\t' '$1 ~ / missing$/ { h = $1; sub(/ missing$/, "", h); sub(/^[^\t]*\t/, ""); print "missing " h " " $0 }' "$tmp/convpairs" > "$tmp/out" || exit 4
while IFS= read -r f; do
  size=$(wc -c < "$f") || exit 4
  if command -v sha256sum > /dev/null 2>&1; then oid=$(sha256sum < "$f") || exit 4
  else oid=$(shasum -a 256 < "$f") || exit 4
  fi
  oid=${oid%%[!0-9a-f]*}
  [ ${#oid} -eq 64 ] || exit 4
  printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize %d\n' "$oid" $((size)) > "$tmp/pointer" || exit 4
  h=$(git hash-object --stdin < "$tmp/pointer") || exit 4
  git cat-file -e "$h" 2> /dev/null || printf 'missing %s %s\n' "$h" "$f" >> "$tmp/out" || exit 4
done < "$tmp/lfs"
echo "checked $n"
cat "$tmp/out" || exit 4
sed 's/^/nested /' "$tmp/nested" || exit 4
"#;

// ── configuration ────────────────────────────────────────────────────────────

/// `[adapters.orphaned-worktree]`. The keys are [`FsConfig`]'s, with this
/// adapter's own defaults for `max_age` and `confirm`; a flattened `FsConfig`
/// would impose its 30 days and `confirm = false`.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct WorktreeConfig {
    /// Directories to walk. Defaults to `["~"]`.
    roots: Vec<PathBuf>,
    /// Absolute-path globs never walked, proposed or removed.
    exclude: Vec<String>,
    /// Levels below each root to descend. Defaults to `8`.
    max_depth: usize,
    /// Idle time after which a checkout is proposed under normal pressure.
    #[serde(with = "humantime_serde")]
    max_age: Duration,
    /// Floor under scavenge; `None` is [`DEFAULT_MIN_AGE`].
    #[serde(with = "humantime_serde")]
    min_age: Option<Duration>,
    /// Ask before removing even a checkout whose evidence is clean.
    confirm: bool,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        WorktreeConfig {
            roots: vec![PathBuf::from("~")],
            exclude: Vec::new(),
            max_depth: 8,
            max_age: DEFAULT_MAX_AGE,
            min_age: None,
            confirm: true,
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

/// The factory the CLI registry uses to build the `orphaned-worktree` adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

/// Resolve the scavenge floor, validate the thresholds, and compile `exclude`
/// up front so a bad glob is a startup error rather than a failed run.
fn build(cfg: WorktreeConfig, cx: &ConfigCx) -> Result<OrphanedWorktrees, ConfigError> {
    // Our 3-day floor gives way to a shorter `max_age` the user wrote; a
    // `min_age` they wrote above `max_age` is reported.
    let min_age = Configured::from(cfg.min_age)
        .with_default(DEFAULT_MIN_AGE)
        .capped_at(cfg.max_age, cx, "min_age", "max_age")?
        .unwrap_or(DEFAULT_MIN_AGE);
    Ok(OrphanedWorktrees {
        policy: cx.retention(cfg.max_age, min_age)?,
        exclude: cx.globs("exclude", &cfg.exclude)?,
        cfg,
    })
}

/// The `orphaned-worktree` adapter.
struct OrphanedWorktrees {
    cfg: WorktreeConfig,
    policy: RetentionPolicy,
    /// The user's `exclude` globs, re-checked in `execute` so an approval that
    /// predates a new exclude cannot remove what it now covers.
    exclude: GlobSet,
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
/// because a worktree wrongly judged orphaned is a candidate for deletion.
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
struct Listed(HashMap<PathBuf, Vec<PathBuf>>);

impl Listed {
    /// The worktree checkouts `common` lists: each `<common>/worktrees/*/gitdir`
    /// holds the path of a checkout's `.git` file.
    fn of(&mut self, p: &dyn Platform, common: &Path) -> &[PathBuf] {
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
                    normalize(&a.path.join(dotgit))
                        .parent()
                        .map(Path::to_path_buf)
                })
                .collect()
        })
    }
}

/// Why an orphaned checkout must be left alone, if it must.
///
/// Checked in `plan` and again in `execute`, so an approval cannot outlive the
/// reason it would now be refused.
fn protected(p: &dyn Platform, dir: &Path, link: &Link, listed: &mut Listed) -> Option<String> {
    if let Some(agent) = agent_component(dir) {
        return Some(format!("agent-managed: it is under a {agent} directory"));
    }
    let home = p.home_dir();
    if let Some(workspace) = AGENT_WORKSPACES
        .iter()
        .find(|ws| dir.starts_with(home.join(ws)))
    {
        return Some(format!("agent-managed: it is under ~/{workspace}"));
    }
    for agent in AGENT_DIRS {
        let worktrees = dir.join(agent).join("worktrees");
        if p.read_dir(&worktrees).is_ok_and(|e| !e.is_empty()) {
            return Some(format!(
                "it holds agent-managed worktrees in {agent}/worktrees"
            ));
        }
    }
    for checkout in listed.of(p, &link.common) {
        if checkout == dir {
            return Some(format!(
                "{} still lists a worktree here; `git worktree repair` can relink it",
                tilde(&home, link.repo())
            ));
        }
        if checkout.starts_with(dir) {
            return Some(format!(
                "it holds {}, a worktree {} still lists",
                tilde(&home, checkout),
                tilde(&home, link.repo())
            ));
        }
    }
    None
}

/// When the checkout was last used: the newest mtime among its top-level
/// entries, ignoring [`NOT_ACTIVITY`]; failing that, the directory's own.
///
/// `now` if neither can be read: activity we cannot see is treated as recent.
fn last_used(p: &dyn Platform, dir: &Path, now: SystemTime) -> SystemTime {
    let newest = p
        .read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter(|e| !NOT_ACTIVITY.contains(&e.file_name.as_str()))
        .filter_map(|e| p.metadata(&e.path).ok().map(|m| m.modified))
        .max();
    newest
        .or_else(|| p.metadata(dir).ok().map(|m| m.modified))
        .unwrap_or(now)
}

// ── evidence ─────────────────────────────────────────────────────────────────

/// What the evidence check found in one checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    /// Every non-ignored file's content is in the object store.
    Clean {
        /// Regular files hashed.
        checked: u64,
    },
    /// Some content is not.
    Unsaved {
        /// Regular files hashed.
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

/// One path whose content is not in the object store.
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

    /// The line the script printed for it. Identifies the content as well as
    /// the path, so an edit to an already-unsaved file shows up as a change.
    fn key(&self) -> String {
        match self {
            Lost::Repo { path } => format!("nested {path}"),
            Lost::File { path, blob } => format!("missing {blob} {path}"),
        }
    }
}

impl Evidence {
    /// Clean, with at least one file actually checked. A checkout in which the
    /// script found nothing to hash (everything ignored) proved nothing, so it
    /// is not enough to remove one without asking.
    fn is_clean(&self) -> bool {
        matches!(self, Evidence::Clean { checked } if *checked > 0)
    }

    /// The label's account of the evidence.
    fn summary(&self) -> String {
        match self {
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
                    parts.push(format!("{n_files} {} not in git", plural(n_files, "file")));
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
                format!("couldn't check for unsaved work: {reason}")
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

/// Run the evidence check on `dir`, a checkout of the repository at `common`.
fn evidence(p: &dyn Platform, dir: &Path, link: &Link) -> Evidence {
    let home = p.home_dir();
    let unknown = |reason: String| Evidence::Unknown { reason };
    match p.metadata(&link.common) {
        Ok(m) if m.kind == FileKind::Dir => {}
        Ok(_) => {
            return unknown(format!("{} is not a directory", tilde(&home, &link.common)));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return unknown(format!(
                "its repository {} is gone",
                tilde(&home, link.repo())
            ));
        }
        Err(e) => return unknown(format!("couldn't read {}: {e}", tilde(&home, &link.common))),
    }
    if !p
        .metadata(&link.common.join("objects"))
        .is_ok_and(|m| m.kind == FileKind::Dir)
    {
        return unknown(format!(
            "{} is not a git repository",
            tilde(&home, &link.common)
        ));
    }

    match p.run_command(&check_command(dir, &link.common)) {
        Err(e) => unknown(format!("couldn't run the check: {e}")),
        Ok(out) if !out.success() => unknown(failure(&out)),
        Ok(out) => parse_report(&out.stdout_string())
            .unwrap_or_else(|| unknown("the check printed something unexpected".to_string())),
    }
}

/// The evidence check for `dir` against `common`: paths travel as positional
/// parameters, never as shell text.
fn check_command(dir: &Path, common: &Path) -> CommandSpec {
    CommandSpec::new(
        "sh",
        [
            "-c",
            EVIDENCE_SCRIPT,
            "sh",
            &dir.to_string_lossy(),
            &common.to_string_lossy(),
        ],
    )
    .with_timeout(CHECK_TIMEOUT)
}

/// Why the script exited non-zero: its exit code's meaning plus the last line
/// it wrote to stderr (git's `fatal:` line, usually).
fn failure(out: &CommandOutput) -> String {
    let what = match out.status {
        3 => "couldn't start the check".to_string(),
        4 => "the check failed".to_string(),
        status => format!("the check exited with status {status}"),
    };
    let stderr = out.stderr_string();
    match stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(line) => format!("{what}: {}", line.chars().take(200).collect::<String>()),
        None => what,
    }
}

/// Parse the script's report (see [`EVIDENCE_SCRIPT`]). `None` if any line is
/// not one the script prints.
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
    evidence: Recorded,
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

impl OrphanedWorktrees {
    /// Re-check a candidate immediately before removing it. `Err` is the reason
    /// to skip it.
    fn revalidate(&self, ctx: &Ctx, c: &Candidate) -> Result<(), String> {
        let p = ctx.platform;
        let dir = Path::new(&c.id);
        if !dir.is_absolute() {
            return Err(format!("{} is not an absolute path", c.id));
        }
        if let Some(agent) = agent_component(dir) {
            return Err(format!("agent-managed: it is under a {agent} directory"));
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
        let link = read_link(p, dir)
            .ok_or_else(|| "its .git no longer points at a worktree admin directory".to_string())?;
        if !admin_missing(p, &link) {
            return Err("it is registered with its repository again".to_string());
        }
        if let Some(reason) = protected(p, dir, &link, &mut Listed::default()) {
            return Err(reason);
        }
        if last_used(p, dir, ctx.now()) > c.last_used {
            return Err("it has been used since it was proposed".to_string());
        }

        let baseline = match ctx.kv.get::<Baseline>(&baseline_key(&c.id)) {
            Ok(Some(b)) => b,
            Ok(None) | Err(_) => {
                return Err(
                    "no record of the evidence it was proposed with; the next run re-checks it"
                        .to_string(),
                );
            }
        };
        if baseline.label != c.label {
            return Err(
                "its evidence changed after it was proposed; the next run proposes it again"
                    .to_string(),
            );
        }
        match regression(&baseline.evidence, &evidence(p, dir, &link)) {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }
}

impl Adapter for OrphanedWorktrees {
    fn name(&self) -> &'static str {
        NAME
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        // No-op: age comes from the filesystem.
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        if !ctx.pressure.deletes() {
            return Ok(Vec::new());
        }
        let now = ctx.now();
        let home = ctx.platform.home_dir();
        let (roots, opts) =
            self.cfg.as_fs().resolve_walk(ctx.platform).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}"))
            })?;

        // Registered worktrees are not matches, so the walk carries on into
        // them, but each is noted so `why` can say why it was left.
        let registered = RefCell::new(Vec::new());
        let orphans = find_dirs(ctx.platform, &roots, &opts, &|p, entry| match read_link(
            p,
            &entry.path,
        ) {
            Some(link) if admin_missing(p, &link) => true,
            Some(link) => {
                registered.borrow_mut().push((entry.path.clone(), link));
                false
            }
            None => false,
        });
        for (dir, link) in registered.into_inner() {
            record_kept(
                ctx,
                &dir,
                None,
                format!("still registered with {}", tilde(&home, link.repo())),
            );
        }

        let mut listed = Listed::default();
        let mut candidates = Vec::new();
        for dir in orphans {
            // Re-read rather than carry the matcher's result out of the walk;
            // it is one small file.
            let Some(link) = read_link(ctx.platform, &dir) else {
                continue;
            };
            if let Some(owner) = ctx.claims.owner(&dir) {
                record_kept(ctx, &dir, None, format!("claimed by {owner}"));
                continue;
            }
            // Ours whether or not it is proposed: `git-ignored` must not offer
            // up its contents piecemeal.
            ctx.claim(dir.clone());
            if let Some(reason) = protected(ctx.platform, &dir, &link, &mut listed) {
                record_kept(ctx, &dir, None, reason);
                continue;
            }
            let used = last_used(ctx.platform, &dir, now);
            let age = now.duration_since(used).unwrap_or_default();
            if !self.policy.eligible(age, ctx.pressure) {
                let threshold = match ctx.pressure {
                    Pressure::Scavenge { .. } => self.policy.min_age,
                    Pressure::Normal | Pressure::Comfortable => self.policy.max_age,
                };
                record_kept(
                    ctx,
                    &dir,
                    Some(age),
                    format!(
                        "idle {}, under the {} threshold",
                        humanize(age),
                        humanize(threshold)
                    ),
                );
                continue;
            }

            let evidence = evidence(ctx.platform, &dir, &link);
            let bytes = ctx.platform.dir_size(&dir).unwrap_or(0);
            let id = dir.to_string_lossy().into_owned();
            let label = format!(
                "orphaned worktree of {}: {} — {}",
                tilde(&home, link.repo()),
                tilde(&home, &dir),
                evidence.summary()
            );
            let baseline = Baseline {
                label: label.clone(),
                recorded_at: unix(now),
                evidence: Recorded::of(&evidence),
            };
            if let Err(e) = ctx.kv.set(&baseline_key(&id), &baseline, now) {
                // Without a baseline `execute` refuses it, so this costs a
                // removal, never a wrong one.
                tracing::warn!(target: NAME, error = %e, path = %id, "couldn't store evidence");
            }
            // Unsaved or unknown work always waits for a person.
            let confirm = self.cfg.confirm || !evidence.is_clean();
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
                Ok(()) => {
                    let dir = Path::new(&c.id);
                    // Measured now: another adapter may have removed a
                    // `target/` inside it since it was planned.
                    let bytes = ctx.platform.dir_size(dir).unwrap_or(c.bytes);
                    match ctx.platform.remove_dir_all(dir) {
                        Ok(()) => {
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
