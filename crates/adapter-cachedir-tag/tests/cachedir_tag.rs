//! Integration tests for the `cachedir-tag` adapter, driven entirely against
//! [`FakePlatform`] (no real filesystem). The fake's clock reads day 400 and its
//! home is `/home/tester`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_cachedir_tag::factory;
use disk_saver_core::{
    Adapter, Candidate, Claims, Class, Ctx, DecisionLog, Outcome, Platform, Pressure, Store,
};
use disk_saver_platform::FakePlatform;

const DAY: u64 = 24 * 60 * 60;
const SCAVENGE: Pressure = Pressure::Scavenge { need: 1 };

/// What cargo writes into every target directory.
const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
    # This file is a cache directory tag created by cargo.\n\
    # For information about cache directory tags see https://bford.info/cachedir/\n";

/// What virtualenv writes into every environment (pre-commit's among them).
const VIRTUALENV_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
    # This file is a cache directory tag created by Python virtualenv.\n\
    # For information about cache directory tags, see:\n\
    #   https://bford.info/cachedir/";

/// What uv writes: the bare signature, no newline, no comment.
const BARE_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// A fixed `SystemTime` `days` days after the epoch.
fn day(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * DAY)
}

/// Build the adapter from a TOML snippet (`""` for the defaults).
fn adapter(toml: &str) -> Box<dyn Adapter> {
    let raw: toml::Value = toml::from_str(toml).unwrap();
    factory().build(Some(raw)).unwrap()
}

/// Whether the config in `toml` is rejected.
fn rejected(toml: &str) -> bool {
    let raw: toml::Value = toml::from_str(toml).unwrap();
    factory().build(Some(raw)).is_err()
}

/// A cargo target directory at `dir` holding 5,000 bytes of build output, with
/// everything in it last written at `at`.
fn cargo_target(fake: FakePlatform, dir: &str, at: SystemTime) -> FakePlatform {
    fake.with_file(format!("{dir}/CACHEDIR.TAG"), CARGO_TAG, at)
        .with_sized_dir(format!("{dir}/debug/deps"), 5_000, at)
        .with_dir(format!("{dir}/debug"), at)
        .with_dir(dir, at)
}

/// A directory at `dir` tagged with `tag`, last written at `at`.
fn tagged(fake: FakePlatform, dir: &str, tag: &str, at: SystemTime) -> FakePlatform {
    fake.with_file(format!("{dir}/CACHEDIR.TAG"), tag, at)
        .with_dir(dir, at)
}

/// The size of a [`cargo_target`].
const TARGET_BYTES: u64 = 5_000 + CARGO_TAG.len() as u64;

/// Run `plan`, returning the candidates and the paths the adapter claimed.
fn plan_claiming(
    fake: &FakePlatform,
    adapter: &mut Box<dyn Adapter>,
    pressure: Pressure,
    claims: &Claims,
) -> (Vec<Candidate>, Vec<PathBuf>) {
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(fake, store.bucket(adapter.name()), pressure, &log).with_claims(claims);
    let cands = adapter.plan(&mut ctx).unwrap();
    (cands, ctx.take_claimed())
}

/// Run `plan` with nothing claimed.
fn plan(fake: &FakePlatform, adapter: &mut Box<dyn Adapter>, pressure: Pressure) -> Vec<Candidate> {
    plan_claiming(fake, adapter, pressure, &Claims::new()).0
}

/// The ids `plan` proposes, sorted.
fn planned_ids(
    fake: &FakePlatform,
    adapter: &mut Box<dyn Adapter>,
    pressure: Pressure,
) -> Vec<String> {
    let mut ids: Vec<String> = plan(fake, adapter, pressure)
        .into_iter()
        .map(|c| c.id)
        .collect();
    ids.sort();
    ids
}

/// Run `execute` on `batch` with `claims` visible.
fn execute(
    fake: &FakePlatform,
    adapter: &mut Box<dyn Adapter>,
    batch: &[Candidate],
    claims: &Claims,
) -> Vec<Outcome> {
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx =
        Ctx::new(fake, store.bucket(adapter.name()), Pressure::Normal, &log).with_claims(claims);
    adapter.execute(&mut ctx, batch).unwrap()
}

/// A candidate for `dir` as a plan would have produced it, last used at `at`.
fn candidate(dir: &str, at: SystemTime) -> Candidate {
    Candidate::new(dir, "cache dir", TARGET_BYTES, at, Class::Cache)
}

fn assert_skipped(outcomes: &[Outcome], why: &str) {
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        Outcome::Skipped { reason, .. } => {
            assert!(reason.contains(why), "expected '{why}' in '{reason}'")
        }
        other => panic!("expected Skipped, got {other:?}"),
    }
}

// ── identification ─────────────────────────────────────────────────────────

#[test]
fn finds_a_cargo_target_outside_any_project_with_the_default_config() {
    // The case this adapter exists for: CARGO_TARGET_DIR pointed into ~/.cache.
    let fake = cargo_target(FakePlatform::new(), "~/.cache/debug-target", day(360));
    let mut a = factory().build(None).unwrap();

    let cands = plan(&fake, &mut a, Pressure::Normal);
    assert_eq!(cands.len(), 1, "{cands:?}");
    let c = &cands[0];
    assert_eq!(c.id, "/home/tester/.cache/debug-target");
    assert_eq!(c.label, "cache dir tagged by cargo: ~/.cache/debug-target");
    assert_eq!(c.bytes, TARGET_BYTES);
    assert_eq!(c.class, Class::Cache);
    assert_eq!(c.last_used, day(360));
    assert!(!c.requires_confirmation);
}

#[test]
fn a_bare_signature_is_a_valid_tag_with_no_creator() {
    let fake = tagged(FakePlatform::new(), "~/.cache/tool", BARE_TAG, day(300));
    let cands = plan(&fake, &mut adapter(""), Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert_eq!(
        cands[0].label,
        "cache dir with a CACHEDIR.TAG: ~/.cache/tool"
    );
}

#[test]
fn only_a_regular_file_starting_with_the_signature_is_a_tag() {
    let fake = FakePlatform::new()
        // Wrong signature, and an empty file.
        .with_file("~/.cache/wrong/CACHEDIR.TAG", "Signature: cargo", day(300))
        .with_file("~/.cache/empty/CACHEDIR.TAG", "", day(300))
        // The signature, but not at the start.
        .with_file(
            "~/.cache/late/CACHEDIR.TAG",
            format!("# hello\n{BARE_TAG}"),
            day(300),
        )
        // A directory, and a symlink to a valid tag elsewhere.
        .with_dir("~/.cache/dir/CACHEDIR.TAG", day(300))
        .with_file("~/elsewhere/CACHEDIR.TAG", BARE_TAG, day(300))
        .with_symlink(
            "~/.cache/link/CACHEDIR.TAG",
            "~/elsewhere/CACHEDIR.TAG",
            day(300),
        )
        // Not UTF-8 after the signature, and absurdly large.
        .with_file(
            "~/.cache/binary/CACHEDIR.TAG",
            [BARE_TAG.as_bytes(), &[0xff, 0xfe]].concat(),
            day(300),
        )
        .with_file(
            "~/.cache/huge/CACHEDIR.TAG",
            format!("{BARE_TAG}\n{}", "#".repeat(100_000)),
            day(300),
        )
        // Tagged under another name.
        .with_file("~/.cache/name/cachedir.tag", BARE_TAG, day(300));

    assert!(plan(&fake, &mut adapter(""), SCAVENGE).is_empty());
}

#[test]
fn virtualenvs_are_kept_whatever_skip_creators_says() {
    // pre-commit's hook environments: tagged by virtualenv, holding pyvenv.cfg.
    let venv = "~/.cache/pre-commit/repoabc/py_env-python3.14";
    let fake = tagged(FakePlatform::new(), venv, VIRTUALENV_TAG, day(300))
        .with_file(format!("{venv}/pyvenv.cfg"), "home = /usr/bin", day(300))
        // uv's venvs carry the bare signature.
        .with_file("~/.cache/uvenv/CACHEDIR.TAG", BARE_TAG, day(300))
        .with_file("~/.cache/uvenv/pyvenv.cfg", "home = /usr/bin", day(300));

    for toml in ["", "skip_creators = []"] {
        let (cands, claimed) = plan_claiming(&fake, &mut adapter(toml), SCAVENGE, &Claims::new());
        assert!(cands.is_empty(), "{toml}: {cands:?}");
        assert!(
            claimed.is_empty(),
            "a skipped venv is not ours: {claimed:?}"
        );
    }
}

/// uv's cache as uv lays it out: a bare tag and versioned buckets.
fn uv_cache(at: SystemTime) -> FakePlatform {
    tagged(FakePlatform::new(), "~/.cache/uv", BARE_TAG, at)
        .with_sized_dir("~/.cache/uv/archive-v0", 9_000, at)
        .with_dir("~/.cache/uv/wheels-v7", at)
        .with_file("~/.cache/uv/.lock", "", at)
}

#[test]
fn uvs_cache_is_left_to_the_uv_adapter_by_default() {
    let fake = uv_cache(day(300));
    assert!(plan(&fake, &mut adapter(""), SCAVENGE).is_empty());

    // An empty skip list un-skips it, and the label names uv.
    let cands = plan(&fake, &mut adapter("skip_creators = []"), SCAVENGE);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "/home/tester/.cache/uv");
    assert_eq!(cands[0].label, "cache dir tagged by uv: ~/.cache/uv");
}

#[test]
fn a_tag_comment_naming_uv_is_skipped_too() {
    let tag = format!("{BARE_TAG}\n# created by uv\n");
    let fake = tagged(FakePlatform::new(), "~/.cache/elsewhere-uv", &tag, day(300));
    assert!(plan(&fake, &mut adapter(""), SCAVENGE).is_empty());
}

#[test]
fn skip_creators_matches_case_insensitively_by_name_or_word() {
    let some_tool = format!("{BARE_TAG}\n# This cache was created by Some Tool.\n");
    let fake = cargo_target(FakePlatform::new(), "~/.cache/target", day(300)).with_file(
        "~/.cache/tool/CACHEDIR.TAG",
        some_tool,
        day(300),
    );

    let ids = |toml: &str| planned_ids(&fake, &mut adapter(toml), SCAVENGE);
    let target = "/home/tester/.cache/target".to_string();
    let tool = "/home/tester/.cache/tool".to_string();

    assert_eq!(ids(""), vec![target.clone(), tool.clone()]);
    assert_eq!(ids(r#"skip_creators = ["CARGO"]"#), vec![tool.clone()]);
    assert_eq!(ids(r#"skip_creators = ["tool"]"#), vec![target.clone()]);
    assert_eq!(ids(r#"skip_creators = ["some tool"]"#), vec![target]);
    assert_eq!(
        ids(r#"skip_creators = ["cargo", " Some "]"#),
        Vec::<String>::new()
    );
}

// ── claims ───────────────────────────────────────────────────────────────────

#[test]
fn paths_claimed_earlier_in_the_run_are_left_to_their_claimant() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/claimed", day(300));
    let fake = cargo_target(fake, "~/.cache/build/under-claim", day(300));
    let fake = cargo_target(fake, "~/.cache/free", day(300));

    let home = fake.home_dir();
    let mut claims = Claims::new();
    claims.insert(home.join(".cache/claimed"), "rust-target");
    claims.insert(home.join(".cache/build"), "node-modules");

    let (cands, claimed) = plan_claiming(&fake, &mut adapter(""), SCAVENGE, &claims);
    let ids: Vec<&str> = cands.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["/home/tester/.cache/free"]);
    assert_eq!(claimed, [home.join(".cache/free")]);
}

#[test]
fn claims_what_it_judges_by_age_even_when_kept() {
    // Idle 10 days: kept under Normal (30d), but still this adapter's call.
    let fake = cargo_target(FakePlatform::new(), "~/.cache/young", day(390));
    let (cands, claimed) = plan_claiming(&fake, &mut adapter(""), Pressure::Normal, &Claims::new());
    assert!(cands.is_empty());
    assert_eq!(claimed, [fake.home_dir().join(".cache/young")]);
}

// ── the walk ─────────────────────────────────────────────────────────────────

#[test]
fn a_tagged_root_is_not_itself_a_candidate() {
    let fake = tagged(FakePlatform::new(), "~/.cache/outer", BARE_TAG, day(300));
    let fake = cargo_target(fake, "~/.cache/outer/inner", day(300));

    let ids = planned_ids(
        &fake,
        &mut adapter(r#"roots = ["~/.cache/outer"]"#),
        SCAVENGE,
    );
    assert_eq!(ids, ["/home/tester/.cache/outer/inner"]);
}

#[test]
fn a_tagged_directory_inside_another_is_not_reported_separately() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/outer", day(300));
    let fake = cargo_target(fake, "~/.cache/outer/debug/nested", day(300));
    let ids = planned_ids(&fake, &mut adapter(""), SCAVENGE);
    assert_eq!(ids, ["/home/tester/.cache/outer"]);
}

#[test]
fn max_depth_bounds_the_walk() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/a/one", day(300));
    let fake = cargo_target(fake, "~/.cache/a/b/two", day(300));
    let fake = cargo_target(fake, "~/.cache/a/b/c/three", day(300));
    let fake = cargo_target(fake, "~/.cache/a/b/c/d/four", day(300));

    assert_eq!(
        planned_ids(&fake, &mut adapter("max_depth = 1"), SCAVENGE),
        ["/home/tester/.cache/a/one"]
    );
    // The default of 3 reads ~/.cache/a/b/c, so finds what is directly in it.
    assert_eq!(
        planned_ids(&fake, &mut adapter(""), SCAVENGE),
        [
            "/home/tester/.cache/a/b/c/three",
            "/home/tester/.cache/a/b/two",
            "/home/tester/.cache/a/one",
        ]
    );
}

#[test]
fn exclude_globs_prune_the_walk() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/keep-me", day(300));
    let fake = cargo_target(fake, "~/.cache/pinned/target", day(300));
    let fake = cargo_target(fake, "~/.cache/sweep-me", day(300));

    let toml = r#"exclude = ["**/keep-*", "/home/tester/.cache/pinned"]"#;
    assert_eq!(
        planned_ids(&fake, &mut adapter(toml), SCAVENGE),
        ["/home/tester/.cache/sweep-me"]
    );
}

#[test]
fn library_and_trash_are_never_walked() {
    let fake = cargo_target(FakePlatform::new(), "~/Library/Caches/tool", day(300));
    let fake = cargo_target(fake, "~/.Trash", day(300));
    let fake = cargo_target(fake, "~/.local/share/Trash/files/old", day(300));
    let fake = cargo_target(fake, "~/dev/out", day(300));

    assert_eq!(
        planned_ids(&fake, &mut adapter(r#"roots = ["~"]"#), SCAVENGE),
        ["/home/tester/dev/out"]
    );
    for root in ["~/Library/Caches", "~/.local/share/Trash/files"] {
        let toml = format!("roots = [\"{root}\"]");
        assert!(planned_ids(&fake, &mut adapter(&toml), SCAVENGE).is_empty());
    }
}

#[test]
fn the_user_cache_dir_is_walked_even_where_it_is_the_platform_cache_dir() {
    // On Linux `user_cache_dir` is ~/.cache, which the project adapters' shared
    // denylist drops. This adapter must not inherit that.
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(300));
    assert_eq!(fake.user_cache_dir(), fake.home_dir().join(".cache"));
    assert_eq!(plan(&fake, &mut adapter(""), SCAVENGE).len(), 1);
}

#[test]
fn honours_the_keep_sentinel() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/kept", day(300)).with_file(
        "~/.cache/kept/.disk-saver-keep",
        "",
        day(300),
    );
    let fake = cargo_target(fake, "~/.cache/tree/t", day(300)).with_file(
        "~/.cache/tree/.disk-saver-keep",
        "",
        day(300),
    );
    assert!(plan(&fake, &mut adapter(""), SCAVENGE).is_empty());
}

#[test]
fn never_proposes_the_home_directory_or_an_ancestor() {
    let fake = cargo_target(FakePlatform::new(), "~", day(300));
    assert!(plan(&fake, &mut adapter(r#"roots = ["/home"]"#), SCAVENGE).is_empty());
}

// ── age ─────────────────────────────────────────────────────────────────────

#[test]
fn age_comes_from_grandchildren_but_no_deeper() {
    // cargo's pattern: target/ and target/debug old, target/debug/deps bumped by
    // the last build, and a file deeper still that the probe must not reach.
    let fake = FakePlatform::new()
        .with_file("~/.cache/t/debug/deps/libx.rlib", "x", day(399))
        .with_file("~/.cache/t/CACHEDIR.TAG", CARGO_TAG, day(300))
        .with_dir("~/.cache/t/debug/deps", day(380))
        .with_dir("~/.cache/t/debug", day(300))
        .with_dir("~/.cache/t", day(300));

    let cands = plan(&fake, &mut adapter(""), SCAVENGE);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].last_used, day(380));

    // Idle 20 days by that measure: too young for Normal's 30.
    assert!(plan(&fake, &mut adapter(""), Pressure::Normal).is_empty());
}

#[test]
fn access_times_are_ignored() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(300));
    fake.set_accessed("~/.cache/t", Some(day(399)));
    fake.set_accessed("~/.cache/t/debug", Some(day(399)));
    fake.set_accessed("~/.cache/t/CACHEDIR.TAG", Some(day(399)));

    let cands = plan(&fake, &mut adapter(""), Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].last_used, day(300));
}

// ── pressure policy ──────────────────────────────────────────────────────────

#[test]
fn comfortable_plans_nothing_and_does_not_walk() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(1));
    let (cands, claimed) = plan_claiming(
        &fake,
        &mut adapter(""),
        Pressure::Comfortable,
        &Claims::new(),
    );
    assert!(cands.is_empty());
    // A walk would have found and claimed it, as Normal does.
    assert!(claimed.is_empty());
}

#[test]
fn normal_needs_max_age_and_scavenge_min_age() {
    let idle = |d: Duration| cargo_target(FakePlatform::new(), "~/.cache/t", day(400) - d);
    let mut a = adapter("");

    // 40 days: past the 30d max_age.
    assert_eq!(
        plan(
            &idle(Duration::from_secs(40 * DAY)),
            &mut a,
            Pressure::Normal
        )
        .len(),
        1
    );

    // 20 days: only under scavenge.
    let mid = idle(Duration::from_secs(20 * DAY));
    assert!(plan(&mid, &mut a, Pressure::Normal).is_empty());
    assert_eq!(plan(&mid, &mut a, SCAVENGE).len(), 1);

    // The 1d floor: 12 hours is inside it, 2 days past it.
    assert!(plan(&idle(Duration::from_secs(DAY / 2)), &mut a, SCAVENGE).is_empty());
    assert_eq!(
        plan(&idle(Duration::from_secs(2 * DAY)), &mut a, SCAVENGE).len(),
        1
    );
}

#[test]
fn ages_are_configurable_and_confirm_flags_candidates() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(395));
    let mut a = adapter("max_age = \"2d\"\nmin_age = \"1h\"\nconfirm = true");
    let cands = plan(&fake, &mut a, Pressure::Normal);
    assert_eq!(cands.len(), 1);
    assert!(cands[0].requires_confirmation);
}

// ── config ───────────────────────────────────────────────────────────────────

#[test]
fn bad_config_is_rejected_at_startup() {
    assert!(rejected("max_age = \"1d\"\nmin_age = \"2d\""));
    assert!(rejected(r#"exclude = ["[unterminated"]"#));
    assert!(rejected(r#"roots = ["relative/dir"]"#));
    assert!(rejected(r#"skip_creators = [" "]"#));
    // A typo in a safety key must not silently fall back to the default.
    assert!(rejected(r#"skip_creator = []"#));
    assert!(!rejected(r#"roots = ["~/.cache", "/srv/cache"]"#));
}

// ── execute ──────────────────────────────────────────────────────────────────

#[test]
fn execute_removes_a_planned_directory() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(300));
    let mut a = adapter("");
    let cands = plan(&fake, &mut a, Pressure::Normal);
    assert_eq!(cands.len(), 1);

    let outcomes = execute(&fake, &mut a, &cands, &Claims::new());
    match &outcomes[..] {
        [Outcome::Removed { id, bytes }] => {
            assert_eq!(id, "/home/tester/.cache/t");
            assert_eq!(*bytes, TARGET_BYTES);
        }
        other => panic!("expected one Removed, got {other:?}"),
    }
    assert!(!fake.exists("~/.cache/t"));
    assert!(fake.exists("~/.cache"));
    assert_eq!(fake.removed(), [PathBuf::from("/home/tester/.cache/t")]);
}

#[test]
fn execute_skips_a_directory_whose_tag_is_gone_or_broken() {
    let mut a = adapter("");
    let gone = FakePlatform::new()
        .with_sized_dir("~/.cache/t/debug", 5_000, day(300))
        .with_dir("~/.cache/t", day(300));
    let broken = FakePlatform::new()
        .with_file("~/.cache/t/CACHEDIR.TAG", "Signature: nope", day(300))
        .with_dir("~/.cache/t", day(300));

    for fake in [gone, broken] {
        let out = execute(
            &fake,
            &mut a,
            &[candidate("/home/tester/.cache/t", day(300))],
            &Claims::new(),
        );
        assert_skipped(&out, "valid CACHEDIR.TAG");
        assert!(fake.removed().is_empty());
    }
}

#[test]
fn execute_skips_a_vanished_directory_or_a_symlink() {
    let mut a = adapter("");
    let c = [candidate("/home/tester/.cache/t", day(300))];

    let vanished = FakePlatform::new();
    assert_skipped(
        &execute(&vanished, &mut a, &c, &Claims::new()),
        "no longer exists",
    );

    let link = cargo_target(FakePlatform::new(), "~/real", day(300)).with_symlink(
        "~/.cache/t",
        "~/real",
        day(300),
    );
    assert_skipped(
        &execute(&link, &mut a, &c, &Claims::new()),
        "no longer a directory",
    );
    assert!(link.removed().is_empty());
}

#[test]
fn execute_skips_a_directory_written_to_since_planning() {
    // Planned with last_used day 300; a build has since bumped debug/deps.
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(300))
        .with_dir("~/.cache/t/debug/incremental", day(399));
    let out = execute(
        &fake,
        &mut adapter(""),
        &[candidate("/home/tester/.cache/t", day(300))],
        &Claims::new(),
    );
    assert_skipped(&out, "written to since it was planned");
    assert!(fake.exists("~/.cache/t"));
}

#[test]
fn execute_skips_what_became_a_venv_or_a_skipped_creators() {
    let c = [candidate("/home/tester/.cache/t", day(300))];

    let venv = cargo_target(FakePlatform::new(), "~/.cache/t", day(300)).with_file(
        "~/.cache/t/pyvenv.cfg",
        "",
        day(300),
    );
    assert_skipped(
        &execute(&venv, &mut adapter(""), &c, &Claims::new()),
        "virtualenv",
    );

    // The config changed while the approval waited: cargo is skipped now.
    let target = cargo_target(FakePlatform::new(), "~/.cache/t", day(300));
    let mut a = adapter(r#"skip_creators = ["cargo"]"#);
    assert_skipped(
        &execute(&target, &mut a, &c, &Claims::new()),
        "created by cargo",
    );
    assert!(target.removed().is_empty());
}

#[test]
fn execute_skips_a_claimed_path() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(300));
    let mut claims = Claims::new();
    claims.insert(fake.home_dir().join(".cache"), "some-adapter");
    let out = execute(
        &fake,
        &mut adapter(""),
        &[candidate("/home/tester/.cache/t", day(300))],
        &claims,
    );
    assert_skipped(&out, "claimed by some-adapter");
}

#[test]
fn execute_skips_what_the_current_config_would_not_walk() {
    let fake = cargo_target(FakePlatform::new(), "~/.cache/t", day(300));
    let fake = cargo_target(fake, "~/.cache/kept/t", day(300)).with_file(
        "~/.cache/kept/.disk-saver-keep",
        "",
        day(300),
    );
    let fake = cargo_target(fake, "~/other/t", day(300));
    let mut a = adapter("");
    let run = |id: &str, a: &mut Box<dyn Adapter>| {
        execute(&fake, a, &[candidate(id, day(300))], &Claims::new())
    };

    assert_skipped(
        &run("/home/tester/other/t", &mut a),
        "no longer under a configured root",
    );
    assert_skipped(
        &run("/home/tester/.cache", &mut a),
        "no longer under a configured root",
    );
    assert_skipped(
        &run("/home/tester/.cache/kept/t", &mut a),
        ".disk-saver-keep",
    );
    assert_skipped(
        &run("/home/tester/.cache/../other/t", &mut a),
        "plain absolute path",
    );
    assert_skipped(&run(".cache/t", &mut a), "plain absolute path");

    let mut excluding = adapter(r#"exclude = ["**/.cache/t"]"#);
    assert_skipped(&run("/home/tester/.cache/t", &mut excluding), "is excluded");

    assert!(fake.removed().is_empty());
}
