//! `disk-saver-adapter-node-modules` — the `node-modules` adapter
//! (ARCHITECTURE.md §12.2).
//!
//! One of the three thin filesystem adapters built over the shared
//! [`disk_saver_scan`] walker. It finds `node_modules/` directories that sit
//! next to a `package.json` (the strict-identification marker, §11.6) and
//! proposes the ones whose surrounding project has been idle past the retention
//! policy for deletion.
//!
//! # Phases
//!
//! * **check / observe** — no-ops. This adapter keeps no KV usage state; a
//!   project's age is derived entirely from the filesystem (source-file mtimes
//!   and `.git/HEAD`, computed by the scan crate).
//! * **plan** — expands the configured `roots` (with `~` resolved against the
//!   platform home), drops any root under the built-in denylist (`~/Library`,
//!   `~/.Trash`, the user cache dir), scans for `node_modules` directories, and
//!   proposes each one whose project age satisfies the retention policy for the
//!   current [`Pressure`](disk_saver_core::Pressure). Candidates are
//!   [`Class::Cache`].
//! * **execute** — re-validates the marker (the `node_modules` directory still
//!   exists *and* its sibling `package.json` is still present) before removing
//!   the directory via [`Platform::remove_dir_all`]; a vanished marker yields
//!   [`Outcome::Skipped`] rather than a delete.
//!
//! Safety guardrails (§11): `Comfortable` deletes nothing and the `min_age`
//! floor under scavenge are enforced by [`RetentionPolicy::eligible`]; the
//! `.disk-saver-keep` sentinel, dot-directory skipping, and no-symlink-follow
//! are enforced by the scan crate; the marker is re-checked at execute time.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigCx, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, FileKind, Outcome, Platform, RetentionPolicy, expand_tilde,
};
use disk_saver_scan::{FsConfig, Rule, ScanOptions, find_artifacts};

/// The stable adapter name (config section, KV bucket, log target).
const NAME: &str = "node-modules";

/// The single artifact rule: a `node_modules/` directory next to a
/// `package.json`. No extra validity check is needed (the sibling is proof
/// enough that this is an npm/yarn/pnpm project).
static RULES: &[Rule] = &[Rule {
    artifact_dir: "node_modules",
    required_sibling: &["package.json"],
    validate: |_, _| true,
}];

/// The factory the CLI registry uses to build the `node-modules` adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

/// Validate the retention thresholds; a bad pair yields
/// [`ConfigError::Adapter`].
fn build(cfg: FsConfig, cx: &ConfigCx) -> Result<FsAdapter, ConfigError> {
    Ok(FsAdapter {
        policy: cx.retention(cfg.max_age, cfg.min_age.unwrap_or(FsConfig::MIN_AGE))?,
        confirm: cfg.confirm,
        cfg,
    })
}

/// The `node-modules` adapter. Constructed by [`factory`]; holds no run-to-run
/// state (age comes from the filesystem).
struct FsAdapter {
    policy: RetentionPolicy,
    cfg: FsConfig,
    confirm: bool,
}

impl Adapter for FsAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        // No KV usage tracking: a project's age is derived from the filesystem.
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let home = ctx.platform.home_dir();

        // Built-in denylist: never sweep the OS/user data areas even if a root
        // (e.g. the default `~`) contains them (§12.2).
        let denylist = [
            home.join("Library"),
            home.join(".Trash"),
            ctx.platform.user_cache_dir(),
        ];

        // Expand `~` in each root and drop any root that is, or lives under, a
        // denylisted directory.
        let roots: Vec<PathBuf> = self
            .cfg
            .roots
            .iter()
            .map(|r| expand_tilde(r, &home))
            .filter(|r| !denylist.iter().any(|d| r == d || r.starts_with(d)))
            .collect();

        // Exclude set: the configured globs plus the denylist expressed as
        // `<dir>` and `<dir>/**` patterns (so the subtrees are pruned mid-walk).
        let exclude = build_exclude(&self.cfg.exclude, &denylist)?;
        let opts = ScanOptions {
            max_depth: self.cfg.max_depth,
            exclude,
        };

        let mut candidates = Vec::new();
        for f in find_artifacts(ctx.platform, &roots, RULES, &opts) {
            let id = f.artifact_dir.to_string_lossy().into_owned();
            let label = format!("node_modules in {}", f.project_root.display());
            let age = ctx.now().duration_since(f.last_active).unwrap_or_default();

            if self.policy.eligible(age, ctx.pressure) {
                candidates.push(
                    Candidate::new(
                        id.clone(),
                        label.clone(),
                        f.size,
                        f.last_active,
                        Class::Cache,
                    )
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
            let artifact_dir = Path::new(&c.id);

            // Re-validate the marker between plan and execute: the artifact dir
            // must still be a real directory and its required sibling(s) still
            // present, else something changed under us and we skip (§11.6).
            let outcome = if !marker_holds(ctx.platform, artifact_dir) {
                Outcome::Skipped {
                    id: c.id.clone(),
                    reason: "marker no longer holds (node_modules or package.json gone)"
                        .to_string(),
                }
            } else {
                match ctx.platform.remove_dir_all(artifact_dir) {
                    Ok(()) => Outcome::Removed {
                        id: c.id.clone(),
                        bytes: c.bytes,
                    },
                    Err(e) => Outcome::Failed {
                        id: c.id.clone(),
                        error: e.to_string(),
                    },
                }
            };

            record_outcome(ctx, c, &outcome);
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

/// Build the exclude [`globset::GlobSet`] from the configured patterns plus the
/// denylist directories (each added as an exact `<dir>` pattern and a
/// recursive `<dir>/**` pattern). Denylist paths are glob-escaped so any special
/// characters in the home path are matched literally.
fn build_exclude(
    patterns: &[String],
    denylist: &[PathBuf],
) -> Result<globset::GlobSet, AdapterError> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let glob = globset::Glob::new(pattern).map_err(|e| {
            AdapterError::Failed(anyhow::anyhow!("invalid exclude glob '{pattern}': {e}"))
        })?;
        builder.add(glob);
    }
    for dir in denylist {
        let escaped = globset::escape(&dir.to_string_lossy());
        // Unwrap is safe: an escaped literal path is always a valid glob.
        builder.add(globset::Glob::new(&escaped).expect("escaped path is a valid glob"));
        builder.add(
            globset::Glob::new(&format!("{escaped}/**")).expect("escaped path is a valid glob"),
        );
    }
    builder
        .build()
        .map_err(|e| AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}")))
}

/// Re-check that `artifact_dir` still satisfies a rule: it is a real directory
/// (no symlink follow), every required sibling is present in the project root,
/// and the rule's extra validity check still passes.
fn marker_holds(p: &dyn Platform, artifact_dir: &Path) -> bool {
    let Some(name) = artifact_dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(rule) = RULES.iter().find(|r| r.artifact_dir == name) else {
        return false;
    };
    match p.metadata(artifact_dir) {
        Ok(m) if m.kind == FileKind::Dir => {}
        _ => return false,
    }
    let Some(root) = artifact_dir.parent() else {
        return false;
    };
    if !rule
        .required_sibling
        .iter()
        .all(|sib| p.metadata(&root.join(sib)).is_ok())
    {
        return false;
    }
    (rule.validate)(p, artifact_dir)
}

/// Record the terminal decision for an execute outcome (guarded so lean builds
/// without the `decision-log` feature pay nothing).
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
