//! `disk-saver-adapter-pip` — prunes pip's global HTTP/wheel cache.
//!
//! pip caches downloaded packages and locally-built wheels under the platform
//! cache directory (`~/Library/Caches/pip` on macOS, `$XDG_CACHE_HOME/pip` or
//! `~/.cache/pip` on Linux). It is re-fetchable/re-buildable, so it is
//! [`Class::Cache`](disk_saver_core::Class) and pruned when idle past the
//! retention policy. `$PIP_CACHE_DIR` is not read in v1; point the `paths`
//! config key at a custom cache if you set it.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use disk_saver_cachedir::os_cache_dirs;
use disk_saver_core::{AdapterFactory, Platform};

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "pip";

/// pip's cache directory. Derived from `home` and tried for every OS layout
/// (`~/Library/Caches/pip`, `~/.cache/pip`), plus the platform cache dir so an
/// `$XDG_CACHE_HOME` override is still honored. Non-existent ones are skipped.
fn resolver(p: &dyn Platform) -> Vec<PathBuf> {
    let mut dirs = os_cache_dirs(&p.home_dir(), "pip");
    dirs.push(p.user_cache_dir().join("pip"));
    dirs
}

/// The factory the CLI registry uses to build the pip adapter. Everything
/// but the name and the `resolver` is shared with the other
/// cache-directory adapters.
pub fn factory() -> AdapterFactory {
    disk_saver_cachedir::factory(NAME, resolver)
}
