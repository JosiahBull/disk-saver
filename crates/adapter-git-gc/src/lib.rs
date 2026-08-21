//! `disk-saver-adapter-git-gc` — reclaims space by running `git gc` on idle
//! repositories.
//!
//! Over time a git repo accumulates loose objects and unreferenced history in
//! `.git`. `git gc` repacks and prunes those (never touching *reachable*
//! history), which can shrink `.git` substantially. This adapter finds git repos
//! under the configured `roots`, and — for those idle past the retention policy
//! and with something worth packing — runs `git gc`, reporting the bytes the
//! `.git` directory shrank by.
//!
//! It is [`Class::Rebuildable`](disk_saver_core::Class): only regenerable
//! packfile layout / unreachable objects change, so `confirm` defaults to
//! `false`. Age-gating means actively-developed repos (which git auto-gcs
//! anyway) are left alone; the one-time win is on repos you have not touched in
//! a while. Requires `git` on `PATH` (else the adapter is `Unavailable`).

#![forbid(unsafe_code)]

use std::path::Path;
use std::time::Duration;

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, CommandSpec, ConfigError, Ctx,
    Decision, DecisionKind, DecisionLog, FileKind, Outcome, Platform, RetentionPolicy,
    parse_adapter_config,
};
use disk_saver_scan::{FsConfig, Rule, find_artifacts};
use serde::Deserialize;

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "git-gc";

/// A repo is any directory containing a `.git` directory.
static GIT_RULE: &[Rule] = &[Rule {
    artifact_dir: ".git",
    required_sibling: &[],
    validate: |_, _| true,
}];

/// Scavenge floor when the config does not name one — see [`build`].
const DEFAULT_MIN_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// `[adapters.git-gc]`: the shared filesystem-scan config plus `aggressive`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GcConfig {
    #[serde(flatten)]
    fs: FsConfig,
    /// Pass `--aggressive` to `git gc` (slower, sometimes smaller). Default off.
    aggressive: bool,
}

/// The factory the CLI registry uses to build the git-gc adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    // `FsConfig` defaults `min_age` to 1h, which suits adapters that delete a
    // build artifact outright. `git gc` instead *repacks* a live repo: doing
    // that to something touched an hour ago is work the next commit undoes, and
    // it competes with the developer for IO. Only override when the user was
    // silent, so an explicit `min_age = "1h"` still wins.
    let min_age_unset = raw
        .as_ref()
        .and_then(toml::Value::as_table)
        .is_none_or(|t| !t.contains_key("min_age"));
    let mut cfg: GcConfig = parse_adapter_config(NAME, raw)?;
    if min_age_unset {
        cfg.fs.min_age = DEFAULT_MIN_AGE.min(cfg.fs.max_age);
    }
    let policy = RetentionPolicy::new(cfg.fs.max_age, cfg.fs.min_age);
    policy.validate().map_err(|message| ConfigError::Adapter {
        adapter: NAME.to_string(),
        message,
    })?;
    Ok(Box::new(GitGcAdapter { policy, cfg }))
}

struct GitGcAdapter {
    policy: RetentionPolicy,
    cfg: GcConfig,
}

/// Probe that `git` is runnable. Missing binary or non-zero exit ⇒ `Unavailable`.
fn ensure_git(ctx: &Ctx) -> Result<(), AdapterError> {
    match ctx
        .platform
        .run_command(&CommandSpec::new("git", ["--version"]))
    {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => Err(AdapterError::unavailable(format!(
            "`git --version` exited {}",
            out.status
        ))),
        Err(_) => Err(AdapterError::unavailable("`git` not found on PATH")),
    }
}

/// Estimate the bytes `git gc` could reclaim from `repo`: loose-object size plus
/// garbage, from `git count-objects -v` (reported in KiB). `0` means nothing is
/// worth packing.
fn reclaimable_bytes(platform: &dyn Platform, repo: &Path) -> u64 {
    let repo_s = repo.to_string_lossy().into_owned();
    let spec = CommandSpec::new("git", ["-C", repo_s.as_str(), "count-objects", "-v"]);
    let Ok(out) = platform.run_command(&spec) else {
        return 0;
    };
    if !out.success() {
        return 0;
    }
    let text = out.stdout_string();
    let mut kib = 0u64;
    for line in text.lines() {
        let mut parts = line.splitn(2, ": ");
        if let (Some(key), Some(val)) = (parts.next(), parts.next())
            && (key == "size" || key == "size-garbage")
        {
            kib = kib.saturating_add(val.trim().parse::<u64>().unwrap_or(0));
        }
    }
    kib.saturating_mul(1024)
}

impl Adapter for GitGcAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        ensure_git(ctx)
    }

    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        // Surfaces a missing git as Unavailable (skip this run, retry next).
        ensure_git(ctx)
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let now = ctx.now();
        let (roots, opts) =
            self.cfg.fs.resolve_walk(ctx.platform).map_err(|e| {
                AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}"))
            })?;

        let mut candidates = Vec::new();
        for found in find_artifacts(ctx.platform, &roots, GIT_RULE, &opts) {
            let repo = found.project_root;
            let age = now.duration_since(found.last_active).unwrap_or_default();
            let id = repo.to_string_lossy().into_owned();
            let label = format!("git gc: {}", repo.display());
            if !self.policy.eligible(age, ctx.pressure) {
                if DecisionLog::compiled() {
                    ctx.decide(
                        Decision::new(DecisionKind::Kept, NAME, id)
                            .label(label)
                            .age(age)
                            .reason("repo active more recently than the retention policy"),
                    );
                }
                continue;
            }
            let bytes = reclaimable_bytes(ctx.platform, &repo);
            if bytes == 0 {
                continue; // already packed / nothing to reclaim
            }
            candidates.push(
                Candidate::new(
                    id.clone(),
                    label.clone(),
                    bytes,
                    found.last_active,
                    Class::Rebuildable,
                )
                .confirm(self.cfg.fs.confirm),
            );
            if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Planned, NAME, id)
                        .label(label)
                        .bytes(bytes)
                        .age(age),
                );
            }
        }
        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let mut outcomes = Vec::with_capacity(batch.len());
        for c in batch {
            let repo = Path::new(&c.id);
            let gitdir = repo.join(".git");
            let is_repo = matches!(
                ctx.platform.metadata(&gitdir),
                Ok(m) if m.kind == FileKind::Dir
            );
            let outcome = if !is_repo {
                Outcome::Skipped {
                    id: c.id.clone(),
                    reason: "no longer a git repository".to_string(),
                }
            } else {
                let before = ctx.platform.dir_size(&gitdir).unwrap_or(0);
                let repo_s = c.id.clone();
                let mut args = vec!["-C", repo_s.as_str(), "gc"];
                if self.cfg.aggressive {
                    args.push("--aggressive");
                }
                match ctx.platform.run_command(&CommandSpec::new("git", args)) {
                    Ok(out) if out.success() => {
                        let after = ctx.platform.dir_size(&gitdir).unwrap_or(before);
                        Outcome::Removed {
                            id: c.id.clone(),
                            bytes: before.saturating_sub(after),
                        }
                    }
                    Ok(out) => Outcome::Failed {
                        id: c.id.clone(),
                        error: format!(
                            "`git gc` exited {}: {}",
                            out.status,
                            out.stderr_string().trim()
                        ),
                    },
                    Err(e) => Outcome::Failed {
                        id: c.id.clone(),
                        error: format!("running `git gc`: {e}"),
                    },
                }
            };
            if DecisionLog::compiled() {
                let kind = match &outcome {
                    Outcome::Removed { .. } => DecisionKind::Deleted,
                    Outcome::Skipped { .. } => DecisionKind::Skipped,
                    Outcome::Failed { .. } => DecisionKind::Failed,
                };
                ctx.decide(Decision::new(kind, NAME, c.id.clone()));
            }
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_saver_core::{Pressure, Store};
    use disk_saver_platform::{CommandOutput, FakePlatform};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn t(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    const COUNT_OBJECTS: &str = "count: 100\nsize: 2048\nin-pack: 5\npacks: 1\nsize-pack: 500\nprune-packable: 0\ngarbage: 3\nsize-garbage: 512\n";

    fn fake_repo() -> FakePlatform {
        FakePlatform::new()
            .with_now(t(400))
            .with_command(
                "git",
                &["--version"],
                CommandOutput::ok("git version 2.44.0"),
            )
            // An idle repo (source last touched 60 days ago) under ~/dev.
            .with_file("~/dev/proj/src/main.rs", b"fn main(){}".to_vec(), t(340))
            .with_file(
                "~/dev/proj/.git/HEAD",
                b"ref: refs/heads/main".to_vec(),
                t(340),
            )
            .with_file("~/dev/proj/.git/objects/aa/loose", vec![0u8; 4096], t(340))
    }

    fn cfg_dev() -> Option<toml::Value> {
        Some(toml::from_str("roots = [\"~/dev\"]").unwrap())
    }

    /// A repo whose source was last touched `idle` before the fake's `now`.
    fn repo_idle_for(idle: Duration) -> FakePlatform {
        let stamp = t(400) - idle;
        FakePlatform::new()
            .with_now(t(400))
            .with_command(
                "git",
                &["--version"],
                CommandOutput::ok("git version 2.44.0"),
            )
            .with_command_prefix("git", &["-C"], CommandOutput::ok(COUNT_OBJECTS))
            .with_file("~/dev/proj/src/main.rs", b"fn main(){}".to_vec(), stamp)
            .with_file(
                "~/dev/proj/.git/HEAD",
                b"ref: refs/heads/main".to_vec(),
                stamp,
            )
            .with_file("~/dev/proj/.git/objects/aa/loose", vec![0u8; 4096], stamp)
    }

    /// Plan against `fake` under scavenge and report how many repos were proposed.
    fn scavenge_plan_len(fake: &FakePlatform, cfg: Option<toml::Value>) -> usize {
        let mut a = (factory().build)(cfg).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        a.observe(&mut ctx).unwrap();
        a.plan(&mut ctx).unwrap().len()
    }

    #[test]
    fn default_min_age_is_six_hours_not_the_shared_one_hour() {
        // git-gc opts out of `FsConfig`'s 1h floor: it repacks a live repo rather
        // than deleting a rebuildable artifact, so a repo touched two hours ago
        // is still "in use" for its purposes.
        assert_eq!(
            scavenge_plan_len(&repo_idle_for(Duration::from_secs(2 * 60 * 60)), cfg_dev()),
            0,
            "2h idle must stay inside the 6h floor"
        );
        assert_eq!(
            scavenge_plan_len(&repo_idle_for(Duration::from_secs(8 * 60 * 60)), cfg_dev()),
            1,
            "8h idle is past the 6h floor"
        );
    }

    #[test]
    fn explicit_min_age_beats_the_git_gc_default() {
        // The override applies only when the user was silent.
        let cfg = Some(toml::from_str("roots = [\"~/dev\"]\nmin_age = \"1h\"\n").unwrap());
        assert_eq!(
            scavenge_plan_len(&repo_idle_for(Duration::from_secs(2 * 60 * 60)), cfg),
            1,
            "an explicit 1h floor must not be silently raised to 6h"
        );
    }

    #[test]
    fn short_max_age_does_not_trip_the_default_floor() {
        // A 30m `max_age` is shorter than our 6h default floor. Since the floor
        // is our opinion and not the user's, it gives way rather than making the
        // whole config unloadable.
        let cfg = Some(toml::from_str("roots = [\"~/dev\"]\nmax_age = \"30m\"\n").unwrap());
        assert!(
            (factory().build)(cfg).is_ok(),
            "our own default floor must not reject the user's shorter max_age"
        );
    }

    #[test]
    fn clamped_floor_still_gates_on_the_users_max_age() {
        // Not merely loadable: the clamp lands on 30m, so that is the scavenge
        // floor — 10m idle is protected, 45m idle is collectable.
        let cfg = || Some(toml::from_str("roots = [\"~/dev\"]\nmax_age = \"30m\"\n").unwrap());
        assert_eq!(
            scavenge_plan_len(&repo_idle_for(Duration::from_secs(10 * 60)), cfg()),
            0,
            "10m idle is inside the clamped 30m floor"
        );
        assert_eq!(
            scavenge_plan_len(&repo_idle_for(Duration::from_secs(45 * 60)), cfg()),
            1,
            "45m idle is past the clamped 30m floor"
        );
    }

    #[test]
    fn contradictory_explicit_pair_is_still_rejected() {
        // The clamp applies only to *our* default. When the user writes both
        // numbers and they contradict, that is a mistake worth reporting.
        let cfg = Some(toml::from_str("min_age = \"12h\"\nmax_age = \"30m\"\n").unwrap());
        assert!((factory().build)(cfg).is_err());
    }

    #[test]
    fn unavailable_when_git_missing() {
        let fake = FakePlatform::new().with_now(t(400));
        let mut a = (factory().build)(cfg_dev()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        assert!(matches!(
            a.observe(&mut ctx),
            Err(AdapterError::Unavailable(_))
        ));
    }

    #[test]
    fn plans_idle_repo_with_loose_objects_then_runs_gc() {
        let fake = fake_repo().with_command_prefix(
            "git",
            &["-C"],
            CommandOutput::ok(COUNT_OBJECTS), // covers count-objects; gc also matches -C prefix
        );
        let mut a = (factory().build)(cfg_dev()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(&fake, store.bucket(NAME), Pressure::Normal, &log);

        a.observe(&mut ctx).unwrap();
        let plan = a.plan(&mut ctx).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].class, Class::Rebuildable);
        // reclaimable estimate = (size 2048 + size-garbage 512) KiB.
        assert_eq!(plan[0].bytes, (2048 + 512) * 1024);
        assert!(!plan[0].requires_confirmation);

        let out = a.execute(&mut ctx, &plan).unwrap();
        assert!(matches!(&out[0], Outcome::Removed { .. }));
        assert!(
            fake.commands_run()
                .iter()
                .any(|c| c.program == "git" && c.args.iter().any(|a| a == "gc"))
        );
    }

    #[test]
    fn active_repo_is_not_gced() {
        // Source touched an hour ago → inside the 6h floor, so not eligible even
        // under scavenge. Repacking a repo someone is actively committing to is
        // work the next commit undoes.
        let recent = t(400) - Duration::from_secs(60 * 60);
        let fake = FakePlatform::new()
            .with_now(t(400))
            .with_command(
                "git",
                &["--version"],
                CommandOutput::ok("git version 2.44.0"),
            )
            .with_command_prefix("git", &["-C"], CommandOutput::ok(COUNT_OBJECTS))
            .with_file("~/dev/proj/src/main.rs", b"fn main(){}".to_vec(), recent)
            .with_file(
                "~/dev/proj/.git/HEAD",
                b"ref: refs/heads/main".to_vec(),
                recent,
            );
        let mut a = (factory().build)(cfg_dev()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let log = DecisionLog::disabled();
        let mut ctx = Ctx::new(
            &fake,
            store.bucket(NAME),
            Pressure::Scavenge { need: 1 },
            &log,
        );
        a.observe(&mut ctx).unwrap();
        assert!(a.plan(&mut ctx).unwrap().is_empty());
    }
}
