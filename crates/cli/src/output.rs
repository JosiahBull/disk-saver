//! Human- and machine-readable rendering helpers.
//!
//! `--json` output is always the serialized domain type (e.g. [`RunReport`]) so
//! it is a stable scripting contract; the human renderers are free to change.

use anyhow::Result;
use disk_saver_core::{AdapterStatus, RunReport};

/// Format a byte count for human display (e.g. `1.2 GB`).
pub fn bytes(n: u64) -> String {
    bytesize::ByteSize(n).to_string()
}

/// Print a run report as pretty JSON.
pub fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Render a [`RunReport`] as a human-readable summary.
pub fn print_run_report(report: &RunReport) {
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
