use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_cachedir::native::{Tool, factory};
use disk_saver_core::{Adapter, Candidate, Class, Ctx, DecisionLog, Outcome, Pressure, Store};
use disk_saver_platform::{CommandOutput, FakePlatform};

fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 86400)
}
fn run<R>(
    p: &FakePlatform,
    a: &mut dyn Adapter,
    f: impl FnOnce(&mut dyn Adapter, &mut Ctx) -> R,
) -> R {
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(p, store.bucket(a.name()), Pressure::Normal, &log);
    f(a, &mut ctx)
}
fn fixture(tool: Tool) -> FakePlatform {
    match tool {
        Tool::Uv => FakePlatform::new()
            .with_command(
                "uv",
                &["cache", "dir"],
                CommandOutput::ok("/home/tester/.cache/uv\n"),
            )
            .with_file("~/.cache/uv/archive-v0/blob", b"cache".to_vec(), t(300)),
        Tool::Npm => FakePlatform::new()
            .with_command(
                "npm",
                &["config", "get", "cache"],
                CommandOutput::ok("/home/tester/.npm\n"),
            )
            .with_file("~/.npm/_cacache/content-v2/blob", b"cache".to_vec(), t(300)),
    }
}
#[test]
fn uv_planning_is_read_only_and_maintenance_uses_native_prune() {
    let p = fixture(Tool::Uv).with_command(
        "uv",
        &["cache", "prune", "--cache-dir", "/home/tester/.cache/uv"],
        CommandOutput::ok("nothing to prune"),
    );
    let mut a = factory(Tool::Uv).build(None).unwrap();
    run(&p, a.as_mut(), |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].requires_confirmation);
        assert!(plan[0].label.contains("upper bound"));
        assert_eq!(p.commands_run().len(), 1);
        assert!(p.removed().is_empty());
        let out = a.execute(ctx, &plan).unwrap();
        assert!(
            matches!(out[0], Outcome::Removed { bytes: 0, .. }),
            "native prune kept everything; cannot claim the entire footprint was reclaimed"
        );
        assert!(p.removed().is_empty());
    });
}
#[test]
fn npm_scope_preserves_npx_logs_and_installed_tools() {
    let p = fixture(Tool::Npm)
        .with_file("~/.npm/_npx/tool/bin", b"installed tool".to_vec(), t(399))
        .with_file("~/.npm/_logs/log", b"log".to_vec(), t(399))
        .with_command(
            "npm",
            &["--cache", "/home/tester/.npm", "cache", "clean", "--force"],
            CommandOutput::ok(""),
        );
    let mut a = factory(Tool::Npm).build(None).unwrap();
    run(&p, a.as_mut(), |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].bytes, 5);
        assert!(plan[0].id.ends_with("/_cacache"));
        a.execute(ctx, &plan).unwrap();
    });
    assert!(p.exists("~/.npm/_npx/tool/bin"));
    assert!(p.exists("~/.npm/_logs/log"));
    assert!(p.removed().is_empty());
}
#[test]
fn nested_recent_access_keeps_cache_and_unknown_tool_reports_unavailable() {
    let p = fixture(Tool::Uv);
    p.set_accessed("~/.cache/uv/archive-v0/blob", Some(t(399)));
    let mut a = factory(Tool::Uv).build(None).unwrap();
    run(&p, a.as_mut(), |a, ctx| {
        assert!(a.plan(ctx).unwrap().is_empty())
    });
    let p = FakePlatform::new();
    run(&p, a.as_mut(), |a, ctx| assert!(a.plan(ctx).is_err()));
}
#[test]
fn changed_activity_and_forged_paths_never_run_cleanup() {
    let p = fixture(Tool::Uv);
    let mut a = factory(Tool::Uv).build(None).unwrap();
    run(&p, a.as_mut(), |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        p.set_accessed("~/.cache/uv/archive-v0/blob", Some(t(399)));
        assert!(matches!(
            a.execute(ctx, &plan).unwrap()[0],
            Outcome::Skipped { .. }
        ));
        let forged = Candidate::new("/home/tester/Documents", "bad", 1, t(300), Class::Cache);
        assert!(matches!(
            a.execute(ctx, &[forged]).unwrap()[0],
            Outcome::Skipped { .. }
        ));
    });
    assert!(p.commands_run().iter().all(|c| c.args == ["cache", "dir"]));
}
#[test]
fn symlinked_cache_and_nested_mount_fail_closed() {
    let p = FakePlatform::new()
        .with_command(
            "uv",
            &["cache", "dir"],
            CommandOutput::ok("/home/tester/.cache/uv\n"),
        )
        .with_symlink("~/.cache/uv", "~/Documents", t(300));
    let mut a = factory(Tool::Uv).build(None).unwrap();
    run(&p, a.as_mut(), |a, ctx| assert!(a.plan(ctx).is_err()));
    let p = fixture(Tool::Uv).with_mount("~/.cache/uv/mount");
    run(&p, a.as_mut(), |a, ctx| assert!(a.plan(ctx).is_err()));
}
#[test]
fn native_failure_is_reported_and_has_no_filesystem_fallback() {
    let p = fixture(Tool::Uv).with_command(
        "uv",
        &["cache", "prune", "--cache-dir", "/home/tester/.cache/uv"],
        CommandOutput {
            status: 1,
            stdout: vec![],
            stderr: b"cache busy".to_vec(),
        },
    );
    let mut a = factory(Tool::Uv).build(None).unwrap();
    run(&p, a.as_mut(), |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        assert!(matches!(
            a.execute(ctx, &plan).unwrap()[0],
            Outcome::Failed { .. }
        ));
    });
    assert!(p.removed().is_empty());
}
