//! Shared project/artifact-directory walker for the filesystem adapters
//! (`node-modules`, `rust-target`, `python-cache`).
//!
//! The three filesystem adapters are the same problem with different markers:
//! walk a set of roots, find directories whose deletion is safe (a marker dir
//! next to a required sibling file, passing an extra validity check), and work
//! out how recently the surrounding *project* was active so an age policy can
//! be applied. That shared walk lives here.
//!
//! # Safety guardrails
//!
//! The walk implements the guardrails from `ARCHITECTURE.md` §11.5–8 / §12.2:
//!
//! * A [`KEEP_SENTINEL`] (`.disk-saver-keep`) file in a directory exempts that
//!   whole subtree — nothing under it is matched or descended.
//! * Dot-directories (names starting with `.`) are never descended, but
//!   `<project>/.git/HEAD` is still read to inform `last_active`.
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
/// `.disk-saver-keep` sentinel, dot-directory skipping (with `.git/HEAD` still
/// read for activity), never descending into a matched artifact directory,
/// never following symlinks, `max_depth`, and the `exclude` glob set.
///
/// The project activity time reported on each [`Found`] is:
///
/// ```text
/// project_last_active =
///     max( mtime of each entry directly in project_root, EXCLUDING any
///          artifact directory (a child whose name matches a rule) and the
///          `.git` directory,
///          mtime of project_root/.git/HEAD if present )
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
    /// root's device), plus git activity via `.git/HEAD` (moves on
    /// checkout/switch) and `.git/logs/HEAD` (the reflog — appended on every
    /// commit/reset/fetch, including same-branch commits). Scanning the whole
    /// tree, not just the project root's direct children, is what makes editing a
    /// *nested* source file — the common case — refresh the signal.
    fn last_active(&self, project_root: &Path, entries: &[DirEntry]) -> SystemTime {
        let mut newest: Option<SystemTime> = None;

        // Top level is already read; recurse into non-excluded subdirectories.
        self.scan_activity_entries(entries, self.opts.max_depth, &mut newest);

        // Git activity signals.
        for rel in [["HEAD"].as_slice(), ["logs", "HEAD"].as_slice()] {
            let mut path = project_root.join(".git");
            for comp in rel {
                path = path.join(comp);
            }
            if let Ok(meta) = self.p.metadata(&path)
                && meta.kind == FileKind::File
            {
                bump(&mut newest, meta.modified);
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
            // up via source files) and `.git` (handled via HEAD/logs/HEAD).
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
    /// disk pressure. Defaults to 7 days.
    #[serde(with = "humantime_serde")]
    pub min_age: Duration,
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
            min_age: Duration::from_secs(7 * 24 * 60 * 60),
            confirm: false,
        }
    }
}

impl FsConfig {
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

    #[test]
    fn last_active_reflects_a_same_branch_commit_via_reflog() {
        // HEAD is old (no branch switch), but `.git/logs/HEAD` (the reflog) moved
        // — a commit on the current branch. last_active must reflect it.
        let fake = FakePlatform::new()
            .with_file("~/dev/app/package.json", "{}", t(10))
            .with_sized_dir("~/dev/app/node_modules", 1_200, t(10))
            .with_file("~/dev/app/.git/HEAD", "ref: refs/heads/main", t(10))
            .with_file("~/dev/app/.git/logs/HEAD", "commit ...", t(90));

        let found = find_artifacts(
            &fake,
            &[PathBuf::from("~/dev")],
            &[node_rule()],
            &ScanOptions::default(),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].last_active,
            t(90),
            "same-branch commit must refresh activity"
        );
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
        assert_eq!(c.min_age, Duration::from_secs(7 * 24 * 60 * 60));
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
        // min_age omitted → default 7d.
        assert_eq!(cfg.min_age, Duration::from_secs(7 * 24 * 60 * 60));
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
}
