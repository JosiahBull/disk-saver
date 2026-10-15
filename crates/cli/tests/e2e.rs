//! End-to-end tests of the `disk-saver` binary (§16).
//!
//! Each test isolates the process inside a tempdir by pointing `HOME` and
//! `DISK_SAVER_CONFIG` there, so `RealPlatform` never touches the developer's
//! real filesystem. XDG overrides are cleared for the same reason.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A `disk-saver` invocation confined to `home` with config at `config`.
fn cmd(home: &Path, config: &Path) -> Command {
    let mut cmd = Command::cargo_bin("disk-saver").expect("binary builds");
    cmd.env("HOME", home)
        .env("DISK_SAVER_CONFIG", config)
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CODEX_HOME")
        .env_remove("RUST_LOG");
    cmd
}

#[test]
fn config_init_writes_a_file_then_check_passes() {
    let home = TempDir::new().unwrap();
    let config = home.path().join("config.toml");

    // `config init` writes the default config.
    cmd(home.path(), &config)
        .args(["config", "init"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote default config"));
    assert!(config.exists(), "config init should create the file");

    // `config check` validates it and builds every enabled adapter.
    cmd(home.path(), &config)
        .args(["config", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config OK"))
        .stdout(predicate::str::contains("rust-target"));
}

#[test]
fn config_init_refuses_to_clobber_an_existing_file() {
    let home = TempDir::new().unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(&config, "# hand-edited\n").unwrap();

    cmd(home.path(), &config)
        .args(["config", "init"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not overwriting"));

    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "# hand-edited\n",
        "existing config must be left untouched"
    );
}

#[test]
fn plan_json_emits_the_target_candidate_and_deletes_nothing() {
    let home = TempDir::new().unwrap();

    // A minimal cargo project with a build-artifact `target/` dir.
    let proj = home.path().join("proj");
    let target = proj.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(
        proj.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let cachedir_tag = target.join("CACHEDIR.TAG");
    std::fs::write(
        &cachedir_tag,
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();

    // Only rust-target enabled, rooted at the project tree, with zero age
    // floors so the freshly-created target qualifies at normal pressure.
    let state = home.path().join("state");
    let config = home.path().join("config.toml");
    let toml = format!(
        "[global]\nstate_dir = \"{state}\"\n\n\
         [adapters.docker]\nenabled = false\n\
         [adapters.node-modules]\nenabled = false\n\
         [adapters.python-cache]\nenabled = false\n\
         [adapters.trash]\nenabled = false\n\n\
         [adapters.rust-target]\nroots = [\"{roots}\"]\nmax_age = \"0s\"\nmin_age = \"0s\"\n",
        state = state.display(),
        roots = proj.display(),
    );
    std::fs::write(&config, toml).unwrap();

    cmd(home.path(), &config)
        .args(["plan", "--json", "--pressure", "normal"])
        .assert()
        .success()
        // dry run: nothing removed, not throttled on the first run.
        .stdout(predicate::str::contains("\"dry_run\": true"))
        .stdout(predicate::str::contains("\"throttled\": false"))
        // exactly the rust-target adapter, with one candidate and no removals.
        .stdout(predicate::str::contains("\"name\": \"rust-target\""))
        .stdout(predicate::str::contains("\"candidates\": 1"))
        .stdout(predicate::str::contains("\"removed\": 0"))
        .stdout(predicate::str::contains("\"name\": \"node-modules\"").not());

    // The plan must not have deleted anything.
    assert!(
        cachedir_tag.exists(),
        "plan/dry-run must never delete the target dir"
    );
    assert!(target.exists());
}

#[test]
fn plan_detailed_lists_the_project_path_per_candidate() {
    let home = TempDir::new().unwrap();
    let proj = home.path().join("proj");
    let target = proj.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(proj.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(
        target.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();

    let state = home.path().join("state");
    let config = home.path().join("config.toml");
    let toml = format!(
        "[global]\nstate_dir = \"{state}\"\n\n\
         [adapters.docker]\nenabled = false\n\
         [adapters.node-modules]\nenabled = false\n\
         [adapters.python-cache]\nenabled = false\n\
         [adapters.trash]\nenabled = false\n\n\
         [adapters.rust-target]\nroots = [\"{roots}\"]\nmax_age = \"0s\"\nmin_age = \"0s\"\n",
        state = state.display(),
        roots = proj.display(),
    );
    std::fs::write(&config, toml).unwrap();

    cmd(home.path(), &config)
        .args(["plan", "--pressure", "normal", "--detailed"])
        .assert()
        .success()
        // The per-adapter detail section and the candidate's project path.
        .stdout(predicate::str::contains("── rust-target"))
        .stdout(predicate::str::contains("class"))
        .stdout(predicate::str::contains(
            proj.to_string_lossy().into_owned(),
        ));

    // Still a dry run — nothing deleted.
    assert!(target.join("CACHEDIR.TAG").exists());
}

#[test]
fn plan_reports_agent_worktrees_and_never_removes_them() {
    let home = TempDir::new().unwrap();
    let repo = home.path().join("dev").join("repo");
    let admin = repo.join(".git").join("worktrees").join("agent-1");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/fix/x\n").unwrap();

    // A Claude Code worktree with build output in it, and a Codex checkout.
    let claude = repo.join(".claude").join("worktrees").join("agent-1");
    std::fs::create_dir_all(claude.join("target")).unwrap();
    std::fs::write(
        claude.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .unwrap();
    std::fs::write(claude.join("target").join("big"), vec![0u8; 64 * 1024]).unwrap();
    let codex = home.path().join(".codex/worktrees/445f/repo");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(codex.join("README"), "x").unwrap();

    let state = home.path().join("state");
    let config = home.path().join("config.toml");
    let toml = format!(
        "[global]\nstate_dir = \"{state}\"\n\n\
         [adapters.claude-worktrees]\nroots = [\"{roots}\"]\n",
        state = state.display(),
        roots = home.path().join("dev").display(),
    );
    std::fs::write(&config, toml).unwrap();

    let only = [
        "--adapter",
        "claude-worktrees",
        "--adapter",
        "codex-worktrees",
    ];
    // Scavenge is the pressure most willing to delete; still nothing is planned.
    cmd(home.path(), &config)
        .args(["plan", "--pressure", "scavenge", "--detailed"])
        .args(only)
        .assert()
        .success()
        .stdout(predicate::str::contains("reported only, never cleaned"))
        .stdout(predicate::str::contains(format!(
            "{} [fix/x]",
            claude.display()
        )))
        .stdout(predicate::str::contains(
            codex.to_string_lossy().into_owned(),
        ))
        .stdout(predicate::str::contains("candidate(s)").not());

    cmd(home.path(), &config)
        .args(["plan", "--json", "--pressure", "scavenge"])
        .args(only)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"footprint\": ["))
        .stdout(predicate::str::contains("\"candidates\": 0"));

    assert!(claude.join("target").join("big").exists());
    assert!(codex.join("README").exists());
}

#[test]
fn doctor_grant_access_does_not_mistake_a_missing_trash_for_a_denied_one() {
    let home = TempDir::new().unwrap();
    let config = home.path().join("config.toml");
    cmd(home.path(), &config)
        .args(["config", "init"])
        .assert()
        .success();

    // The sandboxed HOME has no trash directory at all. `NotFound` is not a
    // permission problem, so the command must report access as fine — and in
    // doing so return before it would open System Settings, which is what keeps
    // this test safe to run unattended.
    cmd(home.path(), &config)
        .args(["doctor", "--grant-access"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("already granted").or(predicate::str::contains("macOS-only")),
        );
}
