//! Command implementations.
//!
//! Each command takes the shared [`crate::app::App`] (or, for `config init`,
//! just the platform + path) and returns the process exit code it wants:
//! `0` clean, `2` when at least one adapter failed. Any returned `Err` is
//! mapped by `main` to exit code `1` (a config or environment error where
//! nothing ran).

pub mod config;
pub mod doctor;
pub mod run;
pub mod state;
pub mod stubs;
