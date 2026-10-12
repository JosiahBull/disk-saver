use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// nvm retirement and cache policy.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Optional nvm root; otherwise NVM_DIR or ~/.nvm.
    pub nvm_dir: Option<PathBuf>,
    /// Include installed versions; these always require approval.
    pub include_versions: bool,
    /// Project trees checked for .nvmrc and .node-version pins.
    pub roots: Vec<PathBuf>,
    /// Additional version/alias specifications to protect.
    pub protect: Vec<String>,
    /// Normal inactivity threshold.
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
    /// Scavenge inactivity floor.
    #[serde(with = "humantime_serde")]
    pub min_age: Duration,
    /// Require approval for cache archives too. Versions always require it.
    pub confirm: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nvm_dir: None,
            include_versions: true,
            roots: vec![PathBuf::from("~/dev")],
            protect: vec![],
            max_age: Duration::from_secs(90 * 86400),
            min_age: Duration::from_secs(30 * 86400),
            confirm: true,
        }
    }
}
