//! Integration tests for the `claude-worktrees` adapter, driven entirely
//! against [`FakePlatform`] through the public [`factory`] surface.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_claude_worktrees::factory;
use disk_saver_core::{
    Candidate, Class, ConfigError, Ctx, DecisionLog, Footprint, Outcome, Platform, Pressure, Store,
};
use disk_saver_platform::FakePlatform;

fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

const REPO: &str = "/home/u/dev/sure";
const AGENT: &str = "/home/u/dev/sure/.claude/worktrees/agent-1";
const NAMED: &str = "/home/u/dev/sure/.claude/worktrees/bright-fox";

fn fake() -> FakePlatform {
    FakePlatform::new()
        .with_home("/home/u")
        .with_now(t(100))
        .with_file(format!("{REPO}/.git/HEAD"), "ref: refs/heads/main\n", t(90))
        // An agent's worktree, idle for two months with its build still in it.
        .with_file(
            format!("{AGENT}/.git"),
            format!("gitdir: {REPO}/.git/worktrees/agent-1\n"),
            t(40),
        )
        .with_sized_dir(format!("{AGENT}/target"), 12_000, t(40))
        .with_file(
            format!("{REPO}/.git/worktrees/agent-1/HEAD"),
            "ref: refs/heads/fix/interest\n",
            t(40),
        )
        // A session's worktree that a running agent still holds.
        .with_file(
            format!("{NAMED}/.git"),
            format!("gitdir: {REPO}/.git/worktrees/bright-fox\n"),
            t(99),
        )
        .with_sized_dir(format!("{NAMED}/node_modules"), 300, t(99))
        .with_file(
            format!("{REPO}/.git/worktrees/bright-fox/HEAD"),
            "ref: refs/heads/worktree-bright-fox\n",
            t(99),
        )
        .with_file(
            format!("{REPO}/.git/worktrees/bright-fox/locked"),
            "",
            t(99),
        )
        // `.claude` holds settings too; only `worktrees/` is reported.
        .with_file(format!("{REPO}/.claude/settings.json"), "{}", t(10))
}

fn cfg(text: &str) -> Option<toml::Value> {
    Some(toml::from_str(text).unwrap())
}

/// Observe, plan and report under the heaviest pressure there is, asserting
/// that nothing is ever planned, and return the footprints sorted by id.
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
    let mut items = a.report(&mut ctx).unwrap();
    items.sort_by(|x, y| x.id.cmp(&y.id));
    items
}

#[test]
fn reports_each_worktree_with_its_branch_and_size() {
    let fake = fake();
    let items = report(&fake, None);

    assert_eq!(items.len(), 2);
    let (agent, named) = (&items[0], &items[1]);

    assert_eq!(agent.id, AGENT);
    assert_eq!(agent.label, format!("{AGENT} [fix/interest]"));
    assert_eq!(agent.bytes, fake.dir_size(Path::new(AGENT)).unwrap());
    assert!(agent.bytes >= 12_000);
    assert_eq!(agent.last_used, t(40));

    assert_eq!(named.id, NAMED);
    assert_eq!(
        named.label,
        format!("{NAMED} [worktree-bright-fox, locked]")
    );
}

#[test]
fn a_claude_dir_outside_a_repository_is_ignored() {
    let fake = FakePlatform::new().with_home("/home/u").with_sized_dir(
        "/home/u/notes/.claude/worktrees/x",
        500,
        t(1),
    );
    assert!(report(&fake, None).is_empty());
}

#[test]
fn exclude_drops_a_single_worktree() {
    let items = report(&fake(), cfg("exclude = [\"**/worktrees/agent-*\"]"));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, NAMED);
}

#[test]
fn overlapping_roots_count_a_worktree_once() {
    let items = report(&fake(), cfg("roots = [\"~\", \"~/dev\"]"));
    assert_eq!(items.len(), 2);
}

#[test]
fn execute_never_removes_anything() {
    let fake = fake();
    let mut a = factory().build(None).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(a.name()),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    let forged = Candidate::new(AGENT, AGENT, 12_000, t(1), Class::UserData);
    let outcomes = a.execute(&mut ctx, &[forged]).unwrap();

    assert!(matches!(&outcomes[..], [Outcome::Skipped { id, .. }] if id == AGENT));
    assert!(fake.removed().is_empty());
    assert!(fake.exists(AGENT));
}

#[test]
fn a_bad_exclude_glob_is_a_config_error() {
    match factory().build(cfg("exclude = [\"[unterminated\"]")) {
        Err(ConfigError::Adapter { adapter, .. }) => assert_eq!(adapter, "claude-worktrees"),
        Err(other) => panic!("expected an adapter config error, got {other:?}"),
        Ok(_) => panic!("expected a config error"),
    }
}
