//! `disk-saver-adapter-python-cache` — the `python-cache` adapter
//! (ARCHITECTURE.md §12.4).
//!
//! One of the three filesystem adapters built on [`disk_saver_scan`]. It walks a
//! set of roots and proposes deletion of Python tooling caches that are cheaply
//! regenerated: `__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.ruff_cache/`
//! and `.tox/`, plus `.venv/` when explicitly opted in via `include_venvs`.
//!
//! Everything an adapter of this kind needs is derived from the filesystem, so
//! there is no KV state and [`observe`](Adapter::observe) is a no-op — an
//! artifact's age comes from its surrounding project's `last_active` time (see
//! [`disk_saver_scan::find_artifacts`]).
//!
//! # Safety
//!
//! * `__pycache__` is only ever a candidate when every entry inside it looks like
//!   a compiled-bytecode file (`*.pyc*`); a stray source or data file rejects the
//!   directory (ARCHITECTURE.md §11.6).
//! * A built-in denylist keeps the walk out of `~/Library`, `~/.Trash` and the
//!   user cache directory: roots at or under those are dropped and the walk is
//!   given matching `exclude` globs.
//! * `.disk-saver-keep`, dot-directory skipping and no-symlink-follow are handled
//!   inside the scan crate; the marker is re-validated in
//!   [`execute`](Adapter::execute) immediately before deletion.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigCx, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, FileKind, Outcome, Platform, RetentionPolicy, expand_tilde,
};
use disk_saver_scan::{FsConfig, Rule, ScanOptions, find_artifacts};
use globset::{Glob, GlobSetBuilder};
use serde::Deserialize;

/// The stable adapter name (config section, KV bucket, log target).
const NAME: &str = "python-cache";

/// Rules always considered, regardless of configuration. `__pycache__` carries a
/// validity check; the tool-cache directories need only exist.
static BASE_RULES: &[Rule] = &[
    Rule {
        artifact_dir: "__pycache__",
        required_sibling: &[],
        validate: pycache_only_pyc,
    },
    Rule {
        artifact_dir: ".pytest_cache",
        required_sibling: &[],
        validate: always,
    },
    Rule {
        artifact_dir: ".mypy_cache",
        required_sibling: &[],
        validate: always,
    },
    Rule {
        artifact_dir: ".ruff_cache",
        required_sibling: &[],
        validate: always,
    },
    Rule {
        artifact_dir: ".tox",
        required_sibling: &[],
        validate: always,
    },
];

/// The `.venv` rule, appended to [`BASE_RULES`] only when `include_venvs` is set.
const VENV_RULE: Rule = Rule {
    artifact_dir: ".venv",
    required_sibling: &[],
    validate: always,
};

/// A rule validator that accepts unconditionally (the artifact directory needs no
/// extra check beyond its name).
fn always(_p: &dyn Platform, _dir: &Path) -> bool {
    true
}

/// Validate a `__pycache__` directory: it must contain *only* compiled-bytecode
/// files (`*.pyc*`, e.g. `mod.cpython-311.pyc`). A subdirectory, a source file, or
/// an unreadable directory all reject the match, so we never delete a directory
/// that a project is (mis)using for something else.
fn pycache_only_pyc(p: &dyn Platform, dir: &Path) -> bool {
    match p.read_dir(dir) {
        Ok(entries) => entries
            .iter()
            .all(|e| e.kind == FileKind::File && e.file_name.contains(".pyc")),
        Err(_) => false,
    }
}

/// The factory the CLI registry uses to build the `python-cache` adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

/// Validate the retention thresholds and the `exclude` globs; either failing
/// yields [`ConfigError::Adapter`].
fn build(cfg: PyConfig, cx: &ConfigCx) -> Result<PyCacheAdapter, ConfigError> {
    let policy = cx.retention(cfg.fs.max_age, cfg.fs.min_age.unwrap_or(FsConfig::MIN_AGE))?;
    // Surface a bad exclude glob at config time rather than mid-run.
    cx.wrap(cfg.fs.glob_set(), "invalid exclude glob")?;
    Ok(PyCacheAdapter {
        policy,
        confirm: cfg.fs.confirm,
        cfg,
    })
}

/// Typed view of the `[adapters.python-cache]` table: the shared filesystem
/// config plus the `.venv` opt-in.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PyConfig {
    /// The shared filesystem-adapter configuration (`roots`, `exclude`,
    /// `max_depth`, `max_age`, `min_age`, `confirm`).
    #[serde(flatten)]
    fs: FsConfig,
    /// Whether `.venv` directories are eligible for deletion. Defaults to `false`
    /// — virtualenv removal is opt-in.
    #[serde(default)]
    include_venvs: bool,
}

/// The `python-cache` adapter. Constructed by [`factory`].
struct PyCacheAdapter {
    /// Age thresholds derived from `max_age`/`min_age`.
    policy: RetentionPolicy,
    /// The parsed configuration.
    cfg: PyConfig,
    /// Whether proposed deletions require confirmation (routed to approvals).
    confirm: bool,
}

impl PyCacheAdapter {
    /// The active rule set for this run: [`BASE_RULES`] plus [`VENV_RULE`] when
    /// `include_venvs` is enabled. Built fresh (rules are `Copy`) so the scan and
    /// the execute re-validation agree on the same set.
    fn rules(&self) -> Vec<Rule> {
        let mut rules = BASE_RULES.to_vec();
        if self.cfg.include_venvs {
            rules.push(VENV_RULE);
        }
        rules
    }

    /// Directories the walk must never enter: `~/Library`, `~/.Trash` and the
    /// user cache directory.
    fn denylist(platform: &dyn Platform) -> [PathBuf; 3] {
        let home = platform.home_dir();
        [
            home.join("Library"),
            home.join(".Trash"),
            platform.user_cache_dir(),
        ]
    }

    /// Re-check, immediately before deletion, that a candidate still matches its
    /// marker rule. Returns `Some(reason)` (a skip reason) when the marker no
    /// longer holds, or `None` when the directory is still safe to remove.
    fn marker_broken(&self, p: &dyn Platform, dir: &Path, rules: &[Rule]) -> Option<String> {
        match p.metadata(dir) {
            Ok(m) if m.kind == FileKind::Dir => {}
            Ok(_) => return Some("artifact path is no longer a directory".to_string()),
            Err(_) => return Some("artifact directory no longer exists".to_string()),
        }

        // Identify the rule from the directory's own name and re-run its checks.
        let name = dir.file_name().and_then(|n| n.to_str());
        if let Some(rule) = name.and_then(|n| rules.iter().find(|r| r.artifact_dir == n)) {
            if let Some(parent) = dir.parent() {
                let entries = p.read_dir(parent).unwrap_or_default();
                for sib in rule.required_sibling {
                    if !entries.iter().any(|e| e.file_name == *sib) {
                        return Some(format!("required sibling '{sib}' no longer present"));
                    }
                }
            }
            if !(rule.validate)(p, dir) {
                return Some("artifact no longer matches its marker rule".to_string());
            }
        }
        None
    }
}

impl Adapter for PyCacheAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    /// No-op: age is derived entirely from the filesystem, so nothing needs to be
    /// persisted between runs.
    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let home = ctx.platform.home_dir();
        let denylist = Self::denylist(ctx.platform);

        // Expand `~` in every root and drop any root at or under a denylisted dir.
        let roots: Vec<PathBuf> = self
            .cfg
            .fs
            .roots
            .iter()
            .map(|r| expand_tilde(r, &home))
            .filter(|r| !denylist.iter().any(|d| r.starts_with(d)))
            .collect();

        // Exclude set: the user's globs plus each denylisted dir (as both the dir
        // itself and everything under it), so a broad root like `~` still can't
        // reach into Library/Trash/caches.
        let mut builder = GlobSetBuilder::new();
        for pat in &self.cfg.fs.exclude {
            let glob = Glob::new(pat).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("invalid exclude glob '{pat}': {e}"))
            })?;
            builder.add(glob);
        }
        for dir in &denylist {
            // Escape glob metacharacters in the literal path so a home dir like
            // `/data/foo[old]` still excludes ~/Library etc. After escaping the
            // pattern always compiles, so a build error is a real bug, not user input.
            let s = globset::escape(&dir.to_string_lossy());
            for pat in [s.clone(), format!("{s}/**")] {
                match Glob::new(&pat) {
                    Ok(glob) => {
                        builder.add(glob);
                    }
                    Err(e) => {
                        tracing::debug!(pattern = %pat, error = %e, "skipping unbuildable denylist glob");
                    }
                }
            }
        }
        let exclude = builder
            .build()
            .map_err(|e| AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}")))?;

        let opts = ScanOptions {
            max_depth: self.cfg.fs.max_depth,
            exclude,
        };

        let rules = self.rules();
        let now = ctx.now();
        let mut candidates: Vec<Candidate> = Vec::new();

        for f in find_artifacts(ctx.platform, &roots, &rules, &opts) {
            let age = now.duration_since(f.last_active).unwrap_or_default();
            let id = f.artifact_dir.to_string_lossy().into_owned();
            let label = format!("{} in {}", f.rule, f.project_root.display());

            if self.policy.eligible(age, ctx.pressure) {
                candidates.push(
                    Candidate::new(
                        id.clone(),
                        label.clone(),
                        f.size,
                        f.last_active,
                        Class::Rebuildable,
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
                        .reason(format!("age {age:?} below threshold")),
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
        let rules = self.rules();
        let mut outcomes = Vec::with_capacity(batch.len());

        for c in batch {
            let dir = Path::new(&c.id);
            let outcome = match self.marker_broken(ctx.platform, dir, &rules) {
                Some(reason) => Outcome::Skipped {
                    id: c.id.clone(),
                    reason,
                },
                None => match ctx.platform.remove_dir_all(dir) {
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

            if DecisionLog::compiled() {
                let (kind, reason) = match &outcome {
                    Outcome::Removed { .. } => (DecisionKind::Deleted, None),
                    Outcome::Skipped { reason, .. } => {
                        (DecisionKind::Skipped, Some(reason.clone()))
                    }
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

            outcomes.push(outcome);
        }

        Ok(outcomes)
    }
}
