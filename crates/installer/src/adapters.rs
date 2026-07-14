//! The adapter catalogue the wizard offers, plus generation of the
//! `~/.disk-saver.toml` text from the user's choices.
//!
//! The installer is intentionally decoupled from the CLI's compiled-in registry
//! (which is feature-gated and not exposed as a library): this small table is
//! the single source of truth for what the wizard shows.

/// One selectable adapter in the wizard.
pub struct AdapterInfo {
    /// Config section / adapter name.
    pub name: &'static str,
    /// One-line description shown in the list.
    pub desc: &'static str,
    /// Whether the adapter takes a `roots` list (filesystem / git adapters).
    pub takes_roots: bool,
}

/// The v1 adapter catalogue, in the CLI's registry order.
pub const ADAPTERS: &[AdapterInfo] = &[
    AdapterInfo {
        name: "docker",
        desc: "Docker images, containers, build cache",
        takes_roots: false,
    },
    AdapterInfo {
        name: "node-modules",
        desc: "node_modules/ directories",
        takes_roots: true,
    },
    AdapterInfo {
        name: "rust-target",
        desc: "cargo target/ directories",
        takes_roots: true,
    },
    AdapterInfo {
        name: "python-cache",
        desc: "__pycache__, .pytest_cache, .mypy_cache, …",
        takes_roots: true,
    },
    AdapterInfo {
        name: "git-gc",
        desc: "git gc on idle repos (shrinks .git)",
        takes_roots: true,
    },
    AdapterInfo {
        name: "pnpm",
        desc: "pnpm store + cache",
        takes_roots: false,
    },
    AdapterInfo {
        name: "cargo-registry",
        desc: "~/.cargo registry & git caches",
        takes_roots: false,
    },
    AdapterInfo {
        name: "pip",
        desc: "pip download / wheel cache",
        takes_roots: false,
    },
    AdapterInfo {
        name: "git-ignored",
        desc: "git-ignored objects — asks first",
        takes_roots: true,
    },
    AdapterInfo {
        name: "trash",
        desc: "OS trash / recycle bin — asks first",
        takes_roots: false,
    },
];

/// A free-space threshold preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Clean early and often.
    Aggressive,
    /// The built-in defaults.
    Balanced,
    /// Clean only when space is genuinely tight.
    Conservative,
}

impl Preset {
    /// All presets, in display order.
    pub const ALL: [Preset; 3] = [Preset::Balanced, Preset::Conservative, Preset::Aggressive];

    /// Short label for the list.
    pub fn label(self) -> &'static str {
        match self {
            Preset::Aggressive => "Aggressive",
            Preset::Balanced => "Balanced (default)",
            Preset::Conservative => "Conservative",
        }
    }

    /// One-line description.
    pub fn desc(self) -> &'static str {
        match self {
            Preset::Aggressive => "start<30% warn<20% scavenge<12% → 25%, full run every 6h",
            Preset::Balanced => "start<20% warn<12% scavenge<8% → 15%, full run every 12h",
            Preset::Conservative => "start<10% warn<7% scavenge<5% → 12%, full run every 24h",
        }
    }

    /// `(start_cleaning_below, warn_below, scavenge_below, scavenge_target)`.
    fn thresholds(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Preset::Aggressive => ("30%", "20%", "12%", "25%"),
            Preset::Balanced => ("20%", "12%", "8%", "15%"),
            Preset::Conservative => ("10%", "7%", "5%", "12%"),
        }
    }

    fn run_every(self) -> &'static str {
        match self {
            Preset::Aggressive => "6h",
            Preset::Balanced => "12h",
            Preset::Conservative => "24h",
        }
    }
}

/// The user's answers, assembled by the wizard.
pub struct Choices {
    /// Threshold/cadence preset.
    pub preset: Preset,
    /// Enable flags, parallel to [`ADAPTERS`].
    pub enabled: Vec<bool>,
    /// Raw `roots` field (comma-separated) for the root-taking adapters.
    pub roots: String,
}

impl Choices {
    /// Sensible starting point: every adapter on, scanning `~/dev`.
    pub fn defaults() -> Self {
        Choices {
            preset: Preset::Balanced,
            enabled: vec![true; ADAPTERS.len()],
            roots: "~/dev".to_string(),
        }
    }
}

/// Parse the comma-separated `roots` field into a TOML array literal, e.g.
/// `~/dev, ~/work` → `["~/dev", "~/work"]`. Empty → `["~"]` (the built-in default).
fn roots_array(raw: &str) -> String {
    let items: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{}\"", s.replace('"', "")))
        .collect();
    if items.is_empty() {
        "[\"~\"]".to_string()
    } else {
        format!("[{}]", items.join(", "))
    }
}

/// Render the `~/.disk-saver.toml` text for the given choices.
pub fn render_config(choices: &Choices) -> String {
    let (start, warn, scavenge, target) = choices.preset.thresholds();
    let roots = roots_array(&choices.roots);

    let mut out = String::new();
    out.push_str("# disk-saver configuration — generated by disk-saver-installer.\n");
    out.push_str("# Edit freely; `disk-saver config check` validates it.\n\n");

    out.push_str("[global]\n");
    out.push_str(&format!("start_cleaning_below = \"{start}\"\n"));
    out.push_str(&format!("warn_below           = \"{warn}\"\n"));
    out.push_str(&format!("scavenge_below       = \"{scavenge}\"\n"));
    out.push_str(&format!("scavenge_target      = \"{target}\"\n\n"));

    out.push_str("[schedule]\n");
    out.push_str(&format!(
        "run_every = \"{}\"\n\n",
        choices.preset.run_every()
    ));

    out.push_str("# Adapters. A disabled one is turned off explicitly; a root-taking one\n");
    out.push_str("# is pointed at your project directories. The rest use built-in defaults.\n");
    for (info, &enabled) in ADAPTERS.iter().zip(&choices.enabled) {
        if !enabled {
            out.push_str(&format!("\n[adapters.{}]\nenabled = false\n", info.name));
        } else if info.takes_roots {
            out.push_str(&format!("\n[adapters.{}]\nroots = {roots}\n", info.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_array_parses_and_quotes() {
        assert_eq!(roots_array("~/dev"), "[\"~/dev\"]");
        assert_eq!(roots_array(" ~/dev , ~/work "), "[\"~/dev\", \"~/work\"]");
        assert_eq!(roots_array(""), "[\"~\"]");
    }

    #[test]
    fn generated_config_is_valid_and_reflects_choices() {
        let mut choices = Choices::defaults();
        choices.preset = Preset::Aggressive;
        // Disable docker; keep the rest.
        choices.enabled[0] = false;
        let text = render_config(&choices);

        // Parses under the real core config loader.
        let cfg = disk_saver_core::Config::parse(&text).expect("generated config must parse");
        cfg.validate().expect("generated config must validate");

        // Reflects the choices.
        assert!(text.contains("start_cleaning_below = \"30%\""));
        assert!(text.contains("run_every = \"6h\""));
        assert!(text.contains("[adapters.docker]\nenabled = false"));
        assert!(text.contains("[adapters.rust-target]\nroots = [\"~/dev\"]"));
        // docker is disabled, not enabled-with-defaults.
        assert_eq!(cfg.adapters.get("docker").map(|s| s.enabled), Some(false));
    }

    #[test]
    fn every_preset_produces_valid_config() {
        for preset in Preset::ALL {
            let choices = Choices {
                preset,
                enabled: vec![true; ADAPTERS.len()],
                roots: "~/dev".into(),
            };
            let text = render_config(&choices);
            disk_saver_core::Config::parse(&text)
                .and_then(|c| c.validate())
                .unwrap_or_else(|e| panic!("{} config invalid: {e}", preset.label()));
        }
    }
}
