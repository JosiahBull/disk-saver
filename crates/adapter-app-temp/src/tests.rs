//! Tests for the `app-temp` adapter, driven through [`factory`] against
//! [`FakePlatform`] with a fixed clock.
//!
//! `FakePlatform` cannot refuse access or fail a removal, so [`World`] wraps it
//! to add both — plus a count of filesystem calls, to prove a comfortable run
//! makes none.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use disk_saver_core::{
    Adapter, AdapterError, Candidate, Class, CommandOutput, CommandSpec, ConfigError, Ctx,
    DecisionLog, DirEntry, DiskUsage, FileMeta, Notification, Outcome, Platform, Pressure, Store,
};
use disk_saver_platform::FakePlatform;

use super::{NAME, factory, human_age};

const DAY: u64 = 24 * 60 * 60;

const CONTAINERS: &str = "~/Library/Containers";
const SMS: &str = "~/Library/Containers/com.apple.MobileSMS/Data/tmp";
const SMS_ID: &str = "/home/tester/Library/Containers/com.apple.MobileSMS/Data/tmp";
const GEOD: &str = "~/Library/Containers/com.apple.geod/Data/tmp";
const GEOD_ID: &str = "/home/tester/Library/Containers/com.apple.geod/Data/tmp";
const SCAVENGE: Pressure = Pressure::Scavenge { need: 1 };

// ── fixtures ─────────────────────────────────────────────────────────────────

/// The fake clock's fixed "now".
fn now() -> SystemTime {
    FakePlatform::new().now()
}

fn days_ago(n: u64) -> SystemTime {
    now() - Duration::from_secs(n * DAY)
}

/// `~` expanded against the fake home, as the fake stores paths.
fn abs(path: &str) -> PathBuf {
    PathBuf::from(path.replacen('~', "/home/tester", 1))
}

/// Build the adapter from a TOML fragment (empty → defaults).
fn adapter(cfg: &str) -> Box<dyn Adapter> {
    try_build(cfg).expect("config should be valid")
}

fn try_build(cfg: &str) -> Result<Box<dyn Adapter>, ConfigError> {
    let raw = (!cfg.is_empty())
        .then(|| toml::from_str::<toml::Value>(cfg).expect("test config is valid TOML"));
    factory().build(raw)
}

fn with_ctx<R>(p: &dyn Platform, pressure: Pressure, f: impl FnOnce(&mut Ctx) -> R) -> R {
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(p, store.bucket(NAME), pressure, &log);
    f(&mut ctx)
}

fn try_plan(
    p: &dyn Platform,
    pressure: Pressure,
    cfg: &str,
) -> Result<Vec<Candidate>, AdapterError> {
    with_ctx(p, pressure, |ctx| adapter(cfg).plan(ctx))
}

fn plan(p: &dyn Platform, pressure: Pressure) -> Vec<Candidate> {
    try_plan(p, pressure, "").expect("plan should succeed")
}

fn check(p: &dyn Platform) -> Result<(), AdapterError> {
    with_ctx(p, Pressure::Normal, |ctx| adapter("").check(ctx))
}

fn execute(p: &dyn Platform, cfg: &str, batch: &[Candidate]) -> Vec<Outcome> {
    with_ctx(p, Pressure::Normal, |ctx| adapter(cfg).execute(ctx, batch)).unwrap()
}

fn ids(plan: &[Candidate]) -> Vec<&str> {
    plan.iter().map(|c| c.id.as_str()).collect()
}

fn unavailable_message(r: Result<impl std::fmt::Debug, AdapterError>) -> String {
    match r {
        Err(AdapterError::Unavailable(msg)) => msg,
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// [`FakePlatform`] plus what it cannot script: macOS refusing a directory
/// (it may be stat'ed but not listed, and nothing below it can be stat'ed),
/// removals that fail, and a count of every filesystem call.
struct World {
    fake: FakePlatform,
    refused: Vec<PathBuf>,
    failing: Vec<PathBuf>,
    fs_calls: AtomicUsize,
}

impl World {
    fn new(fake: FakePlatform) -> Self {
        World {
            fake,
            refused: Vec::new(),
            failing: Vec::new(),
            fs_calls: AtomicUsize::new(0),
        }
    }

    fn refuse(mut self, dir: &str) -> Self {
        self.refused.push(abs(dir));
        self
    }

    fn fail_removal(mut self, path: &str) -> Self {
        self.failing.push(abs(path));
        self
    }

    fn fs_calls(&self) -> usize {
        self.fs_calls.load(Ordering::SeqCst)
    }

    fn count(&self) {
        self.fs_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn refused_error() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "Operation not permitted")
    }
}

impl Platform for World {
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
        self.count();
        if self
            .refused
            .iter()
            .any(|r| path != r && path.starts_with(r))
        {
            return Err(Self::refused_error());
        }
        self.fake.metadata(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.count();
        if self.refused.iter().any(|r| path.starts_with(r)) {
            return Err(Self::refused_error());
        }
        self.fake.read_dir(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.count();
        self.fake.read_to_string(path)
    }
    fn dir_size(&self, path: &Path) -> io::Result<u64> {
        self.count();
        self.fake.dir_size(path)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.count();
        if self.failing.iter().any(|f| f == path) {
            return Err(Self::refused_error());
        }
        self.fake.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.count();
        if self.failing.iter().any(|f| f == path) {
            return Err(Self::refused_error());
        }
        self.fake.remove_dir_all(path)
    }
    fn disk_usage(&self, path: &Path) -> io::Result<DiskUsage> {
        self.fake.disk_usage(path)
    }
    fn notify(&self, n: &Notification) -> io::Result<()> {
        self.fake.notify(n)
    }
    fn run_command(&self, cmd: &CommandSpec) -> io::Result<CommandOutput> {
        self.fake.run_command(cmd)
    }
}

// ── config ───────────────────────────────────────────────────────────────────

#[test]
fn defaults_propose_old_items_as_user_data_awaiting_review() {
    let fake = FakePlatform::new()
        .with_file(format!("{SMS}/old.png"), vec![0u8; 100], days_ago(40))
        .with_file(format!("{SMS}/new.png"), vec![0u8; 50], days_ago(20));

    let mut a = adapter("");
    assert_eq!(a.name(), NAME);
    let plan = with_ctx(&fake, Pressure::Normal, |ctx| a.plan(ctx)).unwrap();

    assert_eq!(ids(&plan), [SMS_ID]);
    let c = &plan[0];
    assert_eq!(c.bytes, 100, "only the item past the 30d default");
    assert_eq!(c.last_used, days_ago(40));
    assert_eq!(c.class, Class::UserData);
    assert!(c.requires_confirmation, "confirm defaults to true");
}

#[test]
fn confirm_false_yields_an_unflagged_candidate() {
    let fake = FakePlatform::new().with_file(format!("{SMS}/old.png"), "x", days_ago(40));
    let plan = try_plan(&fake, Pressure::Normal, "confirm = false").unwrap();
    assert_eq!(plan.len(), 1);
    assert!(!plan[0].requires_confirmation);
}

#[test]
fn bad_config_is_rejected_against_the_adapter() {
    for cfg in [
        "max_age = \"1d\"\nmin_age = \"7d\"",
        "exclude_bundle = [\"com.apple.geod\"]",
        "containers_dir = \"Library/Containers\"",
        "exclude_bundles = [\"~/Library/Containers/com.apple.geod\"]",
        "exclude_bundles = [\"\"]",
    ] {
        match try_build(cfg) {
            Err(ConfigError::Adapter { adapter, .. }) => assert_eq!(adapter, NAME, "{cfg}"),
            Err(other) => panic!("{cfg}: expected an adapter error, got {other:?}"),
            Ok(_) => panic!("{cfg}: expected a config error"),
        }
    }
}

#[test]
fn containers_dir_can_point_elsewhere() {
    let fake = FakePlatform::new()
        .with_file(
            "/srv/containers/com.example.app/Data/tmp/old.bin",
            "xy",
            days_ago(40),
        )
        .with_file(format!("{SMS}/old.png"), "x", days_ago(40));
    let plan = try_plan(
        &fake,
        Pressure::Normal,
        "containers_dir = \"/srv/containers\"",
    )
    .unwrap();
    assert_eq!(ids(&plan), ["/srv/containers/com.example.app/Data/tmp"]);
}

// ── pressure and age ─────────────────────────────────────────────────────────

#[test]
fn age_threshold_follows_pressure() {
    let fake = FakePlatform::new()
        .with_file(format!("{SMS}/3d.bin"), vec![0u8; 1], days_ago(3))
        .with_file(format!("{SMS}/10d.bin"), vec![0u8; 10], days_ago(10))
        .with_file(format!("{SMS}/40d.bin"), vec![0u8; 40], days_ago(40));

    assert!(plan(&fake, Pressure::Comfortable).is_empty());

    let normal = plan(&fake, Pressure::Normal);
    assert_eq!(normal.len(), 1);
    assert_eq!(normal[0].bytes, 40, "normal: max_age 30d");

    let scavenge = plan(&fake, SCAVENGE);
    assert_eq!(scavenge.len(), 1);
    assert_eq!(
        scavenge[0].bytes, 50,
        "scavenge: min_age 7d, never the 3d item"
    );
    assert_eq!(scavenge[0].last_used, days_ago(10));
}

#[test]
fn comfortable_runs_and_observe_never_touch_the_filesystem() {
    let world =
        World::new(FakePlatform::new().with_file(format!("{SMS}/old.bin"), "x", days_ago(400)));
    with_ctx(&world, Pressure::Normal, |ctx| adapter("").observe(ctx)).unwrap();
    assert!(plan(&world, Pressure::Comfortable).is_empty());
    assert_eq!(world.fs_calls(), 0);

    // The same world at normal pressure does look, and finds it.
    assert_eq!(plan(&world, Pressure::Normal).len(), 1);
    assert!(world.fs_calls() > 0);
}

#[test]
fn one_recent_file_inside_keeps_the_whole_unit() {
    let fake = FakePlatform::new()
        .with_file(format!("{SMS}/project/a.bin"), vec![0u8; 5], days_ago(90))
        .with_file(
            format!("{SMS}/project/deep/er/b.bin"),
            vec![0u8; 5],
            days_ago(90),
        )
        .with_file(format!("{SMS}/project/deep/er/fresh.log"), "x", days_ago(2))
        .with_file(format!("{SMS}/read/report.pdf"), vec![0u8; 7], days_ago(90))
        .with_file(format!("{SMS}/listed/old.bin"), vec![0u8; 11], days_ago(90));
    // Reading a file is activity; listing a directory is not.
    fake.set_accessed(format!("{SMS}/read/report.pdf"), Some(days_ago(1)));
    fake.set_accessed(format!("{SMS}/listed"), Some(days_ago(1)));

    let plan = plan(&fake, Pressure::Normal);
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].bytes, 11, "only `listed` is old all the way down");
    assert!(plan[0].label.contains("1 old item in"), "{}", plan[0].label);
}

// ── units ────────────────────────────────────────────────────────────────────

#[test]
fn temporary_items_children_are_units_and_it_is_never_removed_itself() {
    let fake = FakePlatform::new()
        .with_file(
            format!("{SMS}/TemporaryItems/NSIRD_Messages_abc/draft.txt"),
            vec![0u8; 10],
            days_ago(60),
        )
        .with_file(
            format!("{SMS}/TemporaryItems/NSIRD_Messages_def/live.txt"),
            vec![0u8; 20],
            days_ago(1),
        )
        .with_file(
            format!("{SMS}/TemporaryItems/stale.txt"),
            vec![0u8; 5],
            days_ago(60),
        );

    let plan = plan(&fake, Pressure::Normal);
    assert_eq!(plan.len(), 1);
    assert_eq!(
        plan[0].bytes, 15,
        "the recent staging item does not hold back its siblings"
    );
    assert!(
        plan[0]
            .label
            .starts_with("2 old items in com.apple.MobileSMS temp")
    );

    let outcomes = execute(&fake, "", &plan);
    assert!(
        matches!(&outcomes[..], [Outcome::Removed { bytes: 15, .. }]),
        "{outcomes:?}"
    );
    assert_eq!(
        fake.removed(),
        [
            abs(&format!("{SMS}/TemporaryItems/NSIRD_Messages_abc")),
            abs(&format!("{SMS}/TemporaryItems/stale.txt")),
        ]
    );
    assert!(fake.exists(format!("{SMS}/TemporaryItems/NSIRD_Messages_def/live.txt")));
}

/// The case this adapter exists for: Messages stages each attachment in its own
/// `TemporaryItems/com.apple.MobileSMS/Media/<UUID>/`, next to ones that
/// arrived today.
#[test]
fn messages_staging_root_holds_one_unit_per_attachment() {
    let items = format!("{SMS}/TemporaryItems");
    let fake = FakePlatform::new()
        .with_file(
            format!("{items}/com.apple.MobileSMS/Media/AAAA/Some Installer-1.0.pkg"),
            vec![0u8; 860],
            days_ago(240),
        )
        .with_file(
            format!("{items}/com.apple.MobileSMS/Media/BBBB/IMG_0001.heic"),
            vec![0u8; 50],
            days_ago(0),
        )
        .with_file(
            format!("{items}/com.apple.MobileSMS/LinkedFiles/CCCC/notes.pdf"),
            vec![0u8; 40],
            days_ago(100),
        )
        // Another bundle's folder is not this app's staging root: one unit,
        // held back by its recent file.
        .with_file(
            format!("{items}/com.example.other/Media/DDDD/old.bin"),
            vec![0u8; 9],
            days_ago(300),
        )
        .with_file(
            format!("{items}/com.example.other/Media/EEEE/new.bin"),
            vec![0u8; 9],
            days_ago(0),
        );

    let plan = plan(&fake, Pressure::Normal);
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].bytes, 900);
    assert!(
        plan[0]
            .label
            .ends_with("Some Installer-1.0.pkg 860 B, notes.pdf 40 B"),
        "a UUID wrapper is named after what it holds: {}",
        plan[0].label
    );

    execute(&fake, "", &plan);
    assert_eq!(
        fake.removed(),
        [
            abs(&format!("{items}/com.apple.MobileSMS/LinkedFiles/CCCC")),
            abs(&format!("{items}/com.apple.MobileSMS/Media/AAAA")),
        ]
    );
    assert!(fake.exists(format!("{items}/com.apple.MobileSMS/Media")));
    assert!(fake.exists(format!("{items}/com.apple.MobileSMS/Media/BBBB")));
    assert!(fake.exists(format!("{items}/com.example.other/Media/DDDD/old.bin")));
}

#[test]
fn one_candidate_per_container_labelled_with_ages_and_largest_items() {
    let items = format!("{SMS}/TemporaryItems/com.apple.MobileSMS/Media");
    let fake = FakePlatform::new()
        // A sized directory stands in for an 8.6 GB file the fake cannot hold.
        .with_sized_dir(
            format!("{items}/AAAA/Some Installer-1.0.pkg"),
            8_600_000_000,
            days_ago(240),
        )
        .with_file(format!("{SMS}/hw_1.png"), vec![0u8; 30], days_ago(31))
        .with_file(format!("{SMS}/hw_2.png"), vec![0u8; 20], days_ago(100))
        .with_file(format!("{SMS}/hw_3.png"), vec![0u8; 10], days_ago(50))
        .with_file(format!("{SMS}/hw_4.png"), vec![0u8; 10], days_ago(2))
        .with_file(format!("{GEOD}/tile-1"), vec![0u8; 300], days_ago(45))
        .with_file(format!("{GEOD}/tile-2"), vec![0u8; 0], days_ago(45))
        .with_file(format!("{GEOD}/tile-3"), vec![0u8; 300], days_ago(1))
        .with_file(
            "~/Library/Containers/com.apple.Notes/Data/tmp/scratch",
            "x",
            days_ago(400),
        )
        .with_dir(
            "~/Library/Containers/com.apple.Empty/Data/tmp",
            days_ago(400),
        );

    let plan = plan(&fake, Pressure::Normal);
    assert_eq!(
        ids(&plan),
        [
            SMS_ID,
            "/home/tester/Library/Containers/com.apple.Notes/Data/tmp",
            GEOD_ID
        ]
    );

    let sms = &plan[0];
    assert_eq!(
        sms.label,
        "4 old items in com.apple.MobileSMS temp (oldest 8 months, newest 31 days): \
         Some Installer-1.0.pkg 8.6 GB, hw_1.png 30 B, hw_2.png 20 B, +1 more"
    );
    assert_eq!(sms.bytes, 8_600_000_060);
    assert_eq!(
        sms.last_used,
        days_ago(31),
        "the group is as young as its youngest item"
    );

    assert_eq!(
        plan[1].label,
        "1 old item in com.apple.Notes temp (13 months old): scratch 1 B"
    );
    assert_eq!(
        plan[2].label,
        "2 old items in com.apple.geod temp (oldest 45 days, newest 45 days): \
         tile-1 300 B, tile-2 0 B"
    );
    assert_eq!(plan[2].bytes, 300);
}

#[test]
fn old_items_that_reclaim_nothing_are_not_worth_a_prompt() {
    let fake = FakePlatform::new()
        .with_dir(format!("{SMS}/NSIRD_empty"), days_ago(90))
        .with_file(format!("{SMS}/zero.bin"), vec![0u8; 0], days_ago(90));
    assert!(plan(&fake, Pressure::Normal).is_empty());
}

// ── safety ───────────────────────────────────────────────────────────────────

#[test]
fn symlinks_are_never_followed() {
    let elsewhere = "~/elsewhere";
    let fake = FakePlatform::new()
        .with_file(
            format!("{elsewhere}/container/Data/tmp/old.bin"),
            vec![0u8; 1000],
            days_ago(100),
        )
        .with_file(
            format!("{elsewhere}/staging/NSIRD_x/old.bin"),
            vec![0u8; 1000],
            days_ago(100),
        )
        .with_file(
            format!("{elsewhere}/big.bin"),
            vec![0u8; 1000],
            days_ago(100),
        )
        // A symlinked container, `Data`, `tmp`, staging folder and unit.
        .with_symlink(
            format!("{CONTAINERS}/com.link.container"),
            format!("{elsewhere}/container"),
            days_ago(100),
        )
        .with_symlink(
            format!("{CONTAINERS}/com.link.data/Data"),
            format!("{elsewhere}/container/Data"),
            days_ago(100),
        )
        .with_symlink(
            format!("{CONTAINERS}/com.link.tmp/Data/tmp"),
            format!("{elsewhere}/container/Data/tmp"),
            days_ago(100),
        )
        .with_symlink(
            format!("{SMS}/TemporaryItems"),
            format!("{elsewhere}/staging"),
            days_ago(100),
        )
        .with_symlink(
            format!("{SMS}/big-link"),
            format!("{elsewhere}/big.bin"),
            days_ago(100),
        )
        .with_file(format!("{SMS}/real.bin"), vec![0u8; 10], days_ago(100));

    let plan = plan(&fake, Pressure::Normal);
    assert_eq!(ids(&plan), [SMS_ID]);
    assert_eq!(plan[0].bytes, 10);

    // Even an approval naming a symlinked temp folder removes nothing.
    let forged = Candidate::new(
        "/home/tester/Library/Containers/com.link.tmp/Data/tmp",
        "forged",
        u64::MAX,
        now(),
        Class::UserData,
    );
    let outcomes = execute(&fake, "", &[plan[0].clone(), forged]);
    assert!(
        matches!(outcomes[0], Outcome::Removed { bytes: 10, .. }),
        "{outcomes:?}"
    );
    assert!(
        matches!(outcomes[1], Outcome::Skipped { .. }),
        "{outcomes:?}"
    );
    assert_eq!(fake.removed(), [abs(&format!("{SMS}/real.bin"))]);
    for kept in [
        "container/Data/tmp/old.bin",
        "staging/NSIRD_x/old.bin",
        "big.bin",
    ] {
        assert!(fake.exists(format!("{elsewhere}/{kept}")), "{kept}");
    }
}

#[test]
fn units_never_cross_a_filesystem_boundary() {
    let fake = FakePlatform::new()
        .with_file(
            format!("{SMS}/holds-a-mount/old.bin"),
            vec![0u8; 10],
            days_ago(100),
        )
        .with_mount(format!("{SMS}/holds-a-mount/volume"))
        .with_mount(format!("{SMS}/is-a-mount"))
        .with_file(
            format!("{SMS}/is-a-mount/old.bin"),
            vec![0u8; 10],
            days_ago(100),
        );
    // Age everything, the mounts included, well past the threshold: only the
    // device checks stand in the way.
    fake.advance(Duration::from_secs(100 * DAY));
    assert!(plan(&fake, Pressure::Normal).is_empty());
}

#[test]
fn excluded_bundles_are_left_alone_even_when_already_approved() {
    let fake = FakePlatform::new()
        .with_file(format!("{SMS}/old.bin"), "x", days_ago(100))
        .with_file(format!("{GEOD}/old.bin"), "x", days_ago(100));
    let cfg = "exclude_bundles = [\"com.apple.geod\"]";

    assert_eq!(
        ids(&try_plan(&fake, Pressure::Normal, cfg).unwrap()),
        [SMS_ID]
    );

    // An approval from before the exclusion is not acted on.
    let approved = plan(&fake, Pressure::Normal);
    assert_eq!(ids(&approved), [SMS_ID, GEOD_ID]);
    let outcomes = execute(&fake, cfg, &approved[1..]);
    assert!(
        matches!(&outcomes[..], [Outcome::Skipped { .. }]),
        "{outcomes:?}"
    );
    assert!(fake.removed().is_empty());
}

#[test]
fn execute_only_acts_on_app_temp_folders() {
    let fake = FakePlatform::new()
        .with_file(format!("{SMS}/old.bin"), "x", days_ago(100))
        .with_file("~/Documents/old.bin", "x", days_ago(100));
    let forged = |id: &str| Candidate::new(id, "forged", u64::MAX, now(), Class::UserData);
    let batch = [
        forged("/home/tester/Documents"),
        forged("/home/tester/Library/Containers/com.apple.MobileSMS/Data"),
        forged("/home/tester/Library/Containers/com.apple.MobileSMS/Data/tmp/old.bin"),
        forged("/home/tester/Library/Containers/../../Documents/Data/tmp"),
        forged("Library/Containers/com.apple.MobileSMS/Data/tmp"),
    ];
    let outcomes = execute(&fake, "", &batch);
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, Outcome::Skipped { .. })),
        "{outcomes:?}"
    );
    assert!(fake.removed().is_empty());
}

// ── platforms and permissions ────────────────────────────────────────────────

#[test]
fn no_containers_dir_is_nothing_to_do() {
    // Every Linux machine.
    let fake = FakePlatform::new().with_file("~/.cache/old.bin", "x", days_ago(400));
    assert!(plan(&fake, SCAVENGE).is_empty());
    assert!(check(&fake).is_ok());
}

#[test]
fn an_unlistable_containers_dir_is_unavailable_with_a_full_disk_access_hint() {
    let world =
        World::new(FakePlatform::new().with_file(format!("{SMS}/old.bin"), "x", days_ago(100)))
            .refuse(CONTAINERS);
    for msg in [
        unavailable_message(try_plan(&world, Pressure::Normal, "")),
        unavailable_message(check(&world)),
    ] {
        assert!(msg.contains("Full Disk Access"), "{msg}");
    }
}

#[test]
fn an_unreadable_container_is_skipped_and_the_rest_still_planned() {
    let world = World::new(
        FakePlatform::new()
            .with_file(format!("{SMS}/old.bin"), "x", days_ago(100))
            .with_file(format!("{GEOD}/old.bin"), "x", days_ago(100))
            .with_file(
                "~/Library/Containers/com.apple.Notes/Data/tmp/old.bin",
                "x",
                days_ago(100),
            ),
    )
    // Refused at the container (stat fails) and at the temp folder (listing fails).
    .refuse("~/Library/Containers/com.apple.MobileSMS")
    .refuse("~/Library/Containers/com.apple.Notes/Data/tmp");

    assert_eq!(ids(&plan(&world, Pressure::Normal)), [GEOD_ID]);
    assert!(check(&world).is_ok());
}

#[test]
fn every_container_refused_is_unavailable() {
    // What a launchd job without Full Disk Access sees: the containers
    // directory lists, but nothing inside it opens.
    let world = World::new(
        FakePlatform::new()
            .with_file(format!("{SMS}/old.bin"), "x", days_ago(100))
            .with_file(format!("{GEOD}/old.bin"), "x", days_ago(100))
            .with_dir("~/Library/Containers/com.apple.NoTemp/Data", days_ago(100)),
    )
    .refuse("~/Library/Containers/com.apple.MobileSMS")
    .refuse(GEOD);

    for msg in [
        unavailable_message(try_plan(&world, Pressure::Normal, "")),
        unavailable_message(check(&world)),
    ] {
        assert!(msg.contains("Full Disk Access"), "{msg}");
        assert!(msg.contains("2 refused"), "{msg}");
    }
}

// ── execute ──────────────────────────────────────────────────────────────────

#[test]
fn execute_removes_only_units_at_least_as_old_as_approved() {
    let fake = FakePlatform::new()
        .with_file(format!("{SMS}/a.bin"), vec![0u8; 100], days_ago(100))
        .with_file(format!("{SMS}/b/inner.bin"), vec![0u8; 40], days_ago(40))
        .with_file(format!("{SMS}/olddir/x.bin"), vec![0u8; 7], days_ago(90));
    let approved = plan(&fake, Pressure::Normal);
    assert_eq!(approved[0].bytes, 147);
    assert_eq!(approved[0].last_used, days_ago(40));

    // Since approval: `b` was opened, and an item arrived that is past the
    // threshold but newer than anything the reviewer was shown.
    fake.set_accessed(format!("{SMS}/b/inner.bin"), Some(days_ago(0)));
    let fake = fake.with_file(format!("{SMS}/c.bin"), vec![0u8; 3], days_ago(35));

    let outcomes = execute(&fake, "", &approved);
    match &outcomes[..] {
        [Outcome::Removed { id, bytes }] => {
            assert_eq!(id, SMS_ID);
            assert_eq!(*bytes, 107, "the bytes actually freed, not the estimate");
        }
        other => panic!("expected one Removed, got {other:?}"),
    }
    // A file via remove_file and a directory via remove_dir_all (the fake
    // refuses either on the wrong kind).
    assert_eq!(
        fake.removed(),
        [abs(&format!("{SMS}/a.bin")), abs(&format!("{SMS}/olddir"))]
    );
    assert!(fake.exists(format!("{SMS}/b/inner.bin")));
    assert!(fake.exists(format!("{SMS}/c.bin")));
}

#[test]
fn execute_never_removes_more_than_was_approved() {
    let fake = FakePlatform::new().with_file(format!("{SMS}/a.bin"), vec![0u8; 100], days_ago(100));
    let approved = plan(&fake, Pressure::Normal);
    // Something arrives carrying an ancient timestamp (an unpacked archive, a
    // moved file): it would pass the age test, but nobody approved it.
    let fake = fake.with_file(format!("{SMS}/unpacked.bin"), vec![0u8; 5], days_ago(300));

    let outcomes = execute(&fake, "", &approved);
    match &outcomes[..] {
        [Outcome::Skipped { reason, .. }] => assert!(reason.contains("proposed again"), "{reason}"),
        other => panic!("expected one Skipped, got {other:?}"),
    }
    assert!(fake.removed().is_empty());
}

#[test]
fn a_partial_failure_is_reported_as_failed_with_what_was_removed() {
    let world = World::new(
        FakePlatform::new()
            .with_file(format!("{SMS}/a.bin"), vec![0u8; 100], days_ago(100))
            .with_file(format!("{SMS}/locked/b.bin"), vec![0u8; 40], days_ago(100)),
    )
    .fail_removal(&format!("{SMS}/locked"));
    let approved = plan(&world, Pressure::Normal);

    let outcomes = execute(&world, "", &approved);
    match &outcomes[..] {
        [Outcome::Failed { id, error }] => {
            assert_eq!(id, SMS_ID);
            assert!(
                error.contains("removed 1 of 2 old items (100 B)"),
                "{error}"
            );
            assert!(error.contains("locked: Operation not permitted"), "{error}");
        }
        other => panic!("expected one Failed, got {other:?}"),
    }
    assert_eq!(world.fake.removed(), [abs(&format!("{SMS}/a.bin"))]);
}

#[test]
fn execute_skips_a_temp_folder_that_has_gone_or_emptied() {
    let fake = FakePlatform::new().with_file(format!("{SMS}/a.bin"), "x", days_ago(100));
    let approved = plan(&fake, Pressure::Normal);
    fake.remove_file(&abs(&format!("{SMS}/a.bin"))).unwrap();
    assert!(matches!(
        &execute(&fake, "", &approved)[..],
        [Outcome::Skipped { .. }]
    ));

    fake.remove_dir_all(&abs(SMS)).unwrap();
    assert!(matches!(
        &execute(&fake, "", &approved)[..],
        [Outcome::Skipped { .. }]
    ));
}

// ── wording ──────────────────────────────────────────────────────────────────

#[test]
fn ages_read_the_way_people_say_them() {
    let cases = [
        (Duration::from_secs(45 * 60), "45 minutes"),
        (Duration::from_secs(60 * 60), "1 hour"),
        (Duration::from_secs(DAY), "1 day"),
        (Duration::from_secs(31 * DAY), "31 days"),
        (Duration::from_secs(59 * DAY), "59 days"),
        (Duration::from_secs(240 * DAY), "8 months"),
        (Duration::from_secs(800 * DAY), "2 years"),
    ];
    for (age, text) in cases {
        assert_eq!(human_age(age), text);
    }
}
