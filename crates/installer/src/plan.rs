//! The install plan: an ordered list of [`Action`]s the wizard reviews and then
//! executes after the TUI is torn down. Each action can describe itself
//! (`summary`/`detail`), report whether it would overwrite something
//! (`conflict`), and apply itself (`execute`).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A single install step.
pub enum Action {
    /// Write `~/.disk-saver.toml`.
    WriteConfig { path: PathBuf, contents: String },
    /// Copy the `disk-saver` binary onto the user's PATH.
    InstallBinary { from: PathBuf, to: PathBuf },
    /// Run a `disk-saver` subcommand (e.g. `schedule install`).
    Run {
        label: String,
        program: PathBuf,
        args: Vec<String>,
    },
}

/// Whether an action would overwrite existing content (for review warnings).
pub enum Conflict {
    /// Nothing is overwritten.
    None,
    /// A file at `path` already exists and would be overwritten (backed up).
    Overwrite { path: PathBuf },
}

/// An action plus the user's apply/skip decision.
pub struct Decided {
    /// The step.
    pub action: Action,
    /// Whether to run it.
    pub apply: bool,
}

impl Action {
    /// One-line summary for the plan list.
    pub fn summary(&self) -> String {
        match self {
            Action::WriteConfig { path, .. } => format!("Write config {}", home_rel(path)),
            Action::InstallBinary { to, .. } => format!("Install disk-saver to {}", home_rel(to)),
            Action::Run { label, .. } => label.clone(),
        }
    }

    /// Longer detail shown under the selected action.
    pub fn detail(&self) -> String {
        match self {
            Action::WriteConfig { contents, .. } => {
                format!("{} lines of TOML", contents.lines().count())
            }
            Action::InstallBinary { from, to } => {
                format!("cp {} {}", home_rel(from), home_rel(to))
            }
            Action::Run { program, args, .. } => {
                format!("$ {} {}", home_rel(program), args.join(" "))
            }
        }
    }

    /// Whether applying this action overwrites something.
    pub fn conflict(&self) -> Conflict {
        match self {
            Action::WriteConfig { path, .. } if path.exists() => {
                Conflict::Overwrite { path: path.clone() }
            }
            _ => Conflict::None,
        }
    }

    /// Apply the action.
    pub fn execute(&self) -> io::Result<()> {
        match self {
            Action::WriteConfig { path, contents } => {
                // Back up an existing config we're about to change.
                if let Ok(existing) = std::fs::read_to_string(path)
                    && existing != *contents
                {
                    let backup = path.with_extension("toml.bak");
                    let _ = std::fs::copy(path, &backup);
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, contents)
            }
            Action::InstallBinary { from, to } => {
                if from == to {
                    return Ok(());
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(from, to)?;
                make_executable(to)
            }
            Action::Run { program, args, .. } => {
                let status = Command::new(program).args(args).status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "{} exited with {status}",
                        program.display()
                    )))
                }
            }
        }
    }
}

/// Build the install plan from the config text, the located binary, and `home`.
///
/// `bin` is where a `disk-saver` binary was found. It is installed to
/// `~/.local/bin/disk-saver` (unless it already lives there), and that installed
/// path is what runs `schedule install` — so the generated launchd/systemd unit
/// references a stable location.
pub fn build(config: String, bin: &Path, home: &Path) -> Vec<Action> {
    let config_path = home.join(".disk-saver.toml");
    let installed = home.join(".local").join("bin").join("disk-saver");

    let mut actions = vec![Action::WriteConfig {
        path: config_path,
        contents: config,
    }];

    let scheduler_bin = if bin == installed {
        installed.clone()
    } else {
        actions.push(Action::InstallBinary {
            from: bin.to_path_buf(),
            to: installed.clone(),
        });
        installed
    };

    actions.push(Action::Run {
        label: "Install the scheduler unit (disk-saver schedule install)".to_string(),
        program: scheduler_bin,
        args: vec!["schedule".to_string(), "install".to_string()],
    });
    actions
}

/// Find a `disk-saver` binary to install: `$DISK_SAVER_BIN`, then a sibling of
/// this installer, then the usual bin dirs. Returns the first that exists.
pub fn locate_binary(home: &Path) -> Option<PathBuf> {
    if let Some(env) = std::env::var_os("DISK_SAVER_BIN") {
        let p = PathBuf::from(env);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("disk-saver"));
    }
    candidates.push(home.join(".local").join("bin").join("disk-saver"));
    candidates.push(home.join(".cargo").join("bin").join("disk-saver"));
    candidates.push(PathBuf::from("/usr/local/bin/disk-saver"));
    candidates.into_iter().find(|p| p.is_file())
}

/// Render `path` with `~` for the home dir, for tidy display.
pub fn home_rel(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_installs_binary_when_not_already_in_local_bin() {
        let home = PathBuf::from("/home/u");
        let bin = PathBuf::from("/some/target/debug/disk-saver");
        let actions = build("cfg".into(), &bin, &home);
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[0], Action::WriteConfig { .. }));
        assert!(matches!(actions[1], Action::InstallBinary { .. }));
        match &actions[2] {
            Action::Run { program, args, .. } => {
                assert_eq!(program, &home.join(".local/bin/disk-saver"));
                assert_eq!(args, &["schedule", "install"]);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn plan_skips_binary_copy_when_already_installed() {
        let home = PathBuf::from("/home/u");
        let installed = home.join(".local/bin/disk-saver");
        let actions = build("cfg".into(), &installed, &home);
        assert_eq!(actions.len(), 2, "no InstallBinary step");
        assert!(matches!(actions[0], Action::WriteConfig { .. }));
        assert!(matches!(actions[1], Action::Run { .. }));
    }
}
