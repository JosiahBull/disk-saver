//! The decision log (§15.2) — the "when/why" audit layer, compiled fully in or
//! out behind the `decision-log` cargo feature.
//!
//! * **Feature on:** [`DecisionLog::to_path`] opens an append writer and every
//!   [`DecisionLog::record`] call appends one JSON line ([`RecordedDecision`]).
//!   [`read_decisions`] reads them back for `disk-saver why`.
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

/// An append-only, best-effort decision log.
///
/// Always constructible via [`DecisionLog::disabled`]. When the `decision-log`
/// feature is off this is a zero-sized type and [`record`](DecisionLog::record)
/// is an inlined no-op.
#[derive(Debug)]
pub struct DecisionLog {
    #[cfg(feature = "decision-log")]
    inner: Option<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
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

    /// Open an append writer at `path`, creating the file if needed.
    ///
    /// Only available with the `decision-log` feature.
    #[cfg(feature = "decision-log")]
    pub fn to_path(path: std::path::PathBuf) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(DecisionLog {
            inner: Some(std::sync::Mutex::new(std::io::BufWriter::new(file))),
        })
    }

    /// Record a decision. A no-op when disabled or when the feature is off; else
    /// appends one JSON line (flushed immediately for durability).
    pub fn record(&self, at: SystemTime, pressure: Pressure, d: &Decision) {
        #[cfg(feature = "decision-log")]
        {
            use std::io::Write;
            if let Some(writer) = &self.inner {
                let rec = RecordedDecision {
                    at_unix: at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|x| x.as_secs())
                        .unwrap_or(0),
                    pressure: pressure.label().to_owned(),
                    kind: d.kind,
                    adapter: d.adapter.clone(),
                    id: d.id.clone(),
                    label: d.label.clone(),
                    bytes: d.bytes,
                    age_secs: d.age.as_secs(),
                    reason: d.reason.clone(),
                };
                if let Ok(line) = serde_json::to_string(&rec) {
                    let mut guard = writer.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = writeln!(guard, "{line}");
                    let _ = guard.flush();
                }
            }
        }
        #[cfg(not(feature = "decision-log"))]
        {
            let _ = (at, pressure, d);
        }
    }

    /// Compile-time constant: `true` iff built with the `decision-log` feature.
    /// Use it to guard expensive [`Decision`] construction.
    pub const fn compiled() -> bool {
        cfg!(feature = "decision-log")
    }
}

/// One line of the decision log, as persisted to and read from disk.
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

/// Read and parse a decision log file into [`RecordedDecision`]s (one per line).
///
/// Blank lines are skipped; a malformed line yields an
/// [`InvalidData`](std::io::ErrorKind::InvalidData) error.
///
/// Only available with the `decision-log` feature.
#[cfg(feature = "decision-log")]
pub fn read_decisions(path: &std::path::Path) -> std::io::Result<Vec<RecordedDecision>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: RecordedDecision = serde_json::from_str(line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        out.push(rec);
    }
    Ok(out)
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

    #[cfg(feature = "decision-log")]
    #[test]
    fn records_round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        {
            let log = DecisionLog::to_path(path.clone()).unwrap();
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
        assert_eq!(recs[0].pressure, "scavenge");
        assert_eq!(recs[0].bytes, 4096);
        assert_eq!(recs[0].age_secs, 864000);
        assert_eq!(recs[0].at_unix, 100);
        assert_eq!(recs[1].kind, DecisionKind::Kept);
        assert_eq!(recs[1].pressure, "normal");
        assert_eq!(recs[1].reason.as_deref(), Some("in use"));
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn read_decisions_skips_blank_lines() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{{\"at_unix\":1,\"pressure\":\"normal\",\"kind\":\"observed\",\"adapter\":\"a\",\"id\":\"i\",\"label\":\"\",\"bytes\":0,\"age_secs\":0,\"reason\":null}}"
        )
        .unwrap();
        writeln!(f).unwrap();
        drop(f);
        let recs = read_decisions(&path).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, DecisionKind::Observed);
    }

    #[cfg(feature = "decision-log")]
    #[test]
    fn read_decisions_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        std::fs::write(&path, "this is not json\n").unwrap();
        let err = read_decisions(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
