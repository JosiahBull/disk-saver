//! Core value types shared by the engine and every adapter: impact [`Class`],
//! deletion [`Candidate`]s, per-candidate [`Outcome`]s, and the disk-[`Pressure`]
//! model.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Impact class of deleting an item, **lowest impact first**.
///
/// The [`Ord`] derive follows declaration order, so
/// `Rebuildable < Cache < UserData`. The engine relies on this ordering to
/// scavenge in impact waves (§8): regenerable build artifacts are burned before
/// caches, and caches before anything a human put somewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Class {
    /// Regenerated automatically by a tool (`target/`, `__pycache__`, build cache).
    Rebuildable,
    /// Re-fetchable but costs time/bandwidth (`node_modules`, docker images).
    Cache,
    /// Gone is gone (trash contents, downloads).
    UserData,
}

/// A single item an adapter proposes deleting.
///
/// `bytes` is an *estimate* (shared docker layers, hardlinked stores etc. make
/// exactness impossible); the engine re-measures real free space rather than
/// trusting it. `last_used` drives oldest-first ordering within a scavenge wave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Adapter-scoped stable id (path, docker image id, …). Round-trips to
    /// [`Adapter::execute`](crate::Adapter::execute) and identifies the item in
    /// the approvals queue.
    pub id: String,
    /// Human-readable one-liner for `--dry-run` output, `review`, and logs.
    pub label: String,
    /// Estimated reclaimable bytes.
    pub bytes: u64,
    /// When this item was last known to be used.
    pub last_used: SystemTime,
    /// Impact of deleting this item.
    pub class: Class,
    /// If `true`, the engine never auto-deletes this item — it is routed to the
    /// approvals queue and waits for `disk-saver review`.
    pub requires_confirmation: bool,
}

impl Candidate {
    /// Build a candidate. `requires_confirmation` defaults to `false`; use
    /// [`Candidate::confirm`] to flip it.
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        bytes: u64,
        last_used: SystemTime,
        class: Class,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bytes,
            last_used,
            class,
            requires_confirmation: false,
        }
    }

    /// Set [`requires_confirmation`](Candidate::requires_confirmation),
    /// consuming and returning `self`.
    #[must_use]
    pub fn confirm(mut self, yes: bool) -> Self {
        self.requires_confirmation = yes;
        self
    }

    /// Age of the item at `now` (`now - last_used`), saturating to zero if
    /// `last_used` is in the future.
    pub fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.last_used).unwrap_or(Duration::ZERO)
    }
}

/// The result of attempting to delete one candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    /// The item was removed, reclaiming (approximately) `bytes`.
    Removed {
        /// The candidate's id.
        id: String,
        /// Bytes reclaimed (the candidate's estimate).
        bytes: u64,
    },
    /// The item was intentionally not removed (e.g. it became in-use again).
    Skipped {
        /// The candidate's id.
        id: String,
        /// Why it was skipped.
        reason: String,
    },
    /// Removal was attempted and failed.
    Failed {
        /// The candidate's id.
        id: String,
        /// The error message.
        error: String,
    },
}

impl Outcome {
    /// The candidate id this outcome refers to.
    pub fn id(&self) -> &str {
        match self {
            Outcome::Removed { id, .. }
            | Outcome::Skipped { id, .. }
            | Outcome::Failed { id, .. } => id,
        }
    }
}

/// Disk-pressure tier, derived from free space at the start of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// Free space is plentiful: observe only, delete nothing.
    Comfortable,
    /// Between the thresholds: delete items older than each adapter's `max_age`.
    Normal,
    /// Free space is low: delete items older than `min_age`, lowest-impact and
    /// oldest first, until free space reaches `scavenge_target`.
    Scavenge {
        /// Bytes still needed to reach `scavenge_target`.
        need: u64,
    },
}

impl Pressure {
    /// Whether this tier deletes anything: `false` only for
    /// [`Pressure::Comfortable`].
    pub fn deletes(&self) -> bool {
        !matches!(self, Pressure::Comfortable)
    }

    /// Short label for logs and records: `"comfortable"`, `"normal"`, or
    /// `"scavenge"`.
    pub fn label(&self) -> &'static str {
        match self {
            Pressure::Comfortable => "comfortable",
            Pressure::Normal => "normal",
            Pressure::Scavenge { .. } => "scavenge",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn class_ordering_is_declaration_order() {
        assert!(Class::Rebuildable < Class::Cache);
        assert!(Class::Cache < Class::UserData);
        let mut v = vec![Class::UserData, Class::Rebuildable, Class::Cache];
        v.sort();
        assert_eq!(v, vec![Class::Rebuildable, Class::Cache, Class::UserData]);
    }

    #[test]
    fn candidate_new_defaults_to_no_confirmation() {
        let c = Candidate::new("id", "label", 10, UNIX_EPOCH, Class::Cache);
        assert!(!c.requires_confirmation);
        assert_eq!(c.id, "id");
        assert_eq!(c.label, "label");
        assert_eq!(c.bytes, 10);
    }

    #[test]
    fn candidate_confirm_sets_flag() {
        let c = Candidate::new("id", "l", 1, UNIX_EPOCH, Class::UserData).confirm(true);
        assert!(c.requires_confirmation);
        assert!(!c.confirm(false).requires_confirmation);
    }

    #[test]
    fn candidate_age_saturates() {
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let past = Candidate::new(
            "i",
            "l",
            0,
            UNIX_EPOCH + Duration::from_secs(400),
            Class::Cache,
        );
        assert_eq!(past.age(now), Duration::from_secs(600));
        // last_used in the future → zero, not a panic.
        let future = Candidate::new(
            "i",
            "l",
            0,
            UNIX_EPOCH + Duration::from_secs(5000),
            Class::Cache,
        );
        assert_eq!(future.age(now), Duration::ZERO);
    }

    #[test]
    fn outcome_id_accessor() {
        assert_eq!(
            Outcome::Removed {
                id: "a".into(),
                bytes: 1
            }
            .id(),
            "a"
        );
        assert_eq!(
            Outcome::Skipped {
                id: "b".into(),
                reason: "x".into()
            }
            .id(),
            "b"
        );
        assert_eq!(
            Outcome::Failed {
                id: "c".into(),
                error: "e".into()
            }
            .id(),
            "c"
        );
    }

    #[test]
    fn pressure_deletes_and_label() {
        assert!(!Pressure::Comfortable.deletes());
        assert!(Pressure::Normal.deletes());
        assert!(Pressure::Scavenge { need: 5 }.deletes());
        assert_eq!(Pressure::Comfortable.label(), "comfortable");
        assert_eq!(Pressure::Normal.label(), "normal");
        assert_eq!(Pressure::Scavenge { need: 0 }.label(), "scavenge");
    }

    #[test]
    fn candidate_round_trips_json() {
        let c = Candidate::new("id", "label", 42, UNIX_EPOCH, Class::Rebuildable).confirm(true);
        let json = serde_json::to_string(&c).unwrap();
        let back: Candidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, c.id);
        assert_eq!(back.bytes, c.bytes);
        assert_eq!(back.class, c.class);
        assert!(back.requires_confirmation);
    }
}
