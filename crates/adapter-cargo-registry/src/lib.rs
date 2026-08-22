//! `disk-saver-adapter-cargo-registry` — prunes cargo's global download caches.
//!
//! `cargo` caches everything it fetches under `~/.cargo`: downloaded crate
//! tarballs (`registry/cache`), their extracted sources (`registry/src`), and
//! git dependencies (`git/db`, `git/checkouts`). All of it is re-fetchable from
//! crates.io / the git remotes, so it is [`Class::Cache`](disk_saver_core::Class)
//! and pruned when idle past the retention policy. The registry *index* is left
//! alone (small, and re-downloading it is slow).
//!
//! Each of the four directories is treated independently, so an actively-used
//! `registry/src` can be spared while a stale `git/checkouts` is reclaimed.
//! `$CARGO_HOME` is not honoured in v1 (the default `~/.cargo` is assumed); point
//! the `paths` config key at a custom location if you relocate it.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use disk_saver_core::{AdapterFactory, Platform};

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "cargo-registry";

/// The re-fetchable cargo cache directories under `~/.cargo`.
fn resolver(p: &dyn Platform) -> Vec<PathBuf> {
    let cargo = p.home_dir().join(".cargo");
    vec![
        cargo.join("registry").join("cache"),
        cargo.join("registry").join("src"),
        cargo.join("git").join("db"),
        cargo.join("git").join("checkouts"),
    ]
}

/// The factory the CLI registry uses to build the cargo-registry adapter. Everything
/// but the name and the `resolver` is shared with the other
/// cache-directory adapters.
pub fn factory() -> AdapterFactory {
    disk_saver_cachedir::factory(NAME, resolver)
}
