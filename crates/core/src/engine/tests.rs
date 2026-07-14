//! Engine scenario tests with scripted stub adapters (ARCHITECTURE.md §16
//! "Engine (core)" row): panic/Unavailable isolation, Comfortable ⇒ no
//! plan/execute, scavenge wave ordering + early stop, flagged-candidate routing,
//! approvals GC, the throttle gate, notifications + `min_gap` dedupe, and
//! `RunReport` contents.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_platform::FakePlatform;

use crate::{
    Adapter, AdapterError, AdapterStatus, Approvals, Candidate, Class, Config, Ctx, DecisionLog,
    Engine, Outcome, Pressure, RunOptions, RunReport, Threshold, classify_pressure,
};

// ── test harness ────────────────────────────────────────────────────────────

/// FakePlatform's fixed default clock: `UNIX_EPOCH + 400 days`.
const NOW_SECS: u64 = 400 * 24 * 3600;

fn fake_now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW_SECS)
}

fn cand(id: &str, bytes: u64, class: Class, age_days: u64) -> Candidate {
    let last = fake_now() - Duration::from_secs(age_days * 24 * 3600);
    Candidate::new(id, id, bytes, last, class)
}

type Log = Arc<Mutex<Vec<String>>>;

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn events(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[derive(Clone, Copy)]
enum Behavior {
    Ok,
    UnavailableObserve,
    PanicObserve,
    PanicPlan,
    FailExecute,
}

/// A scripted adapter that logs each phase call to a shared log.
struct Stub {
    name: &'static str,
    log: Log,
    plan: Vec<Candidate>,
    behavior: Behavior,
    /// If set, `execute` raises the shared platform's free space (early-stop test).
    raise_free: Option<(Arc<FakePlatform>, u64, u64)>,
}

impl Stub {
    fn new(name: &'static str, log: Log) -> Self {
        Stub {
            name,
            log,
            plan: Vec::new(),
            behavior: Behavior::Ok,
            raise_free: None,
        }
    }

    fn with_plan(mut self, plan: Vec<Candidate>) -> Self {
        self.plan = plan;
        self
    }

    fn behavior(mut self, b: Behavior) -> Self {
        self.behavior = b;
        self
    }

    fn raise_free(mut self, p: Arc<FakePlatform>, available: u64, total: u64) -> Self {
        self.raise_free = Some((p, available, total));
        self
    }

    fn boxed(self) -> Box<dyn Adapter> {
        Box::new(self)
    }

    fn push(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }
}

impl Adapter for Stub {
    fn name(&self) -> &'static str {
        self.name
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        self.push(format!("obs:{}", self.name));
        match self.behavior {
            Behavior::UnavailableObserve => Err(AdapterError::unavailable("down")),
            Behavior::PanicObserve => panic!("observe boom"),
            _ => Ok(()),
        }
    }

    fn plan(&mut self, _ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        self.push(format!("plan:{}", self.name));
        if let Behavior::PanicPlan = self.behavior {
            panic!("plan boom");
        }
        Ok(self.plan.clone())
    }

    fn execute(
        &mut self,
        _ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        for c in batch {
            self.push(format!("exec:{}:{}", self.name, c.id));
        }
        if let Behavior::FailExecute = self.behavior {
            return Err(AdapterError::Failed(anyhow::anyhow!("execute failed")));
        }
        if let Some((p, available, total)) = &self.raise_free {
            p.set_free_space(*available, *total);
        }
        Ok(batch
            .iter()
            .map(|c| Outcome::Removed {
                id: c.id.clone(),
                bytes: c.bytes,
            })
            .collect())
    }
}

/// Run the engine once against `fake` with a disabled decision log.
fn run(
    fake: &FakePlatform,
    cfg: &Config,
    store: &crate::Store,
    adapters: Vec<Box<dyn Adapter>>,
    opts: &RunOptions,
) -> RunReport {
    let dlog = DecisionLog::disabled();
    let mut engine = Engine::new(fake, cfg, store, &dlog, adapters);
    engine.run(opts)
}

fn find<'r>(report: &'r RunReport, name: &str) -> &'r crate::AdapterReport {
    report.adapters.iter().find(|r| r.name == name).unwrap()
}

fn forced() -> RunOptions {
    RunOptions {
        force: true,
        ..Default::default()
    }
}

// ── pure helpers ────────────────────────────────────────────────────────────

#[test]
fn classify_pressure_boundaries() {
    // Defaults resolve against total=1000 to start=200, scavenge=80, target=150.
    let g = Config::defaults().global;
    let total = 1000;
    assert_eq!(classify_pressure(200, total, &g), Pressure::Comfortable); // == start
    assert_eq!(classify_pressure(500, total, &g), Pressure::Comfortable);
    assert_eq!(classify_pressure(199, total, &g), Pressure::Normal);
    assert_eq!(classify_pressure(80, total, &g), Pressure::Normal); // == scavenge → normal
    assert_eq!(
        classify_pressure(79, total, &g),
        Pressure::Scavenge { need: 150 - 79 }
    );
    assert_eq!(
        classify_pressure(0, total, &g),
        Pressure::Scavenge { need: 150 }
    );
}

#[test]
fn classify_pressure_zero_total_is_comfortable_even_with_absolute_thresholds() {
    // A failed disk measurement surfaces as total == 0. Absolute thresholds
    // ignore `total`, so without the guard classify_pressure(0, 0) would resolve
    // free=0 below scavenge_below and return Scavenge — turning a measurement
    // error into mass deletion. The guard must force Comfortable.
    let mut g = Config::defaults().global;
    g.start_cleaning_below = Threshold::Absolute(20_000_000_000);
    g.warn_below = Threshold::Absolute(12_000_000_000);
    g.scavenge_below = Threshold::Absolute(8_000_000_000);
    g.scavenge_target = Threshold::Absolute(15_000_000_000);
    assert_eq!(classify_pressure(0, 0, &g), Pressure::Comfortable);
    assert_eq!(
        classify_pressure(5_000_000_000, 0, &g),
        Pressure::Comfortable
    );
    // Sanity: with a real total the same thresholds still scavenge when low.
    assert_eq!(
        classify_pressure(1_000_000_000, 500_000_000_000, &g),
        Pressure::Scavenge {
            need: 15_000_000_000 - 1_000_000_000
        }
    );
}

#[test]
fn run_options_default_is_all_off() {
    let o = RunOptions::default();
    assert!(!o.dry_run);
    assert!(!o.force);
    assert!(o.only.is_none());
    assert!(o.pressure_override.is_none());
}

// ── comfortable ⇒ observe only ──────────────────────────────────────────────

#[test]
fn comfortable_observes_but_never_plans_or_executes() {
    let fake = FakePlatform::new().with_free_space(500, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("a", log.clone())
            .with_plan(vec![cand("x", 10, Class::Cache, 100)])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    assert_eq!(events(&log), vec!["obs:a".to_string()]);
    assert_eq!(report.pressure, "comfortable");
    assert!(!report.throttled);
    assert_eq!(report.free_after, 500);
    assert_eq!(find(&report, "a").candidates, 0);
    assert_eq!(find(&report, "a").removed, 0);
    assert_eq!(report.bytes_removed(), 0);
}

// ── normal mode ─────────────────────────────────────────────────────────────

#[test]
fn normal_executes_each_adapters_own_unflagged_candidates() {
    let fake = FakePlatform::new().with_free_space(150, 1000); // Normal, not below warn
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("a", log.clone())
            .with_plan(vec![
                cand("a1", 10, Class::Cache, 100),
                cand("a2", 20, Class::Rebuildable, 100),
            ])
            .boxed(),
        Stub::new("b", log.clone())
            .with_plan(vec![cand("b1", 30, Class::Cache, 100)])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    assert_eq!(report.pressure, "normal");
    assert_eq!(find(&report, "a").candidates, 2);
    assert_eq!(find(&report, "a").candidate_bytes, 30); // 10 + 20, estimated reclaimable
    assert_eq!(find(&report, "a").removed, 2);
    assert_eq!(find(&report, "a").bytes_removed, 30);
    assert_eq!(find(&report, "b").candidate_bytes, 30);
    assert_eq!(find(&report, "b").removed, 1);
    assert_eq!(report.bytes_removed(), 60);
    assert_eq!(report.candidate_bytes(), 60);
    assert!(!report.any_failed());

    // Per-candidate detail is populated (for `plan --detailed`).
    let a_detail = &find(&report, "a").candidates_detail;
    assert_eq!(a_detail.len(), 2);
    let ids: Vec<&str> = a_detail.iter().map(|c| c.id.as_str()).collect();
    assert!(ids.contains(&"a1") && ids.contains(&"a2"));
    let a1 = a_detail.iter().find(|c| c.id == "a1").unwrap();
    assert_eq!(a1.bytes, 10);
    assert_eq!(a1.class, Class::Cache);
    assert_eq!(a1.age_secs, 100 * 24 * 3600);

    let ev = events(&log);
    assert!(ev.contains(&"exec:a:a1".to_string()));
    assert!(ev.contains(&"exec:a:a2".to_string()));
    assert!(ev.contains(&"exec:b:b1".to_string()));
}

// ── isolation ───────────────────────────────────────────────────────────────

#[test]
fn panic_in_observe_becomes_failed_and_isolates_the_adapter() {
    let fake = FakePlatform::new().with_free_space(150, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("bad", log.clone())
            .behavior(Behavior::PanicObserve)
            .boxed(),
        Stub::new("good", log.clone())
            .with_plan(vec![cand("g1", 10, Class::Cache, 100)])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    assert!(matches!(
        find(&report, "bad").status,
        AdapterStatus::Failed(_)
    ));
    assert!(matches!(find(&report, "good").status, AdapterStatus::Ok));
    assert_eq!(find(&report, "good").removed, 1);
    assert!(report.any_failed());

    let ev = events(&log);
    assert!(ev.contains(&"obs:bad".to_string()));
    assert!(!ev.iter().any(|e| e.starts_with("plan:bad")));
    assert!(!ev.iter().any(|e| e.starts_with("exec:bad")));
    assert!(ev.contains(&"exec:good:g1".to_string()));
}

#[test]
fn panic_in_plan_becomes_failed_and_isolates_the_adapter() {
    let fake = FakePlatform::new().with_free_space(150, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("bad", log.clone())
            .behavior(Behavior::PanicPlan)
            .with_plan(vec![cand("b1", 10, Class::Cache, 100)])
            .boxed(),
        Stub::new("good", log.clone())
            .with_plan(vec![cand("g1", 10, Class::Cache, 100)])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    assert!(matches!(
        find(&report, "bad").status,
        AdapterStatus::Failed(_)
    ));
    assert!(matches!(find(&report, "good").status, AdapterStatus::Ok));
    assert_eq!(find(&report, "good").removed, 1);
    let ev = events(&log);
    assert!(!ev.iter().any(|e| e.starts_with("exec:bad")));
}

#[test]
fn unavailable_in_observe_skips_adapter_without_failing_run() {
    let fake = FakePlatform::new().with_free_space(150, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("down", log.clone())
            .behavior(Behavior::UnavailableObserve)
            .with_plan(vec![cand("d1", 10, Class::Cache, 100)])
            .boxed(),
        Stub::new("up", log.clone())
            .with_plan(vec![cand("u1", 10, Class::Cache, 100)])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    assert!(matches!(
        find(&report, "down").status,
        AdapterStatus::Unavailable(_)
    ));
    assert_eq!(find(&report, "down").candidates, 0);
    assert_eq!(find(&report, "up").removed, 1);
    assert!(!report.any_failed()); // Unavailable is not a failure

    let ev = events(&log);
    assert!(!ev.iter().any(|e| e.starts_with("plan:down")));
    assert!(!ev.iter().any(|e| e.starts_with("exec:down")));
}

#[test]
fn failed_execute_marks_adapter_failed_but_siblings_proceed() {
    let fake = FakePlatform::new().with_free_space(150, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("x", log.clone())
            .behavior(Behavior::FailExecute)
            .with_plan(vec![cand("x1", 10, Class::Cache, 100)])
            .boxed(),
        Stub::new("y", log.clone())
            .with_plan(vec![cand("y1", 10, Class::Cache, 100)])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    assert!(matches!(
        find(&report, "x").status,
        AdapterStatus::Failed(_)
    ));
    assert_eq!(find(&report, "x").removed, 0);
    assert_eq!(find(&report, "y").removed, 1);
    assert!(report.any_failed());
}

// ── scavenge waves ──────────────────────────────────────────────────────────

#[test]
fn scavenge_processes_impact_waves_oldest_first() {
    // Scavenge (free 50 < scavenge 80), target 150 never reached (nothing frees
    // real space), so every wave runs.
    let fake = FakePlatform::new().with_free_space(50, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("multi", log.clone())
            .with_plan(vec![
                cand("c1", 10, Class::Cache, 50),
                cand("u1", 10, Class::UserData, 50),
                cand("r_new", 10, Class::Rebuildable, 10),
                cand("r_old", 10, Class::Rebuildable, 90),
            ])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    let execs: Vec<String> = events(&log)
        .into_iter()
        .filter(|e| e.starts_with("exec:"))
        .collect();
    assert_eq!(
        execs,
        vec![
            "exec:multi:r_old".to_string(), // Rebuildable wave, oldest first
            "exec:multi:r_new".to_string(),
            "exec:multi:c1".to_string(), // then Cache
            "exec:multi:u1".to_string(), // then UserData
        ]
    );
    assert_eq!(find(&report, "multi").removed, 4);
}

#[test]
fn scavenge_stops_early_when_target_reached() {
    // The rebuildable adapter raises real free space to 200 (>= target 150) on
    // execute; the cache/user waves must never run.
    let fake = Arc::new(FakePlatform::new().with_free_space(50, 1000));
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let adapters = vec![
        Stub::new("rebuild", log.clone())
            .with_plan(vec![cand("r1", 10, Class::Rebuildable, 50)])
            .raise_free(fake.clone(), 200, 1000)
            .boxed(),
        Stub::new("cache", log.clone())
            .with_plan(vec![cand("c1", 10, Class::Cache, 50)])
            .boxed(),
        Stub::new("user", log.clone())
            .with_plan(vec![cand("u1", 10, Class::UserData, 50)])
            .boxed(),
    ];
    let report = {
        let dlog = DecisionLog::disabled();
        let mut engine = Engine::new(&*fake, &cfg, &store, &dlog, adapters);
        engine.run(&RunOptions::default())
    };

    let execs: Vec<String> = events(&log)
        .into_iter()
        .filter(|e| e.starts_with("exec:"))
        .collect();
    assert_eq!(execs, vec!["exec:rebuild:r1".to_string()]);

    // All three adapters were planned before execution began.
    let plans = events(&log)
        .into_iter()
        .filter(|e| e.starts_with("plan:"))
        .count();
    assert_eq!(plans, 3);

    assert_eq!(report.free_after, 200);
    assert_eq!(find(&report, "cache").removed, 0);
    assert_eq!(find(&report, "user").removed, 0);
    assert_eq!(find(&report, "rebuild").removed, 1);
}

// ── confirmation queue ──────────────────────────────────────────────────────

#[test]
fn flagged_candidates_are_queued_and_never_executed_even_under_scavenge() {
    let fake = FakePlatform::new().with_free_space(50, 1000); // Scavenge
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let flagged = cand("secret", 100, Class::UserData, 100).confirm(true);
    let adapters = vec![
        Stub::new("trash", log.clone())
            .with_plan(vec![flagged])
            .boxed(),
        Stub::new("build", log.clone())
            .with_plan(vec![cand("junk", 10, Class::Rebuildable, 100)])
            .boxed(),
    ];
    let report = run(&fake, &cfg, &store, adapters, &RunOptions::default());

    let ev = events(&log);
    assert!(!ev.iter().any(|e| e.contains("exec:trash:secret")));
    assert!(ev.contains(&"exec:build:junk".to_string()));

    // The flagged item lands in the approvals queue.
    let pending = Approvals::open(&store).pending(fake_now());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].candidate.id, "secret");
    assert_eq!(pending[0].adapter, "trash");

    assert_eq!(find(&report, "trash").queued, 1);
    assert_eq!(report.queued_items, 1);

    // The approvals-pending notification fired.
    assert!(
        fake.notifications()
            .iter()
            .any(|n| n.title.contains("approvals pending"))
    );
}

#[test]
fn comfortable_run_garbage_collects_pending_approvals() {
    let fake = FakePlatform::new().with_free_space(50, 1000); // Scavenge first
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    // Queue a flagged item under scavenge.
    let flagged = cand("secret", 100, Class::UserData, 100).confirm(true);
    run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("trash", log.clone())
                .with_plan(vec![flagged])
                .boxed(),
        ],
        &forced(),
    );
    assert_eq!(Approvals::open(&store).pending(fake_now()).len(), 1);

    // Now comfortable: the item is no longer planned, so it is GC'd.
    fake.set_free_space(500, 1000);
    run(
        &fake,
        &cfg,
        &store,
        vec![Stub::new("trash", log.clone()).boxed()],
        &forced(),
    );
    assert_eq!(Approvals::open(&store).pending(fake_now()).len(), 0);
}

// ── throttle gate ───────────────────────────────────────────────────────────

#[test]
fn throttle_gate_blocks_second_run_until_interval_elapses() {
    let fake = FakePlatform::new().with_free_space(500, 1000); // Comfortable, not below warn
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults(); // run_every 12h

    let r1 = run(&fake, &cfg, &store, vec![], &RunOptions::default());
    assert!(!r1.throttled);

    let r2 = run(&fake, &cfg, &store, vec![], &RunOptions::default());
    assert!(r2.throttled);
    assert!(r2.adapters.is_empty());

    // --force bypasses the gate.
    let r3 = run(&fake, &cfg, &store, vec![], &forced());
    assert!(!r3.throttled);

    // After run_every elapses, a normal run is due again.
    fake.advance(Duration::from_secs(13 * 3600));
    let r4 = run(&fake, &cfg, &store, vec![], &RunOptions::default());
    assert!(!r4.throttled);
}

#[test]
fn throttle_uses_faster_cadence_below_warn() {
    let fake = FakePlatform::new().with_free_space(100, 1000); // below warn (120), Normal
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults(); // run_every 12h, pressure_run_every 1h

    let r1 = run(&fake, &cfg, &store, vec![], &RunOptions::default());
    assert!(!r1.throttled);

    // 90 minutes later: due under the 1h pressure cadence (would NOT be under 12h).
    fake.advance(Duration::from_secs(90 * 60));
    let r2 = run(&fake, &cfg, &store, vec![], &RunOptions::default());
    assert!(!r2.throttled);
}

// ── notifications ───────────────────────────────────────────────────────────

#[test]
fn scavenge_warning_fires_on_transition_then_dedupes_within_min_gap() {
    let fake = FakePlatform::new().with_free_space(100, 1000); // below warn, Normal
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults(); // min_gap 12h

    let warns = |f: &FakePlatform| {
        f.notifications()
            .iter()
            .filter(|n| n.title.contains("low disk space"))
            .count()
    };

    // First run crosses below warn → notify.
    run(&fake, &cfg, &store, vec![], &RunOptions::default());
    assert_eq!(warns(&fake), 1);

    // Second run soon (force past the throttle): not a transition, within
    // min_gap → suppressed.
    run(&fake, &cfg, &store, vec![], &forced());
    assert_eq!(warns(&fake), 1);

    // After min_gap elapses → fires again.
    fake.advance(Duration::from_secs(13 * 3600));
    run(&fake, &cfg, &store, vec![], &forced());
    assert_eq!(warns(&fake), 2);
}

#[test]
fn disabled_notifications_are_silent() {
    let fake = FakePlatform::new().with_free_space(50, 1000); // Scavenge
    let store = crate::Store::open_in_memory().unwrap();
    let mut cfg = Config::defaults();
    cfg.notifications.enabled = false;
    let log = new_log();
    run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("a", log)
                .with_plan(vec![cand("a1", 10, Class::Cache, 100)])
                .boxed(),
        ],
        &RunOptions::default(),
    );
    assert!(fake.notifications().is_empty());
}

#[test]
fn scavenge_ran_notification_fires_under_scavenge() {
    let fake = FakePlatform::new().with_free_space(50, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("a", log)
                .with_plan(vec![cand("a1", 10, Class::Rebuildable, 100)])
                .boxed(),
        ],
        &RunOptions::default(),
    );
    assert!(
        fake.notifications()
            .iter()
            .any(|n| n.title.contains("scavenge ran"))
    );
}

#[test]
fn adapter_failing_notification_after_three_consecutive_failures() {
    let fake = FakePlatform::new().with_free_space(500, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();

    for i in 0..3 {
        let dlog = DecisionLog::disabled();
        let mut engine = Engine::new(
            &fake,
            &cfg,
            &store,
            &dlog,
            vec![
                Stub::new("flaky", log.clone())
                    .behavior(Behavior::PanicObserve)
                    .boxed(),
            ],
        );
        engine.run(&forced());
        let fired = fake
            .notifications()
            .iter()
            .any(|n| n.title.contains("adapter failing"));
        // Fires only once the streak reaches 3.
        assert_eq!(fired, i >= 2, "run {i}");
    }
}

#[test]
fn failstreak_resets_after_a_successful_run() {
    let fake = FakePlatform::new().with_free_space(500, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();

    // Two failing runs, then a healthy one, then two failing again → still no
    // notification (the streak was reset by the healthy run).
    let behaviors = [
        Behavior::PanicObserve,
        Behavior::PanicObserve,
        Behavior::Ok,
        Behavior::PanicObserve,
        Behavior::PanicObserve,
    ];
    for b in behaviors {
        let dlog = DecisionLog::disabled();
        let mut engine = Engine::new(
            &fake,
            &cfg,
            &store,
            &dlog,
            vec![Stub::new("flaky", log.clone()).behavior(b).boxed()],
        );
        engine.run(&forced());
    }
    assert!(
        !fake
            .notifications()
            .iter()
            .any(|n| n.title.contains("adapter failing"))
    );
}

// ── dry run ─────────────────────────────────────────────────────────────────

#[test]
fn dry_run_plans_but_does_not_execute_route_or_persist() {
    let fake = FakePlatform::new().with_free_space(50, 1000); // Scavenge
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let flagged = cand("secret", 100, Class::UserData, 100).confirm(true);
    let report = run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("trash", log.clone())
                .with_plan(vec![flagged])
                .boxed(),
            Stub::new("build", log.clone())
                .with_plan(vec![cand("junk", 10, Class::Rebuildable, 100)])
                .boxed(),
        ],
        &RunOptions {
            dry_run: true,
            ..Default::default()
        },
    );

    assert!(report.dry_run);
    assert_eq!(find(&report, "build").candidates, 1);
    assert_eq!(find(&report, "build").removed, 0);

    let ev = events(&log);
    assert!(ev.contains(&"plan:build".to_string()));
    assert!(!ev.iter().any(|e| e.starts_with("exec:")));

    // No approvals mutation and no notifications.
    assert!(Approvals::open(&store).list().is_empty());
    assert!(fake.notifications().is_empty());

    // last_full_run was not written → a subsequent real run is not throttled.
    let r2 = run(&fake, &cfg, &store, vec![], &RunOptions::default());
    assert!(!r2.throttled);
}

// ── run report / persistence / filters ──────────────────────────────────────

#[test]
fn run_report_fields_are_populated() {
    let fake = FakePlatform::new().with_free_space(150, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let report = run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("a", log)
                .with_plan(vec![cand("a1", 40, Class::Cache, 100)])
                .boxed(),
        ],
        &RunOptions::default(),
    );

    assert_eq!(report.at_unix, NOW_SECS);
    assert_eq!(report.pressure, "normal");
    assert!(!report.throttled);
    assert!(!report.dry_run);
    assert_eq!(report.total, 1000);
    assert_eq!(report.free_before, 150);
    assert_eq!(report.adapters.len(), 1);
    assert_eq!(report.adapters[0].name, "a");
    assert_eq!(report.adapters[0].candidates, 1);
    assert_eq!(report.adapters[0].removed, 1);
    assert_eq!(report.bytes_removed(), 40);
    assert_eq!(report.queued_items, 0);
}

#[test]
fn reports_persist_to_the_engine_ring_buffer() {
    let fake = FakePlatform::new().with_free_space(500, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    run(&fake, &cfg, &store, vec![], &forced());

    let seq: Option<u64> = store.bucket("_engine").get("report_seq").unwrap();
    assert_eq!(seq, Some(1));
    let r0: Option<RunReport> = store.bucket("_engine").get("report:0").unwrap();
    assert!(r0.is_some());
    assert_eq!(r0.unwrap().pressure, "comfortable");
}

#[test]
fn only_filter_restricts_the_adapter_set() {
    let fake = FakePlatform::new().with_free_space(150, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let report = run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("a", log.clone())
                .with_plan(vec![cand("a1", 10, Class::Cache, 100)])
                .boxed(),
            Stub::new("b", log.clone())
                .with_plan(vec![cand("b1", 10, Class::Cache, 100)])
                .boxed(),
        ],
        &RunOptions {
            only: Some(vec!["b".to_string()]),
            ..Default::default()
        },
    );

    assert_eq!(report.adapters.len(), 1);
    assert_eq!(report.adapters[0].name, "b");
    let ev = events(&log);
    assert!(!ev.iter().any(|e| e.ends_with(":a")));
    assert!(ev.contains(&"exec:b:b1".to_string()));
}

#[test]
fn pressure_override_forces_the_tier() {
    // Physically comfortable (would delete nothing), but overridden to Normal →
    // the adapter's candidates are executed unconditionally.
    let fake = FakePlatform::new().with_free_space(500, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let report = run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("a", log.clone())
                .with_plan(vec![cand("a1", 10, Class::Rebuildable, 100)])
                .boxed(),
        ],
        &RunOptions {
            pressure_override: Some(Pressure::Normal),
            ..Default::default()
        },
    );
    assert_eq!(report.pressure, "normal");
    assert_eq!(find(&report, "a").removed, 1);
    assert!(events(&log).contains(&"exec:a:a1".to_string()));
}

#[test]
fn scavenge_override_respects_real_free_space_target() {
    // Overriding to Scavenge when the disk already has more free space than the
    // target correctly deletes nothing (the wave stops immediately).
    let fake = FakePlatform::new().with_free_space(500, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults(); // target 15% = 150 < 500
    let log = new_log();
    let report = run(
        &fake,
        &cfg,
        &store,
        vec![
            Stub::new("a", log.clone())
                .with_plan(vec![cand("a1", 10, Class::Rebuildable, 100)])
                .boxed(),
        ],
        &RunOptions {
            pressure_override: Some(Pressure::Scavenge { need: 999 }),
            ..Default::default()
        },
    );
    assert_eq!(report.pressure, "scavenge");
    assert_eq!(find(&report, "a").removed, 0);
    assert!(!events(&log).iter().any(|e| e.starts_with("exec:")));
}

// ── execute_approved ──────────────────────────────────────────────────────────

#[test]
fn execute_approved_runs_the_named_adapter() {
    let fake = FakePlatform::new().with_free_space(150, 1000);
    let store = crate::Store::open_in_memory().unwrap();
    let cfg = Config::defaults();
    let log = new_log();
    let dlog = DecisionLog::disabled();
    let mut engine = Engine::new(
        &fake,
        &cfg,
        &store,
        &dlog,
        vec![Stub::new("trash", log.clone()).boxed()],
    );

    let batch = vec![cand("photo.png", 1234, Class::UserData, 100)];
    let outcomes = engine.execute_approved("trash", &batch).unwrap();
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        Outcome::Removed { id, bytes } => {
            assert_eq!(id, "photo.png");
            assert_eq!(*bytes, 1234);
        }
        other => panic!("expected Removed, got {other:?}"),
    }
    assert!(events(&log).contains(&"exec:trash:photo.png".to_string()));

    // Unknown adapter → error.
    assert!(engine.execute_approved("nope", &batch).is_err());
}
