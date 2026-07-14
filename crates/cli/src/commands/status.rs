//! `status` — disk usage vs thresholds, cadence state, pending approvals, and
//! recent run summaries (§15.4).
//!
//! Read-only: no single-instance lock is taken. It measures the disk, resolves
//! every threshold to bytes, reports the current tier and the next-full-run
//! cadence, counts the pending approvals queue, and renders the most recent run
//! reports from the `_engine` KV ring buffer.

use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use disk_saver_core::{
    AdapterStatus, Approvals, GlobalConfig, Platform, RunReport, classify_pressure, run_due,
};
use serde::Serialize;

use crate::app::App;
use crate::output;

/// How many recent run reports to show.
const RECENT: usize = 5;

/// The four thresholds resolved to absolute bytes against the measured disk.
#[derive(Serialize)]
struct Thresholds {
    /// Free above this → observe only.
    start_cleaning_below: u64,
    /// Free below this → warn + faster cadence.
    warn_below: u64,
    /// Free below this → scavenge mode.
    scavenge_below: u64,
    /// Scavenge until this much is free.
    scavenge_target: u64,
}

/// A compact summary of one persisted [`RunReport`].
#[derive(Serialize)]
struct RunSummary {
    /// Unix seconds when the run started.
    at_unix: u64,
    /// Pressure label at run time.
    pressure: String,
    /// Whether it was a dry run.
    dry_run: bool,
    /// Whether it exited at the throttle gate.
    throttled: bool,
    /// Total bytes freed across adapters.
    freed_bytes: u64,
    /// Adapters that ended in `Failed`.
    failed_adapters: Vec<String>,
    /// Adapters that were `Unavailable`.
    unavailable_adapters: Vec<String>,
}

/// The whole `status --json` payload.
#[derive(Serialize)]
struct StatusJson {
    /// The filesystem whose free space drives the pressure model.
    disk_path: String,
    /// Total disk capacity in bytes.
    total: u64,
    /// Free bytes.
    free: u64,
    /// Used bytes (`total - free`).
    used: u64,
    /// The current tier: `comfortable` / `normal` / `warn` / `scavenge`.
    state: &'static str,
    /// The engine pressure label (`comfortable` / `normal` / `scavenge`).
    pressure: String,
    /// Thresholds resolved to bytes.
    thresholds: Thresholds,
    /// Whether free space is below `warn_below` (selects the faster cadence).
    below_warn: bool,
    /// Whether a full run is due now.
    full_run_due: bool,
    /// Seconds until the next full run is due (absent when due now / never run).
    #[serde(skip_serializing_if = "Option::is_none")]
    next_run_in_secs: Option<u64>,
    /// Unix seconds of the last full run, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_full_run_unix: Option<u64>,
    /// Count of actionable-pending approvals.
    pending_approvals: usize,
    /// Total estimated bytes of pending approvals.
    pending_bytes: u64,
    /// The most recent run summaries, newest first.
    recent_runs: Vec<RunSummary>,
}

/// The 4-level display tier for the current free space.
fn tier_label(free: u64, total: u64, g: &GlobalConfig) -> &'static str {
    if free < g.scavenge_below.bytes(total) {
        "scavenge"
    } else if free < g.warn_below.bytes(total) {
        "warn"
    } else if free < g.start_cleaning_below.bytes(total) {
        "normal"
    } else {
        "comfortable"
    }
}

/// Summarise one persisted run report.
fn summarise(r: &RunReport) -> RunSummary {
    let mut failed = Vec::new();
    let mut unavailable = Vec::new();
    for a in &r.adapters {
        match &a.status {
            AdapterStatus::Failed(_) => failed.push(a.name.clone()),
            AdapterStatus::Unavailable(_) => unavailable.push(a.name.clone()),
            AdapterStatus::Ok => {}
        }
    }
    RunSummary {
        at_unix: r.at_unix,
        pressure: r.pressure.clone(),
        dry_run: r.dry_run,
        throttled: r.throttled,
        freed_bytes: r.bytes_removed(),
        failed_adapters: failed,
        unavailable_adapters: unavailable,
    }
}

/// Read the most recent (up to [`RECENT`]) run reports, newest first.
fn recent_reports(store: &disk_saver_core::Store) -> Vec<RunReport> {
    let eng = store.bucket("_engine");
    let mut reports: Vec<RunReport> = eng
        .iter::<RunReport>("report:")
        .unwrap_or_default()
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    reports.sort_by_key(|r| std::cmp::Reverse(r.at_unix));
    reports.truncate(RECENT);
    reports
}

/// Render a timestamp (unix seconds) as an RFC-3339 string.
fn stamp(at_unix: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_secs(at_unix)).to_string()
}

/// Run the `status` command.
pub fn run(app: &App) -> Result<u8> {
    let usage = app.disk_usage()?;
    let total = usage.total;
    let free = usage.available;
    let g = &app.config.global;

    let pressure = classify_pressure(free, total, g);
    let state = tier_label(free, total, g);
    let below_warn = free < g.warn_below.bytes(total);

    let store = app.open_store()?;
    let approvals = Approvals::open(&store);
    let now = app.platform.now();
    let pending_approvals = approvals.pending(now).len();
    let pending_bytes = approvals.total_bytes_pending(now);

    let last_full_run_unix: Option<u64> =
        store.bucket("_engine").get("last_full_run").ok().flatten();
    let last_st = last_full_run_unix.map(|s| UNIX_EPOCH + Duration::from_secs(s));
    let sched = &app.config.schedule;
    let interval = if below_warn {
        sched.pressure_run_every
    } else {
        sched.run_every
    };
    let full_run_due = run_due(
        now,
        last_st,
        below_warn,
        sched.run_every,
        sched.pressure_run_every,
    );
    let next_run_in_secs = if full_run_due {
        None
    } else {
        last_st.and_then(|last| {
            let elapsed = now.duration_since(last).unwrap_or_default();
            interval.checked_sub(elapsed).map(|d| d.as_secs())
        })
    };

    let recent: Vec<RunSummary> = recent_reports(&store).iter().map(summarise).collect();

    if app.json {
        let payload = StatusJson {
            disk_path: app.disk_path().display().to_string(),
            total,
            free,
            used: total.saturating_sub(free),
            state,
            pressure: pressure.label().to_owned(),
            thresholds: Thresholds {
                start_cleaning_below: g.start_cleaning_below.bytes(total),
                warn_below: g.warn_below.bytes(total),
                scavenge_below: g.scavenge_below.bytes(total),
                scavenge_target: g.scavenge_target.bytes(total),
            },
            below_warn,
            full_run_due,
            next_run_in_secs,
            last_full_run_unix,
            pending_approvals,
            pending_bytes,
            recent_runs: recent,
        };
        output::print_json(&payload)?;
        return Ok(0);
    }

    render_human(
        app,
        &RenderCtx {
            total,
            free,
            state,
            g,
            below_warn,
            interval,
            full_run_due,
            next_run_in_secs,
            pending_approvals,
            pending_bytes,
            recent: &recent,
        },
    );
    Ok(0)
}

/// Bundle of resolved values for the human renderer.
struct RenderCtx<'a> {
    total: u64,
    free: u64,
    state: &'static str,
    g: &'a GlobalConfig,
    below_warn: bool,
    interval: Duration,
    full_run_due: bool,
    next_run_in_secs: Option<u64>,
    pending_approvals: usize,
    pending_bytes: u64,
    recent: &'a [RunSummary],
}

/// Render the human-readable `status` summary.
fn render_human(app: &App, c: &RenderCtx<'_>) {
    println!("disk: {}", app.disk_path().display());
    println!(
        "  {} free of {} ({} used) — {}",
        output::bytes(c.free),
        output::bytes(c.total),
        output::bytes(c.total.saturating_sub(c.free)),
        c.state,
    );
    println!(
        "  thresholds: clean<{} warn<{} scavenge<{} target {}",
        output::bytes(c.g.start_cleaning_below.bytes(c.total)),
        output::bytes(c.g.warn_below.bytes(c.total)),
        output::bytes(c.g.scavenge_below.bytes(c.total)),
        output::bytes(c.g.scavenge_target.bytes(c.total)),
    );

    let cadence = if c.below_warn { "pressure" } else { "normal" };
    println!(
        "cadence: {cadence} (full run every {})",
        humantime::format_duration(c.interval),
    );
    if c.full_run_due {
        println!("  next full run: due now");
    } else if let Some(secs) = c.next_run_in_secs {
        println!(
            "  next full run: in {}",
            humantime::format_duration(Duration::from_secs(secs)),
        );
    } else {
        println!("  next full run: unknown");
    }

    if c.pending_approvals == 0 {
        println!("approvals: none pending");
    } else {
        println!(
            "approvals: {} pending ({}) — run `disk-saver review`",
            c.pending_approvals,
            output::bytes(c.pending_bytes),
        );
    }

    if c.recent.is_empty() {
        println!("recent runs: none recorded yet");
    } else {
        println!("recent runs:");
        for r in c.recent {
            let mode = if r.dry_run { " dry-run" } else { "" };
            let throttled = if r.throttled { " throttled" } else { "" };
            print!(
                "  {}  {:<11}{mode}{throttled}  freed {}",
                stamp(r.at_unix),
                r.pressure,
                output::bytes(r.freed_bytes),
            );
            if !r.failed_adapters.is_empty() {
                print!("  failed: {}", r.failed_adapters.join(", "));
            }
            if !r.unavailable_adapters.is_empty() {
                print!("  unavailable: {}", r.unavailable_adapters.join(", "));
            }
            println!();
        }
    }
}
