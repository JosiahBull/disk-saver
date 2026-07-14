//! The orchestration engine (§8) — the run lifecycle that ties adapters,
//! pressure, approvals, notifications, and the decision log together.
//!
//! [`Engine::run`] performs the full cycle *except* the single-instance lock
//! (the CLI owns that): throttle gate → measure → classify → observe (isolated)
//! → (if comfortable) GC approvals & return → plan → route confirmations →
//! execute (Normal: each adapter's unflagged candidates; Scavenge: impact waves
//! with real free-space re-measurement and early stop) → persist report &
//! `last_full_run` → notify.
//!
//! Adapter panics are caught (`catch_unwind`) and become
//! [`AdapterStatus::Failed`]; a broken adapter can never abort the janitor.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::adapter::{Adapter, Ctx};
use crate::approvals::Approvals;
use crate::config::{Config, GlobalConfig, NotificationEvent};
use crate::decision::{Decision, DecisionKind, DecisionLog};
use crate::error::AdapterError;
use crate::throttle::run_due;
use crate::types::{Candidate, Class, Outcome, Pressure};
use crate::{DiskUsage, Notification, Store, Urgency};

/// Resolve free/total space into a [`Pressure`] tier against `g`'s thresholds.
///
/// `warn_below` is deliberately *not* consulted here — it is an engine-level
/// concern (notifications and cadence), not a tier boundary.
///
/// Boundaries: `free >= start_cleaning_below` → Comfortable; `free <
/// scavenge_below` → Scavenge (with `need = scavenge_target - free`); otherwise
/// Normal.
pub fn classify_pressure(free: u64, total: u64, g: &GlobalConfig) -> Pressure {
    // A zero total means the disk could not be measured (statvfs failed, or the
    // configured path does not exist). Fail SAFE: never let a measurement
    // failure classify as pressure that would delete files. Without this guard,
    // absolute thresholds (which ignore `total`) would resolve against a `free`
    // of 0 and yield `Scavenge`, turning a measurement error into mass deletion.
    if total == 0 {
        return Pressure::Comfortable;
    }
    let start = g.start_cleaning_below.bytes(total);
    let scavenge = g.scavenge_below.bytes(total);
    if free >= start {
        Pressure::Comfortable
    } else if free < scavenge {
        let target = g.scavenge_target.bytes(total);
        Pressure::Scavenge {
            need: target.saturating_sub(free),
        }
    } else {
        Pressure::Normal
    }
}

/// Options controlling a single [`Engine::run`]. All fields default to
/// off/`None`.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Plan only: run observe/plan but never route, execute, or persist.
    pub dry_run: bool,
    /// Bypass the throttle gate (a full run *now*).
    pub force: bool,
    /// Restrict the run to these adapter names (`--adapter`).
    pub only: Option<Vec<String>>,
    /// Override measured pressure (`--pressure`).
    pub pressure_override: Option<Pressure>,
}

/// The outcome of driving one adapter through a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AdapterStatus {
    /// The adapter ran to completion.
    Ok,
    /// The adapter was skipped this run (expected, retry next).
    Unavailable(String),
    /// The adapter failed (contributes to exit code 2).
    Failed(String),
}

/// Per-adapter summary for a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterReport {
    /// The adapter's name.
    pub name: String,
    /// How the adapter fared.
    pub status: AdapterStatus,
    /// Candidates returned by `plan`.
    pub candidates: usize,
    /// Estimated reclaimable bytes across all planned candidates (an estimate —
    /// shared docker layers, hardlinks and pnpm stores make it approximate; the
    /// engine re-measures real free space rather than trusting it).
    pub candidate_bytes: u64,
    /// Items removed.
    pub removed: usize,
    /// Bytes removed (sum of `Removed` outcome estimates).
    pub bytes_removed: u64,
    /// Items skipped during execution.
    pub skipped: usize,
    /// Items routed to the approvals queue.
    pub queued: usize,
    /// Items whose removal failed.
    pub failed: usize,
}

/// The report of one full run (also persisted to a KV ring buffer for `status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// Unix seconds when the run started.
    pub at_unix: u64,
    /// Pressure label (`Pressure::label`).
    pub pressure: String,
    /// Whether the run exited at the throttle gate.
    pub throttled: bool,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Total disk capacity in bytes.
    pub total: u64,
    /// Free bytes measured at the start.
    pub free_before: u64,
    /// Free bytes after execution (equal to `free_before` if nothing ran).
    pub free_after: u64,
    /// Per-adapter reports.
    pub adapters: Vec<AdapterReport>,
    /// Actionable-pending items in the approvals queue after this run.
    pub queued_items: usize,
}

impl RunReport {
    /// Total bytes removed across all adapters.
    pub fn bytes_removed(&self) -> u64 {
        self.adapters
            .iter()
            .fold(0u64, |acc, a| acc.saturating_add(a.bytes_removed))
    }

    /// Total estimated reclaimable bytes across all planned candidates.
    pub fn candidate_bytes(&self) -> u64 {
        self.adapters
            .iter()
            .fold(0u64, |acc, a| acc.saturating_add(a.candidate_bytes))
    }

    /// Whether any adapter ended in [`AdapterStatus::Failed`] (exit code 2).
    pub fn any_failed(&self) -> bool {
        self.adapters
            .iter()
            .any(|a| matches!(a.status, AdapterStatus::Failed(_)))
    }
}

/// Reason an adapter phase did not complete normally.
enum Skip {
    Unavailable(String),
    Failed(String),
}

/// Mutable per-adapter bookkeeping while a run is in flight.
struct AdapterRun {
    /// Index into the engine's `adapters` vec.
    idx: usize,
    /// The adapter name (mirrors `report.name`).
    name: String,
    /// Candidates returned by this adapter's `plan`.
    planned: Vec<Candidate>,
    /// The report accumulated for this adapter.
    report: AdapterReport,
}

impl AdapterRun {
    fn new(idx: usize, name: &str) -> Self {
        AdapterRun {
            idx,
            name: name.to_owned(),
            planned: Vec::new(),
            report: AdapterReport {
                name: name.to_owned(),
                status: AdapterStatus::Ok,
                candidates: 0,
                candidate_bytes: 0,
                removed: 0,
                bytes_removed: 0,
                skipped: 0,
                queued: 0,
                failed: 0,
            },
        }
    }

    fn is_ok(&self) -> bool {
        matches!(self.report.status, AdapterStatus::Ok)
    }
}

/// The disk-saver engine. Borrows its collaborators and owns the adapters.
pub struct Engine<'a> {
    platform: &'a dyn crate::Platform,
    config: &'a Config,
    store: &'a Store,
    decisions: &'a DecisionLog,
    adapters: Vec<Box<dyn Adapter>>,
}

impl<'a> Engine<'a> {
    /// Assemble an engine from its collaborators and the built adapter set.
    pub fn new(
        platform: &'a dyn crate::Platform,
        config: &'a Config,
        store: &'a Store,
        decisions: &'a DecisionLog,
        adapters: Vec<Box<dyn Adapter>>,
    ) -> Self {
        Self {
            platform,
            config,
            store,
            decisions,
            adapters,
        }
    }

    /// Run the full lifecycle (§8). Always returns a report; a throttled or
    /// skipped exit returns one with `throttled = true`.
    pub fn run(&mut self, opts: &RunOptions) -> RunReport {
        let now = self.platform.now();
        let disk_path = self.disk_path();
        // A failed measurement fails SAFE: total = 0 makes classify_pressure
        // return Comfortable, and below_warn is false (no false pressure cadence
        // or scavenge-soon warning).
        let measured = self.measure(&disk_path);
        let free_before = measured.map(|u| u.available).unwrap_or(0);
        let total = measured.map(|u| u.total).unwrap_or(0);
        let below_warn = measured
            .map(|u| u.available < self.config.global.warn_below.bytes(u.total))
            .unwrap_or(false);

        // 2. THROTTLE GATE (skipped with --force).
        if !opts.force {
            let last = self.read_last_full_run();
            let due = run_due(
                now,
                last,
                below_warn,
                self.config.schedule.run_every,
                self.config.schedule.pressure_run_every,
            );
            if !due {
                let pressure = opts
                    .pressure_override
                    .unwrap_or_else(|| classify_pressure(free_before, total, &self.config.global));
                let queued_items = Approvals::open(self.store).pending(now).len();
                return RunReport {
                    at_unix: to_unix(now),
                    pressure: pressure.label().to_owned(),
                    throttled: true,
                    dry_run: opts.dry_run,
                    total,
                    free_before,
                    free_after: free_before,
                    adapters: Vec::new(),
                    queued_items,
                };
            }
        }

        let pressure = opts
            .pressure_override
            .unwrap_or_else(|| classify_pressure(free_before, total, &self.config.global));
        let decisions = self.decisions;
        let active = self.active_indices(opts);

        // 6. OBSERVE (isolated) — always, even when Comfortable.
        let mut runs: Vec<AdapterRun> = active
            .iter()
            .map(|&idx| AdapterRun::new(idx, self.adapters[idx].name()))
            .collect();
        for run in runs.iter_mut() {
            if let Err(skip) = self.run_phase(run.idx, pressure, |a, ctx| a.observe(ctx)) {
                run.report.status = skip.into();
            }
        }

        let mut free_after = free_before;
        let queued_items;

        if !pressure.deletes() {
            // 7. COMFORTABLE: GC approvals, no plan/execute. Only adapters that
            // observed OK this cycle are authoritative for GC (see rebuild).
            let approvals = Approvals::open(self.store);
            if !opts.dry_run {
                approvals.rebuild(now, &[], &authoritative_names(&runs));
            }
            queued_items = approvals.pending(now).len();
        } else {
            // 8. PLAN (isolated) for still-Ok adapters.
            for run in runs.iter_mut() {
                if !run.is_ok() {
                    continue;
                }
                match self.run_phase(run.idx, pressure, |a, ctx| a.plan(ctx)) {
                    Ok(cands) => {
                        run.report.candidates = cands.len();
                        run.report.candidate_bytes = cands
                            .iter()
                            .fold(0u64, |acc, c| acc.saturating_add(c.bytes));
                        run.planned = cands;
                    }
                    Err(skip) => run.report.status = skip.into(),
                }
            }

            // 9. ROUTE flagged candidates → approvals queue.
            let mut flagged: Vec<(String, Candidate)> = Vec::new();
            for run in runs.iter_mut() {
                if !run.is_ok() {
                    continue;
                }
                let mut queued = 0usize;
                for c in &run.planned {
                    if c.requires_confirmation {
                        queued += 1;
                        decisions.record(
                            now,
                            pressure,
                            &Decision::new(DecisionKind::Queued, run.name.clone(), c.id.clone())
                                .label(c.label.clone())
                                .bytes(c.bytes)
                                .age(c.age(now)),
                        );
                        flagged.push((run.name.clone(), c.clone()));
                    }
                }
                run.report.queued = queued;
            }
            let approvals = Approvals::open(self.store);
            queued_items = if opts.dry_run {
                approvals.pending(now).len()
            } else {
                approvals.rebuild(now, &flagged, &authoritative_names(&runs))
            };

            // 10. EXECUTE (never in dry-run; never flagged candidates).
            if !opts.dry_run {
                if let Pressure::Scavenge { .. } = pressure {
                    free_after = self.scavenge(&mut runs, pressure, &disk_path, free_before, total);
                } else {
                    self.execute_normal(&mut runs, pressure);
                    free_after = self
                        .measure(&disk_path)
                        .map(|u| u.available)
                        .unwrap_or(free_before);
                }
            }
        }

        // 11. REPORT.
        let report = RunReport {
            at_unix: to_unix(now),
            pressure: pressure.label().to_owned(),
            throttled: false,
            dry_run: opts.dry_run,
            total,
            free_before,
            free_after,
            adapters: runs.iter().map(|r| r.report.clone()).collect(),
            queued_items,
        };

        // Side effects are skipped for dry runs to keep `plan` side-effect-free.
        if !opts.dry_run {
            self.persist_report(&report, now);
            self.write_last_full_run(now);
            self.update_failstreaks(&runs, now);
            self.handle_scavenge_warning(below_warn, free_before, total, now);
            self.maybe_notify_approvals(queued_items, now);
            self.maybe_notify_scavenge_ran(pressure, free_before, free_after, total, now);
        }

        report
    }

    /// Run one adapter's `execute` on an approved batch now (used by `review`).
    pub fn execute_approved(
        &mut self,
        adapter: &str,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let idx = self
            .adapters
            .iter()
            .position(|a| a.name() == adapter)
            .ok_or_else(|| AdapterError::unavailable(format!("no such adapter: {adapter}")))?;
        // Approval is explicit user intent, so the batch is executed regardless
        // of pressure; the measurement only informs the Ctx an adapter sees. A
        // failed measurement → Comfortable (never fabricated pressure).
        let pressure = self
            .measure(&self.disk_path())
            .map(|u| classify_pressure(u.available, u.total, &self.config.global))
            .unwrap_or(Pressure::Comfortable);
        let name = self.adapters[idx].name();
        let bucket = self.store.bucket(name);
        let mut ctx = Ctx::new(self.platform, bucket, pressure, self.decisions);
        let adapter_ref: &mut dyn Adapter = &mut *self.adapters[idx];
        // Isolate an adapter panic here just as run_phase does for scheduled runs
        // (§10): a logic bug in one adapter must never abort `review`.
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            adapter_ref.execute(&mut ctx, batch)
        }));
        match caught {
            Ok(res) => res,
            Err(p) => Err(AdapterError::Failed(anyhow::anyhow!(panic_message(p)))),
        }
    }

    // ── phase execution & isolation ─────────────────────────────────────

    /// Build a [`Ctx`] for adapter `idx`, run `f` under `catch_unwind`, and map
    /// its result / panic into a `Skip`.
    fn run_phase<R, F>(&mut self, idx: usize, pressure: Pressure, f: F) -> Result<R, Skip>
    where
        F: FnOnce(&mut dyn Adapter, &mut Ctx<'_>) -> Result<R, AdapterError>,
    {
        let name = self.adapters[idx].name();
        let bucket = self.store.bucket(name);
        let mut ctx = Ctx::new(self.platform, bucket, pressure, self.decisions);
        let adapter: &mut dyn Adapter = &mut *self.adapters[idx];
        let caught =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(adapter, &mut ctx)));
        match caught {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(AdapterError::Unavailable(m))) => Err(Skip::Unavailable(m)),
            Ok(Err(AdapterError::Failed(e))) => Err(Skip::Failed(e.to_string())),
            Err(p) => Err(Skip::Failed(panic_message(p))),
        }
    }

    /// Normal mode: each Ok adapter executes all of its own unflagged candidates.
    fn execute_normal(&mut self, runs: &mut [AdapterRun], pressure: Pressure) {
        let decisions = self.decisions;
        let now = self.platform.now();
        for run in runs.iter_mut() {
            if !run.is_ok() {
                continue;
            }
            let batch: Vec<Candidate> = run
                .planned
                .iter()
                .filter(|c| !c.requires_confirmation)
                .cloned()
                .collect();
            if batch.is_empty() {
                continue;
            }
            let idx = run.idx;
            let res = self.run_phase(idx, pressure, |a, ctx| a.execute(ctx, &batch));
            apply_outcomes(run, res, decisions, now, pressure);
        }
    }

    /// Scavenge mode: impact waves (Rebuildable → Cache → UserData), oldest-first
    /// within a wave, per-adapter batches of ~10, re-measuring real free space
    /// after each batch and stopping once `scavenge_target` is reached. Returns
    /// the final free-space measurement.
    fn scavenge(
        &mut self,
        runs: &mut [AdapterRun],
        pressure: Pressure,
        disk_path: &Path,
        free_before: u64,
        total: u64,
    ) -> u64 {
        let decisions = self.decisions;
        let now = self.platform.now();
        let target = self.config.global.scavenge_target.bytes(total);
        let mut free_now = free_before;

        for class in [Class::Rebuildable, Class::Cache, Class::UserData] {
            if free_now >= target {
                break;
            }
            // Gather this class's unflagged candidates across Ok adapters,
            // oldest-first (smallest last_used first).
            let mut wave: Vec<(usize, Candidate)> = Vec::new();
            for (ri, run) in runs.iter().enumerate() {
                if !run.is_ok() {
                    continue;
                }
                for c in &run.planned {
                    if !c.requires_confirmation && c.class == class {
                        wave.push((ri, c.clone()));
                    }
                }
            }
            wave.sort_by_key(|e| e.1.last_used);

            let mut i = 0;
            while i < wave.len() {
                if free_now >= target {
                    break;
                }
                // Collect up to 10 consecutive candidates for the same adapter.
                let ri = wave[i].0;
                let mut batch = Vec::new();
                let mut j = i;
                while j < wave.len() && batch.len() < 10 && wave[j].0 == ri {
                    batch.push(wave[j].1.clone());
                    j += 1;
                }
                let idx = runs[ri].idx;
                let res = self.run_phase(idx, pressure, |a, ctx| a.execute(ctx, &batch));
                apply_outcomes(&mut runs[ri], res, decisions, now, pressure);
                // Real measurement, not candidate byte-estimates. If the disk can
                // no longer be measured, STOP scavenging: continuing blind would
                // over-delete (a `0` free reading would defeat the early-stop and
                // burn through every remaining wave).
                match self.measure(disk_path) {
                    Some(u) => free_now = u.available,
                    None => return free_now,
                }
                i = j;
            }
        }
        free_now
    }

    // ── engine-owned KV state (`_engine` bucket) ────────────────────────

    /// The filesystem whose free space drives the pressure model: the configured
    /// `global.disk` (tilde-expanded, so it matches the path the CLI validated
    /// and `status` reports), else the home directory.
    fn disk_path(&self) -> PathBuf {
        match &self.config.global.disk {
            Some(d) => crate::config::expand_tilde(d, &self.platform.home_dir()),
            None => self.platform.home_dir(),
        }
    }

    /// Measure the disk, or `None` if `statvfs` failed. Returning `None` (rather
    /// than a `{0,0}` sentinel) forces callers to decide the fail-safe explicitly:
    /// a run treats it as [`Pressure::Comfortable`] (delete nothing), and scavenge
    /// re-measurement stops rather than assuming zero free space.
    fn measure(&self, path: &Path) -> Option<DiskUsage> {
        match self.platform.disk_usage(path) {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "disk_usage failed; treating disk as comfortable (deleting nothing)");
                None
            }
        }
    }

    fn active_indices(&self, opts: &RunOptions) -> Vec<usize> {
        (0..self.adapters.len())
            .filter(|&i| match &opts.only {
                None => true,
                Some(names) => names.iter().any(|n| n == self.adapters[i].name()),
            })
            .collect()
    }

    fn read_last_full_run(&self) -> Option<SystemTime> {
        let secs: Option<u64> = self
            .store
            .bucket("_engine")
            .get("last_full_run")
            .ok()
            .flatten();
        secs.map(|s| UNIX_EPOCH + Duration::from_secs(s))
    }

    fn write_last_full_run(&self, now: SystemTime) {
        let _ = self
            .store
            .bucket("_engine")
            .set("last_full_run", &to_unix(now), now);
    }

    fn persist_report(&self, report: &RunReport, now: SystemTime) {
        let eng = self.store.bucket("_engine");
        let seq: u64 = eng.get("report_seq").ok().flatten().unwrap_or(0);
        let _ = eng.set(&format!("report:{seq}"), report, now);
        let _ = eng.set("report_seq", &seq.wrapping_add(1), now);
        // Ring buffer: keep the most recent ~50 reports.
        if seq >= 50 {
            let _ = eng.delete(&format!("report:{}", seq - 50));
        }
    }

    fn update_failstreaks(&self, runs: &[AdapterRun], now: SystemTime) {
        let eng = self.store.bucket("_engine");
        for run in runs {
            let key = format!("failstreak:{}", run.report.name);
            match run.report.status {
                AdapterStatus::Failed(_) => {
                    let n = eng
                        .get::<u64>(&key)
                        .ok()
                        .flatten()
                        .unwrap_or(0)
                        .saturating_add(1);
                    let _ = eng.set(&key, &n, now);
                    if n >= 3 {
                        let body = format!(
                            "The {} adapter has failed {n} consecutive runs — see `disk-saver status`.",
                            run.report.name
                        );
                        self.notify_event(
                            NotificationEvent::AdapterFailing,
                            now,
                            "disk-saver: adapter failing".to_owned(),
                            body,
                            Urgency::Normal,
                            false,
                        );
                    }
                }
                _ => {
                    let _ = eng.set(&key, &0u64, now);
                }
            }
        }
    }

    // ── notifications (§15.3) ───────────────────────────────────────────

    /// Emit a notification unless it is disabled, not in the `events` list, or
    /// suppressed by `min_gap` dedupe. A `transition` (e.g. crossing below
    /// `warn_below`) always fires. Records the last-notified timestamp.
    fn notify_event(
        &self,
        event: NotificationEvent,
        now: SystemTime,
        title: String,
        body: String,
        urgency: Urgency,
        transition: bool,
    ) {
        if !self.config.notifications.enabled || !self.config.notifications.events.contains(&event)
        {
            return;
        }
        let eng = self.store.bucket("_engine");
        let key = format!("notified:{}", event_key(event));
        if !transition
            && let Ok(Some(last)) = eng.get::<u64>(&key)
            && to_unix(now).saturating_sub(last) < self.config.notifications.min_gap.as_secs()
        {
            return;
        }
        let _ = self.platform.notify(&Notification {
            title,
            body,
            urgency,
        });
        let _ = eng.set(&key, &to_unix(now), now);
    }

    fn handle_scavenge_warning(&self, below_warn: bool, free: u64, total: u64, now: SystemTime) {
        let eng = self.store.bucket("_engine");
        let was_below: bool = eng.get("was_below_warn").ok().flatten().unwrap_or(false);
        let _ = eng.set("was_below_warn", &below_warn, now);
        if !below_warn {
            return;
        }
        let transition = !was_below;
        let pct = if total > 0 {
            (free as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let body = format!(
            "Disk space getting low ({pct:.0}% free). Scavenge begins below the scavenge \
             threshold — cleanup now runs more often."
        );
        self.notify_event(
            NotificationEvent::ScavengeWarning,
            now,
            "disk-saver: low disk space".to_owned(),
            body,
            Urgency::Normal,
            transition,
        );
    }

    fn maybe_notify_approvals(&self, queued_items: usize, now: SystemTime) {
        if queued_items == 0 {
            return;
        }
        let bytes = Approvals::open(self.store).total_bytes_pending(now);
        let body = format!(
            "{queued_items} item(s) ({}) await approval — run `disk-saver review`.",
            human_bytes(bytes)
        );
        self.notify_event(
            NotificationEvent::ApprovalsPending,
            now,
            "disk-saver: approvals pending".to_owned(),
            body,
            Urgency::Normal,
            false,
        );
    }

    fn maybe_notify_scavenge_ran(
        &self,
        pressure: Pressure,
        free_before: u64,
        free_after: u64,
        total: u64,
        now: SystemTime,
    ) {
        if !matches!(pressure, Pressure::Scavenge { .. }) {
            return;
        }
        let freed = free_after.saturating_sub(free_before);
        let target = self.config.global.scavenge_target.bytes(total);
        let body = if free_after >= target {
            format!(
                "Freed {} — {} now free.",
                human_bytes(freed),
                human_bytes(free_after)
            )
        } else {
            format!("Freed {} but still below target.", human_bytes(freed))
        };
        self.notify_event(
            NotificationEvent::ScavengeRan,
            now,
            "disk-saver: scavenge ran".to_owned(),
            body,
            Urgency::Normal,
            false,
        );
    }
}

/// The names of adapters that ran to completion (`is_ok()`) this cycle — the
/// set whose vanished approvals entries may be GC'd (see [`Approvals::rebuild`]).
fn authoritative_names(runs: &[AdapterRun]) -> BTreeSet<String> {
    runs.iter()
        .filter(|r| r.is_ok())
        .map(|r| r.name.clone())
        .collect()
}

/// Unix seconds for `t`, saturating to zero before the epoch.
fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best-effort human size for notification text.
fn human_bytes(b: u64) -> String {
    bytesize::ByteSize(b).to_string()
}

/// Stable snake_case key for a notification event (for dedupe bookkeeping).
fn event_key(e: NotificationEvent) -> &'static str {
    match e {
        NotificationEvent::ScavengeWarning => "scavenge_warning",
        NotificationEvent::ScavengeRan => "scavenge_ran",
        NotificationEvent::ApprovalsPending => "approvals_pending",
        NotificationEvent::AdapterFailing => "adapter_failing",
    }
}

/// Extract a human-readable message from a caught panic payload.
fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "adapter panicked".to_owned()
    }
}

impl From<Skip> for AdapterStatus {
    fn from(s: Skip) -> Self {
        match s {
            Skip::Unavailable(m) => AdapterStatus::Unavailable(m),
            Skip::Failed(m) => AdapterStatus::Failed(m),
        }
    }
}

/// Fold execution outcomes into an adapter's report and record decisions.
fn apply_outcomes(
    run: &mut AdapterRun,
    res: Result<Vec<Outcome>, Skip>,
    decisions: &DecisionLog,
    now: SystemTime,
    pressure: Pressure,
) {
    match res {
        Ok(outcomes) => {
            for o in &outcomes {
                match o {
                    Outcome::Removed { id, bytes } => {
                        run.report.removed += 1;
                        run.report.bytes_removed = run.report.bytes_removed.saturating_add(*bytes);
                        decisions.record(
                            now,
                            pressure,
                            &Decision::new(DecisionKind::Deleted, run.name.clone(), id.clone())
                                .bytes(*bytes),
                        );
                    }
                    Outcome::Skipped { id, reason } => {
                        run.report.skipped += 1;
                        decisions.record(
                            now,
                            pressure,
                            &Decision::new(DecisionKind::Skipped, run.name.clone(), id.clone())
                                .reason(reason.clone()),
                        );
                    }
                    Outcome::Failed { id, error } => {
                        run.report.failed += 1;
                        decisions.record(
                            now,
                            pressure,
                            &Decision::new(DecisionKind::Failed, run.name.clone(), id.clone())
                                .reason(error.clone()),
                        );
                    }
                }
            }
        }
        Err(skip) => run.report.status = skip.into(),
    }
}

#[cfg(test)]
mod tests;
