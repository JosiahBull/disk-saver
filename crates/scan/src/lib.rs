//! Shared project/artifact-directory walker for the filesystem adapters
//! (`node-modules`, `rust-target`, `python-cache`).
//!
//! The three filesystem adapters are the same problem with different markers:
//! walk a set of roots, find directories whose deletion is safe (a marker dir
//! next to a required sibling file, passing an extra validity check), and work
//! out how recently the surrounding *project* was active so an age policy can
//! be applied. That shared walk lives here.
//!
//! Two smaller walks share its guardrails: [`find_dirs`], for directories
//! recognised by a predicate rather than a fixed name (orphaned worktrees,
//! `CACHEDIR.TAG` caches), and [`find_marker_files`], for projects that pin
//! something in a file (`rust-toolchain.toml`) together with their activity.
//!
//! # Safety guardrails
//!
//! The walk implements the guardrails from `ARCHITECTURE.md` §11.5–8 / §12.2:
//!
//! * A [`KEEP_SENTINEL`] (`.disk-saver-keep`) file in a directory exempts that
//!   whole subtree — nothing under it is matched or descended.
//! * Dot-directories (names starting with `.`) are never descended, but
//!   `<project>/.git/HEAD` and its reflog are still read to inform
//!   `last_active`.
//! * A matched artifact directory is never descended into (so a nested
//!   `node_modules` inside a `node_modules` is never reported).
//! * Symlinks are never followed — only real directories are descended, and a
//!   symlink named like an artifact dir is not treated as one.
//! * `max_depth` bounds the walk and an [`exclude`](ScanOptions::exclude)
//!   glob set (matched against absolute paths) prunes both subtrees and
//!   individual artifact directories.
//!
//! * The walk stays on a single filesystem: it captures each root's device
//!   (`FileMeta::dev`) and never descends onto a different one, so a mount point
//!   nested under a root is not traversed and its artifact dirs are never found.
//!   Sizing and deletion are independently device-confined by the [`Platform`]
//!   implementation's `dir_size` / `remove_dir_all`.
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_platform::{DirEntry, FileKind, Platform};

/// Filename that, when present in a directory, exempts that directory and its
/// entire subtree from every filesystem adapter.
pub const KEEP_SENTINEL: &str = ".disk-saver-keep";

/// A single artifact-matching rule.
///
/// A directory is a match for this rule when it contains a child directory
/// named [`artifact_dir`](Rule::artifact_dir), every file named in
/// [`required_sibling`](Rule::required_sibling) is present directly alongside
/// it, and [`validate`](Rule::validate) returns `true` for the artifact
/// directory.
#[derive(Clone, Copy)]
pub struct Rule {
    /// The marker directory to delete, e.g. `"node_modules"`, `"target"`,
    /// `"__pycache__"`.
    pub artifact_dir: &'static str,
    /// Sibling files that must exist directly in the project root for the match
    /// to count (e.g. `["package.json"]`). An empty slice means none required.
    pub required_sibling: &'static [&'static str],
    /// Extra validity check run against the absolute path of the candidate
    /// artifact directory (e.g. "contains a `CACHEDIR.TAG`"). Returning `false`
    /// rejects the match.
    pub validate: fn(&dyn Platform, &Path) -> bool,
}

/// One matched artifact directory, ready for an adapter to age-filter and
/// (possibly) delete.
#[derive(Debug, Clone)]
pub struct Found {
    /// The [`Rule::artifact_dir`] of the rule that matched.
    pub rule: &'static str,
    /// Absolute path of the directory to delete.
    pub artifact_dir: PathBuf,
    /// Absolute path of the directory holding the marker/sibling (the project).
    pub project_root: PathBuf,
    /// Apparent size of `artifact_dir` from
    /// [`Platform::dir_size`] (`0` if it could not be measured).
    pub size: u64,
    /// How recently the surrounding project was active (see
    /// [`find_artifacts`] for the exact definition).
    pub last_active: SystemTime,
}

/// Knobs for a walk.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// How many directory levels below each root to descend (root itself is
    /// depth 0). Defaults to `8`.
    pub max_depth: usize,
    /// Absolute-path globs to exclude. Matching a directory prunes its whole
    /// subtree; matching an artifact directory drops that result. An empty set
    /// excludes nothing.
    pub exclude: globset::GlobSet,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_depth: 8,
            exclude: globset::GlobSet::empty(),
        }
    }
}

/// Walk every root and return one [`Found`] per matched artifact directory.
///
/// Honors all guardrails documented on the [crate root](crate): the
/// `.disk-saver-keep` sentinel, dot-directory skipping (with `.git/HEAD` and
/// the reflog still read for activity), never descending into a matched
/// artifact directory, never following symlinks, `max_depth`, and the
/// `exclude` glob set.
///
/// The project activity time reported on each [`Found`] is:
///
/// ```text
/// project_last_active =
///     max( mtime of each entry directly in project_root, EXCLUDING any
///          artifact directory (a child whose name matches a rule) and the
///          `.git` directory,
///          mtime of project_root/.git/HEAD if present,
///          newest entry time in project_root/.git/logs/HEAD if present )
/// ```
///
/// The artifact directory's own mtime is deliberately ignored (a build touches
/// it, but the build shows up via source-file mtimes anyway). If a project has
/// no qualifying entries at all, the project root's own mtime is used as a
/// floor.
pub fn find_artifacts(
    p: &dyn Platform,
    roots: &[PathBuf],
    rules: &[Rule],
    opts: &ScanOptions,
) -> Vec<Found> {
    let rule_names: HashSet<&str> = rules.iter().map(|r| r.artifact_dir).collect();
    let mut walk = Walk {
        p,
        rules,
        rule_names: &rule_names,
        opts,
        root_dev: None,
        out: Vec::new(),
    };
    for root in roots {
        // Capture the root's device so the walk can stay on one filesystem
        // (ARCHITECTURE.md §11.8). `None` (root unreadable) disables the check;
        // deletion is separately device-confined by `Platform::remove_dir_all`.
        walk.root_dev = p.metadata(root).map(|m| m.dev).ok();
        walk.run(root, 0);
    }
    walk.out
}

/// Walk every root and return each directory `matches` accepts.
///
/// For when the thing to find is not a fixed artifact name next to a sibling
/// file — "a checkout whose `.git` file points nowhere", "a directory carrying a
/// `CACHEDIR.TAG`". The same guardrails as [`find_artifacts`] apply: the
/// `.disk-saver-keep` sentinel (in a walked directory *or* in the matched one),
/// `max_depth`, the `exclude` set, no symlinks, and no crossing onto another
/// filesystem.
///
/// One difference is deliberate: `matches` is also offered dot-named
/// directories. Agents and scripts park worktrees in names like `.wt-feature`,
/// and those are exactly what a matcher may be looking for. The walk still never
/// *descends* into a dot-directory, and never descends into a match, so a match
/// nested inside another match is not reported.
pub fn find_dirs(
    p: &dyn Platform,
    roots: &[PathBuf],
    opts: &ScanOptions,
    matches: &dyn Fn(&dyn Platform, &DirEntry) -> bool,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        let root_dev = p.metadata(root).map(|m| m.dev).ok();
        walk_dirs(p, root, 0, opts, root_dev, matches, &mut out);
    }
    out
}

/// The recursive body of [`find_dirs`].
fn walk_dirs(
    p: &dyn Platform,
    dir: &Path,
    depth: usize,
    opts: &ScanOptions,
    root_dev: Option<u64>,
    matches: &dyn Fn(&dyn Platform, &DirEntry) -> bool,
    out: &mut Vec<PathBuf>,
) {
    if opts.exclude.is_match(dir) {
        return;
    }
    let Ok(entries) = p.read_dir(dir) else {
        return;
    };
    if entries.iter().any(|e| e.file_name == KEEP_SENTINEL) {
        return;
    }
    for entry in &entries {
        if entry.kind != FileKind::Dir || opts.exclude.is_match(&entry.path) {
            continue;
        }
        // Another filesystem is neither reported nor walked (§11.8).
        if let Some(root_dev) = root_dev
            && p.metadata(&entry.path).map(|m| m.dev).ok() != Some(root_dev)
        {
            continue;
        }
        if matches(p, entry) {
            if p.metadata(&entry.path.join(KEEP_SENTINEL)).is_err() {
                out.push(entry.path.clone());
            }
            continue;
        }
        if depth < opts.max_depth && !entry.file_name.starts_with('.') {
            walk_dirs(p, &entry.path, depth + 1, opts, root_dev, matches, out);
        }
    }
}

/// A project holding one of the marker files [`find_marker_files`] looks for.
#[derive(Debug, Clone)]
pub struct Marked {
    /// Absolute path of the marker file, e.g. `<project>/rust-toolchain.toml`.
    pub file: PathBuf,
    /// The directory holding it.
    pub project_root: PathBuf,
    /// How recently the project was active, computed exactly as
    /// [`Found::last_active`] is (see [`find_artifacts`]).
    pub last_active: SystemTime,
}

/// What [`find_marker_files`] found, and where it could not look.
#[derive(Debug, Clone, Default)]
pub struct MarkerScan {
    /// Every marker file found, with its project's activity.
    pub marked: Vec<Marked>,
    /// Directories the walk could not read (any error but not-found). A pin in
    /// one was not seen, so an adapter protecting what pins name should treat
    /// a non-empty list as an incomplete scan.
    pub unreadable: Vec<PathBuf>,
    /// Directories at `max_depth` that hold subdirectories the walk did not
    /// enter. Kept apart from `unreadable` because a depth limit is a choice,
    /// shared by every walk, and is hit routinely (vendored chart trees, deep
    /// fixtures) where no project root lives.
    pub too_deep: Vec<PathBuf>,
}

/// Walk every root and return one [`Marked`] per file named in `names`, with
/// its project's activity.
///
/// For an adapter that needs to know which projects *pin* something — a
/// `rust-toolchain.toml`, an `.nvmrc` — and whether anyone still works on them.
/// Unlike [`find_artifacts`] the walk keeps descending below a match, so a
/// nested project with its own pin is found too. `ignore` names directories
/// whose mtimes do not count as activity and that are never descended — the
/// build output a project regenerates (`target`, `node_modules`), which would
/// otherwise make the activity walk both slow and wrong.
///
/// This walk *protects* rather than deletes, so two guardrails flip. The
/// `.disk-saver-keep` sentinel is not honoured: a kept project's pin should
/// still protect what it names. And a marker that is a symlink is reported
/// (the walk still never descends through one): reading it is harmless, while
/// missing it would leave its toolchain unprotected. The other guardrails —
/// `max_depth`, `exclude`, dot-directories, staying on one filesystem — apply.
/// Where the walk could not read, or stopped at `max_depth`, is listed in
/// [`MarkerScan::unreadable`] and [`MarkerScan::too_deep`].
pub fn find_marker_files(
    p: &dyn Platform,
    roots: &[PathBuf],
    names: &[&str],
    ignore: &[&'static str],
    opts: &ScanOptions,
) -> MarkerScan {
    let ignored: HashSet<&str> = ignore.iter().copied().collect();
    let mut walk = Walk {
        p,
        rules: &[],
        rule_names: &ignored,
        opts,
        root_dev: None,
        out: Vec::new(),
    };
    let mut scan = MarkerScan::default();
    for root in roots {
        walk.root_dev = p.metadata(root).map(|m| m.dev).ok();
        walk.run_markers(root, 0, names, &mut scan);
    }
    scan
}

/// Mutable walk state, so the recursive helper stays a single method rather
/// than a function with a long argument list.
struct Walk<'a> {
    p: &'a dyn Platform,
    rules: &'a [Rule],
    rule_names: &'a HashSet<&'a str>,
    opts: &'a ScanOptions,
    /// Device id of the current root; the walk never descends onto a different
    /// filesystem. `None` disables the check (the root could not be stat'd).
    root_dev: Option<u64>,
    out: Vec<Found>,
}

impl Walk<'_> {
    /// [`find_marker_files`]'s walk: report every marker file in `dir`, then
    /// descend into its real, non-dot, non-ignored subdirectories.
    fn run_markers(&self, dir: &Path, depth: usize, names: &[&str], scan: &mut MarkerScan) {
        if self.opts.exclude.is_match(dir) {
            return;
        }
        let entries = match self.p.read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {
                scan.unreadable.push(dir.to_path_buf());
                return;
            }
        };
        let mut last_active = None;
        for entry in &entries {
            let file_like = matches!(entry.kind, FileKind::File | FileKind::Symlink);
            if file_like && names.contains(&entry.file_name.as_str()) {
                let when = *last_active.get_or_insert_with(|| self.last_active(dir, &entries));
                scan.marked.push(Marked {
                    file: entry.path.clone(),
                    project_root: dir.to_path_buf(),
                    last_active: when,
                });
            }
        }
        for entry in &entries {
            if entry.kind != FileKind::Dir
                || entry.file_name.starts_with('.')
                || self.rule_names.contains(entry.file_name.as_str())
                || self.opts.exclude.is_match(&entry.path)
            {
                continue;
            }
            if let Some(root_dev) = self.root_dev
                && self.p.metadata(&entry.path).map(|m| m.dev).ok() != Some(root_dev)
            {
                continue;
            }
            if depth >= self.opts.max_depth {
                scan.too_deep.push(dir.to_path_buf());
                return;
            }
            self.run_markers(&entry.path, depth + 1, names, scan);
        }
    }

    /// Examine `dir` (at `depth` below its root) for matches and recurse.
    fn run(&mut self, dir: &Path, depth: usize) {
        // Excluded subtree: prune entirely.
        if self.opts.exclude.is_match(dir) {
            tracing::debug!(path = %dir.display(), "scan: skipping excluded directory");
            return;
        }

        let entries = match self.p.read_dir(dir) {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!(path = %dir.display(), error = %err, "scan: unreadable directory");
                return;
            }
        };

        // Opt-out sentinel: exempt this whole subtree.
        if entries.iter().any(|e| e.file_name == KEEP_SENTINEL) {
            tracing::debug!(path = %dir.display(), "scan: honoring .disk-saver-keep sentinel");
            return;
        }

        // Find matches. `matched_names` collects every matched artifact name
        // (even excluded ones) so we never descend into them; `reportable`
        // holds the matches that survive the exclude check.
        let mut matched_names: Vec<&str> = Vec::new();
        let mut reportable: Vec<(&Rule, PathBuf)> = Vec::new();
        for rule in self.rules {
            let Some(artifact) = entries
                .iter()
                .find(|e| e.file_name == rule.artifact_dir && e.kind == FileKind::Dir)
            else {
                continue;
            };
            let siblings_present = rule
                .required_sibling
                .iter()
                .all(|sib| entries.iter().any(|e| &e.file_name == sib));
            if !siblings_present {
                continue;
            }
            if !(rule.validate)(self.p, &artifact.path) {
                continue;
            }
            matched_names.push(rule.artifact_dir);
            if self.opts.exclude.is_match(&artifact.path) {
                tracing::debug!(path = %artifact.path.display(), "scan: excluding matched artifact dir");
                continue;
            }
            reportable.push((rule, artifact.path.clone()));
        }

        if !reportable.is_empty() {
            let last_active = self.last_active(dir, &entries);
            for (rule, artifact_dir) in reportable {
                let size = self.p.dir_size(&artifact_dir).unwrap_or(0);
                self.out.push(Found {
                    rule: rule.artifact_dir,
                    artifact_dir,
                    project_root: dir.to_path_buf(),
                    size,
                    last_active,
                });
            }
        }

        // Descend, honoring depth, dot-dirs, symlinks, and matched artifacts.
        if depth >= self.opts.max_depth {
            return;
        }
        for entry in &entries {
            // Only real directories are descended (symlinks are never
            // followed, files are irrelevant).
            if entry.kind != FileKind::Dir {
                continue;
            }
            if entry.file_name.starts_with('.') {
                continue;
            }
            if matched_names.iter().any(|n| *n == entry.file_name) {
                continue;
            }
            // Never descend onto a different filesystem (a mount point nested
            // under the root, §11.8): an artifact dir on a mounted volume must
            // not be found, planned, or deleted.
            if let Some(root_dev) = self.root_dev
                && self.p.metadata(&entry.path).map(|m| m.dev).ok() != Some(root_dev)
            {
                tracing::debug!(path = %entry.path.display(),
                    "scan: skipping directory on a different filesystem");
                continue;
            }
            self.run(&entry.path, depth + 1);
        }
    }

    /// Compute `project_last_active` for `project_root` (see [`find_artifacts`]).
    ///
    /// This is the newest mtime of anything the developer plausibly touched:
    /// every file and directory in the project tree (bounded by `max_depth`,
    /// excluding artifact dirs, `.git`, and dot-directories, staying on the
    /// root's device), plus git activity via `.git/HEAD`'s mtime (moves on
    /// checkout/switch) and the newest entry in `.git/logs/HEAD` (the reflog —
    /// appended on every commit/reset/merge, including same-branch commits; its
    /// mtime is ignored because `git gc` rewrites it). Scanning the whole
    /// tree, not just the project root's direct children, is what makes editing a
    /// *nested* source file — the common case — refresh the signal.
    fn last_active(&self, project_root: &Path, entries: &[DirEntry]) -> SystemTime {
        let mut newest: Option<SystemTime> = None;

        // Top level is already read; recurse into non-excluded subdirectories.
        self.scan_activity_entries(entries, self.opts.max_depth, &mut newest);

        // Git activity signals.
        let git = project_root.join(".git");
        if let Ok(meta) = self.p.metadata(&git.join("HEAD"))
            && meta.kind == FileKind::File
        {
            bump(&mut newest, meta.modified);
        }
        // The reflog's mtime is not activity: `git gc` (including our own
        // `git-gc` adapter) expires entries and rewrites the file even when
        // nothing moved, so use the newest entry's recorded time instead.
        let reflog = git.join("logs").join("HEAD");
        if let Ok(meta) = self.p.metadata(&reflog)
            && meta.kind == FileKind::File
        {
            match self.p.read_to_string(&reflog) {
                // Every entry expired: no activity in the expiry window.
                Ok(text) if text.trim().is_empty() => {}
                Ok(text) => bump(
                    &mut newest,
                    newest_reflog_entry(&text).unwrap_or(meta.modified),
                ),
                // Unreadable/unparseable falls back to the mtime: overstating
                // activity only delays a deletion.
                Err(_) => bump(&mut newest, meta.modified),
            }
        }

        newest.unwrap_or_else(|| {
            self.p
                .metadata(project_root)
                .map(|m| m.modified)
                .unwrap_or(UNIX_EPOCH)
        })
    }

    /// Fold the mtimes of `entries` (and, recursively, their non-excluded
    /// subdirectories) into `newest`. `budget` bounds the descent depth.
    fn scan_activity(&self, dir: &Path, budget: usize, newest: &mut Option<SystemTime>) {
        if let Ok(entries) = self.p.read_dir(dir) {
            self.scan_activity_entries(&entries, budget, newest);
        }
    }

    /// The shared body of [`scan_activity`] operating on already-read `entries`.
    fn scan_activity_entries(
        &self,
        entries: &[DirEntry],
        budget: usize,
        newest: &mut Option<SystemTime>,
    ) {
        for entry in entries {
            // Ignore artifact directories (builds touch them; their effect shows
            // up via source files) and `.git` (handled via HEAD and the reflog).
            if entry.file_name == ".git" || self.rule_names.contains(entry.file_name.as_str()) {
                continue;
            }
            let Ok(meta) = self.p.metadata(&entry.path) else {
                continue;
            };
            // Never let another filesystem's mtimes count (stay on the root device).
            if let Some(root_dev) = self.root_dev
                && meta.dev != root_dev
            {
                continue;
            }
            bump(newest, meta.modified);
            // Recurse into real, non-dot subdirectories within the depth budget.
            if entry.kind == FileKind::Dir && budget > 0 && !entry.file_name.starts_with('.') {
                self.scan_activity(&entry.path, budget - 1, newest);
            }
        }
    }
}

/// The time recorded on the last parseable line of a reflog. Each line is
/// `<old-oid> <new-oid> <name> <<email>> <unix-secs> <tz>[\t<message>]`, and
/// git appends, so the last line is the newest entry.
fn newest_reflog_entry(text: &str) -> Option<SystemTime> {
    text.lines().rev().find_map(|line| {
        let header = line.split('\t').next()?;
        let (_, when) = header.rsplit_once('>')?;
        let secs: u64 = when.split_whitespace().next()?.parse().ok()?;
        UNIX_EPOCH.checked_add(Duration::from_secs(secs))
    })
}

/// Update `newest` to `t` if `t` is later (or `newest` is unset).
fn bump(newest: &mut Option<SystemTime>, t: SystemTime) {
    if newest.is_none_or(|cur| t > cur) {
        *newest = Some(t);
    }
}

/// Shared configuration for the three filesystem adapters.
///
/// This is a plain serde type deliberately *not* tied to the core config
/// machinery, so `disk-saver-scan` need not depend on `disk-saver-core`. Each
/// adapter deserializes its opaque config table into this (or a wrapper that
/// flattens it), then builds a retention policy from `max_age`/`min_age`.
///
/// `~` in `roots` is expanded by the adapter (via the platform's home dir), not
/// here.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct FsConfig {
    /// Directories to scan. Defaults to `["~"]`.
    pub roots: Vec<PathBuf>,
    /// Absolute-path glob patterns to exclude from the scan.
    pub exclude: Vec<String>,
    /// Maximum directory depth to descend below each root. Defaults to `8`.
    pub max_depth: usize,
    /// Normal-mode age threshold: items unused at least this long are eligible.
    /// Defaults to 30 days.
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
    /// Scavenge floor: items younger than this are never deleted, whatever the
    /// disk pressure. `None` when the config did not name one, in which case the
    /// adapter picks the floor — usually [`FsConfig::MIN_AGE`].
    ///
    /// That default is short on purpose. Everything these adapters delete is a
    /// build artifact that regenerates from sources already on disk, so under
    /// real pressure the only cost of being wrong is a rebuild. `git-gc` repacks
    /// a live repo instead and wants a longer floor, which is why the choice is
    /// left to the adapter rather than baked in here.
    #[serde(with = "humantime_serde")]
    pub min_age: Option<Duration>,
    /// Whether deletions should be routed to the approvals queue for
    /// confirmation rather than run automatically. Defaults to `false`.
    pub confirm: bool,
}

impl Default for FsConfig {
    fn default() -> Self {
        FsConfig {
            roots: vec![PathBuf::from("~")],
            exclude: Vec::new(),
            max_depth: 8,
            max_age: Duration::from_secs(30 * 24 * 60 * 60),
            min_age: None,
            confirm: false,
        }
    }
}

impl FsConfig {
    /// The scavenge floor adapters use when [`min_age`](FsConfig::min_age) is
    /// `None` — short, because everything these adapters delete regenerates from
    /// sources already on disk. An adapter that deletes something dearer than a
    /// build artifact names its own instead.
    pub const MIN_AGE: Duration = Duration::from_secs(60 * 60);

    /// Compile [`exclude`](FsConfig::exclude) into a [`globset::GlobSet`].
    ///
    /// Returns the underlying [`globset::Error`] on the first bad pattern; it
    /// intentionally does not depend on the core `ConfigError` type.
    pub fn glob_set(&self) -> Result<globset::GlobSet, globset::Error> {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.exclude {
            builder.add(globset::Glob::new(pattern)?);
        }
        builder.build()
    }

    /// Resolve this config into the concrete `(roots, ScanOptions)` for a walk:
    /// `~`-expand each root against the platform home, drop any root at or under
    /// the built-in [`denylist`](FsConfig::denylist), and build the exclude set
    /// from the user globs *plus* the denylist (glob-escaped, so paths with
    /// metacharacters still match). Shared by every roots-based adapter so the
    /// denylist and escaping stay consistent.
    pub fn resolve_walk(
        &self,
        platform: &dyn Platform,
    ) -> Result<(Vec<PathBuf>, ScanOptions), globset::Error> {
        let home = platform.home_dir();
        let denylist = Self::denylist(platform);
        let roots: Vec<PathBuf> = self
            .roots
            .iter()
            .map(|r| expand_tilde(r, &home))
            .filter(|r| !denylist.iter().any(|d| r == d || r.starts_with(d)))
            .collect();

        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.exclude {
            builder.add(globset::Glob::new(pattern)?);
        }
        for dir in &denylist {
            let base = globset::escape(&dir.to_string_lossy());
            builder.add(globset::Glob::new(&base)?);
            builder.add(globset::Glob::new(&format!("{base}/**"))?);
        }
        let exclude = builder.build()?;
        Ok((
            roots,
            ScanOptions {
                max_depth: self.max_depth,
                exclude,
            },
        ))
    }

    /// The built-in denylist: directories a scan must never enter even when a
    /// root (e.g. the default `~`) contains them — the OS/user data areas that
    /// hold no reclaimable dev artifacts. `~/Library`, `~/.Trash`, and the user
    /// cache dir.
    pub fn denylist(platform: &dyn Platform) -> Vec<PathBuf> {
        let home = platform.home_dir();
        vec![
            home.join("Library"),
            home.join(".Trash"),
            platform.user_cache_dir(),
        ]
    }
}

/// Expand a leading `~` component of `path` against `home` (scan keeps its own
/// copy to avoid depending on `disk-saver-core`).
fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    let mut comps = path.components();
    if let Some(std::path::Component::Normal(first)) = comps.next()
        && first == "~"
    {
        return home.join(comps.as_path());
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_saver_platform::FakePlatform;

    /// `t(days)` → a fixed `SystemTime` `days` days after the epoch.
    fn t(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    /// A rule that always validates (used by most tests).
    fn node_rule() -> Rule {
        Rule {
            artifact_dir: "node_modules",
            required_sibling: &["package.json"],
            validate: |_, _| true,
        }
    }

    fn opts_with_depth(depth: usize) -> ScanOptions {
        ScanOptions {
            max_depth: depth,
            exclude: globset::GlobSet::empty(),
        }
    }

    fn opts_excluding(patterns: &[&str]) -> ScanOptions {
        let mut b = globset::GlobSetBuilder::new();
        for p in patterns {
            b.add(globset::Glob::new(p).unwrap());
        }
        ScanOptions {
            max_depth: 8,
            exclude: b.build().unwrap(),
        }
    }

    #[test]
    fn finds_artifact_dir_with_required_sibling() {
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_200, t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );

        assert_eq!(found.len(), 1);
        let f = &found[0];
        assert_eq!(f.rule, "node_modules");
        assert_eq!(
            f.artifact_dir,
            PathBuf::from("/home/tester/dev/app/node_modules")
        );
        assert_eq!(f.project_root, PathBuf::from("/home/tester/dev/app"));
        assert_eq!(f.size, 1_200);
    }

    #[test]
    fn does_not_cross_a_filesystem_boundary() {
        // A stale, valid node_modules living on a mount nested under the scanned
        // root must NOT be found (ARCHITECTURE.md §11.8): the walk stays on the
        // root's device.
        let fake = FakePlatform::new()
            .with_dir("~/dev", t(10))
            .with_mount("~/dev/archive")
            .with_file("~/dev/archive/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/archive/app/node_modules", 5_000, t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert!(
            found.is_empty(),
            "artifact dirs on a mounted volume must not be found, got {found:?}"
        );

        // Sanity: the same layout on the SAME device IS found.
        let same_dev = FakePlatform::new()
            .with_dir("~/dev", t(10))
            .with_file("~/dev/archive/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/archive/app/node_modules", 5_000, t(10));
        let found = find_artifacts(
            &same_dev,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1, "same-device artifact dir is found");
    }

    #[test]
    fn last_active_reflects_a_nested_file_edit() {
        // Project root + its direct children are old, but a deeply nested source
        // file was just edited. last_active must be the nested file's mtime, so
        // the project is not judged idle (§12.2).
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_200, t(10))
            .with_file("~/dev/app/src/deep/mod/foo.js", "code", t(100));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].last_active,
            t(100),
            "nested edit must refresh activity"
        );
    }

    /// One reflog line recording a HEAD move at `when`.
    fn reflog_line(when: SystemTime, msg: &str) -> String {
        let secs = when.duration_since(UNIX_EPOCH).unwrap().as_secs();
        format!(
            "{} {} A U Thor <a@example.com> {secs} +1300\t{msg}\n",
            "0".repeat(40),
            "1".repeat(40)
        )
    }

    /// A project whose sources and `.git/HEAD` were last touched at day 10,
    /// with `.git/logs/HEAD` holding `reflog` and last written at `mtime`.
    fn project_with_reflog(reflog: impl Into<Vec<u8>>, mtime: SystemTime) -> SystemTime {
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_200, t(10))
            .with_file("~/dev/app/.git/HEAD", "ref: refs/heads/main", t(10))
            .with_file("~/dev/app/.git/logs/HEAD", reflog, mtime);
        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1);
        found[0].last_active
    }

    #[test]
    fn last_active_reflects_a_same_branch_commit_via_reflog() {
        // HEAD is old (no branch switch), but the reflog gained an entry — a
        // commit on the current branch. last_active must reflect it.
        let reflog = reflog_line(t(5), "clone") + &reflog_line(t(90), "commit: fix");
        assert_eq!(
            project_with_reflog(reflog, t(90)),
            t(90),
            "same-branch commit must refresh activity"
        );
    }

    #[test]
    fn last_active_ignores_a_reflog_rewritten_by_gc() {
        // `git gc` rewrote the reflog at day 300 without adding an entry; the
        // newest entry (day 50) is the real activity, not the rewrite.
        let reflog = reflog_line(t(5), "clone") + &reflog_line(t(50), "commit: fix");
        assert_eq!(project_with_reflog(reflog, t(300)), t(50));
    }

    #[test]
    fn last_active_ignores_a_reflog_emptied_by_expiry() {
        // gc expired every entry, leaving an empty file with a fresh mtime.
        assert_eq!(project_with_reflog("", t(300)), t(10));
    }

    #[test]
    fn last_active_falls_back_to_reflog_mtime_when_unparseable() {
        // Content we can't read a time from is treated as activity at its mtime,
        // the conservative direction (it only delays a deletion).
        assert_eq!(project_with_reflog("garbage\n", t(90)), t(90));
        assert_eq!(project_with_reflog(vec![0xff, 0xfe], t(90)), t(90));
    }

    #[test]
    fn newest_reflog_entry_parses_git_lines() {
        let at = |secs| Some(UNIX_EPOCH + Duration::from_secs(secs));
        let oids = format!("{} {}", "a".repeat(40), "b".repeat(40));
        // Last line wins; a name with spaces and a message with `>` are fine.
        let text = format!(
            "{oids} Jo Q Public <jo@x> 100 +0000\tcommit: a > b\n\
             {oids} Jo Q Public <jo@x> 200 -0700\tcheckout: moving from x to y\n"
        );
        assert_eq!(newest_reflog_entry(&text), at(200));
        // No message (git omits the tab), CRLF, and a corrupt trailing line.
        let text = format!("{oids} J <j@x> 300 +0000\r\nnot a reflog line\n");
        assert_eq!(newest_reflog_entry(&text), at(300));
        assert_eq!(newest_reflog_entry("junk\n"), None);
        assert_eq!(newest_reflog_entry(""), None);
    }

    #[test]
    fn ignores_artifact_dir_without_required_sibling() {
        // node_modules present but no package.json sibling.
        let fake = FakePlatform::new().with_sized_dir("~/dev/app/node_modules", 1_200, t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn ignores_artifact_dir_failing_validate() {
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_200, t(10));

        let never = Rule {
            artifact_dir: "node_modules",
            required_sibling: &["package.json"],
            validate: |_, _| false,
        };

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[never],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn validate_receives_artifact_dir_path() {
        // A validate closure that only accepts the artifact dir when it holds a
        // sentinel file (mirrors the rust-target CACHEDIR.TAG check).
        fn has_tag(p: &dyn Platform, dir: &Path) -> bool {
            p.metadata(&dir.join("CACHEDIR.TAG")).is_ok()
        }
        let rule = Rule {
            artifact_dir: "target",
            required_sibling: &["Cargo.toml"],
            validate: has_tag,
        };

        // Project WITH the tag → found.
        let with_tag = FakePlatform::new()
            .with_file("~/dev/rs/Cargo.toml", "[package]", t(5))
            .with_sized_dir("~/dev/rs/target", 42, t(5))
            .with_file("~/dev/rs/target/CACHEDIR.TAG", "Signature", t(5));
        let found = find_artifacts(
            &with_tag,
            &[PathBuf::from("~/dev")],
            &[rule],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].size, 42);

        // Project WITHOUT the tag → skipped.
        let no_tag = FakePlatform::new()
            .with_file("~/dev/rs/Cargo.toml", "[package]", t(5))
            .with_sized_dir("~/dev/rs/target", 42, t(5));
        let found = find_artifacts(
            &no_tag,
            &[PathBuf::from("~/dev")],
            &[rule],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn does_not_descend_into_matched_artifact_dir() {
        // Top-level project plus a nested project *inside* node_modules; the
        // nested one must never be discovered.
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_000, t(10))
            // nested valid project hidden inside node_modules
            .with_file("~/dev/app/node_modules/inner/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules/inner/node_modules", 500, t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );

        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].artifact_dir,
            PathBuf::from("/home/tester/dev/app/node_modules")
        );
    }

    #[test]
    fn honors_disk_saver_keep_sentinel() {
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_000, t(10))
            .with_file("~/dev/app/.disk-saver-keep", "", t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn sentinel_exempts_entire_subtree() {
        // Sentinel at ~/dev exempts the nested project under it.
        let fake = FakePlatform::new()
            .with_file("~/dev/.disk-saver-keep", "", t(10))
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_000, t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn honors_exclude_glob_on_artifact_dir() {
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_000, t(10));

        // Control: no exclude → found.
        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1);

        // With exclude on the artifact dir → nothing.
        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &opts_excluding(&["**/node_modules"]),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn honors_exclude_glob_pruning_subtree() {
        let fake = FakePlatform::new()
            .with_file("~/dev/skip/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/skip/app/node_modules", 1_000, t(10))
            .with_file("~/dev/keep/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/keep/app/node_modules", 2_000, t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &opts_excluding(&["**/skip"]),
        );

        // Only the project outside the pruned subtree survives.
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].project_root,
            PathBuf::from("/home/tester/dev/keep/app")
        );
    }

    #[test]
    fn last_active_reflects_newest_source_and_git_head() {
        // Newest non-artifact source file (day 250) beats git HEAD (day 200);
        // the artifact dir's own mtime (day 900) is ignored.
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(20))
            .with_file("~/dev/app/main.js", "code", t(250))
            .with_file("~/dev/app/old.txt", "x", t(50))
            .with_sized_dir("~/dev/app/node_modules", 1_000, t(900))
            .with_file("~/dev/app/.git/HEAD", "ref: refs/heads/main", t(200));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].last_active, t(250));

        // Now let git HEAD be the most recent signal → it wins.
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(20))
            .with_file("~/dev/app/main.js", "code", t(100))
            .with_sized_dir("~/dev/app/node_modules", 1_000, t(900))
            .with_file("~/dev/app/.git/HEAD", "ref: refs/heads/main", t(400));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].last_active, t(400));
    }

    #[test]
    fn symlinked_dir_is_not_followed() {
        // A real project under `real/`, plus a symlink `link` → `real`.
        // Walking must find the project once via the real path, never via the
        // symlink.
        let fake = FakePlatform::new()
            .with_file("~/root/real/app/package.json", "{}", t(10))
            .with_sized_dir("~/root/real/app/node_modules", 1_000, t(10))
            .with_symlink("~/root/link", "~/root/real", t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/root")],
            &[node_rule()],
            &ScanOptions::default(),
        );

        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].artifact_dir,
            PathBuf::from("/home/tester/root/real/app/node_modules")
        );
    }

    #[test]
    fn symlink_named_like_artifact_is_not_a_match() {
        // `node_modules` here is a symlink, not a real dir → not an artifact.
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_dir("~/other/real_modules", t(10))
            .with_symlink("~/dev/app/node_modules", "~/other/real_modules", t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn respects_max_depth() {
        // project_root is at depth 3 below the root (~/code=0, a=1, b=2, proj=3).
        let build = || {
            FakePlatform::new()
                .with_file("~/code/a/b/proj/package.json", "{}", t(10))
                .with_sized_dir("~/code/a/b/proj/node_modules", 1_000, t(10))
        };

        // Deep enough → found.
        let fake = build();
        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/code")],
            &[node_rule()],
            &opts_with_depth(3),
        );
        assert_eq!(found.len(), 1);

        // Too shallow → not found.
        let fake = build();
        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/code")],
            &[node_rule()],
            &opts_with_depth(2),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn skips_dot_dirs_during_descent() {
        // A project buried inside a dot-dir is never descended into.
        let fake = FakePlatform::new()
            .with_file("~/dev/.hidden/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/.hidden/app/node_modules", 1_000, t(10));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn multiple_rules_and_roots() {
        let py_rule = Rule {
            artifact_dir: "__pycache__",
            required_sibling: &[],
            validate: |_, _| true,
        };
        let fake = FakePlatform::new()
            // node project under ~/a
            .with_file("~/a/js/package.json", "{}", t(10))
            .with_sized_dir("~/a/js/node_modules", 100, t(10))
            // python cache under ~/b (no sibling required)
            .with_file("~/b/py/mod.py", "x", t(10))
            .with_sized_dir("~/b/py/__pycache__", 200, t(10));

        let mut found = find_artifacts(
            &fake,
            &[PathBuf::from("~/a"), PathBuf::from("~/b")],
            &[node_rule(), py_rule],
            &ScanOptions::default(),
        );
        found.sort_by(|a, b| a.rule.cmp(b.rule));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].rule, "__pycache__");
        assert_eq!(found[0].size, 200);
        assert_eq!(found[1].rule, "node_modules");
        assert_eq!(found[1].size, 100);
    }

    #[test]
    fn unreadable_root_is_skipped() {
        let fake = FakePlatform::new();
        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/does/not/exist")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert!(found.is_empty());
    }

    // ── FsConfig ────────────────────────────────────────────────────────

    #[test]
    fn fsconfig_defaults() {
        let c = FsConfig::default();
        assert_eq!(c.roots, vec![PathBuf::from("~")]);
        assert!(c.exclude.is_empty());
        assert_eq!(c.max_depth, 8);
        assert_eq!(c.max_age, Duration::from_secs(30 * 24 * 60 * 60));
        // Deliberately short — build artifacts regenerate from sources on disk.
        assert_eq!(c.min_age, None);
        assert_eq!(FsConfig::MIN_AGE, Duration::from_secs(60 * 60));
        assert!(!c.confirm);
    }

    #[test]
    fn fsconfig_deserializes_partial_toml_with_humantime() {
        let cfg: FsConfig = toml::from_str(
            r#"
            roots = ["~/dev", "~/work"]
            exclude = ["**/vendor/**"]
            max_depth = 4
            max_age = "45d"
            confirm = true
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.roots,
            vec![PathBuf::from("~/dev"), PathBuf::from("~/work")]
        );
        assert_eq!(cfg.exclude, vec!["**/vendor/**".to_string()]);
        assert_eq!(cfg.max_depth, 4);
        assert_eq!(cfg.max_age, Duration::from_secs(45 * 24 * 60 * 60));
        // min_age omitted → left to the adapter, which uses `MIN_AGE`.
        assert_eq!(cfg.min_age, None);
        assert!(cfg.confirm);
    }

    #[test]
    fn fsconfig_empty_toml_is_defaults() {
        let cfg: FsConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.roots, vec![PathBuf::from("~")]);
        assert_eq!(cfg.max_depth, 8);
    }

    #[test]
    fn glob_set_compiles_and_matches() {
        let cfg = FsConfig {
            exclude: vec!["**/node_modules".to_string(), "**/target".to_string()],
            ..FsConfig::default()
        };
        let set = cfg.glob_set().unwrap();
        assert!(set.is_match("/home/tester/dev/app/node_modules"));
        assert!(set.is_match("/home/tester/dev/rs/target"));
        assert!(!set.is_match("/home/tester/dev/app/src"));
    }

    #[test]
    fn glob_set_empty_matches_nothing() {
        let set = FsConfig::default().glob_set().unwrap();
        assert!(!set.is_match("/anything/at/all"));
    }

    #[test]
    fn glob_set_reports_bad_pattern() {
        let cfg = FsConfig {
            exclude: vec!["[".to_string()],
            ..FsConfig::default()
        };
        assert!(cfg.glob_set().is_err());
    }

    /// A `find_dirs` matcher: "has a `.git` file", i.e. a linked worktree.
    fn has_git_file(p: &dyn Platform, e: &DirEntry) -> bool {
        p.metadata(&e.path.join(".git"))
            .is_ok_and(|m| m.kind == FileKind::File)
    }

    #[test]
    fn find_dirs_offers_dot_dirs_to_the_matcher_but_never_descends_them() {
        let fake = FakePlatform::new()
            .with_file("~/work/.wt-feature/.git", "gitdir: x", t(10))
            .with_file("~/work/app/.git", "gitdir: y", t(10))
            .with_file("~/work/.hidden/nested/.git", "gitdir: z", t(10));

        let mut found = find_dirs(
            &fake,
            &[PathBuf::from("~/work")],
            &ScanOptions::default(),
            &has_git_file,
        );
        found.sort();

        assert_eq!(
            found,
            vec![
                PathBuf::from("/home/tester/work/.wt-feature"),
                PathBuf::from("/home/tester/work/app"),
            ]
        );
    }

    #[test]
    fn find_dirs_does_not_descend_into_a_match() {
        let fake = FakePlatform::new()
            .with_file("~/work/outer/.git", "gitdir: x", t(10))
            .with_file("~/work/outer/inner/.git", "gitdir: y", t(10));

        let found = find_dirs(
            &fake,
            &[PathBuf::from("~/work")],
            &ScanOptions::default(),
            &has_git_file,
        );

        assert_eq!(found, vec![PathBuf::from("/home/tester/work/outer")]);
    }

    #[test]
    fn find_dirs_honours_sentinel_exclude_depth_mounts_and_symlinks() {
        let fake = FakePlatform::new()
            .with_file("~/work/kept/.disk-saver-keep", "", t(10))
            .with_file("~/work/kept/wt/.git", "gitdir: a", t(10))
            .with_file("~/work/self-kept/.git", "gitdir: b", t(10))
            .with_file("~/work/self-kept/.disk-saver-keep", "", t(10))
            .with_file("~/work/excluded/wt/.git", "gitdir: c", t(10))
            .with_file("~/work/a/b/c/deep/.git", "gitdir: d", t(10))
            .with_mount("~/work/mnt")
            .with_file("~/work/mnt/wt/.git", "gitdir: e", t(10))
            .with_file("~/elsewhere/wt/.git", "gitdir: f", t(10))
            .with_symlink("~/work/link", "~/elsewhere", t(10));
        let mut opts = opts_excluding(&["/home/tester/work/excluded"]);
        opts.max_depth = 2;

        let found = find_dirs(&fake, &[PathBuf::from("~/work")], &opts, &has_git_file);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn find_marker_files_reports_nested_pins_with_project_activity() {
        let fake = FakePlatform::new()
            .with_file("~/dev/app/rust-toolchain.toml", "", t(10))
            .with_file("~/dev/app/src/main.rs", "", t(40))
            .with_sized_dir("~/dev/app/target", 1_000, t(90))
            .with_file("~/dev/app/target/debug/rust-toolchain", "", t(90))
            .with_file("~/dev/app/libs/inner/rust-toolchain", "", t(20))
            .with_file("~/dev/.hidden/rust-toolchain", "", t(20));

        let scan = find_marker_files(
            &fake,
            &[PathBuf::from("~/dev")],
            &["rust-toolchain", "rust-toolchain.toml"],
            &["target"],
            &ScanOptions::default(),
        );
        assert!(
            scan.unreadable.is_empty() && scan.too_deep.is_empty(),
            "{scan:?}"
        );
        let mut found = scan.marked;
        found.sort_by(|a, b| a.file.cmp(&b.file));

        let files: Vec<_> = found.iter().map(|m| m.file.clone()).collect();
        assert_eq!(
            files,
            vec![
                PathBuf::from("/home/tester/dev/app/libs/inner/rust-toolchain"),
                PathBuf::from("/home/tester/dev/app/rust-toolchain.toml"),
            ]
        );
        // The outer project's edit to src/ counts; its target/ does not.
        assert_eq!(found[1].project_root, PathBuf::from("/home/tester/dev/app"));
        assert_eq!(found[1].last_active, t(40));
        assert_eq!(found[0].last_active, t(20));
    }

    #[test]
    fn find_marker_files_reports_kept_and_symlinked_pins() {
        // A sentinel exempts a project from deletion, not from protecting what
        // its pin names; a symlinked pin still names a toolchain.
        let fake = FakePlatform::new()
            .with_file("~/dev/kept/.disk-saver-keep", "", t(10))
            .with_file("~/dev/kept/rust-toolchain", "1.90.0", t(10))
            .with_file("~/shared/rust-toolchain", "1.91.0", t(10))
            .with_symlink(
                "~/dev/linked/rust-toolchain",
                "~/shared/rust-toolchain",
                t(10),
            );

        let scan = find_marker_files(
            &fake,
            &[PathBuf::from("~/dev")],
            &["rust-toolchain"],
            &[],
            &ScanOptions::default(),
        );
        let mut files: Vec<_> = scan.marked.into_iter().map(|m| m.file).collect();
        files.sort();

        assert_eq!(
            files,
            vec![
                PathBuf::from("/home/tester/dev/kept/rust-toolchain"),
                PathBuf::from("/home/tester/dev/linked/rust-toolchain"),
            ]
        );
    }

    #[test]
    fn find_marker_files_lists_where_depth_stopped_it() {
        let fake = FakePlatform::new()
            .with_file("~/dev/a/rust-toolchain", "1.90.0", t(10))
            .with_file("~/dev/a/b/c/rust-toolchain", "1.91.0", t(10))
            .with_file("~/dev/leaf/rust-toolchain", "1.92.0", t(10));

        let scan = find_marker_files(
            &fake,
            &[PathBuf::from("~/dev")],
            &["rust-toolchain"],
            &[],
            &opts_with_depth(1),
        );

        // `a` was read (its pin found) but `a/b` was not descended; `leaf` has
        // no subdirectories, so stopping there missed nothing.
        assert_eq!(scan.marked.len(), 2);
        assert_eq!(scan.too_deep, vec![PathBuf::from("/home/tester/dev/a")]);
    }
}
