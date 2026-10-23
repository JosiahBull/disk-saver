//! The decision log (§15.2) — the "when/why" audit layer, compiled fully in or
//! out behind the `decision-log` cargo feature.
//!
//! * **Feature on:** [`DecisionLog::open`] opens a sqlite database and every
//!   [`DecisionLog::record`] call inserts one row. An item's adapter, id and
//!   label are stored once in an `items` table, so the kept/planned/queued rows
//!   every run re-records about the same item cost a handful of integers each.
//!   [`DecisionLog::prune`] bounds the history by age and row count, and
//!   [`read_decisions`] reads it back for `disk-saver why`.
//! * **Feature off:** [`DecisionLog`] is a zero-sized struct whose `record` is an
//!   inlined no-op; call sites are identical either way and the optimizer removes
//!   the strings and the I/O entirely.
//!
//! Guard expensive [`Decision`] construction with [`DecisionLog::compiled`] so
//! lean builds pay nothing.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::types::Pressure;

/// The kind of decision being recorded about a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// The item was seen and its usage recorded.
    Observed,
    /// The item was kept (not eligible yet).
    Kept,
    /// The item was selected as a deletion candidate.
    Planned,
    /// The item was routed to the approvals queue.
    Queued,
    /// A queued item was approved via `review`.
    Approved,
    /// A queued item was permanently denied.
    Denied,
    /// A queued item was snoozed.
    Snoozed,
    /// The item was deleted.
    Deleted,
    /// The item was intentionally skipped during execution.
    Skipped,
    /// Deletion of the item failed.
    Failed,
}

impl DecisionKind {
    /// The snake_case name, as stored in the decision log and shown by `why`.
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionKind::Observed => "observed",
            DecisionKind::Kept => "kept",
            DecisionKind::Planned => "planned",
            DecisionKind::Queued => "queued",
            DecisionKind::Approved => "approved",
            DecisionKind::Denied => "denied",
            DecisionKind::Snoozed => "snoozed",
            DecisionKind::Deleted => "deleted",
            DecisionKind::Skipped => "skipped",
            DecisionKind::Failed => "failed",
        }
    }

    /// The kind whose [`as_str`](DecisionKind::as_str) is `s`, if any.
    #[cfg(feature = "decision-log")]
    fn from_name(s: &str) -> Option<Self> {
        [
            DecisionKind::Observed,
            DecisionKind::Kept,
            DecisionKind::Planned,
            DecisionKind::Queued,
            DecisionKind::Approved,
            DecisionKind::Denied,
            DecisionKind::Snoozed,
            DecisionKind::Deleted,
            DecisionKind::Skipped,
            DecisionKind::Failed,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }
}

/// A single decision to record. Built fluently; only [`DecisionKind`],
/// `adapter`, and `id` are required.
#[derive(Debug, Clone)]
pub struct Decision {
    /// What happened.
    pub kind: DecisionKind,
    /// The adapter that made (or is subject to) the decision.
    pub adapter: String,
    /// The candidate id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Size in bytes.
    pub bytes: u64,
    /// Age of the item at decision time.
    pub age: Duration,
    /// Optional free-text reason (e.g. `"age 3d < min_age 7d"`).
    pub reason: Option<String>,
}

impl Decision {
    /// Start building a decision. `label`/`bytes`/`age`/`reason` default to
    /// empty/zero/`None`.
    pub fn new(kind: DecisionKind, adapter: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind,
            adapter: adapter.into(),
            id: id.into(),
            label: String::new(),
            bytes: 0,
            age: Duration::ZERO,
            reason: None,
        }
    }

    /// Set the human-readable label.
    #[must_use]
    pub fn label(mut self, s: impl Into<String>) -> Self {
        self.label = s.into();
        self
    }

    /// Set the size in bytes.
    #[must_use]
    pub fn bytes(mut self, b: u64) -> Self {
        self.bytes = b;
        self
    }

    /// Set the item's age at decision time.
    #[must_use]
    pub fn age(mut self, d: Duration) -> Self {
        self.age = d;
        self
    }

    /// Set the free-text reason.
    #[must_use]
    pub fn reason(mut self, s: impl Into<String>) -> Self {
        self.reason = Some(s.into());
        self
    }
}

/// How long a decision is kept before [`DecisionLog::prune`] drops it.
#[cfg(feature = "decision-log")]
const RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// The most decisions kept, whatever their age — the backstop for an adapter
/// that records far more than usual. At roughly 75 bytes a row this caps the
/// database at about 15 MB.
#[cfg(feature = "decision-log")]
const MAX_ROWS: u64 = 200_000;

/// The decision log's schema. `seq` is an explicit `INTEGER PRIMARY KEY` so the
/// row cap can trim by insertion order (a `VACUUM` may renumber implicit rowids).
#[cfg(feature = "decision-log")]
const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous  = NORMAL;
    PRAGMA busy_timeout = 5000;
    PRAGMA foreign_keys = ON;
    CREATE TABLE IF NOT EXISTS items (
        item    INTEGER PRIMARY KEY,
        adapter TEXT NOT NULL,
        id      TEXT NOT NULL,
        label   TEXT NOT NULL,
        UNIQUE (adapter, id, label)
    );
    CREATE TABLE IF NOT EXISTS decisions (
        seq      INTEGER PRIMARY KEY,
        at_unix  INTEGER NOT NULL,
        pressure TEXT    NOT NULL,
        kind     TEXT    NOT NULL,
        item     INTEGER NOT NULL REFERENCES items (item),
        bytes    INTEGER NOT NULL,
        age_secs INTEGER NOT NULL,
        reason   TEXT
    );
    CREATE INDEX IF NOT EXISTS decisions_item ON decisions (item);
";

/// A best-effort, bounded decision log.
///
/// Always constructible via [`DecisionLog::disabled`]. When the `decision-log`
/// feature is off this is a zero-sized type and [`record`](DecisionLog::record)
/// is an inlined no-op.
#[derive(Debug)]
pub struct DecisionLog {
    #[cfg(feature = "decision-log")]
    inner: Option<std::sync::Mutex<rusqlite::Connection>>,
    #[cfg(not(feature = "decision-log"))]
    _priv: (),
}

impl DecisionLog {
    /// A disabled log that records nothing. Always available.
    pub fn disabled() -> Self {
        DecisionLog {
            #[cfg(feature = "decision-log")]
            inner: None,
            #[cfg(not(feature = "decision-log"))]
            _priv: (),
        }
    }

    /// Open the decision database at `path`, creating it if needed.
    ///
    /// Only available with the `decision-log` feature.
    #[cfg(feature = "decision-log")]
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let conn = rusqlite::Connection::open(path).map_err(std::io::Error::other)?;
        conn.execute_batch(SCHEMA).map_err(std::io::Error::other)?;
        Ok(DecisionLog {
            inner: Some(std::sync::Mutex::new(conn)),
        })
    }

    /// Record a decision. A no-op when disabled or when the feature is off; else
    /// inserts one row, committed immediately for durability.
    pub fn record(&self, at: SystemTime, pressure: Pressure, d: &Decision) {
        #[cfg(feature = "decision-log")]
        {
            if let Some(conn) = &self.inner {
                let rec = RecordedDecision {
                    at_unix: unix_secs(at),
                    pressure: pressure.label().to_owned(),
                    kind: d.kind,
                    adapter: d.adapter.clone(),
                    id: d.id.clone(),
                    label: d.label.clone(),
                    bytes: d.bytes,
                    age_secs: d.age.as_secs(),
                    reason: d.reason.clone(),
                };
                let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                let _ = insert(&conn, &rec);
            }
        }
        #[cfg(not(feature = "decision-log"))]
        {
            let _ = (at, pressure, d);
        }
    }

    /// Drop decisions older than 90 days, then the oldest beyond the newest
    /// 200,000, then any item no remaining decision refers to. Freed pages are
    /// reused by later inserts, so the file stops growing once it reaches its
    /// working size. A no-op when disabled.
    ///
    /// Only available with the `decision-log` feature.
    #[cfg(feature = "decision-log")]
    pub fn prune(&self, now: SystemTime) -> std::io::Result<()> {
        self.prune_to(now, RETENTION, MAX_ROWS)
    }

    /// [`prune`](DecisionLog::prune) with explicit limits, so tests need not
    /// insert 200,000 rows to reach the cap.
    #[cfg(feature = "decision-log")]
    fn prune_to(&self, now: SystemTime, retention: Duration, max_rows: u64) -> std::io::Result<()> {
        let Some(conn) = &self.inner else {
            return Ok(());
        };
        let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = unix_secs(now.checked_sub(retention).unwrap_or(SystemTime::UNIX_EPOCH));
        let prune = || -> rusqlite::Result<()> {
            conn.execute(
                "DELETE FROM decisions WHERE at_unix < ?1",
                (to_sql_int(cutoff),),
            )?;
            conn.execute(
                "DELETE FROM decisions WHERE seq <= (SELECT max(seq) FROM decisions) - ?1",
                (to_sql_int(max_rows),),
            )?;
            conn.execute(
                "DELETE FROM items
                 WHERE NOT EXISTS (SELECT 1 FROM decisions WHERE decisions.item = items.item)",
                (),
            )?;
            Ok(())
        };
        prune().map_err(std::io::Error::other)
    }

    /// Import a `decisions.jsonl` written before the log moved to sqlite: one
    /// [`RecordedDecision`] JSON object per line. A line that does not parse —
    /// say, a last line torn by a crash — is skipped rather than blocking the
    /// import. Returns the number of decisions imported; the file itself is left
    /// for the caller to remove. A no-op when disabled.
    ///
    /// Only available with the `decision-log` feature.
    #[cfg(feature = "decision-log")]
    pub fn import_jsonl(&self, path: &std::path::Path) -> std::io::Result<usize> {
        let Some(conn) = &self.inner else {
            return Ok(0);
        };
        let text = std::fs::read_to_string(path)?;
        let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(std::io::Error::other)?;
        let mut imported = 0;
        for line in text.lines() {
            let Ok(rec) = serde_json::from_str::<RecordedDecision>(line) else {
                continue;
            };
            insert(&tx, &rec).map_err(std::io::Error::other)?;
            imported += 1;
        }
        tx.commit().map_err(std::io::Error::other)?;
        Ok(imported)
    }

    /// Compile-time constant: `true` iff built with the `decision-log` feature.
    /// Use it to guard expensive [`Decision`] construction.
    pub const fn compiled() -> bool {
        cfg!(feature = "decision-log")
    }
}

/// Seconds since the Unix epoch; a time before it reads as `0`.
#[cfg(feature = "decision-log")]
fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|x| x.as_secs())
        .unwrap_or(0)
}

/// A `u64` as sqlite's signed integer, saturating; no size, age or timestamp
/// this tool records comes near `i64::MAX`.
#[cfg(feature = "decision-log")]
fn to_sql_int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The inverse of [`to_sql_int`]; a negative value (never written) reads as `0`.
#[cfg(feature = "decision-log")]
fn from_sql_int(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Insert one decision, adding its item to `items` the first time it is seen.
#[cfg(feature = "decision-log")]
fn insert(conn: &rusqlite::Connection, r: &RecordedDecision) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;

    let key = (&r.adapter, &r.id, &r.label);
    let found: Option<i64> = conn
        .prepare_cached("SELECT item FROM items WHERE adapter = ?1 AND id = ?2 AND label = ?3")?
        .query_row(key, |row| row.get(0))
        .optional()?;
    let item = match found {
        Some(item) => item,
        None => {
            conn.prepare_cached("INSERT INTO items (adapter, id, label) VALUES (?1, ?2, ?3)")?
                .execute(key)?;
            conn.last_insert_rowid()
        }
    };
    conn.prepare_cached(
        "INSERT INTO decisions (at_unix, pressure, kind, item, bytes, age_secs, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute((
        to_sql_int(r.at_unix),
        &r.pressure,
        r.kind.as_str(),
        item,
        to_sql_int(r.bytes),
        to_sql_int(r.age_secs),
        &r.reason,
    ))?;
    Ok(())
}

/// One recorded decision, as read back by [`read_decisions`] (and as one line of
/// the `decisions.jsonl` that [`DecisionLog::import_jsonl`] migrates).
///
/// Only available with the `decision-log` feature.
#[cfg(feature = "decision-log")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedDecision {
    /// Unix seconds when the decision was made.
    pub at_unix: u64,
    /// Pressure label at decision time.
    pub pressure: String,
    /// What happened.
    pub kind: DecisionKind,
    /// The adapter involved.
    pub adapter: String,
    /// The candidate id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Size in bytes.
    pub bytes: u64,
    /// Age of the item at decision time, in seconds.
    pub age_secs: u64,
    /// Optional free-text reason.
    pub reason: Option<String>,
}

/// Read every decision in the database at `path`, oldest first.
///
/// A missing database is a [`NotFound`](std::io::ErrorKind::NotFound) error; an
/// unreadable one, or a row with an unknown kind, is any other error.
///
/// Only available with the `decision-log` feature.
#[cfg(feature = "decision-log")]
pub fn read_decisions(path: &std::path::Path) -> std::io::Result<Vec<RecordedDecision>> {
    // `Connection::open` would create an empty database rather than fail.
    if !path.try_exists()? {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    let read = || -> rusqlite::Result<Vec<RecordedDecision>> {
        let conn = rusqlite::Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mut stmt = conn.prepare(
            "SELECT d.at_unix, d.pressure, d.kind, i.adapter, i.id, i.label,
                    d.bytes, d.age_secs, d.reason
             FROM decisions d JOIN items i ON i.item = d.item
             ORDER BY d.at_unix, d.seq",
        )?;
        stmt.query_map((), |row| {
            let kind: String = row.get(2)?;
            let kind = DecisionKind::from_name(&kind).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    format!("unknown decision kind {kind:?}").into(),
                )
            })?;
            Ok(RecordedDecision {
                at_unix: from_sql_int(row.get(0)?),
                pressure: row.get(1)?,
                kind,
                adapter: row.get(3)?,
                id: row.get(4)?,
                label: row.get(5)?,
                bytes: from_sql_int(row.get(6)?),
                age_secs: from_sql_int(row.get(7)?),
                reason: row.get(8)?,
            })
        })?
        .collect()
    };
    read().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn compiled_matches_feature() {
        assert_eq!(DecisionLog::compiled(), cfg!(feature = "decision-log"));
    }

    #[test]
    fn disabled_record_is_noop() {
        // Must not panic, and (feature off) must do nothing observable.
        let log = DecisionLog::disabled();
        log.record(
            UNIX_EPOCH,
            Pressure::Normal,
            &Decision::new(DecisionKind::Deleted, "docker", "img:1").bytes(10),
        );
    }

    #[test]
    fn decision_builder_sets_fields() {
        let d = Decision::new(DecisionKind::Kept, "trash", "x")
            .label("Screenshot.png")
            .bytes(1234)
            .age(Duration::from_secs(99))
            .reason("too young");
        assert_eq!(d.kind, DecisionKind::Kept);
        assert_eq!(d.adapter, "trash");
        assert_eq!(d.id, "x");
        assert_eq!(d.label, "Screenshot.png");
        assert_eq!(d.bytes, 1234);
        assert_eq!(d.age, Duration::from_secs(99));
        assert_eq!(d.reason.as_deref(), Some("too young"));
    }

    #[test]
    fn as_str_matches_the_serde_name() {
        // The legacy JSON-lines log was written through serde; the database is
        // written through `as_str`. They must agree for the import to be lossless.
        for kind in [
            DecisionKind::Observed,
            DecisionKind::Kept,
            DecisionKind::Planned,
            DecisionKind::Queued,
            DecisionKind::Approved,
            DecisionKind::Denied,
            DecisionKind::Snoozed,
            DecisionKind::Deleted,
            DecisionKind::Skipped,
            DecisionKind::Failed,
        ] {
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
    }

    #[cfg(feature = "decision-log")]
    fn count(path: &std::path::Path, table: &str) -> i64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), (), |r| r.get(0))
            .unwrap()
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn records_round_trip_through_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.db");
        {
            let log = DecisionLog::open(&path).unwrap();
            log.record(
                UNIX_EPOCH + Duration::from_secs(100),
                Pressure::Scavenge { need: 5 },
                &Decision::new(DecisionKind::Deleted, "rust-target", "/a/target")
                    .label("target")
                    .bytes(4096)
                    .age(Duration::from_secs(864000)),
            );
            log.record(
                UNIX_EPOCH + Duration::from_secs(200),
                Pressure::Normal,
                &Decision::new(DecisionKind::Kept, "docker", "img:2").reason("in use"),
            );
        }
        let recs = read_decisions(&path).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].kind, DecisionKind::Deleted);
        assert_eq!(recs[0].adapter, "rust-target");
        assert_eq!(recs[0].id, "/a/target");
        assert_eq!(recs[0].label, "target");
        assert_eq!(recs[0].pressure, "scavenge");
        assert_eq!(recs[0].bytes, 4096);
        assert_eq!(recs[0].age_secs, 864000);
        assert_eq!(recs[0].at_unix, 100);
        assert_eq!(recs[0].reason, None);
        assert_eq!(recs[1].kind, DecisionKind::Kept);
        assert_eq!(recs[1].pressure, "normal");
        assert_eq!(recs[1].reason.as_deref(), Some("in use"));
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn repeated_decisions_about_one_item_share_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.db");
        let log = DecisionLog::open(&path).unwrap();
        for (secs, kind) in [
            (1, DecisionKind::Kept),
            (2, DecisionKind::Planned),
            (3, DecisionKind::Queued),
        ] {
            log.record(
                UNIX_EPOCH + Duration::from_secs(secs),
                Pressure::Normal,
                &Decision::new(kind, "node-modules", "/p/node_modules").label("node_modules"),
            );
        }
        drop(log);
        assert_eq!(count(&path, "items"), 1);
        assert_eq!(count(&path, "decisions"), 3);
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn prune_drops_old_and_excess_decisions_and_their_orphaned_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.db");
        let log = DecisionLog::open(&path).unwrap();
        let day = Duration::from_secs(24 * 60 * 60);
        let now = UNIX_EPOCH + 100 * day;
        // Day 1 is past a 30-day retention; days 80, 90 and 99 are inside it.
        for (at, id) in [(1, "old"), (80, "a"), (90, "b"), (99, "c")] {
            log.record(
                UNIX_EPOCH + at * day,
                Pressure::Normal,
                &Decision::new(DecisionKind::Kept, "trash", id),
            );
        }

        log.prune_to(now, 30 * day, 2).unwrap();
        drop(log);

        let ids: Vec<_> = read_decisions(&path)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            ids,
            ["b", "c"],
            "the expired row and the oldest over the cap go"
        );
        assert_eq!(count(&path, "items"), 2, "items nothing refers to go too");
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn prune_keeps_recent_decisions_under_the_shipped_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.db");
        let log = DecisionLog::open(&path).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        log.record(
            now - Duration::from_secs(89 * 24 * 60 * 60),
            Pressure::Normal,
            &Decision::new(DecisionKind::Deleted, "trash", "/t/x"),
        );
        log.prune(now).unwrap();
        drop(log);
        assert_eq!(read_decisions(&path).unwrap().len(), 1);
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn import_jsonl_keeps_parseable_lines_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("decisions.jsonl");
        std::fs::write(
            &legacy,
            "{\"at_unix\":1,\"pressure\":\"normal\",\"kind\":\"observed\",\"adapter\":\"a\",\"id\":\"i\",\"label\":\"l\",\"bytes\":7,\"age_secs\":3,\"reason\":null}\n\
             \n\
             this is not json\n\
             {\"at_unix\":2,\"pressure\":\"scavenge\",\"kind\":\"deleted\",\"adapter\":\"a\",\"id\":\"i\",\"label\":\"l\",\"bytes\":7,\"age_secs\":4,\"reason\":\"old\"}\n\
             {\"at_unix\":3,\"pressure\":\"nor",
        )
        .unwrap();
        let path = dir.path().join("decisions.db");
        let log = DecisionLog::open(&path).unwrap();

        assert_eq!(log.import_jsonl(&legacy).unwrap(), 2);
        drop(log);

        let recs = read_decisions(&path).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].kind, DecisionKind::Observed);
        assert_eq!(recs[1].kind, DecisionKind::Deleted);
        assert_eq!(recs[1].pressure, "scavenge");
        assert_eq!(recs[1].reason.as_deref(), Some("old"));
        assert_eq!(count(&path, "items"), 1);
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn read_decisions_reports_a_missing_database_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.db");
        let err = read_decisions(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!path.exists(), "reading must not create the database");
    }
}
