//! Integration tests for the `pip` adapter, driven entirely against
//! [`FakePlatform`] through the public [`factory`] surface (no real filesystem).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_pip::factory;
use disk_saver_core::{Adapter, Class, Ctx, DecisionLog, Outcome, Pressure, Store};
use disk_saver_platform::FakePlatform;

/// A fixed `SystemTime` `days` days after the epoch.
fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

/// Build the adapter from a small TOML snippet (empty string → defaults).
fn adapter_from(toml_src: &str) -> Box<dyn Adapter> {
    let cfg = if toml_src.is_empty() {
        None
    } else {
        Some(toml::from_str(toml_src).unwrap())
    };
    factory().build(cfg).unwrap()
}

#[test]
fn prunes_the_stale_pip_cache() {
    let fake = FakePlatform::new().with_now(t(400)).with_file(
        "~/.cache/pip/http/deadbeef",
        vec![0u8; 16384],
        t(320),
    );
    let mut a = adapter_from("");
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
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
    let mut a = adapter_from("min_age = \"7d\"\n");
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    assert!(a.plan(&mut ctx).unwrap().is_empty());
}
