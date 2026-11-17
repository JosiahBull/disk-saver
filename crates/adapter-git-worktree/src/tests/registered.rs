//! Tests for listed worktrees: checkouts their repository still lists, judged
//! by their branch. Everything but the last section runs against
//! [`FakePlatform`], with git, the forge CLIs and the evidence scripted per
//! checkout; the last runs the real check against real `git`.

use super::*;

/// The repository's `origin`.
const URL: &str = "git@gitlab.example.com:team/monorepo.git";
const MERGED: &str = r#"[{"id":1,"iid":7,"state":"merged"}]"#;
const CLOSED: &str = r#"[{"id":1,"iid":7,"state":"closed"}]"#;
const OPEN: &str = r#"[{"id":1,"iid":7,"state":"merged"},{"id":2,"iid":9,"state":"opened"}]"#;
/// The admin directory a worktree at `path` is listed under.
fn admin(path: &str) -> String {
    let name = Path::new(path).file_name().unwrap().to_str().unwrap();
    format!("{COMMON}/worktrees/{name}")
}

/// Add a worktree of `~/work/monorepo` at `path`, on `branch` (detached when
/// `None`) and last used at `active`: its `.git` file, the admin directory
/// naming it back, and a couple of files.
fn worktree(
    fake: FakePlatform,
    path: &str,
    branch: Option<&str>,
    active: SystemTime,
) -> FakePlatform {
    let admin = admin(path);
    let head = match branch {
        Some(b) => format!("ref: refs/heads/{b}\n"),
        None => format!("{}\n", blob('a')),
    };
    fake.with_file(format!("{path}/.git"), format!("gitdir: {admin}\n"), day(1))
        .with_file(
            format!("{admin}/gitdir"),
            format!("{}/.git\n", abs(path)),
            day(1),
        )
        .with_file(format!("{admin}/HEAD"), head, active)
        .with_file(format!("{admin}/index"), "DIRC", active)
        .with_sized_dir(format!("{path}/src"), 4_000, active)
        .with_file(format!("{path}/Cargo.toml"), "[package]", active)
}

/// Script `branch`'s remote: no `branch.<name>.remote`, and `origin` at `url`.
fn remote(fake: FakePlatform, path: &str, branch: &str, url: &str) -> FakePlatform {
    let dir = abs(path);
    let key = format!("branch.{branch}.remote");
    fake.with_command(
        "git",
        &["-C", &dir, "config", "--get", &key],
        CommandOutput {
            status: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    )
    .with_command(
        "git",
        &["-C", &dir, "remote", "get-url", "origin"],
        CommandOutput::ok(format!("{url}\n")),
    )
}

/// Script what GitLab says about the branch checked out at `path`, in the
/// repository at [`URL`].
fn gitlab(fake: FakePlatform, path: &str, branch: &str, json: &str) -> FakePlatform {
    let endpoint = format!(
        "projects/team%2Fmonorepo/merge_requests?source_branch={}&state=all&per_page=100",
        branch.replace('/', "%2F")
    );
    remote(fake, path, branch, URL).with_command(
        "glab",
        &["api", "--hostname", "gitlab.example.com", &endpoint],
        CommandOutput::ok(json),
    )
}

/// A worktree at `path` on `branch` whose MR is in `json`, last used at
/// `active`, with nothing uncommitted.
fn on_gitlab(
    fake: FakePlatform,
    path: &str,
    branch: &str,
    json: &str,
    active: SystemTime,
) -> FakePlatform {
    let fake = worktree(fake, path, Some(branch), active);
    report(gitlab(fake, path, branch, json), path, CLEAN)
}

/// Plan once against a fresh store and adapter.
fn plan_once(fake: &FakePlatform, extra: &str, pressure: Pressure) -> Vec<Candidate> {
    let store = Store::open_in_memory().unwrap();
    plan(fake, &mut adapter(extra), &store, pressure).0
}

fn only(cands: &[Candidate]) -> &Candidate {
    assert_eq!(cands.len(), 1, "{cands:#?}");
    &cands[0]
}

/// Every program run, in order.
fn programs(fake: &FakePlatform) -> Vec<String> {
    fake.commands_run().into_iter().map(|c| c.program).collect()
}

// ── discovery ────────────────────────────────────────────────────────────────

#[test]
fn finds_worktrees_through_their_repository_including_dot_directories() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(300));
    let fake = on_gitlab(
        fake,
        "~/work/monorepo/.claude/worktrees/agent",
        "agent",
        MERGED,
        day(300),
    );
    let fake = on_gitlab(fake, "~/work/.wt-dotted", "dotted", MERGED, day(300));

    let cands = plan_once(&fake, "", Pressure::Normal);
    assert_eq!(
        ids(&cands),
        [
            abs("~/work/.wt-dotted"),
            abs("~/work/feat"),
            abs("~/work/monorepo/.claude/worktrees/agent"),
        ]
    );
}

#[test]
fn a_bare_repository_lists_its_worktrees_too() {
    let fake = FakePlatform::new()
        .with_dir("~/work/bare.git/objects", day(1))
        .with_file("~/work/bare.git/HEAD", "ref: refs/heads/main\n", day(1))
        .with_file(
            "~/work/bare.git/worktrees/wt/gitdir",
            format!("{}/.git\n", abs("~/work/elsewhere/wt")),
            day(1),
        )
        .with_file(
            "~/work/bare.git/worktrees/wt/HEAD",
            "ref: refs/heads/wt\n",
            day(300),
        )
        .with_file(
            "~/work/elsewhere/wt/.git",
            format!("gitdir: {}\n", abs("~/work/bare.git/worktrees/wt")),
            day(1),
        )
        .with_file("~/work/elsewhere/wt/a.txt", "a", day(300));
    let fake = report(
        gitlab(fake, "~/work/elsewhere/wt", "wt", MERGED),
        "~/work/elsewhere/wt",
        CLEAN,
    );

    let cands = plan_once(&fake, "", Pressure::Normal);
    assert_eq!(ids(&cands), [abs("~/work/elsewhere/wt")]);
    assert!(
        only(&cands)
            .label
            .starts_with("worktree of ~/work/bare.git: ")
    );
}

#[test]
fn only_checkouts_under_the_roots_and_in_scope_are_considered() {
    let fake = on_gitlab(world(), "~/elsewhere/outside", "outside", MERGED, day(300));
    let fake = on_gitlab(fake, "~/work/skipped", "skipped", MERGED, day(300));
    let fake = on_gitlab(fake, "~/work/kept/wt", "kept", MERGED, day(300)).with_file(
        "~/work/kept/.disk-saver-keep",
        "",
        day(1),
    );
    let fake = on_gitlab(fake, "~/work/in", "in", MERGED, day(300));

    let cands = plan_once(
        &fake,
        &format!("exclude = [\"{}\"]", abs("~/work/skipped")),
        Pressure::Normal,
    );
    assert_eq!(ids(&cands), [abs("~/work/in")]);
    // Nothing out of scope was asked about or checked.
    let dirs: Vec<String> = fake
        .commands_run()
        .into_iter()
        .filter_map(|c| c.args.get(1).cloned())
        .collect();
    assert!(
        dirs.iter()
            .all(|d| !d.contains("outside") && !d.contains("skipped"))
    );
}

#[test]
fn a_listing_its_checkout_does_not_name_back_is_skipped() {
    let fake = on_gitlab(world(), "~/work/real", "real", MERGED, day(300));
    // A second listing claims the same checkout, whose .git names `real`.
    let fake = fake
        .with_file(
            format!("{COMMON}/worktrees/ghost/gitdir"),
            format!("{}/.git\n", abs("~/work/real")),
            day(1),
        )
        .with_file(
            format!("{COMMON}/worktrees/gone/gitdir"),
            format!("{}/.git\n", abs("~/work/gone")),
            day(1),
        );

    let cands = plan_once(&fake, "", Pressure::Normal);
    assert_eq!(ids(&cands), [abs("~/work/real")]);
}

#[test]
fn the_main_checkout_is_never_a_candidate() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(300));
    let cands = plan_once(&fake, "", SCAVENGE);
    assert!(
        cands.iter().all(|c| c.id != abs("~/work/monorepo")),
        "{cands:#?}"
    );
}

// ── whether it is done ───────────────────────────────────────────────────────

#[test]
fn a_merged_clean_worktree_asks_unless_confirm_is_off() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));

    let asked = plan_once(&fake, "", Pressure::Normal);
    assert!(only(&asked).requires_confirmation);
    assert_eq!(only(&asked).class, Class::UserData);

    let unasked = plan_once(&fake, "confirm = false", Pressure::Normal);
    assert!(!only(&unasked).requires_confirmation);
}

#[test]
fn uncommitted_work_always_asks() {
    let fake = worktree(world(), "~/work/feat", Some("feat"), day(398));
    let fake = gitlab(fake, "~/work/feat", "feat", MERGED);
    let fake = report(
        fake,
        "~/work/feat",
        &format!("checked 1\nmissing {} notes.md\n", blob('b')),
    );
    let cands = plan_once(&fake, "confirm = false", Pressure::Normal);
    assert!(only(&cands).requires_confirmation);
}

#[test]
fn a_worktree_younger_than_merged_age_runs_nothing() {
    let fake = on_gitlab(
        world(),
        "~/work/feat",
        "feat",
        MERGED,
        day(400) - Duration::from_secs(3600),
    );
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());
    assert!(fake.commands_run().is_empty());

    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(397));
    assert!(plan_once(&fake, "merged_age = \"4d\"", SCAVENGE).is_empty());
    assert_eq!(plan_once(&fake, "merged_age = \"2d\"", SCAVENGE).len(), 1);
}

#[test]
fn a_scavenge_floor_below_merged_age_still_reaches_unmerged_ones() {
    let extra = "merged_age = \"2d\"\nmin_age = \"1h\"";
    let closed = on_gitlab(world(), "~/work/feat", "feat", CLOSED, day(399));
    assert_eq!(plan_once(&closed, extra, SCAVENGE).len(), 1);

    let merged = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(399));
    assert!(plan_once(&merged, extra, SCAVENGE).is_empty());
}

#[test]
fn the_admin_index_counts_as_activity() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(300)).with_file(
        format!("{}/index", admin("~/work/feat")),
        "DIRC",
        day(400) - Duration::from_secs(3600),
    );
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());
}

#[test]
fn an_open_request_keeps_it_however_old() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", OPEN, day(1));
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());
    assert!(
        !programs(&fake).contains(&"evidence".to_string()),
        "no evidence check"
    );
}

#[test]
fn anything_unmerged_waits_for_the_retention_policy_and_always_asks() {
    for json in [CLOSED, "[]"] {
        let young = on_gitlab(world(), "~/work/feat", "feat", json, day(390));
        assert!(plan_once(&young, "confirm = false", Pressure::Normal).is_empty());

        let old = on_gitlab(world(), "~/work/feat", "feat", json, day(380));
        let cands = plan_once(&old, "confirm = false", Pressure::Normal);
        assert!(only(&cands).requires_confirmation, "{json}");

        // Scavenge lowers the threshold to `min_age`.
        let scavenged = on_gitlab(world(), "~/work/feat", "feat", json, day(396));
        assert_eq!(plan_once(&scavenged, "", SCAVENGE).len(), 1, "{json}");
    }
}

#[test]
fn no_answer_from_the_forge_is_treated_as_unmerged() {
    let fake = worktree(world(), "~/work/feat", Some("feat"), day(380));
    let fake = remote(fake, "~/work/feat", "feat", URL)
        .with_command_error("glab", io::ErrorKind::NotFound);
    let fake = report(fake, "~/work/feat", CLEAN);

    let cands = plan_once(&fake, "confirm = false", Pressure::Normal);
    let c = only(&cands);
    assert!(c.requires_confirmation);
    assert!(
        c.label
            .contains("couldn't look up its MR or PR: glab is not installed"),
        "{}",
        c.label
    );
}

#[test]
fn a_detached_head_is_looked_up_nowhere() {
    let fake = worktree(world(), "~/work/detached", None, day(380));
    let fake = report(fake, "~/work/detached", CLEAN);

    let cands = plan_once(&fake, "", Pressure::Normal);
    assert_eq!(
        only(&cands).label,
        "worktree of ~/work/monorepo: ~/work/detached at a detached HEAD — no branch to look up — nothing uncommitted"
    );
    assert_eq!(programs(&fake), ["evidence"]);
}

#[test]
fn commits_only_a_detached_head_holds_keep_it() {
    let fake = worktree(world(), "~/work/detached", None, day(1));
    let fake = reply(
        fake,
        "~/work/detached",
        CommandOutput {
            status: 5,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());
}

// ── guardrails ───────────────────────────────────────────────────────────────

#[test]
fn a_claude_worktree_is_proposed_only_once_merged() {
    let path = "~/work/monorepo/.claude/worktrees/agent";
    let closed = on_gitlab(world(), path, "agent", CLOSED, day(1));
    assert!(plan_once(&closed, "", SCAVENGE).is_empty());

    let merged = on_gitlab(world(), path, "agent", MERGED, day(398));
    assert_eq!(plan_once(&merged, "", Pressure::Normal).len(), 1);
}

#[test]
fn codex_worktrees_are_never_proposed() {
    let fake = on_gitlab(
        world(),
        "~/work/monorepo/.codex/worktrees/task",
        "task",
        MERGED,
        day(1),
    );
    let fake = on_gitlab(
        fake,
        "~/Documents/Codex/2026-01-01/task/wt",
        "wt",
        MERGED,
        day(1),
    );
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());

    let everywhere = configured("roots = [\"~\"]");
    let store = Store::open_in_memory().unwrap();
    assert!(
        plan(&fake, &mut { everywhere }, &store, SCAVENGE)
            .0
            .is_empty()
    );
    assert!(fake.commands_run().is_empty());
}

#[test]
fn a_checkout_inside_another_repositorys_work_tree_is_never_proposed() {
    // A vendored import: a worktree of a bare cache inside the monorepo's own
    // git directory, checked out inside the monorepo's work tree.
    let cache = "~/work/monorepo/.git/vendor/lib.git";
    let fake = world()
        .with_dir(format!("{cache}/objects"), day(1))
        .with_file(
            format!("{cache}/worktrees/lib/gitdir"),
            format!("{}/.git\n", abs("~/work/monorepo/imports/lib")),
            day(1),
        )
        .with_file(
            format!("{cache}/worktrees/lib/HEAD"),
            format!("{}\n", blob('a')),
            day(1),
        )
        .with_file(
            "~/work/monorepo/imports/lib/.git",
            format!("gitdir: {}/worktrees/lib\n", abs(cache)),
            day(1),
        )
        .with_file("~/work/monorepo/imports/lib/lib.rs", "", day(1));
    let fake = report(fake, "~/work/monorepo/imports/lib", CLEAN);
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());
    assert!(fake.commands_run().is_empty());
}

/// The bare cache a vendoring tool checks imports out of.
const CACHE: &str = "~/work/monorepo/.git/vendor/lib.git";

/// Add an import of the cache's `lib` at `<host>/imports/lib`, detached, as a
/// worktree of [`CACHE`] listed under `name`.
fn import(fake: FakePlatform, host: &str, name: &str) -> FakePlatform {
    let admin = format!("{}/worktrees/{name}", abs(CACHE));
    let dir = format!("{host}/imports/lib");
    fake.with_dir(format!("{CACHE}/objects"), day(1))
        .with_file(
            format!("{admin}/gitdir"),
            format!("{}/.git\n", abs(&dir)),
            day(1),
        )
        .with_file(format!("{admin}/HEAD"), format!("{}\n", blob('e')), day(1))
        .with_file(format!("{dir}/.git"), format!("gitdir: {admin}\n"), day(1))
        .with_file(format!("{dir}/lib.rs"), "", day(1))
}

#[test]
fn an_import_inside_a_worktree_goes_with_it() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));
    let fake = import(fake, "~/work/feat", "lib1");
    let fake = report(fake, "~/work/feat/imports/lib", CLEAN);
    let cands = plan_once(&fake, "", Pressure::Normal);
    assert_eq!(ids(&cands), [abs("~/work/feat")]);
    assert!(only(&cands).label.ends_with("— nothing uncommitted"));

    let outcome = plan_then_execute(&fake, &fake);
    assert!(matches!(outcome, Outcome::Removed { .. }), "{outcome:?}");
    assert_eq!(
        fake.removed(),
        [
            PathBuf::from(abs("~/work/feat")),
            PathBuf::from(admin("~/work/feat")),
            PathBuf::from(format!("{}/worktrees/lib1", abs(CACHE))),
        ]
    );
}

#[test]
fn an_imports_uncommitted_work_is_the_worktrees() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));
    let fake = import(fake, "~/work/feat", "lib1");
    let fake = report(
        fake,
        "~/work/feat/imports/lib",
        &format!("checked 1\nmissing {} lib.rs\n", blob('f')),
    );
    let cands = plan_once(&fake, "confirm = false", Pressure::Normal);
    let c = only(&cands);
    assert!(c.requires_confirmation);
    assert!(
        c.label
            .ends_with("— 1 uncommitted file (imports/lib/lib.rs)"),
        "{}",
        c.label
    );

    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));
    let fake = import(fake, "~/work/feat", "lib1");
    let fake = reply(
        fake,
        "~/work/feat/imports/lib",
        CommandOutput {
            status: 5,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );
    let cands = plan_once(&fake, "", Pressure::Normal);
    assert!(
        only(&cands).label.ends_with(&format!(
            "couldn't check for uncommitted work: imports/lib: {UNREACHABLE}"
        )),
        "{}",
        only(&cands).label
    );
}

#[test]
fn a_locked_worktree_is_never_proposed() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(1)).with_file(
        format!("{}/locked", admin("~/work/feat")),
        "on a USB disk",
        day(1),
    );
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());
}

#[test]
fn a_checkout_holding_another_worktree_is_kept() {
    let fake = on_gitlab(world(), "~/work/outer", "outer", MERGED, day(1));
    let fake = on_gitlab(fake, "~/work/outer/inner", "inner", MERGED, day(1));
    let cands = plan_once(&fake, "", SCAVENGE);
    assert_eq!(ids(&cands), [abs("~/work/outer/inner")]);

    let fake = on_gitlab(world(), "~/work/host", "host", MERGED, day(1))
        .with_dir("~/work/host/.claude/worktrees/x", day(1));
    assert!(plan_once(&fake, "", SCAVENGE).is_empty());
}

#[test]
fn claims_what_it_proposes_and_leaves_what_others_claimed() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(1));
    let fake = on_gitlab(fake, "~/work/live", "live", OPEN, day(1));
    let store = Store::open_in_memory().unwrap();
    let (cands, claimed) = plan(&fake, &mut adapter(""), &store, SCAVENGE);
    assert_eq!(ids(&cands), [abs("~/work/feat")]);
    assert_eq!(claimed, [PathBuf::from(abs("~/work/feat"))]);

    let mut claims = Claims::new();
    claims.insert(PathBuf::from(abs("~/work/feat")), "other");
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(1));
    let (cands, _) = plan_claimed(&fake, &mut adapter(""), &store, SCAVENGE, &claims);
    assert!(cands.is_empty());
    assert!(fake.commands_run().is_empty());
}

// ── labels ───────────────────────────────────────────────────────────────────

#[test]
fn labels_carry_the_branch_the_request_and_the_evidence() {
    let fake = on_gitlab(world(), "~/work/feat", "me/feat", MERGED, day(398));
    assert_eq!(
        only(&plan_once(&fake, "", Pressure::Normal)).label,
        "worktree of ~/work/monorepo: ~/work/feat on me/feat — MR !7 merged — nothing uncommitted"
    );

    let null = "0".repeat(40);
    let fake = worktree(world(), "~/work/feat", Some("feat"), day(398));
    let fake = gitlab(fake, "~/work/feat", "feat", MERGED);
    let fake = report(
        fake,
        "~/work/feat",
        &format!(
            "checked 4\nmissing {null} gone.rs\nmissing {} src/a.rs\nmissing {} src/b.rs\nnested vendor/\n",
            blob('b'),
            blob('c')
        ),
    );
    assert_eq!(
        only(&plan_once(&fake, "", Pressure::Normal)).label,
        "worktree of ~/work/monorepo: ~/work/feat on feat — MR !7 merged — \
         3 uncommitted files and 1 nested repository not checked (vendor/, gone.rs, src/a.rs, …)"
    );

    let fake = worktree(world(), "~/work/feat", Some("feat"), day(380));
    let fake = gitlab(fake, "~/work/feat", "feat", CLOSED);
    let fake = reply(
        fake,
        "~/work/feat",
        CommandOutput {
            status: 4,
            stdout: Vec::new(),
            stderr: b"fatal: bad object HEAD\n".to_vec(),
        },
    );
    assert_eq!(
        only(&plan_once(&fake, "", Pressure::Normal)).label,
        "worktree of ~/work/monorepo: ~/work/feat on feat — MR !7 closed without merging — \
         couldn't check for uncommitted work: fatal: bad object HEAD"
    );
}

// ── the forge ────────────────────────────────────────────────────────────────

#[test]
fn a_host_in_forges_is_asked_with_that_forge() {
    let fake = worktree(world(), "~/work/feat", Some("feat"), day(398));
    let fake = remote(
        fake,
        "~/work/feat",
        "feat",
        "git@Code.Example.org:team/monorepo.git",
    );
    let fake = fake.with_command(
        "glab",
        &[
            "api",
            "--hostname",
            "code.example.org",
            "projects/team%2Fmonorepo/merge_requests?source_branch=feat&state=all&per_page=100",
        ],
        CommandOutput::ok(MERGED),
    );
    let fake = report(fake, "~/work/feat", CLEAN);
    let cands = plan_once(
        &fake,
        "forges = { \"Code.Example.ORG\" = \"gitlab\" }",
        Pressure::Normal,
    );
    assert!(only(&cands).label.contains("MR !7 merged"));
}

// ── doctor ───────────────────────────────────────────────────────────────────

/// Run the adapter's `check`, as `disk-saver doctor` does.
fn doctor(fake: &FakePlatform, extra: &str) -> Result<(), String> {
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut ctx = Ctx::new(fake, store.bucket(NAME), Pressure::Comfortable, &log);
    adapter(extra).check(&mut ctx).map_err(|e| e.to_string())
}

#[test]
fn doctor_asks_each_forge_host_once() {
    let fake = on_gitlab(world(), "~/work/a", "a", MERGED, day(400));
    let fake = on_gitlab(fake, "~/work/b", "b", OPEN, day(400));
    assert_eq!(doctor(&fake, ""), Ok(()));
    assert_eq!(
        programs(&fake).iter().filter(|p| *p == "glab").count(),
        1,
        "one lookup per host"
    );
}

#[test]
fn doctor_reports_a_host_it_cannot_ask() {
    let fake = worktree(world(), "~/work/a", Some("a"), day(400));
    let fake =
        remote(fake, "~/work/a", "a", URL).with_command_error("glab", io::ErrorKind::NotFound);
    assert_eq!(
        doctor(&fake, ""),
        Err("unavailable: can't tell which worktrees merged \
             (gitlab.example.com: glab is not installed)"
            .to_string())
    );

    let fake = worktree(world(), "~/work/a", Some("a"), day(400));
    let fake = remote(
        fake,
        "~/work/a",
        "a",
        "git@code.example.org:team/monorepo.git",
    );
    assert_eq!(
        doctor(&fake, ""),
        Err(
            "unavailable: can't tell which worktrees merged (code.example.org: \
             not a known forge; name it in `forges` (or map it to \"none\"))"
                .to_string()
        )
    );
    assert_eq!(
        doctor(&fake, "forges = { \"code.example.org\" = \"none\" }"),
        Ok(())
    );
}

#[test]
fn doctor_ignores_what_needs_no_lookup() {
    let fake = worktree(world(), "~/work/detached", None, day(400));
    let fake = fake.with_file(
        "~/work/old/.git",
        format!("gitdir: {}\n", admin("~/work/old")),
        day(1),
    );
    assert_eq!(doctor(&fake, ""), Ok(()));
    assert!(fake.commands_run().is_empty());
}

// ── config ───────────────────────────────────────────────────────────────────

#[test]
fn rejects_a_merged_age_above_max_age() {
    let raw: toml::Value = toml::from_str("merged_age = \"20d\"\nmax_age = \"14d\"").unwrap();
    let err = factory().build(Some(raw)).err().unwrap().to_string();
    assert!(err.contains("merged_age"), "{err}");
}

#[test]
fn rejects_an_unknown_forge() {
    let raw: toml::Value = toml::from_str("forges = { \"x.org\" = \"gitea\" }").unwrap();
    assert!(factory().build(Some(raw)).is_err());
}

// ── execute ──────────────────────────────────────────────────────────────────

#[test]
fn execute_removes_the_checkout_then_its_admin_directory() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));
    let outcome = plan_then_execute(&fake, &fake);
    let Outcome::Removed { id, bytes } = outcome else {
        panic!("expected Removed, got {outcome:?}");
    };
    assert_eq!(id, abs("~/work/feat"));
    assert!(bytes >= 4_000, "{bytes}");
    assert_eq!(
        fake.removed(),
        [
            PathBuf::from(abs("~/work/feat")),
            PathBuf::from(admin("~/work/feat"))
        ]
    );
}

#[test]
fn execute_skips_when_uncommitted_work_appeared() {
    let before = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));
    let after = worktree(world(), "~/work/feat", Some("feat"), day(398));
    let after = report(
        after,
        "~/work/feat",
        &format!("checked 1\nmissing {} new.rs\n", blob('d')),
    );
    let outcome = plan_then_execute(&before, &after);
    assert_eq!(
        skip_reason(&outcome),
        "1 path not in git now, but every file was when it was proposed"
    );
    assert!(after.removed().is_empty());
}

#[test]
fn execute_skips_what_changed_since_it_was_proposed() {
    let before = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));

    let used = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398)).with_file(
        format!("{}/index", admin("~/work/feat")),
        "DIRC",
        day(399),
    );
    assert_eq!(
        skip_reason(&plan_then_execute(&before, &used)),
        "it has been used since it was proposed"
    );

    let locked = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398)).with_file(
        format!("{}/locked", admin("~/work/feat")),
        "",
        day(1),
    );
    assert_eq!(
        skip_reason(&plan_then_execute(&before, &locked)),
        "it is locked with `git worktree lock`"
    );

    // The repository forgot it since.
    let forgotten = world()
        .with_file(
            "~/work/feat/.git",
            format!("gitdir: {}\n", admin("~/work/feat")),
            day(1),
        )
        .with_sized_dir("~/work/feat/src", 4_000, day(398));
    assert_eq!(
        skip_reason(&plan_then_execute(&before, &forgotten)),
        "its repository no longer lists it"
    );

    let detached = worktree(world(), "~/work/feat", Some("feat"), day(398));
    let detached = reply(
        detached,
        "~/work/feat",
        CommandOutput {
            status: 5,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );
    assert_eq!(
        skip_reason(&plan_then_execute(&before, &detached)),
        UNREACHABLE
    );
}

// ── forgotten and listed together ────────────────────────────────────────────

#[test]
fn one_walk_judges_forgotten_and_listed_worktrees() {
    let fake = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(398));
    let fake = report(
        orphan(fake, "~/work/old", day(300)),
        "~/work/old",
        "checked 3\n",
    );

    let cands = plan_once(&fake, "confirm = false", Pressure::Normal);
    let labels: Vec<&str> = cands.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "worktree of ~/work/monorepo: ~/work/feat on feat — MR !7 merged — nothing uncommitted",
            "worktree of ~/work/monorepo: ~/work/old — forgotten by its repository — \
             every non-ignored file's content is in git (3 checked)",
        ]
    );
    assert!(cands.iter().all(|c| !c.requires_confirmation));
}

#[test]
fn a_forgotten_checkout_takes_its_imports_with_it() {
    let fake = report(
        orphan(world(), "~/work/old", day(300)),
        "~/work/old",
        "checked 3\n",
    );
    let fake = import(fake, "~/work/old", "lib1");
    let fake = report(fake, "~/work/old/imports/lib", CLEAN);

    let outcome = plan_then_execute(&fake, &fake);
    assert!(matches!(outcome, Outcome::Removed { .. }), "{outcome:?}");
    // No admin directory of its own to remove; the import's goes.
    assert_eq!(
        fake.removed(),
        [
            PathBuf::from(abs("~/work/old")),
            PathBuf::from(format!("{}/worktrees/lib1", abs(CACHE))),
        ]
    );
}

#[test]
fn a_forgotten_import_inside_a_checkout_is_kept() {
    // The cache forgot the import, but the monorepo around it still needs it.
    let fake = world()
        .with_dir(format!("{CACHE}/objects"), day(1))
        .with_file(
            "~/work/monorepo/imports/lib/.git",
            format!("gitdir: {}/worktrees/lib\n", abs(CACHE)),
            day(1),
        )
        .with_file("~/work/monorepo/imports/lib/lib.rs", "", day(1));
    let store = Store::open_in_memory().unwrap();
    let (cands, claimed) = plan(&fake, &mut adapter(""), &store, SCAVENGE);
    assert!(cands.is_empty(), "{cands:#?}");
    assert_eq!(claimed, [PathBuf::from(abs("~/work/monorepo/imports/lib"))]);
    assert!(fake.commands_run().is_empty());
}

#[test]
fn execute_skips_a_worktree_whose_state_changed() {
    let listed = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(300));
    let forgotten_now = report(
        orphan(world(), "~/work/feat", day(300)),
        "~/work/feat",
        "checked 3\n",
    );
    assert_eq!(
        skip_reason(&plan_then_execute(&listed, &forgotten_now)),
        "its repository no longer lists it"
    );

    let listed_again = on_gitlab(world(), "~/work/feat", "feat", MERGED, day(300));
    let old = report(
        orphan(world(), "~/work/feat", day(300)),
        "~/work/feat",
        "checked 3\n",
    );
    assert_eq!(
        skip_reason(&plan_then_execute(&old, &listed_again)),
        "it is registered with its repository again"
    );
}

// ── the status check ─────────────────────────────────────────────────────────

/// `git <args>` as the status check runs it, scripted to answer `out`.
fn git_answers(fake: FakePlatform, args: &[&str], out: CommandOutput) -> FakePlatform {
    let mut all = vec!["--no-optional-locks", "-c", "core.fsmonitor=false"];
    all.extend_from_slice(args);
    fake.with_command_prefix("git", &all, out)
}

fn exit(status: i32, stderr: &str) -> CommandOutput {
    CommandOutput {
        status,
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn the_status_check_names_what_failed() {
    let dir = PathBuf::from(abs("~/work/feat"));
    let unknown = |fake: &FakePlatform| match check(fake, &Worktree::at(fake, &dir).unwrap()) {
        Status::Checked(Evidence::Unknown { reason }) => reason,
        other => panic!("expected Unknown, got {other:?}"),
    };
    let on_branch = || worktree(world(), "~/work/feat", Some("feat"), day(1));

    let failing = git_answers(
        on_branch(),
        &["status"],
        exit(128, "fatal: index file corrupt\n"),
    );
    assert_eq!(
        unknown(&failing),
        "git status failed: fatal: index file corrupt"
    );

    let odd = git_answers(on_branch(), &["status"], CommandOutput::ok("?x\0"));
    assert_eq!(
        unknown(&odd),
        "git status printed a record it does not document"
    );

    let unreadable = on_branch().with_file(format!("{}/HEAD", admin("~/work/feat")), "?\n", day(1));
    assert_eq!(
        unknown(&unreadable),
        "couldn't read what it has checked out"
    );

    // Every git call runs in the checkout, never reading a hook's repository.
    let clean = git_answers(on_branch(), &["status"], CommandOutput::ok(""));
    assert_eq!(
        check(&clean, &Worktree::at(&clean, &dir).unwrap()),
        Status::Checked(Evidence::Clean { checked: 0 })
    );
    for run in clean.commands_run() {
        assert_eq!(run.dir.as_deref(), Some(dir.as_path()));
        for var in HOOK_VARIABLES {
            assert!(
                run.env.contains(&(var.to_string(), None)),
                "{var} is removed"
            );
        }
    }
}

#[test]
fn a_detached_head_nothing_holds_is_unreachable() {
    let dir = PathBuf::from(abs("~/work/feat"));
    let detached = || worktree(world(), "~/work/feat", None, day(1));
    let unheld = git_answers(detached(), &["for-each-ref"], CommandOutput::ok(""));
    let wt = Worktree::at(&unheld, &dir).unwrap();
    assert_eq!(check(&unheld, &wt), Status::Unreachable);

    let held = git_answers(
        detached(),
        &["for-each-ref"],
        CommandOutput::ok("refs/heads/main\n"),
    );
    let held = git_answers(held, &["status"], CommandOutput::ok(""));
    assert!(matches!(check(&held, &wt), Status::Checked(_)));
}

// ── the real check ───────────────────────────────────────────────────────────

/// The status check against real git: clean, then with every kind of
/// uncommitted change, then detached on a commit nothing else holds.
#[test]
fn the_check_reports_exactly_the_uncommitted_paths() {
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
    let wt = root.join("wt");
    let write = |path: &Path, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    let p = disk_saver_platform::RealPlatform::new();
    let paths = |evidence: &Evidence| match evidence {
        Evidence::Unsaved { lost, .. } => lost.iter().map(|l| l.key()).collect::<Vec<_>>(),
        other => panic!("expected Unsaved, got {other:?}"),
    };

    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo.join("src/a.txt"), "one\n");
    write(&repo.join("src/b.txt"), "two\n");
    write(&repo.join("gone.txt"), "bye\n");
    write(&repo.join(".gitignore"), "*.log\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    git(&repo, &["worktree", "add", "-q", "../wt", "-b", "feat"]);

    let worktree = Worktree::at(&p, &wt).unwrap();

    // A fresh checkout: nothing uncommitted, and the index left alone.
    let index = repo.join(".git/worktrees/wt/index");
    let before = std::fs::metadata(&index).unwrap().modified().unwrap();
    let Status::Checked(Evidence::Clean { checked: 0 }) = check(&p, &worktree) else {
        panic!("expected a clean checkout");
    };
    write(&wt.join("debug.log"), "ignored\n");
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(wt.join("src/a.txt"), "one\n").unwrap(); // touched, same content
    let Status::Checked(Evidence::Clean { .. }) = check(&p, &worktree) else {
        panic!("an ignored file and a touched file are not uncommitted work");
    };
    assert_eq!(
        std::fs::metadata(&index).unwrap().modified().unwrap(),
        before,
        "the check must not rewrite the index"
    );

    write(&wt.join("src/b.txt"), "two\nchanged\n");
    write(&wt.join("staged.txt"), "staged\n");
    git(&wt, &["add", "staged.txt"]);
    write(&wt.join("odd name\twith tab.txt"), "odd\n");
    std::fs::remove_file(wt.join("gone.txt")).unwrap();
    std::os::unix::fs::symlink("src", wt.join("link")).unwrap();
    std::fs::create_dir(wt.join("nested")).unwrap();
    git(&wt.join("nested"), &["init", "-q"]);
    write(&wt.join("nested/n.txt"), "n\n");

    let Status::Checked(evidence) = check(&p, &worktree) else {
        panic!("expected a report");
    };
    let id = |content: &str| {
        let out = std::process::Command::new("git")
            .args(["hash-object", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(content.as_bytes())?;
                child.wait_with_output()
            })
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let null = "0".repeat(40);
    assert_eq!(
        paths(&evidence),
        [
            "nested nested/".to_string(),
            format!("missing {null} gone.txt"),
            format!("missing {} link", id("src")),
            format!("missing {} odd name\twith tab.txt", id("odd\n")),
            format!("missing {} src/b.txt", id("two\nchanged\n")),
            format!("missing {} staged.txt", id("staged\n")),
        ]
    );
    let Evidence::Unsaved { checked, .. } = evidence else {
        unreachable!()
    };
    assert_eq!(checked, 6);

    // A file name with a newline cannot be reported, so nothing is known.
    write(&wt.join("bad\nname"), "x\n");
    let Status::Checked(Evidence::Unknown { reason }) = check(&p, &worktree) else {
        panic!("expected Unknown");
    };
    assert_eq!(reason, "a file name contains a newline");
    std::fs::remove_file(wt.join("bad\nname")).unwrap();

    // Detached on the branch's commit: the branch holds it.
    git(&wt, &["checkout", "-q", "--detach"]);
    assert!(matches!(check(&p, &worktree), Status::Checked(_)));
    // Detached on a commit only HEAD holds.
    git(&wt, &["commit", "-qam", "only here"]);
    assert!(matches!(check(&p, &worktree), Status::Unreachable));
}
