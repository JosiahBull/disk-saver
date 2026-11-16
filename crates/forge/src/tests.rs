//! Lookups run against [`FakePlatform`], with `git` and the forge CLIs
//! scripted per call.

use std::collections::BTreeMap;
use std::path::Path;

use disk_saver_platform::{CommandOutput, FakePlatform};

use super::*;

const DIR: &str = "/home/tester/work/feat";
const URL: &str = "git@gitlab.example.com:team/monorepo.git";

const MERGED: &str = r#"[{"id":1,"iid":7,"state":"merged"}]"#;
const CLOSED: &str = r#"[{"id":1,"iid":7,"state":"closed"}]"#;
const OPEN: &str = r#"[{"id":1,"iid":7,"state":"merged"},{"id":2,"iid":9,"state":"opened"}]"#;

fn exit(status: i32, stderr: &str) -> CommandOutput {
    CommandOutput {
        status,
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// Script `branch`'s remote in [`DIR`]: no `branch.<name>.remote`, and
/// `origin` at `url`.
fn remote(fake: FakePlatform, branch: &str, url: &str) -> FakePlatform {
    let key = format!("branch.{branch}.remote");
    fake.with_command("git", &["-C", DIR, "config", "--get", &key], exit(1, ""))
        .with_command(
            "git",
            &["-C", DIR, "remote", "get-url", "origin"],
            CommandOutput::ok(format!("{url}\n")),
        )
}

/// Script `glab api`'s answer about `feat` in the repository at [`URL`].
fn glab(fake: FakePlatform, out: CommandOutput) -> FakePlatform {
    fake.with_command(
        "glab",
        &[
            "api",
            "--hostname",
            "gitlab.example.com",
            "projects/team%2Fmonorepo/merge_requests?source_branch=feat&state=all&per_page=100",
        ],
        out,
    )
}

fn look(fake: &FakePlatform, branch: &str) -> String {
    lookup(fake, Path::new(DIR), branch, &BTreeMap::new()).to_string()
}

#[test]
fn parses_remote_urls() {
    let remote = |url: &str| Remote::parse(url).map(|r| format!("{} {}", r.host, r.path));
    for (url, want) in [
        (
            "git@github.com:owner/repo.git",
            Some("github.com owner/repo"),
        ),
        ("github.com:owner/repo", Some("github.com owner/repo")),
        (
            "https://GitLab.Example.com/a/b/c.git",
            Some("gitlab.example.com a/b/c"),
        ),
        (
            "https://user:token@gitlab.example.com/a/b/",
            Some("gitlab.example.com a/b"),
        ),
        (
            "ssh://git@gitlab.example.com:2222/a/b.git",
            Some("gitlab.example.com a/b"),
        ),
        ("git://example.org/a/b", Some("example.org a/b")),
        ("file:///srv/repo.git", None),
        ("/srv/repo.git", None),
        ("../repo", None),
        ("./a:b/c", None),
        ("https://example.org/repo", None),
        ("", None),
    ] {
        assert_eq!(remote(url).as_deref(), want, "{url}");
    }
}

#[test]
fn picks_the_forge_by_host() {
    let mut forges = BTreeMap::new();
    assert_eq!(kind_for("github.com", &forges), ForgeKind::Github);
    assert_eq!(kind_for("github.example.com", &forges), ForgeKind::Github);
    assert_eq!(kind_for("gitlab.com", &forges), ForgeKind::Gitlab);
    assert_eq!(kind_for("gitlab.example.com", &forges), ForgeKind::Gitlab);
    assert_eq!(kind_for("code.example.org", &forges), ForgeKind::None);
    forges.insert("code.example.org".to_string(), ForgeKind::Gitlab);
    forges.insert("gitlab.example.com".to_string(), ForgeKind::None);
    assert_eq!(kind_for("code.example.org", &forges), ForgeKind::Gitlab);
    assert_eq!(kind_for("gitlab.example.com", &forges), ForgeKind::None);
}

#[test]
fn encodes_paths_and_branches_for_the_api() {
    assert_eq!(encode("team/sub/repo"), "team%2Fsub%2Frepo");
    assert_eq!(
        encode("me/fix-a+b#c d~e.f_g"),
        "me%2Ffix-a%2Bb%23c%20d~e.f_g"
    );
    assert_eq!(encode("ünï"), "%C3%BCn%C3%AF");
}

#[test]
fn reduces_the_requests_to_one_state() {
    let gl = |json: &str| parse_requests(ForgeKind::Gitlab, json).map(|s| s.to_string());
    let gh = |json: &str| parse_requests(ForgeKind::Github, json).map(|s| s.to_string());
    assert_eq!(gl(MERGED).as_deref(), Some("MR !7 merged"));
    assert_eq!(gl(OPEN).as_deref(), Some("MR !9 is open"));
    assert_eq!(gl(CLOSED).as_deref(), Some("MR !7 closed without merging"));
    assert_eq!(gl("[]").as_deref(), Some("no MR from this branch"));
    assert_eq!(
        gl(r#"[{"iid":3,"state":"merged"},{"iid":5,"state":"merged"},{"iid":8,"state":"closed"}]"#)
            .as_deref(),
        Some("MR !5 merged"),
    );
    // A state neither API documents counts as open.
    assert_eq!(
        gl(r#"[{"iid":3,"state":"locked"}]"#).as_deref(),
        Some("MR !3 is open")
    );
    // GitHub's merged pull request is closed, with a `merged_at`.
    assert_eq!(
        gh(r#"[{"number":14,"state":"closed","merged_at":"2026-10-23T10:22:13Z"}]"#).as_deref(),
        Some("PR #14 merged")
    );
    assert_eq!(
        gh(r#"[{"number":14,"state":"closed","merged_at":null}]"#).as_deref(),
        Some("PR #14 closed without merging")
    );
    assert_eq!(
        gh(r#"[{"number":14,"state":"open","merged_at":null}]"#).as_deref(),
        Some("PR #14 is open")
    );
    assert_eq!(gh("[]").as_deref(), Some("no PR from this branch"));
    // An error body, or anything but a list of requests, is no answer.
    assert_eq!(gh(r#"{"message":"Not Found"}"#), None);
    assert_eq!(gl("not json"), None);
    assert_eq!(gl("[{\"state\":\"merged\"}]"), None);
}

#[test]
fn looks_up_the_branch_remote_on_github() {
    let fake = FakePlatform::new()
        .with_command(
            "git",
            &["-C", DIR, "config", "--get", "branch.feat.remote"],
            CommandOutput::ok("upstream\n"),
        )
        .with_command(
            "git",
            &["-C", DIR, "remote", "get-url", "upstream"],
            CommandOutput::ok("https://github.com/owner/repo.git\n"),
        )
        .with_command(
            "gh",
            &[
                "api",
                "--hostname",
                "github.com",
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2022-11-28",
                "repos/owner/repo/pulls?head=owner:feat&state=all&per_page=100",
            ],
            CommandOutput::ok(
                r#"[{"number":3,"state":"closed","merged_at":null},
                    {"number":4,"state":"closed","merged_at":"2026-11-01T00:00:00Z"}]"#,
            ),
        );
    assert_eq!(look(&fake, "feat"), "PR #4 merged");
}

#[test]
fn asks_gitlab_by_its_encoded_project_path() {
    let fake = remote(
        FakePlatform::new(),
        "me/feat",
        "git@gitlab.example.com:team/sub/monorepo.git",
    )
    .with_command(
        "glab",
        &[
            "api",
            "--hostname",
            "gitlab.example.com",
            "projects/team%2Fsub%2Fmonorepo/merge_requests?source_branch=me%2Ffeat&state=all&per_page=100",
        ],
        CommandOutput::ok(OPEN),
    );
    assert_eq!(look(&fake, "me/feat"), "MR !9 is open");
}

#[test]
fn every_failure_is_an_answer_that_names_no_url() {
    let no_remote = FakePlatform::new()
        .with_command(
            "git",
            &["-C", DIR, "config", "--get", "branch.feat.remote"],
            exit(1, ""),
        )
        .with_command(
            "git",
            &["-C", DIR, "remote", "get-url", "origin"],
            exit(2, "error: No such remote 'origin'\n"),
        );
    assert_eq!(
        look(&no_remote, "feat"),
        "couldn't look up its MR or PR: it has no remote named origin"
    );

    let secret = remote(FakePlatform::new(), "feat", "https://token-123@/nowhere");
    let said = look(&secret, "feat");
    assert_eq!(
        said,
        "couldn't look up its MR or PR: the URL of origin names no forge repository"
    );
    assert!(!said.contains("token-123"));

    let unknown_host = remote(FakePlatform::new(), "feat", "git@code.example.org:a/b.git");
    assert_eq!(
        look(&unknown_host, "feat"),
        "couldn't look up its MR or PR: no forge is configured for code.example.org"
    );

    let missing = remote(FakePlatform::new(), "feat", URL)
        .with_command_error("glab", std::io::ErrorKind::NotFound);
    assert_eq!(
        look(&missing, "feat"),
        "couldn't look up its MR or PR: glab is not installed"
    );

    let failing = glab(
        remote(FakePlatform::new(), "feat", URL),
        exit(1, "Warning: something\nglab: 401 Unauthorized (HTTP 401)\n"),
    );
    assert_eq!(
        look(&failing, "feat"),
        "couldn't look up its MR or PR: glab failed: glab: 401 Unauthorized (HTTP 401)"
    );

    let garbled = glab(
        remote(FakePlatform::new(), "feat", URL),
        CommandOutput::ok("<html>"),
    );
    assert_eq!(
        look(&garbled, "feat"),
        "couldn't look up its MR or PR: glab returned something other than a list of requests"
    );
}

#[test]
fn a_host_in_forges_is_asked_with_that_forge() {
    let fake = remote(
        FakePlatform::new(),
        "feat",
        "git@code.example.org:team/monorepo.git",
    )
    .with_command(
        "glab",
        &[
            "api",
            "--hostname",
            "code.example.org",
            "projects/team%2Fmonorepo/merge_requests?source_branch=feat&state=all&per_page=100",
        ],
        CommandOutput::ok(MERGED),
    );
    let forges = BTreeMap::from([("code.example.org".to_string(), ForgeKind::Gitlab)]);
    assert_eq!(
        lookup(&fake, Path::new(DIR), "feat", &forges).to_string(),
        "MR !7 merged"
    );
}
