//! Integration tests for the `git-gc` adapter, driven entirely against
//! [`FakePlatform`] through the public [`factory`] surface (no real filesystem,
//! no real `git`).

use disk_saver_adapter_git_gc::factory;
use disk_saver_core::{AdapterError, Class, Ctx, DecisionLog, Outcome};
use disk_saver_core::{Pressure, Store};
use disk_saver_platform::{CommandOutput, FakePlatform};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

const COUNT_OBJECTS: &str = "count: 100\nsize: 2048\nin-pack: 5\npacks: 1\nsize-pack: 500\nprune-packable: 0\ngarbage: 3\nsize-garbage: 512\n";

fn fake_repo() -> FakePlatform {
    FakePlatform::new()
        .with_now(t(400))
        .with_command(
            "git",
            &["--version"],
            CommandOutput::ok("git version 2.44.0"),
        )
        // An idle repo (source last touched 60 days ago) under ~/dev.
        .with_file("~/dev/proj/src/main.rs", b"fn main(){}".to_vec(), t(340))
        .with_file(
            "~/dev/proj/.git/HEAD",
            b"ref: refs/heads/main".to_vec(),
            t(340),
        )
        .with_file("~/dev/proj/.git/objects/aa/loose", vec![0u8; 4096], t(340))
}

fn cfg_dev() -> Option<toml::Value> {
    Some(toml::from_str("roots = [\"~/dev\"]").unwrap())
}

/// A repo whose source was last touched `idle` before the fake's `now`.
fn repo_idle_for(idle: Duration) -> FakePlatform {
    let stamp = t(400) - idle;
    FakePlatform::new()
        .with_now(t(400))
        .with_command(
            "git",
            &["--version"],
            CommandOutput::ok("git version 2.44.0"),
        )
        .with_command_prefix("git", &["-C"], CommandOutput::ok(COUNT_OBJECTS))
        .with_file("~/dev/proj/src/main.rs", b"fn main(){}".to_vec(), stamp)
        .with_file(
            "~/dev/proj/.git/HEAD",
            b"ref: refs/heads/main".to_vec(),
            stamp,
        )
        .with_file("~/dev/proj/.git/objects/aa/loose", vec![0u8; 4096], stamp)
}

/// Plan against `fake` under scavenge and report how many repos were proposed.
fn scavenge_plan_len(fake: &FakePlatform, cfg: Option<toml::Value>) -> usize {
    let mut a = factory().build(cfg).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        fake,
        store.bucket(a.name()),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    a.observe(&mut ctx).unwrap();
    a.plan(&mut ctx).unwrap().len()
}

#[test]
fn default_min_age_is_six_hours_not_the_shared_one_hour() {
    // git-gc opts out of `FsConfig`'s 1h floor: it repacks a live repo rather
    // than deleting a rebuildable artifact, so a repo touched two hours ago
    // is still "in use" for its purposes.
    assert_eq!(
        scavenge_plan_len(&repo_idle_for(Duration::from_secs(2 * 60 * 60)), cfg_dev()),
        0,
        "2h idle must stay inside the 6h floor"
    );
    assert_eq!(
        scavenge_plan_len(&repo_idle_for(Duration::from_secs(8 * 60 * 60)), cfg_dev()),
        1,
        "8h idle is past the 6h floor"
    );
}

#[test]
fn explicit_min_age_beats_the_git_gc_default() {
    // The override applies only when the user was silent.
    let cfg = Some(toml::from_str("roots = [\"~/dev\"]\nmin_age = \"1h\"\n").unwrap());
    assert_eq!(
        scavenge_plan_len(&repo_idle_for(Duration::from_secs(2 * 60 * 60)), cfg),
        1,
        "an explicit 1h floor must not be silently raised to 6h"
    );
}

#[test]
fn short_max_age_does_not_trip_the_default_floor() {
    // A 30m `max_age` is shorter than our 6h default floor. Since the floor
    // is our opinion and not the user's, it gives way rather than making the
    // whole config unloadable.
    let cfg = Some(toml::from_str("roots = [\"~/dev\"]\nmax_age = \"30m\"\n").unwrap());
    assert!(
        factory().build(cfg).is_ok(),
        "our own default floor must not reject the user's shorter max_age"
    );
}

#[test]
fn clamped_floor_still_gates_on_the_users_max_age() {
    // Not merely loadable: the clamp lands on 30m, so that is the scavenge
    // floor — 10m idle is protected, 45m idle is collectable.
    let cfg = || Some(toml::from_str("roots = [\"~/dev\"]\nmax_age = \"30m\"\n").unwrap());
    assert_eq!(
        scavenge_plan_len(&repo_idle_for(Duration::from_secs(10 * 60)), cfg()),
        0,
        "10m idle is inside the clamped 30m floor"
    );
    assert_eq!(
        scavenge_plan_len(&repo_idle_for(Duration::from_secs(45 * 60)), cfg()),
        1,
        "45m idle is past the clamped 30m floor"
    );
}

#[test]
fn contradictory_explicit_pair_is_still_rejected() {
    // The clamp applies only to *our* default. When the user writes both
    // numbers and they contradict, that is a mistake worth reporting.
    let cfg = Some(toml::from_str("min_age = \"12h\"\nmax_age = \"30m\"\n").unwrap());
    assert!(factory().build(cfg).is_err());
}

#[test]
fn unavailable_when_git_missing() {
    let fake = FakePlatform::new().with_now(t(400));
    let mut a = factory().build(cfg_dev()).unwrap();
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

#[test]
fn plans_idle_repo_with_loose_objects_then_runs_gc() {
    let fake = fake_repo().with_command_prefix(
        "git",
        &["-C"],
        CommandOutput::ok(COUNT_OBJECTS), // covers count-objects; gc also matches -C prefix
    );
    let mut a = factory().build(cfg_dev()).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);

    a.observe(&mut ctx).unwrap();
    let plan = a.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].class, Class::Rebuildable);
    // reclaimable estimate = (size 2048 + size-garbage 512) KiB.
    assert_eq!(plan[0].bytes, (2048 + 512) * 1024);
    assert!(!plan[0].requires_confirmation);

    let out = a.execute(&mut ctx, &plan).unwrap();
    assert!(matches!(&out[0], Outcome::Removed { .. }));
    assert!(
        fake.commands_run()
            .iter()
            .any(|c| c.program == "git" && c.args.iter().any(|a| a == "gc"))
    );
}

#[test]
fn active_repo_is_not_gced() {
    // Source touched an hour ago → inside the 6h floor, so not eligible even
    // under scavenge. Repacking a repo someone is actively committing to is
    // work the next commit undoes.
    let recent = t(400) - Duration::from_secs(60 * 60);
    let fake = FakePlatform::new()
        .with_now(t(400))
        .with_command(
            "git",
            &["--version"],
            CommandOutput::ok("git version 2.44.0"),
        )
        .with_command_prefix("git", &["-C"], CommandOutput::ok(COUNT_OBJECTS))
        .with_file("~/dev/proj/src/main.rs", b"fn main(){}".to_vec(), recent)
        .with_file(
            "~/dev/proj/.git/HEAD",
            b"ref: refs/heads/main".to_vec(),
            recent,
        );
    let mut a = factory().build(cfg_dev()).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    a.observe(&mut ctx).unwrap();
    assert!(a.plan(&mut ctx).unwrap().is_empty());
}
