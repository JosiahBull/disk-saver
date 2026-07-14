//! `why <query>` — replay every recorded decision about an item (§15.2).
//!
//! Only compiled with the `decision-log` feature. Reads
//! `<state_dir>/decisions.jsonl`, filters to records whose id or label contains
//! the query (case-insensitive), and renders them as a chronological timeline.

use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use disk_saver_core::read_decisions;
use serde::Serialize;

use crate::app::App;
use crate::output;

/// A machine-readable timeline entry for `why --json`.
#[derive(Serialize)]
struct WhyEntry {
    /// Unix seconds when the decision was made.
    at_unix: u64,
    /// Pressure label at decision time.
    pressure: String,
    /// The kind of decision (snake_case).
    kind: String,
    /// The adapter involved.
    adapter: String,
    /// The candidate id.
    id: String,
    /// Human-readable label.
    label: String,
    /// Size in bytes.
    bytes: u64,
    /// Age of the item at decision time, in seconds.
    age_secs: u64,
    /// Optional free-text reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Run the `why` command for `query`.
pub fn run(app: &App, query: &str) -> Result<u8> {
    let path = app.state_dir.join("decisions.jsonl");
    let records = match read_decisions(&path) {
        Ok(records) => records,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading decision log {}", path.display()));
        }
    };

    let needle = query.to_lowercase();
    let mut matches: Vec<_> = records
        .into_iter()
        .filter(|r| {
            r.id.to_lowercase().contains(&needle) || r.label.to_lowercase().contains(&needle)
        })
        .collect();
    // Chronological: the log is append-order, but sort defensively.
    matches.sort_by_key(|r| r.at_unix);

    if app.json {
        let entries: Vec<WhyEntry> = matches
            .iter()
            .map(|r| WhyEntry {
                at_unix: r.at_unix,
                pressure: r.pressure.clone(),
                kind: kind_str(r.kind),
                adapter: r.adapter.clone(),
                id: r.id.clone(),
                label: r.label.clone(),
                bytes: r.bytes,
                age_secs: r.age_secs,
                reason: r.reason.clone(),
            })
            .collect();
        output::print_json(&entries)?;
        return Ok(0);
    }

    if matches.is_empty() {
        println!("no recorded decisions match {query:?}");
        return Ok(0);
    }

    println!("timeline for {query:?} ({} decision(s)):", matches.len());
    for r in &matches {
        let stamp = humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_secs(r.at_unix));
        print!(
            "  {stamp}  {:<9}  {:<12}  {}  {}",
            kind_str(r.kind),
            r.adapter,
            output::bytes(r.bytes),
            r.id,
        );
        if let Some(reason) = &r.reason {
            print!("  — {reason}");
        }
        println!("  [{}]", r.pressure);
    }
    Ok(0)
}

/// The snake_case wire name of a decision kind.
fn kind_str(k: disk_saver_core::DecisionKind) -> String {
    use disk_saver_core::DecisionKind as K;
    match k {
        K::Observed => "observed",
        K::Kept => "kept",
        K::Planned => "planned",
        K::Queued => "queued",
        K::Approved => "approved",
        K::Denied => "denied",
        K::Snoozed => "snoozed",
        K::Deleted => "deleted",
        K::Skipped => "skipped",
        K::Failed => "failed",
    }
    .to_owned()
}
