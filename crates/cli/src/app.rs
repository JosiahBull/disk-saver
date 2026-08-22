//! Shared per-invocation context and CLI plumbing.
//!
//! [`App`] bundles everything a command needs after the global flags are
//! resolved: the [`RealPlatform`], the loaded [`Config`], the resolved config
//! path, the created state directory, and the `--json` flag. It also owns the
//! plumbing that must *not* go through the [`Platform`] abstraction — config
//! path / state-dir resolution (the only place `cfg(target_os)` is allowed
//! outside the `platform` crate), the KV store, the decision log, the
//! single-instance lock, and the adapter factory registry.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use disk_saver_core::{Adapter, Config, DecisionLog, Platform, RealPlatform, Store, expand_tilde};

use crate::registry;

/// The name of the environment variable that overrides the config path.
const CONFIG_ENV: &str = "DISK_SAVER_CONFIG";

/// The resolved context for a single CLI invocation.
pub struct App {
    /// The production platform (real filesystem, clock, subprocesses).
    pub platform: RealPlatform,
    /// The path the config was loaded from (may not exist → defaults).
    pub config_path: PathBuf,
    /// The fully-resolved configuration.
    pub config: Config,
    /// The created state directory (`state.db`, `lock`, `logs/`, decisions).
    pub state_dir: PathBuf,
    /// Whether `--json` was requested.
    pub json: bool,
}

impl App {
    /// The filesystem whose free space drives the pressure model: the configured
    /// `global.disk` (tilde-expanded) or, failing that, the home directory.
    pub fn disk_path(&self) -> PathBuf {
        match &self.config.global.disk {
            Some(d) => expand_tilde(d, &self.platform.home_dir()),
            None => self.platform.home_dir(),
        }
    }

    /// Measure the disk backing [`App::disk_path`].
    pub fn disk_usage(&self) -> Result<disk_saver_core::DiskUsage> {
        let path = self.disk_path();
        self.platform
            .disk_usage(&path)
            .with_context(|| format!("measuring disk usage at {}", path.display()))
    }

    /// Open the KV store at `<state_dir>/state.db`.
    pub fn open_store(&self) -> Result<Store> {
        let path = self.state_dir.join("state.db");
        Store::open(&path).with_context(|| format!("opening KV store at {}", path.display()))
    }

    /// Open the decision log. With the `decision-log` feature it appends to
    /// `<state_dir>/decisions.jsonl` (falling back to disabled on I/O error);
    /// without it, it is a zero-cost no-op.
    pub fn open_decisions(&self) -> DecisionLog {
        #[cfg(feature = "decision-log")]
        {
            let path = self.state_dir.join("decisions.jsonl");
            match DecisionLog::to_path(path.clone()) {
                Ok(log) => log,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "could not open decision log; decisions will not be recorded");
                    DecisionLog::disabled()
                }
            }
        }
        #[cfg(not(feature = "decision-log"))]
        {
            DecisionLog::disabled()
        }
    }

    /// Build every *enabled* adapter from the factory registry.
    ///
    /// A config section for an adapter with no matching factory is a WARN (not
    /// fatal). An adapter whose `enabled` is not explicitly `false` is built;
    /// its `rest` table is handed to the factory. A build error is fatal (the
    /// caller maps it to exit code 1).
    pub fn build_adapters(&self) -> Result<Vec<Box<dyn Adapter>>> {
        let factories = registry::registry();
        let known: Vec<&'static str> = factories.iter().map(|f| f.name).collect();

        for name in self.config.adapters.keys() {
            if !known.contains(&name.as_str()) {
                tracing::warn!(
                    adapter = %name,
                    "config has a section for an unknown adapter (no such factory); ignoring it"
                );
            }
        }

        let mut built: Vec<Box<dyn Adapter>> = Vec::new();
        for factory in factories {
            let section = self.config.adapters.get(factory.name);
            let enabled = section.map(|s| s.enabled).unwrap_or(true);
            if !enabled {
                tracing::debug!(adapter = %factory.name, "adapter disabled in config; skipping");
                continue;
            }
            let rest = section.and_then(|s| s.rest.clone());
            let adapter = factory
                .build(rest)
                .with_context(|| format!("building adapter '{}'", factory.name))?;
            built.push(adapter);
        }
        Ok(built)
    }

    /// Run `f` while holding the single-instance advisory lock at
    /// `<state_dir>/lock`.
    ///
    /// The lock file is `flock`-style (via `fd-lock`); the guard is held for the
    /// whole closure and released when it returns. If another instance already
    /// holds the lock, `f` is *not* run and `on_contended` is returned instead
    /// (the caller logs and exits cleanly). An error only occurs if the lock
    /// file itself cannot be opened.
    pub fn with_lock<T>(&self, on_contended: T, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let path = self.state_dir.join("lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening lock file {}", path.display()))?;

        let mut lock = fd_lock::RwLock::new(file);
        match lock.try_write() {
            Ok(_guard) => f(),
            Err(_) => {
                tracing::info!(
                    "another disk-saver run is already in progress; exiting without changes"
                );
                Ok(on_contended)
            }
        }
    }
}

/// Resolve the config path from `--config`, then `$DISK_SAVER_CONFIG`, then
/// `~/.disk-saver.toml`. A leading `~` is expanded against the platform home.
pub fn resolve_config_path(flag: Option<&Path>, platform: &dyn Platform) -> PathBuf {
    let home = platform.home_dir();
    if let Some(p) = flag {
        return expand_tilde(p, &home);
    }
    if let Ok(env) = std::env::var(CONFIG_ENV)
        && !env.is_empty()
    {
        return expand_tilde(Path::new(&env), &home);
    }
    home.join(".disk-saver.toml")
}

/// Resolve the state directory (CLI plumbing; not routed through `Platform`).
///
/// Order: `global.state_dir` (tilde-expanded), else the per-OS default —
/// `~/Library/Application Support/disk-saver` on macOS, else
/// `$XDG_STATE_HOME/disk-saver` or `~/.local/state/disk-saver`. The directory
/// and its `logs/` subdirectory are created if missing.
pub fn resolve_state_dir(config: &Config, platform: &dyn Platform) -> Result<PathBuf> {
    let home = platform.home_dir();
    let dir = match &config.global.state_dir {
        Some(d) => expand_tilde(d, &home),
        None => default_state_dir(&home),
    };
    std::fs::create_dir_all(dir.join("logs"))
        .with_context(|| format!("creating state directory {}", dir.display()))?;
    Ok(dir)
}

/// The per-OS default state directory. This is the one place `cfg(target_os)`
/// is permitted outside the `platform` crate.
fn default_state_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/disk-saver")
    }
    #[cfg(not(target_os = "macos"))]
    {
        match std::env::var_os("XDG_STATE_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x).join("disk-saver"),
            _ => home.join(".local/state/disk-saver"),
        }
    }
}
