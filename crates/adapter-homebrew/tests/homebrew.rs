use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_homebrew::factory;
use disk_saver_core::{Ctx, DecisionLog, Outcome, Pressure, Store};
use disk_saver_platform::{CommandOutput, FakePlatform};

fn t(d: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(d * 86400)
}

fn archive() -> String {
    format!("{}--package.tar.gz", "a".repeat(64))
}

#[test]
fn only_completed_hashed_downloads_are_candidates_and_installed_packages_survive() {
    let path = format!(
        "/home/tester/Library/Caches/Homebrew/downloads/{}",
        archive()
    );
    let p = FakePlatform::new()
        .with_command(
            "brew",
            &["--cache"],
            CommandOutput::ok("/home/tester/Library/Caches/Homebrew\n"),
        )
        .with_file(&path, b"archive".to_vec(), t(300))
        .with_file(format!("{path}.incomplete"), b"partial".to_vec(), t(300))
        .with_file(
            "~/Library/Caches/Homebrew/downloads/unrelated.tar.gz",
            b"keep".to_vec(),
            t(300),
        )
        .with_symlink(format!("{path}.zip"), "~/Documents", t(300))
        .with_file(
            "/opt/homebrew/Cellar/tool/1/bin/tool",
            b"installed".to_vec(),
            t(300),
        )
        .with_command(
            "lsof",
            &["-t", "--", &path],
            CommandOutput {
                status: 1,
                stdout: vec![],
                stderr: vec![],
            },
        );
    let mut a = factory().build(None).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&p, store.bucket("homebrew"), Pressure::Normal, &log);
    let plan = a.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(
        !plan[0].requires_confirmation,
        "archives are removed without asking"
    );
    assert!(p.removed().is_empty());
    assert!(p.commands_run().iter().all(|c| c.args == ["--cache"]));
    assert!(matches!(
        a.execute(&mut ctx, &plan).unwrap()[0],
        Outcome::Removed { .. }
    ));
    assert!(p.exists("/opt/homebrew/Cellar/tool/1/bin/tool"));
    assert!(p.exists(format!("{path}.incomplete")));
    assert_eq!(p.removed().len(), 1);
}

#[test]
fn recent_access_open_files_and_unavailable_open_file_checks_are_protected() {
    let path = format!("/home/tester/.cache/Homebrew/downloads/{}", archive());
    let p = FakePlatform::new().with_file(&path, b"archive".to_vec(), t(300));
    let mut a = factory().build(None).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&p, store.bucket("homebrew"), Pressure::Normal, &log);
    let plan = a.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(matches!(
        a.execute(&mut ctx, &plan).unwrap()[0],
        Outcome::Skipped { .. }
    ));
    p.set_accessed(&path, Some(t(399)));
    assert!(a.plan(&mut ctx).unwrap().is_empty());
    assert!(p.removed().is_empty());
}

#[test]
fn configured_cache_cannot_escape_through_traversal_or_an_open_archive() {
    let path = format!("/home/tester/cache/downloads/{}", archive());
    let p = FakePlatform::new()
        .with_file(&path, b"archive".to_vec(), t(300))
        .with_command("lsof", &["-t", "--", &path], CommandOutput::ok("12345\n"));
    let cfg = toml::from_str("cache_dir = '~/cache'").unwrap();
    let mut a = factory().build(Some(cfg)).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&p, store.bucket("homebrew"), Pressure::Normal, &log);
    let plan = a.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(matches!(
        a.execute(&mut ctx, &plan).unwrap()[0],
        Outcome::Skipped { .. }
    ));
    assert!(p.removed().is_empty());
    let mut forged = plan[0].clone();
    forged.id = format!(
        "/home/tester/cache/downloads/../../../Documents/{}",
        archive()
    );
    assert!(matches!(
        a.execute(&mut ctx, &[forged]).unwrap()[0],
        Outcome::Skipped { .. }
    ));
}

#[test]
fn confirm_queues_archives_for_approval() {
    let path = format!("/home/tester/cache/downloads/{}", archive());
    let p = FakePlatform::new().with_file(&path, b"archive".to_vec(), t(300));
    let cfg = toml::from_str("cache_dir = '~/cache'\nconfirm = true").unwrap();
    let mut a = factory().build(Some(cfg)).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&p, store.bucket("homebrew"), Pressure::Normal, &log);
    let plan = a.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(plan[0].requires_confirmation);
}
