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
    Adapter, AdapterError, AdapterFactory, Candidate, Class, CommandSpec, ConfigCx, ConfigError,
    Configured, Ctx, Decision, DecisionKind, DecisionLog, FileKind, Outcome, Platform,
    RetentionPolicy,
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

/// Scavenge floor when the config does not name one.
///
/// Longer than [`FsConfig::MIN_AGE`], the shared floor the build-artifact
/// adapters use: `git gc` *repacks a live repo* rather than deleting something
/// rebuildable, so doing it to a repo touched an hour ago is work the next
/// commit undoes, and it competes with the developer for IO.
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
    AdapterFactory::typed(NAME, build)
}

/// Swap the shared scavenge floor for this adapter's own longer one, then
/// validate the retention thresholds.
fn build(cfg: GcConfig, cx: &ConfigCx) -> Result<GitGcAdapter, ConfigError> {
    // `with_default` leaves an explicit `min_age = "1h"` alone and replaces only
    // the shared default; `capped_at` then lets *our* number give way to a
    // shorter `max_age` the user did write, while still reporting a `min_age`
    // they set above it.
    let min_age = Configured::from(cfg.fs.min_age)
        .with_default(DEFAULT_MIN_AGE)
        .capped_at(cfg.fs.max_age, cx, "min_age", "max_age")?
        .unwrap_or(DEFAULT_MIN_AGE);
    Ok(GitGcAdapter {
        policy: cx.retention(cfg.fs.max_age, min_age)?,
        cfg,
    })
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
