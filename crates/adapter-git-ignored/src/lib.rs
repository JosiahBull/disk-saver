//! `disk-saver-adapter-git-ignored` — reclaims space from **git-ignored** objects.
//!
//! A repo's `.gitignore` typically covers big regenerable-or-local things: build
//! outputs, dependency dirs, logs, local databases, virtualenvs. This adapter
//! finds each git repo under `roots`, asks git which ignored objects it would
//! remove (`git clean -Xdn` — ignored only, never tracked or plain-untracked
//! files), and proposes the ones idle past the retention policy.
//!
//! Because ignored objects can include genuinely valuable local state (a local
//! `.env`, a dev database, uncommitted-but-ignored scratch work), this adapter
//! is [`Class::UserData`](disk_saver_core::Class) and defaults to
//! `confirm = true`: like the trash adapter, nothing is deleted automatically —
//! candidates queue for `disk-saver review`. By default any entry whose name
//! matches `*env*` (`.env`, `venv`, `env/`, …) is never proposed; adjust with the
//! `protect` glob list. Requires `git` on `PATH` (else `Unavailable`).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, CommandSpec, ConfigError, Ctx,
    Decision, DecisionKind, DecisionLog, FileKind, Outcome, Platform, RetentionPolicy,
    parse_adapter_config,
};
use disk_saver_scan::{FsConfig, Rule, find_artifacts};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "git-ignored";

/// A repo is any directory containing a `.git` directory.
static GIT_RULE: &[Rule] = &[Rule {
    artifact_dir: ".git",
    required_sibling: &[],
    validate: |_, _| true,
}];

fn default_max_age() -> Duration {
    Duration::from_secs(30 * 24 * 60 * 60)
}
fn default_min_age() -> Duration {
    Duration::from_secs(7 * 24 * 60 * 60)
}

/// `[adapters.git-ignored]` config.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct IgnoredConfig {
    /// Directories to scan for repos. Defaults to `["~"]`.
    roots: Vec<PathBuf>,
    /// Absolute-path globs excluded from the scan.
    exclude: Vec<String>,
    /// Maximum scan depth below each root. Defaults to `8`.
    max_depth: usize,
    #[serde(with = "humantime_serde")]
    max_age: Duration,
    #[serde(with = "humantime_serde")]
    min_age: Duration,
    /// Route to the approvals queue rather than auto-delete. Defaults to `true`
    /// (this is `UserData`).
    confirm: bool,
    /// Globs matched against an entry's *name*; matches are never proposed.
    /// Defaults to `["*env*"]` so `.env`, `venv`, `env/`, … are always protected.
    protect: Vec<String>,
}

impl Default for IgnoredConfig {
    fn default() -> Self {
        IgnoredConfig {
            roots: vec![PathBuf::from("~")],
            exclude: Vec::new(),
            max_depth: 8,
            max_age: default_max_age(),
            min_age: default_min_age(),
            confirm: true,
            protect: vec!["*env*".to_string()],
        }
    }
}

impl IgnoredConfig {
    /// Project onto the shared [`FsConfig`] for root/exclude resolution.
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

/// The factory the CLI registry uses to build the git-ignored adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    let cfg: IgnoredConfig = parse_adapter_config(NAME, raw)?;
    let policy = RetentionPolicy::new(cfg.max_age, cfg.min_age);
    policy.validate().map_err(|message| ConfigError::Adapter {
        adapter: NAME.to_string(),
        message,
    })?;
    let mut builder = GlobSetBuilder::new();
    for pattern in &cfg.protect {
        let glob = Glob::new(pattern).map_err(|e| ConfigError::Adapter {
            adapter: NAME.to_string(),
            message: format!("invalid protect glob '{pattern}': {e}"),
        })?;
        builder.add(glob);
    }
    let protect = builder.build().map_err(|e| ConfigError::Adapter {
        adapter: NAME.to_string(),
        message: format!("building protect glob set: {e}"),
    })?;
    Ok(Box::new(GitIgnoredAdapter {
        policy,
        confirm: cfg.confirm,
        protect,
        cfg,
    }))
}

struct GitIgnoredAdapter {
    policy: RetentionPolicy,
    confirm: bool,
    protect: GlobSet,
    cfg: IgnoredConfig,
}

/// Probe that `git` is runnable. Missing binary or non-zero exit ⇒ `Unavailable`.
fn ensure_git(ctx: &Ctx) -> Result<(), AdapterError> {
    match ctx
        .platform
        .run_command(&CommandSpec::new("git", ["--version"]))
    {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(AdapterError::unavailable(format!(
            "`git --version` exited {}",
            out.status
        ))),
        Err(_) => Err(AdapterError::unavailable("`git` not found on PATH")),
    }
}

/// Absolute paths of the ignored objects `git clean -Xdn` would remove in `repo`.
fn ignored_objects(platform: &dyn Platform, repo: &Path) -> Vec<PathBuf> {
    let repo_s = repo.to_string_lossy().into_owned();
    let spec = CommandSpec::new("git", ["-C", repo_s.as_str(), "clean", "-Xdn"]);
    let Ok(out) = platform.run_command(&spec) else {
        return Vec::new();
    };
    if !out.success() {
        return Vec::new();
    }
    out.stdout_string()
        .lines()
        .filter_map(|line| {
            // Format: "Would remove <path>" (dirs carry a trailing slash).
            let rel = line
                .strip_prefix("Would remove ")?
                .trim()
                .trim_end_matches('/');
            if rel.is_empty() {
                None
            } else {
                Some(repo.join(rel))
            }
        })
        .collect()
}

/// True if `path`'s final component matches a `protect` glob.
fn is_protected(protect: &GlobSet, path: &Path) -> bool {
    match path.file_name() {
        Some(name) => protect.is_match(Path::new(name)),
        None => false,
    }
}

impl Adapter for GitIgnoredAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        ensure_git(ctx)
    }

    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        ensure_git(ctx)
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let now = ctx.now();
        let (roots, opts) =
            self.cfg.as_fs().resolve_walk(ctx.platform).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}"))
            })?;

        let mut candidates = Vec::new();
        for found in find_artifacts(ctx.platform, &roots, GIT_RULE, &opts) {
            let repo = found.project_root;
            for path in ignored_objects(ctx.platform, &repo) {
                if is_protected(&self.protect, &path) {
                    continue; // e.g. anything matching *env*
                }
                let meta = match ctx.platform.metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let age = now.duration_since(meta.modified).unwrap_or_default();
                if !self.policy.eligible(age, ctx.pressure) {
                    continue;
                }
                let size = if meta.kind == FileKind::Dir {
                    ctx.platform.dir_size(&path).unwrap_or(0)
                } else {
                    meta.len
                };
                let id = path.to_string_lossy().into_owned();
                let label = format!("gitignored: {} (in {})", path.display(), repo.display());
                candidates.push(
                    Candidate::new(
                        id.clone(),
                        label.clone(),
                        size,
                        meta.modified,
                        Class::UserData,
                    )
                    .confirm(self.confirm),
                );
                if DecisionLog::compiled() {
                    ctx.decide(
                        Decision::new(DecisionKind::Planned, NAME, id)
                            .label(label)
                            .bytes(size)
                            .age(age),
                    );
                }
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
            let path = PathBuf::from(&c.id);
            // Defense in depth: never remove a protected entry, even if approved.
            let outcome = if is_protected(&self.protect, &path) {
                Outcome::Skipped {
                    id: c.id.clone(),
                    reason: "protected by a `protect` glob".to_string(),
                }
            } else {
                match ctx.platform.metadata(&path) {
                    Ok(m) if m.kind == FileKind::Dir => match ctx.platform.remove_dir_all(&path) {
                        Ok(()) => Outcome::Removed {
                            id: c.id.clone(),
                            bytes: c.bytes,
                        },
                        Err(e) => Outcome::Failed {
                            id: c.id.clone(),
                            error: e.to_string(),
                        },
                    },
                    Ok(_) => match ctx.platform.remove_file(&path) {
                        Ok(()) => Outcome::Removed {
                            id: c.id.clone(),
                            bytes: c.bytes,
                        },
                        Err(e) => Outcome::Failed {
                            id: c.id.clone(),
                            error: e.to_string(),
                        },
                    },
                    Err(_) => Outcome::Skipped {
                        id: c.id.clone(),
                        reason: "no longer present".to_string(),
                    },
                }
            };
            if DecisionLog::compiled() {
                let kind = match &outcome {
                    Outcome::Removed { .. } => DecisionKind::Deleted,
                    Outcome::Skipped { .. } => DecisionKind::Skipped,
                    Outcome::Failed { .. } => DecisionKind::Failed,
                };
                ctx.decide(Decision::new(kind, NAME, c.id.clone()).bytes(c.bytes));
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
    use disk_saver_platform::{CommandOutput, FakePlatform};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn t(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    /// `git clean -Xdn` output: a build dir, a log file, and a protected `.env`.
    const CLEAN: &str = "Would remove target/\nWould remove debug.log\nWould remove .env\n";

    fn fake_repo() -> FakePlatform {
        FakePlatform::new()
            .with_now(t(400))
            .with_command(
                "git",
                &["--version"],
                CommandOutput::ok("git version 2.44.0"),
            )
            .with_command_prefix("git", &["-C"], CommandOutput::ok(CLEAN))
            .with_file("~/dev/proj/.git/HEAD", b"ref".to_vec(), t(300))
            .with_sized_dir("~/dev/proj/target", 9_000_000, t(300))
            .with_file("~/dev/proj/debug.log", vec![0u8; 1024], t(300))
            .with_file("~/dev/proj/.env", b"SECRET=1".to_vec(), t(300))
    }

    fn cfg_dev() -> Option<toml::Value> {
        Some(toml::from_str("roots = [\"~/dev\"]").unwrap())
    }

    #[test]
    fn plans_ignored_objects_flagged_and_protects_env() {
        let fake = fake_repo();
        let mut a = (factory().build)(cfg_dev()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );

        a.observe(&mut ctx).unwrap();
        let plan = a.plan(&mut ctx).unwrap();
        let ids: Vec<&str> = plan.iter().map(|c| c.id.as_str()).collect();
        // target/ and debug.log proposed; .env protected out.
        assert_eq!(plan.len(), 2, "got {ids:?}");
        assert!(ids.iter().any(|id| id.ends_with("target")));
        assert!(ids.iter().any(|id| id.ends_with("debug.log")));
        assert!(
            !ids.iter().any(|id| id.ends_with(".env")),
            ".env must be protected"
        );
        // UserData + confirm=true by default → flagged for review.
        assert!(plan.iter().all(|c| c.class == Class::UserData));
        assert!(plan.iter().all(|c| c.requires_confirmation));
    }

    #[test]
    fn execute_removes_files_and_dirs_but_refuses_protected() {
        let fake = fake_repo();
        let mut a = (factory().build)(cfg_dev()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        a.observe(&mut ctx).unwrap();
        let plan = a.plan(&mut ctx).unwrap();

        let out = a.execute(&mut ctx, &plan).unwrap();
        assert!(out.iter().all(|o| matches!(o, Outcome::Removed { .. })));
        assert!(!fake.exists("~/dev/proj/target"));
        assert!(!fake.exists("~/dev/proj/debug.log"));

        // Even if a protected path is somehow submitted, execute refuses it.
        let env = Candidate::new(
            fake.home_dir()
                .join("dev/proj/.env")
                .to_string_lossy()
                .into_owned(),
            ".env",
            8,
            t(300),
            Class::UserData,
        );
        let out = a.execute(&mut ctx, &[env]).unwrap();
        assert!(matches!(&out[0], Outcome::Skipped { reason, .. } if reason.contains("protect")));
        assert!(fake.exists("~/dev/proj/.env"), ".env must survive");
    }

    #[test]
    fn unavailable_without_git() {
        let fake = FakePlatform::new().with_now(t(400));
        let mut a = (factory().build)(cfg_dev()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        assert!(matches!(
            a.observe(&mut ctx),
            Err(AdapterError::Unavailable(_))
        ));
    }
}
