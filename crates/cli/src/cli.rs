//! The `clap` command-line surface (§13).
//!
//! This module defines only the *shape* of the CLI — the parsed [`Cli`] struct,
//! the global flags, and the whole subcommand tree. Behaviour lives in
//! [`crate::commands`]. The tree is wired in full now (Stage 1) so later stages
//! only fill in command bodies, never the parser.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use disk_saver_core::Pressure;

/// A small, boring, trustworthy disk janitor for macOS and Linux.
#[derive(Debug, Parser)]
#[command(name = "disk-saver", version, about, long_about = None)]
pub struct Cli {
    /// Path to the config file (overrides `$DISK_SAVER_CONFIG` and
    /// `~/.disk-saver.toml`).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Raise the logging verbosity (INFO → DEBUG).
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// Emit machine-readable JSON instead of a human summary, where supported.
    #[arg(long, global = true)]
    pub json: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The top-level subcommands (§13).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one full cleanup cycle (the scheduled entry point).
    Run(RunArgs),

    /// Alias for `run --dry-run`: show the plan, change nothing.
    Plan(PlanArgs),

    /// Inspect and resolve the approvals queue (Stage 2).
    Review(ReviewArgs),

    /// Show disk usage, cadence state, pending approvals, and recent runs
    /// (Stage 2).
    Status,

    /// Replay every recorded decision about an item (requires the
    /// `decision-log` feature; Stage 2).
    #[cfg(feature = "decision-log")]
    Why(WhyArgs),

    /// Validate config and probe every enabled adapter's readiness.
    Doctor,

    /// Manage the configuration file.
    Config(ConfigArgs),

    /// Manage the OS scheduler unit (Stage 2).
    Schedule(ScheduleArgs),

    /// Inspect or reset an adapter's KV bucket (escape hatch).
    State(StateArgs),
}

/// Arguments to `run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Plan only: observe and plan, but never route, execute, or persist.
    #[arg(long)]
    pub dry_run: bool,

    /// Bypass the throttle gate — force a full run now.
    #[arg(long)]
    pub force: bool,

    /// Restrict the run to these adapters (repeatable). Empty = all enabled.
    #[arg(long = "adapter", value_name = "NAME")]
    pub adapter: Vec<String>,

    /// Override measured disk pressure.
    #[arg(long, value_enum)]
    pub pressure: Option<PressureArg>,
}

/// Arguments to `plan` (a dry run). Mirrors [`RunArgs`] minus `--dry-run`.
#[derive(Debug, Args)]
pub struct PlanArgs {
    /// Restrict the plan to these adapters (repeatable). Empty = all enabled.
    #[arg(long = "adapter", value_name = "NAME")]
    pub adapter: Vec<String>,

    /// Override measured disk pressure.
    #[arg(long, value_enum)]
    pub pressure: Option<PressureArg>,
}

/// The `--pressure` override values.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PressureArg {
    /// Observe only; delete nothing.
    Comfortable,
    /// Delete items past each adapter's `max_age`.
    Normal,
    /// Delete down to `scavenge_target`, lowest-impact first.
    Scavenge,
}

impl PressureArg {
    /// Convert to the engine's [`Pressure`]. `Scavenge` carries `need = 0`; the
    /// engine drives to `scavenge_target` from the real measurement, so the
    /// `need` estimate is informational only.
    pub fn to_pressure(self) -> Pressure {
        match self {
            PressureArg::Comfortable => Pressure::Comfortable,
            PressureArg::Normal => Pressure::Normal,
            PressureArg::Scavenge => Pressure::Scavenge { need: 0 },
        }
    }
}

/// Arguments to `review` (Stage 2 fills in the body).
#[derive(Debug, Args)]
pub struct ReviewArgs {
    /// List the queue non-interactively (pair with `--json` for scripting).
    #[arg(long)]
    pub list: bool,

    /// Approve specific queued items by key/id (repeatable).
    #[arg(long = "approve", value_name = "ID")]
    pub approve: Vec<String>,

    /// Approve every pending item (optionally limited by `--adapter`).
    #[arg(long)]
    pub approve_all: bool,

    /// Limit `--approve-all` to a single adapter.
    #[arg(long = "adapter", value_name = "NAME")]
    pub adapter: Option<String>,
}

/// Arguments to `why` (Stage 2 fills in the body).
#[cfg(feature = "decision-log")]
#[derive(Debug, Args)]
pub struct WhyArgs {
    /// An id or path substring to search the decision log for.
    #[arg(value_name = "QUERY")]
    pub query: String,
}

/// Arguments to `config`.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// The config action.
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// `config` subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Write a fully-commented default config file.
    Init,
    /// Load, validate, and build all enabled adapters against the config.
    Check,
    /// Print the effective configuration.
    Show,
}

/// Arguments to `schedule` (Stage 2 fills in the body).
#[derive(Debug, Args)]
pub struct ScheduleArgs {
    /// The schedule action.
    #[command(subcommand)]
    pub action: ScheduleAction,
}

/// `schedule` subcommands.
#[derive(Debug, Subcommand)]
pub enum ScheduleAction {
    /// Install the launchd agent / systemd user timer.
    Install,
    /// Remove the installed unit.
    Uninstall,
    /// Show installed-unit / config drift.
    Status,
}

/// Arguments to `state`.
#[derive(Debug, Args)]
pub struct StateArgs {
    /// The state action.
    #[command(subcommand)]
    pub action: StateAction,
}

/// `state` subcommands.
#[derive(Debug, Subcommand)]
pub enum StateAction {
    /// Delete every key in an adapter's KV bucket.
    Clear {
        /// The adapter (KV bucket) name.
        #[arg(value_name = "ADAPTER")]
        adapter: String,
    },
    /// Dump every key/value in an adapter's KV bucket.
    Show {
        /// The adapter (KV bucket) name.
        #[arg(value_name = "ADAPTER")]
        adapter: String,
    },
}
