//! `disk-saver-adapter-rust-target` — the `rust-target` adapter
//! (ARCHITECTURE.md §12.3).
//!
//! One of the three filesystem adapters built on the shared
//! [`disk_saver_scan`] walker. It proposes cargo `target/` directories for
//! deletion: they are [`Class::Rebuildable`] (a `cargo build` reconstructs
//! them) and are identified strictly (§11.6) — a `target/` directory only
//! counts when it sits next to a `Cargo.toml` **and** contains the
//! `CACHEDIR.TAG` file cargo writes.
//!
//! # Phases
//!
//! * **check / observe** — no-op. Age derives entirely from the filesystem
//!   (the project's most-recent source mtime / `.git/HEAD`), so no KV state is
//!   needed.
//! * **plan** — walk the configured roots (with the built-in denylist and the
//!   user's `exclude` globs applied), and for every matched `target/` whose
//!   project has been idle past the retention policy for the current pressure,
//!   emit a [`Candidate`]. Under [`Pressure::Scavenge`] a `target/` that is
//!   *too young* to delete outright still contributes its incremental caches —
//!   see below.
//! * **execute** — re-validate the marker and then
//!   [`remove_dir_all`](disk_saver_core::Platform::remove_dir_all). For a whole
//!   `target/` the marker is the directory, its sibling `Cargo.toml`, and the
//!   `CACHEDIR.TAG`; for an incremental cache it is all of those on the
//!   enclosing `target/` *plus* the cache directory itself. A vanished marker
//!   yields [`Outcome::Skipped`] rather than a delete.
//!
//! # The incremental sweep
//!
//! The whole-`target/` rule above only ever fires on a project nobody has
//! touched lately, which is precisely the wrong shape for the directories that
//! actually get large: an *active* workspace's `target/` is rebuilt hourly, so
//! its `last_active` never ages past any floor worth having, and it grows
//! without bound the entire time.
//!
//! It grows because cargo never reclaims anything. Every distinct build
//! configuration — a different feature union from `-p one-crate` versus
//! `--workspace --all-features`, clippy's rustc wrapper versus cargo's — gets
//! its own `-C metadata` and therefore its own artifacts, and the old set is
//! left on disk forever. `target/<profile>/incremental` is the worst of it,
//! because rustc garbage-collects sessions *within* one keyed directory but
//! never removes a directory whose key changed. A real measurement (a Rust
//! workspace of 14 members, eight days of ordinary work): `target/` at 142 GB,
//! of which 60 GB was `debug/incremental` spread over 1,691 session
//! directories, several hundred MB each.
//!
//! So under scavenge — and only under scavenge, where the alternative is
//! deleting something that costs more — this adapter also proposes the
//! `incremental` directories inside a `target/` it is not allowed to delete
//! wholesale. That is a strictly smaller loss than the parent: every compiled
//! rlib stays, so nothing has to be *re*built; the next edit to a crate just
//! recompiles it in full instead of by codegen unit. Because it is cheap it
//! gets its own, much shorter floor
//! ([`incremental_min_age`](RustTargetConfig::incremental_min_age), 15 minutes
//! by default) rather than the `min_age` that guards the whole directory.
//!
//! The two never overlap: when a `target/` is itself eligible the sweep is
//! skipped for that project, so the same bytes are never proposed twice.
//!
//! # Safety guardrails
//!
//! The `.disk-saver-keep` sentinel, dot-directory skipping, and the
//! no-symlink-follow rule are enforced by [`disk_saver_scan`]. This crate adds
//! the built-in root denylist (`~/Library`, `~/.Trash`, the user cache dir),
//! honors the `confirm` flag, and re-validates every marker at execute time.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, FileKind, Outcome, Platform, Pressure, RetentionPolicy,
    expand_tilde, parse_adapter_config,
};
use disk_saver_scan::{FsConfig, Rule, ScanOptions, find_artifacts};
use serde::Deserialize;

/// The stable adapter name (config section, KV bucket, log target).
const NAME: &str = "rust-target";

/// The class every candidate from this adapter carries: a `target/` directory
/// is always rebuildable from source.
const CLASS: Class = Class::Rebuildable;

/// The artifact-matching rules for this adapter.
///
/// A single rule: a `target/` directory next to a `Cargo.toml`, validated by
/// the presence of the `CACHEDIR.TAG` file cargo writes into every `target/`
/// (which distinguishes a cargo build directory from an unrelated directory a
/// user happened to name `target`).
static RULES: &[Rule] = &[Rule {
    artifact_dir: "target",
    required_sibling: &["Cargo.toml"],
    validate: |p, dir| p.metadata(&dir.join("CACHEDIR.TAG")).is_ok(),
}];

/// The name of the incremental-compilation cache directory cargo creates inside
/// each profile directory.
const INCREMENTAL_DIR: &str = "incremental";

/// How many directory levels below `target/` the search descends while looking
/// for [`INCREMENTAL_DIR`] (a match is taken at any level it reaches, including
/// the last). Cargo puts the cache at `target/<profile>/incremental` for a host
/// build and `target/<triple>/<profile>/incremental` once `--target` is
/// involved, so two covers both layouts; nothing deeper than that is cargo's.
const INCREMENTAL_MAX_DEPTH: usize = 2;

/// Floor for the incremental sweep when the config does not name one.
///
/// Being a default it never overrules an explicit `max_age`: [`build`] lowers it
/// to `max_age` when the user did not set it themselves.
///
/// Much shorter than the 1h `FsConfig::min_age` that guards a whole `target/`,
/// and deliberately so: deleting an incremental cache leaves every compiled
/// artifact in place, so the cost of being wrong is that the next edit to one
/// crate recompiles that crate whole instead of by codegen unit. Fifteen
/// minutes is about "you have stopped typing", which is all the protection this
/// needs.
const DEFAULT_INCREMENTAL_MIN_AGE: Duration = Duration::from_secs(15 * 60);

/// `[adapters.rust-target]`: the shared filesystem-scan config plus the one
/// knob that only means anything for cargo.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RustTargetConfig {
    #[serde(flatten)]
    fs: FsConfig,
    /// Floor for the scavenge-only incremental sweep: an `incremental`
    /// directory inside a project idle for less than this is left alone.
    ///
    /// Independent of `min_age`, which governs the whole `target/`. Set it to a
    /// large value to disable the sweep in all but the most idle projects.
    #[serde(with = "humantime_serde")]
    pub incremental_min_age: Duration,
}

impl Default for RustTargetConfig {
    fn default() -> Self {
        RustTargetConfig {
            fs: FsConfig::default(),
            incremental_min_age: DEFAULT_INCREMENTAL_MIN_AGE,
        }
    }
}

/// The factory the CLI registry uses to build the `rust-target` adapter.
///
/// `build` deserializes the `[adapters.rust-target]` table via
/// [`parse_adapter_config`] into a [`RustTargetConfig`] and validates the
/// retention thresholds; a failure yields [`ConfigError`].
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

/// Construct a boxed [`FsAdapter`] from its opaque config table.
fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    // Whether the user named the sweep floor themselves, decided before `raw` is
    // consumed — the two cases below are handled differently.
    let incremental_unset = raw
        .as_ref()
        .and_then(toml::Value::as_table)
        .is_none_or(|t| !t.contains_key("incremental_min_age"));
    let mut cfg: RustTargetConfig = parse_adapter_config(NAME, raw)?;

    RetentionPolicy::new(cfg.fs.max_age, cfg.fs.min_age)
        .validate()
        .map_err(|message| ConfigError::Adapter {
            adapter: NAME.into(),
            message,
        })?;

    // The sweep floor obeys the same invariant `min_age` does: a floor above the
    // normal-mode threshold would have scavenge protecting more than normal mode
    // does, which is backwards. How to enforce that depends on whose number it
    // is. A config saying `max_age = "0s"` means "collect this the moment it is
    // idle", and our *default* has no business turning that into a hard error —
    // so when the user was silent the default gives way. An explicit pair that
    // contradicts itself is a real mistake and is reported.
    if incremental_unset {
        cfg.incremental_min_age = cfg.incremental_min_age.min(cfg.fs.max_age);
    } else if cfg.incremental_min_age > cfg.fs.max_age {
        return Err(ConfigError::Adapter {
            adapter: NAME.into(),
            message: format!(
                "incremental_min_age ({:?}) must not exceed max_age ({:?})",
                cfg.incremental_min_age, cfg.fs.max_age
            ),
        });
    }

    Ok(Box::new(FsAdapter {
        policy: RetentionPolicy::new(cfg.fs.max_age, cfg.fs.min_age),
        confirm: cfg.fs.confirm,
        cfg,
    }))
}

/// The `rust-target` adapter.
///
/// Holds the derived [`RetentionPolicy`] and the parsed [`FsConfig`]; `confirm`
/// is lifted out of the config for convenience since it is consulted per
/// candidate.
struct FsAdapter {
    policy: RetentionPolicy,
    cfg: RustTargetConfig,
    confirm: bool,
}

impl FsAdapter {
    /// Resolve the roots to scan: expand `~`, then drop any root equal to or
    /// nested under a denylisted directory.
    fn roots(&self, home: &Path, denylist: &[PathBuf]) -> Vec<PathBuf> {
        self.cfg
            .fs
            .roots
            .iter()
            .map(|r| expand_tilde(r, home))
            .filter(|r| !is_under_any(r, denylist))
            .collect()
    }

    /// Propose the incremental-compilation caches inside a `target/` that is
    /// itself too young to delete, returning whether anything was proposed.
    ///
    /// A no-op unless the pressure is [`Pressure::Scavenge`] and the project has
    /// been idle for at least
    /// [`incremental_min_age`](RustTargetConfig::incremental_min_age). `age` is
    /// the project's idle time, already computed by the caller, and every
    /// candidate carries the project's `last_active` so the engine's
    /// oldest-first wave ordering still reaches the stalest projects first.
    fn sweep_incremental(
        &self,
        ctx: &mut Ctx,
        f: &disk_saver_scan::Found,
        age: std::time::Duration,
        out: &mut Vec<Candidate>,
    ) -> bool {
        match ctx.pressure {
            Pressure::Comfortable | Pressure::Normal => return false,
            Pressure::Scavenge { .. } => {}
        }
        if age < self.cfg.incremental_min_age {
            return false;
        }

        let mut swept = false;
        for dir in incremental_dirs(ctx.platform, &f.artifact_dir) {
            let size = ctx.platform.dir_size(&dir).unwrap_or(0);
            let id = dir.to_string_lossy().into_owned();
            // Name the cache by its path under the project, so a project with
            // several profiles produces distinguishable lines.
            let rel = dir.strip_prefix(&f.project_root).unwrap_or(&dir);
            let label = format!(
                "cargo incremental cache {} in {}",
                rel.display(),
                f.project_root.display()
            );

            out.push(
                Candidate::new(id.clone(), label.clone(), size, f.last_active, CLASS)
                    .confirm(self.confirm),
            );
            swept = true;

            if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Planned, NAME, id)
                        .label(label)
                        .bytes(size)
                        .age(age),
                );
            }
        }
        swept
    }

    /// Compile the user `exclude` globs plus the denylist directories (as both
    /// `<dir>` and `<dir>/**`) into one [`globset::GlobSet`].
    fn exclude_set(&self, denylist: &[PathBuf]) -> Result<globset::GlobSet, AdapterError> {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.cfg.fs.exclude {
            let glob = globset::Glob::new(pattern).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("invalid exclude glob '{pattern}': {e}"))
            })?;
            builder.add(glob);
        }
        for dir in denylist {
            // Escape glob metacharacters in the literal path so a home dir like
            // `/data/foo[old]` still excludes ~/Library etc. (`[..]` would
            // otherwise be parsed as a character class and silently not match).
            let base = globset::escape(&dir.to_string_lossy());
            for pat in [base.clone(), format!("{base}/**")] {
                let glob = globset::Glob::new(&pat).map_err(|e| {
                    AdapterError::Failed(anyhow::anyhow!("invalid denylist glob '{pat}': {e}"))
                })?;
                builder.add(glob);
            }
        }
        builder
            .build()
            .map_err(|e| AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}")))
    }
}

impl Adapter for FsAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        // No-op: age derives from the filesystem, so there is no KV state to
        // reconcile.
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let home = ctx.platform.home_dir();
        let denylist = [
            home.join("Library"),
            home.join(".Trash"),
            ctx.platform.user_cache_dir(),
        ];

        let roots = self.roots(&home, &denylist);
        let exclude = self.exclude_set(&denylist)?;
        let opts = ScanOptions {
            max_depth: self.cfg.fs.max_depth,
            exclude,
        };

        let mut candidates = Vec::new();
        for f in find_artifacts(ctx.platform, &roots, RULES, &opts) {
            let age = ctx.now().duration_since(f.last_active).unwrap_or_default();
            let id = f.artifact_dir.to_string_lossy().into_owned();
            let label = format!("cargo target in {}", f.project_root.display());

            if self.policy.eligible(age, ctx.pressure) {
                candidates.push(
                    Candidate::new(id.clone(), label.clone(), f.size, f.last_active, CLASS)
                        .confirm(self.confirm),
                );
                if DecisionLog::compiled() {
                    ctx.decide(
                        Decision::new(DecisionKind::Planned, NAME, id)
                            .label(label)
                            .bytes(f.size)
                            .age(age),
                    );
                }
                // The whole directory is going; sweeping its incremental caches
                // as well would propose the same bytes twice.
                continue;
            }

            // Too young to delete outright. Under scavenge its incremental
            // caches are still fair game — see the module docs.
            let swept = self.sweep_incremental(ctx, &f, age, &mut candidates);

            if !swept && DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Kept, NAME, id)
                        .label(label)
                        .bytes(f.size)
                        .age(age)
                        .reason(format!("age {age:?} < threshold")),
                );
            }
        }

        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let mut outcomes = Vec::with_capacity(batch.len());
        for c in batch {
            let dir = Path::new(&c.id);
            let outcome = match candidate_still_valid(ctx.platform, dir) {
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

/// Every `incremental` directory cargo may have put inside `target_dir`.
///
/// Descends at most [`INCREMENTAL_MAX_DEPTH`] levels and never follows a
/// symlink — entry kinds come from `read_dir`, which reports a symlink as
/// [`FileKind::Symlink`] rather than [`FileKind::Dir`], so a link *named*
/// `incremental` is neither returned nor descended. A matched directory is not
/// descended into either: an `incremental` nested inside an `incremental` is
/// not a thing cargo makes, and the outer one subsumes it regardless.
fn incremental_dirs(p: &dyn Platform, target_dir: &Path) -> Vec<PathBuf> {
    fn walk(p: &dyn Platform, dir: &Path, descents_left: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = p.read_dir(dir) else {
            return;
        };
        for e in entries.iter().filter(|e| e.kind == FileKind::Dir) {
            let path = dir.join(&e.file_name);
            if e.file_name == INCREMENTAL_DIR {
                out.push(path);
            } else if descents_left > 0 {
                walk(p, &path, descents_left - 1, out);
            }
        }
    }

    let mut out = Vec::new();
    walk(p, target_dir, INCREMENTAL_MAX_DEPTH, &mut out);
    out
}

/// Re-validate a candidate immediately before deleting it, dispatching on what
/// kind of thing its path is.
///
/// An `incremental` cache is only safe to delete because of what encloses it,
/// so it is checked through its `target/`; everything else this adapter
/// proposes *is* a `target/`.
fn candidate_still_valid(p: &dyn Platform, path: &Path) -> Result<(), String> {
    if path.file_name().and_then(|n| n.to_str()) == Some(INCREMENTAL_DIR) {
        incremental_still_holds(p, path)
    } else {
        marker_still_holds(p, path)
    }
}

/// Re-validate an incremental-cache candidate: `dir` must still be a directory,
/// and the `target/` enclosing it — found by walking up at most
/// [`INCREMENTAL_MAX_DEPTH`] + 1 ancestors — must still satisfy every check
/// [`marker_still_holds`] applies to a whole `target/`.
///
/// A path that is not inside a cargo `target/` is refused outright rather than
/// deleted, which is what stops a stale or hand-edited candidate id from
/// pointing this at an unrelated directory that happens to be called
/// `incremental`.
fn incremental_still_holds(p: &dyn Platform, dir: &Path) -> Result<(), String> {
    match p.metadata(dir) {
        Ok(m) if m.kind == FileKind::Dir => {}
        Ok(_) => return Err(format!("{} is no longer a directory", dir.display())),
        Err(_) => return Err(format!("{} no longer exists", dir.display())),
    }

    let mut ancestor = dir.parent();
    for _ in 0..=INCREMENTAL_MAX_DEPTH {
        let Some(a) = ancestor else { break };
        let name = a.file_name().and_then(|n| n.to_str());
        if RULES.iter().any(|r| name == Some(r.artifact_dir)) {
            return marker_still_holds(p, a);
        }
        ancestor = a.parent();
    }

    Err(format!(
        "{} is not inside a cargo target directory",
        dir.display()
    ))
}

/// Re-validate the marker for `artifact_dir` immediately before deleting it: the
/// directory itself must still exist, every required sibling must still be
/// present in its project root, and the rule's [`validate`](Rule::validate)
/// check (the `CACHEDIR.TAG`) must still pass. Returns the reason to skip on the
/// first failing check.
fn marker_still_holds(p: &dyn Platform, artifact_dir: &Path) -> Result<(), String> {
    let name = artifact_dir.file_name().and_then(|n| n.to_str());
    let Some(rule) = RULES.iter().find(|r| Some(r.artifact_dir) == name) else {
        return Err(format!("{} matches no known rule", artifact_dir.display()));
    };

    match p.metadata(artifact_dir) {
        Ok(m) if m.kind == FileKind::Dir => {}
        Ok(_) => {
            return Err(format!(
                "{} is no longer a directory",
                artifact_dir.display()
            ));
        }
        Err(_) => return Err(format!("{} no longer exists", artifact_dir.display())),
    }

    let Some(root) = artifact_dir.parent() else {
        return Err(format!("{} has no project root", artifact_dir.display()));
    };
    for sib in rule.required_sibling {
        if p.metadata(&root.join(sib)).is_err() {
            return Err(format!("required sibling {sib} is gone"));
        }
    }

    if !(rule.validate)(p, artifact_dir) {
        return Err(format!(
            "{} no longer looks like a cargo target dir (CACHEDIR.TAG missing?)",
            artifact_dir.display()
        ));
    }

    Ok(())
}

/// Record the terminal decision for an execute outcome (guarded so lean builds
/// pay nothing).
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

/// Whether `path` is equal to, or nested under, any directory in `bases`.
fn is_under_any(path: &Path, bases: &[PathBuf]) -> bool {
    bases.iter().any(|b| path == b || path.starts_with(b))
}
