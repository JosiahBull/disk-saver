//! `disk-saver-adapter-cargo-registry` — prunes cargo's global download caches.
//!
//! `cargo` caches everything it fetches under `~/.cargo`: downloaded crate
//! tarballs (`registry/cache`), their extracted sources (`registry/src`), and
//! git dependencies (`git/db`, `git/checkouts`). All of it is re-fetchable from
//! crates.io / the git remotes, so it is [`Class::Cache`](disk_saver_core::Class)
//! and pruned when idle past the retention policy. The registry *index* is left
//! alone (small, and re-downloading it is slow).
//!
//! Each of the four directories is treated independently, so an actively-used
//! `registry/src` can be spared while a stale `git/checkouts` is reclaimed.
//! `$CARGO_HOME` is not honoured in v1 (the default `~/.cargo` is assumed); point
//! the `paths` config key at a custom location if you relocate it.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use disk_saver_cachedir::{CacheConfig, CacheDirAdapter};
use disk_saver_core::{Adapter, AdapterFactory, ConfigError, Platform, parse_adapter_config};

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "cargo-registry";

/// The re-fetchable cargo cache directories under `~/.cargo`.
fn resolver(p: &dyn Platform) -> Vec<PathBuf> {
    let cargo = p.home_dir().join(".cargo");
    vec![
        cargo.join("registry").join("cache"),
        cargo.join("registry").join("src"),
        cargo.join("git").join("db"),
        cargo.join("git").join("checkouts"),
    ]
}

/// The factory the CLI registry uses to build the cargo-registry adapter.
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
    fn prunes_stale_registry_cache_and_src_but_not_the_index() {
        let fake = FakePlatform::new()
            .with_now(t(400))
            .with_file(
                "~/.cargo/registry/cache/idx/foo-1.0.crate",
                vec![0u8; 8192],
                t(300),
            )
            .with_file(
                "~/.cargo/registry/src/idx/foo-1.0/lib.rs",
                vec![0u8; 4096],
                t(300),
            )
            // The index is not a resolver target, so it must never be proposed.
            .with_file("~/.cargo/registry/index/idx/cache", vec![0u8; 1024], t(300));

        let mut a = (factory().build)(None).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

        let plan = a.plan(&mut ctx).unwrap();
        let ids: Vec<&str> = plan.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.iter().any(|id| id.ends_with("registry/cache")));
        assert!(ids.iter().any(|id| id.ends_with("registry/src")));
        assert!(
            !ids.iter().any(|id| id.contains("registry/index")),
            "the registry index must never be a candidate"
        );
        assert!(plan.iter().all(|c| c.class == Class::Cache));

        let out = a.execute(&mut ctx, &plan).unwrap();
        assert!(out.iter().all(|o| matches!(o, Outcome::Removed { .. })));
        assert!(
            fake.exists("~/.cargo/registry/index/idx/cache"),
            "index untouched"
        );
    }

    #[test]
    fn comfortable_proposes_nothing() {
        let fake = FakePlatform::new().with_now(t(400)).with_file(
            "~/.cargo/registry/cache/idx/foo-1.0.crate",
            vec![0u8; 8192],
            t(100),
        );
        let mut a = (factory().build)(None).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Comfortable, &log);
        assert!(a.plan(&mut ctx).unwrap().is_empty());
    }
}
