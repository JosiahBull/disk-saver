//! Integration tests for the `python-cache` adapter, driven entirely against
//! [`FakePlatform`] through the public [`factory`] surface (no real filesystem).

use std::path::{Path, PathBuf};

use disk_saver_adapter_python_cache::factory;
use disk_saver_core::{Adapter, Candidate, Class, Ctx, DecisionLog, Outcome, Platform};
use disk_saver_core::{Pressure, Store};
use disk_saver_platform::FakePlatform;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `t(days)` → a fixed `SystemTime` `days` days after the epoch. The fake
/// clock sits at day 400, so `t(360)` is 40 days old, `t(395)` is 5 days old.
fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

/// Build the adapter from a TOML fragment (exercises the real deserialize +
/// flatten path).
fn adapter(toml_src: &str) -> Box<dyn Adapter> {
    let raw: toml::Value = toml::from_str(toml_src).unwrap();
    factory().build(Some(raw)).unwrap()
}

/// Run `plan` against a fake platform at the given pressure.
fn plan_with(adapter: &mut dyn Adapter, fake: &FakePlatform, pressure: Pressure) -> Vec<Candidate> {
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(fake, store.bucket(adapter.name()), pressure, &log);
    adapter.plan(&mut ctx).unwrap()
}

#[test]
fn build_defaults_and_flatten() {
    // include_venvs defaults false; fs defaults flow through the flatten.
    let a = adapter("");
    assert_eq!(a.name(), "python-cache");
    // A .venv is ignored with the default config (see dedicated test), and
    // parsing a fragment that sets both flattened + own fields works.
    let _ = adapter(
        r#"
        roots = ["~/dev"]
        max_age = "30d"
        min_age = "7d"
        include_venvs = true
        confirm = true
        "#,
    );
}

#[test]
fn build_rejects_inverted_thresholds() {
    let raw: toml::Value = toml::from_str("max_age = \"2d\"\nmin_age = \"9d\"").unwrap();
    assert!(factory().build(Some(raw)).is_err());
}

#[test]
fn finds_pycache_with_only_pyc() {
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "print()", t(360)) // last_active = 40d old
        .with_file(
            "~/dev/proj/__pycache__/mod.cpython-311.pyc",
            vec![0u8; 500],
            t(360),
        );

    let mut a = adapter("roots = [\"~/dev\"]");
    let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);

    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "/home/tester/dev/proj/__pycache__");
    assert_eq!(cands[0].bytes, 500);
    assert_eq!(cands[0].class, Class::Rebuildable);
    assert!(cands[0].label.contains("__pycache__"));
    assert!(cands[0].label.contains("/home/tester/dev/proj"));
    assert!(!cands[0].requires_confirmation);
}

#[test]
fn rejects_pycache_with_stray_source_file() {
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "print()", t(360))
        .with_file(
            "~/dev/proj/__pycache__/mod.cpython-311.pyc",
            vec![0u8; 500],
            t(360),
        )
        // A stray, non-bytecode file makes the directory unsafe to remove.
        .with_file("~/dev/proj/__pycache__/notes.py", "important", t(360));

    let mut a = adapter("roots = [\"~/dev\"]");
    let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
    assert!(cands.is_empty());
}

#[test]
fn finds_mypy_cache() {
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(360))
        .with_sized_dir("~/dev/proj/.mypy_cache", 2_048, t(360));

    let mut a = adapter("roots = [\"~/dev\"]");
    let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "/home/tester/dev/proj/.mypy_cache");
    assert_eq!(cands[0].bytes, 2_048);
}

#[test]
fn venv_ignored_unless_opted_in() {
    let build_fake = || {
        FakePlatform::new()
            .with_file("~/dev/proj/main.py", "x", t(360))
            .with_sized_dir("~/dev/proj/.venv", 9_000, t(360))
    };

    // Default: .venv is not a candidate.
    let fake = build_fake();
    let mut off = adapter("roots = [\"~/dev\"]");
    assert!(plan_with(off.as_mut(), &fake, Pressure::Normal).is_empty());

    // Opt in: .venv becomes a candidate.
    let fake = build_fake();
    let mut on = adapter("roots = [\"~/dev\"]\ninclude_venvs = true");
    let cands = plan_with(on.as_mut(), &fake, Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "/home/tester/dev/proj/.venv");
    assert_eq!(cands[0].bytes, 9_000);
}

#[test]
fn age_tiers_normal_vs_scavenge() {
    // last_active 10 days old: eligible only under scavenge (>= min_age 7d),
    // not under normal (< max_age 30d).
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(390))
        .with_sized_dir("~/dev/proj/.ruff_cache", 100, t(390));

    let mut a = adapter("roots = [\"~/dev\"]");
    assert!(plan_with(a.as_mut(), &fake, Pressure::Comfortable).is_empty());
    assert!(plan_with(a.as_mut(), &fake, Pressure::Normal).is_empty());
    assert_eq!(
        plan_with(a.as_mut(), &fake, Pressure::Scavenge { need: 1 }).len(),
        1
    );
}

#[test]
fn min_age_floor_under_scavenge() {
    // last_active 5 days old (< min_age 7d): never eligible, even at scavenge.
    // The floor is pinned here; the shipped default is 1h, which this
    // fixture would sail past without testing anything.
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(395))
        .with_sized_dir("~/dev/proj/.pytest_cache", 100, t(395));

    let mut a = adapter("roots = [\"~/dev\"]\nmin_age = \"7d\"\n");
    assert!(plan_with(a.as_mut(), &fake, Pressure::Scavenge { need: 1 }).is_empty());
}

#[test]
fn confirm_flag_flags_candidates() {
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(360))
        .with_sized_dir("~/dev/proj/.tox", 100, t(360));

    let mut a = adapter("roots = [\"~/dev\"]\nconfirm = true");
    let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert!(cands[0].requires_confirmation);
}

#[test]
fn denylist_drops_roots_and_prunes_subtrees() {
    // A cache under ~/Library must never be proposed, whether it is reached
    // via a denylisted root or by walking down from home.
    let fake = FakePlatform::new()
        .with_file("~/Library/proj/mod.py", "x", t(360))
        .with_sized_dir("~/Library/proj/.mypy_cache", 100, t(360))
        // A legitimate one elsewhere, to prove the walk still works.
        .with_file("~/code/proj/mod.py", "x", t(360))
        .with_sized_dir("~/code/proj/.mypy_cache", 200, t(360));

    // Root explicitly at Library → dropped entirely.
    let mut a = adapter("roots = [\"~/Library\", \"~/code\"]");
    let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "/home/tester/code/proj/.mypy_cache");

    // Broad root at home → Library pruned by the exclude globs, code found.
    let fake = FakePlatform::new()
        .with_file("~/Library/proj/mod.py", "x", t(360))
        .with_sized_dir("~/Library/proj/.mypy_cache", 100, t(360))
        .with_file("~/code/proj/mod.py", "x", t(360))
        .with_sized_dir("~/code/proj/.mypy_cache", 200, t(360));
    let mut a = adapter("roots = [\"~\"]");
    let cands = plan_with(a.as_mut(), &fake, Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "/home/tester/code/proj/.mypy_cache");
}

#[test]
fn honors_disk_saver_keep() {
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(360))
        .with_sized_dir("~/dev/proj/.mypy_cache", 100, t(360))
        .with_file("~/dev/proj/.disk-saver-keep", "", t(360));

    let mut a = adapter("roots = [\"~/dev\"]");
    assert!(plan_with(a.as_mut(), &fake, Pressure::Normal).is_empty());
}

#[test]
fn execute_removes_and_records_outcome() {
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(360))
        .with_sized_dir("~/dev/proj/.mypy_cache", 4_096, t(360));

    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut a = adapter("roots = [\"~/dev\"]");

    let cands = {
        let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);
        a.plan(&mut ctx).unwrap()
    };
    assert_eq!(cands.len(), 1);

    let outcomes = {
        let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);
        a.execute(&mut ctx, &cands).unwrap()
    };
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        Outcome::Removed { id, bytes } => {
            assert_eq!(id, "/home/tester/dev/proj/.mypy_cache");
            assert_eq!(*bytes, 4_096);
        }
        other => panic!("expected Removed, got {other:?}"),
    }
    assert!(!fake.exists("~/dev/proj/.mypy_cache"));
    assert_eq!(
        fake.removed(),
        vec![PathBuf::from("/home/tester/dev/proj/.mypy_cache")]
    );
}

#[test]
fn execute_skips_when_marker_gone() {
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(360))
        .with_sized_dir("~/dev/proj/.mypy_cache", 100, t(360));

    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut a = adapter("roots = [\"~/dev\"]");

    let cands = {
        let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);
        a.plan(&mut ctx).unwrap()
    };

    // The directory disappears between plan and execute.
    fake.remove_dir_all(Path::new("/home/tester/dev/proj/.mypy_cache"))
        .unwrap();

    let outcomes = {
        let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);
        a.execute(&mut ctx, &cands).unwrap()
    };
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], Outcome::Skipped { .. }));
}

#[test]
fn execute_skips_pycache_that_grew_a_source_file() {
    // Planned as a clean __pycache__, but a source file appears before
    // execute: the re-validation must skip it rather than delete.
    let fake = FakePlatform::new()
        .with_file("~/dev/proj/mod.py", "x", t(360))
        .with_file(
            "~/dev/proj/__pycache__/mod.cpython-311.pyc",
            vec![0u8; 100],
            t(360),
        );

    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut a = adapter("roots = [\"~/dev\"]");

    let cands = {
        let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);
        a.plan(&mut ctx).unwrap()
    };
    assert_eq!(cands.len(), 1);

    // A non-bytecode file lands in the directory after planning.
    let fake = fake.with_file("~/dev/proj/__pycache__/late.py", "oops", t(361));

    let outcomes = {
        let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);
        a.execute(&mut ctx, &cands).unwrap()
    };
    assert!(matches!(outcomes[0], Outcome::Skipped { .. }));
    assert!(fake.exists("~/dev/proj/__pycache__"));
}
