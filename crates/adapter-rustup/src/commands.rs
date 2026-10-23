//! Everything that runs rustup or another program. rustup is a real executable,
//! so it is invoked directly with positional arguments: no shell, no PATH.

use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_saver_core::{AdapterError, CommandSpec, FileKind, Platform, expand_tilde};

use crate::toolchain::{valid_name, valid_triple};

/// `rustup show` reads local state only; anything slower is wedged.
const SHOW_TIMEOUT: Duration = Duration::from_secs(30);
/// Removing a toolchain deletes tens of thousands of files.
const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Locate rustup without consulting `PATH` (see [`Config::rustup`](crate::Config::rustup)).
///
/// A configured path is authoritative: if it does not exist the adapter is
/// unavailable rather than quietly using some other rustup, which could manage
/// a different set of toolchains.
pub(crate) fn rustup_binary(
    p: &dyn Platform,
    configured: Option<&Path>,
) -> Result<PathBuf, AdapterError> {
    let home = p.home_dir();
    if let Some(path) = configured {
        let path = expand_tilde(path, &home);
        return if runnable(p, &path) {
            Ok(path)
        } else {
            Err(AdapterError::unavailable(format!(
                "configured rustup {} does not exist",
                path.display()
            )))
        };
    }
    p.env_var("CARGO_HOME")
        .map(|cargo| PathBuf::from(cargo).join("bin/rustup"))
        .into_iter()
        .chain([home.join(".cargo/bin/rustup")])
        .find(|path| runnable(p, path))
        .ok_or_else(|| {
            AdapterError::unavailable(
                "rustup not found in $CARGO_HOME/bin or ~/.cargo/bin; set `rustup` to its path",
            )
        })
}

/// An absolute path to a file, or to a symlink (Homebrew installs rustup as a
/// link into its Cellar). Running it follows the link; nothing here deletes it.
fn runnable(p: &dyn Platform, path: &Path) -> bool {
    path.is_absolute()
        && matches!(p.metadata(path), Ok(m) if matches!(m.kind, FileKind::File | FileKind::Symlink))
}

/// rustup's own home: `$RUSTUP_HOME`, else `~/.rustup`. Toolchains live in its
/// `toolchains` directory and its settings in `settings.toml`.
pub(crate) fn rustup_home(p: &dyn Platform) -> PathBuf {
    p.env_var("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| p.home_dir().join(".rustup"))
}

/// What `rustup show` says about this machine.
#[derive(Debug)]
pub(crate) struct Show {
    /// `Default host:` — the triple native toolchains are built for.
    pub host: String,
    /// `rustup home:` — compared with the home this adapter measures, so that
    /// `toolchain uninstall` acts on the directory that was inspected.
    pub home: Option<PathBuf>,
    /// Toolchains listed with a `default` flag, e.g.
    /// `nightly-aarch64-apple-darwin (active, default)` or, from older rustup,
    /// `stable-x86_64-unknown-linux-gnu (default)`.
    pub defaults: Vec<String>,
}

/// Run `rustup show`. It reports the host triple without running `rustc`, and
/// from rustup 1.28 it never installs the active toolchain as a side effect.
pub(crate) fn show(p: &dyn Platform, rustup: &Path) -> Result<Show, AdapterError> {
    let spec = CommandSpec::new(rustup.to_string_lossy(), ["show"]).with_timeout(SHOW_TIMEOUT);
    let out = p
        .run_command(&spec)
        .map_err(|e| AdapterError::unavailable(format!("cannot run `rustup show`: {e}")))?;
    if !out.success() {
        return Err(AdapterError::unavailable(format!(
            "`rustup show` exited {}: {}",
            out.status,
            out.stderr_string().trim()
        )));
    }
    parse_show(&out.stdout_string()).map_err(AdapterError::unavailable)
}

/// Parse `rustup show` output; see [`Show`] for the lines that matter.
pub(crate) fn parse_show(text: &str) -> Result<Show, String> {
    let mut host = None;
    let mut home = None;
    let mut defaults = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Default host:") {
            host = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("rustup home:") {
            home = Some(PathBuf::from(rest.trim()));
        } else if let Some((name, flags)) = line.split_once(" (")
            && let Some(flags) = flags.strip_suffix(')')
            && valid_name(name)
            && flags.split(',').any(|flag| flag.trim() == "default")
        {
            defaults.push(name.to_owned());
        }
    }
    let host = host
        .filter(|h| valid_triple(h))
        .ok_or("`rustup show` did not report a usable `Default host`")?;
    Ok(Show {
        host,
        home,
        defaults,
    })
}

/// `rustup toolchain uninstall <name>`: rustup's own removal, which also drops
/// the toolchain's update hash so nothing is left half-registered.
pub(crate) fn uninstall(rustup: &Path, name: &str) -> CommandSpec {
    CommandSpec::new(rustup.to_string_lossy(), ["toolchain", "uninstall", name])
        .with_timeout(UNINSTALL_TIMEOUT)
}

/// No running process has any of `files` open (an executing binary counts as
/// open). A missing `lsof`, an error, or any output means "maybe in use".
pub(crate) fn idle(p: &dyn Platform, files: &[PathBuf]) -> bool {
    let mut args = vec!["-t".to_owned(), "--".to_owned()];
    args.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
    matches!(
        p.run_command(&CommandSpec::new("lsof", args)),
        Ok(out) if out.status == 1 && out.stdout.is_empty() && out.stderr.is_empty()
    )
}
