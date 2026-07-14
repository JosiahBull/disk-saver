//! `review` — inspect and resolve the approvals queue (§9).
//!
//! High-impact deletions flagged `requires_confirmation` are parked in the
//! `_approvals` queue by a scheduled run and wait here. `review` takes the same
//! single-instance lock as `run`, loads the queue, and either prints it
//! (`--list`), approves specified items (`--approve` / `--approve-all`), or
//! prompts interactively. Approved items are executed *now* via the adapter's
//! `execute`, exactly as if the run had deleted them.

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use disk_saver_core::{
    AdapterError, ApprovalState, Approvals, Engine, Outcome, Platform, QueuedItem,
};
use serde::Serialize;

use crate::app::App;
use crate::cli::ReviewArgs;
use crate::output;

/// "Not now" snooze duration (§9 default `14d`).
const SNOOZE: Duration = Duration::from_secs(14 * 24 * 3600);

/// A machine-readable projection of a [`QueuedItem`] for `--list --json`.
#[derive(Serialize)]
struct QueuedView {
    /// The queue key (`"<adapter>/<id>"`).
    key: String,
    /// The adapter that proposed the deletion.
    adapter: String,
    /// The candidate's adapter-scoped id.
    id: String,
    /// Human-readable label.
    label: String,
    /// Estimated reclaimable bytes.
    bytes: u64,
    /// The item's queue state.
    state: ApprovalState,
    /// Unix seconds when the item was first queued.
    first_queued_unix: u64,
}

impl From<&QueuedItem> for QueuedView {
    fn from(it: &QueuedItem) -> Self {
        QueuedView {
            key: it.key(),
            adapter: it.adapter.clone(),
            id: it.candidate.id.clone(),
            label: it.candidate.label.clone(),
            bytes: it.candidate.bytes,
            state: it.state,
            first_queued_unix: it.first_queued_unix,
        }
    }
}

/// The outcome of approving one item, for `--approve*` (JSON and human output).
#[derive(Serialize)]
struct ApproveResult {
    /// The queue key acted on.
    key: String,
    /// The adapter that owns the item.
    adapter: String,
    /// The per-candidate execution outcomes (empty if the adapter errored).
    outcomes: Vec<Outcome>,
    /// An adapter-level error (unavailable / failed) or "not queued", if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `review`: take the single-instance lock, then dispatch to the chosen mode.
/// If the lock is held by another instance, exit `0` without acting.
pub fn execute(app: &App, args: ReviewArgs) -> Result<u8> {
    app.with_lock(0u8, || review_locked(app, &args))
}

/// The body that runs while the single-instance lock is held.
fn review_locked(app: &App, args: &ReviewArgs) -> Result<u8> {
    let store = app.open_store()?;
    let approvals = Approvals::open(&store);
    let now = app.platform.now();
    let pending = approvals.pending(now);

    if args.list {
        render_list(app, &pending);
        return Ok(0);
    }

    // Every remaining mode may execute an adapter, so build the engine exactly
    // like `run` does.
    let decisions = app.open_decisions();
    let adapters = app.build_adapters()?;
    let mut engine = Engine::new(&app.platform, &app.config, &store, &decisions, adapters);

    if args.approve_all || !args.approve.is_empty() {
        return batch_approve(app, &mut engine, &approvals, args, &pending);
    }

    // Default: interactive review, but only with a real terminal to prompt on.
    if pending.is_empty() {
        render_list(app, &pending);
        return Ok(0);
    }
    if app.json || !io::stdin().is_terminal() {
        render_list(app, &pending);
        if !app.json {
            println!();
            println!(
                "stdin is not a terminal — use `review --approve <key>`, \
                 `review --approve-all`, or `review --list --json`."
            );
        }
        return Ok(0);
    }
    interactive(app, &mut engine, &approvals, &pending, now)
}

/// Print the pending queue: JSON view when `--json`, otherwise grouped by
/// adapter with the biggest items first.
fn render_list(app: &App, pending: &[QueuedItem]) {
    if app.json {
        let views: Vec<QueuedView> = pending.iter().map(QueuedView::from).collect();
        if let Err(e) = output::print_json(&views) {
            eprintln!("error: {e:#}");
        }
        return;
    }
    if pending.is_empty() {
        println!("no items awaiting approval");
        return;
    }
    let mut current: Option<&str> = None;
    for it in pending {
        if current != Some(it.adapter.as_str()) {
            current = Some(it.adapter.as_str());
            println!("\n{}", it.adapter);
        }
        println!(
            "  {:<40} {:>10}  {}",
            it.key(),
            output::bytes(it.candidate.bytes),
            it.candidate.label,
        );
    }
    println!();
    println!(
        "{} item(s) pending — approve with `review --approve <key>` or `review --approve-all`.",
        pending.len(),
    );
}

/// Non-interactive `--approve <key>…` / `--approve-all [--adapter <n>]`.
fn batch_approve(
    app: &App,
    engine: &mut Engine<'_>,
    approvals: &Approvals<'_>,
    args: &ReviewArgs,
    pending: &[QueuedItem],
) -> Result<u8> {
    let mut targets: Vec<&QueuedItem> = Vec::new();
    let mut results: Vec<ApproveResult> = Vec::new();

    if args.approve_all {
        for it in pending {
            if args.adapter.as_deref().is_none_or(|a| a == it.adapter) {
                targets.push(it);
            }
        }
    } else {
        for key in &args.approve {
            match pending.iter().find(|it| &it.key() == key) {
                Some(it) => targets.push(it),
                None => results.push(ApproveResult {
                    key: key.clone(),
                    adapter: String::new(),
                    outcomes: Vec::new(),
                    error: Some("not in the pending queue".to_owned()),
                }),
            }
        }
    }

    let mut any_failed = false;
    for it in targets {
        let result = approve_one(engine, approvals, it);
        any_failed |= result_failed(&result);
        results.push(result);
    }

    if app.json {
        output::print_json(&results)?;
    } else {
        for r in &results {
            print_result(r);
        }
        if results.is_empty() {
            println!("nothing to approve");
        }
    }
    Ok(if any_failed { 2 } else { 0 })
}

/// Interactive review: prompt per pending item, grouped by adapter (biggest
/// first). Reads decisions from stdin one line at a time.
fn interactive(
    app: &App,
    engine: &mut Engine<'_>,
    approvals: &Approvals<'_>,
    pending: &[QueuedItem],
    now: std::time::SystemTime,
) -> Result<u8> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut auto_adapter: Option<String> = None;
    let mut any_failed = false;

    for it in pending {
        let key = it.key();

        // "all" for this adapter: approve remaining items without prompting.
        if auto_adapter.as_deref() == Some(it.adapter.as_str()) {
            let result = approve_one(engine, approvals, it);
            any_failed |= result_failed(&result);
            print_result(&result);
            continue;
        }

        print!(
            "\n{}\n  {}  ({})\n  [y]es / [n]ot now / [N]ever / [a]ll / [q]uit > ",
            key,
            it.candidate.label,
            output::bytes(it.candidate.bytes),
        );
        io::stdout().flush().ok();

        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF → treat as quit.
        }
        match parse_choice(&line) {
            Choice::Yes => {
                let result = approve_one(engine, approvals, it);
                any_failed |= result_failed(&result);
                print_result(&result);
            }
            Choice::NotNow => {
                approvals.snooze(&key, now, SNOOZE);
                println!("  snoozed for 14d");
            }
            Choice::Never => {
                approvals.deny_forever(&key);
                println!("  denied permanently.");
                println!(
                    "  tip: for a durable, visible rule add a `protect` glob to \
                     [adapters.{}] in {}.",
                    it.adapter,
                    app.config_path.display(),
                );
            }
            Choice::All => {
                auto_adapter = Some(it.adapter.clone());
                let result = approve_one(engine, approvals, it);
                any_failed |= result_failed(&result);
                print_result(&result);
            }
            Choice::Quit => break,
            Choice::Unknown => println!("  unrecognised input — left queued."),
        }
    }
    Ok(if any_failed { 2 } else { 0 })
}

/// Execute one approved item now and remove it from the queue on success.
fn approve_one(
    engine: &mut Engine<'_>,
    approvals: &Approvals<'_>,
    it: &QueuedItem,
) -> ApproveResult {
    match engine.execute_approved(&it.adapter, std::slice::from_ref(&it.candidate)) {
        Ok(outcomes) => {
            approvals.approve(&it.key());
            ApproveResult {
                key: it.key(),
                adapter: it.adapter.clone(),
                outcomes,
                error: None,
            }
        }
        Err(AdapterError::Unavailable(m)) => ApproveResult {
            key: it.key(),
            adapter: it.adapter.clone(),
            outcomes: Vec::new(),
            error: Some(format!("unavailable: {m}")),
        },
        Err(AdapterError::Failed(e)) => ApproveResult {
            key: it.key(),
            adapter: it.adapter.clone(),
            outcomes: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Whether an [`ApproveResult`] represents a genuine failure (exit code 2): an
/// [`AdapterError::Failed`] or any [`Outcome::Failed`]. Unavailable and
/// not-queued are expected conditions.
fn result_failed(r: &ApproveResult) -> bool {
    let adapter_failed = r
        .error
        .as_deref()
        .is_some_and(|e| !e.starts_with("unavailable:") && e != "not in the pending queue");
    let outcome_failed = r
        .outcomes
        .iter()
        .any(|o| matches!(o, Outcome::Failed { .. }));
    adapter_failed || outcome_failed
}

/// Render one approve result as human-readable lines.
fn print_result(r: &ApproveResult) {
    if let Some(err) = &r.error {
        println!("{}: {err}", r.key);
        return;
    }
    if r.outcomes.is_empty() {
        println!("{}: approved (no outcome reported)", r.key);
    }
    for o in &r.outcomes {
        match o {
            Outcome::Removed { id, bytes } => {
                println!("{}: removed {} ({id})", r.key, output::bytes(*bytes));
            }
            Outcome::Skipped { id, reason } => {
                println!("{}: skipped {id} ({reason})", r.key);
            }
            Outcome::Failed { id, error } => {
                println!("{}: FAILED {id} ({error})", r.key);
            }
        }
    }
}

/// A parsed interactive choice.
enum Choice {
    /// Approve and execute now.
    Yes,
    /// Snooze this item.
    NotNow,
    /// Deny permanently.
    Never,
    /// Approve the rest for this adapter.
    All,
    /// Stop the review.
    Quit,
    /// Anything unrecognised.
    Unknown,
}

/// Parse one line of interactive input into a [`Choice`]. `n` (not now) and `N`
/// (never) are case-sensitive single letters, per the prompt; word forms are
/// case-insensitive.
fn parse_choice(s: &str) -> Choice {
    match s.trim() {
        "y" | "Y" => Choice::Yes,
        "n" => Choice::NotNow,
        "N" => Choice::Never,
        "a" | "A" => Choice::All,
        "q" | "Q" => Choice::Quit,
        other => match other.to_ascii_lowercase().as_str() {
            "yes" => Choice::Yes,
            "not now" | "notnow" => Choice::NotNow,
            "never" => Choice::Never,
            "all" => Choice::All,
            "quit" => Choice::Quit,
            _ => Choice::Unknown,
        },
    }
}
