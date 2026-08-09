//! `disk-saver-adapter-python-cache` — the `python-cache` adapter
//! (ARCHITECTURE.md §12.4).
//!
//! One of the three filesystem adapters built on [`disk_saver_scan`]. It walks a
//! set of roots and proposes deletion of Python tooling caches that are cheaply
//! regenerated: `__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.ruff_cache/`
//! and `.tox/`, plus `.venv/` when explicitly opted in via `include_venvs`.
//!
//! Everything an adapter of this kind needs is derived from the filesystem, so
//! there is no KV state and [`observe`](PyCacheAdapter::observe) is a no-op — an
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
//!   [`execute`](PyCacheAdapter::execute) immediately before deletion.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, FileKind, Outcome, Platform, RetentionPolicy, expand_tilde,
    parse_adapter_config,
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
///
/// `build` deserializes the `[adapters.python-cache]` table via
/// [`parse_adapter_config`], validates the retention thresholds and the
/// `exclude` globs, and returns a boxed adapter. Any failure yields
/// [`ConfigError::Adapter`].
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

/// Construct a boxed [`PyCacheAdapter`] from its opaque config table.
fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    let cfg: PyConfig = parse_adapter_config(NAME, raw)?;

    RetentionPolicy::new(cfg.fs.max_age, cfg.fs.min_age)
        .validate()
        .map_err(|message| ConfigError::Adapter {
            adapter: NAME.to_string(),
            message,
        })?;

    // Surface a bad exclude glob at config time rather than mid-run.
    cfg.fs.glob_set().map_err(|e| ConfigError::Adapter {
        adapter: NAME.to_string(),
        message: format!("invalid exclude glob: {e}"),
    })?;

    let policy = RetentionPolicy::new(cfg.fs.max_age, cfg.fs.min_age);
    let confirm = cfg.fs.confirm;
    Ok(Box::new(PyCacheAdapter {
        policy,
        cfg,
        confirm,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use disk_saver_core::{Pressure, Store};
    use disk_saver_platform::FakePlatform;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// `t(days)` → a fixed `SystemTime` `days` days after the epoch. The fake
    /// clock sits at day 400, so `t(360)` is 40 days old, `t(395)` is 5 days old.
    fn t(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    /// Build the adapter from a TOML fragment (exercises the real deserialize +
    /// flatten path).
    fn adapter(toml_src: &str) -> Box<dyn Adapter> {
        let raw: toml::Value = toml::from_str(toml_src).unwrap();
        build(Some(raw)).unwrap()
    }

    /// Run `plan` against a fake platform at the given pressure.
    fn plan_with(
        adapter: &mut dyn Adapter,
        fake: &FakePlatform,
        pressure: Pressure,
    ) -> Vec<Candidate> {
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(fake, store.bucket(NAME), pressure, &log);
        adapter.plan(&mut ctx).unwrap()
    }

    #[test]
    fn build_defaults_and_flatten() {
        // include_venvs defaults false; fs defaults flow through the flatten.
        let a = adapter("");
        assert_eq!(a.name(), NAME);
        // A .venv is ignored with the default config (see dedicated test), and
        // parsing a fragment that sets both flattened + own fields works.
        let _ = adapter(
            r#"
            roots = ["~/dev"]
            max_age = "30d"
            min_age = "7d"
            include_venvs = true
            confirm = true
            "#,
        );
    }

    #[test]
    fn build_rejects_inverted_thresholds() {
        let raw: toml::Value = toml::from_str("max_age = \"2d\"\nmin_age = \"9d\"").unwrap();
        assert!(build(Some(raw)).is_err());
    }

    #[test]
    fn finds_pycache_with_only_pyc() {
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "print()", t(360)) // last_active = 40d old
            .with_file(
                "~/dev/proj/__pycache__/mod.cpython-311.pyc",
                vec![0u8; 500],
                t(360),
            );

        let mut a = adapter("roots = [\"~/dev\"]");
        let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);

        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "/home/tester/dev/proj/__pycache__");
        assert_eq!(cands[0].bytes, 500);
        assert_eq!(cands[0].class, Class::Rebuildable);
        assert!(cands[0].label.contains("__pycache__"));
        assert!(cands[0].label.contains("/home/tester/dev/proj"));
        assert!(!cands[0].requires_confirmation);
    }

    #[test]
    fn rejects_pycache_with_stray_source_file() {
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "print()", t(360))
            .with_file(
                "~/dev/proj/__pycache__/mod.cpython-311.pyc",
                vec![0u8; 500],
                t(360),
            )
            // A stray, non-bytecode file makes the directory unsafe to remove.
            .with_file("~/dev/proj/__pycache__/notes.py", "important", t(360));

        let mut a = adapter("roots = [\"~/dev\"]");
        let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
        assert!(cands.is_empty());
    }

    #[test]
    fn finds_mypy_cache() {
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(360))
            .with_sized_dir("~/dev/proj/.mypy_cache", 2_048, t(360));

        let mut a = adapter("roots = [\"~/dev\"]");
        let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "/home/tester/dev/proj/.mypy_cache");
        assert_eq!(cands[0].bytes, 2_048);
    }

    #[test]
    fn venv_ignored_unless_opted_in() {
        let build_fake = || {
            FakePlatform::new()
                .with_file("~/dev/proj/main.py", "x", t(360))
                .with_sized_dir("~/dev/proj/.venv", 9_000, t(360))
        };

        // Default: .venv is not a candidate.
        let fake = build_fake();
        let mut off = adapter("roots = [\"~/dev\"]");
        assert!(plan_with(off.as_mut(), &fake, Pressure::Normal).is_empty());

        // Opt in: .venv becomes a candidate.
        let fake = build_fake();
        let mut on = adapter("roots = [\"~/dev\"]\ninclude_venvs = true");
        let cands = plan_with(on.as_mut(), &fake, Pressure::Normal);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "/home/tester/dev/proj/.venv");
        assert_eq!(cands[0].bytes, 9_000);
    }

    #[test]
    fn age_tiers_normal_vs_scavenge() {
        // last_active 10 days old: eligible only under scavenge (>= min_age 7d),
        // not under normal (< max_age 30d).
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(390))
            .with_sized_dir("~/dev/proj/.ruff_cache", 100, t(390));

        let mut a = adapter("roots = [\"~/dev\"]");
        assert!(plan_with(a.as_mut(), &fake, Pressure::Comfortable).is_empty());
        assert!(plan_with(a.as_mut(), &fake, Pressure::Normal).is_empty());
        assert_eq!(
            plan_with(a.as_mut(), &fake, Pressure::Scavenge { need: 1 }).len(),
            1
        );
    }

    #[test]
    fn min_age_floor_under_scavenge() {
        // last_active 5 days old (< min_age 7d): never eligible, even at scavenge.
        // The floor is pinned here; the shipped default is 1h, which this
        // fixture would sail past without testing anything.
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(395))
            .with_sized_dir("~/dev/proj/.pytest_cache", 100, t(395));

        let mut a = adapter("roots = [\"~/dev\"]\nmin_age = \"7d\"\n");
        assert!(plan_with(a.as_mut(), &fake, Pressure::Scavenge { need: 1 }).is_empty());
    }

    #[test]
    fn confirm_flag_flags_candidates() {
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(360))
            .with_sized_dir("~/dev/proj/.tox", 100, t(360));

        let mut a = adapter("roots = [\"~/dev\"]\nconfirm = true");
        let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
        assert_eq!(cands.len(), 1);
        assert!(cands[0].requires_confirmation);
    }

    #[test]
    fn denylist_drops_roots_and_prunes_subtrees() {
        // A cache under ~/Library must never be proposed, whether it is reached
        // via a denylisted root or by walking down from home.
        let fake = FakePlatform::new()
            .with_file("~/Library/proj/mod.py", "x", t(360))
            .with_sized_dir("~/Library/proj/.mypy_cache", 100, t(360))
            // A legitimate one elsewhere, to prove the walk still works.
            .with_file("~/code/proj/mod.py", "x", t(360))
            .with_sized_dir("~/code/proj/.mypy_cache", 200, t(360));

        // Root explicitly at Library → dropped entirely.
        let mut a = adapter("roots = [\"~/Library\", \"~/code\"]");
        let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "/home/tester/code/proj/.mypy_cache");

        // Broad root at home → Library pruned by the exclude globs, code found.
        let fake = FakePlatform::new()
            .with_file("~/Library/proj/mod.py", "x", t(360))
            .with_sized_dir("~/Library/proj/.mypy_cache", 100, t(360))
            .with_file("~/code/proj/mod.py", "x", t(360))
            .with_sized_dir("~/code/proj/.mypy_cache", 200, t(360));
        let mut a = adapter("roots = [\"~\"]");
        let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "/home/tester/code/proj/.mypy_cache");
    }

    #[test]
    fn honors_disk_saver_keep() {
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(360))
            .with_sized_dir("~/dev/proj/.mypy_cache", 100, t(360))
            .with_file("~/dev/proj/.disk-saver-keep", "", t(360));

        let mut a = adapter("roots = [\"~/dev\"]");
        assert!(plan_with(a.as_mut(), &fake, Pressure::Normal).is_empty());
    }

    #[test]
    fn execute_removes_and_records_outcome() {
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(360))
            .with_sized_dir("~/dev/proj/.mypy_cache", 4_096, t(360));

        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut a = adapter("roots = [\"~/dev\"]");

        let cands = {
            let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
            a.plan(&mut ctx).unwrap()
        };
        assert_eq!(cands.len(), 1);

        let outcomes = {
            let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
            a.execute(&mut ctx, &cands).unwrap()
        };
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::Removed { id, bytes } => {
                assert_eq!(id, "/home/tester/dev/proj/.mypy_cache");
                assert_eq!(*bytes, 4_096);
            }
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(!fake.exists("~/dev/proj/.mypy_cache"));
        assert_eq!(
            fake.removed(),
            vec![PathBuf::from("/home/tester/dev/proj/.mypy_cache")]
        );
    }

    #[test]
    fn execute_skips_when_marker_gone() {
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(360))
            .with_sized_dir("~/dev/proj/.mypy_cache", 100, t(360));

        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut a = adapter("roots = [\"~/dev\"]");

        let cands = {
            let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
            a.plan(&mut ctx).unwrap()
        };

        // The directory disappears between plan and execute.
        fake.remove_dir_all(Path::new("/home/tester/dev/proj/.mypy_cache"))
            .unwrap();

        let outcomes = {
            let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
            a.execute(&mut ctx, &cands).unwrap()
        };
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], Outcome::Skipped { .. }));
    }

    #[test]
    fn execute_skips_pycache_that_grew_a_source_file() {
        // Planned as a clean __pycache__, but a source file appears before
        // execute: the re-validation must skip it rather than delete.
        let fake = FakePlatform::new()
            .with_file("~/dev/proj/mod.py", "x", t(360))
            .with_file(
                "~/dev/proj/__pycache__/mod.cpython-311.pyc",
                vec![0u8; 100],
                t(360),
            );

        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut a = adapter("roots = [\"~/dev\"]");

        let cands = {
            let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
            a.plan(&mut ctx).unwrap()
        };
        assert_eq!(cands.len(), 1);

        // A non-bytecode file lands in the directory after planning.
        let fake = fake.with_file("~/dev/proj/__pycache__/late.py", "oops", t(361));

        let outcomes = {
            let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
            a.execute(&mut ctx, &cands).unwrap()
        };
        assert!(matches!(outcomes[0], Outcome::Skipped { .. }));
        assert!(fake.exists("~/dev/proj/__pycache__"));
    }
}
