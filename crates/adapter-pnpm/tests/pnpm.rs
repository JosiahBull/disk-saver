//! Integration tests for the `pnpm` adapter, driven entirely against
//! [`FakePlatform`] through the public [`factory`] surface (no real filesystem).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_pnpm::factory;
use disk_saver_core::{Adapter, Class, Ctx, DecisionLog, Outcome, Pressure, Store};
use disk_saver_platform::{CommandOutput, FakePlatform};

/// A fixed `SystemTime` `days` days after the epoch.
fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

/// Build the adapter from default config.
fn adapter_default() -> Box<dyn Adapter> {
    factory().build(None).unwrap()
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

    let mut a = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);

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
    let mut a = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);
    let plan = a.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
}
