//! Parse-only stubs for commands filled in by Stage 2.
//!
//! The clap tree wires these subcommands now so `--help`, argument parsing, and
//! shell completion are complete; the bodies land in Stage 2 (`review`,
//! `status`, `why`, `schedule`). Invoking one today prints a notice and exits
//! cleanly.

use anyhow::Result;

/// Print a "not yet implemented" notice for `name` and exit `0`.
pub fn not_implemented(name: &str) -> Result<u8> {
    println!("`disk-saver {name}` is not implemented yet (coming in Stage 2)");
    Ok(0)
}
