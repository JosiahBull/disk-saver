//! `disk-saver-adapter-cachedir-tag` — the `cachedir-tag` adapter, the "generic
//! `CACHEDIR.TAG` sweeper" of ARCHITECTURE.md §21.
//!
//! A `CACHEDIR.TAG` file (<https://bford.info/cachedir/>) is a tool's promise
//! that the directory holding it is a cache: everything in it can be
//! regenerated, and a backup program may skip it. Cargo writes one into every
//! `target/`, virtualenv and uv into every environment they create, uv into its
//! cache. This adapter walks explicitly configured roots — `~/.cache` by
//! default — and proposes the tagged directories it finds once nothing has been
//! written to them for longer than the retention policy allows.
//!
//! It exists for what the specialised adapters cannot see. `rust-target`
//! recognises a `target/` by the `Cargo.toml` beside it, so a cargo target
//! directory living outside any project (`CARGO_TARGET_DIR=~/.cache/foo`, a
//! container building into a bind mount) never matches; and the project walks
//! never enter a dot-directory like `~/.cache` anyway. The directory that
//! prompted this adapter was 13 GB.
//!
//! # What is not proposed
//!
//! A tagged directory the walk finds is still left alone, with a `kept`
//! decision saying why, when it is:
//!
//! * **claimed** by an adapter planned earlier in the run (§12.8), at or under
//!   the claimed path. The registry plans this adapter after `node-modules`,
//!   `rust-target` and `python-cache`, which judge their artifacts by project
//!   activity — better evidence than a tag.
//! * **a Python virtualenv** (it holds a `pyvenv.cfg`). uv tags every venv it
//!   makes and virtualenv does too, but a project's `.venv` is opt-in even for
//!   `python-cache` (§20.3), and the hook environments under
//!   `~/.cache/pre-commit` come back only through a slow reinstall.
//! * **made by a tool in `skip_creators`** (`["uv"]` by default). The `uv`
//!   adapter prunes uv's cache through uv itself, under uv's lock; deleting it
//!   underneath a running uv could corrupt it. The creator comes from the tag's
//!   comment (`… created by cargo.`), matched case-insensitively against the
//!   whole name or any word of it. uv's own tag is the bare signature with no
//!   comment, so a directory holding uv's versioned cache buckets
//!   (`archive-v0`, `wheels-v7`, …) counts as made by uv.
//! * **the home directory or one of its ancestors**, whatever a tag there says.
//!
//! A configured root that is itself tagged is never a candidate, only what the
//! walk finds below it. Nor is a tagged directory inside another tagged one:
//! the walk does not descend into a match.
//!
//! # Age
//!
//! A candidate's `last_used` is the newest modification time among the
//! directory itself, its children and its grandchildren. Two levels, because
//! that is where these caches show activity: cargo adds files under
//! `target/debug/{deps,incremental,build}` on every build, which bumps those
//! directories' mtimes, while `target/` and `target/debug` change only when a
//! new top-level entry appears. A full recursive walk of a multi-gigabyte cache
//! on every run would cost far more than it adds. Access times are never used:
//! listing a directory updates its access time, so this adapter's own probe
//! would make every cache look in use. A directory the probe cannot list is
//! kept, not guessed at.
//!
//! # Configuration
//!
//! ```toml
//! [adapters.cachedir-tag]
//! roots = ["~/.cache"]     # walked for tagged directories
//! exclude = []             # absolute-path globs pruned from the walk
//! max_depth = 3            # directory levels read below each root
//! max_age = "30d"          # normal mode: idle at least this long
//! min_age = "1d"           # scavenge floor
//! confirm = false          # true routes candidates to `disk-saver review`
//! skip_creators = ["uv"]   # tools whose caches are left to them
//! ```
//!
//! See [`CachedirTagConfig`] for why each default is what it is.
//!
//! # Phases
//!
//! * **check / observe** — no-op. Age comes from the filesystem, so there is no
//!   KV state.
//! * **plan** — under [`Pressure::Comfortable`] return nothing without walking.
//!   Otherwise walk the roots with [`disk_saver_scan::find_dirs`], skip what is
//!   listed above, claim the rest (so `git-ignored` defers to this adapter's
//!   better age signal), and propose what is idle past the policy as
//!   [`Class::Cache`]. Only proposed directories are sized: sizing everything
//!   found would walk uv's whole cache every run just to skip it.
//! * **execute** — re-check everything immediately before deleting: still under
//!   a configured root and not excluded or sentinelled, still a real directory
//!   with a valid tag, not a virtualenv, not a skipped creator's, not claimed,
//!   and not modified since `last_used` was planned. Any failure yields
//!   [`Outcome::Skipped`] with the reason. An approval can sit in the queue for
//!   days; what it approved must still be true when it runs.
//!
//! # Safety guardrails
//!
//! The `.disk-saver-keep` sentinel, the exclude globs, `max_depth`, never
//! following symlinks and never crossing onto another filesystem are enforced
//! by [`disk_saver_scan::find_dirs`]. On top of the user's `exclude`, this crate
//! always excludes `~/Library`, `~/.Trash` and the platform trash directories.
//! Unlike the project adapters it does *not* exclude the user cache directory:
//! on Linux that is `~/.cache`, the default root, which is why the walk is not
//! built with `FsConfig::resolve_walk`.

#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Claims, Class, ConfigCx, ConfigError, Ctx,
    Decision, DecisionKind, DecisionLog, DirEntry, FileKind, Outcome, Platform, Pressure,
    RetentionPolicy, expand_tilde,
};
use disk_saver_scan::{KEEP_SENTINEL, ScanOptions, find_dirs};
use globset::{Glob, GlobSetBuilder};
use serde::Deserialize;

/// The stable adapter name (config section, KV bucket, log target).
const NAME: &str = "cachedir-tag";

/// Every candidate is a cache: regenerable by the tag's promise, but by a tool
/// this adapter may know nothing about, at whatever cost that tool pays.
const CLASS: Class = Class::Cache;

/// The tag's file name, fixed by the specification.
const TAG_FILE: &str = "CACHEDIR.TAG";

/// The 43 bytes a valid tag must begin with. Anything may follow.
const SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// The largest tag file read. Real tags are a few hundred bytes; this only
/// stops a pathological file from being read into memory on every run.
const MAX_TAG_LEN: u64 = 64 * 1024;

/// Every Python virtualenv has one at its top level (PEP 405).
const VENV_MARKER: &str = "pyvenv.cfg";

/// The longest creator name kept from a tag comment, in characters.
const MAX_CREATOR_CHARS: usize = 40;

/// uv's cache buckets, each of which it names `<bucket>-v<N>` and renames when
/// its format changes. uv's tag carries no comment, so these are how its cache
/// is recognised.
const UV_BUCKETS: &[&str] = &[
    "archive",
    "binaries",
    "builds",
    "environments",
    "flat-index",
    "git",
    "interpreter",
    "python",
    "sdists",
    "simple",
    "wheels",
];

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Why a directory the age probe could not list is kept: no age is a guess.
const UNDATABLE: &str = "could not list its contents to tell when it was last written";

/// `[adapters.cachedir-tag]`.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CachedirTagConfig {
    /// Directories to walk, `~` expanded. Defaults to `["~/.cache"]`.
    ///
    /// Not `["~"]`: project trees belong to the specialised adapters, which
    /// judge by project activity, and they are full of tagged `.venv`s and
    /// `target/`s that would otherwise be judged here by a weaker signal.
    pub roots: Vec<PathBuf>,
    /// Absolute-path globs to exclude. Matching a directory prunes its whole
    /// subtree. Defaults to none.
    pub exclude: Vec<String>,
    /// How many directory levels below each root the walk reads. With the
    /// default of 3 a tag is found as deep as `~/.cache/a/b/c/tagged`, which
    /// covers pre-commit's `~/.cache/pre-commit/<repo>/py_env-*` with room to
    /// spare, while keeping the walk cheap in a directory that can hold
    /// millions of files.
    pub max_depth: usize,
    /// Normal-mode threshold: a cache idle at least this long is proposed.
    /// Defaults to 30 days.
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
    /// Scavenge floor: a cache written to more recently than this is never
    /// proposed. Defaults to 1 day — longer than the hour the project adapters
    /// use, because whatever regenerates a tagged cache may not be a quick
    /// rebuild from sources already on disk, and still short enough that a
    /// build cache abandoned yesterday can go when space is short.
    #[serde(with = "humantime_serde")]
    pub min_age: Duration,
    /// Route candidates to the approvals queue instead of deleting them.
    /// Defaults to `false`: the tag is the tool's own statement that the
    /// contents are disposable.
    pub confirm: bool,
    /// Tools whose tagged directories are never proposed, matched
    /// case-insensitively against the creator a tag names, or any word of it
    /// (`"virtualenv"` matches `created by Python virtualenv`). Defaults to
    /// `["uv"]`: the `uv` adapter prunes uv's cache through uv's own lock.
    pub skip_creators: Vec<String>,
}

impl Default for CachedirTagConfig {
    fn default() -> Self {
        CachedirTagConfig {
            roots: vec![PathBuf::from("~/.cache")],
            exclude: Vec::new(),
            max_depth: 3,
            max_age: 30 * DAY,
            min_age: DAY,
            confirm: false,
            skip_creators: vec!["uv".to_owned()],
        }
    }
}

/// The factory the CLI registry uses to build the `cachedir-tag` adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

/// Validate the config, so a mistake fails at startup rather than mid-run.
fn build(cfg: CachedirTagConfig, cx: &ConfigCx) -> Result<CachedirTag, ConfigError> {
    let policy = cx.retention(cfg.max_age, cfg.min_age)?;
    cx.globs("exclude", &cfg.exclude)?;

    // A relative root would be resolved against whatever directory the
    // scheduler starts us in, which is never what was meant.
    if let Some(root) = cfg
        .roots
        .iter()
        .find(|r| !r.is_absolute() && !r.starts_with("~"))
    {
        return Err(cx.err(format!(
            "root '{}' must be absolute or start with '~'",
            root.display()
        )));
    }

    let skip_creators: Vec<String> = cfg
        .skip_creators
        .iter()
        .map(|c| c.trim().to_lowercase())
        .collect();
    if skip_creators.iter().any(String::is_empty) {
        return Err(cx.err("skip_creators entries must not be empty"));
    }

    Ok(CachedirTag {
        policy,
        skip_creators,
        cfg,
    })
}

/// The `cachedir-tag` adapter.
struct CachedirTag {
    cfg: CachedirTagConfig,
    policy: RetentionPolicy,
    /// `cfg.skip_creators`, trimmed and lowercased once.
    skip_creators: Vec<String>,
}

/// What a valid tag says about its directory.
struct Tagged {
    /// The tool that made it, from the tag's comment or its layout.
    creator: Option<String>,
    /// Whether it is a Python virtualenv.
    venv: bool,
}

/// The roots to walk and the walk's options, resolved against the platform.
struct Scope {
    roots: Vec<PathBuf>,
    opts: ScanOptions,
}

impl CachedirTag {
    /// Expand the roots and compile the user's excludes together with the
    /// built-in ones.
    fn scope(&self, p: &dyn Platform) -> Result<Scope, AdapterError> {
        let home = p.home_dir();
        let roots = self
            .cfg
            .roots
            .iter()
            .map(|r| expand_tilde(r, &home))
            .collect();

        let mut builder = GlobSetBuilder::new();
        for pattern in &self.cfg.exclude {
            builder.add(Glob::new(pattern).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("invalid exclude glob '{pattern}': {e}"))
            })?);
        }
        for dir in built_in_excludes(p) {
            // Escaped, so a home like `/data/foo[old]` still matches literally.
            let base = globset::escape(&dir.to_string_lossy());
            for pattern in [base.clone(), format!("{base}/**")] {
                builder.add(Glob::new(&pattern).map_err(|e| {
                    AdapterError::Failed(anyhow::anyhow!("invalid exclude glob '{pattern}': {e}"))
                })?);
            }
        }
        let exclude = builder
            .build()
            .map_err(|e| AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}")))?;

        Ok(Scope {
            roots,
            opts: ScanOptions {
                max_depth: self.cfg.max_depth,
                exclude,
            },
        })
    }

    /// Why a tagged directory is not this adapter's to delete, if it is not.
    /// Checked while planning and again just before deleting.
    fn refusal(&self, claims: &Claims, home: &Path, dir: &Path, tag: &Tagged) -> Option<String> {
        if home.starts_with(dir) {
            return Some("the home directory or one of its ancestors".to_owned());
        }
        if let Some(owner) = claims.owner(dir) {
            return Some(format!("claimed by {owner}"));
        }
        if tag.venv {
            return Some(format!(
                "a Python virtualenv ({VENV_MARKER}); virtualenvs are opt-in, \
                 see python-cache's include_venvs"
            ));
        }
        if let Some(creator) = &tag.creator
            && self.skips(creator)
        {
            return Some(format!(
                "created by {creator}, which skip_creators leaves to that tool"
            ));
        }
        None
    }

    /// Whether `creator` is named by `skip_creators`, whole or by any word.
    fn skips(&self, creator: &str) -> bool {
        let creator = creator.to_lowercase();
        self.skip_creators
            .iter()
            .any(|s| creator == *s || creator.split_whitespace().any(|w| w == s))
    }

    /// The threshold `pressure` applies, and its config key, for kept reasons.
    fn threshold(&self, pressure: Pressure) -> (&'static str, Duration) {
        match pressure {
            Pressure::Scavenge { .. } => ("min_age", self.policy.min_age),
            Pressure::Normal | Pressure::Comfortable => ("max_age", self.policy.max_age),
        }
    }

    /// Re-check a candidate immediately before deleting it, returning the reason
    /// to skip on the first check that fails.
    fn still_valid(
        &self,
        ctx: &Ctx,
        scope: &Scope,
        home: &Path,
        dir: &Path,
        planned_last_used: SystemTime,
    ) -> Result<(), String> {
        let p = ctx.platform;
        if !is_plain_absolute(dir) {
            return Err(format!("{} is not a plain absolute path", dir.display()));
        }
        scope.reachable(p, dir)?;
        let tag = inspect(p, dir)?;
        if let Some(reason) = self.refusal(ctx.claims, home, dir, &tag) {
            return Err(reason);
        }
        match newest_mtime(p, dir) {
            None => Err(UNDATABLE.to_owned()),
            Some(newest) if newest > planned_last_used => Err(format!(
                "written to since it was planned (newest mtime {})",
                humantime::format_rfc3339_seconds(newest)
            )),
            Some(_) => Ok(()),
        }
    }
}

impl Adapter for CachedirTag {
    fn name(&self) -> &'static str {
        NAME
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        // No-op: age derives from the filesystem, so there is no KV state.
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        if !ctx.pressure.deletes() {
            return Ok(Vec::new());
        }

        let scope = self.scope(ctx.platform)?;
        let home = ctx.platform.home_dir();
        let now = ctx.now();

        let mut found = find_dirs(ctx.platform, &scope.roots, &scope.opts, &is_tagged);
        // Overlapping roots find the same directory twice.
        found.sort();
        found.dedup();

        let mut candidates = Vec::new();
        for dir in found {
            let id = dir.to_string_lossy().into_owned();

            // The walk saw a valid tag a moment ago; it may have gone since.
            let tag = match inspect(ctx.platform, &dir) {
                Ok(tag) => tag,
                Err(reason) => {
                    keep(ctx, &id, &label(&home, &dir, None), None, reason);
                    continue;
                }
            };
            let label = label(&home, &dir, tag.creator.as_deref());

            if let Some(reason) = self.refusal(ctx.claims, &home, &dir, &tag) {
                keep(ctx, &id, &label, None, reason);
                continue;
            }

            // Ours from here, proposed or kept: `git-ignored` must not offer it
            // up on the strength of its own, weaker age signal.
            ctx.claim(dir.clone());

            let Some(last_used) = newest_mtime(ctx.platform, &dir) else {
                keep(ctx, &id, &label, None, UNDATABLE.to_owned());
                continue;
            };
            let age = now.duration_since(last_used).unwrap_or_default();

            if !self.policy.eligible(age, ctx.pressure) {
                let (key, threshold) = self.threshold(ctx.pressure);
                let reason = format!("idle {}, below {key} of {}", human(age), human(threshold));
                keep(ctx, &id, &label, Some(age), reason);
                continue;
            }

            let bytes = match ctx.platform.dir_size(&dir) {
                Ok(bytes) => bytes,
                Err(e) => {
                    keep(
                        ctx,
                        &id,
                        &label,
                        Some(age),
                        format!("could not size it: {e}"),
                    );
                    continue;
                }
            };

            if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Planned, NAME, id.clone())
                        .label(label.clone())
                        .bytes(bytes)
                        .age(age),
                );
            }
            candidates
                .push(Candidate::new(id, label, bytes, last_used, CLASS).confirm(self.cfg.confirm));
        }

        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let scope = self.scope(ctx.platform)?;
        let home = ctx.platform.home_dir();

        let mut outcomes = Vec::with_capacity(batch.len());
        for c in batch {
            let dir = Path::new(&c.id);
            let outcome = match self.still_valid(ctx, &scope, &home, dir, c.last_used) {
                Err(reason) => Outcome::Skipped {
                    id: c.id.clone(),
                    reason,
                },
                Ok(()) => match ctx.platform.remove_dir_all(dir) {
                    Ok(()) => Outcome::Removed {
                        id: c.id.clone(),
                        bytes: c.bytes,
                    },
                    Err(e) => Outcome::Failed {
                        id: c.id.clone(),
                        error: e.to_string(),
                    },
                },
            };
            record_outcome(ctx, c, &outcome);
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

impl Scope {
    /// Whether a walk of the current scope could still reach `dir`: strictly
    /// below some root, with no excluded directory and no `.disk-saver-keep`
    /// between that root and `dir`, inclusive. A config change made while an
    /// approval waited in the queue applies to it.
    fn reachable(&self, p: &dyn Platform, dir: &Path) -> Result<(), String> {
        let mut reason = format!("{} is no longer under a configured root", dir.display());
        for root in self
            .roots
            .iter()
            .filter(|r| dir != r.as_path() && dir.starts_with(r))
        {
            let blocked = dir
                .ancestors()
                .take_while(|a| a.starts_with(root))
                .find_map(|a| {
                    if self.opts.exclude.is_match(a) {
                        Some(format!("{} is excluded", a.display()))
                    } else if p.metadata(&a.join(KEEP_SENTINEL)).is_ok() {
                        Some(format!("{} holds a {KEEP_SENTINEL}", a.display()))
                    } else {
                        None
                    }
                });
            match blocked {
                None => return Ok(()),
                Some(r) => reason = r,
            }
        }
        Err(reason)
    }
}

/// Directories never walked, whatever the roots: macOS's per-user application
/// data and the trash, which the `trash` adapter owns and asks about.
fn built_in_excludes(p: &dyn Platform) -> Vec<PathBuf> {
    let home = p.home_dir();
    let mut dirs = vec![home.join("Library"), home.join(".Trash")];
    dirs.extend(p.trash_dirs());
    dirs
}

/// The walk's matcher: `entry` holds a valid tag.
fn is_tagged(p: &dyn Platform, entry: &DirEntry) -> bool {
    tag_comment(p, &entry.path).is_some()
}

/// What `dir` is: a real directory (not a symlink) holding a valid tag, with
/// its creator and whether it is a virtualenv. The error is the reason it is
/// not, worded for a decision record or a skipped outcome.
fn inspect(p: &dyn Platform, dir: &Path) -> Result<Tagged, String> {
    match p.metadata(dir) {
        Ok(m) if m.kind == FileKind::Dir => {}
        Ok(_) => return Err(format!("{} is no longer a directory", dir.display())),
        Err(_) => return Err(format!("{} no longer exists", dir.display())),
    }
    let Some(comment) = tag_comment(p, dir) else {
        return Err(format!(
            "{} no longer holds a valid {TAG_FILE}",
            dir.display()
        ));
    };
    Ok(Tagged {
        creator: creator_from_comment(&comment).or_else(|| infer_creator(p, dir)),
        // Any node by that name: a venv that is half-built still counts.
        venv: p.metadata(&dir.join(VENV_MARKER)).is_ok(),
    })
}

/// Everything after the signature in `dir`'s tag, or `None` when there is no
/// valid tag: not a regular file (a symlink does not count), too large, not
/// UTF-8, or not starting with the signature.
fn tag_comment(p: &dyn Platform, dir: &Path) -> Option<String> {
    let path = dir.join(TAG_FILE);
    let meta = p.metadata(&path).ok()?;
    if meta.kind != FileKind::File || meta.len > MAX_TAG_LEN {
        return None;
    }
    let text = p.read_to_string(&path).ok()?;
    text.strip_prefix(SIGNATURE).map(str::to_owned)
}

/// The tool a tag's comment says made it: the text after `created by` on the
/// first line containing it, case-insensitively, without a trailing full stop
/// (`# This file is a cache directory tag created by cargo.` → `cargo`).
/// Control characters are dropped and the name is capped, because it ends up
/// in labels printed to a terminal.
fn creator_from_comment(comment: &str) -> Option<String> {
    const MARKER: &str = "created by ";
    comment.lines().find_map(|line| {
        // ASCII lowercasing keeps every byte offset, so `at` indexes `line`.
        let at = line.to_ascii_lowercase().find(MARKER)?;
        let rest = line.get(at + MARKER.len()..)?;
        let name: String = rest
            .trim()
            .trim_end_matches('.')
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_CREATOR_CHARS)
            .collect();
        let name = name.trim();
        (!name.is_empty()).then(|| name.to_owned())
    })
}

/// The creator of a tag that names none, recognised by its layout. Only uv for
/// now: its tag is the bare signature, and its cache root holds buckets named
/// `<bucket>-v<N>`.
fn infer_creator(p: &dyn Platform, dir: &Path) -> Option<String> {
    let entries = p.read_dir(dir).ok()?;
    entries
        .iter()
        .any(|e| e.kind == FileKind::Dir && is_uv_bucket(&e.file_name))
        .then(|| "uv".to_owned())
}

/// Whether `name` is one of uv's cache buckets, at any format version.
fn is_uv_bucket(name: &str) -> bool {
    name.rsplit_once("-v").is_some_and(|(bucket, version)| {
        UV_BUCKETS.contains(&bucket)
            && !version.is_empty()
            && version.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The newest modification time among `dir`, its children and its
/// grandchildren — see the module docs for why two levels and why never the
/// access time. Entries on another filesystem are ignored, as the walk ignores
/// them. `None` when `dir` or one of its subdirectories cannot be listed.
fn newest_mtime(p: &dyn Platform, dir: &Path) -> Option<SystemTime> {
    let meta = p.metadata(dir).ok()?;
    let dev = meta.dev;
    let mut newest = meta.modified;
    for child in p.read_dir(dir).ok()? {
        // Gone since the listing: it no longer has anything to say.
        let Ok(m) = p.metadata(&child.path) else {
            continue;
        };
        if m.dev != dev {
            continue;
        }
        newest = newest.max(m.modified);
        if child.kind != FileKind::Dir {
            continue;
        }
        for grandchild in p.read_dir(&child.path).ok()? {
            if let Ok(m) = p.metadata(&grandchild.path)
                && m.dev == dev
            {
                newest = newest.max(m.modified);
            }
        }
    }
    Some(newest)
}

/// `true` for an absolute path made only of normal components — no `..` for a
/// stale or hand-edited candidate id to climb out of a root with.
fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// The one-line label: `cache dir tagged by cargo: ~/.cache/foo`.
fn label(home: &Path, dir: &Path, creator: Option<&str>) -> String {
    let shown = match dir.strip_prefix(home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => dir.display().to_string(),
    };
    match creator {
        Some(creator) => format!("cache dir tagged by {creator}: {shown}"),
        None => format!("cache dir with a {TAG_FILE}: {shown}"),
    }
}

/// A duration for a decision reason, to the minute.
fn human(d: Duration) -> humantime::FormattedDuration {
    humantime::format_duration(Duration::from_secs(d.as_secs() / 60 * 60))
}

/// Record a `kept` decision (guarded so lean builds pay nothing).
fn keep(ctx: &Ctx, id: &str, label: &str, age: Option<Duration>, reason: String) {
    if !DecisionLog::compiled() {
        return;
    }
    let mut d = Decision::new(DecisionKind::Kept, NAME, id)
        .label(label)
        .reason(reason);
    if let Some(age) = age {
        d = d.age(age);
    }
    ctx.decide(d);
}

/// Record the terminal decision for an execute outcome.
fn record_outcome(ctx: &Ctx, c: &Candidate, outcome: &Outcome) {
    if !DecisionLog::compiled() {
        return;
    }
    let (kind, reason) = match outcome {
        Outcome::Removed { .. } => (DecisionKind::Deleted, None),
        Outcome::Skipped { reason, .. } => (DecisionKind::Skipped, Some(reason.clone())),
        Outcome::Failed { error, .. } => (DecisionKind::Failed, Some(error.clone())),
    };
    let mut d = Decision::new(kind, NAME, c.id.clone())
        .label(c.label.clone())
        .bytes(c.bytes)
        .age(c.age(ctx.now()));
    if let Some(r) = reason {
        d = d.reason(r);
    }
    ctx.decide(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creator_comes_from_the_comment_line_that_names_it() {
        let cargo = "\n# This file is a cache directory tag created by cargo.\n\
                     # For information about cache directory tags see https://bford.info/cachedir/\n";
        assert_eq!(creator_from_comment(cargo).as_deref(), Some("cargo"));

        let virtualenv = "\n# This file is a cache directory tag created by Python virtualenv.\n\
                          # For information about cache directory tags, see:\n\
                          #   https://bford.info/cachedir/";
        assert_eq!(
            creator_from_comment(virtualenv).as_deref(),
            Some("Python virtualenv")
        );

        // mypy's wording, and any capitalisation.
        let mypy = "\n# This file is a cache directory tag automatically created by mypy.\n";
        assert_eq!(creator_from_comment(mypy).as_deref(), Some("mypy"));
        assert_eq!(
            creator_from_comment("\n# CREATED BY Foo Tool\n").as_deref(),
            Some("Foo Tool")
        );
    }

    #[test]
    fn a_comment_naming_no_creator_yields_none() {
        assert_eq!(creator_from_comment(""), None);
        assert_eq!(creator_from_comment("\n# just a cache\n"), None);
        assert_eq!(creator_from_comment("\n# created by \n"), None);
        assert_eq!(creator_from_comment("\n# created by .\n"), None);
    }

    #[test]
    fn creator_is_stripped_of_control_characters_and_capped() {
        let sneaky = "\n# created by evil\u{1b}[2Jtool.\n";
        assert_eq!(creator_from_comment(sneaky).as_deref(), Some("evil[2Jtool"));
        let long = format!("\n# created by {}\n", "x".repeat(500));
        assert_eq!(
            creator_from_comment(&long).map(|c| c.chars().count()),
            Some(MAX_CREATOR_CHARS)
        );
    }

    #[test]
    fn uv_buckets_are_recognised_at_any_version() {
        for name in ["archive-v0", "wheels-v7", "sdists-v10", "flat-index-v2"] {
            assert!(is_uv_bucket(name), "{name}");
        }
        for name in [
            "archive",
            "archive-v",
            "archive-vx",
            "debug-v0",
            "wheels-v7a",
        ] {
            assert!(!is_uv_bucket(name), "{name}");
        }
    }

    #[test]
    fn plain_absolute_paths_only() {
        assert!(is_plain_absolute(Path::new("/home/a/.cache/x")));
        assert!(!is_plain_absolute(Path::new("home/a")));
        assert!(!is_plain_absolute(Path::new("/home/a/../b")));
    }

    #[test]
    fn labels_abbreviate_the_home_directory() {
        let home = Path::new("/home/tester");
        assert_eq!(
            label(home, Path::new("/home/tester/.cache/t"), Some("cargo")),
            "cache dir tagged by cargo: ~/.cache/t"
        );
        assert_eq!(
            label(home, Path::new("/srv/cache"), None),
            "cache dir with a CACHEDIR.TAG: /srv/cache"
        );
    }
}
