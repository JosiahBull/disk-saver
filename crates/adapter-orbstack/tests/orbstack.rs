//! Integration tests for the orbstack adapter, driven against [`FakePlatform`]
//! with scripted `orbctl`, `docker` and `du` output (no real OrbStack).

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_orbstack::factory;
use disk_saver_core::{
    Adapter, AdapterError, Candidate, Class, CommandOutput, CommandSpec, Ctx, DecisionKind,
    DecisionLog, DirEntry, DiskUsage, FileMeta, Notification, Outcome, Platform, Pressure,
    RecordedDecision, Store, read_decisions,
};
use disk_saver_platform::FakePlatform;

const DAY: u64 = 24 * 60 * 60;

/// The standard data image under the fake home (`/home/tester`).
const IMAGE: &str =
    "/home/tester/Library/Group Containers/HUAQ24HBR6.dev.orbstack/data/data.img.raw";

/// `du -k` of the stuck image: 39,550,782 KiB = 40,499,000,768 bytes ("40.5 GB").
const STUCK_KIB: u64 = 39_550_782;
/// `du -k` once the gap has been reclaimed: 9,523,200,000 bytes, ~23 MB over
/// what Docker accounts for.
const RECLAIMED_KIB: u64 = 9_300_000;
/// `du -k` of a freshly reset image.
const FRESH_KIB: u64 = 1_000_000;

/// Docker accounts for 9.5 GB in all: four images and some build cache.
const DF: &str = concat!(
    r#"{"Active":"1","Reclaimable":"8GB (86%)","Size":"9.2GB","TotalCount":"4","Type":"Images"}"#,
    "\n",
    r#"{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Containers"}"#,
    "\n",
    r#"{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Local Volumes"}"#,
    "\n",
    r#"{"Active":"0","Reclaimable":"300MB","Size":"300MB","TotalCount":"12","Type":"Build Cache"}"#,
    "\n",
);

const RESET: &[&str] = &["reset", "-y"];
const START: &[&str] = &["start"];

// ── a FakePlatform whose commands can answer differently each time ──────────

/// A command, as `(program, args)`.
type Cmd = (String, Vec<String>);

/// [`FakePlatform`] plus two things it cannot script: a command whose output
/// changes from one call to the next (`du` before and after a reset), and a
/// command that removes a file (a reset deleting the data image).
struct Scripted {
    fake: FakePlatform,
    /// Outputs per exact command, consumed in order; the last one repeats.
    sequences: Mutex<Vec<(Cmd, VecDeque<CommandOutput>)>>,
    /// Running this command removes this path from the fake filesystem.
    removes: Vec<(Cmd, PathBuf)>,
}

fn is_cmd(spec: &CommandSpec, program: &str, args: &[String]) -> bool {
    spec.program == program && spec.args == args
}

impl Platform for Scripted {
    fn now(&self) -> SystemTime {
        self.fake.now()
    }
    fn home_dir(&self) -> PathBuf {
        self.fake.home_dir()
    }
    fn env_var(&self, name: &str) -> Option<String> {
        self.fake.env_var(name)
    }
    fn trash_dirs(&self) -> Vec<PathBuf> {
        self.fake.trash_dirs()
    }
    fn user_cache_dir(&self) -> PathBuf {
        self.fake.user_cache_dir()
    }
    fn metadata(&self, path: &Path) -> io::Result<FileMeta> {
        self.fake.metadata(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.fake.read_dir(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.fake.read_to_string(path)
    }
    fn dir_size(&self, path: &Path) -> io::Result<u64> {
        self.fake.dir_size(path)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.fake.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.fake.remove_dir_all(path)
    }
    fn disk_usage(&self, path: &Path) -> io::Result<DiskUsage> {
        self.fake.disk_usage(path)
    }
    fn notify(&self, n: &Notification) -> io::Result<()> {
        self.fake.notify(n)
    }
    fn run_command(&self, cmd: &CommandSpec) -> io::Result<CommandOutput> {
        // Always go through the fake first so it records the call.
        let canned = self.fake.run_command(cmd);
        for ((program, args), path) in &self.removes {
            if is_cmd(cmd, program, args) {
                let _ = self.fake.remove_file(path);
            }
        }
        let mut sequences = self.sequences.lock().unwrap();
        if let Some((_, outputs)) = sequences
            .iter_mut()
            .find(|((program, args), _)| is_cmd(cmd, program, args))
        {
            let next = if outputs.len() > 1 {
                outputs.pop_front()
            } else {
                outputs.front().cloned()
            };
            return Ok(next.expect("a sequence has at least one output"));
        }
        canned
    }
}

impl Scripted {
    /// Every command run so far.
    fn commands(&self) -> Vec<CommandSpec> {
        self.fake.commands_run()
    }

    /// Whether `program args` was ever run.
    fn ran(&self, program: &str, args: &[&str]) -> bool {
        self.commands()
            .iter()
            .any(|c| c.program == program && c.args == args)
    }

    fn advance(&self, by: Duration) {
        self.fake.advance(by);
    }
}

// ── the world the adapter sees ─────────────────────────────────────────────

fn ok(stdout: &str) -> CommandOutput {
    CommandOutput::ok(stdout)
}

fn out(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
    CommandOutput {
        status,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// One machine's state, healthy by default: OrbStack running, a 40.5 GB image
/// of which Docker accounts for 9.5 GB, and nothing a reset could lose.
#[derive(Clone)]
struct World {
    status: CommandOutput,
    image: &'static str,
    image_exists: bool,
    /// Successive `du -k` readings in KiB; the last repeats.
    du_kib: Vec<u64>,
    df: CommandOutput,
    machines: CommandOutput,
    k8s: CommandOutput,
    containers: CommandOutput,
    volumes: CommandOutput,
    reset: CommandOutput,
    start: CommandOutput,
    reset_removes_image: bool,
}

impl Default for World {
    fn default() -> Self {
        World {
            status: ok("Running\n"),
            image: IMAGE,
            image_exists: true,
            du_kib: vec![STUCK_KIB],
            df: ok(DF),
            machines: ok(""),
            k8s: ok("false\n"),
            containers: ok(""),
            volumes: ok(""),
            reset: ok(""),
            start: ok("Docker engine is ready to use.\n"),
            reset_removes_image: false,
        }
    }
}

/// The default clock: well past the epoch, so seeded timestamps stay positive.
fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(400 * DAY)
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn docker(args: &[&'static str]) -> Vec<&'static str> {
    let mut all = vec!["--context", "orbstack"];
    all.extend_from_slice(args);
    all
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

impl World {
    fn platform(&self, now: SystemTime) -> Scripted {
        let mut fake = FakePlatform::new()
            .with_now(now)
            .with_command("orbctl", &["status"], self.status.clone())
            .with_command("orbctl", &["list", "-q"], self.machines.clone())
            .with_command("orbctl", &["config", "get", "k8s.enable"], self.k8s.clone())
            .with_command("orbctl", RESET, self.reset.clone())
            .with_command("orbctl", START, self.start.clone())
            .with_command(
                "docker",
                &docker(&["system", "df", "--format", "{{json .}}"]),
                self.df.clone(),
            )
            .with_command("docker", &docker(&["ps", "-aq"]), self.containers.clone())
            .with_command(
                "docker",
                &docker(&["volume", "ls", "-q"]),
                self.volumes.clone(),
            );
        if self.image_exists {
            fake = fake.with_file(self.image, "", now);
        }
        let du = self
            .du_kib
            .iter()
            .map(|kib| ok(&format!("{kib}\t{}\n", self.image)))
            .collect();
        let removes = if self.reset_removes_image {
            vec![(
                ("orbctl".to_string(), strings(RESET)),
                PathBuf::from(self.image),
            )]
        } else {
            Vec::new()
        };
        Scripted {
            fake,
            sequences: Mutex::new(vec![(("du".to_string(), strings(&["-k", self.image])), du)]),
            removes,
        }
    }
}

// ── adapter, store and decision-log plumbing ──────────────────────────────

fn adapter() -> Box<dyn Adapter> {
    factory().build(None).unwrap()
}

fn adapter_from(toml: &str) -> Box<dyn Adapter> {
    let raw: toml::Value = toml::from_str(toml).unwrap();
    factory().build(Some(raw)).unwrap()
}

/// The error a config is rejected with.
fn config_error(toml: &str) -> String {
    let raw: toml::Value = toml::from_str(toml).unwrap();
    match factory().build(Some(raw)) {
        Ok(_) => panic!("expected {toml:?} to be rejected"),
        Err(e) => e.to_string(),
    }
}

/// A real decision log in a temp dir, so tests can read back what `why` shows.
struct Log {
    _dir: tempfile::TempDir,
    path: PathBuf,
    log: DecisionLog,
}

impl Log {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.db");
        let log = DecisionLog::open(&path).unwrap();
        Log {
            _dir: dir,
            path,
            log,
        }
    }

    /// The most recent decision of `kind`.
    fn last(&self, kind: DecisionKind) -> RecordedDecision {
        read_decisions(&self.path)
            .unwrap()
            .into_iter()
            .rfind(|d| d.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} decision recorded"))
    }

    fn reason(&self, kind: DecisionKind) -> String {
        self.last(kind).reason.unwrap_or_default()
    }
}

fn ctx<'a>(p: &'a dyn Platform, store: &'a Store, pressure: Pressure, log: &'a Log) -> Ctx<'a> {
    Ctx::new(p, store.bucket("orbstack"), pressure, &log.log)
}

/// Observe then plan, as a scheduled run does.
fn cycle(
    adapter: &mut dyn Adapter,
    p: &dyn Platform,
    store: &Store,
    pressure: Pressure,
    log: &Log,
) -> Vec<Candidate> {
    let mut ctx = ctx(p, store, pressure, log);
    adapter.observe(&mut ctx).unwrap();
    adapter.plan(&mut ctx).unwrap()
}

fn gap_since(store: &Store) -> Option<u64> {
    store.bucket("orbstack").get("gap_since").unwrap()
}

/// A store in which the gap was first seen `age` before `now`.
fn store_with_gap(now: SystemTime, age: Duration) -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .bucket("orbstack")
        .set("gap_since", &unix(now - age), now)
        .unwrap();
    store
}

/// The candidate a healthy world proposes once the gap is four days old.
fn planned_reset() -> Candidate {
    let p = World::default().platform(t0());
    let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
    let log = Log::new();
    let mut plan = cycle(&mut *adapter(), &p, &store, Pressure::Normal, &log);
    assert_eq!(plan.len(), 1);
    plan.pop().unwrap()
}

/// Execute `batch` with a fresh adapter that never observed — exactly what
/// `disk-saver review` does with an approved item.
fn execute(p: &dyn Platform, store: &Store, log: &Log, batch: &[Candidate]) -> Vec<Outcome> {
    let mut ctx = ctx(p, store, Pressure::Normal, log);
    adapter().execute(&mut ctx, batch).unwrap()
}

// ── availability ─────────────────────────────────────────────────────────

#[test]
fn missing_orbctl_is_unavailable() {
    let fake = FakePlatform::new().with_command_error("orbctl", io::ErrorKind::NotFound);
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut adapter = adapter();
    let mut ctx = ctx(&fake, &store, Pressure::Normal, &log);

    let err = adapter.observe(&mut ctx).unwrap_err();
    assert!(matches!(err, AdapterError::Unavailable(_)), "got {err:?}");
    assert!(matches!(
        adapter.check(&mut ctx).unwrap_err(),
        AdapterError::Unavailable(_)
    ));
    // Nothing observed, so nothing to plan.
    assert!(adapter.plan(&mut ctx).unwrap().is_empty());
}

#[test]
fn stopped_or_starting_orbstack_is_unavailable_and_never_started() {
    for (status, code) in [("Stopped", 1), ("Starting", 2)] {
        let world = World {
            status: out(code, &format!("{status}\n"), ""),
            ..World::default()
        };
        let p = world.platform(t0());
        let store = store_with_gap(t0(), Duration::from_secs(30 * DAY));
        let log = Log::new();
        let mut adapter = adapter();
        let mut ctx = ctx(&p, &store, Pressure::Scavenge { need: 1 }, &log);

        match adapter.observe(&mut ctx).unwrap_err() {
            AdapterError::Unavailable(m) => {
                assert!(m.contains("OrbStack is not running"), "{m}");
                assert!(m.contains(status), "{m}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert!(adapter.plan(&mut ctx).unwrap().is_empty());
        // `orbctl status` is the only thing it asked; it never starts OrbStack.
        let ran: Vec<_> = p.commands().into_iter().map(|c| c.args).collect();
        assert_eq!(ran, vec![strings(&["status"])], "{status}");
        // An unavailable run leaves the persistence clock alone.
        assert_eq!(gap_since(&store), Some(unix(t0()) - 30 * DAY));
    }
}

#[test]
fn missing_data_image_is_unavailable() {
    let world = World {
        image_exists: false,
        ..World::default()
    };
    let p = world.platform(t0());
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut ctx = ctx(&p, &store, Pressure::Normal, &log);

    match adapter().observe(&mut ctx).unwrap_err() {
        AdapterError::Unavailable(m) => assert!(m.contains("data image not found"), "{m}"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(!p.ran("du", &["-k", IMAGE]));
}

#[test]
fn configured_data_image_expands_tilde() {
    let world = World {
        image: "/home/tester/vm/data.img.raw",
        ..World::default()
    };
    let p = world.platform(t0());
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut adapter = adapter_from(r#"data_image = "~/vm/data.img.raw""#);
    let mut ctx = ctx(&p, &store, Pressure::Normal, &log);

    adapter.observe(&mut ctx).unwrap();
    assert!(p.ran("du", &["-k", "/home/tester/vm/data.img.raw"]));
    assert_eq!(gap_since(&store), Some(unix(t0())));
}

#[test]
fn unparseable_docker_accounting_is_unavailable_not_zero() {
    // Reading Docker's figure as 0 would make the whole image look like waste.
    let world = World {
        df: ok("WARNING: something new\n"),
        ..World::default()
    };
    let p = world.platform(t0());
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut ctx = ctx(&p, &store, Pressure::Normal, &log);

    let err = adapter().observe(&mut ctx).unwrap_err();
    assert!(matches!(err, AdapterError::Unavailable(_)), "got {err:?}");
    assert_eq!(gap_since(&store), None);
}

// ── gap persistence ─────────────────────────────────────────────────────────

#[test]
fn gap_since_is_set_kept_and_cleared() {
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut adapter = adapter();
    let observe = |adapter: &mut dyn Adapter, world: World, at: SystemTime| {
        let p = world.platform(at);
        adapter
            .observe(&mut ctx(&p, &store, Pressure::Comfortable, &log))
            .unwrap();
    };
    let reclaimed = World {
        du_kib: vec![RECLAIMED_KIB],
        ..World::default()
    };

    // First sighting of a 31 GB gap starts the clock…
    observe(&mut *adapter, World::default(), t0());
    assert_eq!(gap_since(&store), Some(unix(t0())));
    assert!(log.reason(DecisionKind::Observed).contains("gap 31.0 GB"));

    // …a later sighting keeps it…
    let day1 = t0() + Duration::from_secs(DAY);
    observe(&mut *adapter, World::default(), day1);
    assert_eq!(gap_since(&store), Some(unix(t0())));

    // …the gap closing clears it…
    let day2 = t0() + Duration::from_secs(2 * DAY);
    observe(&mut *adapter, reclaimed, day2);
    assert_eq!(gap_since(&store), None);

    // …and a gap that comes back starts again from zero.
    let day3 = t0() + Duration::from_secs(3 * DAY);
    observe(&mut *adapter, World::default(), day3);
    assert_eq!(gap_since(&store), Some(unix(day3)));
}

#[test]
fn a_gap_below_min_gap_never_starts_the_clock() {
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    // 31 GB of gap, but the user only cares about 40 GB or more.
    let mut adapter = adapter_from(r#"min_gap = "40GB""#);
    let p = World::default().platform(t0());

    let plan = cycle(&mut *adapter, &p, &store, Pressure::Normal, &log);
    assert!(plan.is_empty());
    assert_eq!(gap_since(&store), None);
    assert!(log.reason(DecisionKind::Kept).contains("min_gap 40.0 GB"));
}

// ── retention policy per pressure ─────────────────────────────────────────

#[test]
fn comfortable_plans_nothing_and_checks_nothing() {
    let p = World::default().platform(t0());
    let store = store_with_gap(t0(), Duration::from_secs(30 * DAY));
    let log = Log::new();

    let plan = cycle(&mut *adapter(), &p, &store, Pressure::Comfortable, &log);
    assert!(plan.is_empty());
    assert!(!p.ran("docker", &docker(&["ps", "-aq"])));
    assert_eq!(
        log.reason(DecisionKind::Kept),
        "disk pressure is comfortable"
    );
}

#[test]
fn normal_pressure_waits_for_max_age() {
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut adapter = adapter();
    let p = World::default().platform(t0());

    // Gap first seen now.
    assert!(cycle(&mut *adapter, &p, &store, Pressure::Normal, &log).is_empty());

    // Just short of three days: still kept, and the reason says why.
    p.advance(Duration::from_secs(3 * DAY - 60));
    assert!(cycle(&mut *adapter, &p, &store, Pressure::Normal, &log).is_empty());
    let reason = log.reason(DecisionKind::Kept);
    assert!(reason.contains("gap seen for 2 days"), "{reason}");
    assert!(reason.contains("3days (max_age)"), "{reason}");

    // Three days: proposed.
    p.advance(Duration::from_secs(60));
    let plan = cycle(&mut *adapter, &p, &store, Pressure::Normal, &log);
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].last_used, t0());
}

#[test]
fn scavenge_waits_for_min_age_only() {
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut adapter = adapter();
    let p = World::default().platform(t0());
    let scavenge = Pressure::Scavenge {
        need: 50_000_000_000,
    };

    assert!(cycle(&mut *adapter, &p, &store, scavenge, &log).is_empty());

    p.advance(Duration::from_secs(DAY - 60));
    assert!(cycle(&mut *adapter, &p, &store, scavenge, &log).is_empty());
    assert!(log.reason(DecisionKind::Kept).contains("1day (min_age)"));

    // One day: eligible under scavenge, but not yet under normal pressure.
    p.advance(Duration::from_secs(60));
    assert_eq!(cycle(&mut *adapter, &p, &store, scavenge, &log).len(), 1);
    assert!(cycle(&mut *adapter, &p, &store, Pressure::Normal, &log).is_empty());
}

#[test]
fn retention_is_configurable_and_validated() {
    let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
    let log = Log::new();
    let p = World::default().platform(t0());
    let mut adapter = adapter_from(r#"max_age = "7d""#);
    assert!(cycle(&mut *adapter, &p, &store, Pressure::Normal, &log).is_empty());

    let err = config_error("max_age = \"1d\"\nmin_age = \"2d\"");
    assert!(err.contains("min_age"), "{err}");
}

// ── guards ───────────────────────────────────────────────────────────────

#[test]
fn each_guard_blocks_the_reset_with_its_reason() {
    let cases: Vec<(World, &str)> = vec![
        (
            World {
                machines: ok("ubuntu\n"),
                ..World::default()
            },
            "1 Linux machine (ubuntu)",
        ),
        (
            World {
                containers: ok("3f2a1b\n9c8d7e\n"),
                ..World::default()
            },
            "2 containers",
        ),
        (
            World {
                volumes: ok("pgdata\n"),
                ..World::default()
            },
            "1 volume (data nothing can rebuild)",
        ),
        (
            World {
                k8s: ok("true\n"),
                ..World::default()
            },
            "Kubernetes is enabled",
        ),
        // A guard that cannot be checked blocks too.
        (
            World {
                machines: out(1, "", "permission denied"),
                ..World::default()
            },
            "cannot list Linux machines",
        ),
        (
            World {
                volumes: out(1, "", "daemon gone"),
                ..World::default()
            },
            "cannot list volumes",
        ),
    ];
    for (world, expected) in cases {
        let p = world.platform(t0());
        let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
        let log = Log::new();

        let plan = cycle(&mut *adapter(), &p, &store, Pressure::Normal, &log);
        assert!(plan.is_empty(), "{expected}: planned {plan:?}");
        let reason = log.reason(DecisionKind::Kept);
        assert!(reason.starts_with("reset blocked: "), "{reason}");
        assert!(reason.contains(expected), "{expected}: {reason}");
        // The clock keeps running; only the guards stand in the way.
        assert!(gap_since(&store).is_some());
    }
}

#[test]
fn every_blocker_is_named_at_once() {
    let world = World {
        containers: ok("abc\n"),
        volumes: ok("v1\nv2\n"),
        ..World::default()
    };
    let p = world.platform(t0());
    let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
    let log = Log::new();

    assert!(cycle(&mut *adapter(), &p, &store, Pressure::Normal, &log).is_empty());
    assert_eq!(
        log.reason(DecisionKind::Kept),
        "reset blocked: 1 container; 2 volumes (data nothing can rebuild)"
    );
}

// ── the candidate ────────────────────────────────────────────────────────

#[test]
fn candidate_is_one_confirmed_reset_with_a_full_label() {
    let p = World::default().platform(t0());
    let store = store_with_gap(t0(), Duration::from_secs(3 * DAY));
    let log = Log::new();

    let plan = cycle(&mut *adapter(), &p, &store, Pressure::Normal, &log);
    assert_eq!(plan.len(), 1);
    let c = &plan[0];
    assert_eq!(c.id, "orbstack-reset");
    assert_eq!(
        c.label,
        "reset OrbStack: Docker accounts for 9.5 GB of the 40.5 GB its disk image holds \
         (gap seen for 3 days); removes 4 images (re-pullable); no containers, volumes or \
         machines"
    );
    assert_eq!(c.bytes, STUCK_KIB * 1024);
    assert_eq!(c.last_used, t0() - Duration::from_secs(3 * DAY));
    assert_eq!(c.class, Class::Cache);
    assert!(c.requires_confirmation, "a reset is never automatic");

    let planned = log.last(DecisionKind::Planned);
    assert_eq!(planned.id, "orbstack-reset");
    assert_eq!(planned.label, c.label);

    // Planning is read-only: nothing reset, restarted or deleted.
    assert!(!p.ran("orbctl", RESET));
    assert!(!p.ran("orbctl", START));
    assert!(p.fake.removed().is_empty());
    // Every docker call went to OrbStack's engine, whatever the current context.
    for cmd in p.commands().iter().filter(|c| c.program == "docker") {
        assert_eq!(cmd.args[..2], strings(&["--context", "orbstack"])[..]);
    }
}

#[test]
fn docker_context_is_configurable() {
    let world = World::default();
    let p = world.platform(t0());
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();
    let mut adapter = adapter_from(r#"docker_context = "elsewhere""#);
    let mut ctx = ctx(&p, &store, Pressure::Normal, &log);

    // The world only answers on the `orbstack` context, so this cannot measure.
    assert!(matches!(
        adapter.observe(&mut ctx).unwrap_err(),
        AdapterError::Unavailable(_)
    ));
    assert!(p.ran(
        "docker",
        &[
            "--context",
            "elsewhere",
            "system",
            "df",
            "--format",
            "{{json .}}"
        ]
    ));
}

#[test]
fn confirmation_cannot_be_turned_off() {
    let err = config_error("confirm = false");
    assert!(err.contains("`confirm` cannot be false"), "{err}");
    // Saying yes is redundant but harmless.
    let _ = adapter_from("confirm = true");
}

#[test]
fn bad_config_is_rejected() {
    assert!(config_error(r#"min_gap = "lots""#).contains("invalid size"));
    assert!(config_error(r#"min_gap = "0B""#).contains("min_gap"));
    assert!(config_error(r#"data_image = "relative/data.img.raw""#).contains("data_image"));
    assert!(config_error(r#"protect = ["x"]"#).contains("unknown field"));
}

// ── execute ──────────────────────────────────────────────────────────────

#[test]
fn execute_resets_restarts_and_reports_what_the_image_shrank_by() {
    let candidate = planned_reset();
    let world = World {
        // Re-measured before the reset, then once more after.
        du_kib: vec![STUCK_KIB, FRESH_KIB],
        ..World::default()
    };
    let p = world.platform(t0());
    let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
    let log = Log::new();

    let outcomes = execute(&p, &store, &log, std::slice::from_ref(&candidate));
    match &outcomes[..] {
        [Outcome::Removed { id, bytes }] => {
            assert_eq!(id, "orbstack-reset");
            assert_eq!(*bytes, (STUCK_KIB - FRESH_KIB) * 1024);
        }
        other => panic!("expected one removal, got {other:?}"),
    }

    // Reset, then start, each with a timeout long enough for the job.
    let cmds = p.commands();
    let pos = |args: &[&str]| {
        cmds.iter()
            .position(|c| c.program == "orbctl" && c.args == args)
            .unwrap_or_else(|| panic!("`orbctl {args:?}` never ran"))
    };
    assert!(pos(RESET) < pos(START));
    assert_eq!(cmds[pos(RESET)].timeout, Some(Duration::from_secs(300)));
    assert_eq!(cmds[pos(START)].timeout, Some(Duration::from_secs(120)));
    // The guards were re-checked before the reset.
    assert!(pos(&["list", "-q"]) < pos(RESET));

    assert_eq!(gap_since(&store), None, "the gap went with the reset");
    let deleted = log.last(DecisionKind::Deleted);
    assert_eq!(deleted.bytes, (STUCK_KIB - FRESH_KIB) * 1024);
    assert_eq!(deleted.reason, None);
}

#[test]
fn an_unflagged_reset_is_never_run() {
    let mut candidate = planned_reset();
    candidate.requires_confirmation = false;
    let p = World::default().platform(t0());
    let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
    let log = Log::new();

    let outcomes = execute(&p, &store, &log, &[candidate]);
    assert!(
        matches!(&outcomes[..], [Outcome::Skipped { reason, .. }] if reason.contains("approval")),
        "got {outcomes:?}"
    );
    assert!(
        !p.commands().iter().any(|c| c.args == RESET),
        "no reset may run"
    );
}

#[test]
fn an_image_gone_after_the_reset_counts_as_fully_reclaimed() {
    let candidate = planned_reset();
    let world = World {
        reset_removes_image: true,
        ..World::default()
    };
    let p = world.platform(t0());
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();

    let outcomes = execute(&p, &store, &log, &[candidate]);
    assert!(
        matches!(&outcomes[..], [Outcome::Removed { bytes, .. }] if *bytes == STUCK_KIB * 1024),
        "got {outcomes:?}"
    );
}

#[test]
fn execute_rechecks_everything_and_skips_on_any_change() {
    let candidate = planned_reset();
    let cases: Vec<(World, &str)> = vec![
        (
            World {
                status: out(1, "Stopped\n", ""),
                ..World::default()
            },
            "OrbStack is not running",
        ),
        (
            World {
                image_exists: false,
                ..World::default()
            },
            "data image not found",
        ),
        (
            World {
                du_kib: vec![RECLAIMED_KIB],
                ..World::default()
            },
            "the gap has closed",
        ),
        (
            World {
                machines: ok("ubuntu\n"),
                ..World::default()
            },
            "1 Linux machine (ubuntu)",
        ),
        (
            World {
                containers: ok("abc\n"),
                ..World::default()
            },
            "1 container",
        ),
        (
            World {
                volumes: ok("pgdata\n"),
                ..World::default()
            },
            "1 volume",
        ),
        (
            World {
                k8s: ok("true\n"),
                ..World::default()
            },
            "Kubernetes is enabled",
        ),
    ];
    for (world, expected) in cases {
        let p = world.platform(t0());
        let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
        let log = Log::new();

        let outcomes = execute(&p, &store, &log, std::slice::from_ref(&candidate));
        match &outcomes[..] {
            [Outcome::Skipped { id, reason }] => {
                assert_eq!(id, "orbstack-reset");
                assert!(reason.contains(expected), "{expected}: {reason}");
            }
            other => panic!("{expected}: expected a skip, got {other:?}"),
        }
        assert!(!p.ran("orbctl", RESET), "{expected}: reset anyway");
        assert!(!p.ran("orbctl", START), "{expected}: started OrbStack");
        assert!(log.reason(DecisionKind::Skipped).contains(expected));
        assert!(gap_since(&store).is_some());
    }
}

#[test]
fn a_failed_reset_is_failed_and_still_restarts_orbstack() {
    let candidate = planned_reset();
    let world = World {
        reset: out(1, "", "reset: VM busy"),
        ..World::default()
    };
    let p = world.platform(t0());
    let store = store_with_gap(t0(), Duration::from_secs(4 * DAY));
    let log = Log::new();

    let outcomes = execute(&p, &store, &log, &[candidate]);
    match &outcomes[..] {
        [Outcome::Failed { id, error }] => {
            assert_eq!(id, "orbstack-reset");
            assert!(error.contains("reset: VM busy"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(p.ran("orbctl", START), "OrbStack must be left running");
    assert!(gap_since(&store).is_some(), "the gap is still there");
    assert!(log.reason(DecisionKind::Failed).contains("reset: VM busy"));
}

#[test]
fn a_reset_that_cannot_restart_is_still_a_removal_but_says_so() {
    let candidate = planned_reset();
    let world = World {
        start: out(1, "", "VM failed to boot"),
        reset_removes_image: true,
        ..World::default()
    };
    let p = world.platform(t0());
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();

    let outcomes = execute(&p, &store, &log, &[candidate]);
    assert!(
        matches!(&outcomes[..], [Outcome::Removed { bytes, .. }] if *bytes == STUCK_KIB * 1024),
        "got {outcomes:?}"
    );
    let reason = log.reason(DecisionKind::Deleted);
    assert!(reason.contains("run `orbctl start`"), "{reason}");
    assert!(reason.contains("VM failed to boot"), "{reason}");
}

#[test]
fn an_unknown_candidate_is_skipped() {
    let mut candidate = planned_reset();
    candidate.id = "something-else".to_string();
    let p = World::default().platform(t0());
    let store = Store::open_in_memory().unwrap();
    let log = Log::new();

    let outcomes = execute(&p, &store, &log, &[candidate]);
    assert!(matches!(&outcomes[..], [Outcome::Skipped { .. }]));
    assert!(p.commands().is_empty());
}
