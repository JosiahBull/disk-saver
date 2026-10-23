//! The protection set: every installed toolchain something still relies on,
//! with the reason, plus the references that no longer count (for the label).

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use disk_saver_core::{AdapterError, FileKind, Platform};
use disk_saver_scan::{MarkerScan, ScanOptions, find_marker_files};
use serde::Deserialize;

use crate::commands::Show;
use crate::label::{date, display};
use crate::toolchain::{resolve, valid_name};

/// The files rustup reads a project's toolchain from.
const PIN_FILES: &[&str] = &["rust-toolchain", "rust-toolchain.toml"];

/// Build output a project regenerates: its mtimes say nothing about whether
/// anyone works on the project, and walking it would be slow.
const NOT_ACTIVITY: &[&str] = &["target", "node_modules"];

/// A toolchain file is a line or two of text. Anything larger is not one.
const MAX_PIN_BYTES: u64 = 64 * 1024;

/// The parts of `<rustup home>/settings.toml` that protect toolchains.
#[derive(Debug, Default, Deserialize)]
struct Settings {
    default_toolchain: Option<String>,
    /// `rustup override set`: directory → toolchain.
    #[serde(default)]
    overrides: BTreeMap<String, String>,
}

/// Read rustup's settings. A missing file is rustup with nothing set; an
/// unreadable or unparseable one leaves the default toolchain unknown, which
/// refuses the run.
fn read_settings(p: &dyn Platform, rustup_home: &Path) -> Result<Settings, AdapterError> {
    let path = rustup_home.join("settings.toml");
    match p.read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| {
            AdapterError::unavailable(format!("cannot parse {}: {}", path.display(), e.message()))
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(AdapterError::unavailable(format!(
            "cannot read {}: {e}",
            path.display()
        ))),
    }
}

/// What a pin walk could not establish.
#[derive(Debug, Default)]
pub(crate) struct PinGaps {
    /// Pins in *active* projects that could not be read or parsed. Each may
    /// name any toolchain, so while one exists the protection set is unknown.
    pub broken: Vec<String>,
    /// Directories the walk could not read, sorted and without repeats. A
    /// pin in one was not seen. Directories below `max_depth` are not listed:
    /// the depth limit is a choice shared by every walk, not a failure, and
    /// is hit routinely where no project lives (vendored chart trees).
    pub unreadable: Vec<PathBuf>,
}

/// Installed toolchain names that are kept, and why; and the pins and
/// overrides that name a toolchain but no longer protect it.
#[derive(Debug)]
pub(crate) struct Protection {
    /// The host triple, completing channel names (`1.88.0` → `1.88.0-<host>`).
    host: String,
    /// Every entry in the toolchains directory, for exact-name resolution.
    names: BTreeSet<String>,
    kept: BTreeMap<String, String>,
    stale: BTreeMap<String, Vec<String>>,
}

impl Protection {
    /// Everything except project pins, which need a walk: the default
    /// toolchain, the host's channel toolchains, and directory overrides.
    ///
    /// `stable-<host>`, `beta-<host>` and `nightly-<host>` are kept whatever
    /// their age: each follows a channel, `rustup update` replaces it in place,
    /// and so it is never the "old" toolchain a pin bump leaves behind. The
    /// same channels built for another host get no such pass.
    ///
    /// An override protects its toolchain for as long as its directory exists,
    /// without the `pin_max_idle` test pins get. It is an explicit instruction
    /// in rustup's own settings rather than a file that travels with a
    /// checkout, there are few of them, and `rustup override unset` removes
    /// one. An override for a directory that is gone protects nothing.
    pub(crate) fn base(
        p: &dyn Platform,
        rustup_home: &Path,
        show: &Show,
        names: &BTreeSet<String>,
    ) -> Result<Self, AdapterError> {
        let settings = read_settings(p, rustup_home)?;
        let host = &show.host;
        let home = p.home_dir();
        let mut protection = Protection {
            host: host.clone(),
            names: names.clone(),
            kept: BTreeMap::new(),
            stale: BTreeMap::new(),
        };
        for name in settings.default_toolchain.iter().chain(&show.defaults) {
            protection.keep(protection.resolve(name), "the default toolchain".into());
        }
        for channel in ["stable", "beta", "nightly"] {
            protection.keep(
                format!("{channel}-{host}"),
                format!("follows the {channel} channel, which `rustup update` keeps current"),
            );
        }
        for (dir, toolchain) in &settings.overrides {
            let name = protection.resolve(toolchain);
            let shown = display(&home, Path::new(dir));
            match p.metadata(Path::new(dir)) {
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    protection.stale(name, format!("{shown} (rustup override; directory gone)"));
                }
                // Present, or impossible to tell: either way it still counts.
                _ => protection.keep(name, format!("rustup override for {shown}")),
            }
        }
        Ok(protection)
    }

    /// Walk `roots` for `rust-toolchain` / `rust-toolchain.toml` pins. A pin
    /// whose project was active within `max_idle` protects its toolchain; an
    /// idle one is only remembered for the label. A pin in a project carrying
    /// `.disk-saver-keep` counts like any other: the sentinel exempts a
    /// project from deletion, not from protecting what it names.
    ///
    /// Returns what the walk could not establish (see [`PinGaps`]). A broken
    /// pin in an idle project protects nothing either way, so it is not a gap.
    pub(crate) fn add_pins(
        &mut self,
        p: &dyn Platform,
        roots: &[PathBuf],
        opts: &ScanOptions,
        now: SystemTime,
        max_idle: Duration,
    ) -> PinGaps {
        let home = p.home_dir();
        let MarkerScan {
            marked,
            mut unreadable,
            too_deep: _,
        } = find_marker_files(p, roots, PIN_FILES, NOT_ACTIVITY, opts);
        let mut broken = Vec::new();
        for pin in marked {
            let idle = now.duration_since(pin.last_active).unwrap_or_default();
            let active = idle <= max_idle;
            let project = display(&home, &pin.project_root);
            let edited = date(pin.last_active);
            match (read_pin(p, &pin.file), active) {
                (Ok(None), _) => {}
                (Ok(Some(channel)), true) => self.keep(
                    self.resolve(&channel),
                    format!("pinned by {project} (last edited {edited})"),
                ),
                (Ok(Some(channel)), false) => self.stale(
                    self.resolve(&channel),
                    format!("{project} (last edited {edited})"),
                ),
                (Err(e), true) => broken.push(format!("{}: {e}", pin.file.display())),
                (Err(e), false) => {
                    tracing::debug!(pin = %pin.file.display(), error = %e,
                        "rustup: ignoring an unreadable pin in an idle project");
                }
            }
        }
        for refs in self.stale.values_mut() {
            refs.sort();
        }
        unreadable.sort();
        unreadable.dedup();
        PinGaps { broken, unreadable }
    }

    /// The installed toolchain `channel` selects (see [`resolve`]).
    fn resolve(&self, channel: &str) -> String {
        resolve(channel, &self.host, &self.names)
    }

    /// Why `name` is kept, if it is.
    pub(crate) fn reason(&self, name: &str) -> Option<&str> {
        self.kept.get(name).map(String::as_str)
    }

    /// The idle pins and dead overrides that name `name`.
    pub(crate) fn stale_refs(&self, name: &str) -> &[String] {
        self.stale.get(name).map_or(&[], Vec::as_slice)
    }

    /// The first reason recorded wins: "the default toolchain" says more than
    /// "pinned by …".
    fn keep(&mut self, name: String, why: String) {
        self.kept.entry(name).or_insert(why);
    }

    fn stale(&mut self, name: String, what: String) {
        self.stale.entry(name).or_default().push(what);
    }
}

/// Read one pin file: `Ok(None)` when it names no installed toolchain to
/// protect (a `path` toolchain, or only components for the default).
///
/// A symlinked pin is read through the link, since what it points at is what
/// rustup reads; a link that cannot be read is an unreadable pin. Only a
/// regular file can be bounded before reading — the platform has no
/// link-following metadata — so a link's contents are bounded after.
fn read_pin(p: &dyn Platform, file: &Path) -> Result<Option<String>, String> {
    let too_large = |len: u64| format!("{len} bytes is too large for a toolchain file");
    let meta = p.metadata(file).map_err(|e| e.to_string())?;
    match meta.kind {
        FileKind::File if meta.len > MAX_PIN_BYTES => return Err(too_large(meta.len)),
        FileKind::File | FileKind::Symlink => {}
        FileKind::Dir | FileKind::Other => return Err("not a file".into()),
    }
    let text = p.read_to_string(file).map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_PIN_BYTES {
        return Err(too_large(text.len() as u64));
    }
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    parse_pin(name, &text)
}

/// Parse a toolchain file's channel.
///
/// `rust-toolchain.toml`, and a legacy `rust-toolchain` containing a
/// `[toolchain]` table, are TOML: `[toolchain] channel = "…"`. Otherwise a
/// legacy file's first non-empty line is the channel; if that line is not a
/// toolchain name the file is tried as TOML before giving up, since reading a
/// pin too generously only keeps a toolchain longer.
pub(crate) fn parse_pin(file_name: &str, text: &str) -> Result<Option<String>, String> {
    if file_name == "rust-toolchain.toml" || text.contains("[toolchain]") {
        return parse_toml_pin(text);
    }
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or("the toolchain file is empty")?;
    match checked(line) {
        Ok(channel) => Ok(Some(channel)),
        Err(e) => parse_toml_pin(text).map_err(|_| e),
    }
}

fn parse_toml_pin(text: &str) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    struct File {
        toolchain: Section,
    }
    #[derive(Deserialize)]
    struct Section {
        channel: Option<String>,
        path: Option<String>,
    }
    let file: File =
        toml::from_str(text).map_err(|e| format!("not a valid toolchain file: {}", e.message()))?;
    // A `path` toolchain is a build somewhere else, not one rustup installed.
    if file.toolchain.path.is_some() {
        return Ok(None);
    }
    file.toolchain.channel.as_deref().map(checked).transpose()
}

fn checked(channel: &str) -> Result<String, String> {
    if valid_name(channel) {
        Ok(channel.to_owned())
    } else {
        Err(format!("{channel:?} is not a toolchain name"))
    }
}
