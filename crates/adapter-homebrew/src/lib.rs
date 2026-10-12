//! Homebrew's completed download cache only; never Cellar, Caskroom, or installed packages.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use disk_saver_core::{AdapterFactory, CommandSpec, Platform, expand_tilde};

fn root(p: &dyn Platform, configured: Option<&Path>) -> PathBuf {
    let base = if let Some(path) = configured {
        expand_tilde(path, &p.home_dir())
    } else if let Ok(out) = p.run_command(&CommandSpec::new("brew", ["--cache"]))
        && out.success()
        && out.stdout_string().trim().lines().count() == 1
    {
        PathBuf::from(out.stdout_string().trim())
    } else {
        p.user_cache_dir().join("Homebrew")
    };
    base.join("downloads")
}

fn archive(name: &str) -> bool {
    // Homebrew downloads are SHA-256-prefixed; arbitrary cache files are not candidates.
    let Some((hash, _)) = name.split_once("--") else {
        return false;
    };
    hash.len() == 64
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && disk_saver_cachedir::downloads::archive_name(name)
}

/// Build the Homebrew download-cache adapter.
pub fn factory() -> AdapterFactory {
    disk_saver_cachedir::downloads::factory("homebrew", root, archive)
}
