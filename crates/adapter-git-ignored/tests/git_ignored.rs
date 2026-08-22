//! Integration tests for the `git-ignored` adapter, driven entirely against
//! [`FakePlatform`] through the public [`factory`] surface (no real filesystem,
//! no real `git`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_git_ignored::factory;
use disk_saver_core::{
    Adapter, AdapterError, Candidate, Class, Ctx, DecisionLog, Outcome, Platform, Pressure, Store,
};
use disk_saver_platform::{CommandOutput, FakePlatform};

/// A fixed `SystemTime` `days` days after the epoch.
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

/// Build the adapter with its roots restricted to `~/dev`.
fn adapter_dev() -> Box<dyn Adapter> {
    let cfg = toml::from_str("roots = [\"~/dev\"]").unwrap();
    factory().build(Some(cfg)).unwrap()
}

#[test]
fn plans_ignored_objects_flagged_and_protects_env() {
    let fake = fake_repo();
    let mut a = adapter_dev();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
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
    let mut a = adapter_dev();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
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
    let mut a = adapter_dev();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    assert!(matches!(
        a.observe(&mut ctx),
        Err(AdapterError::Unavailable(_))
    ));
}
