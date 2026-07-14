//! Shared engine for adapters that prune a well-known **global cache directory**
//! (the pnpm store, the cargo registry cache, the pip cache, …).
//!
//! Unlike the project-scanning filesystem adapters (which walk `roots` looking
//! for artifact dirs), these adapters target a small, fixed set of directories
//! that a developer tool owns. Each such directory is treated as one coarse
//! [`Candidate`]: sized with [`Platform::dir_size`], aged by its most recent
//! activity, classed [`Class::Cache`] (re-fetchable, but costs bandwidth/time),
//! and removed wholesale with [`Platform::remove_dir_all`] when it is past the
//! retention policy for the current pressure.
//!
//! A thin adapter crate supplies just two things: a stable `name` and a
//! [`Resolver`] that returns the candidate directories (from the platform's
//! well-known dirs and/or a tool subprocess). Everything else — config parsing,
//! age/size, eligibility, deletion, decision records — lives here.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use disk_saver_core::{
    Adapter, AdapterError, Candidate, Class, ConfigError, Ctx, Decision, DecisionKind, DecisionLog,
    Outcome, Platform, RetentionPolicy, expand_tilde,
};
use serde::Deserialize;

/// Resolves the candidate cache directories for an adapter. Given the platform
/// (so it can consult well-known dirs or shell out to a tool), it returns every
/// directory the adapter might prune; non-existent ones are filtered out later.
/// Best-effort: a resolver that needs a missing tool should return what it can
/// (often just an empty list), never panic.
pub type Resolver = fn(&dyn Platform) -> Vec<PathBuf>;

/// The `[adapters.<name>]` config shared by every cache-directory adapter.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Normal mode: prune when the cache has been idle at least this long.
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
    /// Scavenge floor: never prune a cache used more recently than this.
    #[serde(with = "humantime_serde")]
    pub min_age: Duration,
    /// Route to the approvals queue instead of auto-deleting (default `false` —
    /// these caches are regenerable).
    pub confirm: bool,
    /// Extra directories to prune in addition to the built-in ones (`~` expanded).
    /// Handy when a tool stores its cache somewhere non-standard.
    pub paths: Vec<PathBuf>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            max_age: Duration::from_secs(30 * 24 * 60 * 60),
            min_age: Duration::from_secs(7 * 24 * 60 * 60),
            confirm: false,
            paths: Vec::new(),
        }
    }
}

/// A generic adapter that prunes the directories named by its [`Resolver`].
pub struct CacheDirAdapter {
    name: &'static str,
    resolver: Resolver,
    policy: RetentionPolicy,
    confirm: bool,
    extra: Vec<PathBuf>,
}

impl CacheDirAdapter {
    /// Build from a name, a resolver, and the parsed config. Validates the
    /// retention thresholds.
    pub fn new(
        name: &'static str,
        resolver: Resolver,
        cfg: CacheConfig,
    ) -> Result<Self, ConfigError> {
        let policy = RetentionPolicy::new(cfg.max_age, cfg.min_age);
        policy.validate().map_err(|message| ConfigError::Adapter {
            adapter: name.to_string(),
            message,
        })?;
        Ok(CacheDirAdapter {
            name,
            resolver,
            policy,
            confirm: cfg.confirm,
            extra: cfg.paths,
        })
    }

    /// The full, de-duplicated, `~`-expanded set of directories to consider.
    fn target_dirs(&self, platform: &dyn Platform) -> Vec<PathBuf> {
        let home = platform.home_dir();
        let mut dirs: Vec<PathBuf> = (self.resolver)(platform);
        dirs.extend(self.extra.iter().map(|p| expand_tilde(p, &home)));
        dirs.sort();
        dirs.dedup();
        dirs
    }
}

/// The most recent activity of a cache directory: the newest of its own mtime
/// and its direct children's mtimes (a cheap, shallow proxy — cache writes add
/// or remove entries, which bumps these). Falls back to `dir_mtime` if the
/// listing cannot be read.
fn last_activity(platform: &dyn Platform, dir: &Path, dir_mtime: SystemTime) -> SystemTime {
    let mut newest = dir_mtime;
    if let Ok(entries) = platform.read_dir(dir) {
        for e in entries {
            if let Ok(meta) = platform.metadata(&e.path)
                && meta.modified > newest
            {
                newest = meta.modified;
            }
        }
    }
    newest
}

impl Adapter for CacheDirAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        // Age is derived from the filesystem; no KV state to keep.
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let now = ctx.now();
        let mut candidates = Vec::new();
        for dir in self.target_dirs(ctx.platform) {
            // Only real directories (never follow a symlink to somewhere else).
            let meta = match ctx.platform.metadata(&dir) {
                Ok(m) if m.kind == disk_saver_core::FileKind::Dir => m,
                _ => continue,
            };
            let size = ctx.platform.dir_size(&dir).unwrap_or(0);
            if size == 0 {
                continue; // nothing to reclaim
            }
            let last_used = last_activity(ctx.platform, &dir, meta.modified);
            let age = now.duration_since(last_used).unwrap_or_default();
            let id = dir.to_string_lossy().into_owned();
            let label = format!("{} cache: {}", self.name, dir.display());
            if self.policy.eligible(age, ctx.pressure) {
                candidates.push(
                    Candidate::new(id.clone(), label.clone(), size, last_used, Class::Cache)
                        .confirm(self.confirm),
                );
                if DecisionLog::compiled() {
                    ctx.decide(
                        Decision::new(DecisionKind::Planned, self.name, id)
                            .label(label)
                            .bytes(size)
                            .age(age),
                    );
                }
            } else if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Kept, self.name, id)
                        .label(label)
                        .bytes(size)
                        .age(age)
                        .reason("cache used more recently than the retention policy"),
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
            let still_a_dir = matches!(
                ctx.platform.metadata(dir),
                Ok(m) if m.kind == disk_saver_core::FileKind::Dir
            );
            let outcome = if !still_a_dir {
                Outcome::Skipped {
                    id: c.id.clone(),
                    reason: "cache directory no longer present".to_string(),
                }
            } else {
                match ctx.platform.remove_dir_all(dir) {
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
            if DecisionLog::compiled() {
                let kind = match &outcome {
                    Outcome::Removed { .. } => DecisionKind::Deleted,
                    Outcome::Skipped { .. } => DecisionKind::Skipped,
                    Outcome::Failed { .. } => DecisionKind::Failed,
                };
                ctx.decide(Decision::new(kind, self.name, c.id.clone()).bytes(c.bytes));
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

    fn t(days: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    /// A resolver returning a single fixed directory under the fake home.
    fn resolver(p: &dyn Platform) -> Vec<PathBuf> {
        vec![p.home_dir().join(".cache/tool")]
    }

    fn adapter(cfg: CacheConfig) -> CacheDirAdapter {
        CacheDirAdapter::new("tool", resolver, cfg).unwrap()
    }

    fn ctx_run<F, R>(fake: &FakePlatform, pressure: Pressure, f: F) -> R
    where
        F: FnOnce(&mut Ctx) -> R,
    {
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(fake, store.bucket("tool"), pressure, &log);
        f(&mut ctx)
    }

    #[test]
    fn plans_an_aged_cache_and_skips_a_fresh_one() {
        // Cache last touched 40 days ago; now is day 400.
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cache/tool/blob",
            vec![0u8; 4096],
            t(360),
        );
        let mut a = adapter(CacheConfig::default());

        // Normal: 40d >= max_age 30d → eligible.
        let normal = ctx_run(&fake, Pressure::Normal, |ctx| a.plan(ctx).unwrap());
        assert_eq!(normal.len(), 1);
        assert_eq!(normal[0].class, Class::Cache);
        assert_eq!(normal[0].bytes, 4096);
        assert!(!normal[0].requires_confirmation);

        // Comfortable: never eligible.
        let comfy = ctx_run(&fake, Pressure::Comfortable, |ctx| a.plan(ctx).unwrap());
        assert!(comfy.is_empty());
    }

    #[test]
    fn min_age_floor_holds_under_scavenge() {
        // Used 3 days ago; min_age is 7d → not eligible even under scavenge.
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cache/tool/blob",
            vec![0u8; 4096],
            t(397),
        );
        let mut a = adapter(CacheConfig::default());
        let plan = ctx_run(&fake, Pressure::Scavenge { need: 1 }, |ctx| {
            a.plan(ctx).unwrap()
        });
        assert!(plan.is_empty(), "min_age floor must protect a recent cache");
    }

    #[test]
    fn empty_or_missing_dir_yields_no_candidate() {
        let fake = FakePlatform::new().with_now(t(400)); // no cache dir at all
        let mut a = adapter(CacheConfig::default());
        let plan = ctx_run(&fake, Pressure::Scavenge { need: 1 }, |ctx| {
            a.plan(ctx).unwrap()
        });
        assert!(plan.is_empty());
    }

    #[test]
    fn confirm_flag_marks_candidate() {
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cache/tool/blob",
            vec![0u8; 4096],
            t(300),
        );
        let mut a = adapter(CacheConfig {
            confirm: true,
            ..CacheConfig::default()
        });
        let plan = ctx_run(&fake, Pressure::Normal, |ctx| a.plan(ctx).unwrap());
        assert_eq!(plan.len(), 1);
        assert!(plan[0].requires_confirmation);
    }

    #[test]
    fn execute_removes_the_dir_and_reports_removed() {
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cache/tool/blob",
            vec![0u8; 4096],
            t(300),
        );
        let mut a = adapter(CacheConfig::default());
        let plan = ctx_run(&fake, Pressure::Normal, |ctx| a.plan(ctx).unwrap());
        let out = ctx_run(&fake, Pressure::Normal, |ctx| {
            a.execute(ctx, &plan).unwrap()
        });
        assert!(matches!(&out[0], Outcome::Removed { bytes, .. } if *bytes == 4096));
        assert!(!fake.exists("~/.cache/tool"));
        assert!(fake.removed().iter().any(|p| p.ends_with(".cache/tool")));
    }

    #[test]
    fn extra_paths_are_included() {
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/somewhere/custom/x",
            vec![0u8; 2048],
            t(300),
        );
        let cfg = CacheConfig {
            paths: vec![PathBuf::from("~/somewhere/custom")],
            ..CacheConfig::default()
        };
        let mut a = adapter(cfg);
        let plan = ctx_run(&fake, Pressure::Normal, |ctx| a.plan(ctx).unwrap());
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].bytes, 2048);
    }
}
