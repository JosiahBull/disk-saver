//! Tests for the `orphaned-worktree` adapter. Everything but the last section
//! runs against [`FakePlatform`], with the evidence script's output scripted per
//! checkout; the last runs the real script against real `git`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_core::{
    Adapter, Candidate, Claims, Class, CommandOutput, Ctx, DecisionLog, Outcome, Platform,
    Pressure, RecordedDecision, Store, read_decisions,
};
use disk_saver_platform::FakePlatform;

use super::*;

const HOME: &str = "/home/tester";
/// The repository's git directory. Its work tree is `~/work/monorepo`.
const COMMON: &str = "/home/tester/work/monorepo/.git";
const SCAVENGE: Pressure = Pressure::Scavenge { need: 1 };

/// `n` days after the epoch. The fake clock reads day 400.
fn day(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(n * DAY)
}

/// `~/…` as an absolute path.
fn abs(path: &str) -> String {
    path.replacen('~', HOME, 1)
}

/// A fake home holding the repository `~/work/monorepo`.
fn world() -> FakePlatform {
    FakePlatform::new()
        .with_dir("~/work/monorepo/.git/objects", day(1))
        .with_file(
            "~/work/monorepo/.git/HEAD",
            "ref: refs/heads/main\n",
            day(1),
        )
        .with_file("~/work/monorepo/README.md", "hello", day(1))
}

/// Add a linked worktree of `~/work/monorepo` at `path`, last used at `active`,
/// whose `.git` names the admin directory `worktrees/<its name>`. Nothing
/// creates that admin directory, so it is orphaned.
fn orphan(fake: FakePlatform, path: &str, active: SystemTime) -> FakePlatform {
    let name = Path::new(path).file_name().unwrap().to_str().unwrap();
    gitfile(fake, path, &format!("{COMMON}/worktrees/{name}"))
        .with_sized_dir(format!("{path}/src"), 4_000, active)
        .with_file(format!("{path}/Cargo.toml"), "[package]", active)
}

/// Add `path/.git` reading `gitdir: <target>`.
fn gitfile(fake: FakePlatform, path: &str, target: &str) -> FakePlatform {
    fake.with_file(
        format!("{path}/.git"),
        format!("gitdir: {target}\n"),
        day(1),
    )
}

/// Register the worktree at `path`: create its admin directory.
fn register(fake: FakePlatform, path: &str) -> FakePlatform {
    let name = Path::new(path).file_name().unwrap().to_str().unwrap();
    fake.with_file(
        format!("{COMMON}/worktrees/{name}/gitdir"),
        format!("{}/.git\n", abs(path)),
        day(1),
    )
}

/// Script the evidence check's report for the checkout at `path`.
fn report(fake: FakePlatform, path: &str, stdout: &str) -> FakePlatform {
    reply(fake, path, CommandOutput::ok(stdout))
}

/// Script the evidence check's full output for the checkout at `path`.
fn reply(fake: FakePlatform, path: &str, out: CommandOutput) -> FakePlatform {
    let dir = abs(path);
    fake.with_command_prefix("sh", &["-c", EVIDENCE_SCRIPT, "sh", &dir], out)
}

const CLEAN: &str = "checked 12\n";

fn blob(c: char) -> String {
    c.to_string().repeat(40)
}

/// The adapter with `roots = ["~/work"]` plus `extra` config lines.
fn adapter(extra: &str) -> Box<dyn Adapter> {
    let raw: toml::Value = toml::from_str(&format!("roots = [\"~/work\"]\n{extra}")).unwrap();
    factory().build(Some(raw)).unwrap()
}

/// Run `plan`, returning the candidates and the paths claimed.
fn plan(
    fake: &FakePlatform,
    a: &mut Box<dyn Adapter>,
    store: &Store,
    pressure: Pressure,
) -> (Vec<Candidate>, Vec<PathBuf>) {
    plan_claimed(fake, a, store, pressure, &Claims::new())
}

fn plan_claimed(
    fake: &FakePlatform,
    a: &mut Box<dyn Adapter>,
    store: &Store,
    pressure: Pressure,
    claims: &Claims,
) -> (Vec<Candidate>, Vec<PathBuf>) {
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(fake, store.bucket(NAME), pressure, &log).with_claims(claims);
    let cands = a.plan(&mut ctx).unwrap();
    (cands, ctx.take_claimed())
}

fn execute(
    fake: &FakePlatform,
    a: &mut Box<dyn Adapter>,
    store: &Store,
    batch: &[Candidate],
) -> Vec<Outcome> {
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(fake, store.bucket(NAME), Pressure::Normal, &log);
    a.execute(&mut ctx, batch).unwrap()
}

fn ids(cands: &[Candidate]) -> Vec<&str> {
    let mut ids: Vec<&str> = cands.iter().map(|c| c.id.as_str()).collect();
    ids.sort();
    ids
}

/// Every checkout the evidence check ran against, in order.
fn checked(fake: &FakePlatform) -> Vec<String> {
    fake.commands_run()
        .into_iter()
        .filter(|c| c.program == "sh")
        .map(|c| c.args[3].clone())
        .collect()
}

fn skip_reason(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Skipped { reason, .. } => reason,
        other => panic!("expected Skipped, got {other:?}"),
    }
}

// ── discovery ────────────────────────────────────────────────────────────────

#[test]
fn finds_orphans_including_dot_named_ones() {
    let fake = orphan(world(), "~/work/feature-routing", day(380));
    let fake = orphan(fake, "~/work/.wt-prepare-latency", day(380));
    let fake = report(fake, "~/work/feature-routing", CLEAN);
    let fake = report(fake, "~/work/.wt-prepare-latency", CLEAN);
    let store = Store::open_in_memory().unwrap();

    let (cands, _) = plan(&fake, &mut adapter(""), &store, Pressure::Normal);

    assert_eq!(
        ids(&cands),
        [
            "/home/tester/work/.wt-prepare-latency",
            "/home/tester/work/feature-routing"
        ]
    );
    let c = cands
        .iter()
        .find(|c| c.id.ends_with("feature-routing"))
        .unwrap();
    assert_eq!(
        c.label,
        "orphaned worktree of ~/work/monorepo: ~/work/feature-routing \
         — every non-ignored file's content is in git (12 checked)"
    );
    assert_eq!(c.class, Class::UserData);
    assert_eq!(c.last_used, day(380));
    assert_eq!(c.bytes, fake.dir_size(Path::new(&c.id)).unwrap());
    assert!(c.bytes >= 4_000);
}

#[test]
fn a_registered_worktree_is_neither_proposed_nor_claimed() {
    let fake = register(
        orphan(world(), "~/work/bucket-fix", day(1)),
        "~/work/bucket-fix",
    );
    let store = Store::open_in_memory().unwrap();

    let (cands, claimed) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert!(cands.is_empty());
    assert!(claimed.is_empty());
    assert!(checked(&fake).is_empty());
}

#[test]
fn an_orphan_inside_a_registered_worktree_is_still_found() {
    let fake = register(orphan(world(), "~/work/live", day(1)), "~/work/live");
    let fake = orphan(fake, "~/work/live/old", day(1));
    let fake = report(fake, "~/work/live/old", CLEAN);
    let store = Store::open_in_memory().unwrap();

    let (cands, _) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert_eq!(ids(&cands), ["/home/tester/work/live/old"]);
}

#[test]
fn a_submodule_gitfile_is_not_a_worktree() {
    // `.git/modules/lib` does not exist, but it is not under `worktrees`.
    let fake = gitfile(world(), "~/work/app/lib", "../.git/modules/lib")
        .with_file("~/work/app/lib/lib.rs", "", day(1))
        .with_dir("~/work/app/.git/objects", day(1));
    let store = Store::open_in_memory().unwrap();

    let (cands, claimed) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert!(cands.is_empty());
    assert!(claimed.is_empty());
}

#[test]
fn a_relative_gitdir_is_resolved_against_the_checkout() {
    let fake = gitfile(world(), "~/work/rel", "../monorepo/.git/worktrees/rel").with_file(
        "~/work/rel/main.rs",
        "",
        day(1),
    );
    let fake = report(fake, "~/work/rel", CLEAN);
    let store = Store::open_in_memory().unwrap();

    let (cands, _) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert_eq!(ids(&cands), ["/home/tester/work/rel"]);
    assert!(
        cands[0]
            .label
            .starts_with("orphaned worktree of ~/work/monorepo: ~/work/rel")
    );
    // The script got `<common>` with `..` resolved.
    let run = &fake.commands_run()[0];
    assert_eq!(run.args[4], COMMON);
    assert_eq!(run.timeout, Some(CHECK_TIMEOUT));
}

#[test]
fn a_gitfile_that_is_not_a_gitdir_line_is_ignored() {
    let fake = world()
        .with_file("~/work/odd/.git", "not a gitfile\n", day(1))
        .with_file("~/work/empty/.git", "gitdir: \n", day(1));
    let store = Store::open_in_memory().unwrap();

    let (cands, claimed) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert!(cands.is_empty() && claimed.is_empty());
}

#[test]
fn agent_managed_worktrees_are_claimed_but_never_proposed() {
    // The walk never enters a dot-directory, so only a root inside one
    // reaches these.
    let fake = orphan(world(), "~/work/monorepo/.claude/worktrees/agent-a", day(1));
    let fake = orphan(fake, "~/.codex/worktrees/abc/monorepo", day(1));
    let cfg = r#"roots = ["~/work/monorepo/.claude/worktrees", "~/.codex/worktrees"]"#;
    let mut a = factory().build(Some(toml::from_str(cfg).unwrap())).unwrap();
    let store = Store::open_in_memory().unwrap();

    let (cands, mut claimed) = plan(&fake, &mut a, &store, SCAVENGE);

    assert!(cands.is_empty());
    claimed.sort();
    assert_eq!(
        claimed,
        [
            PathBuf::from("/home/tester/.codex/worktrees/abc/monorepo"),
            PathBuf::from("/home/tester/work/monorepo/.claude/worktrees/agent-a"),
        ]
    );
    assert!(checked(&fake).is_empty());
}

#[test]
fn checkouts_in_the_codex_app_workspace_are_never_proposed() {
    // The Codex app keeps task checkouts under ~/Documents/Codex, outside any
    // dot-directory, so the walk does reach them.
    let path = "~/Documents/Codex/2026-10-19/a-task/outputs/rust-libs";
    let fake = orphan(world(), path, day(1));
    let fake = report(fake, path, CLEAN);
    let store = Store::open_in_memory().unwrap();
    let raw: toml::Value = toml::from_str(r#"roots = ["~/Documents"]"#).unwrap();
    let mut a = factory().build(Some(raw)).unwrap();

    let (cands, claimed) = plan(&fake, &mut a, &store, SCAVENGE);

    assert!(cands.is_empty(), "{cands:?}");
    assert_eq!(
        claimed,
        [PathBuf::from(
            "/home/tester/Documents/Codex/2026-10-19/a-task/outputs/rust-libs"
        )]
    );
    assert!(checked(&fake).is_empty());
}

#[test]
fn a_checkout_holding_agent_worktrees_is_kept() {
    let fake = orphan(world(), "~/work/host", day(1)).with_file(
        "~/work/host/.claude/worktrees/agent-1/.git",
        "gitdir: x\n",
        day(1),
    );
    let fake = orphan(fake, "~/work/codex-host", day(1))
        .with_dir("~/work/codex-host/.codex/worktrees/run", day(1));
    // An empty `worktrees` directory protects nothing.
    let fake =
        orphan(fake, "~/work/plain", day(1)).with_dir("~/work/plain/.claude/worktrees", day(1));
    let fake = report(fake, "~/work/plain", CLEAN);
    let store = Store::open_in_memory().unwrap();

    let (cands, claimed) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert_eq!(ids(&cands), ["/home/tester/work/plain"]);
    assert_eq!(claimed.len(), 3, "kept orphans are still claimed");
}

#[test]
fn a_checkout_the_repository_still_lists_is_kept() {
    // `moved/.git` names a stale admin directory, but the repository lists
    // `moved` under another one: `git worktree repair` territory.
    let fake = gitfile(world(), "~/work/moved", &format!("{COMMON}/worktrees/old"))
        .with_file("~/work/moved/a.rs", "", day(1))
        .with_file(
            format!("{COMMON}/worktrees/new/gitdir"),
            "/home/tester/work/moved/.git\n",
            day(1),
        );
    // `outer` is orphaned but holds `inner`, which is registered.
    let fake = orphan(fake, "~/work/outer", day(1));
    let fake = register(
        orphan(fake, "~/work/outer/inner", day(1)),
        "~/work/outer/inner",
    );
    let store = Store::open_in_memory().unwrap();

    let (cands, claimed) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert!(cands.is_empty(), "{:?}", ids(&cands));
    assert_eq!(claimed.len(), 2);
    assert!(checked(&fake).is_empty());
}

#[test]
fn honours_exclude_and_the_keep_sentinel() {
    let fake = orphan(world(), "~/work/skipped", day(1));
    let fake =
        orphan(fake, "~/work/kept", day(1)).with_file("~/work/kept/.disk-saver-keep", "", day(1));
    let store = Store::open_in_memory().unwrap();

    let (cands, _) = plan(
        &fake,
        &mut adapter(r#"exclude = ["/home/tester/work/skipped"]"#),
        &store,
        SCAVENGE,
    );

    assert!(cands.is_empty());
}

// ── evidence ─────────────────────────────────────────────────────────────────

#[test]
fn parses_clean_and_unsaved_reports() {
    assert_eq!(
        parse_report("checked 3\n"),
        Some(Evidence::Clean { checked: 3 })
    );
    assert_eq!(
        parse_report("checked 0\n"),
        Some(Evidence::Clean { checked: 0 })
    );

    let text = format!(
        "checked 9\nmissing {} src/a b.rs\nmissing {} tab\there.txt\nnested vendor/lib/\n",
        blob('b'),
        "c".repeat(64)
    );
    let Some(Evidence::Unsaved { checked, lost }) = parse_report(&text) else {
        panic!("expected Unsaved");
    };
    assert_eq!(checked, 9);
    assert_eq!(
        lost,
        [
            Lost::Repo {
                path: "vendor/lib/".into()
            },
            Lost::File {
                path: "src/a b.rs".into(),
                blob: blob('b')
            },
            Lost::File {
                path: "tab\there.txt".into(),
                blob: "c".repeat(64)
            },
        ]
    );
}

#[test]
fn rejects_reports_the_script_never_prints() {
    for text in [
        "",
        "missing aaaa x\n",
        "checked three\n",
        "checked 1\nsomething else\n",
        "checked 1\nmissing nothex src/a.rs\n",
        &format!("checked 1\nmissing {}\n", blob('a')),
        &format!("checked 1\nmissing {} \n", blob('a')),
        "checked 1\nnested \n",
    ] {
        assert_eq!(parse_report(text), None, "{text:?}");
    }
}

#[test]
fn failure_names_the_step_and_the_last_stderr_line() {
    let out = |status, stderr: &str| CommandOutput {
        status,
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    };
    assert_eq!(
        failure(&out(4, "warning: CRLF\nfatal: bad object\n\n")),
        "the check failed: fatal: bad object"
    );
    assert_eq!(failure(&out(3, "")), "couldn't start the check");
    assert_eq!(
        failure(&out(137, "Killed")),
        "the check exited with status 137: Killed"
    );
}

#[test]
fn labels_carry_the_evidence() {
    let unsaved = format!(
        "checked 40\nmissing {b} packages/x.py\nmissing {b} src/y.rs\nmissing {b} z.md\nnested vendor/lib/\n",
        b = blob('d')
    );
    let mut fake = world();
    for name in ["clean", "unsaved", "failed", "garbled", "slow"] {
        fake = orphan(fake, &format!("~/work/{name}"), day(1));
    }
    let fake = report(fake, "~/work/clean", CLEAN);
    let fake = report(fake, "~/work/unsaved", &unsaved);
    let fake = reply(
        fake,
        "~/work/failed",
        CommandOutput {
            status: 4,
            stdout: b"checked 1\n".to_vec(),
            stderr: b"fatal: could not open 'secret.txt' for reading: Permission denied\n".to_vec(),
        },
    );
    let fake = report(fake, "~/work/garbled", "hello\n");
    // `slow` has no scripted reply: the fake reports the command as not found,
    // standing in for any error running it, a timeout included.
    let store = Store::open_in_memory().unwrap();

    let (cands, _) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    let label = |name: &str| {
        let id = format!("{HOME}/work/{name}");
        let c = cands.iter().find(|c| c.id == id).unwrap();
        c.label
            .strip_prefix(&format!(
                "orphaned worktree of ~/work/monorepo: ~/work/{name} — "
            ))
            .unwrap()
            .to_string()
    };
    assert_eq!(
        label("unsaved"),
        "3 files not in git and 1 nested repository not checked \
         (vendor/lib/, packages/x.py, src/y.rs, …)"
    );
    assert_eq!(
        label("failed"),
        "couldn't check for unsaved work: the check failed: \
         fatal: could not open 'secret.txt' for reading: Permission denied"
    );
    assert_eq!(
        label("garbled"),
        "couldn't check for unsaved work: the check printed something unexpected"
    );
    assert!(
        label("slow").starts_with("couldn't check for unsaved work: couldn't run the check:"),
        "{}",
        label("slow")
    );
}

#[test]
fn a_timed_out_check_is_unknown() {
    let fake =
        orphan(world(), "~/work/big", day(1)).with_command_error("sh", io::ErrorKind::TimedOut);
    let store = Store::open_in_memory().unwrap();

    let (cands, _) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    assert!(
        cands[0]
            .label
            .contains("couldn't check for unsaved work: couldn't run the check"),
        "{}",
        cands[0].label
    );
    assert!(cands[0].requires_confirmation);
}

#[test]
fn a_vanished_repository_is_unknown_without_running_git() {
    let fake = gitfile(
        world(),
        "~/work/stray",
        "/home/tester/old/.git/worktrees/stray",
    )
    .with_file("~/work/stray/a.rs", "", day(1));
    // A `<common>` that exists but holds no object store is no repository.
    let fake = gitfile(fake, "~/work/fake", "/home/tester/notrepo/worktrees/fake")
        .with_file("~/work/fake/a.rs", "", day(1))
        .with_dir("~/notrepo", day(1));
    let store = Store::open_in_memory().unwrap();

    let (cands, _) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    let labels: Vec<&str> = cands.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(
        &"orphaned worktree of ~/old: ~/work/stray \
          — couldn't check for unsaved work: its repository ~/old is gone"
    ));
    assert!(labels.contains(
        &"orphaned worktree of ~/notrepo: ~/work/fake \
          — couldn't check for unsaved work: ~/notrepo is not a git repository"
    ));
    assert!(checked(&fake).is_empty());
}

// ── confirmation ─────────────────────────────────────────────────────────────

#[test]
fn only_clean_evidence_escapes_confirmation_and_only_when_asked() {
    let unsaved = format!("checked 2\nmissing {} a.rs\n", blob('e'));
    let mut fake = world();
    for name in ["clean", "unsaved", "unknown", "empty"] {
        fake = orphan(fake, &format!("~/work/{name}"), day(1));
    }
    let fake = report(fake, "~/work/clean", CLEAN);
    let fake = report(fake, "~/work/unsaved", &unsaved);
    let fake = reply(
        fake,
        "~/work/unknown",
        CommandOutput {
            status: 4,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );
    let fake = report(fake, "~/work/empty", "checked 0\n");
    let flagged = |a: &mut Box<dyn Adapter>| {
        let store = Store::open_in_memory().unwrap();
        let (cands, _) = plan(&fake, a, &store, SCAVENGE);
        let mut f: Vec<(String, bool)> = cands
            .iter()
            .map(|c| {
                (
                    c.id.rsplit('/').next().unwrap().to_string(),
                    c.requires_confirmation,
                )
            })
            .collect();
        f.sort();
        f
    };

    let pairs = |v: &[(&str, bool)]| -> Vec<(String, bool)> {
        v.iter().map(|(n, b)| (n.to_string(), *b)).collect()
    };
    // The default asks about everything.
    assert_eq!(
        flagged(&mut adapter("")),
        pairs(&[
            ("clean", true),
            ("empty", true),
            ("unknown", true),
            ("unsaved", true)
        ])
    );
    // `confirm = false` lets through only a clean check that checked something.
    assert_eq!(
        flagged(&mut adapter("confirm = false")),
        pairs(&[
            ("clean", false),
            ("empty", true),
            ("unknown", true),
            ("unsaved", true)
        ])
    );
}

// ── age policy ───────────────────────────────────────────────────────────────

#[test]
fn age_policy_follows_pressure_and_gates_the_check() {
    let fake = orphan(world(), "~/work/one-day", day(399));
    let fake = orphan(fake, "~/work/five-days", day(395));
    let fake = orphan(fake, "~/work/twenty-days", day(380));
    let fake = report(fake, "~/work/five-days", CLEAN);
    let fake = report(fake, "~/work/twenty-days", CLEAN);
    let mut a = adapter("");
    let run = |a: &mut Box<dyn Adapter>, pressure| {
        let store = Store::open_in_memory().unwrap();
        let (cands, claimed) = plan(&fake, a, &store, pressure);
        (
            ids(&cands)
                .iter()
                .map(|id| id.rsplit('/').next().unwrap().to_string())
                .collect::<Vec<_>>(),
            claimed.len(),
        )
    };

    // Comfortable: nothing walked, nothing checked.
    assert_eq!(run(&mut a, Pressure::Comfortable), (vec![], 0));
    assert!(checked(&fake).is_empty());

    // Normal: past max_age (14d) only — and only that one is checked.
    assert_eq!(
        run(&mut a, Pressure::Normal),
        (vec!["twenty-days".to_string()], 3)
    );
    assert_eq!(checked(&fake), ["/home/tester/work/twenty-days"]);

    // Scavenge: past min_age (3d).
    assert_eq!(
        run(&mut a, SCAVENGE).0,
        ["five-days".to_string(), "twenty-days".to_string()]
    );
}

#[test]
fn last_used_ignores_build_output_git_and_finder() {
    let fake = orphan(world(), "~/work/wt", day(300))
        .with_sized_dir("~/work/wt/target", 9, day(399))
        .with_sized_dir("~/work/wt/node_modules", 9, day(399))
        .with_sized_dir("~/work/wt/.venv", 9, day(399))
        .with_sized_dir("~/work/wt/__pycache__", 9, day(399))
        .with_file("~/work/wt/.DS_Store", "", day(399))
        .with_file(
            "~/work/wt/.git",
            "gitdir: /home/tester/work/monorepo/.git/worktrees/wt\n",
            day(399),
        );
    assert_eq!(
        last_used(&fake, Path::new("/home/tester/work/wt"), day(400)),
        day(300)
    );

    // With nothing else at the top level, the directory's own mtime.
    let bare = FakePlatform::new()
        .with_dir("~/work/bare", day(42))
        .with_sized_dir("~/work/bare/target", 9, day(399));
    assert_eq!(
        last_used(&bare, Path::new("/home/tester/work/bare"), day(400)),
        day(42)
    );
}

// ── claims ───────────────────────────────────────────────────────────────────

#[test]
fn claims_every_orphan_proposed_or_kept() {
    let fake = orphan(world(), "~/work/old", day(300));
    let fake = orphan(fake, "~/work/young", day(399));
    let fake = report(fake, "~/work/old", CLEAN);
    let store = Store::open_in_memory().unwrap();

    let (cands, mut claimed) = plan(&fake, &mut adapter(""), &store, Pressure::Normal);

    assert_eq!(ids(&cands), ["/home/tester/work/old"]);
    claimed.sort();
    assert_eq!(
        claimed,
        [
            PathBuf::from("/home/tester/work/old"),
            PathBuf::from("/home/tester/work/young")
        ]
    );
}

#[test]
fn leaves_a_checkout_another_adapter_claimed() {
    let fake = orphan(world(), "~/work/old", day(300));
    let store = Store::open_in_memory().unwrap();
    let mut claims = Claims::new();
    claims.insert(PathBuf::from("/home/tester/work"), "cachedir-tag");

    let (cands, claimed) = plan_claimed(&fake, &mut adapter(""), &store, SCAVENGE, &claims);

    assert!(cands.is_empty() && claimed.is_empty());
}

// ── execute ──────────────────────────────────────────────────────────────────

/// One orphan at `~/work/wt`, idle 100 days, whose check prints `stdout`.
fn single(stdout: &str) -> FakePlatform {
    report(orphan(world(), "~/work/wt", day(300)), "~/work/wt", stdout)
}

/// Plan against `before`, then execute the plan against `after`.
fn plan_then_execute(before: &FakePlatform, after: &FakePlatform) -> Outcome {
    let store = Store::open_in_memory().unwrap();
    let mut a = adapter("");
    let (cands, _) = plan(before, &mut a, &store, SCAVENGE);
    assert_eq!(cands.len(), 1);
    let mut out = execute(after, &mut a, &store, &cands);
    out.pop().unwrap()
}

#[test]
fn execute_removes_it_and_reports_the_bytes_measured_then() {
    let fake = single(CLEAN);
    let store = Store::open_in_memory().unwrap();
    let mut a = adapter("");
    let (cands, _) = plan(&fake, &mut a, &store, SCAVENGE);
    let key = baseline_key(&cands[0].id);
    assert!(store.bucket(NAME).get::<Baseline>(&key).unwrap().is_some());

    let out = execute(&fake, &mut a, &store, &cands);

    let Outcome::Removed { bytes, .. } = out[0] else {
        panic!("expected Removed, got {:?}", out[0]);
    };
    assert_eq!(bytes, cands[0].bytes);
    assert!(!fake.exists("~/work/wt"));
    assert_eq!(fake.removed(), [PathBuf::from("/home/tester/work/wt")]);
    assert!(store.bucket(NAME).get::<Baseline>(&key).unwrap().is_none());
}

#[test]
fn execute_skips_when_unsaved_work_appeared_after_a_clean_proposal() {
    let now_unsaved = format!("checked 12\nmissing {} notes.md\n", blob('a'));
    let out = plan_then_execute(&single(CLEAN), &single(&now_unsaved));
    assert_eq!(
        skip_reason(&out),
        "1 path not in git now, but every file was when it was proposed"
    );
}

#[test]
fn execute_requires_every_unsaved_path_to_have_been_shown() {
    let proposed = format!(
        "checked 5\nmissing {} a.rs\nmissing {} b.rs\n",
        blob('a'),
        blob('b')
    );

    // Fewer: the user saw both, one has since reached git.
    let fewer = format!("checked 5\nmissing {} a.rs\n", blob('a'));
    let out = plan_then_execute(&single(&proposed), &single(&fewer));
    assert!(matches!(out, Outcome::Removed { .. }), "{out:?}");

    // Same count, but one is a file nobody was shown.
    let swapped = format!(
        "checked 5\nmissing {} a.rs\nmissing {} c.rs\n",
        blob('a'),
        blob('c')
    );
    let out = plan_then_execute(&single(&proposed), &single(&swapped));
    assert_eq!(
        skip_reason(&out),
        "1 path not in git changed or appeared since it was proposed (c.rs)"
    );

    // Same path, new content: edited after it was shown.
    let edited = format!(
        "checked 5\nmissing {} a.rs\nmissing {} b.rs\n",
        blob('a'),
        blob('f')
    );
    let out = plan_then_execute(&single(&proposed), &single(&edited));
    assert!(skip_reason(&out).contains("(b.rs)"));

    // Clean now is always fine.
    let out = plan_then_execute(&single(&proposed), &single(CLEAN));
    assert!(matches!(out, Outcome::Removed { .. }), "{out:?}");
}

#[test]
fn execute_after_an_unknown_proposal() {
    let unknown = || {
        reply(
            orphan(world(), "~/work/wt", day(300)),
            "~/work/wt",
            CommandOutput {
                status: 4,
                stdout: Vec::new(),
                stderr: b"fatal: oops\n".to_vec(),
            },
        )
    };
    // Still unknown is what was approved.
    let out = plan_then_execute(&unknown(), &unknown());
    assert!(matches!(out, Outcome::Removed { .. }), "{out:?}");

    // Unsaved files are news the label never showed.
    let unsaved = format!("checked 3\nmissing {} a.rs\n", blob('a'));
    let out = plan_then_execute(&unknown(), &single(&unsaved));
    assert_eq!(
        skip_reason(&out),
        "1 path not in git now, which its proposal couldn't show"
    );

    // Proposed with evidence, now unknown: can't confirm nothing was added.
    let proposed = format!("checked 3\nmissing {} a.rs\n", blob('a'));
    let out = plan_then_execute(&single(&proposed), &unknown());
    assert_eq!(
        skip_reason(&out),
        "couldn't re-check it for unsaved work: the check failed: fatal: oops"
    );
}

#[test]
fn execute_rechecks_what_it_is_removing() {
    let reason = |after: FakePlatform| {
        let out = plan_then_execute(&single(CLEAN), &after);
        assert!(after.removed().is_empty(), "nothing may be removed");
        skip_reason(&out).to_string()
    };

    assert_eq!(
        reason(register(single(CLEAN), "~/work/wt")),
        "it is registered with its repository again"
    );
    assert_eq!(reason(world()), "it no longer exists");
    assert_eq!(
        reason(world().with_dir("~/elsewhere", day(1)).with_symlink(
            "~/work/wt",
            "~/elsewhere",
            day(1)
        )),
        "it is now a symlink"
    );
    assert_eq!(
        reason(single(CLEAN).with_file("~/work/.disk-saver-keep", "", day(1))),
        "/home/tester/work/.disk-saver-keep exempts it"
    );
    assert_eq!(
        reason(single(CLEAN).with_file("~/work/wt/notes.md", "", day(399))),
        "it has been used since it was proposed"
    );
    assert_eq!(
        reason(single(CLEAN).with_dir("~/work/wt/.claude/worktrees/agent", day(1))),
        "it holds agent-managed worktrees in .claude/worktrees"
    );
    assert_eq!(
        reason(single(CLEAN).with_file("~/work/wt/.git", "gitdir: ../.git/modules/wt\n", day(1))),
        "its .git no longer points at a worktree admin directory"
    );
}

#[test]
fn execute_needs_the_baseline_the_label_was_built_from() {
    let fake = single(CLEAN);
    let store = Store::open_in_memory().unwrap();
    let mut a = adapter("");
    let (cands, _) = plan(&fake, &mut a, &store, SCAVENGE);

    // A label that is not the one recorded: a later plan (a dry run, say)
    // replaced the baseline this approval was shown.
    let mut stale = cands[0].clone();
    stale.label = format!("{} (older)", stale.label);
    let out = execute(&fake, &mut a, &store, &[stale]);
    assert_eq!(
        skip_reason(&out[0]),
        "its evidence changed after it was proposed; the next run proposes it again"
    );

    // No baseline at all.
    let fresh = Store::open_in_memory().unwrap();
    let out = execute(&fake, &mut a, &fresh, &cands);
    assert!(skip_reason(&out[0]).starts_with("no record of the evidence"));
    assert!(fake.removed().is_empty());
}

#[test]
fn execute_refuses_paths_it_would_never_propose() {
    let fake = single(CLEAN).with_file(
        "~/work/monorepo/.claude/worktrees/x/.git",
        "gitdir: y\n",
        day(1),
    );
    let store = Store::open_in_memory().unwrap();
    let mut a = adapter(r#"exclude = ["/home/tester/work/wt"]"#);
    let made_up = |id: &str| Candidate::new(id, "x", 1, day(1), Class::UserData);

    let out = execute(
        &fake,
        &mut a,
        &store,
        &[
            made_up("/home/tester/work/monorepo/.claude/worktrees/x"),
            made_up("/home/tester/work/wt"),
            made_up("relative/path"),
            made_up("/home/tester/.cache/wt"),
        ],
    );

    assert_eq!(
        out.iter().map(skip_reason).collect::<Vec<_>>(),
        [
            "agent-managed: it is under a .claude directory",
            "an `exclude` glob covers it",
            "relative/path is not an absolute path",
            "it is in a directory disk-saver never cleans",
        ]
    );
    assert!(fake.removed().is_empty());
}

// ── decisions ────────────────────────────────────────────────────────────────

/// The `(kind, reason)` recorded for `~/work/<name>`, in order.
fn why(decisions: &[RecordedDecision], name: &str) -> Vec<(String, String)> {
    let id = format!("{HOME}/work/{name}");
    decisions
        .iter()
        .filter(|d| d.id == id)
        .map(|d| {
            (
                d.kind.as_str().to_string(),
                d.reason.clone().unwrap_or_default(),
            )
        })
        .collect()
}

#[test]
fn records_why_each_checkout_was_kept_planned_removed_or_skipped() {
    let fake = register(orphan(world(), "~/work/live", day(1)), "~/work/live");
    let fake = orphan(fake, "~/work/young", day(398));
    let fake =
        orphan(fake, "~/work/host", day(1)).with_dir("~/work/host/.claude/worktrees/agent", day(1));
    let fake = orphan(fake, "~/work/old", day(300));
    let fake = report(fake, "~/work/old", CLEAN);
    let fake = orphan(fake, "~/work/changed", day(300));
    let fake = report(fake, "~/work/changed", CLEAN);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.db");
    let log = DecisionLog::open(&path).unwrap();
    let store = Store::open_in_memory().unwrap();
    let mut a = adapter("");

    let mut ctx = Ctx::new(&fake, store.bucket(NAME), SCAVENGE, &log);
    let cands = a.plan(&mut ctx).unwrap();
    let mut changed = cands
        .iter()
        .find(|c| c.id.ends_with("changed"))
        .unwrap()
        .clone();
    changed.label.push_str(" (stale)");
    let old = cands
        .iter()
        .find(|c| c.id.ends_with("old"))
        .unwrap()
        .clone();
    a.execute(&mut ctx, &[old, changed]).unwrap();
    drop(ctx);

    let decisions = read_decisions(&path).unwrap();
    let pair = |kind: &str, reason: &str| (kind.to_string(), reason.to_string());
    assert_eq!(
        why(&decisions, "live"),
        [pair("kept", "still registered with ~/work/monorepo")]
    );
    assert_eq!(
        why(&decisions, "young"),
        [pair("kept", "idle 2d, under the 3d threshold")]
    );
    assert_eq!(
        why(&decisions, "host"),
        [pair(
            "kept",
            "it holds agent-managed worktrees in .claude/worktrees"
        )]
    );
    assert_eq!(
        why(&decisions, "old"),
        [pair("planned", ""), pair("deleted", "")]
    );
    assert_eq!(
        why(&decisions, "changed"),
        [
            pair("planned", ""),
            pair(
                "skipped",
                "its evidence changed after it was proposed; the next run proposes it again"
            )
        ]
    );
    let deleted = decisions
        .iter()
        .find(|d| d.kind.as_str() == "deleted")
        .unwrap();
    assert!(
        deleted
            .label
            .contains("every non-ignored file's content is in git")
    );
    assert!(deleted.bytes > 0);
}

// ── baselines ────────────────────────────────────────────────────────────────

#[test]
fn a_large_unsaved_set_is_recorded_as_a_digest() {
    let lost: Vec<Lost> = (0..=MAX_RECORDED_LOST)
        .map(|i| Lost::File {
            path: format!("f{i}"),
            blob: blob('a'),
        })
        .collect();
    let evidence = Evidence::Unsaved {
        checked: 1,
        lost: lost.clone(),
    };
    let recorded = Recorded::of(&evidence);
    assert!(matches!(recorded, Recorded::Unsaved { lost: None, .. }));

    // Unchanged passes; any change does not, even a smaller set.
    assert_eq!(regression(&recorded, &evidence), None);
    let fewer = Evidence::Unsaved {
        checked: 1,
        lost: lost[1..].to_vec(),
    };
    assert_eq!(
        regression(&recorded, &fewer).as_deref(),
        Some("the files not in git changed since it was proposed")
    );
}

#[test]
fn digest_is_stable() {
    // Stored in KV across upgrades, so it must never change.
    assert_eq!(digest(&[]), "cbf29ce484222325");
    assert_eq!(digest(&["a".to_string()]), "089bdc07b544e7b2");
    assert_eq!(
        digest(&["missing aaaa x".to_string(), "nested y/".to_string()]),
        "af49a227c7cad010"
    );
    assert_ne!(
        digest(&["ab".to_string()]),
        digest(&["a".to_string(), "b".to_string()])
    );
}

#[test]
fn stale_baselines_expire() {
    let fake = single(CLEAN);
    let store = Store::open_in_memory().unwrap();
    let bucket = store.bucket(NAME);
    let old = Baseline {
        label: "gone".into(),
        recorded_at: unix(day(300)),
        evidence: Recorded::Clean,
    };
    bucket
        .set("planned:/home/tester/work/gone", &old, day(300))
        .unwrap();
    bucket
        .set(
            "planned:/home/tester/work/junk",
            &"not a baseline",
            day(399),
        )
        .unwrap();

    let (cands, _) = plan(&fake, &mut adapter(""), &store, SCAVENGE);

    let keys = store.bucket(NAME).keys(BASELINE_PREFIX).unwrap();
    assert_eq!(keys, [baseline_key(&cands[0].id)]);
}

// ── configuration ────────────────────────────────────────────────────────────

#[test]
fn config_defaults_and_validation() {
    let cfg = WorktreeConfig::default();
    assert_eq!(cfg.roots, [PathBuf::from("~")]);
    assert_eq!(cfg.max_depth, 8);
    assert_eq!(cfg.max_age, Duration::from_secs(14 * DAY));
    assert_eq!(cfg.min_age, None);
    assert!(cfg.confirm);
    let built = build(cfg, &ConfigCx::new(NAME)).ok().unwrap();
    assert_eq!(built.policy.min_age, Duration::from_secs(3 * DAY));
    assert!(factory().build(None).is_ok());

    let parse = |text: &str| factory().build(Some(toml::from_str(text).unwrap()));
    // Our 3-day floor gives way to a shorter max_age…
    assert!(parse(r#"max_age = "1d""#).is_ok());
    // …but a floor the user wrote above it is a mistake.
    let err = parse("max_age = \"1d\"\nmin_age = \"2d\"").err().unwrap();
    assert!(err.to_string().contains("min_age"), "{err}");
    let err = parse(r#"exclude = ["["]"#).err().unwrap();
    assert!(err.to_string().contains("exclude"), "{err}");
}

// ── helpers ──────────────────────────────────────────────────────────────────

#[test]
fn reads_gitfiles_the_way_git_does() {
    let fake = world()
        .with_file(
            "~/w/crlf/.git",
            "gitdir: /r/.git/worktrees/crlf\r\n",
            day(1),
        )
        .with_file(
            "~/w/dots/.git",
            "gitdir: ./../../r/.git/./worktrees/dots",
            day(1),
        )
        .with_file(
            "~/w/bare/.git",
            "gitdir: /srv/r.git/worktrees/bare\n",
            day(1),
        )
        .with_file("~/w/nospace/.git", "gitdir:/r/.git/worktrees/x\n", day(1))
        .with_dir("~/w/dir/.git", day(1));
    let link = |name: &str| read_link(&fake, &Path::new(HOME).join("w").join(name));

    let crlf = link("crlf").unwrap();
    assert_eq!(crlf.admin, PathBuf::from("/r/.git/worktrees/crlf"));
    assert_eq!(crlf.common, PathBuf::from("/r/.git"));
    assert_eq!(crlf.repo(), Path::new("/r"));
    assert_eq!(
        link("dots").unwrap().admin,
        PathBuf::from("/home/tester/r/.git/worktrees/dots")
    );
    assert_eq!(link("bare").unwrap().repo(), Path::new("/srv/r.git"));
    assert!(link("nospace").is_none());
    assert!(link("dir").is_none());
}

#[test]
fn tilde_and_humanize() {
    let home = Path::new(HOME);
    assert_eq!(tilde(home, Path::new("/home/tester")), "~");
    assert_eq!(tilde(home, Path::new("/home/tester/work/x")), "~/work/x");
    assert_eq!(tilde(home, Path::new("/srv/x")), "/srv/x");
    assert_eq!(tilde(home, Path::new("/home/testerx")), "/home/testerx");
    assert_eq!(humanize(Duration::from_secs(5 * 3600)), "5h");
    assert_eq!(humanize(Duration::from_secs(50 * 3600)), "2d");
}

// ── the real script ──────────────────────────────────────────────────────────

/// Run `git` for test setup, isolated from the machine's git config (a global
/// LFS filter would otherwise take over `git add`) and from any repository the
/// test process sits inside.
///
/// A git hook exports `GIT_DIR`, `GIT_INDEX_FILE` and friends, and the
/// pre-commit hook runs this suite: inherited, they would aim `git init`,
/// `add` and `commit` here at the repository being committed to rather than
/// the scratch one. So every `GIT_*` variable is dropped first.
fn git(dir: &Path, args: &[&str]) {
    let mut cmd = std::process::Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    let status = cmd
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// The evidence script against real git: a worktree with a new file, a
/// modified file, ignored, empty and duplicate files, a symlink, a nested
/// repository and a Git LFS file, whose admin directory is then deleted.
#[test]
fn the_script_reports_exactly_the_unsaved_files() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: git is not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    let write = |path: &Path, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };

    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo.join("src/a.txt"), "one\n");
    write(&repo.join("src/b.txt"), "two\n");
    write(&repo.join(".gitignore"), "*.log\n");
    write(
        &repo.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    );
    // With no LFS filter configured, `git add` stores this pointer verbatim,
    // which is what git-lfs would have stored for "lfs payload\n".
    write(
        &repo.join("big.bin"),
        "version https://git-lfs.github.com/spec/v1\n\
         oid sha256:8cbc820853c0da042dd8f2526edd6beab441d1c5353aa7db6fa35e656bc2b189\n\
         size 12\n",
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    git(&repo, &["worktree", "add", "-q", "../wt", "-b", "feat"]);

    let wt = root.join("wt");
    write(&wt.join("big.bin"), "lfs payload\n"); // as git-lfs would check it out
    write(&wt.join("src/new.txt"), "new\n");
    write(&wt.join("src/b.txt"), "two\nchanged\n");
    write(&wt.join("debug.log"), "ignored\n");
    write(&wt.join("empty.txt"), "");
    write(&wt.join("deep/copy-of-a.txt"), "one\n");
    write(&wt.join("odd name\twith tab.txt"), "odd\n");
    std::os::unix::fs::symlink("src", wt.join("link")).unwrap();
    std::fs::create_dir(wt.join("nested")).unwrap();
    git(&wt.join("nested"), &["init", "-q"]);
    write(&wt.join("nested/n.txt"), "n\n");
    std::fs::remove_dir_all(repo.join(".git/worktrees/wt")).unwrap();

    let p = disk_saver_platform::RealPlatform::new();
    let link = read_link(&p, &wt).unwrap();
    assert!(admin_missing(&p, &link));
    let Evidence::Unsaved { checked, lost } = evidence(&p, &wt, &link) else {
        panic!("expected Unsaved");
    };

    let paths: Vec<&str> = lost.iter().map(Lost::path).collect();
    assert_eq!(
        paths,
        [
            "nested/",
            "odd name\twith tab.txt",
            "src/b.txt",
            "src/new.txt"
        ]
    );
    // .gitattributes, .gitignore, big.bin, deep/copy-of-a.txt, the tab file,
    // src/{a,b,new}.txt. Not the ignored, empty, symlinked or nested ones.
    assert_eq!(checked, 8);
    assert!(!repo.join(".git/lfs").exists(), "git-lfs must not have run");

    // A file name with a newline cannot be checked, so nothing is known.
    write(&wt.join("bad\nname"), "x\n");
    let Evidence::Unknown { reason } = evidence(&p, &wt, &link) else {
        panic!("expected Unknown");
    };
    assert_eq!(reason, "the check failed: a file name contains a newline");
}
