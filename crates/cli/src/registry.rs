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
/// stable factory order keeps runs reproducible. An adapter sees only the
/// claims of adapters before it, so `git-ignored` follows the filesystem
/// adapters whose artifacts it must leave to them.
pub fn registry() -> Vec<AdapterFactory> {
    vec![
        #[cfg(feature = "docker")]
        disk_saver_adapter_docker::factory(),
        #[cfg(feature = "orbstack")]
        disk_saver_adapter_orbstack::factory(),
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
        #[cfg(feature = "nvm")]
        disk_saver_adapter_nvm::factory(),
        #[cfg(feature = "rustup")]
        disk_saver_adapter_rustup::factory(),
        #[cfg(feature = "uv")]
        disk_saver_adapter_uv::factory(),
        #[cfg(feature = "npm")]
        disk_saver_adapter_npm::factory(),
        #[cfg(feature = "homebrew")]
        disk_saver_adapter_homebrew::factory(),
        #[cfg(feature = "cachedir-tag")]
        disk_saver_adapter_cachedir_tag::factory(),
        #[cfg(feature = "orphaned-worktree")]
        disk_saver_adapter_orphaned_worktree::factory(),
        #[cfg(feature = "git-ignored")]
        disk_saver_adapter_git_ignored::factory(),
        #[cfg(feature = "app-temp")]
        disk_saver_adapter_app_temp::factory(),
        #[cfg(feature = "trash")]
        disk_saver_adapter_trash::factory(),
    ]
}

#[cfg(test)]
mod tests {
    use super::registry;

    #[test]
    fn git_ignored_plans_after_the_adapters_whose_claims_it_honours() {
        let names: Vec<&str> = registry().iter().map(|f| f.name).collect();
        let at = |name| names.iter().position(|n| *n == name);
        let Some(git_ignored) = at("git-ignored") else {
            return;
        };
        for owner in [
            "node-modules",
            "rust-target",
            "python-cache",
            "orphaned-worktree",
        ] {
            if let Some(i) = at(owner) {
                assert!(i < git_ignored, "{owner} must plan before git-ignored");
            }
        }
    }

    #[test]
    fn cachedir_tag_plans_after_the_adapters_whose_claims_it_honours() {
        let names: Vec<&str> = registry().iter().map(|f| f.name).collect();
        let at = |name| names.iter().position(|n| *n == name);
        let Some(cachedir_tag) = at("cachedir-tag") else {
            return;
        };
        for owner in ["node-modules", "rust-target", "python-cache"] {
            if let Some(i) = at(owner) {
                assert!(i < cachedir_tag, "{owner} must plan before cachedir-tag");
            }
        }
    }
}
