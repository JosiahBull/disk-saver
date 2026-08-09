//! `config init | check | show` (§13, §7).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use disk_saver_core::{Config, Platform, Threshold};

use crate::app::App;
use crate::output;

/// `config init`: write a fully-commented default config file.
///
/// Refuses to clobber an existing file (prints a note and exits `0`). Only
/// [`Platform`] and the resolved path are needed, so this runs even when an
/// existing config is unloadable.
pub fn init(platform: &dyn Platform, config_path: &Path, json: bool) -> Result<u8> {
    if platform.metadata(config_path).is_ok() {
        if json {
            output::print_json(&serde_json::json!({
                "status": "exists",
                "path": config_path.display().to_string(),
            }))?;
        } else {
            println!(
                "config already exists at {} — not overwriting",
                config_path.display()
            );
        }
        return Ok(0);
    }

    if let Some(parent) = config_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(config_path, Config::default_toml())
        .with_context(|| format!("writing config to {}", config_path.display()))?;

    if json {
        output::print_json(&serde_json::json!({
            "status": "written",
            "path": config_path.display().to_string(),
        }))?;
    } else {
        println!("wrote default config to {}", config_path.display());
    }
    Ok(0)
}

/// `config check`: validate structure + thresholds, and build every enabled
/// adapter (surfacing any [`ConfigError`](disk_saver_core::ConfigError)).
pub fn check(app: &App) -> Result<u8> {
    app.config.validate().context("invalid configuration")?;
    let usage = app.disk_usage()?;
    app.config
        .global
        .validate_thresholds(usage.total)
        .context("invalid disk thresholds")?;
    let adapters = app.build_adapters()?;
    let names: Vec<&str> = adapters.iter().map(|a| a.name()).collect();

    if app.json {
        output::print_json(&serde_json::json!({
            "status": "ok",
            "config_path": app.config_path.display().to_string(),
            "enabled_adapters": names,
        }))?;
    } else {
        println!("config OK: {}", app.config_path.display());
        println!(
            "{} enabled adapter(s): {}",
            names.len(),
            if names.is_empty() {
                "(none)".to_string()
            } else {
                names.join(", ")
            }
        );
    }
    Ok(0)
}

/// `config show`: print the effective configuration (defaults merged with the
/// file), as TOML for humans or JSON with `--json`.
pub fn show(app: &App) -> Result<u8> {
    let value = effective_config_value(&app.config);
    if app.json {
        output::print_json(&value)?;
    } else {
        let text = toml::to_string_pretty(&value).context("rendering effective config as TOML")?;
        print!("{text}");
    }
    Ok(0)
}

/// Render a [`Threshold`] back to a config-style string.
fn threshold_str(t: &Threshold) -> String {
    match t {
        Threshold::Percent(p) => format!("{p}%"),
        Threshold::Absolute(b) => bytesize::ByteSize(*b).to_string(),
    }
}

/// Render a [`Duration`] as a humantime string (e.g. `12h`).
fn duration_str(d: &Duration) -> String {
    humantime::format_duration(*d).to_string()
}

/// Build a `toml::Value` mirror of the effective config. This is display-only
/// (`Config` itself is intentionally not `Serialize`); both the human TOML and
/// the `--json` output are rendered from it.
fn effective_config_value(config: &Config) -> toml::Value {
    use toml::Value;
    use toml::value::Table;

    let mut global = Table::new();
    if let Some(disk) = &config.global.disk {
        global.insert("disk".into(), Value::String(disk.display().to_string()));
    }
    global.insert(
        "start_cleaning_below".into(),
        Value::String(threshold_str(&config.global.start_cleaning_below)),
    );
    global.insert(
        "warn_below".into(),
        Value::String(threshold_str(&config.global.warn_below)),
    );
    global.insert(
        "scavenge_below".into(),
        Value::String(threshold_str(&config.global.scavenge_below)),
    );
    global.insert(
        "scavenge_target".into(),
        Value::String(threshold_str(&config.global.scavenge_target)),
    );
    global.insert(
        "aggressive_prune_age".into(),
        Value::String(duration_str(&config.global.aggressive_prune_age)),
    );
    global.insert(
        "command_timeout".into(),
        Value::String(duration_str(&config.global.command_timeout)),
    );
    if let Some(state_dir) = &config.global.state_dir {
        global.insert(
            "state_dir".into(),
            Value::String(state_dir.display().to_string()),
        );
    }

    let mut schedule = Table::new();
    schedule.insert(
        "check_every".into(),
        Value::String(duration_str(&config.schedule.check_every)),
    );
    schedule.insert(
        "run_every".into(),
        Value::String(duration_str(&config.schedule.run_every)),
    );
    schedule.insert(
        "pressure_run_every".into(),
        Value::String(duration_str(&config.schedule.pressure_run_every)),
    );

    let mut notifications = Table::new();
    notifications.insert(
        "enabled".into(),
        Value::Boolean(config.notifications.enabled),
    );
    notifications.insert(
        "min_gap".into(),
        Value::String(duration_str(&config.notifications.min_gap)),
    );
    notifications.insert(
        "events".into(),
        Value::Array(
            config
                .notifications
                .events
                .iter()
                .map(|e| Value::String(event_str(*e)))
                .collect(),
        ),
    );

    let mut adapters = Table::new();
    for (name, section) in &config.adapters {
        let mut table = match &section.rest {
            Some(Value::Table(t)) => t.clone(),
            _ => Table::new(),
        };
        table.insert("enabled".into(), Value::Boolean(section.enabled));
        adapters.insert(name.clone(), Value::Table(table));
    }

    let mut root = Table::new();
    root.insert("global".into(), Value::Table(global));
    root.insert("schedule".into(), Value::Table(schedule));
    root.insert("notifications".into(), Value::Table(notifications));
    root.insert("adapters".into(), Value::Table(adapters));
    Value::Table(root)
}

/// The snake_case wire name of a notification event.
fn event_str(e: disk_saver_core::NotificationEvent) -> String {
    use disk_saver_core::NotificationEvent as E;
    match e {
        E::ScavengeWarning => "scavenge_warning",
        E::ScavengeRan => "scavenge_ran",
        E::ApprovalsPending => "approvals_pending",
        E::AdapterFailing => "adapter_failing",
        E::AggressivePrune => "aggressive_prune",
    }
    .to_string()
}
