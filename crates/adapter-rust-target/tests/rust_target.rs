//! Integration tests for the `rust-target` adapter, driven entirely against
//! [`FakePlatform`] (no real filesystem).

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_rust_target::factory;
use disk_saver_core::{
    Adapter, Candidate, Class, Ctx, Decision, DecisionKind, DecisionLog, Outcome, Pressure, Store,
};
use disk_saver_platform::FakePlatform;

const DAY: u64 = 24 * 60 * 60;

/// A fixed `SystemTime` `days` days after the epoch.
fn day(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * DAY)
}

/// Build the adapter from default config (30d / 7d, roots `["~"]`).
fn adapter_default() -> Box<dyn Adapter> {
    factory().build(None).unwrap()
}

/// Build the adapter from a small TOML snippet.
fn adapter_from(toml: &str) -> Box<dyn Adapter> {
    let raw: toml::Value = toml::from_str(toml).unwrap();
    factory().build(Some(raw)).unwrap()
}

/// A fake with the clock at day 400 (matching `FakePlatform`'s default) and one
/// cargo project under `~/dev/proj` whose source (`Cargo.toml`) was last touched
/// at `active`.
fn fake_project(active: SystemTime) -> FakePlatform {
    FakePlatform::new()
        .with_file("~/dev/proj/Cargo.toml", "[package]", active)
        .with_file("~/dev/proj/src/main.rs", "fn main() {}", active)
        .with_sized_dir("~/dev/proj/target", 5_000, day(399))
        .with_file(
            "~/dev/proj/target/CACHEDIR.TAG",
            "Signature: cargo",
            day(399),
        )
}

/// Run `plan` against `fake` at the given pressure, restricting roots to `~/dev`.
fn plan(fake: &FakePlatform, adapter: &mut Box<dyn Adapter>, pressure: Pressure) -> Vec<Candidate> {
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(fake, store.bucket(adapter.name()), pressure, &log);
    adapter.plan(&mut ctx).unwrap()
}

/// The default clock is day 400; the fake home is `/home/tester`.
fn dev_root_toml() -> &'static str {
    r#"roots = ["~/dev"]"#
}

// ── identification ─────────────────────────────────────────────────────────

#[test]
fn finds_target_with_cargo_toml_and_cachedir_tag() {
    // Project idle for 40 days → eligible under Normal (>= 30d default max_age).
    let fake = fake_project(day(360));
    let mut adapter = adapter_from(dev_root_toml());
    let cands = plan(&fake, &mut adapter, Pressure::Normal);

    assert_eq!(cands.len(), 1, "expected the target dir to be found");
    let c = &cands[0];
    assert_eq!(c.id, "/home/tester/dev/proj/target");
    assert_eq!(c.bytes, 5_000);
    assert_eq!(c.class, Class::Rebuildable);
    assert_eq!(c.last_used, day(360));
    assert!(c.label.contains("/home/tester/dev/proj"));
    assert!(!c.requires_confirmation);
}

#[test]
fn rejects_target_without_cachedir_tag() {
    // Same project but no CACHEDIR.TAG inside target/.
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/Cargo.toml", "[package]", day(360))
        .with_sized_dir("~/dev/proj/target", 5_000, day(399));
    let mut adapter = adapter_from(dev_root_toml());
    let cands = plan(&fake, &mut adapter, Pressure::Normal);
    assert!(cands.is_empty(), "no CACHEDIR.TAG → not a cargo target");
}

#[test]
fn rejects_target_without_cargo_toml() {
    // target/ + CACHEDIR.TAG but no sibling Cargo.toml.
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/other.txt", "x", day(360))
        .with_sized_dir("~/dev/proj/target", 5_000, day(399))
        .with_file("~/dev/proj/target/CACHEDIR.TAG", "Signature", day(399));
    let mut adapter = adapter_from(dev_root_toml());
    let cands = plan(&fake, &mut adapter, Pressure::Normal);
    assert!(cands.is_empty(), "no Cargo.toml sibling → not a match");
}

// ── age tiers & thresholds ───────────────────────────────────────────────────

#[test]
fn comfortable_plans_nothing() {
    let fake = fake_project(day(1)); // ancient, but pressure is comfortable
    let mut adapter = adapter_from(dev_root_toml());
    let cands = plan(&fake, &mut adapter, Pressure::Comfortable);
    assert!(cands.is_empty(), "comfortable pressure never deletes");
}

#[test]
fn normal_requires_max_age() {
    let mut adapter = adapter_from(dev_root_toml());

    // 20 days idle < 30d max_age → not eligible under Normal.
    let young = fake_project(day(380));
    assert!(plan(&young, &mut adapter, Pressure::Normal).is_empty());

    // 40 days idle >= 30d → eligible under Normal.
    let old = fake_project(day(360));
    assert_eq!(plan(&old, &mut adapter, Pressure::Normal).len(), 1);
}

#[test]
fn scavenge_uses_min_age_floor() {
    // Floor pinned at 7d: this test covers the tier mechanism, and the fixtures
    // below are expressed in days. The shipped default is 1h — see
    // `shipped_min_age_floor_is_one_hour`.
    let mut adapter = adapter_from("roots = [\"~/dev\"]\nmin_age = \"7d\"\n");

    // 20 days idle: below max_age but above min_age (7d) → eligible only under
    // scavenge, not under normal.
    let mid = fake_project(day(380));
    assert!(plan(&mid, &mut adapter, Pressure::Normal).is_empty());
    assert_eq!(
        plan(&mid, &mut adapter, Pressure::Scavenge { need: 1 }).len(),
        1
    );

    // 3 days idle: below the min_age floor → never eligible, even scavenging.
    let fresh = fake_project(day(397));
    assert!(
        plan(&fresh, &mut adapter, Pressure::Scavenge { need: 1 }).is_empty(),
        "min_age floor must protect freshly-used projects even under scavenge"
    );
}

#[test]
fn shipped_min_age_floor_is_one_hour() {
    // Policy, pinned deliberately. This is the sharpest default in the tool:
    // `last_active` comes from *source* mtimes, so an hour of not typing is
    // enough to make a project's `target/` collectable under scavenge — and on a
    // big workspace that is a very long rebuild. Anyone changing this number
    // should have to change this test too.
    let mut adapter = adapter_from(dev_root_toml());
    let scavenge = Pressure::Scavenge { need: 1 };
    let idle_for = |d: Duration| fake_project(day(400) - d);

    let warm = idle_for(Duration::from_secs(30 * 60));
    assert!(
        plan(&warm, &mut adapter, scavenge).is_empty(),
        "30 minutes idle is inside the 1h floor"
    );

    let cold = idle_for(Duration::from_secs(2 * 60 * 60));
    assert_eq!(
        plan(&cold, &mut adapter, scavenge).len(),
        1,
        "2 hours idle is past the 1h floor"
    );
}

// ── guardrails ───────────────────────────────────────────────────────────────

#[test]
fn honors_disk_saver_keep_sentinel() {
    let fake = fake_project(day(360)).with_file("~/dev/proj/.disk-saver-keep", "", day(360));
    let mut adapter = adapter_from(dev_root_toml());
    let cands = plan(&fake, &mut adapter, Pressure::Normal);
    assert!(cands.is_empty(), ".disk-saver-keep exempts the project");
}

#[test]
fn denylisted_roots_are_dropped() {
    // A cargo project sitting inside ~/Library must never be scanned even when
    // it is named as a root.
    let fake = FakePlatform::new()
        .with_file("~/Library/proj/Cargo.toml", "[package]", day(360))
        .with_sized_dir("~/Library/proj/target", 5_000, day(399))
        .with_file("~/Library/proj/target/CACHEDIR.TAG", "Signature", day(399));
    let mut adapter = adapter_from(r#"roots = ["~/Library"]"#);
    let cands = plan(&fake, &mut adapter, Pressure::Normal);
    assert!(cands.is_empty(), "~/Library root is denylisted");
}

#[test]
fn confirm_flag_marks_candidates() {
    let fake = fake_project(day(360));
    let mut adapter = adapter_from(
        r#"
        roots = ["~/dev"]
        confirm = true
        "#,
    );
    let cands = plan(&fake, &mut adapter, Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert!(
        cands[0].requires_confirmation,
        "confirm = true must flag candidates for the approvals queue"
    );
}

// ── execute ─────────────────────────────────────────────────────────────────

#[test]
fn execute_removes_and_records_outcome() {
    let fake = fake_project(day(360));
    let mut adapter = adapter_from(dev_root_toml());
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), Pressure::Normal, &log);

    let cands = adapter.plan(&mut ctx).unwrap();
    assert_eq!(cands.len(), 1);

    let outcomes = adapter.execute(&mut ctx, &cands).unwrap();
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        Outcome::Removed { id, bytes } => {
            assert_eq!(id, "/home/tester/dev/proj/target");
            assert_eq!(*bytes, 5_000);
        }
        other => panic!("expected Removed, got {other:?}"),
    }

    // The directory is gone from the fake and its removal was recorded.
    assert!(!fake.exists("~/dev/proj/target"));
    assert!(
        fake.removed()
            .iter()
            .any(|p| p == &PathBuf::from("/home/tester/dev/proj/target"))
    );
}

#[test]
fn execute_skips_when_cargo_toml_vanished() {
    // A candidate whose sibling Cargo.toml no longer exists (only target/ +
    // CACHEDIR.TAG remain) must be skipped, not deleted.
    let fake = FakePlatform::new()
        .with_sized_dir("~/dev/proj/target", 5_000, day(399))
        .with_file("~/dev/proj/target/CACHEDIR.TAG", "Signature", day(399));
    let mut adapter = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), Pressure::Normal, &log);

    let cand = Candidate::new(
        "/home/tester/dev/proj/target",
        "cargo target in /home/tester/dev/proj",
        5_000,
        day(360),
        Class::Rebuildable,
    );
    let outcomes = adapter.execute(&mut ctx, &[cand]).unwrap();
    assert!(
        matches!(outcomes[0], Outcome::Skipped { .. }),
        "got {:?}",
        outcomes[0]
    );
    // Nothing was removed.
    assert!(fake.removed().is_empty());
    assert!(fake.exists("~/dev/proj/target"));
}

#[test]
fn execute_skips_when_cachedir_tag_vanished() {
    // target/ and Cargo.toml present, but the CACHEDIR.TAG is gone → skip.
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/Cargo.toml", "[package]", day(360))
        .with_sized_dir("~/dev/proj/target", 5_000, day(399));
    let mut adapter = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), Pressure::Normal, &log);

    let cand = Candidate::new(
        "/home/tester/dev/proj/target",
        "cargo target in /home/tester/dev/proj",
        5_000,
        day(360),
        Class::Rebuildable,
    );
    let outcomes = adapter.execute(&mut ctx, &[cand]).unwrap();
    assert!(
        matches!(outcomes[0], Outcome::Skipped { .. }),
        "got {:?}",
        outcomes[0]
    );
    assert!(fake.removed().is_empty());
}

#[test]
fn execute_skips_when_target_vanished() {
    // Whole project gone between plan and execute → skip, no failure.
    let fake = FakePlatform::new();
    let mut adapter = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), Pressure::Normal, &log);

    let cand = Candidate::new(
        "/home/tester/dev/proj/target",
        "cargo target in /home/tester/dev/proj",
        5_000,
        day(360),
        Class::Rebuildable,
    );
    let outcomes = adapter.execute(&mut ctx, &[cand]).unwrap();
    assert!(
        matches!(outcomes[0], Outcome::Skipped { .. }),
        "got {:?}",
        outcomes[0]
    );
    assert!(fake.removed().is_empty());
}

// ── decision log wiring (feature-gated) ──────────────────────────────────────

#[test]
fn plan_records_decisions_when_log_compiled() {
    // Exercises the DecisionLog::compiled() branch: with the decision-log
    // feature on, a Planned decision is recorded; with it off this is a no-op
    // and the test still passes (we only assert it does not panic).
    let fake = fake_project(day(360));
    let mut adapter = adapter_from(dev_root_toml());
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), Pressure::Normal, &log);
    // A stray decision to prove ctx.decide is wired and harmless when disabled.
    ctx.decide(Decision::new(
        DecisionKind::Observed,
        "rust-target",
        "probe",
    ));
    let cands = adapter.plan(&mut ctx).unwrap();
    assert_eq!(cands.len(), 1);
}

// ── the scavenge-only incremental sweep ──────────────────────────────────────

/// Like [`fake_project`], but `target/` is a *real* directory so the sweep can
/// read inside it, holding a host-layout incremental cache (9k) next to some
/// other build output (1k) that must be left alone.
fn fake_project_with_incremental(active: SystemTime) -> FakePlatform {
    FakePlatform::new()
        .with_file("~/dev/proj/Cargo.toml", "[package]", active)
        .with_file("~/dev/proj/src/main.rs", "fn main() {}", active)
        .with_dir("~/dev/proj/target", day(399))
        .with_file(
            "~/dev/proj/target/CACHEDIR.TAG",
            "Signature: cargo",
            day(399),
        )
        .with_sized_dir("~/dev/proj/target/debug/incremental", 9_000, day(399))
        .with_sized_dir("~/dev/proj/target/debug/deps", 1_000, day(399))
}

/// Idle time `d` before the day-400 clock.
fn idle_for(d: Duration) -> FakePlatform {
    fake_project_with_incremental(day(400) - d)
}

const SCAVENGE: Pressure = Pressure::Scavenge { need: 1 };

#[test]
fn scavenge_sweeps_incremental_inside_a_target_too_young_to_delete() {
    // 30 minutes idle: inside the 1h `min_age` guarding the whole `target/`,
    // but past the 15m incremental floor. This is the case the sweep exists
    // for — an active workspace whose `target/` is never collectable and grows
    // the entire time it is being used.
    let fake = idle_for(Duration::from_secs(30 * 60));
    let mut adapter = adapter_from(dev_root_toml());

    assert!(
        plan(&fake, &mut adapter, Pressure::Normal).is_empty(),
        "normal mode has nothing to say about a 30-minute-idle project"
    );

    let cands = plan(&fake, &mut adapter, SCAVENGE);
    assert_eq!(cands.len(), 1, "expected exactly the incremental cache");
    let c = &cands[0];
    assert_eq!(c.id, "/home/tester/dev/proj/target/debug/incremental");
    assert_eq!(c.bytes, 9_000, "sized from the cache, not the whole target");
    assert_eq!(c.class, Class::Rebuildable);
    assert_eq!(
        c.last_used,
        day(400) - Duration::from_secs(30 * 60),
        "carries the project's activity so wave ordering still works"
    );
    assert!(c.label.contains("target/debug/incremental"), "{}", c.label);
    assert!(c.label.contains("/home/tester/dev/proj"), "{}", c.label);
}

#[test]
fn sweep_finds_both_cargo_layouts() {
    // Host builds put the cache at target/<profile>/incremental; a --target
    // build puts it at target/<triple>/<profile>/incremental.
    let fake = idle_for(Duration::from_secs(30 * 60)).with_sized_dir(
        "~/dev/proj/target/aarch64-apple-darwin/release/incremental",
        7_000,
        day(399),
    );
    let mut adapter = adapter_from(dev_root_toml());

    let mut ids: Vec<String> = plan(&fake, &mut adapter, SCAVENGE)
        .into_iter()
        .map(|c| c.id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "/home/tester/dev/proj/target/aarch64-apple-darwin/release/incremental".to_string(),
            "/home/tester/dev/proj/target/debug/incremental".to_string(),
        ]
    );
}

#[test]
fn an_eligible_target_is_not_also_swept() {
    // 40 days idle: the whole target/ is going, so proposing its incremental
    // caches too would put the same bytes in the plan twice.
    let fake = fake_project_with_incremental(day(360));
    let mut adapter = adapter_from(dev_root_toml());

    let cands = plan(&fake, &mut adapter, SCAVENGE);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "/home/tester/dev/proj/target");
}

#[test]
fn comfortable_never_sweeps_incremental() {
    let fake = idle_for(Duration::from_secs(30 * 60));
    let mut adapter = adapter_from(dev_root_toml());
    assert!(plan(&fake, &mut adapter, Pressure::Comfortable).is_empty());
}

#[test]
fn shipped_incremental_floor_is_fifteen_minutes() {
    // Policy, pinned deliberately — the same treatment
    // `shipped_min_age_floor_is_one_hour` gets, and for a sharper reason: this
    // floor is the *only* thing between scavenge and a cache the compiler is
    // about to use again, because the 1h `min_age` above it does not apply.
    let mut adapter = adapter_from(dev_root_toml());

    assert!(
        plan(
            &idle_for(Duration::from_secs(10 * 60)),
            &mut adapter,
            SCAVENGE
        )
        .is_empty(),
        "10 minutes idle is inside the 15m floor"
    );
    assert_eq!(
        plan(
            &idle_for(Duration::from_secs(20 * 60)),
            &mut adapter,
            SCAVENGE
        )
        .len(),
        1,
        "20 minutes idle is past the 15m floor"
    );
}

#[test]
fn incremental_floor_is_configurable() {
    let mut adapter = adapter_from("roots = [\"~/dev\"]\nincremental_min_age = \"2h\"\n");

    assert!(
        plan(
            &idle_for(Duration::from_secs(30 * 60)),
            &mut adapter,
            SCAVENGE
        )
        .is_empty(),
        "30 minutes is inside a raised 2h floor"
    );
    assert_eq!(
        plan(
            &idle_for(Duration::from_secs(3 * 60 * 60)),
            &mut adapter,
            SCAVENGE
        )
        .len(),
        1
    );
}

#[test]
fn the_default_incremental_floor_gives_way_to_an_explicit_max_age() {
    // `max_age = "0s"` means "collect it the moment it is idle". The shipped 15m
    // default must not turn that into a config error — it is our number, not the
    // user's. (Regression: it did, and broke `disk-saver plan` outright for any
    // config with a max_age under 15 minutes.)
    let raw: toml::Value = toml::from_str("max_age = \"0s\"\nmin_age = \"0s\"\n").unwrap();
    assert!(factory().build(Some(raw)).is_ok());
}

#[test]
fn incremental_floor_above_max_age_is_rejected() {
    // Same invariant `RetentionPolicy::validate` enforces for min_age: a floor
    // above the normal-mode threshold would have scavenge protecting more than
    // normal mode does.
    let raw: toml::Value =
        toml::from_str("max_age = \"30d\"\nincremental_min_age = \"60d\"\n").unwrap();
    assert!(factory().build(Some(raw)).is_err());
}

#[test]
fn a_symlink_named_incremental_is_not_swept() {
    // Entry kinds come from read_dir, which reports a symlink as Symlink and
    // never as Dir, so a link planted in a profile directory is ignored rather
    // than followed out of the project.
    let active = day(400) - Duration::from_secs(30 * 60);
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/Cargo.toml", "[package]", active)
        .with_dir("~/dev/proj/target", day(399))
        .with_file("~/dev/proj/target/CACHEDIR.TAG", "Signature", day(399))
        .with_dir("~/dev/proj/target/debug", day(399))
        .with_symlink(
            "~/dev/proj/target/debug/incremental",
            "/somewhere/else",
            day(399),
        );
    let mut adapter = adapter_from(dev_root_toml());
    assert!(plan(&fake, &mut adapter, SCAVENGE).is_empty());
}

#[test]
fn execute_removes_an_incremental_cache_and_leaves_the_target() {
    let fake = idle_for(Duration::from_secs(30 * 60));
    let mut adapter = adapter_from(dev_root_toml());
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), SCAVENGE, &log);

    let cands = adapter.plan(&mut ctx).unwrap();
    assert_eq!(cands.len(), 1);

    let outcomes = adapter.execute(&mut ctx, &cands).unwrap();
    match &outcomes[0] {
        Outcome::Removed { id, bytes } => {
            assert_eq!(id, "/home/tester/dev/proj/target/debug/incremental");
            assert_eq!(*bytes, 9_000);
        }
        other => panic!("expected Removed, got {other:?}"),
    }

    assert!(!fake.exists("~/dev/proj/target/debug/incremental"));
    // The point of the sweep: everything that would have to be recompiled is
    // still there.
    assert!(fake.exists("~/dev/proj/target/debug/deps"));
    assert!(fake.exists("~/dev/proj/target"));
}

#[test]
fn execute_skips_incremental_when_the_enclosing_target_marker_vanished() {
    // The cache is validated through its target/, so a target/ that stopped
    // looking like cargo's between plan and execute protects the cache too.
    let active = day(400) - Duration::from_secs(30 * 60);
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/Cargo.toml", "[package]", active)
        .with_dir("~/dev/proj/target", day(399))
        .with_sized_dir("~/dev/proj/target/debug/incremental", 9_000, day(399));
    let mut adapter = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), SCAVENGE, &log);

    let cand = Candidate::new(
        "/home/tester/dev/proj/target/debug/incremental",
        "cargo incremental cache",
        9_000,
        active,
        Class::Rebuildable,
    );
    let outcomes = adapter.execute(&mut ctx, &[cand]).unwrap();
    assert!(
        matches!(outcomes[0], Outcome::Skipped { .. }),
        "no CACHEDIR.TAG on the enclosing target/ → skip, got {:?}",
        outcomes[0]
    );
    assert!(fake.removed().is_empty());
}

#[test]
fn execute_refuses_an_incremental_outside_a_cargo_target() {
    // A directory that merely happens to be called `incremental` is not this
    // adapter's business, whatever a stale candidate id claims.
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/Cargo.toml", "[package]", day(360))
        .with_sized_dir("~/dev/proj/incremental", 9_000, day(399));
    let mut adapter = adapter_default();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(adapter.name()), SCAVENGE, &log);

    let cand = Candidate::new(
        "/home/tester/dev/proj/incremental",
        "not a cargo cache",
        9_000,
        day(360),
        Class::Rebuildable,
    );
    let outcomes = adapter.execute(&mut ctx, &[cand]).unwrap();
    assert!(
        matches!(outcomes[0], Outcome::Skipped { .. }),
        "got {:?}",
        outcomes[0]
    );
    assert!(fake.removed().is_empty());
    assert!(fake.exists("~/dev/proj/incremental"));
}
