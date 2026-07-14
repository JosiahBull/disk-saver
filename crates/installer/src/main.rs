//! Interactive TUI installer for disk-saver.
//!
//! By default it runs a wizard — pick a cleaning preset, choose adapters, set
//! project roots, review the generated `~/.disk-saver.toml`, then step through
//! the install plan — and afterwards writes the config, installs the binary onto
//! your PATH, and registers the scheduler unit. Pass `--non-interactive` to run
//! the same plan headlessly (dotfiles / CI).
//!
//! The heavy TUI lives in its own crate so the shipped `disk-saver` binary stays
//! lean (it has no TUI dependency); build this on demand with
//! `cargo run -p disk-saver-installer`.

mod adapters;
mod app;
mod diff;
mod plan;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use app::{Outcome, Wizard};
use clap::Parser;
use plan::Decided;

/// Set up disk-saver: write the config, install the binary, and register the
/// scheduler. Interactive by default; pass `--non-interactive` for scripted use.
#[derive(Debug, Parser)]
#[command(name = "disk-saver-installer", version, about)]
struct Cli {
    /// Run the install plan headlessly, without the TUI.
    #[arg(long, short = 'y', visible_alias = "yes")]
    non_interactive: bool,

    /// Config to install as ~/.disk-saver.toml (default: a sensible generated
    /// one). Non-interactive only.
    #[arg(long, value_name = "FILE", requires = "non_interactive")]
    config: Option<PathBuf>,

    /// Print the default generated config and exit.
    #[arg(long)]
    print_config: bool,

    /// List the adapters the wizard offers and exit.
    #[arg(long)]
    list_adapters: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.list_adapters {
        for a in adapters::ADAPTERS {
            println!("{:<16} {}", a.name, a.desc);
        }
        return ExitCode::SUCCESS;
    }
    if cli.print_config {
        print!(
            "{}",
            adapters::render_config(&adapters::Choices::defaults())
        );
        return ExitCode::SUCCESS;
    }

    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("error: HOME is not set");
        return ExitCode::FAILURE;
    };

    let Some(bin) = plan::locate_binary(&home) else {
        eprintln!(
            "error: could not find a `disk-saver` binary. Build it first \
             (cargo build --release) or set DISK_SAVER_BIN=/path/to/disk-saver."
        );
        return ExitCode::FAILURE;
    };

    if cli.non_interactive {
        return run_non_interactive(&home, &bin, cli.config.as_deref());
    }

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("The installer needs an interactive terminal (run it directly, not piped).");
        eprintln!("For scripted installs, pass --non-interactive.");
        return ExitCode::FAILURE;
    }

    let mut terminal = ratatui::init();
    let outcome = Wizard::new(home, bin).run(&mut terminal);
    ratatui::restore();

    match outcome {
        Ok(Outcome::Install(plan)) => execute(&plan),
        Ok(Outcome::Cancelled) => {
            println!("Cancelled — nothing was changed.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Headless install: config from `--config` (or a generated default), then the
/// full plan applied without prompting.
fn run_non_interactive(home: &Path, bin: &Path, config_arg: Option<&Path>) -> ExitCode {
    let config = match config_arg {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("error: cannot read --config {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => adapters::render_config(&adapters::Choices::defaults()),
    };

    // Validate before touching anything.
    if let Err(e) = disk_saver_core::Config::parse(&config).and_then(|c| c.validate()) {
        eprintln!("error: generated/supplied config is invalid: {e}");
        return ExitCode::FAILURE;
    }

    let plan: Vec<Decided> = plan::build(config, bin, home)
        .into_iter()
        .map(|action| Decided {
            action,
            apply: true,
        })
        .collect();
    execute(&plan)
}

/// Apply the decided plan with plain-text logging (the TUI, if any, is already
/// torn down, so it is safe to print here).
fn execute(plan: &[Decided]) -> ExitCode {
    let mut failed = false;
    for step in plan {
        if !step.apply {
            println!("• skipped: {}", step.action.summary());
            continue;
        }
        println!("→ {}", step.action.summary());
        if let Err(e) = step.action.execute() {
            failed = true;
            eprintln!("  failed: {e}");
        }
    }
    if failed {
        eprintln!("\nFinished with errors.");
        return ExitCode::FAILURE;
    }
    println!("\n✓ disk-saver is set up. Try `disk-saver status` or `disk-saver plan`.");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn config_requires_non_interactive() {
        assert!(Cli::try_parse_from(["x", "--config", "/a.toml"]).is_err());
        assert!(Cli::try_parse_from(["x", "-y", "--config", "/a.toml"]).is_ok());
    }
}
