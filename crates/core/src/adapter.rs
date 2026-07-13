//! The [`Adapter`] trait, its per-run [`Ctx`], and the [`AdapterFactory`] used
//! by the CLI's static registry (§4.1).
//!
//! An adapter has zero OS knowledge: it touches the world only through
//! [`Ctx::platform`], persists state via [`Ctx::kv`] (a bucket pre-namespaced to
//! the adapter), and records audit decisions via [`Ctx::decide`].

use std::time::SystemTime;

use crate::decision::{Decision, DecisionLog};
use crate::error::{AdapterError, ConfigError};
use crate::types::{Candidate, Outcome, Pressure};

/// Everything an adapter may touch during one run.
pub struct Ctx<'run> {
    /// The platform abstraction (filesystem, clock, subprocesses, …).
    pub platform: &'run dyn crate::Platform,
    /// A KV bucket pre-namespaced to this adapter.
    pub kv: disk_saver_kv::Bucket<'run>,
    /// The disk-pressure tier for this run.
    pub pressure: Pressure,
    /// The decision log (best-effort; a no-op when disabled).
    pub decisions: &'run DecisionLog,
}

impl<'run> Ctx<'run> {
    /// Assemble a context from its parts.
    pub fn new(
        platform: &'run dyn crate::Platform,
        kv: disk_saver_kv::Bucket<'run>,
        pressure: Pressure,
        decisions: &'run DecisionLog,
    ) -> Self {
        Self {
            platform,
            kv,
            pressure,
            decisions,
        }
    }

    /// The current wall-clock time (from the platform clock).
    pub fn now(&self) -> SystemTime {
        self.platform.now()
    }

    /// Record a decision at the current time and pressure.
    pub fn decide(&self, d: Decision) {
        self.decisions.record(self.now(), self.pressure, &d);
    }
}

/// One per adapter crate. Constructed by a factory from opaque config, then
/// driven through its four phases by the engine.
///
/// Methods take `&mut self` so an adapter can carry in-memory notes from
/// `observe` to `plan` within a run; durable state goes to [`Ctx::kv`].
pub trait Adapter: Send {
    /// Stable machine name. Doubles as the config section `[adapters.<name>]`,
    /// the KV bucket name, and the log target.
    fn name(&self) -> &'static str;

    /// Cheap dependency probe (is the daemon reachable? is the dir readable?).
    /// Used by `disk-saver doctor`. Defaults to `Ok`.
    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        let _ = ctx;
        Ok(())
    }

    /// Record whatever is needed for age tracking. Runs every full cycle,
    /// including when pressure is [`Comfortable`](Pressure::Comfortable).
    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError>;

    /// Return deletion candidates already filtered by the adapter's own policy
    /// for the current pressure. Must not mutate anything user-visible.
    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError>;

    /// Delete a batch of previously-planned candidates. Partial failure is
    /// normal: outcomes are per candidate, not all-or-nothing.
    fn execute(&mut self, ctx: &mut Ctx, batch: &[Candidate])
    -> Result<Vec<Outcome>, AdapterError>;
}

/// A factory the CLI registry uses to build an adapter from its config table.
pub struct AdapterFactory {
    /// The adapter's stable name (matches [`Adapter::name`]).
    pub name: &'static str,
    /// Build the adapter from its `[adapters.<name>]` table (minus `enabled`),
    /// or `None` if the section was absent (→ adapter defaults).
    // The fn-pointer type is pinned verbatim by the frozen API contract.
    #[allow(clippy::type_complexity)]
    pub build: fn(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{DecisionKind, DecisionLog};
    use disk_saver_kv::Store;
    use disk_saver_platform::{FakePlatform, Platform};

    #[test]
    fn ctx_now_reads_platform_clock() {
        let fake = FakePlatform::new();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let ctx = Ctx::new(&fake, store.bucket("x"), Pressure::Normal, &log);
        assert_eq!(ctx.now(), fake.now());
    }

    #[test]
    fn ctx_decide_does_not_panic_when_disabled() {
        let fake = FakePlatform::new();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let ctx = Ctx::new(
            &fake,
            store.bucket("x"),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        ctx.decide(Decision::new(DecisionKind::Observed, "x", "id"));
    }

    #[test]
    fn ctx_kv_is_usable() {
        let fake = FakePlatform::new();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let ctx = Ctx::new(&fake, store.bucket("mybucket"), Pressure::Normal, &log);
        ctx.kv.set("k", &7u64, ctx.now()).unwrap();
        assert_eq!(ctx.kv.get::<u64>("k").unwrap(), Some(7));
        assert_eq!(ctx.kv.name(), "mybucket");
    }

    // A minimal adapter proving the trait's default `check` and object safety.
    struct Noop;
    impl Adapter for Noop {
        fn name(&self) -> &'static str {
            "noop"
        }
        fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
            Ok(())
        }
        fn plan(&mut self, _ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
            Ok(vec![])
        }
        fn execute(
            &mut self,
            _ctx: &mut Ctx,
            _batch: &[Candidate],
        ) -> Result<Vec<Outcome>, AdapterError> {
            Ok(vec![])
        }
    }

    #[test]
    fn adapter_is_object_safe_and_check_defaults_ok() {
        let fake = FakePlatform::new();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut boxed: Box<dyn Adapter> = Box::new(Noop);
        let mut ctx = Ctx::new(&fake, store.bucket("noop"), Pressure::Normal, &log);
        assert_eq!(boxed.name(), "noop");
        assert!(boxed.check(&mut ctx).is_ok());
        assert!(boxed.observe(&mut ctx).is_ok());
        assert!(boxed.plan(&mut ctx).unwrap().is_empty());
    }
}
