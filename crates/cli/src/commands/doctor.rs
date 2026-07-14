//! `doctor` — config validation plus a readiness probe of every enabled
//! adapter (§13).
//!
//! Runs each adapter's [`check`](disk_saver_core::Adapter::check) with a fresh,
//! comfortable-pressure [`Ctx`] and reports reachable / unavailable / failed.
//! Adapter-level problems are *reported*, not fatal — `doctor` still exits `0`.
//! Only a config or environment error (which prevents probing at all) exits `1`.

use anyhow::{Context, Result};
use disk_saver_core::{AdapterError, Ctx, Pressure};
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
