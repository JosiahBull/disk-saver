//! `run` and its dry-run alias `plan` (§8).
//!
//! Acquires the single-instance lock, validates config against the measured
//! disk, builds the engine, drives one full lifecycle, and renders the report.
//! The exit code comes from
//! [`RunReport::any_failed`](disk_saver_core::RunReport::any_failed).

use anyhow::{Context, Result};
use disk_saver_core::{Engine, Pressure, RunOptions};

use crate::app::App;
use crate::cli::{PlanArgs, RunArgs};
use crate::output;

/// Resolved options for one `run`/`plan` invocation.
pub struct RunParams {
    /// Plan only (no route/execute/persist).
    pub dry_run: bool,
    /// Bypass the throttle gate.
    pub force: bool,
    /// Adapter-name filter (empty = all enabled).
    pub adapters: Vec<String>,
    /// Pressure override.
    pub pressure: Option<Pressure>,
}

impl RunParams {
    /// Build params from `run` arguments.
    pub fn from_run(args: RunArgs) -> Self {
        RunParams {
            dry_run: args.dry_run,
            force: args.force,
            adapters: args.adapter,
            pressure: args.pressure.map(|p| p.to_pressure()),
        }
    }

    /// Build params from `plan` arguments.
    ///
    /// `plan` is an on-demand preview, so it bypasses the throttle gate
    /// (`force = true`): a dry run persists nothing (it never writes
    /// `last_full_run`, routes, or executes), so forcing it is side-effect-free
    /// and guarantees the preview reflects the *current* plan rather than an
    /// empty "throttled" report when a scheduled run happened recently. Use
    /// `run --dry-run` for a throttle-respecting dry run.
    pub fn from_plan(args: PlanArgs) -> Self {
        RunParams {
            dry_run: true,
            force: true,
            adapters: args.adapter,
            pressure: args.pressure.map(|p| p.to_pressure()),
        }
    }
}

/// Execute a `run`/`plan`, taking the single-instance lock first. If the lock is
/// already held, log and exit `0` without doing anything.
pub fn execute(app: &App, params: RunParams) -> Result<u8> {
    app.with_lock(0u8, || run_locked(app, &params))
}

/// The body that runs while the lock is held.
fn run_locked(app: &App, params: &RunParams) -> Result<u8> {
    // Config / environment validation up front: any failure here means nothing
    // ran, which the caller maps to exit code 1.
    app.config.validate().context("invalid configuration")?;
    let usage = app.disk_usage()?;
    app.config
        .global
        .validate_thresholds(usage.total)
        .context("invalid disk thresholds")?;

    let store = app.open_store()?;
    let decisions = app.open_decisions();
    let adapters = app.build_adapters()?;

    let only = if params.adapters.is_empty() {
        None
    } else {
        Some(params.adapters.clone())
    };
    let opts = RunOptions {
        dry_run: params.dry_run,
        force: params.force,
        only,
        pressure_override: params.pressure,
    };

    let mut engine = Engine::new(&app.platform, &app.config, &store, &decisions, adapters);
    let report = engine.run(&opts);

    if app.json {
        output::print_json(&report)?;
    } else {
        output::print_run_report(&report);
    }

    Ok(if report.any_failed() { 2 } else { 0 })
}
