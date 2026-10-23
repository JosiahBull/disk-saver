use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_core::{
    Adapter, AdapterError, Candidate, Class, CommandSpec, Ctx, DecisionKind, DecisionLog, DirEntry,
    DiskUsage, FileMeta, Notification, Outcome, Platform, Pressure, RecordedDecision, Store,
    read_decisions,
};
use disk_saver_platform::{CommandOutput, FakePlatform};

use super::factory;
use crate::commands::{parse_show, rustup_binary};
use crate::label::date;
use crate::protection::parse_pin;
use crate::toolchain::{resolve, target_triple};

const HOST: &str = "aarch64-apple-darwin";
const RUSTUP: &str = "/home/tester/.cargo/bin/rustup";
const OLD: &str = "1.93.1-aarch64-apple-darwin";
const OLD_DIR: &str = "/home/tester/.rustup/toolchains/1.93.1-aarch64-apple-darwin";

/// `days` after the epoch; the fake clock reads `t(400)`.
fn t(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 86400)
}

/// `rustup show` in the rustup 1.29 format, with `listed` as the installed list.
fn show(listed: &str) -> CommandOutput {
    CommandOutput::ok(format!(
        "Default host: {HOST}\nrustup home:  /home/tester/.rustup\n\n\
         installed toolchains\n--------------------\n{listed}\n\n\
         active toolchain\n----------------\nname: nightly-{HOST}\n\
         active because: it's the default toolchain\n"
    ))
}

const LISTED: &str = "stable-aarch64-apple-darwin\nnightly-aarch64-apple-darwin (active, default)";

/// rustup's settings.toml with nightly as the default and `overrides` appended.
fn settings(overrides: &str) -> String {
    format!(
        "version = \"12\"\ndefault_toolchain = \"nightly-{HOST}\"\nprofile = \"default\"\n\n[overrides]\n{overrides}"
    )
}

fn no_open_files() -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: vec![],
        stderr: vec![],
    }
}

/// A toolchain whose files were all last touched at `used`: 1010 bytes.
fn with_toolchain(p: FakePlatform, name: &str, used: SystemTime) -> FakePlatform {
    with_toolchain_in(p, "~/.rustup", name, used)
}

fn with_toolchain_in(p: FakePlatform, home: &str, name: &str, used: SystemTime) -> FakePlatform {
    let dir = format!("{home}/toolchains/{name}");
    p.with_file(format!("{dir}/bin/rustc"), "rustc", used)
        .with_file(format!("{dir}/bin/cargo"), "cargo", used)
        .with_file(
            format!("{dir}/lib/librustc_driver.dylib"),
            vec![0u8; 1000],
            used,
        )
}

/// rustup, its settings and `rustup show`; no toolchains, no `lsof`.
fn bare() -> FakePlatform {
    FakePlatform::new()
        .with_file("~/.cargo/bin/rustup", "rustup", t(100))
        .with_file("~/.rustup/settings.toml", settings(""), t(100))
        .with_command(RUSTUP, &["show"], show(LISTED))
}

/// The host's channel toolchains and one old numbered one, all last used 200
/// days ago (`now` is day 400), with `lsof` finding nothing open.
fn fixture() -> FakePlatform {
    toolchains(bare().with_command_prefix("lsof", &["-t", "--"], no_open_files()))
}

fn toolchains(p: FakePlatform) -> FakePlatform {
    [
        "stable-aarch64-apple-darwin",
        "beta-aarch64-apple-darwin",
        "nightly-aarch64-apple-darwin",
        OLD,
    ]
    .into_iter()
    .fold(p, |p, name| with_toolchain(p, name, t(200)))
}

fn uninstall_ok(p: FakePlatform, name: &str) -> FakePlatform {
    p.with_command(
        RUSTUP,
        &["toolchain", "uninstall", name],
        CommandOutput::ok(format!("info: toolchain '{name}' uninstalled\n")),
    )
}

fn run<R>(
    p: &dyn Platform,
    cfg: &str,
    pressure: Pressure,
    f: impl FnOnce(&mut dyn Adapter, &mut Ctx) -> R,
) -> R {
    let cfg = (!cfg.is_empty()).then(|| toml::from_str(cfg).unwrap());
    let mut a = factory().build(cfg).unwrap();
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(p, store.bucket("rustup"), pressure, &log);
    f(a.as_mut(), &mut ctx)
}

/// Plan at normal pressure with a real decision log, and read it back.
fn propose_logged(
    p: &dyn Platform,
) -> (Result<Vec<Candidate>, AdapterError>, Vec<RecordedDecision>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.db");
    let log = DecisionLog::open(&path).unwrap();
    let store = Store::open_in_memory().unwrap();
    let mut a = factory().build(None).unwrap();
    let mut ctx = Ctx::new(p, store.bucket("rustup"), Pressure::Normal, &log);
    let plan = a.plan(&mut ctx);
    (plan, read_decisions(&path).unwrap())
}

/// The reason recorded for `name` with `kind`.
fn reason(decisions: &[RecordedDecision], kind: DecisionKind, name: &str) -> String {
    let id = format!("/home/tester/.rustup/toolchains/{name}");
    decisions
        .iter()
        .rfind(|d| d.kind == kind && d.id == id)
        .unwrap_or_else(|| panic!("no {kind:?} decision for {name}: {decisions:#?}"))
        .reason
        .clone()
        .unwrap_or_default()
}

fn propose(p: &dyn Platform, cfg: &str) -> Result<Vec<Candidate>, AdapterError> {
    run(p, cfg, Pressure::Normal, |a, ctx| a.plan(ctx))
}

fn execute(p: &dyn Platform, batch: &[Candidate]) -> Vec<Outcome> {
    execute_with(p, "", batch)
}

fn execute_with(p: &dyn Platform, cfg: &str, batch: &[Candidate]) -> Vec<Outcome> {
    run(p, cfg, Pressure::Normal, |a, ctx| {
        a.execute(ctx, batch).unwrap()
    })
}

/// [`FakePlatform`] with two things it cannot model: `read_to_string`
/// follows the given links the way the real platform's does (the fake's
/// refuses a symlink), and `read_dir` on the given directories is denied, as
/// `~/Documents` is to a scheduled job without Full Disk Access.
struct Tweaked {
    fake: FakePlatform,
    links: Vec<(PathBuf, PathBuf)>,
    denied: Vec<PathBuf>,
}

impl Tweaked {
    fn new(fake: FakePlatform) -> Self {
        Tweaked {
            fake,
            links: Vec::new(),
            denied: Vec::new(),
        }
    }

    /// `link` reads as `target` (both absolute).
    fn link(mut self, link: &str, target: &str) -> Self {
        self.links.push((link.into(), target.into()));
        self
    }

    /// Listing `dir` (absolute) fails with `PermissionDenied`.
    fn deny(mut self, dir: &str) -> Self {
        self.denied.push(dir.into());
        self
    }
}

impl Platform for Tweaked {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let target = self
            .links
            .iter()
            .find(|(link, _)| link == path)
            .map_or(path, |(_, target)| target.as_path());
        self.fake.read_to_string(target)
    }
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
        if self.denied.iter().any(|d| d == path) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        self.fake.read_dir(path)
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
        self.fake.run_command(cmd)
    }
}

/// The toolchain names a plan proposes.
fn names(plan: &[Candidate]) -> Vec<&str> {
    plan.iter()
        .map(|c| Path::new(&c.id).file_name().unwrap().to_str().unwrap())
        .collect()
}

fn label<'a>(plan: &'a [Candidate], name: &str) -> &'a str {
    &plan.iter().find(|c| c.id.ends_with(name)).unwrap().label
}

fn uninstalls(p: &FakePlatform) -> usize {
    p.commands_run()
        .iter()
        .filter(|c| c.args.first().map(String::as_str) == Some("toolchain"))
        .count()
}

fn skipped(outcome: &Outcome, because: &str) -> bool {
    matches!(outcome, Outcome::Skipped { reason, .. } if reason.contains(because))
}

// ── planning ────────────────────────────────────────────────────────────────

#[test]
fn old_toolchain_needs_approval_while_default_and_host_channels_are_kept() {
    let p = fixture();
    let plan = propose(&p, "").unwrap();
    assert_eq!(names(&plan), [OLD]);
    let c = &plan[0];
    assert_eq!(c.id, OLD_DIR);
    assert_eq!(c.class, Class::Cache);
    assert!(c.requires_confirmation);
    assert_eq!(c.bytes, 1010);
    assert_eq!(c.last_used, t(200));
    assert_eq!(
        c.label,
        "uninstall Rust 1.93.1-aarch64-apple-darwin: last used 1970-07-20; no project pins it"
    );
    assert_eq!(uninstalls(&p), 0);
    assert!(p.removed().is_empty());

    let plan = propose(&p, "confirm = false").unwrap();
    assert!(!plan[0].requires_confirmation);
}

#[test]
fn rustup_is_found_without_path_in_order() {
    let all = FakePlatform::new()
        .with_env("CARGO_HOME", "/home/tester/cargo-home")
        .with_file("~/cargo-home/bin/rustup", "rustup", t(1))
        .with_file("~/.cargo/bin/rustup", "rustup", t(1))
        .with_file("~/tools/rustup", "rustup", t(1));
    let configured = Path::new("~/tools/rustup");
    assert_eq!(
        rustup_binary(&all, Some(configured)).unwrap(),
        PathBuf::from("/home/tester/tools/rustup")
    );
    assert_eq!(
        rustup_binary(&all, None).unwrap(),
        PathBuf::from("/home/tester/cargo-home/bin/rustup")
    );

    // $CARGO_HOME without a rustup in it falls through to ~/.cargo/bin.
    let fallback = FakePlatform::new()
        .with_env("CARGO_HOME", "/home/tester/cargo-home")
        .with_symlink("~/.cargo/bin/rustup", "/opt/homebrew/bin/rustup", t(1));
    assert_eq!(
        rustup_binary(&fallback, None).unwrap(),
        PathBuf::from("/home/tester/.cargo/bin/rustup")
    );

    // A configured path that is missing does not quietly become another rustup.
    let missing = Path::new("~/elsewhere/rustup");
    assert!(matches!(
        rustup_binary(&all, Some(missing)),
        Err(AdapterError::Unavailable(m)) if m.contains("/home/tester/elsewhere/rustup")
    ));
    assert!(matches!(
        rustup_binary(&FakePlatform::new(), None),
        Err(AdapterError::Unavailable(_))
    ));
}

#[test]
fn missing_rustup_is_unavailable_for_plan_and_doctor() {
    let p = toolchains(FakePlatform::new());
    assert!(matches!(propose(&p, ""), Err(AdapterError::Unavailable(_))));
    run(&p, "", Pressure::Normal, |a, ctx| {
        assert!(matches!(a.check(ctx), Err(AdapterError::Unavailable(_))));
    });
    run(&fixture(), "", Pressure::Normal, |a, ctx| {
        a.check(ctx).unwrap()
    });
}

#[test]
fn comfortable_pressure_runs_and_walks_nothing() {
    let p = fixture().with_file("~/work/app/rust-toolchain.toml", "broken [", t(399));
    let plan = run(&p, "", Pressure::Comfortable, |a, ctx| a.plan(ctx)).unwrap();
    assert!(plan.is_empty());
    assert!(p.commands_run().is_empty());
}

#[test]
fn normal_pressure_needs_max_age_and_scavenge_stops_at_min_age() {
    let p = with_toolchain(fixture(), "1.20.0-aarch64-apple-darwin", t(380));
    let p = with_toolchain(p, "1.10.0-aarch64-apple-darwin", t(397));
    assert_eq!(names(&propose(&p, "").unwrap()), [OLD]);
    let scavenge = run(&p, "", Pressure::Scavenge { need: 1 << 30 }, |a, ctx| {
        a.plan(ctx).unwrap()
    });
    assert_eq!(names(&scavenge), ["1.20.0-aarch64-apple-darwin", OLD]);
}

#[test]
fn recent_use_anywhere_in_the_tree_keeps_a_toolchain() {
    // `bin` looks idle, but rustc read the standard library yesterday.
    let p = fixture().with_file(
        format!("{OLD_DIR}/lib/rustlib/{HOST}/lib/libstd.rlib"),
        "rlib",
        t(200),
    );
    p.set_accessed(
        format!("{OLD_DIR}/lib/rustlib/{HOST}/lib/libstd.rlib"),
        Some(t(399)),
    );
    assert!(propose(&p, "").unwrap().is_empty());
}

// ── pins ────────────────────────────────────────────────────────────────────

#[test]
fn active_pin_protects_and_idle_pin_only_shows_in_the_label() {
    let active = fixture().with_file("~/work/app/rust-toolchain", "1.93.1\n", t(399));
    assert!(propose(&active, "").unwrap().is_empty());

    let idle = fixture().with_file("~/work/app/rust-toolchain", "1.93.1\n", t(300));
    let plan = propose(&idle, "").unwrap();
    assert_eq!(names(&plan), [OLD]);
    assert_eq!(
        plan[0].label,
        "uninstall Rust 1.93.1-aarch64-apple-darwin: last used 1970-07-20; \
         pinned only by ~/work/app (last edited 1970-10-28)"
    );

    // A longer window brings the idle project back into protection.
    let plan = propose(&idle, "pin_max_idle = \"200d\"").unwrap();
    assert!(plan.is_empty());
}

#[test]
fn nested_pins_are_judged_by_their_own_project() {
    // The nested project goes quiet first, then its parent is edited.
    let p = fixture()
        .with_file(
            "~/work/monorepo/imports/rust-libs/rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.88.0\"\ncomponents = [\"clippy\"]\n",
            t(300),
        )
        .with_file("~/work/monorepo/rust-toolchain", "1.96.1\n", t(399));
    let p = with_toolchain(p, "1.96.1-aarch64-apple-darwin", t(200));
    let p = with_toolchain(p, "1.88.0-aarch64-apple-darwin", t(200));
    let plan = propose(&p, "").unwrap();
    assert_eq!(names(&plan), ["1.88.0-aarch64-apple-darwin", OLD]);
    assert!(
        label(&plan, "1.88.0-aarch64-apple-darwin")
            .ends_with("pinned only by ~/work/monorepo/imports/rust-libs (last edited 1970-10-28)")
    );
}

#[test]
fn legacy_and_toml_pins_both_protect() {
    let legacy_toml = fixture().with_file(
        "~/work/app/rust-toolchain",
        "[toolchain]\nchannel = \"1.93.1\"\n",
        t(399),
    );
    assert!(propose(&legacy_toml, "").unwrap().is_empty());
    let toml = fixture().with_file(
        "~/work/app/rust-toolchain.toml",
        "[toolchain]\nchannel = \"1.93.1-aarch64-apple-darwin\"\n",
        t(399),
    );
    assert!(propose(&toml, "").unwrap().is_empty());
    // `1.93` is a different toolchain from `1.93.1` to rustup.
    let other = fixture().with_file("~/work/app/rust-toolchain", "1.93\n", t(399));
    assert_eq!(names(&propose(&other, "").unwrap()), [OLD]);
}

#[test]
fn pin_files_parse_like_rustup_reads_them() {
    let ok = |file: &str, text: &str| parse_pin(file, text).unwrap();
    assert_eq!(ok("rust-toolchain", "1.96.1\n").as_deref(), Some("1.96.1"));
    assert_eq!(
        ok("rust-toolchain", "\n  nightly-2024-05-01 \r\n").as_deref(),
        Some("nightly-2024-05-01")
    );
    assert_eq!(
        ok("rust-toolchain", "[toolchain]\nchannel = \"1.88.0\"\n").as_deref(),
        Some("1.88.0")
    );
    assert_eq!(
        ok("rust-toolchain", "toolchain = { channel = \"1.88.0\" }\n").as_deref(),
        Some("1.88.0")
    );
    assert_eq!(
        ok(
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"stable\"\ncomponents = [\"rustfmt\"]\n"
        )
        .as_deref(),
        Some("stable")
    );
    // A `path` toolchain, or none named at all, protects nothing installed.
    assert_eq!(
        ok("rust-toolchain.toml", "[toolchain]\npath = \"/opt/rust\"\n"),
        None
    );
    assert_eq!(
        ok(
            "rust-toolchain.toml",
            "[toolchain]\nprofile = \"minimal\"\n"
        ),
        None
    );

    for (file, text) in [
        ("rust-toolchain", ""),
        ("rust-toolchain", "  \n"),
        ("rust-toolchain", "nightly; rm -rf ~"),
        ("rust-toolchain.toml", ""),
        ("rust-toolchain.toml", "[toolchain\nchannel ="),
        ("rust-toolchain.toml", "[toolchain]\nchannel = \"--help\"\n"),
    ] {
        assert!(parse_pin(file, text).is_err(), "{file}: {text:?}");
    }
}

#[test]
fn channels_resolve_to_installed_names() {
    let installed = BTreeSet::from(["my-build".to_owned()]);
    assert_eq!(
        resolve("1.88.0", HOST, &installed),
        "1.88.0-aarch64-apple-darwin"
    );
    assert_eq!(
        resolve("1.88", HOST, &installed),
        "1.88-aarch64-apple-darwin"
    );
    assert_eq!(resolve("my-build", HOST, &installed), "my-build");

    assert_eq!(target_triple(OLD).as_deref(), Some(HOST));
    assert_eq!(
        target_triple("1.85-aarch64-apple-darwin").as_deref(),
        Some(HOST)
    );
    assert_eq!(
        target_triple("nightly-2025-01-01-x86_64-unknown-linux-gnu").as_deref(),
        Some("x86_64-unknown-linux-gnu")
    );
    assert_eq!(
        target_triple("1.90.0-beta.3-aarch64-apple-darwin").as_deref(),
        Some(HOST)
    );
    for name in ["my-build", "1.88.0", "stable", "update-hashes", "-rf-x-y"] {
        assert_eq!(target_triple(name), None, "{name}");
    }
}

#[test]
fn unreadable_pin_in_an_active_project_blocks_all_retirement() {
    let p = fixture().with_file(
        "~/work/app/rust-toolchain.toml",
        "[toolchain\nchannel =",
        t(399),
    );
    let err = propose(&p, "").unwrap_err();
    assert!(
        matches!(&err, AdapterError::Unavailable(m)
            if m.contains("/home/tester/work/app/rust-toolchain.toml")),
        "{err}"
    );

    // In an idle project the same file protects nothing and blocks nothing.
    let idle = fixture().with_file(
        "~/work/app/rust-toolchain.toml",
        "[toolchain\nchannel =",
        t(300),
    );
    assert_eq!(names(&propose(&idle, "").unwrap()), [OLD]);
}

#[test]
fn why_each_toolchain_was_kept_is_recorded() {
    let p = with_toolchain(fixture(), "1.96.1-aarch64-apple-darwin", t(200)).with_file(
        "~/work/app/rust-toolchain",
        "1.96.1",
        t(399),
    );
    let p = with_toolchain(p, "1.99.1-aarch64-apple-darwin", t(395));
    let (plan, log) = propose_logged(&p);
    assert_eq!(names(&plan.unwrap()), [OLD]);
    assert_eq!(
        reason(&log, DecisionKind::Kept, "nightly-aarch64-apple-darwin"),
        "the default toolchain"
    );
    assert!(
        reason(&log, DecisionKind::Kept, "stable-aarch64-apple-darwin").contains("stable channel")
    );
    assert_eq!(
        reason(&log, DecisionKind::Kept, "1.96.1-aarch64-apple-darwin"),
        "pinned by ~/work/app (last edited 1971-02-04)"
    );
    assert!(
        reason(&log, DecisionKind::Kept, "1.99.1-aarch64-apple-darwin")
            .starts_with("last used 1971-01-31")
    );
    let planned = log
        .iter()
        .find(|d| d.kind == DecisionKind::Planned)
        .unwrap();
    assert_eq!(planned.id, OLD_DIR);
    assert_eq!(planned.bytes, 1010);

    // A blocked run says, against each toolchain it would have offered, why not.
    let blocked = fixture().with_file("~/work/app/rust-toolchain.toml", "[toolchain", t(399));
    let (plan, log) = propose_logged(&blocked);
    assert!(plan.is_err());
    assert!(
        reason(&log, DecisionKind::Kept, OLD).contains("/home/tester/work/app/rust-toolchain.toml")
    );
    assert!(log.iter().all(|d| d.kind != DecisionKind::Planned));
}

#[test]
fn a_symlinked_pin_is_read_through_its_link() {
    // The target is outside the roots, so only the link can protect anything.
    let fake = with_toolchain(fixture(), "1.96.1-aarch64-apple-darwin", t(200))
        .with_file("/srv/shared/rust-toolchain", "1.93.1\n", t(399))
        .with_symlink(
            "~/work/app/rust-toolchain",
            "/srv/shared/rust-toolchain",
            t(399),
        );
    let linked = Tweaked::new(fake).link(
        "/home/tester/work/app/rust-toolchain",
        "/srv/shared/rust-toolchain",
    );
    assert_eq!(
        names(&propose(&linked, "").unwrap()),
        ["1.96.1-aarch64-apple-darwin"]
    );

    // A link that cannot be read is an unreadable pin: in an active project
    // it stops the run (the fake platform cannot read through any link).
    let dangling = fixture().with_symlink("~/work/app/rust-toolchain", "/srv/gone", t(399));
    assert!(matches!(
        propose(&dangling, ""),
        Err(AdapterError::Unavailable(m)) if m.contains("/home/tester/work/app/rust-toolchain")
    ));
}

#[test]
fn a_kept_projects_pin_still_protects() {
    let p = fixture()
        .with_file("~/work/kept/.disk-saver-keep", "", t(399))
        .with_file("~/work/kept/rust-toolchain", "1.93.1", t(399));
    assert!(propose(&p, "").unwrap().is_empty());
}

/// The fixture, with `~/Desktop`, `~/Documents` and `~/Downloads` present.
fn with_private_folders() -> FakePlatform {
    fixture()
        .with_dir("~/Desktop", t(10))
        .with_dir("~/Documents", t(10))
        .with_dir("~/Downloads", t(10))
}

#[test]
fn an_unreadable_folder_makes_every_candidate_ask_and_say_where() {
    let denied = Tweaked::new(with_private_folders())
        .deny("/home/tester/Documents")
        .deny("/home/tester/Desktop")
        .deny("/home/tester/Downloads");
    let plan = propose(&denied, "confirm = false").unwrap();
    assert_eq!(names(&plan), [OLD]);
    assert!(
        plan[0].requires_confirmation,
        "forced despite confirm = false"
    );
    assert_eq!(
        plan[0].label,
        "uninstall Rust 1.93.1-aarch64-apple-darwin: last used 1970-07-20; no project pins it; \
         pin scan incomplete: 3 folders unreadable (e.g. ~/Desktop)"
    );

    let one = Tweaked::new(with_private_folders()).deny("/home/tester/Documents");
    let plan = propose(&one, "confirm = false").unwrap();
    assert!(
        plan[0]
            .label
            .ends_with("; pin scan incomplete: 1 folder unreadable (~/Documents)")
    );

    // A readable tree leaves `confirm = false` alone.
    let plan = propose(&with_private_folders(), "confirm = false").unwrap();
    assert!(!plan[0].requires_confirmation);
}

#[test]
fn a_tree_deeper_than_max_depth_is_not_an_incomplete_scan() {
    // Like a vendored chart tree: far deeper than the walk goes, no project.
    let p = fixture().with_dir("~/work/infra/charts/a/charts/b/charts/c", t(10));
    let plan = propose(&p, "confirm = false\nmax_depth = 2").unwrap();
    assert_eq!(names(&plan), [OLD]);
    assert!(!plan[0].requires_confirmation);
    assert!(!plan[0].label.contains("incomplete"), "{}", plan[0].label);

    // And a pin below the limit is simply not looked for: `z` is four levels
    // below `~`, so it is read only from max_depth = 4.
    let deep = fixture().with_file("~/work/x/y/z/rust-toolchain", "1.93.1", t(399));
    assert_eq!(names(&propose(&deep, "max_depth = 3").unwrap()), [OLD]);
    assert!(propose(&deep, "max_depth = 4").unwrap().is_empty());
}

// ── rustup's own settings ───────────────────────────────────────────────────

#[test]
fn overrides_protect_while_their_directory_exists() {
    let overrides = settings(&format!("\"/home/tester/work/legacy\" = \"{OLD}\"\n"));
    let live = fixture()
        .with_file("~/.rustup/settings.toml", overrides.clone(), t(100))
        .with_dir("~/work/legacy", t(10));
    assert!(propose(&live, "").unwrap().is_empty());

    let gone = fixture().with_file("~/.rustup/settings.toml", overrides, t(100));
    let plan = propose(&gone, "").unwrap();
    assert!(
        plan[0]
            .label
            .ends_with("pinned only by ~/work/legacy (rustup override; directory gone)")
    );
}

#[test]
fn default_comes_from_settings_or_rustup_show() {
    let short = fixture().with_file(
        "~/.rustup/settings.toml",
        "version = \"12\"\ndefault_toolchain = \"1.93.1\"\n",
        t(100),
    );
    assert!(propose(&short, "").unwrap().is_empty());

    // No settings.toml: older rustup marks the default in `rustup show`.
    let shown = toolchains(
        FakePlatform::new()
            .with_file("~/.cargo/bin/rustup", "rustup", t(100))
            .with_command(
                RUSTUP,
                &["show"],
                CommandOutput::ok(format!(
                    "Default host: {HOST}\nrustup home:  /home/tester/.rustup\n\n{OLD} (default)\nrustc 1.93.1 (01f6ddf75 2026-02-11)\n"
                )),
            )
            .with_command_prefix("lsof", &["-t", "--"], no_open_files()),
    );
    assert!(propose(&shown, "").unwrap().is_empty());

    let garbled = fixture().with_file("~/.rustup/settings.toml", "default_toolchain = ", t(100));
    assert!(matches!(
        propose(&garbled, ""),
        Err(AdapterError::Unavailable(_))
    ));
}

#[test]
fn rustup_show_is_parsed_in_old_and_new_formats() {
    let new = parse_show(&show(LISTED).stdout_string()).unwrap();
    assert_eq!(new.host, HOST);
    assert_eq!(new.home, Some(PathBuf::from("/home/tester/.rustup")));
    assert_eq!(new.defaults, [format!("nightly-{HOST}")]);

    let old = parse_show(
        "Default host: x86_64-unknown-linux-gnu\nrustup home:  /root/.rustup\n\n\
         stable-x86_64-unknown-linux-gnu (default)\nrustc 1.75.0 (82e1608df 2023-12-21)\n",
    )
    .unwrap();
    assert_eq!(old.host, "x86_64-unknown-linux-gnu");
    assert_eq!(old.defaults, ["stable-x86_64-unknown-linux-gnu"]);

    assert!(parse_show("rustup home:  /root/.rustup\n").is_err());
}

#[test]
fn a_different_rustup_home_is_refused_and_rustup_home_is_honoured() {
    let reporting =
        |home: &str| CommandOutput::ok(format!("Default host: {HOST}\nrustup home:  {home}\n"));
    let elsewhere = toolchains(
        FakePlatform::new()
            .with_file("~/.cargo/bin/rustup", "rustup", t(100))
            .with_command(RUSTUP, &["show"], reporting("/elsewhere/.rustup")),
    );
    assert!(matches!(
        propose(&elsewhere, ""),
        Err(AdapterError::Unavailable(m)) if m.contains("/elsewhere/.rustup")
    ));

    let custom = with_toolchain_in(
        FakePlatform::new()
            .with_env("RUSTUP_HOME", "/home/tester/custom-rustup")
            .with_file("~/.cargo/bin/rustup", "rustup", t(100))
            .with_command(RUSTUP, &["show"], reporting("/home/tester/custom-rustup"))
            .with_command_prefix("lsof", &["-t", "--"], no_open_files()),
        "~/custom-rustup",
        "1.80.0-aarch64-apple-darwin",
        t(200),
    );
    let plan = propose(&custom, "").unwrap();
    assert_eq!(
        plan.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["/home/tester/custom-rustup/toolchains/1.80.0-aarch64-apple-darwin"]
    );
}

// ── evidence ────────────────────────────────────────────────────────────────

#[test]
fn foreign_toolchains_are_aged_like_any_other_but_say_so() {
    let p = with_toolchain(fixture(), "stable-x86_64-unknown-linux-gnu", t(200));
    let p = with_toolchain(p, "nightly-x86_64-unknown-linux-gnu", t(395));
    let p = with_toolchain(p, "1.90.0-x86_64-apple-darwin", t(200));
    let plan = propose(&p, "").unwrap();
    assert_eq!(
        names(&plan),
        [
            "1.90.0-x86_64-apple-darwin",
            OLD,
            "stable-x86_64-unknown-linux-gnu"
        ]
    );
    assert_eq!(
        label(&plan, "stable-x86_64-unknown-linux-gnu"),
        "uninstall Rust stable-x86_64-unknown-linux-gnu: built for x86_64-unknown-linux-gnu; \
         can't run on this machine; last used 1970-07-20; no project pins it"
    );
    assert!(
        label(&plan, "1.90.0-x86_64-apple-darwin")
            .contains("built for x86_64-apple-darwin (this machine is aarch64-apple-darwin)")
    );
}

#[test]
fn unknown_access_time_or_an_open_executable_blocks_retirement() {
    let unknown = fixture();
    unknown.set_accessed(format!("{OLD_DIR}/bin/cargo"), None);
    assert!(propose(&unknown, "").unwrap().is_empty());

    let running = toolchains(
        bare()
            .with_command_prefix(
                "lsof",
                &["-t", "--", &format!("{OLD_DIR}/bin/cargo")],
                CommandOutput::ok("4242\n"),
            )
            .with_command_prefix("lsof", &["-t", "--"], no_open_files()),
    );
    assert!(propose(&running, "").unwrap().is_empty());

    let no_lsof = toolchains(bare());
    assert!(propose(&no_lsof, "").unwrap().is_empty());
}

#[test]
fn linked_and_unrecognised_toolchains_are_never_offered() {
    let p = fixture()
        .with_symlink(
            "~/.rustup/toolchains/1.70.0-aarch64-apple-darwin",
            "~/src/rust/build/host/stage2",
            t(10),
        )
        .with_file("~/.rustup/toolchains/my-build/bin/rustc", "rustc", t(10))
        .with_file(
            "~/.rustup/toolchains/1.80.0-aarch64-apple-darwin/lib/libstd.rlib",
            "rlib",
            t(10),
        );
    assert_eq!(names(&propose(&p, "").unwrap()), [OLD]);
}

// ── approval ────────────────────────────────────────────────────────────────

#[test]
fn approval_rechecks_evidence_then_uninstalls_through_rustup() {
    let p = uninstall_ok(fixture(), OLD);
    let plan = propose(&p, "").unwrap();
    let outcomes = execute(&p, &plan);
    assert!(
        matches!(outcomes[..], [Outcome::Removed { bytes: 0, .. }]),
        "the fake uninstall keeps the tree, so no bytes can be claimed: {outcomes:?}"
    );
    let ran = p.commands_run();
    let last = ran.last().unwrap();
    assert_eq!(last.program, RUSTUP);
    assert_eq!(last.args, ["toolchain", "uninstall", OLD]);

    p.set_accessed(format!("{OLD_DIR}/bin/rustc"), Some(t(399)));
    let before = uninstalls(&p);
    assert!(skipped(&execute(&p, &plan)[0], "used or changed"));
    assert_eq!(uninstalls(&p), before);
    assert!(p.removed().is_empty(), "rustup deletes, never the adapter");
}

#[test]
fn approval_skips_what_became_protected_changed_or_unknowable() {
    let plan = propose(&fixture(), "").unwrap();

    let pinned =
        uninstall_ok(fixture(), OLD).with_file("~/work/app/rust-toolchain", "1.93.1", t(399));
    assert!(skipped(&execute(&pinned, &plan)[0], "pinned by ~/work/app"));

    let default = uninstall_ok(fixture(), OLD).with_file(
        "~/.rustup/settings.toml",
        format!("default_toolchain = \"{OLD}\"\n"),
        t(100),
    );
    assert!(skipped(
        &execute(&default, &plan)[0],
        "the default toolchain"
    ));

    let grown = uninstall_ok(fixture(), OLD).with_file(
        format!("{OLD_DIR}/lib/rustlib/src/lib.rs"),
        "src",
        t(200),
    );
    assert!(skipped(&execute(&grown, &plan)[0], "used or changed"));

    let broken = uninstall_ok(fixture(), OLD).with_file(
        "~/work/app/rust-toolchain.toml",
        "[toolchain",
        t(399),
    );
    assert!(skipped(
        &execute(&broken, &plan)[0],
        "cannot confirm nothing needs it"
    ));

    let gone = uninstall_ok(toolchains(FakePlatform::new()), OLD);
    assert!(skipped(&execute(&gone, &plan)[0], "rustup not found"));

    let stray = [Candidate::new(
        "/home/tester/work",
        "x",
        1,
        t(1),
        Class::Cache,
    )];
    assert!(skipped(
        &execute(&uninstall_ok(fixture(), OLD), &stray)[0],
        "not an installed rustup toolchain"
    ));

    for p in [pinned, default, grown, broken, gone] {
        assert_eq!(uninstalls(&p), 0);
    }
}

#[test]
fn an_unapproved_candidate_waits_when_the_pin_scan_has_become_incomplete() {
    let plan = propose(&fixture(), "confirm = false").unwrap();
    assert!(!plan[0].requires_confirmation);
    let p = Tweaked::new(uninstall_ok(with_private_folders(), OLD)).deny("/home/tester/Documents");
    let outcomes = execute_with(&p, "confirm = false", &plan);
    assert!(
        skipped(
            &outcomes[0],
            "pin scan incomplete: 1 folder unreadable (~/Documents)"
        ),
        "{outcomes:?}"
    );
    assert_eq!(uninstalls(&p.fake), 0);

    // An approved one goes ahead: the reviewer saw the note in its label.
    let approved = [plan[0].clone().confirm(true)];
    let outcomes = execute_with(&p, "", &approved);
    assert!(
        matches!(outcomes[..], [Outcome::Removed { .. }]),
        "{outcomes:?}"
    );
}

#[test]
fn a_failed_uninstall_is_reported() {
    let p = fixture().with_command(
        RUSTUP,
        &["toolchain", "uninstall", OLD],
        CommandOutput {
            status: 1,
            stdout: vec![],
            stderr: b"error: toolchain is in use\n".to_vec(),
        },
    );
    let plan = propose(&p, "").unwrap();
    assert!(matches!(
        &execute(&p, &plan)[..],
        [Outcome::Failed { error, .. }] if error.contains("toolchain is in use")
    ));
}

// ── config and words ────────────────────────────────────────────────────────

#[test]
fn bad_config_is_rejected_at_startup() {
    for cfg in [
        "min_age = \"40d\"",
        "exclude = [\"[\"]",
        "rustup = \"bin/rustup\"",
        "pin_max_idel = \"30d\"",
    ] {
        assert!(
            factory().build(Some(toml::from_str(cfg).unwrap())).is_err(),
            "{cfg}"
        );
    }
}

#[test]
fn dates_are_utc_calendar_days() {
    let day = |n: u64| t(n) + Duration::from_secs(23 * 3600);
    assert_eq!(date(t(0)), "1970-01-01");
    assert_eq!(date(day(200)), "1970-07-20");
    assert_eq!(date(t(11_016)), "2000-02-29");
    assert_eq!(date(t(20_749)), "2026-10-23");
    assert_eq!(date(t(47_541)), "2100-03-01");
    assert_eq!(date(UNIX_EPOCH - Duration::from_secs(1)), "before 1970");
}
