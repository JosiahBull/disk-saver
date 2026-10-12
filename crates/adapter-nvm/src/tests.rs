use std::path::Path;
use std::time::{Duration, SystemTime};

use disk_saver_core::{Adapter, Class, Ctx, DecisionLog, Outcome, Pressure, Store};
use disk_saver_platform::{CommandOutput, FakePlatform};

use super::{QUERY, UNINSTALL, command, factory};

fn t(days: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(days * 86400)
}

fn no_open_files() -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: vec![],
        stderr: vec![],
    }
}

fn fixture() -> FakePlatform {
    let root = Path::new("/home/tester/.nvm");
    let mut p = FakePlatform::new()
        .with_file("~/.nvm/nvm.sh", b"nvm".to_vec(), t(200))
        .with_command(
            "bash",
            &command(root, QUERY, "default")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("v24.18.1\n"),
        )
        .with_command(
            "bash",
            &command(root, QUERY, "current")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("system\n"),
        );
    for version in ["v24.13.0", "v24.18.1"] {
        let path = format!("/home/tester/.nvm/versions/node/{version}/bin/node");
        p = p
            .with_file(&path, b"node binary".to_vec(), t(200))
            .with_command("lsof", &["-t", "--", &path], no_open_files());
    }
    p
}

fn run<R>(
    p: &FakePlatform,
    cfg: Option<toml::Value>,
    f: impl FnOnce(&mut dyn Adapter, &mut Ctx) -> R,
) -> R {
    let mut a = factory().build(cfg).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(p, store.bucket("nvm"), Pressure::Normal, &log);
    f(a.as_mut(), &mut ctx)
}

#[test]
fn old_version_requires_approval_with_access_evidence_and_protects_default() {
    let p = fixture();
    run(&p, None, |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].id.ends_with("v24.13.0"));
        assert!(plan[0].requires_confirmation);
        assert_eq!(plan[0].class, Class::UserData);
        assert!(plan[0].label.contains("filesystem proxy"));
        assert!(plan[0].label.contains("global packages"));
        assert!(p.removed().is_empty());
        assert!(
            p.commands_run()
                .iter()
                .all(|c| !c.args.contains(&UNINSTALL.to_string()))
        );
    });
}

#[test]
fn project_pin_protects_old_version() {
    let root = Path::new("/home/tester/.nvm");
    let p = fixture()
        .with_file("~/dev/app/.nvmrc", b"24.13.0\n".to_vec(), t(200))
        .with_command(
            "bash",
            &command(root, QUERY, "24.13.0")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("v24.13.0\n"),
        );
    run(&p, None, |a, ctx| assert!(a.plan(ctx).unwrap().is_empty()));
}

#[test]
fn current_version_is_protected_and_recent_access_cannot_be_retired() {
    let p = fixture();
    p.set_accessed("~/.nvm/versions/node/v24.13.0/bin/node", Some(t(399)));
    run(&p, None, |a, ctx| assert!(a.plan(ctx).unwrap().is_empty()));
    let root = Path::new("/home/tester/.nvm");
    let p = FakePlatform::new()
        .with_file(
            "~/.nvm/versions/node/v24.13.0/bin/node",
            b"node".to_vec(),
            t(200),
        )
        .with_command(
            "bash",
            &command(root, QUERY, "default")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("v24.18.1\n"),
        )
        .with_command(
            "bash",
            &command(root, QUERY, "current")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("v24.13.0\n"),
        );
    run(&p, None, |a, ctx| assert!(a.plan(ctx).unwrap().is_empty()));
}

#[test]
fn approval_rechecks_access_pins_and_requires_native_uninstall() {
    let p = fixture().with_command(
        "bash",
        &command(Path::new("/home/tester/.nvm"), UNINSTALL, "v24.13.0")
            .args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        CommandOutput::ok("Uninstalled"),
    );
    run(&p, None, |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        let outcomes = a.execute(ctx, &plan).unwrap();
        assert!(
            matches!(outcomes[0], Outcome::Removed { bytes: 0, .. }),
            "fake native command keeps the tree, so no bytes can be claimed"
        );
        p.set_accessed("~/.nvm/versions/node/v24.13.0/bin/node", Some(t(399)));
        assert!(matches!(
            a.execute(ctx, &plan).unwrap()[0],
            Outcome::Skipped { .. }
        ));
    });
    assert!(p.removed().is_empty());
}

#[test]
fn unknown_access_and_symlinked_node_fail_closed() {
    let p = fixture();
    p.set_accessed("~/.nvm/versions/node/v24.13.0/bin/node", None);
    run(&p, None, |a, ctx| assert!(a.plan(ctx).unwrap().is_empty()));
    let p = fixture().with_symlink(
        "~/.nvm/versions/node/v24.13.0/bin/node",
        "~/important",
        t(200),
    );
    run(&p, None, |a, ctx| assert!(a.plan(ctx).unwrap().is_empty()));
}

#[test]
fn cache_and_versions_can_be_configured_independently() {
    let path = "/home/tester/.nvm/.cache/bin/node-v24.13.0/node.tar.xz";
    let p = fixture()
        .with_file(path, b"archive".to_vec(), t(200))
        .with_file(
            "~/.nvm/.cache/src/node-v24.13.0/node.tar.gz",
            b"source".to_vec(),
            t(200),
        )
        .with_file(
            "~/.nvm/.cache/bin/node-v24.13.0/node.tar.xz.incomplete",
            b"partial".to_vec(),
            t(200),
        )
        .with_command("lsof", &["-t", "--", path], no_open_files());
    let cfg = toml::from_str("include_versions = false\nconfirm = false").unwrap();
    run(&p, Some(cfg), |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        assert_eq!(plan.len(), 2);
        assert!(plan.iter().all(|c| !c.requires_confirmation));
        assert!(plan.iter().all(|c| c.id.contains("/.cache/")));
    });
    let cfg = toml::from_str("confirm = false").unwrap();
    run(&p, Some(cfg), |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        assert!(
            plan.iter()
                .find(|c| c.id.ends_with("v24.13.0"))
                .unwrap()
                .requires_confirmation
        );
    });
}

#[test]
fn unsafe_roots_and_unknown_default_fail_closed() {
    let p = fixture().with_symlink("~/dev", "~/other", t(200));
    run(&p, None, |a, ctx| assert!(a.plan(ctx).is_err()));
    let p = FakePlatform::new().with_file(
        "~/.nvm/versions/node/v24.13.0/bin/node",
        b"node".to_vec(),
        t(200),
    );
    run(&p, None, |a, ctx| assert!(a.plan(ctx).is_err()));
}

#[test]
fn newly_pinned_or_running_version_is_skipped_at_approval() {
    let p = fixture();
    let plan = run(&p, None, |a, ctx| a.plan(ctx).unwrap());
    let root = Path::new("/home/tester/.nvm");
    let pinned = fixture()
        .with_file("~/dev/app/.node-version", b"24.13.0".to_vec(), t(399))
        .with_command(
            "bash",
            &command(root, QUERY, "24.13.0")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("v24.13.0"),
        );
    run(&pinned, None, |a, ctx| {
        assert!(matches!(
            a.execute(ctx, &plan).unwrap()[0],
            Outcome::Skipped { .. }
        ))
    });
    let running = FakePlatform::new()
        .with_file(
            "~/.nvm/versions/node/v24.13.0/bin/node",
            b"node binary".to_vec(),
            t(200),
        )
        .with_command(
            "bash",
            &command(root, QUERY, "default")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("v24.18.1"),
        )
        .with_command(
            "bash",
            &command(root, QUERY, "current")
                .args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            CommandOutput::ok("system"),
        )
        .with_command(
            "lsof",
            &[
                "-t",
                "--",
                "/home/tester/.nvm/versions/node/v24.13.0/bin/node",
            ],
            CommandOutput::ok("12345\n"),
        );
    run(&running, None, |a, ctx| {
        assert!(matches!(
            a.execute(ctx, &plan).unwrap()[0],
            Outcome::Skipped { .. }
        ))
    });
    assert!(running.removed().is_empty());
    assert!(pinned.removed().is_empty());
}

#[test]
fn environment_override_locates_downloads_and_missing_lsof_blocks_retirement() {
    let p = FakePlatform::new()
        .with_env("NVM_DIR", "/home/tester/custom-nvm")
        .with_file(
            "~/custom-nvm/.cache/bin/node-v24.13.0/node.tar.xz",
            b"archive".to_vec(),
            t(200),
        );
    let cfg = toml::from_str("include_versions = false").unwrap();
    run(&p, Some(cfg), |a, ctx| {
        let plan = a.plan(ctx).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].id.contains("custom-nvm"));
        assert!(matches!(
            a.execute(ctx, &plan).unwrap()[0],
            Outcome::Skipped { .. }
        ));
    });
    assert!(p.removed().is_empty());
}
