//! The confirmation / approvals queue (§9).
//!
//! High-impact deletions (trash, downloads) are flagged `requires_confirmation`
//! by their adapter. Instead of deleting them, the engine routes them here and
//! notifies the user; nothing happens until `disk-saver review` approves.
//!
//! The queue is stored in the reserved `_approvals` KV bucket, one
//! [`QueuedItem`] per key (`"<adapter>/<candidate.id>"`). Each full run
//! [`rebuild`](Approvals::rebuild)s the queue from the current plan so it never
//! goes stale — while preserving `first_queued` and any `DeniedForever` /
//! active-`Snoozed` state.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::Candidate;
use disk_saver_kv::{Bucket, Store};

/// Unix seconds for `t`, saturating to zero before the epoch.
fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The state of a queued item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// Awaiting a decision.
    Pending,
    /// Deferred until [`QueuedItem::snooze_until_unix`]; not re-asked meanwhile.
    Snoozed,
    /// Permanently denied; never auto-deleted and never re-asked.
    DeniedForever,
}

/// One item awaiting confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedItem {
    /// The adapter that proposed the deletion.
    pub adapter: String,
    /// The candidate itself (label, bytes, class, …).
    pub candidate: Candidate,
    /// Unix seconds when the item was first queued (preserved across rebuilds).
    pub first_queued_unix: u64,
    /// The current state.
    pub state: ApprovalState,
    /// For [`Snoozed`](ApprovalState::Snoozed): unix seconds until re-asking.
    pub snooze_until_unix: Option<u64>,
}

impl QueuedItem {
    /// The queue key: `"<adapter>/<candidate.id>"`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.adapter, self.candidate.id)
    }

    /// Whether this item is actionable-pending at `now`: [`Pending`], or a
    /// [`Snoozed`] item whose snooze has expired. [`DeniedForever`] is never
    /// actionable.
    ///
    /// [`Pending`]: ApprovalState::Pending
    /// [`Snoozed`]: ApprovalState::Snoozed
    /// [`DeniedForever`]: ApprovalState::DeniedForever
    fn is_actionable(&self, now_unix: u64) -> bool {
        match self.state {
            ApprovalState::Pending => true,
            ApprovalState::DeniedForever => false,
            ApprovalState::Snoozed => match self.snooze_until_unix {
                Some(until) => now_unix >= until,
                None => true,
            },
        }
    }

    /// Whether this item is actively snoozed at `now` (not yet expired).
    fn is_active_snooze(&self, now_unix: u64) -> bool {
        matches!(self.state, ApprovalState::Snoozed)
            && matches!(self.snooze_until_unix, Some(until) if now_unix < until)
    }
}

/// A handle over the reserved `_approvals` bucket. `now` is passed in for
/// determinism.
pub struct Approvals<'a> {
    bucket: Bucket<'a>,
}

impl<'a> Approvals<'a> {
    /// Open the approvals queue backed by `store`.
    pub fn open(store: &'a Store) -> Self {
        Approvals {
            bucket: store.bucket("_approvals"),
        }
    }

    /// Load every queued item, sorted by adapter then by bytes descending.
    pub fn list(&self) -> Vec<QueuedItem> {
        let mut items: Vec<QueuedItem> = self
            .bucket
            .iter::<QueuedItem>("")
            .map(|pairs| pairs.into_iter().map(|(_, item)| item).collect())
            .unwrap_or_default();
        items.sort_by(|a, b| {
            a.adapter
                .cmp(&b.adapter)
                .then(b.candidate.bytes.cmp(&a.candidate.bytes))
        });
        items
    }

    /// Items that are actionable-pending at `now` (Pending or expired-Snooze,
    /// excluding DeniedForever). Preserves [`list`](Approvals::list) ordering.
    pub fn pending(&self, now: SystemTime) -> Vec<QueuedItem> {
        let now_unix = to_unix(now);
        self.list()
            .into_iter()
            .filter(|it| it.is_actionable(now_unix))
            .collect()
    }

    /// Rebuild the queue from this run's flagged candidates (§9).
    ///
    /// New keys are added as `Pending` with `first_queued = now`; existing keys
    /// keep their `first_queued` and state (so a `DeniedForever` item stays
    /// denied and a `Snoozed` item stays snoozed even after re-planning). Keys
    /// no longer present are dropped unless they are `DeniedForever` or actively
    /// `Snoozed`. Returns the count of currently actionable-pending items.
    pub fn rebuild(&self, now: SystemTime, flagged: &[(String, Candidate)]) -> usize {
        let now_unix = to_unix(now);
        let existing: BTreeMap<String, QueuedItem> =
            self.list().into_iter().map(|it| (it.key(), it)).collect();

        let mut next: BTreeMap<String, QueuedItem> = BTreeMap::new();

        // Add / refresh every flagged candidate, preserving prior bookkeeping.
        for (adapter, cand) in flagged {
            let key = format!("{}/{}", adapter, cand.id);
            let item = match existing.get(&key) {
                Some(prev) => QueuedItem {
                    adapter: adapter.clone(),
                    candidate: cand.clone(),
                    first_queued_unix: prev.first_queued_unix,
                    state: prev.state,
                    snooze_until_unix: prev.snooze_until_unix,
                },
                None => QueuedItem {
                    adapter: adapter.clone(),
                    candidate: cand.clone(),
                    first_queued_unix: now_unix,
                    state: ApprovalState::Pending,
                    snooze_until_unix: None,
                },
            };
            next.insert(key, item);
        }

        // Retain vanished keys only if DeniedForever or actively Snoozed.
        for (key, item) in &existing {
            if next.contains_key(key) {
                continue;
            }
            let keep = matches!(item.state, ApprovalState::DeniedForever)
                || item.is_active_snooze(now_unix);
            if keep {
                next.insert(key.clone(), item.clone());
            }
        }

        // Persist the difference.
        for key in existing.keys() {
            if !next.contains_key(key) {
                let _ = self.bucket.delete(key);
            }
        }
        for (key, item) in &next {
            let _ = self.bucket.set(key, item, now);
        }

        next.values()
            .filter(|it| it.is_actionable(now_unix))
            .count()
    }

    /// Remove and return the item at `key` (used when approving it).
    pub fn approve(&self, key: &str) -> Option<QueuedItem> {
        let item = self.bucket.get::<QueuedItem>(key).ok().flatten()?;
        let _ = self.bucket.delete(key);
        Some(item)
    }

    /// Mark the item at `key` permanently denied (a no-op if it is absent).
    ///
    /// This method takes no clock (its contract signature has none); the KV
    /// `updated_at` bookkeeping column is written as [`UNIX_EPOCH`] and, per the
    /// store's contract, is never read back for logic.
    pub fn deny_forever(&self, key: &str) {
        if let Ok(Some(mut item)) = self.bucket.get::<QueuedItem>(key) {
            item.state = ApprovalState::DeniedForever;
            item.snooze_until_unix = None;
            let _ = self.bucket.set(key, &item, UNIX_EPOCH);
        }
    }

    /// Snooze the item at `key` for `dur` from `now` (a no-op if it is absent).
    pub fn snooze(&self, key: &str, now: SystemTime, dur: Duration) {
        if let Ok(Some(mut item)) = self.bucket.get::<QueuedItem>(key) {
            item.state = ApprovalState::Snoozed;
            // TODO @JO: the timestamps here seem a bit buggy/duplicated work.
            item.snooze_until_unix = Some(to_unix(now).saturating_add(dur.as_secs()));
            let _ = self.bucket.set(key, &item, now);
        }
    }

    /// Total estimated bytes of all actionable-pending items at `now`.
    pub fn total_bytes_pending(&self, now: SystemTime) -> u64 {
        self.pending(now)
            .iter()
            .fold(0u64, |acc, it| acc.saturating_add(it.candidate.bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Class;

    fn cand(id: &str, bytes: u64) -> Candidate {
        Candidate::new(id, id, bytes, UNIX_EPOCH, Class::UserData).confirm(true)
    }

    fn flagged(pairs: &[(&str, &str, u64)]) -> Vec<(String, Candidate)> {
        pairs
            .iter()
            .map(|(a, id, b)| (a.to_string(), cand(id, *b)))
            .collect()
    }

    #[test]
    fn queued_item_key_format() {
        let it = QueuedItem {
            adapter: "trash".into(),
            candidate: cand("photo.png", 1),
            first_queued_unix: 0,
            state: ApprovalState::Pending,
            snooze_until_unix: None,
        };
        assert_eq!(it.key(), "trash/photo.png");
    }

    #[test]
    fn rebuild_adds_new_items_pending() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let n = a.rebuild(now, &flagged(&[("trash", "a", 10), ("trash", "b", 20)]));
        assert_eq!(n, 2);
        let list = a.list();
        assert_eq!(list.len(), 2);
        // Sorted by adapter then bytes desc → b (20) before a (10).
        assert_eq!(list[0].candidate.id, "b");
        assert_eq!(list[1].candidate.id, "a");
        assert!(list.iter().all(|it| it.state == ApprovalState::Pending));
        assert!(list.iter().all(|it| it.first_queued_unix == 1000));
    }

    #[test]
    fn rebuild_preserves_first_queued_and_drops_vanished_pending() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let t0 = UNIX_EPOCH + Duration::from_secs(100);
        a.rebuild(t0, &flagged(&[("trash", "a", 10), ("trash", "b", 20)]));
        // Later run: `a` still planned (bytes changed), `b` gone, `c` new.
        let t1 = UNIX_EPOCH + Duration::from_secs(500);
        let n = a.rebuild(t1, &flagged(&[("trash", "a", 15), ("trash", "c", 30)]));
        assert_eq!(n, 2);
        let list = a.list();
        let keys: Vec<String> = list.iter().map(|it| it.key()).collect();
        assert!(keys.contains(&"trash/a".to_string()));
        assert!(keys.contains(&"trash/c".to_string()));
        assert!(!keys.contains(&"trash/b".to_string())); // vanished + pending → dropped
        let a_item = list.iter().find(|it| it.candidate.id == "a").unwrap();
        assert_eq!(a_item.first_queued_unix, 100); // preserved
        assert_eq!(a_item.candidate.bytes, 15); // refreshed
    }

    #[test]
    fn deny_forever_survives_rebuild_even_when_vanished() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let now = UNIX_EPOCH + Duration::from_secs(10);
        a.rebuild(now, &flagged(&[("trash", "a", 10)]));
        a.deny_forever("trash/a");
        // `a` no longer planned, but a denial must persist.
        let n = a.rebuild(now, &[]);
        assert_eq!(n, 0); // denied is not actionable
        let list = a.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].state, ApprovalState::DeniedForever);
    }

    #[test]
    fn denied_item_stays_denied_when_replanned() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let now = UNIX_EPOCH + Duration::from_secs(10);
        a.rebuild(now, &flagged(&[("trash", "a", 10)]));
        a.deny_forever("trash/a");
        let n = a.rebuild(now, &flagged(&[("trash", "a", 10)]));
        assert_eq!(n, 0);
        assert_eq!(a.list()[0].state, ApprovalState::DeniedForever);
    }

    #[test]
    fn snooze_hides_until_expiry_then_reappears() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let t0 = UNIX_EPOCH + Duration::from_secs(100);
        a.rebuild(t0, &flagged(&[("trash", "a", 10)]));
        a.snooze("trash/a", t0, Duration::from_secs(50));
        // Still within the snooze window → not actionable, but retained on rebuild.
        let mid = UNIX_EPOCH + Duration::from_secs(120);
        assert_eq!(a.pending(mid).len(), 0);
        let n = a.rebuild(mid, &[]); // vanished, but actively snoozed → kept
        assert_eq!(n, 0);
        assert_eq!(a.list().len(), 1);
        // After expiry it becomes actionable again.
        let after = UNIX_EPOCH + Duration::from_secs(200);
        assert_eq!(a.pending(after).len(), 1);
    }

    #[test]
    fn expired_snooze_vanished_is_dropped_on_rebuild() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let t0 = UNIX_EPOCH + Duration::from_secs(100);
        a.rebuild(t0, &flagged(&[("trash", "a", 10)]));
        a.snooze("trash/a", t0, Duration::from_secs(50));
        // After expiry AND no longer planned → dropped.
        let after = UNIX_EPOCH + Duration::from_secs(300);
        let n = a.rebuild(after, &[]);
        assert_eq!(n, 0);
        assert!(a.list().is_empty());
    }

    #[test]
    fn approve_removes_and_returns() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let now = UNIX_EPOCH;
        a.rebuild(now, &flagged(&[("trash", "a", 10)]));
        let item = a.approve("trash/a").unwrap();
        assert_eq!(item.candidate.id, "a");
        assert!(a.list().is_empty());
        assert!(a.approve("trash/a").is_none()); // already gone
    }

    #[test]
    fn total_bytes_pending_sums_actionable_only() {
        let store = Store::open_in_memory().unwrap();
        let a = Approvals::open(&store);
        let now = UNIX_EPOCH + Duration::from_secs(10);
        a.rebuild(
            now,
            &flagged(&[("trash", "a", 10), ("trash", "b", 20), ("trash", "c", 30)]),
        );
        a.deny_forever("trash/c"); // excluded
        a.snooze("trash/b", now, Duration::from_secs(1000)); // excluded (active)
        assert_eq!(a.total_bytes_pending(now), 10);
    }
}
