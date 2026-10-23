//! The words shown in `disk-saver review` and recorded for `disk-saver why`.
//!
//! Times are absolute dates, never "4 months ago": a label is re-planned every
//! run, and one that kept changing would read as a different item each time.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::toolchain::Toolchain;

/// How many stale references a label names before summarising the rest.
const SHOWN_REFS: usize = 3;

/// `path` with the home directory written as `~`.
pub(crate) fn display(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The UTC date of `t`, as `2026-04-12`. Day precision is all filesystem
/// evidence deserves. A timestamp before 1970 (some archives set one) is
/// shown as such rather than refused.
pub(crate) fn date(t: SystemTime) -> String {
    let Ok(since) = t.duration_since(UNIX_EPOCH) else {
        return "before 1970".into();
    };
    let (year, month, day) = civil_from_days(since.as_secs() / 86_400);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a Gregorian `(year, month, day)`: Howard
/// Hinnant's `civil_from_days`, restricted to dates on or after the epoch.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// The note for a run whose pin walk could not read some directories, or
/// `None`. `unreadable` is sorted, so the example is stable from run to run.
pub(crate) fn incomplete(home: &Path, unreadable: &[PathBuf]) -> Option<String> {
    match unreadable {
        [] => None,
        [one] => Some(format!(
            "pin scan incomplete: 1 folder unreadable ({})",
            display(home, one)
        )),
        [first, ..] => Some(format!(
            "pin scan incomplete: {} folders unreadable (e.g. {})",
            unreadable.len(),
            display(home, first)
        )),
    }
}

/// The approval line for a toolchain: what it is, whether this machine can
/// even run it, when it was last used, what still names it, and whether the
/// search for what names it was complete.
pub(crate) fn candidate(
    tc: &Toolchain,
    host: &str,
    last_used: SystemTime,
    stale: &[String],
    incomplete: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if tc.triple != host {
        parts.push(foreign(&tc.triple, host));
    }
    parts.push(format!("last used {}", date(last_used)));
    parts.push(match stale {
        [] => "no project pins it".into(),
        refs if refs.len() <= SHOWN_REFS => format!("pinned only by {}", refs.join(", ")),
        refs => format!(
            "pinned only by {}, and {} more",
            refs[..SHOWN_REFS].join(", "),
            refs.len() - SHOWN_REFS
        ),
    });
    parts.extend(incomplete.map(str::to_owned));
    format!("uninstall Rust {}: {}", tc.name, parts.join("; "))
}

/// A toolchain built for another target goes through the same age policy — a
/// container sharing `~/.rustup` through a mount may still use it — but the
/// reviewer should know this machine has no use for it. One for a different
/// operating system cannot run here at all; one for another architecture of the
/// same system might, under emulation.
fn foreign(triple: &str, host: &str) -> String {
    if triple.split('-').nth(2) != host.split('-').nth(2) {
        format!("built for {triple}; can't run on this machine")
    } else {
        format!("built for {triple} (this machine is {host})")
    }
}
