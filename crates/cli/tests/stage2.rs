//! End-to-end tests for the Stage 2 subcommands: `review`, `status`, `why`,
//! and `schedule` (§16).
//!
//! Each test isolates the process inside a tempdir by pointing `HOME` and
//! `DISK_SAVER_CONFIG` there, and pins `global.state_dir` to a known path so the
//! approvals queue can be seeded deterministically before the binary runs.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use assert_cmd::Command;
use disk_saver_core::{ApprovalState, Candidate, Class, QueuedItem, Store};
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
        .env_remove("RUST_LOG");
    cmd
}

/// Write a config with a pinned `state_dir` and only `rust-target` enabled,
/// rooted at `home`. Returns the config path.
fn write_config(home: &Path, state: &Path) -> PathBuf {
    let config = home.join("config.toml");
    let toml = format!(
        "[global]\nstate_dir = \"{state}\"\n\n\
         [adapters.docker]\nenabled = false\n\
         [adapters.node-modules]\nenabled = false\n\
         [adapters.python-cache]\nenabled = false\n\
         [adapters.trash]\nenabled = false\n\n\
         [adapters.rust-target]\nroots = [\"{roots}\"]\n",
        state = state.display(),
        roots = home.display(),
    );
    std::fs::write(&config, toml).unwrap();
    config
}

/// Seed a single Pending `rust-target` approval whose candidate points at a
/// (non-existent) `target/` directory. Returns the queue key.
fn seed_approval(state: &Path) -> String {
    std::fs::create_dir_all(state).unwrap();
    let ghost = state.join("ghost/target");
    let item = QueuedItem {
        adapter: "rust-target".to_owned(),
        candidate: Candidate::new(
            ghost.to_string_lossy().into_owned(),
            format!("cargo target in {}", state.join("ghost").display()),
            4096,
            SystemTime::UNIX_EPOCH,
            Class::Rebuildable,
        )
        .confirm(true),
        first_queued_unix: 1_000,
        state: ApprovalState::Pending,
        snooze_until_unix: None,
    };
    let key = item.key();
    let store = Store::open(&state.join("state.db")).unwrap();
    store
        .bucket("_approvals")
        .set(&key, &item, SystemTime::now())
        .unwrap();
    key
}

#[test]
fn review_list_json_shows_the_seeded_item() {
    let home = TempDir::new().unwrap();
    let state = home.path().join("state");
    let config = write_config(home.path(), &state);
    let key = seed_approval(&state);

    cmd(home.path(), &config)
        .args(["review", "--list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&key))
        .stdout(predicate::str::contains("\"adapter\": \"rust-target\""))
        .stdout(predicate::str::contains("\"state\": \"pending\""))
        .stdout(predicate::str::contains("\"bytes\": 4096"));
}

#[test]
fn review_approve_runs_the_adapter_and_clears_the_item() {
    let home = TempDir::new().unwrap();
    let state = home.path().join("state");
    let config = write_config(home.path(), &state);
    let key = seed_approval(&state);

    // The candidate's target/ no longer exists, so execute reports Skipped —
    // which still exercises the approve → execute_approved → dequeue path.
    cmd(home.path(), &config)
        .args(["review", "--approve", &key])
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped"));

    // The item must be gone from the queue afterwards.
    cmd(home.path(), &config)
        .args(["review", "--list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&key).not());
}

#[test]
fn review_approve_all_clears_the_queue() {
    let home = TempDir::new().unwrap();
    let state = home.path().join("state");
    let config = write_config(home.path(), &state);
    let key = seed_approval(&state);

    cmd(home.path(), &config)
        .args(["review", "--approve-all"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&key));

    cmd(home.path(), &config)
        .args(["review", "--list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no items awaiting approval"));
}

#[test]
fn review_interactive_quit_is_a_clean_smoke_test() {
    let home = TempDir::new().unwrap();
    let state = home.path().join("state");
    let config = write_config(home.path(), &state);
    let key = seed_approval(&state);

    // Piped stdin is not a TTY, so `review` prints the queue plus a hint and
    // exits cleanly; the `q` is harmless.
    cmd(home.path(), &config)
        .args(["review"])
        .write_stdin("q\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(&key));
}

#[test]
fn status_json_emits_the_expected_fields() {
    let home = TempDir::new().unwrap();
    let state = home.path().join("state");
    let config = write_config(home.path(), &state);
    seed_approval(&state);

    cmd(home.path(), &config)
        .args(["status", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"total\":"))
        .stdout(predicate::str::contains("\"free\":"))
        .stdout(predicate::str::contains("\"state\":"))
        .stdout(predicate::str::contains("\"thresholds\":"))
        .stdout(predicate::str::contains("\"pending_approvals\": 1"))
        .stdout(predicate::str::contains("\"state_db_path\":"))
        .stdout(predicate::str::contains("state.db"))
        .stdout(predicate::str::contains("\"state_db_bytes\":"))
        .stdout(predicate::str::contains("\"recent_runs\":"));
}

#[test]
fn schedule_install_writes_a_unit_then_status_reports_it() {
    let home = TempDir::new().unwrap();
    let state = home.path().join("state");
    let config = write_config(home.path(), &state);

    cmd(home.path(), &config)
        .args(["schedule", "install"])
        .assert()
        .success()
        .stdout(predicate::str::contains("installed"));

    // The per-OS unit file must exist under the sandboxed HOME.
    let unit = if cfg!(target_os = "macos") {
        home.path()
            .join("Library/LaunchAgents/com.disk-saver.plist")
    } else {
        home.path().join(".config/systemd/user/disk-saver.timer")
    };
    assert!(unit.exists(), "install must write {}", unit.display());

    cmd(home.path(), &config)
        .args(["schedule", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("installed"));

    // Uninstall is idempotent and removes the file.
    cmd(home.path(), &config)
        .args(["schedule", "uninstall"])
        .assert()
        .success();
    assert!(!unit.exists(), "uninstall must remove {}", unit.display());
}

#[cfg(feature = "decision-log")]
#[test]
fn why_reports_matching_decisions() {
    use std::io::Write;

    let home = TempDir::new().unwrap();
    let state = home.path().join("state");
    let config = write_config(home.path(), &state);
    std::fs::create_dir_all(&state).unwrap();

    // Seed one decision-log line mentioning a searchable id.
    let mut f = std::fs::File::create(state.join("decisions.jsonl")).unwrap();
    writeln!(
        f,
        "{{\"at_unix\":100,\"pressure\":\"normal\",\"kind\":\"deleted\",\"adapter\":\"rust-target\",\"id\":\"/proj/target\",\"label\":\"cargo target\",\"bytes\":4096,\"age_secs\":900000,\"reason\":null}}"
    )
    .unwrap();
    drop(f);

    cmd(home.path(), &config)
        .args(["why", "target"])
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted"))
        .stdout(predicate::str::contains("/proj/target"));

    // A non-matching query yields a clean "no decisions" message.
    cmd(home.path(), &config)
        .args(["why", "nonesuch"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no recorded decisions"));
}
