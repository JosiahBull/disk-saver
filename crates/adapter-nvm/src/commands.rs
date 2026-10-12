use std::path::Path;
use std::time::Duration;

use disk_saver_core::{CommandSpec, Platform};

// nvm is a shell function, not an executable. No shell startup files are loaded.
// Paths and specs are positional arguments; none are interpolated into shell code.
pub(super) const QUERY: &str =
    "export NVM_DIR=\"$1\"; . \"$NVM_DIR/nvm.sh\" --no-use || exit; nvm version \"$2\"";
pub(super) const UNINSTALL: &str =
    "export NVM_DIR=\"$1\"; . \"$NVM_DIR/nvm.sh\" --no-use || exit; nvm uninstall \"$2\"";

pub(super) fn command(root: &Path, script: &str, spec: &str) -> CommandSpec {
    CommandSpec::new(
        "bash",
        [
            "--noprofile",
            "--norc",
            "-c",
            script,
            "disk-saver",
            &root.to_string_lossy(),
            spec,
        ],
    )
    .with_timeout(Duration::from_secs(30))
}

pub(super) fn version_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('v') else {
        return false;
    };
    let pieces: Vec<_> = rest.split('.').collect();
    pieces.len() == 3
        && pieces
            .iter()
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

/// No open executable means no known running process uses this installation.
pub(super) fn idle(p: &dyn Platform, executable: &Path) -> bool {
    let command = CommandSpec::new("lsof", ["-t", "--", &executable.to_string_lossy()]);
    let result = p.run_command(&command);

    matches!(
        result,
        Ok(output)
            if output.status == 1
                && output.stdout.is_empty()
                && output.stderr.is_empty()
    )
}
