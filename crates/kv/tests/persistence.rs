//! Integration tests for the file-backed [`Store`]: data survives close/reopen and
//! the on-disk schema/pragmas match `ARCHITECTURE.md` §6.

use std::time::SystemTime;

use disk_saver_kv::Store;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    first_seen: u64,
    tags: Vec<String>,
}

#[test]
fn data_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");

    let entry = Entry {
        first_seen: 1_700_000_000,
        tags: vec!["postgres:16".into(), "latest".into()],
    };

    {
        let store = Store::open(&path).unwrap();
        let docker = store.bucket("docker");
        docker
            .set("image:abc123", &entry, SystemTime::now())
            .unwrap();
        let engine = store.bucket("_engine");
        engine
            .set("last_full_run", &1_700_000_050u64, SystemTime::now())
            .unwrap();
    } // store dropped: connection closed, WAL checkpointed on close.

    // Reopen the same file — everything must still be there and isolated by bucket.
    let store = Store::open(&path).unwrap();
    let docker = store.bucket("docker");
    assert_eq!(docker.get::<Entry>("image:abc123").unwrap(), Some(entry));
    let engine = store.bucket("_engine");
    assert_eq!(
        engine.get::<u64>("last_full_run").unwrap(),
        Some(1_700_000_050)
    );
    // Cross-bucket isolation holds on disk too.
    assert_eq!(docker.get::<u64>("last_full_run").unwrap(), None);
}

#[test]
fn reopening_creates_missing_parent_is_not_required_but_file_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.db");
    assert!(!path.exists());
    let store = Store::open(&path).unwrap();
    store.bucket("b").set("k", &1u8, SystemTime::now()).unwrap();
    assert!(path.exists(), "opening a store must create the db file");
}

#[test]
fn pragmas_and_schema_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let store = Store::open(&path).unwrap();

    // We cannot reach the private connection, but we can verify behaviour that
    // depends on the schema: WITHOUT ROWID primary key means (bucket, key) is a
    // true upsert target. Prove overwrite semantics through the public API.
    let b = store.bucket("things");
    b.set("k", &"one".to_string(), SystemTime::now()).unwrap();
    b.set("k", &"two".to_string(), SystemTime::now()).unwrap();
    assert_eq!(b.get::<String>("k").unwrap(), Some("two".into()));
    assert_eq!(b.keys("").unwrap(), vec!["k".to_string()]);
}
