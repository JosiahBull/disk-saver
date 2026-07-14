//! Integration tests for `FakePlatform` — this is load-bearing test infra for
//! every downstream adapter, so it is exercised thoroughly here.
#![cfg(feature = "test-util")]

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_platform::{
    CommandOutput, CommandSpec, FakePlatform, FileKind, Notification, Platform, Urgency,
};

/// A `SystemTime` `secs` after the Unix epoch.
fn t(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn defaults_match_contract() {
    let p = FakePlatform::new();
    assert_eq!(p.home_dir(), PathBuf::from("/home/tester"));
    assert_eq!(p.user_cache_dir(), PathBuf::from("/home/tester/.cache"));
    assert_eq!(
        p.trash_dirs(),
        vec![PathBuf::from("/home/tester/.local/share/Trash")]
    );
    // Clock is fixed, positive, and well past the epoch.
    assert_eq!(
        p.now(),
        UNIX_EPOCH + Duration::from_secs(400 * 24 * 60 * 60)
    );
    let du = p.disk_usage(Path::new("/")).unwrap();
    assert!(du.available > 0 && du.total > du.available);
}

#[test]
fn default_impl_matches_new() {
    let a = FakePlatform::default();
    assert_eq!(a.home_dir(), FakePlatform::new().home_dir());
}

#[test]
fn tilde_expands_in_builder_and_query() {
    let p = FakePlatform::new().with_file("~/notes.txt", b"hi".to_vec(), t(10));
    // Query with `~` and with the absolute form both resolve.
    assert!(p.exists("~/notes.txt"));
    assert!(p.exists("/home/tester/notes.txt"));
    let meta = p.metadata(Path::new("~/notes.txt")).unwrap();
    assert_eq!(meta.kind, FileKind::File);
    assert_eq!(meta.len, 2);
}

#[test]
fn with_home_flows_into_cache_and_trash_defaults() {
    let p = FakePlatform::new().with_home("/Users/bob");
    assert_eq!(p.home_dir(), PathBuf::from("/Users/bob"));
    assert_eq!(p.user_cache_dir(), PathBuf::from("/Users/bob/.cache"));
    assert_eq!(
        p.trash_dirs(),
        vec![PathBuf::from("/Users/bob/.local/share/Trash")]
    );
    // And `~` in later builder calls expands against the new home.
    let p = p.with_file("~/x", b"y".to_vec(), t(1));
    assert!(p.exists("/Users/bob/x"));
}

#[test]
fn cache_and_trash_overrides_win() {
    let p = FakePlatform::new()
        .with_cache_dir("~/Library/Caches")
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")]);
    assert_eq!(
        p.user_cache_dir(),
        PathBuf::from("/home/tester/Library/Caches")
    );
    assert_eq!(p.trash_dirs(), vec![PathBuf::from("/home/tester/.Trash")]);
}

#[test]
fn file_and_dir_metadata() {
    let p = FakePlatform::new()
        .with_file("/a/b/file.txt", b"hello".to_vec(), t(100))
        .with_dir("/a/emptydir", t(200));

    let fm = p.metadata(Path::new("/a/b/file.txt")).unwrap();
    assert_eq!(fm.kind, FileKind::File);
    assert_eq!(fm.len, 5);
    assert_eq!(fm.modified, t(100));

    let dm = p.metadata(Path::new("/a/emptydir")).unwrap();
    assert_eq!(dm.kind, FileKind::Dir);
    assert_eq!(dm.len, 0);
    assert_eq!(dm.modified, t(200));

    // Auto-created ancestors exist as directories.
    assert_eq!(p.metadata(Path::new("/a")).unwrap().kind, FileKind::Dir);
    assert_eq!(p.metadata(Path::new("/a/b")).unwrap().kind, FileKind::Dir);
}

#[test]
fn metadata_missing_is_not_found() {
    let p = FakePlatform::new();
    let err = p.metadata(Path::new("/nope")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn read_dir_lists_children_sorted_with_kinds() {
    let p = FakePlatform::new()
        .with_file("/proj/package.json", b"{}".to_vec(), t(1))
        .with_dir("/proj/node_modules", t(1))
        .with_symlink("/proj/link", "/elsewhere", t(1));

    let entries = p.read_dir(Path::new("/proj")).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.file_name.as_str()).collect();
    // Sorted ascending by path.
    assert_eq!(names, vec!["link", "node_modules", "package.json"]);
    for e in &entries {
        assert!(e.path.is_absolute());
    }
    let kind = |name: &str| entries.iter().find(|e| e.file_name == name).unwrap().kind;
    assert_eq!(kind("package.json"), FileKind::File);
    assert_eq!(kind("node_modules"), FileKind::Dir);
    assert_eq!(kind("link"), FileKind::Symlink);
}

#[test]
fn read_dir_on_file_is_not_a_directory() {
    let p = FakePlatform::new().with_file("/f", b"x".to_vec(), t(1));
    let err = p.read_dir(Path::new("/f")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
}

#[test]
fn read_dir_on_sized_dir_is_empty() {
    let p = FakePlatform::new().with_sized_dir("/big", 999, t(1));
    assert!(p.read_dir(Path::new("/big")).unwrap().is_empty());
}

#[test]
fn read_dir_missing_is_not_found() {
    let p = FakePlatform::new();
    assert_eq!(
        p.read_dir(Path::new("/nope")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn read_to_string_reads_files_only() {
    let p = FakePlatform::new()
        .with_file("/a.txt", b"content".to_vec(), t(1))
        .with_dir("/d", t(1));
    assert_eq!(p.read_to_string(Path::new("/a.txt")).unwrap(), "content");
    // Directory → error, not a panic.
    assert!(p.read_to_string(Path::new("/d")).is_err());
    // Missing → NotFound.
    assert_eq!(
        p.read_to_string(Path::new("/missing")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn read_to_string_rejects_non_utf8() {
    let p = FakePlatform::new().with_file("/bin", vec![0xff, 0x00, 0xfe], t(1));
    assert_eq!(
        p.read_to_string(Path::new("/bin")).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn dir_size_sums_nested_files() {
    let p = FakePlatform::new()
        .with_file("/root/a", vec![0u8; 100], t(1))
        .with_file("/root/sub/b", vec![0u8; 50], t(1))
        .with_file("/root/sub/deep/c", vec![0u8; 25], t(1));
    assert_eq!(p.dir_size(Path::new("/root")).unwrap(), 175);
    assert_eq!(p.dir_size(Path::new("/root/sub")).unwrap(), 75);
}

#[test]
fn dir_size_includes_sized_dir_without_recursing() {
    let p = FakePlatform::new()
        .with_file("/root/a", vec![0u8; 10], t(1))
        .with_sized_dir("/root/cache", 1_000_000, t(1));
    // 10 + 1_000_000; the sized dir contributes its sentinel size.
    assert_eq!(p.dir_size(Path::new("/root")).unwrap(), 1_000_010);
    // Sizing the sized dir directly yields its size.
    assert_eq!(p.dir_size(Path::new("/root/cache")).unwrap(), 1_000_000);
}

#[test]
fn dir_size_on_file_returns_len() {
    let p = FakePlatform::new().with_file("/f", vec![0u8; 42], t(1));
    assert_eq!(p.dir_size(Path::new("/f")).unwrap(), 42);
}

#[test]
fn dir_size_missing_is_not_found() {
    let p = FakePlatform::new();
    assert_eq!(
        p.dir_size(Path::new("/nope")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn dir_size_never_follows_symlinks() {
    // The symlink target holds a large file, but sizing the dir that contains
    // the symlink must not count it.
    let p = FakePlatform::new()
        .with_file("/target/huge", vec![0u8; 5000], t(1))
        .with_file("/proj/small", vec![0u8; 7], t(1))
        .with_symlink("/proj/link", "/target", t(1));
    assert_eq!(p.dir_size(Path::new("/proj")).unwrap(), 7);
    // A symlink as the top path is not followed either.
    assert_eq!(p.dir_size(Path::new("/proj/link")).unwrap(), 0);
}

#[test]
fn remove_file_records_and_deletes() {
    let p = FakePlatform::new().with_file("/x/y.txt", b"z".to_vec(), t(1));
    assert!(p.exists("/x/y.txt"));
    p.remove_file(Path::new("/x/y.txt")).unwrap();
    assert!(!p.exists("/x/y.txt"));
    assert_eq!(p.removed(), vec![PathBuf::from("/x/y.txt")]);
    // Parent dir remains.
    assert!(p.exists("/x"));
}

#[test]
fn remove_file_on_directory_errors() {
    let p = FakePlatform::new().with_dir("/d", t(1));
    let err = p.remove_file(Path::new("/d")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::IsADirectory);
    assert!(p.exists("/d"));
    assert!(p.removed().is_empty());
}

#[test]
fn remove_file_missing_is_not_found_and_unrecorded() {
    let p = FakePlatform::new();
    assert_eq!(
        p.remove_file(Path::new("/nope")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(p.removed().is_empty());
}

#[test]
fn remove_dir_all_removes_all_descendants() {
    let p = FakePlatform::new()
        .with_file("/proj/node_modules/a", vec![0u8; 1], t(1))
        .with_file("/proj/node_modules/deep/b", vec![0u8; 1], t(1))
        .with_file("/proj/keep.txt", b"keep".to_vec(), t(1));

    p.remove_dir_all(Path::new("/proj/node_modules")).unwrap();

    assert!(!p.exists("/proj/node_modules"));
    assert!(!p.exists("/proj/node_modules/a"));
    assert!(!p.exists("/proj/node_modules/deep/b"));
    // Sibling and parent survive.
    assert!(p.exists("/proj/keep.txt"));
    assert!(p.exists("/proj"));
    assert_eq!(p.removed(), vec![PathBuf::from("/proj/node_modules")]);
}

#[test]
fn remove_dir_all_does_not_touch_prefix_siblings() {
    // `/a/bc` must not be considered "under" `/a/b`.
    let p = FakePlatform::new()
        .with_file("/a/b/inside", b"1".to_vec(), t(1))
        .with_file("/a/bc/outside", b"2".to_vec(), t(1));
    p.remove_dir_all(Path::new("/a/b")).unwrap();
    assert!(!p.exists("/a/b/inside"));
    assert!(p.exists("/a/bc/outside"));
}

#[test]
fn remove_dir_all_on_symlink_unlinks_only_the_link() {
    let p = FakePlatform::new()
        .with_file("/target/precious", b"data".to_vec(), t(1))
        .with_symlink("/proj/link", "/target", t(1));
    p.remove_dir_all(Path::new("/proj/link")).unwrap();
    assert!(!p.exists("/proj/link"));
    // Target subtree untouched.
    assert!(p.exists("/target"));
    assert!(p.exists("/target/precious"));
    assert_eq!(p.removed(), vec![PathBuf::from("/proj/link")]);
}

#[test]
fn remove_dir_all_missing_is_not_found() {
    let p = FakePlatform::new();
    assert_eq!(
        p.remove_dir_all(Path::new("/nope")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn removals_recorded_in_call_order() {
    let p = FakePlatform::new()
        .with_file("/one", b"1".to_vec(), t(1))
        .with_file("/two", b"2".to_vec(), t(1))
        .with_dir("/three", t(1));
    p.remove_file(Path::new("/two")).unwrap();
    p.remove_file(Path::new("/one")).unwrap();
    p.remove_dir_all(Path::new("/three")).unwrap();
    assert_eq!(
        p.removed(),
        vec![
            PathBuf::from("/two"),
            PathBuf::from("/one"),
            PathBuf::from("/three")
        ]
    );
}

#[test]
fn command_exact_match_wins_over_prefix() {
    let p = FakePlatform::new()
        .with_command_prefix("docker", &["ps"], CommandOutput::ok("prefix"))
        .with_command("docker", &["ps", "-a"], CommandOutput::ok("exact"));

    // Exact match takes priority even though a prefix rule also matches.
    let out = p
        .run_command(&CommandSpec::new("docker", ["ps", "-a"]))
        .unwrap();
    assert_eq!(out.stdout_string(), "exact");

    // No exact rule → prefix rule applies.
    let out = p.run_command(&CommandSpec::new("docker", ["ps"])).unwrap();
    assert_eq!(out.stdout_string(), "prefix");

    // Prefix also matches longer arg lists that start with the prefix.
    let out = p
        .run_command(&CommandSpec::new("docker", ["ps", "--format", "json"]))
        .unwrap();
    assert_eq!(out.stdout_string(), "prefix");
}

#[test]
fn command_error_rule_simulates_dead_daemon() {
    let p = FakePlatform::new().with_command_error("docker", io::ErrorKind::ConnectionRefused);
    let err = p
        .run_command(&CommandSpec::new("docker", ["version"]))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
}

#[test]
fn command_no_match_is_not_found() {
    let p = FakePlatform::new();
    let err = p
        .run_command(&CommandSpec::new("mystery", ["x"]))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn canned_response_wins_over_error_rule() {
    // A specific exact rule for a program should take priority over an error
    // rule for the same program (canned response before canned error).
    let p = FakePlatform::new()
        .with_command_error("docker", io::ErrorKind::ConnectionRefused)
        .with_command("docker", &["version"], CommandOutput::ok("Client 1.2"));
    let out = p
        .run_command(&CommandSpec::new("docker", ["version"]))
        .unwrap();
    assert_eq!(out.stdout_string(), "Client 1.2");
    // But an unmatched subcommand of the same program still hits the error.
    let err = p
        .run_command(&CommandSpec::new("docker", ["ps"]))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
}

#[test]
fn commands_run_recorded_in_order() {
    let p = FakePlatform::new()
        .with_command("a", &["1"], CommandOutput::ok(""))
        .with_command("b", &["2"], CommandOutput::ok(""));
    let _ = p.run_command(&CommandSpec::new("a", ["1"]));
    let _ = p.run_command(&CommandSpec::new("b", ["2"]));
    let _ = p.run_command(&CommandSpec::new("missing", ["x"])); // still recorded
    let run = p.commands_run();
    assert_eq!(run.len(), 3);
    assert_eq!(run[0].program, "a");
    assert_eq!(run[1].program, "b");
    assert_eq!(run[2].program, "missing");
}

#[test]
fn notifications_recorded_in_order() {
    let p = FakePlatform::new();
    p.notify(&Notification {
        title: "first".into(),
        body: "b1".into(),
        urgency: Urgency::Low,
    })
    .unwrap();
    p.notify(&Notification {
        title: "second".into(),
        body: "b2".into(),
        urgency: Urgency::Critical,
    })
    .unwrap();
    let ns = p.notifications();
    assert_eq!(ns.len(), 2);
    assert_eq!(ns[0].title, "first");
    assert_eq!(ns[0].urgency, Urgency::Low);
    assert_eq!(ns[1].title, "second");
    assert_eq!(ns[1].urgency, Urgency::Critical);
}

#[test]
fn clock_set_and_advance() {
    let p = FakePlatform::new().with_now(t(1000));
    assert_eq!(p.now(), t(1000));
    p.advance(Duration::from_secs(50));
    assert_eq!(p.now(), t(1050));
    p.set_now(t(9999));
    assert_eq!(p.now(), t(9999));
}

#[test]
fn free_space_set_via_builder_and_mutator() {
    let p = FakePlatform::new().with_free_space(10, 100);
    let du = p.disk_usage(Path::new("/")).unwrap();
    assert_eq!(du.available, 10);
    assert_eq!(du.total, 100);

    p.set_free_space(5, 100);
    let du = p.disk_usage(Path::new("/")).unwrap();
    assert_eq!(du.available, 5);
    assert_eq!(du.total, 100);
}

#[test]
fn is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FakePlatform>();
    // Usable as a trait object behind a shared reference (as the engine holds it).
    let p = FakePlatform::new();
    let dyn_ref: &dyn Platform = &p;
    assert!(dyn_ref.now() > UNIX_EPOCH);
}

// ── device boundaries (mounts) — ARCHITECTURE.md §11.8 ───────────────────────

#[test]
fn metadata_reports_distinct_dev_across_a_mount() {
    // A mount nested under a scanned root reports a different `dev`.
    let p = FakePlatform::new()
        .with_dir("/root", t(1))
        .with_file("/root/a.txt", b"x".to_vec(), t(1))
        .with_mount("/root/mnt")
        .with_file("/root/mnt/data.bin", b"precious".to_vec(), t(1));

    let root_dev = p.metadata(Path::new("/root")).unwrap().dev;
    let same = p.metadata(Path::new("/root/a.txt")).unwrap().dev;
    let mount_dev = p.metadata(Path::new("/root/mnt")).unwrap().dev;
    let under_mount = p.metadata(Path::new("/root/mnt/data.bin")).unwrap().dev;

    assert_eq!(root_dev, same, "same filesystem shares a device id");
    assert_ne!(root_dev, mount_dev, "the mount is a different device");
    assert_eq!(
        mount_dev, under_mount,
        "everything under the mount shares its device"
    );
}

#[test]
fn remove_dir_all_never_crosses_a_mount_boundary() {
    // Deleting /root must NOT touch the contents of the /root/mnt mount.
    let p = FakePlatform::new()
        .with_dir("/root", t(1))
        .with_file("/root/junk.txt", b"junk".to_vec(), t(1))
        .with_mount("/root/mnt")
        .with_file("/root/mnt/data.bin", b"precious".to_vec(), t(1));

    let err = p.remove_dir_all(Path::new("/root")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::DirectoryNotEmpty);

    // The mounted volume's contents survive; the same-device junk is gone.
    assert!(p.exists("/root/mnt/data.bin"), "mounted data must survive");
    assert!(p.exists("/root/mnt"), "mount point must survive");
    assert!(
        !p.exists("/root/junk.txt"),
        "same-device content is removed"
    );
}
