//! Toolchain names, the installed inventory, and the filesystem evidence of use.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use disk_saver_cachedir::safety::{activity, real_path};
use disk_saver_core::{AdapterError, FileKind, Platform};

/// The characters rustup toolchain names and channels are made of. Rejecting
/// anything else keeps a pin's contents from reaching rustup's command line as
/// something other than a name, and a leading `-` from reading as a flag.
pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `arch-vendor-os[-env]`, or the two-part forms such as `wasm32-wasi`.
pub(crate) fn valid_triple(s: &str) -> bool {
    valid_name(s) && s.split('-').count() >= 2 && s.split('-').all(|c| !c.is_empty())
}

/// The target triple of an installed toolchain's directory name, which rustup
/// always writes in full: `<channel>[-<YYYY-MM-DD>]-<triple>`, where the
/// channel is `stable`, `beta`, `nightly`, or a version such as `1.88`,
/// `1.88.0` or `1.90.0-beta.3`. `None` for anything else — a custom toolchain
/// name, or a directory rustup did not make — which is never proposed.
pub(crate) fn target_triple(name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    let parts: Vec<&str> = name.split('-').collect();
    let mut rest = match parts.as_slice() {
        [channel, rest @ ..] if matches!(*channel, "stable" | "beta" | "nightly") => rest,
        [version, pre, rest @ ..] if is_version(version) && pre.starts_with("beta") => rest,
        [version, rest @ ..] if is_version(version) => rest,
        _ => return None,
    };
    if let [year, month, day, tail @ ..] = rest
        && digits(year, 4)
        && digits(month, 2)
        && digits(day, 2)
    {
        rest = tail;
    }
    let triple = rest.join("-");
    valid_triple(&triple).then_some(triple)
}

/// `1.88` or `1.88.0`.
fn is_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn digits(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit())
}

/// The installed toolchain a channel names, resolved the way rustup resolves a
/// pin: an installed toolchain of exactly that name, else the channel completed
/// with the host triple (`1.88.0` → `1.88.0-aarch64-apple-darwin`). Note `1.88`
/// and `1.88.0` are different toolchains to rustup, and so they are here.
pub(crate) fn resolve(channel: &str, host: &str, names: &BTreeSet<String>) -> String {
    if names.contains(channel) {
        channel.to_owned()
    } else {
        format!("{channel}-{host}")
    }
}

/// One toolchain this adapter could propose.
#[derive(Debug, Clone)]
pub(crate) struct Toolchain {
    /// The directory name, which is also rustup's name for it.
    pub name: String,
    /// `<rustup home>/toolchains/<name>`; the candidate id.
    pub path: PathBuf,
    /// The triple it was built for.
    pub triple: String,
}

/// What `<rustup home>/toolchains` holds.
pub(crate) struct Installed {
    /// Every entry's name, so that a pin naming a linked toolchain exactly
    /// resolves to it rather than to `<name>-<host>`.
    pub names: BTreeSet<String>,
    /// The real directories with rustup-shaped names. A symlink is a toolchain
    /// made with `rustup toolchain link`, pointing at someone's own build: it
    /// is never offered.
    pub toolchains: Vec<Toolchain>,
}

pub(crate) fn installed(p: &dyn Platform, dir: &Path) -> Result<Installed, AdapterError> {
    let entries = p
        .read_dir(dir)
        .map_err(|e| AdapterError::unavailable(format!("cannot list {}: {e}", dir.display())))?;
    let mut names = BTreeSet::new();
    let mut toolchains = Vec::new();
    for e in entries {
        names.insert(e.file_name.clone());
        if e.kind != FileKind::Dir {
            continue;
        }
        if let Some(triple) = target_triple(&e.file_name) {
            toolchains.push(Toolchain {
                name: e.file_name,
                path: e.path,
                triple,
            });
        }
    }
    toolchains.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Installed { names, toolchains })
}

/// The cheap half of the evidence: a toolchain's `bin` directory.
#[derive(Debug)]
pub(crate) struct Bin {
    /// Newest access or modification time of any file in `bin`. rustup's
    /// proxies in `~/.cargo/bin` exec these, which records an access where the
    /// filesystem keeps access times. Filesystem evidence, not execution
    /// history: anything that reads the file counts as a use.
    pub newest: SystemTime,
    /// Those files, sorted, for the open-file check.
    pub files: Vec<PathBuf>,
}

/// Read the `bin` evidence. A toolchain without a real `bin/rustc` is not a
/// toolchain this adapter recognises, and a file whose access time the
/// filesystem does not record leaves its use unknown: both refuse.
pub(crate) fn bin_activity(p: &dyn Platform, toolchain: &Path) -> Result<Bin, String> {
    if !real_path(p, toolchain) || !is(p, toolchain, FileKind::Dir) {
        return Err("the toolchain is not a real directory".into());
    }
    let bin = toolchain.join("bin");
    if !is(p, &bin, FileKind::Dir) {
        return Err("its bin directory is missing or not a real directory".into());
    }
    if !is(p, &bin.join("rustc"), FileKind::File) {
        return Err("bin/rustc is missing or not a regular file".into());
    }
    let entries = p
        .read_dir(&bin)
        .map_err(|e| format!("cannot list {}: {e}", bin.display()))?;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut files = Vec::new();
    for e in entries {
        if e.kind != FileKind::File {
            continue;
        }
        let m = p
            .metadata(&e.path)
            .map_err(|err| format!("cannot inspect {}: {err}", e.path.display()))?;
        let accessed = m.accessed.ok_or_else(|| {
            format!(
                "the filesystem records no access time for {}, so its use is unknown",
                e.path.display()
            )
        })?;
        newest = newest.max(accessed).max(m.modified);
        files.push(e.path);
    }
    files.sort();
    Ok(Bin { newest, files })
}

/// The full evidence for a toolchain that the `bin` evidence already shows idle.
#[derive(Debug)]
pub(crate) struct Evidence {
    /// The newer of the `bin` evidence and the newest file access/modification
    /// anywhere in the tree (`rustc` reads the standard library's rlibs; adding
    /// a component writes files). Only ever later than the `bin` evidence, so
    /// it can only keep a toolchain longer.
    pub last: SystemTime,
    /// Apparent size of the toolchain directory.
    pub bytes: u64,
}

/// Walk the whole toolchain. Kept separate from [`bin_activity`] because a
/// toolchain is tens of thousands of files, which is only worth reading for one
/// that is otherwise about to be offered. An unreadable tree, a nested mount or
/// an oversized walk refuses.
pub(crate) fn evidence(p: &dyn Platform, toolchain: &Path, bin: &Bin) -> Result<Evidence, String> {
    let tree = activity(p, toolchain).ok_or(
        "the toolchain tree could not be fully checked (unreadable, too large, or crossing a mount)",
    )?;
    let bytes = p
        .dir_size(toolchain)
        .map_err(|e| format!("cannot measure the toolchain: {e}"))?;
    Ok(Evidence {
        last: tree.max(bin.newest),
        bytes,
    })
}

fn is(p: &dyn Platform, path: &Path, kind: FileKind) -> bool {
    matches!(p.metadata(path), Ok(m) if m.kind == kind)
}
