//! Integration tests for the docker adapter, driven entirely against
//! [`FakePlatform`] with canned CLI JSON fixtures (no real docker daemon).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_adapter_docker::factory;
use disk_saver_core::{
    Adapter, AdapterError, Candidate, Class, CommandOutput, Ctx, DecisionLog, Outcome, Platform,
    Pressure, Store,
};
use disk_saver_platform::FakePlatform;

const DAY: u64 = 24 * 60 * 60;

// ── fixtures ─────────────────────────────────────────────────────────────
const VERSION: &str = "Docker version 24.0.7, build afdd53b";
const PS_EMPTY: &str = include_str!("fixtures/ps_empty.json");
const PS_WORKER: &str = include_str!("fixtures/ps_worker_exited.json");
const IMAGE_LS_APP: &str = include_str!("fixtures/image_ls_app.json");
const IMAGE_LS_TIERS: &str = include_str!("fixtures/image_ls_tiers.json");
const IMAGE_LS_SCAVENGE: &str = include_str!("fixtures/image_ls_scavenge.json");
const INSPECT_WORKER: &str = include_str!("fixtures/inspect_worker.json");
const SYSTEM_DF: &str = include_str!("fixtures/system_df.json");

/// Unix seconds of the fake clock's default `now` (`UNIX_EPOCH + 400 days`).
fn now_unix(fake: &FakePlatform) -> u64 {
    fake.now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// A `CommandOutput` with a non-zero status and the given stderr.
fn err_out(status: i32, stderr: &str) -> CommandOutput {
    CommandOutput {
        status,
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// Build a fake with `docker version` scripted to succeed.
fn fake_with_version() -> FakePlatform {
    FakePlatform::new().with_command("docker", &["version"], CommandOutput::ok(VERSION))
}

/// Build the adapter from default config.
fn adapter_default() -> Box<dyn Adapter> {
    (factory().build)(None).unwrap()
}

/// Build the adapter from a small TOML snippet.
fn adapter_from(toml: &str) -> Box<dyn Adapter> {
    let raw: toml::Value = toml::from_str(toml).unwrap();
    (factory().build)(Some(raw)).unwrap()
}

// ── availability ─────────────────────────────────────────────────────────

#[test]
fn daemon_missing_binary_is_unavailable() {
    let fake = FakePlatform::new().with_command_error("docker", std::io::ErrorKind::NotFound);
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = adapter_default();
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);

    let err = adapter.observe(&mut ctx).unwrap_err();
    assert!(matches!(err, AdapterError::Unavailable(_)), "got {err:?}");
    // check() surfaces the same condition for `doctor`.
    assert!(matches!(
        adapter.check(&mut ctx).unwrap_err(),
        AdapterError::Unavailable(_)
    ));
}

#[test]
fn daemon_down_nonzero_version_is_unavailable() {
    let fake = FakePlatform::new().with_command(
        "docker",
        &["version"],
        err_out(
            1,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.",
        ),
    );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = adapter_default();
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);

    let err = adapter.observe(&mut ctx).unwrap_err();
    assert!(matches!(err, AdapterError::Unavailable(_)), "got {err:?}");
}

// ── cold state + first-sighting grace ──────────────────────────────────────

#[test]
fn cold_run_deletes_nothing_then_deletes_after_ageing() {
    let fake = fake_with_version()
        .with_command(
            "docker",
            &["ps", "-a", "--format", "json"],
            CommandOutput::ok(PS_EMPTY),
        )
        .with_command(
            "docker",
            &["image", "ls", "--no-trunc", "--format", "json"],
            CommandOutput::ok(IMAGE_LS_APP),
        )
        .with_command(
            "docker",
            &["image", "rm", "sha256:appimageid00"],
            CommandOutput::ok(""),
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = adapter_default();
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);

    // Run 1: the image is seen for the first time → grace → nothing planned.
    adapter.observe(&mut ctx).unwrap();
    assert!(
        adapter.plan(&mut ctx).unwrap().is_empty(),
        "first run must propose nothing (grace)"
    );

    // Age the image past max_age (7d) and re-observe.
    fake.advance(Duration::from_secs(8 * DAY));
    adapter.observe(&mut ctx).unwrap();
    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1, "expected the aged image to be proposed");
    assert_eq!(plan[0].id, "image:sha256:appimageid00");
    assert_eq!(plan[0].class, Class::Cache);
    assert!(!plan[0].requires_confirmation);

    // Execute removes it.
    let outcomes = adapter.execute(&mut ctx, &plan).unwrap();
    assert!(
        matches!(&outcomes[0], Outcome::Removed { id, .. } if id == "image:sha256:appimageid00")
    );
    assert!(
        fake.commands_run()
            .iter()
            .any(|c| c.program == "docker" && c.args == ["image", "rm", "sha256:appimageid00"])
    );
    // KV entry dropped after successful removal.
    let img: Option<serde_json::Value> = store
        .bucket("docker")
        .get("image:sha256:appimageid00")
        .unwrap();
    assert!(img.is_none(), "KV image entry should be dropped on removal");
}

// ── all policy tiers + protect glob in one plan ────────────────────────────

#[test]
fn plan_covers_all_tiers_and_honors_protect() {
    let fake = fake_with_version()
        .with_command(
            "docker",
            &["ps", "-a", "--format", "json"],
            CommandOutput::ok(PS_WORKER),
        )
        .with_command(
            "docker",
            &["image", "ls", "--no-trunc", "--format", "json"],
            CommandOutput::ok(IMAGE_LS_TIERS),
        )
        .with_command(
            "docker",
            &["inspect", "cworkercontainerid"],
            CommandOutput::ok(INSPECT_WORKER),
        )
        .with_command(
            "docker",
            &["system", "df", "--format", "json"],
            CommandOutput::ok(SYSTEM_DF),
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let now = now_unix(&fake);
    let old = now - 60 * DAY;

    // Seed prior sightings so nothing is in the grace window.
    let seed = store.bucket("docker");
    let t = fake.now();
    for (key, tags) in [
        ("image:sha256:appoldimageid", vec!["app:old"]),
        ("image:sha256:libcacheimgid", vec!["lib:cache"]),
        ("image:sha256:danglingimgid", Vec::<&str>::new()),
        ("image:sha256:postgres15img", vec!["postgres:15"]),
    ] {
        seed.set(
            key,
            &serde_json::json!({"first_seen": old, "last_used": old, "tags": tags}),
            t,
        )
        .unwrap();
    }
    seed.set(
        "container:cworkercontainerid",
        &serde_json::json!({"last_running": old}),
        t,
    )
    .unwrap();

    let mut adapter = adapter_from("protect = [\"postgres:*\"]");
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);

    adapter.observe(&mut ctx).unwrap();
    let plan = adapter.plan(&mut ctx).unwrap();
    let ids: Vec<&str> = plan.iter().map(|c| c.id.as_str()).collect();

    assert_eq!(
        ids,
        vec![
            "container:cworkercontainerid", // ① exited container idle past policy
            "image:sha256:libcacheimgid",   // ② tagged image, unreferenced, past policy
            "image:sha256:danglingimgid",   // ③ dangling image
            "buildcache",                   // ④ coarse build-cache candidate
        ],
        "tier order and membership"
    );

    // Protected postgres image and referenced app:old image are absent.
    assert!(!ids.contains(&"image:sha256:postgres15img"));
    assert!(!ids.contains(&"image:sha256:appoldimageid"));

    // Classes: container + build cache are Rebuildable, images are Cache.
    let by_id = |id: &str| plan.iter().find(|c| c.id == id).unwrap();
    assert_eq!(
        by_id("container:cworkercontainerid").class,
        Class::Rebuildable
    );
    assert_eq!(by_id("image:sha256:libcacheimgid").class, Class::Cache);
    assert_eq!(by_id("image:sha256:danglingimgid").class, Class::Cache);
    assert_eq!(by_id("buildcache").class, Class::Rebuildable);

    // Build cache candidate was sized from `docker system df` (4.2GB).
    assert_eq!(by_id("buildcache").bytes, 4_200_000_000);
    // Image candidate carries the size from `docker image ls` (120MB).
    assert_eq!(by_id("image:sha256:libcacheimgid").bytes, 120_000_000);
}

// ── scavenge min_age floor ─────────────────────────────────────────────────

#[test]
fn scavenge_respects_min_age_floor() {
    let fake = fake_with_version()
        .with_command(
            "docker",
            &["ps", "-a", "--format", "json"],
            CommandOutput::ok(PS_EMPTY),
        )
        .with_command(
            "docker",
            &["image", "ls", "--no-trunc", "--format", "json"],
            CommandOutput::ok(IMAGE_LS_SCAVENGE),
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let now = now_unix(&fake);

    let seed = store.bucket("docker");
    let t = fake.now();
    // "mid" was used 3 days ago (between min_age 2d and max_age 7d).
    seed.set(
        "image:sha256:midimageid000",
        &serde_json::json!({"first_seen": now - 40 * DAY, "last_used": now - 3 * DAY, "tags": ["mid:img"]}),
        t,
    )
    .unwrap();
    // "fresh" was used 1 day ago (younger than min_age → protected by the floor).
    seed.set(
        "image:sha256:freshimageid0",
        &serde_json::json!({"first_seen": now - 40 * DAY, "last_used": now - DAY, "tags": ["fresh:img"]}),
        t,
    )
    .unwrap();

    // Under Normal, nothing is old enough (3d < max_age 7d).
    let mut adapter = adapter_default();
    {
        let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);
        adapter.observe(&mut ctx).unwrap();
        assert!(adapter.plan(&mut ctx).unwrap().is_empty());
    }

    // Under Scavenge, "mid" crosses min_age but "fresh" does not.
    let mut ctx = Ctx::new(
        &fake,
        store.bucket("docker"),
        Pressure::Scavenge { need: 1 },
        &log,
    );
    adapter.observe(&mut ctx).unwrap();
    let ids: Vec<String> = adapter
        .plan(&mut ctx)
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec!["image:sha256:midimageid000".to_string()]);
}

// ── partial execute failure ────────────────────────────────────────────────

#[test]
fn partial_execute_failure_skips_only_the_failing_sibling() {
    let fake = fake_with_version()
        .with_command(
            "docker",
            &["image", "rm", "sha256:aaa"],
            CommandOutput::ok(""),
        )
        .with_command(
            "docker",
            &["image", "rm", "sha256:bbb"],
            err_out(
                1,
                "Error response from daemon: conflict: unable to delete (in use)",
            ),
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = adapter_default();
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);

    let batch = vec![
        Candidate::new(
            "image:sha256:aaa",
            "aaa:latest",
            100,
            fake.now(),
            Class::Cache,
        ),
        Candidate::new(
            "image:sha256:bbb",
            "bbb:latest",
            200,
            fake.now(),
            Class::Cache,
        ),
    ];
    let outcomes = adapter.execute(&mut ctx, &batch).unwrap();

    // Outcomes preserve batch order.
    assert!(
        matches!(&outcomes[0], Outcome::Removed { id, bytes } if id == "image:sha256:aaa" && *bytes == 100)
    );
    assert!(matches!(&outcomes[1], Outcome::Skipped { id, .. } if id == "image:sha256:bbb"));

    // Both removals were attempted — a failing sibling does not stop the rest.
    let cmds = fake.commands_run();
    assert!(cmds.iter().any(|c| c.args == ["image", "rm", "sha256:aaa"]));
    assert!(cmds.iter().any(|c| c.args == ["image", "rm", "sha256:bbb"]));
}

// ── never uses -f; a gone entity is skipped, not failed ─────────────────────

#[test]
fn image_rm_never_forces_and_missing_entity_is_skipped() {
    let fake = fake_with_version().with_command(
        "docker",
        &["image", "rm", "sha256:gone"],
        err_out(1, "Error: No such image: sha256:gone"),
    );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let mut adapter = adapter_default();
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);

    let batch = vec![Candidate::new(
        "image:sha256:gone",
        "gone:latest",
        10,
        fake.now(),
        Class::Cache,
    )];
    let outcomes = adapter.execute(&mut ctx, &batch).unwrap();
    assert!(matches!(&outcomes[0], Outcome::Skipped { .. }));

    // No invocation ever carried a force flag.
    for c in fake.commands_run() {
        assert!(
            !c.args.iter().any(|a| a == "-f" || a == "--force")
                || c.args.first().map(String::as_str) == Some("builder"),
            "container/image rm must never force: {:?}",
            c.args
        );
    }
}

// ── build-cache prune uses the pressure-appropriate age ─────────────────────

#[test]
fn build_cache_prune_uses_max_age_normally_and_min_age_under_scavenge() {
    let batch = |bytes: u64, now: SystemTime| {
        vec![Candidate::new(
            "buildcache",
            "docker build cache",
            bytes,
            now,
            Class::Rebuildable,
        )]
    };

    // Normal → until=<max_age = 7d = 168h>.
    {
        let fake = fake_with_version().with_command_prefix(
            "docker",
            &["builder", "prune"],
            CommandOutput::ok(""),
        );
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut adapter = adapter_default();
        let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);
        let out = adapter.execute(&mut ctx, &batch(42, fake.now())).unwrap();
        assert!(matches!(&out[0], Outcome::Removed { bytes, .. } if *bytes == 42));
        let last = fake.commands_run().pop().unwrap();
        assert_eq!(
            last.args,
            ["builder", "prune", "--force", "--filter", "until=168h"]
        );
    }

    // Scavenge → until=<min_age = 2d = 48h>.
    {
        let fake = fake_with_version().with_command_prefix(
            "docker",
            &["builder", "prune"],
            CommandOutput::ok(""),
        );
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut adapter = adapter_default();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket("docker"),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        adapter.execute(&mut ctx, &batch(42, fake.now())).unwrap();
        let last = fake.commands_run().pop().unwrap();
        assert_eq!(
            last.args,
            ["builder", "prune", "--force", "--filter", "until=48h"]
        );
    }
}

// ── comfortable proposes nothing ────────────────────────────────────────────

#[test]
fn comfortable_pressure_plans_nothing() {
    let fake = fake_with_version()
        .with_command(
            "docker",
            &["ps", "-a", "--format", "json"],
            CommandOutput::ok(PS_EMPTY),
        )
        .with_command(
            "docker",
            &["image", "ls", "--no-trunc", "--format", "json"],
            CommandOutput::ok(IMAGE_LS_TIERS),
        )
        .with_command(
            "docker",
            &["system", "df", "--format", "json"],
            CommandOutput::ok(SYSTEM_DF),
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let now = now_unix(&fake);

    // Seed everything as very old and long-known so only pressure gates it.
    let seed = store.bucket("docker");
    let t = fake.now();
    for key in [
        "image:sha256:appoldimageid",
        "image:sha256:libcacheimgid",
        "image:sha256:danglingimgid",
        "image:sha256:postgres15img",
    ] {
        seed.set(
            key,
            &serde_json::json!({"first_seen": now - 90 * DAY, "last_used": now - 90 * DAY, "tags": []}),
            t,
        )
        .unwrap();
    }

    let mut adapter = adapter_default();
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Comfortable, &log);
    adapter.observe(&mut ctx).unwrap();
    assert!(
        adapter.plan(&mut ctx).unwrap().is_empty(),
        "Comfortable must never propose deletions"
    );
}

// ── vanished images are GC'd from KV ────────────────────────────────────────

#[test]
fn vanished_image_kv_entry_is_dropped() {
    let fake = fake_with_version()
        .with_command(
            "docker",
            &["ps", "-a", "--format", "json"],
            CommandOutput::ok(PS_EMPTY),
        )
        .with_command(
            "docker",
            &["image", "ls", "--no-trunc", "--format", "json"],
            CommandOutput::ok(IMAGE_LS_APP),
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();

    // Seed a stale image that will NOT appear in `docker image ls`.
    let seed = store.bucket("docker");
    seed.set(
        "image:sha256:vanishedid00",
        &serde_json::json!({"first_seen": 1u64, "last_used": 1u64, "tags": ["old:gone"]}),
        fake.now(),
    )
    .unwrap();

    let mut adapter = adapter_default();
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);
    adapter.observe(&mut ctx).unwrap();

    let vanished: Option<serde_json::Value> = store
        .bucket("docker")
        .get("image:sha256:vanishedid00")
        .unwrap();
    assert!(vanished.is_none(), "vanished image must be GC'd");
    // The present image is retained.
    let present: Option<serde_json::Value> = store
        .bucket("docker")
        .get("image:sha256:appimageid00")
        .unwrap();
    assert!(present.is_some());
}

// ── confirm = true routes to the approvals queue (flag on the candidate) ─────

#[test]
fn confirm_true_flags_candidates_for_confirmation() {
    let fake = fake_with_version()
        .with_command(
            "docker",
            &["ps", "-a", "--format", "json"],
            CommandOutput::ok(PS_EMPTY),
        )
        .with_command(
            "docker",
            &["image", "ls", "--no-trunc", "--format", "json"],
            CommandOutput::ok(IMAGE_LS_APP),
        );
    let store = Store::open_in_memory().unwrap();
    let log = DecisionLog::disabled();
    let now = now_unix(&fake);
    store
        .bucket("docker")
        .set(
            "image:sha256:appimageid00",
            &serde_json::json!({"first_seen": now - 40 * DAY, "last_used": now - 40 * DAY, "tags": ["app:1.0"]}),
            fake.now(),
        )
        .unwrap();

    let mut adapter = adapter_from("confirm = true");
    let mut ctx = Ctx::new(&fake, store.bucket("docker"), Pressure::Normal, &log);
    adapter.observe(&mut ctx).unwrap();
    let plan = adapter.plan(&mut ctx).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(
        plan[0].requires_confirmation,
        "confirm=true must flag candidates"
    );
}

// ── invalid config is rejected at build time ────────────────────────────────

#[test]
fn inverted_retention_thresholds_are_rejected() {
    // min_age > max_age must fail the factory build.
    let raw: toml::Value = toml::from_str("max_age = \"2d\"\nmin_age = \"7d\"").unwrap();
    assert!((factory().build)(Some(raw)).is_err());
}

#[test]
fn bad_protect_glob_is_rejected() {
    let raw: toml::Value = toml::from_str("protect = [\"[unterminated\"]").unwrap();
    assert!((factory().build)(Some(raw)).is_err());
}
