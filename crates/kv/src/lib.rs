//! `disk-saver-kv` — a deliberately tiny sqlite-backed key-value store.
//!
//! It behaves like a persistent `Map<(bucket, key), json>`: values are serialised
//! to JSON via serde so the on-disk database is inspectable with the `sqlite3` CLI
//! (`sqlite3 state.db 'select * from kv'`). Each logical namespace ("bucket") is
//! isolated from every other, so an adapter cannot see or clobber another's state.
//!
//! Intended for single-threaded use — the disk-saver engine runs adapters
//! sequentially, so [`Store`] holds a bare [`rusqlite::Connection`] with no locking.
//!
//! See `ARCHITECTURE.md` §6 for the schema and rationale.

#![forbid(unsafe_code)]

use rusqlite::{Connection, OptionalExtension};

/// Result type used throughout this crate.
pub type Result<T> = std::result::Result<T, KvError>;

/// Errors produced by the key-value store.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// An error originating from the underlying sqlite layer.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A (de)serialisation error while converting a value to/from JSON.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persistent `Map<(bucket, key), json>`. Single-threaded use (the engine is sequential).
///
/// Open one with [`Store::open`] (file-backed) or [`Store::open_in_memory`] (tests),
/// then obtain namespaced handles with [`Store::bucket`].
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if absent) a file-backed store at `path`.
    ///
    /// Applies the schema and pragmas from `ARCHITECTURE.md` §6 (WAL journal mode,
    /// `synchronous = NORMAL`, `busy_timeout = 5000`, `user_version = 1`).
    pub fn open(path: &std::path::Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Store { conn })
    }

    /// Open an ephemeral in-memory store (primarily for tests).
    ///
    /// WAL mode is effectively a no-op for `:memory:` databases; that is fine.
    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Store { conn })
    }

    /// Apply pragmas and create the `kv` table if it does not already exist.
    fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA user_version = 1;
             PRAGMA foreign_keys = ON;
             PRAGMA secure_delete = ON;
             CREATE TABLE IF NOT EXISTS kv (
                 bucket     TEXT NOT NULL,
                 key        TEXT NOT NULL,
                 value      TEXT NOT NULL,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (bucket, key)
             ) WITHOUT ROWID, STRICT;
             CREATE INDEX IF NOT EXISTS idx_kv_bucket_key ON kv (bucket, key);
             CREATE INDEX IF NOT EXISTS idx_kv_key ON kv (key);",
        )?;
        Ok(())
    }

    /// Get a handle scoped to the namespace `name`.
    ///
    /// Buckets are created lazily — a bucket "exists" precisely as long as it holds
    /// at least one key. Two buckets with different names never share keys.
    pub fn bucket(&self, name: &str) -> Bucket<'_> {
        Bucket {
            store: self,
            name: name.to_owned(),
        }
    }
}

/// A namespaced handle into a [`Store`]. Cheap to create; borrows the store.
#[derive(Debug)]
pub struct Bucket<'a> {
    store: &'a Store,
    name: String,
}

impl<'a> Bucket<'a> {
    /// Fetch and deserialise the value stored at `key`, or `None` if absent.
    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let json: Option<String> = self
            .store
            .conn
            .query_row(
                "SELECT value FROM kv WHERE bucket = ?1 AND key = ?2",
                (&self.name, key),
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(text) => Ok(Some(serde_json::from_str(&text)?)),
            None => Ok(None),
        }
    }

    /// Serialise `value` to JSON and store it at `key`, overwriting any prior value.
    ///
    /// The `updated_at` bookkeeping column is set to the current unix time in seconds.
    pub fn set<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.store.conn.execute(
            "INSERT INTO kv (bucket, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(bucket, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            (&self.name, key, &json, now_unix_secs()),
        )?;
        Ok(())
    }

    /// Remove `key` from this bucket. Removing a missing key is a no-op (still `Ok`).
    pub fn delete(&self, key: &str) -> Result<()> {
        self.store.conn.execute(
            "DELETE FROM kv WHERE bucket = ?1 AND key = ?2",
            (&self.name, key),
        )?;
        Ok(())
    }

    /// Keys (not prefix-stripped) whose key starts with `prefix`, sorted ascending.
    ///
    /// An empty `prefix` returns every key in the bucket.
    pub fn keys(&self, prefix: &str) -> Result<Vec<String>> {
        // Range scan on the (bucket, key) primary key: every key that starts with
        // `prefix` is >= `prefix` and forms a contiguous, ascending block, so we can
        // stop as soon as a key no longer matches.
        let mut stmt = self
            .store
            .conn
            .prepare("SELECT key FROM kv WHERE bucket = ?1 AND key >= ?2 ORDER BY key ASC")?;
        let rows = stmt.query_map((&self.name, prefix), |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for key in rows {
            let key = key?;
            if !key.starts_with(prefix) {
                break;
            }
            out.push(key);
        }
        Ok(out)
    }

    /// `(key, deserialised value)` for every key starting with `prefix`, sorted by key ascending.
    pub fn iter<T: serde::de::DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let mut stmt = self.store.conn.prepare(
            "SELECT key, value FROM kv WHERE bucket = ?1 AND key >= ?2 ORDER BY key ASC",
        )?;
        let rows = stmt.query_map((&self.name, prefix), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, json) = row?;
            if !key.starts_with(prefix) {
                break;
            }
            out.push((key, serde_json::from_str(&json)?));
        }
        Ok(out)
    }

    /// The name of this bucket.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Current unix time in seconds, saturating to `0` before the epoch.
///
/// Used only for the `updated_at` bookkeeping column — never read back for logic.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Widget {
        id: u32,
        name: String,
        tags: Vec<String>,
    }

    fn widget(id: u32, name: &str) -> Widget {
        Widget {
            id,
            name: name.to_owned(),
            tags: vec!["a".into(), "b".into()],
        }
    }

    #[test]
    fn round_trip_struct() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        let w = widget(1, "hello");
        b.set("k1", &w).unwrap();
        let got: Option<Widget> = b.get("k1").unwrap();
        assert_eq!(got, Some(w));
    }

    #[test]
    fn round_trip_primitives_and_maps() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("prim");
        b.set("count", &42u64).unwrap();
        b.set("label", &"hi there".to_string()).unwrap();
        assert_eq!(b.get::<u64>("count").unwrap(), Some(42));
        assert_eq!(b.get::<String>("label").unwrap(), Some("hi there".into()));
    }

    #[test]
    fn missing_key_is_none() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        let got: Option<Widget> = b.get("nope").unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn delete_removes_value() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        b.set("k1", &widget(1, "x")).unwrap();
        assert!(b.get::<Widget>("k1").unwrap().is_some());
        b.delete("k1").unwrap();
        assert_eq!(b.get::<Widget>("k1").unwrap(), None);
    }

    #[test]
    fn delete_missing_is_ok() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        // Deleting a key that was never set must not error.
        b.delete("ghost").unwrap();
    }

    #[test]
    fn overwrite_updates_value() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        b.set("k1", &widget(1, "first")).unwrap();
        b.set("k1", &widget(2, "second")).unwrap();
        let got: Option<Widget> = b.get("k1").unwrap();
        assert_eq!(got, Some(widget(2, "second")));
        // Overwriting must not create a duplicate row: exactly one key present.
        assert_eq!(b.keys("").unwrap(), vec!["k1".to_string()]);
    }

    #[test]
    fn bucket_isolation() {
        let store = Store::open_in_memory().unwrap();
        let a = store.bucket("alpha");
        let z = store.bucket("zeta");
        a.set("shared", &widget(1, "in-alpha")).unwrap();
        z.set("shared", &widget(2, "in-zeta")).unwrap();

        assert_eq!(
            a.get::<Widget>("shared").unwrap(),
            Some(widget(1, "in-alpha"))
        );
        assert_eq!(
            z.get::<Widget>("shared").unwrap(),
            Some(widget(2, "in-zeta"))
        );

        // Deleting from one bucket leaves the other untouched.
        a.delete("shared").unwrap();
        assert_eq!(a.get::<Widget>("shared").unwrap(), None);
        assert_eq!(
            z.get::<Widget>("shared").unwrap(),
            Some(widget(2, "in-zeta"))
        );

        // keys() is scoped to the bucket.
        assert!(a.keys("").unwrap().is_empty());
        assert_eq!(z.keys("").unwrap(), vec!["shared".to_string()]);
    }

    #[test]
    fn keys_prefix_filtering_and_ordering() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("k");
        // Insert out of order to prove ordering is by query, not insertion.
        for key in ["image:c", "image:a", "container:z", "image:b", "other"] {
            b.set(key, &1u8).unwrap();
        }

        // Prefix filter + ascending sort.
        assert_eq!(
            b.keys("image:").unwrap(),
            vec![
                "image:a".to_string(),
                "image:b".to_string(),
                "image:c".to_string()
            ]
        );
        assert_eq!(
            b.keys("container:").unwrap(),
            vec!["container:z".to_string()]
        );
        // Empty prefix returns everything, sorted.
        assert_eq!(
            b.keys("").unwrap(),
            vec![
                "container:z".to_string(),
                "image:a".to_string(),
                "image:b".to_string(),
                "image:c".to_string(),
                "other".to_string()
            ]
        );
        // Non-matching prefix returns nothing.
        assert!(b.keys("zzz").unwrap().is_empty());
    }

    #[test]
    fn keys_prefix_does_not_leak_adjacent() {
        // A key that is >= the prefix but does not start with it must be excluded
        // (this is the correctness edge the take-while range scan guards).
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("k");
        for key in ["ab", "ac", "abc", "b"] {
            b.set(key, &0u8).unwrap();
        }
        assert_eq!(
            b.keys("ab").unwrap(),
            vec!["ab".to_string(), "abc".to_string()]
        );
    }

    #[test]
    fn iter_prefix_filtering_ordering_and_values() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        b.set("image:b", &widget(2, "beta")).unwrap();
        b.set("image:a", &widget(1, "alpha")).unwrap();
        b.set("container:x", &widget(9, "ex")).unwrap();

        let got: Vec<(String, Widget)> = b.iter("image:").unwrap();
        assert_eq!(
            got,
            vec![
                ("image:a".to_string(), widget(1, "alpha")),
                ("image:b".to_string(), widget(2, "beta")),
            ]
        );

        // Empty prefix iterates the whole bucket in key order.
        let all: Vec<(String, Widget)> = b.iter("").unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "container:x");
        assert_eq!(all[1].0, "image:a");
        assert_eq!(all[2].0, "image:b");
    }

    #[test]
    fn name_returns_bucket_name() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.bucket("docker").name(), "docker");
        assert_eq!(store.bucket("_engine").name(), "_engine");
    }

    #[test]
    fn empty_string_key_is_usable() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        b.set("", &widget(7, "empty-key")).unwrap();
        assert_eq!(b.get::<Widget>("").unwrap(), Some(widget(7, "empty-key")));
        assert_eq!(b.keys("").unwrap(), vec!["".to_string()]);
    }

    #[test]
    fn get_type_mismatch_is_json_error() {
        let store = Store::open_in_memory().unwrap();
        let b = store.bucket("things");
        b.set("k", &"a string".to_string()).unwrap();
        // Stored a JSON string; deserialising as a struct must fail as a Json error.
        let err = b.get::<Widget>("k").unwrap_err();
        assert!(matches!(err, KvError::Json(_)));
    }
}
