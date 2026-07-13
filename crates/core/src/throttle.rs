//! The throttle gate (§8 step 2 / §14): a pure, unit-testable decision about
//! whether a full run is due yet.
//!
//! The OS timer fires often (`check_every`); this gate decides cheaply whether
//! enough time has elapsed since the last full run. `below_warn` selects the
//! cadence — the faster `pressure_run_every` when disk space is low, otherwise
//! the normal `run_every`.

use std::time::{Duration, SystemTime};

/// Whether a full run is due.
///
/// * `last_full_run == None` → always due (never run before).
/// * otherwise → due once `now - last_full_run >= interval`, where `interval` is
///   `pressure_run_every` when `below_warn`, else `run_every`.
///
/// A clock that appears to run backwards (so `now < last_full_run`) is treated
/// as "not due" (zero elapsed), which is the safe direction.
pub fn run_due(
    now: SystemTime,
    last_full_run: Option<SystemTime>,
    below_warn: bool,
    run_every: Duration,
    pressure_run_every: Duration,
) -> bool {
    let interval = if below_warn {
        pressure_run_every
    } else {
        run_every
    };
    match last_full_run {
        None => true,
        Some(last) => now.duration_since(last).unwrap_or(Duration::ZERO) >= interval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn hours(n: u64) -> Duration {
        Duration::from_secs(n * 3600)
    }

    #[test]
    fn never_run_is_always_due() {
        assert!(run_due(UNIX_EPOCH, None, false, hours(12), hours(1)));
        assert!(run_due(UNIX_EPOCH, None, true, hours(12), hours(1)));
    }

    #[test]
    fn normal_cadence_gates_on_run_every() {
        let last = UNIX_EPOCH;
        // 11h elapsed, run_every 12h → not due.
        assert!(!run_due(
            last + hours(11),
            Some(last),
            false,
            hours(12),
            hours(1)
        ));
        // 12h elapsed → due (boundary is >=).
        assert!(run_due(
            last + hours(12),
            Some(last),
            false,
            hours(12),
            hours(1)
        ));
        assert!(run_due(
            last + hours(13),
            Some(last),
            false,
            hours(12),
            hours(1)
        ));
    }

    #[test]
    fn pressure_cadence_uses_pressure_run_every() {
        let last = UNIX_EPOCH;
        // Under pressure the faster 1h interval applies: 2h elapsed → due even
        // though the normal 12h interval would not be.
        assert!(run_due(
            last + hours(2),
            Some(last),
            true,
            hours(12),
            hours(1)
        ));
        // 30m elapsed under pressure → still not due.
        assert!(!run_due(
            last + Duration::from_secs(1800),
            Some(last),
            true,
            hours(12),
            hours(1)
        ));
    }

    #[test]
    fn backwards_clock_is_not_due() {
        let last = UNIX_EPOCH + hours(100);
        assert!(!run_due(
            UNIX_EPOCH + hours(50),
            Some(last),
            false,
            hours(12),
            hours(1)
        ));
    }
}
