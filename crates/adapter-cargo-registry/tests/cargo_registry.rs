//! Integration tests for the `cargo-registry` adapter, driven entirely against
//! [`FakePlatform`] through the public [`factory`] surface (no real filesystem).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_cargo_registry::factory;
use disk_saver_core::{Adapter, Class, Ctx, DecisionLog, Outcome, Pressure, Store};
use disk_saver_platform::FakePlatform;

/// A fixed `SystemTime` `days` days after the epoch.
fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

/// Build the adapter from default config.
fn adapter_default() -> Box<dyn Adapter> {
    factory().build(None).unwrap()
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

    let mut a = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);

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
    let mut a = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Comfortable, &log);
    assert!(a.plan(&mut ctx).unwrap().is_empty());
}
