//! `disk-saver-adapter-pnpm` — prunes the global **pnpm** store and cache.
//!
//! pnpm keeps a content-addressable store (shared package files, hardlinked into
//! each project's `node_modules`) and a metadata cache. Both are regenerable —
//! deleting them just means pnpm re-downloads on the next install — so they are
//! [`Class::Cache`](disk_saver_core::Class) and pruned when idle past the
//! retention policy.
//!
//! The store's location varies by OS and config, so it is discovered
//! authoritatively via `pnpm store path`; the metadata cache lives under the
//! platform cache dir. Either can be overridden/extended with the `paths` config
//! key. If `pnpm` is not installed, only the platform-derived cache dir (and any
//! configured `paths`) are considered.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use disk_saver_cachedir::{CacheConfig, CacheDirAdapter};
use disk_saver_core::{
    Adapter, AdapterFactory, CommandSpec, ConfigError, Platform, parse_adapter_config,
};

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "pnpm";

/// Resolve the pnpm directories to prune: the metadata cache (`<cache>/pnpm`)
/// and the content-addressable store (located via `pnpm store path`).
fn resolver(p: &dyn Platform) -> Vec<PathBuf> {
    let mut dirs = vec![p.user_cache_dir().join("pnpm")];
    // `pnpm store path` prints the absolute store directory. Best-effort: if
    // pnpm is missing or errors, we simply skip the store.
    let spec = CommandSpec::new("pnpm", ["store", "path"]);
    if let Ok(out) = p.run_command(&spec)
        && out.success()
    {
        let path = out.stdout_string().trim().to_string();
        if !path.is_empty() {
            dirs.push(PathBuf::from(path));
        }
    }
    dirs
}

/// The factory the CLI registry uses to build the pnpm adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    let cfg: CacheConfig = parse_adapter_config(NAME, raw)?;
    Ok(Box::new(CacheDirAdapter::new(NAME, resolver, cfg)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_saver_core::{Class, Ctx, DecisionLog, Outcome, Pressure, Store};
    use disk_saver_platform::{CommandOutput, FakePlatform};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn t(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    #[test]
    fn prunes_store_located_via_pnpm_and_the_cache_dir() {
        let fake = FakePlatform::new()
            .with_now(t(400))
            .with_command(
                "pnpm",
                &["store", "path"],
                CommandOutput::ok("/home/tester/.pnpm-store/v3\n"),
            )
            .with_file(
                "/home/tester/.pnpm-store/v3/aa/pkg",
                vec![0u8; 4096],
                t(340),
            )
            .with_file("~/.cache/pnpm/metacache", vec![0u8; 2048], t(340));

        let mut a = (factory().build)(None).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

        let plan = a.plan(&mut ctx).unwrap();
        assert_eq!(plan.len(), 2, "store + cache both proposed");
        assert!(plan.iter().all(|c| c.class == Class::Cache));
        let out = a.execute(&mut ctx, &plan).unwrap();
        assert!(out.iter().all(|o| matches!(o, Outcome::Removed { .. })));
        assert!(!fake.exists("/home/tester/.pnpm-store/v3"));
        assert!(!fake.exists("~/.cache/pnpm"));
    }

    #[test]
    fn works_without_pnpm_installed() {
        // No `pnpm` command scripted → run_command errors → only the cache dir.
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cache/pnpm/metacache",
            vec![0u8; 2048],
            t(340),
        );
        let mut a = (factory().build)(None).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
        let plan = a.plan(&mut ctx).unwrap();
        assert_eq!(plan.len(), 1);
    }
}
