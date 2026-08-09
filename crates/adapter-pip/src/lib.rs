//! `disk-saver-adapter-pip` — prunes pip's global HTTP/wheel cache.
//!
//! pip caches downloaded packages and locally-built wheels under the platform
//! cache directory (`~/Library/Caches/pip` on macOS, `$XDG_CACHE_HOME/pip` or
//! `~/.cache/pip` on Linux). It is re-fetchable/re-buildable, so it is
//! [`Class::Cache`](disk_saver_core::Class) and pruned when idle past the
//! retention policy. `$PIP_CACHE_DIR` is not read in v1; point the `paths`
//! config key at a custom cache if you set it.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use disk_saver_cachedir::{CacheConfig, CacheDirAdapter, os_cache_dirs};
use disk_saver_core::{Adapter, AdapterFactory, ConfigError, Platform, parse_adapter_config};

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "pip";

/// pip's cache directory. Derived from `home` and tried for every OS layout
/// (`~/Library/Caches/pip`, `~/.cache/pip`), plus the platform cache dir so an
/// `$XDG_CACHE_HOME` override is still honored. Non-existent ones are skipped.
fn resolver(p: &dyn Platform) -> Vec<PathBuf> {
    let mut dirs = os_cache_dirs(&p.home_dir(), "pip");
    dirs.push(p.user_cache_dir().join("pip"));
    dirs
}

/// The factory the CLI registry uses to build the pip adapter.
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
    use disk_saver_platform::FakePlatform;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn t(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    #[test]
    fn prunes_the_stale_pip_cache() {
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cache/pip/http/deadbeef",
            vec![0u8; 16384],
            t(320),
        );
        let mut a = (factory().build)(None).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );

        let plan = a.plan(&mut ctx).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].class, Class::Cache);
        assert_eq!(plan[0].bytes, 16384);

        let out = a.execute(&mut ctx, &plan).unwrap();
        assert!(matches!(&out[0], Outcome::Removed { .. }));
        assert!(!fake.exists("~/.cache/pip"));
    }

    #[test]
    fn respects_min_age_floor() {
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cache/pip/http/deadbeef",
            vec![0u8; 16384],
            t(398),
        ); // 2d < 7d
        // Floor pinned: this test is about the floor being honoured at all, not
        // about where the shipped default happens to sit (6h).
        let cfg = toml::from_str("min_age = \"7d\"\n").unwrap();
        let mut a = (factory().build)(Some(cfg)).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        assert!(a.plan(&mut ctx).unwrap().is_empty());
    }
}
