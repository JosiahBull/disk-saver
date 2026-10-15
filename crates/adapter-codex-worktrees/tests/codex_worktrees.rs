//! Integration tests for the `codex-worktrees` adapter, driven entirely
//! against [`FakePlatform`] through the public [`factory`] surface.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_codex_worktrees::factory;
use disk_saver_core::{
    AdapterError, Candidate, Class, Ctx, DecisionLog, Footprint, Outcome, Platform, Pressure, Store,
};
use disk_saver_platform::FakePlatform;

fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

const ROOT: &str = "/home/u/.codex/worktrees";
const CHECKOUT: &str = "/home/u/.codex/worktrees/445f/partly";

/// Codex's layout: one live checkout, one bucket it has already emptied.
fn codex_at(root: &str) -> FakePlatform {
    FakePlatform::new()
        .with_home("/home/u")
        .with_now(t(100))
        .with_file(format!("{root}/.metadata_never_index"), "", t(1))
        .with_file(format!("{root}/445f/.codex-worktree-name"), "", t(90))
        .with_file(
            format!("{root}/445f/partly/.git"),
            "gitdir: /home/u/work/partly/.git/worktrees/partly3\n",
            t(90),
        )
        .with_sized_dir(format!("{root}/445f/partly/packages"), 1_400, t(90))
        .with_file(
            "/home/u/work/partly/.git/worktrees/partly3/HEAD",
            "ref: refs/heads/jo/search\n",
            t(95),
        )
        // Kept so Codex can restore the checkout to the same path.
        .with_file(format!("{root}/083a/.codex-worktree-name"), "", t(20))
}

fn cfg(text: &str) -> Option<toml::Value> {
    Some(toml::from_str(text).unwrap())
}

/// Observe, plan and report under the heaviest pressure there is, asserting
/// that nothing is ever planned.
fn report(fake: &FakePlatform, cfg: Option<toml::Value>) -> Vec<Footprint> {
    let mut a = factory().build(cfg).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        fake,
        store.bucket(a.name()),
        Pressure::Scavenge { need: u64::MAX },
        &log,
    );
    a.observe(&mut ctx).unwrap();
    assert!(a.plan(&mut ctx).unwrap().is_empty(), "never a candidate");
    a.report(&mut ctx).unwrap()
}

#[test]
fn reports_live_checkouts_and_passes_over_emptied_buckets() {
    let fake = codex_at(ROOT);
    let items = report(&fake, None);

    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.id, CHECKOUT);
    assert_eq!(item.label, format!("{CHECKOUT} [jo/search]"));
    assert_eq!(item.bytes, fake.dir_size(Path::new(CHECKOUT)).unwrap());
    assert!(item.bytes >= 1_400);
    // The admin HEAD (t95) is newer than anything in the checkout.
    assert_eq!(item.last_used, t(95));
}

#[test]
fn codex_home_moves_the_default_root() {
    let fake = codex_at("/opt/codex/worktrees").with_env("CODEX_HOME", "/opt/codex");
    let items = report(&fake, None);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "/opt/codex/worktrees/445f/partly");
}

#[test]
fn a_configured_root_wins_and_expands_tilde() {
    let fake = codex_at("/home/u/elsewhere").with_env("CODEX_HOME", "/opt/codex");
    let items = report(&fake, cfg("root = \"~/elsewhere\""));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "/home/u/elsewhere/445f/partly");
}

#[test]
fn no_root_means_unavailable() {
    let fake = FakePlatform::new().with_home("/home/u");
    let mut a = factory().build(None).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(a.name()), Pressure::Normal, &log);

    for result in [a.check(&mut ctx), a.observe(&mut ctx)] {
        match result {
            Err(AdapterError::Unavailable(msg)) => assert!(msg.contains(ROOT), "{msg}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}

#[test]
fn execute_never_removes_anything() {
    let fake = codex_at(ROOT);
    let mut a = factory().build(None).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    let forged = Candidate::new(CHECKOUT, CHECKOUT, 1_400, t(1), Class::UserData);
    let outcomes = a.execute(&mut ctx, &[forged]).unwrap();

    assert!(matches!(&outcomes[..], [Outcome::Skipped { id, .. }] if id == CHECKOUT));
    assert!(fake.removed().is_empty());
    assert!(fake.exists(CHECKOUT));
}
