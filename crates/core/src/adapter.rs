//! The [`Adapter`] trait, its per-run [`Ctx`], the config-time [`ConfigCx`], and
//! the [`AdapterFactory`] used by the CLI's static registry (§4.1).
//!
//! An adapter has zero OS knowledge: it touches the world only through
//! [`Ctx::platform`], persists state via [`Ctx::kv`] (a bucket pre-namespaced to
//! the adapter), and records audit decisions via [`Ctx::decide`].
//!
//! It has zero TOML knowledge either: [`AdapterFactory::typed`] deserializes the
//! `[adapters.<name>]` table into a type the adapter names, so a constructor
//! receives a config struct rather than a [`toml::Value`] it has to remember to
//! parse. What it needs at that point beyond the config — the adapter's own name,
//! for error attribution, and the handful of checks every adapter was writing out
//! by hand — comes from [`ConfigCx`].

use std::fmt::Display;
use std::time::{Duration, SystemTime};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::decision::{Decision, DecisionLog};
use crate::error::{AdapterError, ConfigError, parse_adapter_config};
use crate::retention::RetentionPolicy;
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

/// The type-erased constructor inside an [`AdapterFactory`].
///
/// A boxed closure rather than a bare `fn` pointer so that a factory can
/// *capture* — which is what lets [`disk_saver_cachedir::factory`] hand back a
/// ready-made factory for a name and a resolver, instead of each cache-directory
/// adapter crate writing the same constructor out again.
///
/// [`disk_saver_cachedir::factory`]: https://docs.rs/disk-saver-cachedir
type BuildFn =
    Box<dyn Fn(Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> + Send + Sync>;

/// A factory the CLI registry uses to build an adapter from its config table.
///
/// Built with [`AdapterFactory::typed`]: an adapter names the type its config
/// deserializes into and never handles the raw table itself.
pub struct AdapterFactory {
    /// The adapter's stable name (matches [`Adapter::name`]).
    pub name: &'static str,
    make: BuildFn,
}

impl AdapterFactory {
    /// A factory whose adapter is built from an already-parsed typed config.
    ///
    /// `make` receives `C` — deserialized from the `[adapters.<name>]` table, or
    /// `C::default()` when the section was absent — together with a [`ConfigCx`]
    /// carrying the adapter's name. Deserialization failures are reported as
    /// [`ConfigError::Adapter`] naming this adapter, so no adapter repeats that
    /// plumbing and none can forget it.
    ///
    /// Pass a named `fn` rather than a closure where there is a body worth
    /// naming: the fn item pins both type parameters, so nothing needs a type
    /// annotation and a mismatch is reported against the constructor rather than
    /// against this function's `Fn` bound.
    pub fn typed<C, A, F>(name: &'static str, make: F) -> Self
    where
        C: serde::de::DeserializeOwned + Default,
        A: Adapter + 'static,
        F: Fn(C, &ConfigCx) -> Result<A, ConfigError> + Send + Sync + 'static,
    {
        AdapterFactory {
            name,
            make: Box::new(move |raw| {
                let config: C = parse_adapter_config(name, raw)?;
                Ok(Box::new(make(config, &ConfigCx::new(name))?) as Box<dyn Adapter>)
            }),
        }
    }

    /// Build the adapter from its `[adapters.<name>]` table (minus `enabled`),
    /// or `None` if the section was absent (→ adapter defaults).
    pub fn build(&self, raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
        (self.make)(raw)
    }
}

/// What an adapter's constructor gets alongside its typed config.
///
/// It carries the adapter's name, so that no call site repeats it when building
/// an error, and the two checks nearly every adapter was writing out by hand:
/// validating a [`RetentionPolicy`] and compiling a list of globs.
#[derive(Debug, Clone, Copy)]
pub struct ConfigCx {
    name: &'static str,
}

impl ConfigCx {
    /// A context for the adapter called `name`.
    pub fn new(name: &'static str) -> Self {
        ConfigCx { name }
    }

    /// The adapter's stable name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// A [`ConfigError::Adapter`] attributed to this adapter.
    pub fn err(&self, message: impl Into<String>) -> ConfigError {
        ConfigError::Adapter {
            adapter: self.name.to_owned(),
            message: message.into(),
        }
    }

    /// Attribute a foreign error to this adapter, prefixed with `what`.
    pub fn wrap<T, E: Display>(&self, result: Result<T, E>, what: &str) -> Result<T, ConfigError> {
        result.map_err(|e| self.err(format!("{what}: {e}")))
    }

    /// A [`RetentionPolicy`] with its `min_age <= max_age` invariant checked,
    /// constructed exactly once.
    pub fn retention(
        &self,
        max_age: Duration,
        min_age: Duration,
    ) -> Result<RetentionPolicy, ConfigError> {
        let policy = RetentionPolicy::new(max_age, min_age);
        policy.validate().map_err(|message| self.err(message))?;
        Ok(policy)
    }

    /// Compile the globs from the config key `what` into one [`GlobSet`],
    /// naming the offending pattern on failure.
    pub fn globs(&self, what: &str, patterns: &[String]) -> Result<GlobSet, ConfigError> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern)
                .map_err(|e| self.err(format!("invalid {what} glob '{pattern}': {e}")))?;
            builder.add(glob);
        }
        self.wrap(builder.build(), &format!("compiling {what} globs"))
    }
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

    #[derive(Debug, Default, serde::Deserialize)]
    #[serde(default)]
    struct NoopConfig {
        greeting: String,
    }

    fn build_noop(cfg: NoopConfig, cx: &ConfigCx) -> Result<Noop, ConfigError> {
        assert_eq!(cx.name(), "noop");
        let _ = cfg;
        Ok(Noop)
    }

    fn noop_factory() -> AdapterFactory {
        AdapterFactory::typed("noop", build_noop)
    }

    #[test]
    fn typed_uses_config_defaults_for_an_absent_section() {
        let built = noop_factory().build(None).unwrap();
        assert_eq!(built.name(), "noop");
    }

    #[test]
    fn typed_reports_a_bad_config_against_the_adapter() {
        // `greeting` is a String; a table is a type error.
        let raw: toml::Value = toml::from_str("greeting = { a = 1 }").unwrap();
        match noop_factory().build(Some(raw)) {
            Err(ConfigError::Adapter { adapter, .. }) => assert_eq!(adapter, "noop"),
            Err(other) => panic!("expected Adapter error, got {other:?}"),
            Ok(_) => panic!("expected a config error"),
        }
    }

    #[test]
    fn config_cx_err_names_the_adapter() {
        let e = ConfigCx::new("docker").err("bad thing");
        assert_eq!(e.to_string(), "adapter 'docker': bad thing");
    }

    #[test]
    fn config_cx_wrap_prefixes_a_foreign_error() {
        let cx = ConfigCx::new("docker");
        let r: Result<(), std::fmt::Error> = Err(std::fmt::Error);
        let e = cx.wrap(r, "reading widget").unwrap_err();
        assert!(
            e.to_string()
                .starts_with("adapter 'docker': reading widget: ")
        );
    }

    #[test]
    fn config_cx_retention_validates_once() {
        let cx = ConfigCx::new("docker");
        let day = std::time::Duration::from_secs(86_400);
        let policy = cx.retention(day * 30, day * 7).unwrap();
        assert_eq!(policy.max_age, day * 30);
        assert_eq!(policy.min_age, day * 7);

        let err = cx.retention(day * 7, day * 30).unwrap_err();
        assert!(err.to_string().starts_with("adapter 'docker': min_age "));
    }

    #[test]
    fn config_cx_globs_names_the_bad_pattern() {
        let cx = ConfigCx::new("docker");
        let ok = cx.globs("protect", &["postgres:*".to_string()]).unwrap();
        assert!(ok.is_match("postgres:16"));
        assert!(!ok.is_match("redis:7"));

        let err = cx
            .globs("protect", &["[unterminated".to_string()])
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "adapter 'docker': invalid protect glob '[unterminated': \
             error parsing glob '[unterminated': unclosed character class; \
             missing ']'"
        );
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
