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
//!   emit a [`Candidate`].
//! * **execute** — re-validate the marker (the `target/` directory, its sibling
//!   `Cargo.toml`, and the `CACHEDIR.TAG` must all still be present) and then
//!   [`remove_dir_all`](disk_saver_core::Platform::remove_dir_all). A vanished
//!   marker yields [`Outcome::Skipped`] rather than a delete.
//!
//! # Safety guardrails
//!
//! The `.disk-saver-keep` sentinel, dot-directory skipping, and the
//! no-symlink-follow rule are enforced by [`disk_saver_scan`]. This crate adds
//! the built-in root denylist (`~/Library`, `~/.Trash`, the user cache dir),
//! honors the `confirm` flag, and re-validates every marker at execute time.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, FileKind, Outcome, Platform, RetentionPolicy, expand_tilde,
    parse_adapter_config,
};
use disk_saver_scan::{FsConfig, Rule, ScanOptions, find_artifacts};

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

/// The factory the CLI registry uses to build the `rust-target` adapter.
///
/// `build` deserializes the `[adapters.rust-target]` table via
/// [`parse_adapter_config`] into an [`FsConfig`] and validates the retention
/// thresholds; a failure yields [`ConfigError`].
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

/// Construct a boxed [`FsAdapter`] from its opaque config table.
fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    let cfg: FsConfig = parse_adapter_config(NAME, raw)?;

    RetentionPolicy::new(cfg.max_age, cfg.min_age)
        .validate()
        .map_err(|message| ConfigError::Adapter {
            adapter: NAME.into(),
            message,
        })?;

    Ok(Box::new(FsAdapter {
        policy: RetentionPolicy::new(cfg.max_age, cfg.min_age),
        confirm: cfg.confirm,
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
    cfg: FsConfig,
    confirm: bool,
}

impl FsAdapter {
    /// Resolve the roots to scan: expand `~`, then drop any root equal to or
    /// nested under a denylisted directory.
    fn roots(&self, home: &Path, denylist: &[PathBuf]) -> Vec<PathBuf> {
        self.cfg
            .roots
            .iter()
            .map(|r| expand_tilde(r, home))
            .filter(|r| !is_under_any(r, denylist))
            .collect()
    }

    /// Compile the user `exclude` globs plus the denylist directories (as both
    /// `<dir>` and `<dir>/**`) into one [`globset::GlobSet`].
    fn exclude_set(&self, denylist: &[PathBuf]) -> Result<globset::GlobSet, AdapterError> {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.cfg.exclude {
            let glob = globset::Glob::new(pattern).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("invalid exclude glob '{pattern}': {e}"))
            })?;
            builder.add(glob);
        }
        for dir in denylist {
            let base = dir.to_string_lossy();
            for pat in [base.to_string(), format!("{base}/**")] {
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
            max_depth: self.cfg.max_depth,
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
            } else if DecisionLog::compiled() {
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
            let outcome = match marker_still_holds(ctx.platform, dir) {
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
