//! The retention-policy convention (§4.4).
//!
//! Age thresholds are *adapter* config (opaque to the engine), but every adapter
//! wants the same two knobs, so core ships [`RetentionPolicy`] as a shared
//! helper. Adapters build one from their own `max_age`/`min_age` config and call
//! [`RetentionPolicy::eligible`] inside `plan`, returning only already-eligible
//! candidates. The engine never sees these values.

use std::time::Duration;

use crate::types::Pressure;

/// Age thresholds an adapter applies to decide what is deletable.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Normal mode: delete items unused for at least this long.
    pub max_age: Duration,
    /// Scavenge floor: never delete items younger than this, whatever the
    /// pressure. Must be `<= max_age`.
    pub min_age: Duration,
}

impl RetentionPolicy {
    /// Build a policy from its two thresholds.
    pub fn new(max_age: Duration, min_age: Duration) -> Self {
        Self { max_age, min_age }
    }

    /// Whether an item of the given `age` is eligible for deletion at `pressure`:
    ///
    /// * [`Comfortable`](Pressure::Comfortable) → always `false`.
    /// * [`Normal`](Pressure::Normal) → `age >= max_age`.
    /// * [`Scavenge`](Pressure::Scavenge) → `age >= min_age`.
    pub fn eligible(&self, age: Duration, pressure: Pressure) -> bool {
        match pressure {
            Pressure::Comfortable => false,
            Pressure::Normal => age >= self.max_age,
            Pressure::Scavenge { .. } => age >= self.min_age,
        }
    }

    /// Validate the invariant `min_age <= max_age`, returning a human-readable
    /// message on violation.
    pub fn validate(&self) -> Result<(), String> {
        if self.min_age > self.max_age {
            Err(format!(
                "min_age ({:?}) must not exceed max_age ({:?})",
                self.min_age, self.max_age
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(n: u64) -> Duration {
        Duration::from_secs(n * 24 * 60 * 60)
    }

    fn policy() -> RetentionPolicy {
        RetentionPolicy::new(days(30), days(7))
    }

    #[test]
    fn comfortable_never_eligible() {
        assert!(!policy().eligible(days(365), Pressure::Comfortable));
    }

    #[test]
    fn normal_uses_max_age() {
        let p = policy();
        assert!(!p.eligible(days(29), Pressure::Normal));
        assert!(p.eligible(days(30), Pressure::Normal)); // boundary: >=
        assert!(p.eligible(days(31), Pressure::Normal));
    }

    #[test]
    fn scavenge_uses_min_age_floor() {
        let p = policy();
        assert!(!p.eligible(days(6), Pressure::Scavenge { need: 1 }));
        assert!(p.eligible(days(7), Pressure::Scavenge { need: 1 })); // boundary: >=
        // Items between min_age and max_age become eligible only under scavenge.
        assert!(p.eligible(days(10), Pressure::Scavenge { need: 1 }));
        assert!(!p.eligible(days(10), Pressure::Normal));
    }

    #[test]
    fn validate_rejects_inverted_thresholds() {
        assert!(RetentionPolicy::new(days(7), days(30)).validate().is_err());
        assert!(RetentionPolicy::new(days(30), days(7)).validate().is_ok());
        // Equal is fine.
        assert!(RetentionPolicy::new(days(7), days(7)).validate().is_ok());
    }
}
