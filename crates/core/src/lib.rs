//! `disk-saver-core` — the [`Adapter`] trait, the pressure model, the run
//! [`Engine`], configuration, the approvals queue, and the decision log.
//!
//! This crate defines the orchestration that ties independent adapters together.
//! It depends only on the two leaf crates (`disk-saver-kv`, `disk-saver-platform`)
//! and re-exports their public surface from the crate root so downstream adapters
//! and the CLI can `use disk_saver_core::{Platform, Store, Bucket, ...}` without
//! naming the leaf crates directly.
//!
//! See `ARCHITECTURE.md` for behaviour and rationale.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod approvals;
pub mod config;
pub mod configured;
pub mod decision;
pub mod engine;
pub mod error;
pub mod retention;
pub mod throttle;
pub mod types;

// ── core surface (flat re-exports) ──────────────────────────────────────────
pub use adapter::{Adapter, AdapterFactory, ConfigCx, Ctx};
pub use approvals::{ApprovalState, Approvals, QueuedItem};
pub use config::{
    AdapterSection, Config, GlobalConfig, NotificationConfig, NotificationEvent, ScheduleConfig,
    Threshold, expand_tilde,
};
pub use configured::Configured;
pub use decision::{Decision, DecisionKind, DecisionLog};
#[cfg(feature = "decision-log")]
pub use decision::{RecordedDecision, read_decisions};
pub use engine::{
    AdapterReport, AdapterStatus, CandidateReport, Engine, RunOptions, RunReport, classify_pressure,
};
pub use error::{AdapterError, ConfigError, parse_adapter_config};
pub use retention::RetentionPolicy;
pub use throttle::run_due;
pub use types::{Candidate, Class, Outcome, Pressure};

// ── platform re-exports (so adapters use `disk_saver_core::Platform`) ────────
pub use disk_saver_platform::{
    CommandOutput, CommandSpec, DirEntry, DiskUsage, FileKind, FileMeta, Notification, Platform,
    RealPlatform, Urgency,
};

// ── kv re-exports ────────────────────────────────────────────────────────────
pub use disk_saver_kv::{Bucket, Store};
