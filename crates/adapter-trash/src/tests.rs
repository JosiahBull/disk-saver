//! The units that cannot be reached through the crate's public surface.
//!
//! Everything driven through [`factory`](super::factory) lives in
//! `tests/trash.rs`. What stays here is the pure `.trashinfo` date parser (a
//! table test over inputs whose *parse* outcome is what matters, not the
//! eligibility it happens to produce) and the `io::Error` → [`AdapterError`]
//! mapping, whose `PermissionDenied` arm `FakePlatform` has no way to provoke.

use std::io;
use std::path::Path;

use super::{parse_deletion_date, read_failure, to_unix};
use disk_saver_core::AdapterError;

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
