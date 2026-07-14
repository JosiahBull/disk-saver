//! The static, feature-gated adapter registry (§13).
//!
//! Each v1 adapter lives behind its own cargo feature (all on by default). The
//! registry is the single list the CLI iterates to build the enabled adapter
//! set; adding an adapter is a one-line change here plus a feature flag.

use disk_saver_core::AdapterFactory;

/// The compiled-in adapter factories, in a stable, deterministic order.
///
/// Order matters: the engine observes/plans adapters in this order, and the
/// scavenge impact-wave ordering is by [`Class`](disk_saver_core::Class), so a
/// stable factory order keeps runs reproducible.
pub fn registry() -> Vec<AdapterFactory> {
    vec![
        #[cfg(feature = "docker")]
        disk_saver_adapter_docker::factory(),
        #[cfg(feature = "node-modules")]
        disk_saver_adapter_node_modules::factory(),
        #[cfg(feature = "rust-target")]
        disk_saver_adapter_rust_target::factory(),
        #[cfg(feature = "python-cache")]
        disk_saver_adapter_python_cache::factory(),
        #[cfg(feature = "git-gc")]
        disk_saver_adapter_git_gc::factory(),
        #[cfg(feature = "pnpm")]
        disk_saver_adapter_pnpm::factory(),
        #[cfg(feature = "cargo-registry")]
        disk_saver_adapter_cargo_registry::factory(),
        #[cfg(feature = "pip")]
        disk_saver_adapter_pip::factory(),
        #[cfg(feature = "git-ignored")]
        disk_saver_adapter_git_ignored::factory(),
        #[cfg(feature = "trash")]
        disk_saver_adapter_trash::factory(),
    ]
}
