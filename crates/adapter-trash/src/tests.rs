//! Integration + unit tests for the trash adapter, driven entirely against
//! [`FakePlatform`] (no OS, deterministic clock).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use disk_saver_core::{Candidate, Class, DecisionLog, Outcome, Pressure, Store};
use disk_saver_platform::FakePlatform;

use super::*;

// ── helpers ────────────────────────────────────────────────────────────────

/// Build the adapter from a TOML config fragment (empty string → defaults).
fn build_adapter(cfg: &str) -> Box<dyn Adapter> {
    try_build(cfg).expect("config should be valid")
}

/// Attempt to build the adapter, surfacing config errors.
fn try_build(cfg: &str) -> Result<Box<dyn Adapter>, ConfigError> {
    let raw = if cfg.is_empty() {
        None
    } else {
        Some(toml::from_str::<toml::Value>(cfg).expect("test config is valid TOML"))
    };
    build(raw)
}

/// A `.trashinfo` sidecar body.
fn trashinfo(path: &str, deletion_date: &str) -> String {
    format!("[Trash Info]\nPath={path}\nDeletionDate={deletion_date}\n")
}

/// Candidate labels (== entry names), for order-insensitive assertions.
fn names(cands: &[Candidate]) -> Vec<String> {
    let mut v: Vec<String> = cands.iter().map(|c| c.label.clone()).collect();
    v.sort();
    v
}

// ── config ─────────────────────────────────────────────────────────────────

#[test]
fn defaults_confirm_true_and_60d_max() {
    // A plain entry seeded 100d old should be eligible under Normal (max 60d),
    // and flagged for confirmation (confirm defaults to true).
    let now = FakePlatform::new().now();
    let old = now - Duration::from_secs(100 * DAY);
    let fake = FakePlatform::new()
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")])
        .with_file("~/.Trash/report.pdf", "xxxx", old);
    let store = Store::open_in_memory().unwrap();
    store
        .bucket(NAME)
        .set(
            "entry:report.pdf",
            &EntryRecord {
                first_seen: to_unix(old),
            },
            now,
        )
        .unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    let mut adapter = build_adapter("");
    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(plan[0].requires_confirmation, "confirm defaults to true");
    assert_eq!(plan[0].class, Class::UserData);
    assert_eq!(plan[0].label, "report.pdf");
    assert_eq!(plan[0].bytes, 4);
}

#[test]
fn confirm_false_yields_unflagged_candidate() {
    let now = FakePlatform::new().now();
    let old = now - Duration::from_secs(100 * DAY);
    let fake = FakePlatform::new()
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")])
        .with_file("~/.Trash/report.pdf", "xxxx", old);
    let store = Store::open_in_memory().unwrap();
    store
        .bucket(NAME)
        .set(
            "entry:report.pdf",
            &EntryRecord {
                first_seen: to_unix(old),
            },
            now,
        )
        .unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    let mut adapter = build_adapter("confirm = false");
    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(!plan[0].requires_confirmation);
}

#[test]
fn custom_durations_parse() {
    // max_age = 10d: an entry 8d old is not eligible under Normal.
    let now = FakePlatform::new().now();
    let old = now - Duration::from_secs(8 * DAY);
    let fake = FakePlatform::new()
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")])
        .with_file("~/.Trash/a", "z", old);
    let store = Store::open_in_memory().unwrap();
    store
        .bucket(NAME)
        .set(
            "entry:a",
            &EntryRecord {
                first_seen: to_unix(old),
            },
            now,
        )
        .unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    let mut adapter = build_adapter("max_age = \"10d\"\nmin_age = \"1d\"");
    assert!(adapter.plan(&mut ctx).unwrap().is_empty());
}

#[test]
fn invalid_config_min_gt_max_rejected() {
    match try_build("max_age = \"1d\"\nmin_age = \"7d\"") {
        Err(ConfigError::Adapter { .. }) => {}
        Err(other) => panic!("expected Adapter error, got {other:?}"),
        Ok(_) => panic!("expected an error for min_age > max_age"),
    }
}

#[test]
fn invalid_protect_glob_rejected() {
    match try_build("protect = [\"[\"]") {
        Err(ConfigError::Adapter { .. }) => {}
        Err(other) => panic!("expected Adapter error, got {other:?}"),
        Ok(_) => panic!("expected an error for an invalid glob"),
    }
}

// ── XDG (Linux) layout ───────────────────────────────────────────────────────

#[test]
fn xdg_deletion_date_drives_eligibility() {
    let del_old = parse_deletion_date("2024-01-01T00:00:00").unwrap();
    let del_recent = parse_deletion_date("2024-03-01T00:00:00").unwrap();
    // now = 30 days after the recent deletion → old ≈ 90d, recent = 30d.
    let now = del_recent + Duration::from_secs(30 * DAY);

    let fake = FakePlatform::new()
        .with_now(now)
        .with_file("~/.local/share/Trash/files/old.txt", "0123456789", del_old)
        .with_file(
            "~/.local/share/Trash/info/old.txt.trashinfo",
            trashinfo("/home/tester/old.txt", "2024-01-01T00:00:00"),
            del_old,
        )
        .with_file("~/.local/share/Trash/files/recent.txt", "abc", del_recent)
        .with_file(
            "~/.local/share/Trash/info/recent.txt.trashinfo",
            trashinfo("/home/tester/recent.txt", "2024-03-01T00:00:00"),
            del_recent,
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");

    // Normal: only the 90d-old entry is eligible (max_age 60d).
    {
        let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
        let plan = adapter.plan(&mut ctx).unwrap();
        assert_eq!(names(&plan), vec!["old.txt".to_string()]);
        assert_eq!(plan[0].bytes, 10);
        assert_eq!(plan[0].class, Class::UserData);
    }
    // Scavenge: both are past the 7d min_age floor.
    {
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        let plan = adapter.plan(&mut ctx).unwrap();
        assert_eq!(
            names(&plan),
            vec!["old.txt".to_string(), "recent.txt".to_string()]
        );
    }
    // Comfortable: nothing, ever.
    {
        let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Comfortable, &log);
        assert!(adapter.plan(&mut ctx).unwrap().is_empty());
    }
}

#[test]
fn xdg_scavenge_respects_min_age_floor() {
    let del = parse_deletion_date("2024-05-01T00:00:00").unwrap();
    // Only 3 days old — below the 7d min_age floor, so never eligible.
    let now = del + Duration::from_secs(3 * DAY);
    let fake = FakePlatform::new()
        .with_now(now)
        .with_file("~/.local/share/Trash/files/fresh.txt", "hi", del)
        .with_file(
            "~/.local/share/Trash/info/fresh.txt.trashinfo",
            trashinfo("/home/tester/fresh.txt", "2024-05-01T00:00:00"),
            del,
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(NAME),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    assert!(adapter.plan(&mut ctx).unwrap().is_empty());
}

#[test]
fn xdg_execute_removes_entry_and_sidecar_together() {
    let del = parse_deletion_date("2024-01-01T00:00:00").unwrap();
    let now = del + Duration::from_secs(100 * DAY);
    let fake = FakePlatform::new()
        .with_now(now)
        .with_file("~/.local/share/Trash/files/old.txt", "0123456789", del)
        .with_file(
            "~/.local/share/Trash/info/old.txt.trashinfo",
            trashinfo("/home/tester/old.txt", "2024-01-01T00:00:00"),
            del,
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    let outcomes = adapter.execute(&mut ctx, &plan).unwrap();

    assert!(matches!(&outcomes[0], Outcome::Removed { bytes, .. } if *bytes == 10));
    let removed = fake.removed();
    assert!(removed.contains(&PathBuf::from(
        "/home/tester/.local/share/Trash/files/old.txt"
    )));
    assert!(removed.contains(&PathBuf::from(
        "/home/tester/.local/share/Trash/info/old.txt.trashinfo"
    )));
    assert!(!fake.exists("~/.local/share/Trash/files/old.txt"));
    assert!(!fake.exists("~/.local/share/Trash/info/old.txt.trashinfo"));
}

#[test]
fn xdg_execute_removes_directory_entry_recursively() {
    let del = parse_deletion_date("2024-01-01T00:00:00").unwrap();
    let now = del + Duration::from_secs(100 * DAY);
    let fake = FakePlatform::new()
        .with_now(now)
        .with_file(
            "~/.local/share/Trash/files/proj/inner.bin",
            vec![0u8; 100],
            del,
        )
        .with_file(
            "~/.local/share/Trash/info/proj.trashinfo",
            trashinfo("/home/tester/proj", "2024-01-01T00:00:00"),
            del,
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].bytes, 100, "dir size recurses");
    let outcomes = adapter.execute(&mut ctx, &plan).unwrap();
    assert!(matches!(&outcomes[0], Outcome::Removed { .. }));
    assert!(!fake.exists("~/.local/share/Trash/files/proj"));
    assert!(!fake.exists("~/.local/share/Trash/files/proj/inner.bin"));
    assert!(!fake.exists("~/.local/share/Trash/info/proj.trashinfo"));
}

// ── macOS / plain layout ─────────────────────────────────────────────────────

#[test]
fn macos_first_sighting_grace_then_eligible_after_aging() {
    let fake = FakePlatform::new()
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")])
        .with_file("~/.Trash/oldfile", "data", FakePlatform::new().now());
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    // Run 1: first sighting → stamped, and NOT deleted this run (grace).
    adapter.observe(&mut ctx).unwrap();
    assert!(
        adapter.plan(&mut ctx).unwrap().is_empty(),
        "first-sighting grace: never deleted in the run it was discovered"
    );
    let stamped: EntryRecord = store.bucket(NAME).get("entry:oldfile").unwrap().unwrap();

    // Advance well past max_age.
    fake.advance(Duration::from_secs(100 * DAY));

    // Run 2: stamp preserved, now eligible.
    adapter.observe(&mut ctx).unwrap();
    let again: EntryRecord = store.bucket(NAME).get("entry:oldfile").unwrap().unwrap();
    assert_eq!(
        again.first_seen, stamped.first_seen,
        "first_seen must not move"
    );

    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(plan[0].requires_confirmation);
    assert_eq!(plan[0].class, Class::UserData);

    // Execute: removes the entry and drops its KV record.
    let outcomes = adapter.execute(&mut ctx, &plan).unwrap();
    assert!(matches!(&outcomes[0], Outcome::Removed { bytes, .. } if *bytes == 4));
    assert!(
        fake.removed()
            .contains(&PathBuf::from("/home/tester/.Trash/oldfile"))
    );
    assert!(!fake.exists("~/.Trash/oldfile"));
    assert!(
        store
            .bucket(NAME)
            .get::<EntryRecord>("entry:oldfile")
            .unwrap()
            .is_none(),
        "KV record dropped after removal"
    );
}

#[test]
fn macos_observe_gcs_vanished_entries_and_preserves_present() {
    let now = FakePlatform::new().now();
    let old = to_unix(now - Duration::from_secs(50 * DAY));
    let fake = FakePlatform::new()
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")])
        .with_file("~/.Trash/keep", "k", now);
    let store = Store::open_in_memory().unwrap();
    // Seed a stale record (no matching entry) and a live one.
    store
        .bucket(NAME)
        .set("entry:ghost", &EntryRecord { first_seen: old }, now)
        .unwrap();
    store
        .bucket(NAME)
        .set("entry:keep", &EntryRecord { first_seen: old }, now)
        .unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    adapter.observe(&mut ctx).unwrap();

    assert!(
        store
            .bucket(NAME)
            .get::<EntryRecord>("entry:ghost")
            .unwrap()
            .is_none(),
        "vanished entry GC'd"
    );
    let keep: EntryRecord = store.bucket(NAME).get("entry:keep").unwrap().unwrap();
    assert_eq!(
        keep.first_seen, old,
        "present entry's stamp preserved (age tracking continuity)"
    );
}

// ── permissions / availability ───────────────────────────────────────────────

#[test]
fn unreadable_trash_dir_is_unavailable() {
    // Model an unreadable trash root by making it a non-directory: read_dir then
    // fails with a non-NotFound error, which the adapter maps to Unavailable.
    let fake = FakePlatform::new()
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")])
        .with_file("~/.Trash", "not a dir", FakePlatform::new().now());
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    assert!(matches!(
        adapter.observe(&mut ctx),
        Err(AdapterError::Unavailable(_))
    ));
    assert!(matches!(
        adapter.plan(&mut ctx),
        Err(AdapterError::Unavailable(_))
    ));
    assert!(matches!(
        adapter.check(&mut ctx),
        Err(AdapterError::Unavailable(_))
    ));
}

#[test]
fn permission_denied_carries_full_disk_access_hint() {
    let e = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
    let err = read_failure(Path::new("/home/tester/.Trash"), &e);
    match err {
        AdapterError::Unavailable(msg) => assert!(
            msg.contains("Full Disk Access"),
            "expected FDA hint, got: {msg}"
        ),
        other => panic!("expected Unavailable, got {other:?}"),
    }
    // A non-permission error is still Unavailable, but without the FDA hint.
    let e2 = io::Error::new(io::ErrorKind::NotADirectory, "nope");
    match read_failure(Path::new("/x"), &e2) {
        AdapterError::Unavailable(msg) => assert!(!msg.contains("Full Disk Access")),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn missing_trash_dir_is_empty_not_error() {
    // No trash directory exists at all → no candidates, no error.
    let fake = FakePlatform::new().with_trash_dirs(vec![PathBuf::from("~/.Trash")]);
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);
    adapter.observe(&mut ctx).unwrap();
    assert!(adapter.plan(&mut ctx).unwrap().is_empty());
}

// ── protect globs ────────────────────────────────────────────────────────────

#[test]
fn protect_glob_excludes_matching_entries() {
    let now = FakePlatform::new().now();
    let old = to_unix(now - Duration::from_secs(100 * DAY));
    let fake = FakePlatform::new()
        .with_trash_dirs(vec![PathBuf::from("~/.Trash")])
        .with_file("~/.Trash/secret.txt", "s", now)
        .with_file("~/.Trash/photo.jpg", "p", now);
    let store = Store::open_in_memory().unwrap();
    store
        .bucket(NAME)
        .set("entry:secret.txt", &EntryRecord { first_seen: old }, now)
        .unwrap();
    store
        .bucket(NAME)
        .set("entry:photo.jpg", &EntryRecord { first_seen: old }, now)
        .unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("protect = [\"secret*\"]");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(names(&plan), vec!["photo.jpg".to_string()]);
}

// ── execute edge cases ───────────────────────────────────────────────────────

#[test]
fn execute_skips_entry_that_vanished_before_deletion() {
    let fake = FakePlatform::new().with_trash_dirs(vec![PathBuf::from("~/.Trash")]);
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

    let ghost = Candidate::new(
        "/home/tester/.Trash/ghost",
        "ghost",
        123,
        SystemTime::now(),
        Class::UserData,
    )
    .confirm(true);
    let outcomes = adapter
        .execute(&mut ctx, std::slice::from_ref(&ghost))
        .unwrap();
    assert!(matches!(&outcomes[0], Outcome::Skipped { .. }));
    assert!(
        fake.removed().is_empty(),
        "nothing deleted for a vanished entry"
    );
}

// ── date parsing units ───────────────────────────────────────────────────────

#[test]
fn parse_deletion_date_valid() {
    // 2024-01-01T00:00:00 UTC == unix 1_704_067_200.
    let t = parse_deletion_date("2024-01-01T00:00:00").unwrap();
    assert_eq!(to_unix(t), 1_704_067_200);
    // Tolerates trailing Z and fractional seconds.
    assert_eq!(
        to_unix(parse_deletion_date("2024-01-01T00:00:00Z").unwrap()),
        1_704_067_200
    );
    assert_eq!(
        to_unix(parse_deletion_date("2024-01-01T00:00:00.500").unwrap()),
        1_704_067_200
    );
}

#[test]
fn parse_deletion_date_rejects_garbage() {
    assert!(parse_deletion_date("not-a-date").is_none());
    assert!(parse_deletion_date("2024-13-01T00:00:00").is_none()); // bad month
    assert!(parse_deletion_date("2024-01-01T25:00:00").is_none()); // bad hour
    assert!(parse_deletion_date("2024-01-01").is_none()); // no time
    assert!(parse_deletion_date("").is_none());
}

#[test]
fn xdg_entry_with_missing_sidecar_is_treated_as_fresh() {
    // No .trashinfo → last_used defaults to now → age 0 → never eligible.
    let now = FakePlatform::new().now();
    let fake = FakePlatform::new()
        .with_now(now)
        // Make it XDG by creating an info/ dir (with an unrelated sidecar).
        .with_file("~/.local/share/Trash/info/.keep", "", now)
        .with_dir("~/.local/share/Trash/info", now)
        .with_file("~/.local/share/Trash/files/orphan.txt", "data", now);
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = build_adapter("");
    let mut ctx = Ctx::new(
        &fake,
        store.bucket(NAME),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    assert!(adapter.plan(&mut ctx).unwrap().is_empty());
}
