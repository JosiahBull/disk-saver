//! `doctor` — config validation plus a readiness probe of every enabled
//! adapter (§13).
//!
//! Runs each adapter's [`check`](disk_saver_core::Adapter::check) with a fresh,
//! comfortable-pressure [`Ctx`] and reports reachable / unavailable / failed.
//! Adapter-level problems are *reported*, not fatal — `doctor` still exits `0`.
//! Only a config or environment error (which prevents probing at all) exits `1`.
//!
//! `--grant-access` additionally walks the user through the OS permissions the
//! tool cannot grant itself; see [`grant_access`].

use anyhow::{Context, Result};
use disk_saver_core::{AdapterError, CommandSpec, Ctx, Platform, Pressure};
use serde::Serialize;

use crate::app::App;
use crate::output;

/// The outcome of probing one adapter.
#[derive(Serialize)]
struct AdapterCheck {
    name: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// Run the doctor probe.
pub fn run(app: &App) -> Result<u8> {
    app.config.validate().context("invalid configuration")?;

    let store = app.open_store()?;
    let decisions = app.open_decisions();
    let mut adapters = app.build_adapters()?;

    let mut checks = Vec::with_capacity(adapters.len());
    for adapter in adapters.iter_mut() {
        let name = adapter.name().to_string();
        let bucket = store.bucket(&name);
        let mut ctx = Ctx::new(&app.platform, bucket, Pressure::Comfortable, &decisions);
        let check = match adapter.check(&mut ctx) {
            Ok(()) => AdapterCheck {
                name,
                status: "reachable",
                detail: None,
            },
            Err(AdapterError::Unavailable(reason)) => AdapterCheck {
                name,
                status: "unavailable",
                detail: Some(reason),
            },
            Err(AdapterError::Failed(err)) => AdapterCheck {
                name,
                status: "failed",
                detail: Some(err.to_string()),
            },
        };
        checks.push(check);
    }

    if app.json {
        output::print_json(&serde_json::json!({
            "config_path": app.config_path.display().to_string(),
            "adapters": &checks,
        }))?;
    } else {
        println!("config OK: {}", app.config_path.display());
        if checks.is_empty() {
            println!("no adapters enabled");
        }
        for c in &checks {
            match &c.detail {
                Some(d) => println!("  {:<14} {} ({})", c.name, c.status, d),
                None => println!("  {:<14} {}", c.name, c.status),
            }
        }
    }
    Ok(0)
}

/// Walk the user through granting the OS permissions disk-saver cannot request.
///
/// On macOS the only one that matters is Full Disk Access, which the trash
/// adapter needs to read `~/.Trash`. **There is deliberately no API to request
/// it**: unlike camera or contacts, `kTCCServiceSystemPolicyAllFiles` has no
/// `requestAccess` counterpart, and touching a protected path just returns
/// `EPERM` without raising a prompt — precisely so that malware cannot
/// social-engineer a click. The most a program can do is establish whether it
/// currently has access, name the exact binary the user must add, and open the
/// right pane. So that is what this does.
pub fn grant_access(app: &App) -> Result<u8> {
    if !cfg!(target_os = "macos") {
        println!("--grant-access is macOS-only; no permission gates apply here.");
        return Ok(0);
    }

    let blocked = blocked_trash_dirs(app);
    if blocked.is_empty() {
        println!("Full Disk Access: already granted — the trash is readable.");
        return Ok(0);
    }

    println!("Full Disk Access: NOT granted");
    for (path, err) in &blocked {
        println!("  cannot read {} ({err})", path.display());
    }
    println!();
    println!(
        "macOS has no API to request Full Disk Access — it is the one permission\n\
         that cannot raise a prompt, so it has to be granted by hand. Add this\n\
         exact binary in the pane that is about to open:"
    );

    let exe = current_exe_path();
    println!("\n    {}\n", exe.display());
    println!("  1. Click + (an admin password is required).");
    println!("  2. Press Cmd-Shift-G, paste the path above, and choose it.");
    println!("  3. Re-run `disk-saver doctor` to confirm the trash is readable.");

    if let Some(cdhash) = adhoc_cdhash(app, &exe) {
        println!(
            "\nNote: this binary is ad-hoc signed, so the grant is bound to its exact\n\
             contents (cdhash {cdhash}). Rebuilding or reinstalling disk-saver changes\n\
             that hash and silently invalidates the grant — you would have to remove\n\
             and re-add the entry. Signing with a stable identity avoids this."
        );
    }

    println!("\nOpening System Settings → Privacy & Security → Full Disk Access…");
    open_full_disk_access_pane(app)?;
    Ok(0)
}

/// Trash directories that exist but cannot be read, with the reason.
fn blocked_trash_dirs(app: &App) -> Vec<(std::path::PathBuf, std::io::Error)> {
    app.platform
        .trash_dirs()
        .into_iter()
        .filter_map(|dir| match app.platform.read_dir(&dir) {
            Ok(_) => None,
            // A trash directory that simply is not there is not a permission
            // problem and must not be reported as one.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some((dir, e)),
        })
        .collect()
}

/// The running binary's real path — symlinks resolved, because that is the path
/// TCC records and the one the user has to pick in the file dialog.
fn current_exe_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| "disk-saver".into());
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

/// The cdhash this binary's designated requirement is pinned to, if it is
/// ad-hoc signed. `None` when signed with a real identity (a stable requirement
/// that survives rebuilds) or when `codesign` cannot be run.
fn adhoc_cdhash(app: &App, exe: &std::path::Path) -> Option<String> {
    let spec = CommandSpec::new("codesign", ["-d", "-r-", &exe.display().to_string()]);
    let out = app.platform.run_command(&spec).ok()?;
    // `codesign` writes the requirement to stderr on some releases and stdout on
    // others; check both rather than depending on which.
    let text = format!("{}{}", out.stdout_string(), out.stderr_string());
    let line = text.lines().find(|l| l.contains("designated =>"))?;
    let hash = line.split("cdhash H\"").nth(1)?.split('"').next()?;
    Some(format!("{}…", &hash[..hash.len().min(8)]))
}

/// Open the Full Disk Access pane.
///
/// `Privacy_AllFiles` is the pane's `revealElementKeyName` (see
/// `SecurityPrivacyExtension.appex`'s `TCCServiceList.plist`), reached through
/// the privacy extension's `legacyBundleIdentifier`.
fn open_full_disk_access_pane(app: &App) -> Result<()> {
    const PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
    let spec = CommandSpec::new("open", [PANE]);
    match app.platform.run_command(&spec) {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => {
            println!(
                "  (could not open System Settings: `open` exited {}. Go to\n   \
                 System Settings → Privacy & Security → Full Disk Access.)",
                out.status
            );
            Ok(())
        }
        Err(e) => {
            println!(
                "  (could not open System Settings: {e}. Go to\n   \
                 System Settings → Privacy & Security → Full Disk Access.)"
            );
            Ok(())
        }
    }
}
