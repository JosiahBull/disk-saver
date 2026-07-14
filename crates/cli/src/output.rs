//! Human- and machine-readable rendering helpers.
//!
//! `--json` output is always the serialized domain type (e.g. [`RunReport`]) so
//! it is a stable scripting contract; the human renderers are free to change.

use anyhow::Result;
use disk_saver_core::{AdapterStatus, Class, RunReport};

/// Format a byte count for human display (e.g. `1.2 GB`).
pub fn bytes(n: u64) -> String {
    bytesize::ByteSize(n).to_string()
}

/// Print a run report as pretty JSON.
pub fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Render a [`RunReport`] as a human-readable summary. When `detailed`, also
/// print a per-candidate table for each adapter (the projects/paths being
/// reclaimed, biggest first).
pub fn print_run_report(report: &RunReport, detailed: bool) {
    let mode = if report.dry_run { " (dry-run)" } else { "" };
    println!("disk-saver: {} pressure{}", report.pressure, mode);

    let used = report.total.saturating_sub(report.free_before);
    println!(
        "disk: {} free of {} ({} used)",
        bytes(report.free_before),
        bytes(report.total),
        bytes(used),
    );

    if report.throttled {
        println!("throttled: full run not yet due — nothing observed or changed");
    }

    if report.adapters.is_empty() {
        if !report.throttled {
            println!("no adapters ran");
        }
    } else {
        println!();
        println!(
            "{:<16} {:<24} {:>5} {:>10} {:>5} {:>10} {:>4} {:>5} {:>4}",
            "adapter", "status", "cand", "size", "rm", "freed", "q", "skip", "fail",
        );
        for a in &report.adapters {
            println!(
                "{:<16} {:<24} {:>5} {:>10} {:>5} {:>10} {:>4} {:>5} {:>4}",
                truncate(&a.name, 16),
                truncate(&status_label(&a.status), 24),
                a.candidates,
                bytes(a.candidate_bytes),
                a.removed,
                bytes(a.bytes_removed),
                a.queued,
                a.skipped,
                a.failed,
            );
        }
    }

    println!();
    let total_candidates: usize = report.adapters.iter().map(|a| a.candidates).sum();
    if total_candidates > 0 {
        println!(
            "{} candidate(s), ~{} reclaimable (estimate)",
            total_candidates,
            bytes(report.candidate_bytes()),
        );
    }
    if !report.dry_run {
        println!(
            "freed {} total; {} now free",
            bytes(report.free_after.saturating_sub(report.free_before)),
            bytes(report.free_after),
        );
    }
    if report.queued_items > 0 {
        println!(
            "{} item(s) await approval — run `disk-saver review`",
            report.queued_items
        );
    }

    if detailed {
        print_candidate_detail(report);
    }
}

/// Print a per-adapter table of the individual candidates (project/path, size,
/// age, class, confirm), biggest first. Used by `plan --detailed`.
fn print_candidate_detail(report: &RunReport) {
    for a in &report.adapters {
        if a.candidates_detail.is_empty() {
            continue;
        }
        let mut items: Vec<_> = a.candidates_detail.iter().collect();
        items.sort_by_key(|c| std::cmp::Reverse(c.bytes));

        println!();
        println!(
            "── {} ({} candidate(s), ~{}) ──",
            a.name,
            a.candidates,
            bytes(a.candidate_bytes),
        );
        println!(
            "  {:>10} {:>6} {:<12} {:>7}  item",
            "size", "age", "class", "confirm",
        );
        for c in items {
            println!(
                "  {:>10} {:>6} {:<12} {:>7}  {}",
                bytes(c.bytes),
                human_age(c.age_secs),
                class_label(c.class),
                if c.requires_confirmation { "yes" } else { "no" },
                c.label,
            );
        }
    }
}

/// A short label for an impact [`Class`].
fn class_label(class: Class) -> &'static str {
    match class {
        Class::Rebuildable => "rebuildable",
        Class::Cache => "cache",
        Class::UserData => "userdata",
    }
}

/// A compact human age like `45d`, `3h`, or `12m` from a second count.
fn human_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs >= DAY {
        format!("{}d", secs / DAY)
    } else if secs >= HOUR {
        format!("{}h", secs / HOUR)
    } else if secs >= MIN {
        format!("{}m", secs / MIN)
    } else {
        format!("{secs}s")
    }
}

/// A short display label for an adapter status.
fn status_label(status: &AdapterStatus) -> String {
    match status {
        AdapterStatus::Ok => "ok".to_string(),
        AdapterStatus::Unavailable(reason) => format!("unavailable: {reason}"),
        AdapterStatus::Failed(reason) => format!("FAILED: {reason}"),
    }
}

/// Truncate `s` to `max` display columns, adding an ellipsis if cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let kept: String = s.chars().take(max - 1).collect();
        format!("{kept}…")
    }
}
