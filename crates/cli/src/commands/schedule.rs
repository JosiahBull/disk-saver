//! `schedule install | uninstall | status` — manage the OS scheduler unit
//! (§14).
//!
//! Cadence lives in `[schedule]` config, not the timer; the timer only fires the
//! cheap probe (`disk-saver run`) every `check_every`. This module renders that
//! unit from config and writes it to the per-OS location. It is the only
//! OS-branching code outside the `platform` crate, and it is CLI-level: macOS
//! gets a launchd agent, everything else a systemd user timer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use disk_saver_core::Platform;

use crate::app::App;
use crate::output;

/// The scheduler kind for the current OS (for messages / JSON).
#[cfg(target_os = "macos")]
const KIND: &str = "launchd";
/// The scheduler kind for the current OS (for messages / JSON).
#[cfg(not(target_os = "macos"))]
const KIND: &str = "systemd";

/// One unit file to write: its path and rendered contents.
struct Unit {
    /// Where the unit file lives.
    path: PathBuf,
    /// The rendered file contents.
    contents: String,
}

/// `schedule install`: validate config, then render and write the unit file(s).
pub fn install(app: &App) -> Result<u8> {
    // Same validation as `config check` (structure + threshold ordering).
    app.config.validate().context("invalid configuration")?;
    let usage = app.disk_usage()?;
    app.config
        .global
        .validate_thresholds(usage.total)
        .context("invalid disk thresholds")?;

    let exe = std::env::current_exe().context("resolving current executable path")?;
    let units = build_units(app, &exe);

    for unit in &units {
        if let Some(parent) = unit.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&unit.path, &unit.contents)
            .with_context(|| format!("writing unit {}", unit.path.display()))?;
    }

    let paths: Vec<String> = units.iter().map(|u| u.path.display().to_string()).collect();
    if app.json {
        output::print_json(&serde_json::json!({
            "action": "install",
            "kind": KIND,
            "units": paths,
            "interval_secs": app.config.schedule.check_every.as_secs(),
        }))?;
    } else {
        println!("installed {KIND} unit(s):");
        for p in &paths {
            println!("  {p}");
        }
        println!("{}", activate_hint());
    }
    Ok(0)
}

/// `schedule uninstall`: remove the unit file(s) if present (idempotent).
pub fn uninstall(app: &App) -> Result<u8> {
    let exe = std::env::current_exe().unwrap_or_default();
    let units = build_units(app, &exe);

    let mut removed = Vec::new();
    for unit in &units {
        if unit.path.exists() {
            std::fs::remove_file(&unit.path)
                .with_context(|| format!("removing unit {}", unit.path.display()))?;
            removed.push(unit.path.display().to_string());
        }
    }

    if app.json {
        output::print_json(&serde_json::json!({
            "action": "uninstall",
            "kind": KIND,
            "removed": removed,
        }))?;
    } else if removed.is_empty() {
        println!("no {KIND} unit was installed");
    } else {
        println!("removed {KIND} unit(s):");
        for p in &removed {
            println!("  {p}");
        }
    }
    Ok(0)
}

/// `schedule status`: report whether the unit is installed and whether its
/// interval matches `schedule.check_every` (drift).
pub fn status(app: &App) -> Result<u8> {
    let exe = std::env::current_exe().unwrap_or_default();
    let units = build_units(app, &exe);
    // The interval-carrying unit is the last one (launchd: plist; systemd: timer).
    let primary = units.last().expect("build_units always yields a unit");

    let configured = app.config.schedule.check_every.as_secs();
    let installed = primary.path.exists();
    let installed_interval = if installed {
        std::fs::read_to_string(&primary.path)
            .ok()
            .and_then(|t| parse_interval(&t))
    } else {
        None
    };
    let drift = installed_interval.is_some_and(|i| i != configured);
    let paths: Vec<String> = units.iter().map(|u| u.path.display().to_string()).collect();

    if app.json {
        output::print_json(&serde_json::json!({
            "kind": KIND,
            "installed": installed,
            "unit_paths": paths,
            "configured_interval_secs": configured,
            "installed_interval_secs": installed_interval,
            "drift": drift,
        }))?;
    } else if !installed {
        println!("{KIND}: not installed");
        println!("  run `disk-saver schedule install` to create the unit.");
    } else {
        println!("{KIND}: installed");
        for p in &paths {
            println!("  {p}");
        }
        match installed_interval {
            Some(i) if drift => println!(
                "  drift: installed interval {i}s != configured check_every {configured}s — \
                 re-run `disk-saver schedule install`."
            ),
            Some(i) => println!("  interval: {i}s (matches config)"),
            None => println!("  interval: could not parse from the installed unit"),
        }
    }
    Ok(0)
}

// ── per-OS unit rendering ───────────────────────────────────────────────────

/// Build the unit file(s) for the current OS. The interval-carrying unit is
/// always last.
#[cfg(target_os = "macos")]
fn build_units(app: &App, exe: &Path) -> Vec<Unit> {
    let path = app
        .platform
        .home_dir()
        .join("Library/LaunchAgents/com.disk-saver.plist");
    let err_log = app.state_dir.join("logs/launchd.err");
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.disk-saver</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
    <key>StartInterval</key>
    <integer>{secs}</integer>
    <key>StandardErrorPath</key>
    <string>{err}</string>
    <key>RunAtLoad</key>
    <false/>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.display().to_string()),
        path = LAUNCHD_PATH,
        secs = app.config.schedule.check_every.as_secs(),
        err = xml_escape(&err_log.display().to_string()),
    );
    vec![Unit { path, contents }]
}

/// `PATH` handed to the launchd job.
///
/// launchd does not read a login shell, so without this the job runs with the
/// bare `/usr/bin:/bin:/usr/sbin:/sbin`. Adapters that shell out to a tool
/// installed elsewhere — `docker` under `/usr/local/bin`, `pnpm` and `git` under
/// Homebrew — then probe as *unavailable* on every scheduled run while still
/// reporting reachable from an interactive `doctor`, which makes the failure
/// close to invisible. Both Homebrew prefixes are listed so one plist serves
/// Intel and Apple Silicon.
#[cfg(target_os = "macos")]
const LAUNCHD_PATH: &str = "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Build the unit file(s) for the current OS. The interval-carrying unit is
/// always last.
#[cfg(not(target_os = "macos"))]
fn build_units(app: &App, exe: &Path) -> Vec<Unit> {
    let base = app.platform.home_dir().join(".config/systemd/user");
    let service = Unit {
        path: base.join("disk-saver.service"),
        contents: format!(
            "[Unit]\n\
             Description=disk-saver one-shot cleanup\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={exe} run\n",
            exe = exe.display(),
        ),
    };
    let timer = Unit {
        path: base.join("disk-saver.timer"),
        contents: format!(
            "[Unit]\n\
             Description=disk-saver periodic check\n\
             \n\
             [Timer]\n\
             OnBootSec=5min\n\
             OnUnitActiveSec={secs}s\n\
             Persistent=true\n\
             RandomizedDelaySec=5min\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n",
            secs = app.config.schedule.check_every.as_secs(),
        ),
    };
    vec![service, timer]
}

/// The activation hint printed after `install`.
#[cfg(target_os = "macos")]
fn activate_hint() -> String {
    "load it with `launchctl load ~/Library/LaunchAgents/com.disk-saver.plist`.".to_owned()
}

/// The activation hint printed after `install`.
#[cfg(not(target_os = "macos"))]
fn activate_hint() -> String {
    "enable it with `systemctl --user enable --now disk-saver.timer`.".to_owned()
}

/// Extract the fire interval (seconds) from an installed unit's contents.
#[cfg(target_os = "macos")]
fn parse_interval(text: &str) -> Option<u64> {
    let (_, after_key) = text.split_once("<key>StartInterval</key>")?;
    let (_, after_open) = after_key.split_once("<integer>")?;
    let (value, _) = after_open.split_once("</integer>")?;
    value.trim().parse().ok()
}

/// Extract the fire interval (seconds) from an installed unit's contents.
#[cfg(not(target_os = "macos"))]
fn parse_interval(text: &str) -> Option<u64> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("OnUnitActiveSec="))
        .find_map(|v| v.trim().trim_end_matches('s').trim().parse().ok())
}

/// Minimal XML text escaping for values embedded in the plist.
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
