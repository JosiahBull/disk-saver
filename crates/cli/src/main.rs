//! `disk-saver` — a small, boring, trustworthy disk janitor for macOS and Linux.
//!
//! This binary is deliberately thin: it parses the CLI (`clap`), sets up logging
//! (`tracing`), loads config, resolves the state directory, takes the
//! single-instance lock, wires the [`Engine`](disk_saver_core::Engine) to the
//! feature-gated adapter registry, and maps results to exit codes. All policy
//! lives in `disk-saver-core` and the adapter crates.
//!
//! Exit codes (§8): `0` clean (including throttled / unavailable-skips), `1` a
//! config or environment error where nothing ran, `2` at least one adapter
//! failed while the rest of the run completed.

#![forbid(unsafe_code)]

mod app;
mod cli;
mod commands;
mod logging;
mod output;
mod registry;

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use disk_saver_core::{Config, RealPlatform};

use crate::app::App;
use crate::cli::{Cli, Command, ConfigAction};
use crate::commands::run::RunParams;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

/// Resolve global flags, then dispatch to the chosen command. Returns the
/// desired exit code; an `Err` becomes exit code `1`.
fn dispatch(cli: Cli) -> Result<u8> {
    let platform = RealPlatform::new();
    let config_path = app::resolve_config_path(cli.config.as_deref(), &platform);

    // `config init` must work even when no (or an unloadable) config exists, so
    // handle it before attempting to load one.
    if let Command::Config(args) = &cli.command
        && matches!(args.action, ConfigAction::Init)
    {
        logging::init_interactive(cli.verbose);
        return commands::config::init(&platform, &config_path, cli.json);
    }

    let config = Config::load(&config_path, &platform)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    let state_dir = app::resolve_state_dir(&config, &platform)?;
    let _guard = logging::init(&state_dir, cli.verbose);

    let app = App {
        platform,
        config_path,
        config,
        state_dir,
        json: cli.json,
    };

    match cli.command {
        Command::Run(args) => commands::run::execute(&app, RunParams::from_run(args)),
        Command::Plan(args) => commands::run::execute(&app, RunParams::from_plan(args)),
        Command::Review(args) => commands::review::execute(&app, args),
        Command::Status => commands::status::run(&app),
        #[cfg(feature = "decision-log")]
        Command::Why(args) => commands::why::run(&app, &args.query),
        Command::Doctor => commands::doctor::run(&app),
        Command::Config(args) => match args.action {
            ConfigAction::Init => unreachable!("`config init` is handled before config load"),
            ConfigAction::Check => commands::config::check(&app),
            ConfigAction::Show => commands::config::show(&app),
        },
        Command::Schedule(args) => match args.action {
            cli::ScheduleAction::Install => commands::schedule::install(&app),
            cli::ScheduleAction::Uninstall => commands::schedule::uninstall(&app),
            cli::ScheduleAction::Status => commands::schedule::status(&app),
        },
        Command::State(args) => match args.action {
            cli::StateAction::Clear { adapter } => commands::state::clear(&app, &adapter),
            cli::StateAction::Show { adapter } => commands::state::show(&app, &adapter),
        },
    }
}
