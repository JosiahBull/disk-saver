//! `disk-saver-adapter-docker` — the `docker` adapter (ARCHITECTURE.md §12.1).
//!
//! The reference adapter for KV-based usage tracking. It talks to whatever the
//! `docker` CLI talks to (Docker Desktop, OrbStack, colima, …) exclusively
//! through [`Platform::run_command`](disk_saver_core::Platform::run_command), so
//! it is OS-agnostic by construction and fully testable against a fake platform.
//!
//! # Phases
//!
//! * **check / availability** — `docker version`; a non-zero exit or a missing
//!   binary maps to [`AdapterError::Unavailable`].
//! * **observe** — `docker ps -a` (running containers use their image *now*;
//!   exited ones used it at their `FinishedAt`, read via `docker inspect`) and
//!   `docker image ls` reconcile the KV store: newly seen images get
//!   `first_seen = now` (the first-sighting grace, §11.5), vanished ones are
//!   dropped.
//! * **plan** — proposes, in impact order: exited containers idle past policy,
//!   then tagged images referenced by no container past policy, then dangling
//!   images, then the build cache as one coarse candidate sized from
//!   `docker system df`. `protect` globs and the grace window are honored.
//! * **execute** — `docker container rm` / `docker image rm` (never `-f`, §11.9,
//!   so a re-referenced image yields [`Outcome::Skipped`] rather than a forced
//!   delete) and `docker builder prune --force --filter until=<age>`.
//!
//! # KV state (bucket `docker`)
//!
//! * `image:<id> → { first_seen, last_used, tags }`
//! * `container:<id> → { last_running }`
//!
//! All timestamps are unix seconds derived from the platform clock.

#![forbid(unsafe_code)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, CommandSpec, ConfigError, Ctx,
    Decision, DecisionKind, DecisionLog, Outcome, Pressure, RetentionPolicy, parse_adapter_config,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// The stable adapter name (config section, KV bucket, log target).
const NAME: &str = "docker";

/// Sentinel id used for the single coarse build-cache candidate.
const BUILD_CACHE_ID: &str = "buildcache";

/// The factory the CLI registry uses to build the docker adapter.
///
/// `build` deserializes the adapter's `[adapters.docker]` table via
/// [`parse_adapter_config`], compiles the `protect` globs, and validates the
/// retention thresholds; any of these failing yields [`ConfigError::Adapter`].
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

/// Construct a boxed [`DockerAdapter`] from its opaque config table.
fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    let config: DockerConfig = parse_adapter_config(NAME, raw)?;

    RetentionPolicy::new(config.max_age, config.min_age)
        .validate()
        .map_err(|message| ConfigError::Adapter {
            adapter: NAME.to_string(),
            message,
        })?;

    let protect = compile_globs(&config.protect)?;

    Ok(Box::new(DockerAdapter {
        config,
        protect,
        containers: Vec::new(),
        images: Vec::new(),
    }))
}

/// Compile the `protect` glob patterns into a single [`GlobSet`].
fn compile_globs(patterns: &[String]) -> Result<GlobSet, ConfigError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = Glob::new(pat).map_err(|e| ConfigError::Adapter {
            adapter: NAME.to_string(),
            message: format!("invalid protect glob '{pat}': {e}"),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| ConfigError::Adapter {
        adapter: NAME.to_string(),
        message: format!("compiling protect globs: {e}"),
    })
}

// ── configuration ────────────────────────────────────────────────────────────

/// Typed view of the `[adapters.docker]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct DockerConfig {
    /// Normal-mode age threshold (default 7 days).
    #[serde(with = "humantime_serde")]
    max_age: Duration,
    /// Scavenge floor: never touch anything younger than this (default 2 days).
    #[serde(with = "humantime_serde")]
    min_age: Duration,
    /// Glob patterns for images/containers to never propose.
    protect: Vec<String>,
    /// Whether exited containers idle past policy are proposed (default `true`).
    remove_exited_containers: bool,
    /// Route deletions to the approvals queue instead of auto-deleting (default
    /// `false`).
    confirm: bool,
}

impl Default for DockerConfig {
    fn default() -> Self {
        DockerConfig {
            max_age: Duration::from_secs(7 * 24 * 60 * 60),
            min_age: Duration::from_secs(2 * 24 * 60 * 60),
            protect: Vec::new(),
            remove_exited_containers: true,
            confirm: false,
        }
    }
}

// ── persisted KV state ─────────────────────────────────────────────────────

/// KV value for `image:<id>` — usage tracking for one image.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImageState {
    /// Unix seconds when this image was first observed (drives the grace window).
    first_seen: u64,
    /// Unix seconds this image was last known to be used by a container.
    last_used: u64,
    /// The image's repo:tag references at last sighting.
    tags: Vec<String>,
}

/// KV value for `container:<id>` — last time this container was running.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContainerState {
    /// Unix seconds the container was last running (or finished).
    last_running: u64,
}

// ── in-run snapshot carried observe → plan ──────────────────────────────────

/// A container observed during this run.
#[derive(Debug, Clone)]
struct ObservedContainer {
    id: String,
    name: String,
    image_ref: String,
    /// `true` for a container in the `exited` state.
    exited: bool,
    /// Unix seconds the container was last running (grace-adjusted).
    last_running: u64,
    /// Matches a `protect` glob.
    protected: bool,
}

/// An image observed during this run, merged with its persisted state.
#[derive(Debug, Clone)]
struct ObservedImage {
    id: String,
    tags: Vec<String>,
    /// `true` when repository and tag are both `<none>`.
    dangling: bool,
    /// Referenced by at least one current container (running or stopped).
    referenced: bool,
    first_seen: u64,
    last_used: u64,
    /// Estimated on-disk size in bytes (from `docker image ls`).
    size: u64,
    /// Matches a `protect` glob.
    protected: bool,
}

// ── the adapter ──────────────────────────────────────────────────────────────

/// The docker adapter. Constructed by [`factory`]; state persists in the KV
/// bucket named `docker`.
struct DockerAdapter {
    config: DockerConfig,
    protect: GlobSet,
    containers: Vec<ObservedContainer>,
    images: Vec<ObservedImage>,
}

impl DockerAdapter {
    /// Probe availability: `docker version` must run and exit zero.
    fn ensure_available(ctx: &Ctx) -> Result<(), AdapterError> {
        let spec = CommandSpec::new("docker", ["version"]);
        match ctx.platform.run_command(&spec) {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => Err(AdapterError::unavailable(format!(
                "`docker version` exited {}: {}",
                out.status,
                out.stderr_string().trim()
            ))),
            Err(e) => Err(AdapterError::unavailable(format!(
                "cannot run docker (is it installed and running?): {e}"
            ))),
        }
    }

    /// Run a docker subcommand expected to succeed, returning its stdout.
    /// A failure to spawn or a non-zero exit is treated as [`Unavailable`], so
    /// the adapter is skipped this run and retried next.
    ///
    /// [`Unavailable`]: AdapterError::Unavailable
    fn run_ok(ctx: &Ctx, args: &[&str]) -> Result<String, AdapterError> {
        let spec = CommandSpec::new("docker", args.iter().copied());
        let out = ctx.platform.run_command(&spec).map_err(|e| {
            AdapterError::unavailable(format!("running `docker {}`: {e}", args.join(" ")))
        })?;
        if out.success() {
            Ok(out.stdout_string())
        } else {
            Err(AdapterError::unavailable(format!(
                "`docker {}` exited {}: {}",
                args.join(" "),
                out.status,
                out.stderr_string().trim()
            )))
        }
    }

    /// The retention policy derived from config.
    fn policy(&self) -> RetentionPolicy {
        RetentionPolicy::new(self.config.max_age, self.config.min_age)
    }

    /// Read the exited-container finish time (unix secs) via `docker inspect`.
    /// Best-effort: any failure yields `None`.
    fn inspect_finished_at(ctx: &Ctx, id: &str) -> Option<u64> {
        let spec = CommandSpec::new("docker", ["inspect".to_string(), id.to_string()]);
        let out = ctx.platform.run_command(&spec).ok()?;
        if !out.success() {
            return None;
        }
        let items: Vec<InspectItem> = serde_json::from_str(&out.stdout_string()).ok()?;
        let finished = items.into_iter().next()?.state.finished_at;
        parse_rfc3339_secs(&finished).and_then(|s| u64::try_from(s).ok())
    }
}

impl Adapter for DockerAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        Self::ensure_available(ctx)
    }

    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        Self::ensure_available(ctx)?;

        let now = to_unix(ctx.now());
        self.containers.clear();
        self.images.clear();

        // ── containers: `docker ps -a --format json` (one object per line) ──
        let ps_out = Self::run_ok(ctx, &["ps", "-a", "--format", "json"])?;
        let ps_lines: Vec<PsLine> = parse_ndjson(&ps_out);

        // Per-image usage derived from containers: image_ref → last-used unix.
        let mut current_containers: Vec<String> = Vec::new();
        for line in &ps_lines {
            if line.id.is_empty() {
                continue;
            }
            current_containers.push(line.id.clone());
            let exited = line.state.eq_ignore_ascii_case("exited");
            let running = line.state.eq_ignore_ascii_case("running");

            // First sighting of a container is treated as "running now" so grace
            // (§11.5) prevents deleting it in its discovery run; on later runs an
            // exited container falls back to its real FinishedAt.
            let seen_before = ctx
                .kv
                .get::<ContainerState>(&format!("container:{}", line.id))
                .ok()
                .flatten()
                .is_some();

            let last_running = if running {
                now
            } else if exited {
                if seen_before {
                    Self::inspect_finished_at(ctx, &line.id).unwrap_or(now)
                } else {
                    now
                }
            } else {
                // created / paused / restarting / dead: treat as used now.
                now
            };

            let _ = ctx.kv.set(
                &format!("container:{}", line.id),
                &ContainerState { last_running },
                ctx.now(),
            );

            let protected = self
                .protect
                .is_match(&line.names)
                .then_some(true)
                .or_else(|| self.protect.is_match(&line.image).then_some(true))
                .unwrap_or(false);

            self.containers.push(ObservedContainer {
                id: line.id.clone(),
                name: line.names.clone(),
                image_ref: line.image.clone(),
                exited,
                last_running,
                protected,
            });

            if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(
                        DecisionKind::Observed,
                        NAME,
                        format!("container:{}", line.id),
                    )
                    .label(format!("container {}", line.names))
                    .reason(format!("state {}", line.state)),
                );
            }
        }

        // ── images: `docker image ls --no-trunc --format json` ──
        let img_out = Self::run_ok(ctx, &["image", "ls", "--no-trunc", "--format", "json"])?;
        let img_lines: Vec<ImageLine> = parse_ndjson(&img_out);

        let mut current_images: Vec<String> = Vec::new();
        for line in &img_lines {
            if line.id.is_empty() {
                continue;
            }
            current_images.push(line.id.clone());

            let dangling = line.repository == "<none>" && line.tag == "<none>";
            let tags: Vec<String> = if dangling {
                Vec::new()
            } else {
                vec![format!("{}:{}", line.repository, line.tag)]
            };

            // Usage from any container that references this image.
            let mut used_at: Option<u64> = None;
            let mut referenced = false;
            for c in &self.containers {
                if image_matches_ref(&line.repository, &line.tag, &line.id, &c.image_ref) {
                    referenced = true;
                    let u = if c.exited { c.last_running } else { now };
                    used_at = Some(used_at.map_or(u, |prev| prev.max(u)));
                }
            }

            let key = format!("image:{}", line.id);
            let prior = ctx.kv.get::<ImageState>(&key).ok().flatten();
            let (first_seen, last_used) = match prior {
                Some(prev) => {
                    let last_used = used_at.map_or(prev.last_used, |u| prev.last_used.max(u));
                    (prev.first_seen, last_used)
                }
                None => (now, used_at.unwrap_or(now)),
            };

            let _ = ctx.kv.set(
                &key,
                &ImageState {
                    first_seen,
                    last_used,
                    tags: tags.clone(),
                },
                ctx.now(),
            );

            let protected = tags.iter().any(|t| self.protect.is_match(t))
                || self.protect.is_match(&line.id)
                || self.protect.is_match(short_id(&line.id));

            self.images.push(ObservedImage {
                id: line.id.clone(),
                tags,
                dangling,
                referenced,
                first_seen,
                last_used,
                size: parse_human_size(&line.size).unwrap_or(0),
                protected,
            });
        }

        // ── GC: drop KV entries for entities that have vanished ──
        gc_bucket(ctx, "container:", &current_containers);
        gc_bucket(ctx, "image:", &current_images);

        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let now = to_unix(ctx.now());
        let policy = self.policy();
        let pressure = ctx.pressure;
        let confirm = self.config.confirm;
        let mut candidates: Vec<Candidate> = Vec::new();

        // ① exited containers idle past policy.
        if self.config.remove_exited_containers {
            for c in &self.containers {
                if !c.exited {
                    continue;
                }
                if c.protected {
                    record_kept(ctx, &format!("container:{}", c.id), &c.name, 0, "protected");
                    continue;
                }
                let age = age_of(now, c.last_running);
                if policy.eligible(age, pressure) {
                    candidates.push(
                        Candidate::new(
                            format!("container:{}", c.id),
                            format!("container {} ({})", c.name, c.image_ref),
                            0,
                            unix_to_time(c.last_running),
                            Class::Rebuildable,
                        )
                        .confirm(confirm),
                    );
                } else {
                    record_kept(
                        ctx,
                        &format!("container:{}", c.id),
                        &c.name,
                        0,
                        "not idle past policy",
                    );
                }
            }
        }

        // ② tagged images referenced by no container past policy, then
        // ③ dangling images. Referenced images are held until their container
        // is gone (removing them would fail without `-f`, §11.9).
        for pass_dangling in [false, true] {
            for img in &self.images {
                if img.referenced || img.dangling != pass_dangling {
                    continue;
                }
                let label = image_label(img);
                let key = format!("image:{}", img.id);
                if img.protected {
                    record_kept(ctx, &key, &label, img.size, "protected");
                    continue;
                }
                // First-sighting grace (§11.5): never delete in the discovery run.
                if img.first_seen == now {
                    record_kept(ctx, &key, &label, img.size, "first sighting (grace)");
                    continue;
                }
                let age = age_of(now, img.last_used);
                if policy.eligible(age, pressure) {
                    candidates.push(
                        Candidate::new(
                            key,
                            label,
                            img.size,
                            unix_to_time(img.last_used),
                            Class::Cache,
                        )
                        .confirm(confirm),
                    );
                } else {
                    record_kept(ctx, &key, &label, img.size, "used too recently");
                }
            }
        }

        // ④ build cache — one coarse Rebuildable candidate sized from
        // `docker system df`. Only proposed under pressure; best-effort sizing.
        if pressure.deletes()
            && let Some(size) = build_cache_size(ctx)
            && size > 0
        {
            candidates.push(
                Candidate::new(
                    BUILD_CACHE_ID,
                    "docker build cache",
                    size,
                    ctx.now(),
                    Class::Rebuildable,
                )
                .confirm(confirm),
            );
        }

        // Record what we are proposing.
        if DecisionLog::compiled() {
            for c in &candidates {
                ctx.decide(
                    Decision::new(DecisionKind::Planned, NAME, c.id.clone())
                        .label(c.label.clone())
                        .bytes(c.bytes)
                        .age(c.age(ctx.now())),
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
        // Process containers before images (so `docker image rm` does not fail
        // on a container we also intend to remove), build cache last — but
        // return outcomes in the caller's original order.
        let mut order: Vec<usize> = (0..batch.len()).collect();
        order.sort_by_key(|&i| candidate_rank(&batch[i].id));

        let mut results: Vec<Option<Outcome>> = (0..batch.len()).map(|_| None).collect();
        for &i in &order {
            let c = &batch[i];
            let outcome = if let Some(id) = c.id.strip_prefix("container:") {
                self.remove_entity(ctx, c, "container", id)
            } else if let Some(id) = c.id.strip_prefix("image:") {
                self.remove_entity(ctx, c, "image", id)
            } else if c.id == BUILD_CACHE_ID {
                self.prune_build_cache(ctx, c)
            } else {
                Outcome::Skipped {
                    id: c.id.clone(),
                    reason: "unrecognized candidate id".to_string(),
                }
            };
            record_outcome(ctx, c, &outcome);
            results[i] = Some(outcome);
        }

        Ok(results.into_iter().flatten().collect())
    }
}

impl DockerAdapter {
    /// Remove one container or image via `docker <kind> rm <id>` (never `-f`).
    /// A non-zero exit becomes [`Outcome::Skipped`] (e.g. the image is back in
    /// use, or the entity is already gone); an I/O error becomes
    /// [`Outcome::Failed`]. On success the KV entry is dropped.
    fn remove_entity(&self, ctx: &Ctx, c: &Candidate, kind: &str, id: &str) -> Outcome {
        let spec = CommandSpec::new(
            "docker",
            [kind.to_string(), "rm".to_string(), id.to_string()],
        );
        match ctx.platform.run_command(&spec) {
            Ok(out) if out.success() => {
                let _ = ctx.kv.delete(&format!("{kind}:{id}"));
                Outcome::Removed {
                    id: c.id.clone(),
                    bytes: c.bytes,
                }
            }
            Ok(out) => Outcome::Skipped {
                id: c.id.clone(),
                reason: format!(
                    "`docker {kind} rm` exited {}: {}",
                    out.status,
                    out.stderr_string().trim()
                ),
            },
            Err(e) => Outcome::Failed {
                id: c.id.clone(),
                error: format!("running `docker {kind} rm`: {e}"),
            },
        }
    }

    /// Prune the build cache older than the pressure-appropriate age
    /// (`max_age` normally, `min_age` under scavenge).
    fn prune_build_cache(&self, ctx: &Ctx, c: &Candidate) -> Outcome {
        let age = match ctx.pressure {
            Pressure::Scavenge { .. } => self.config.min_age,
            _ => self.config.max_age,
        };
        // docker's `until` filter accepts a Go-style duration, so pass exact
        // seconds. Truncating to whole hours (`as_secs() / 3600`) would round a
        // sub-hour floor down to `until=0h`, which prunes ALL build cache
        // regardless of age — violating the `min_age`/`max_age` floor (§11.2).
        let filter = format!("until={}s", age.as_secs());
        let spec = CommandSpec::new(
            "docker",
            [
                "builder".to_string(),
                "prune".to_string(),
                "--force".to_string(),
                "--filter".to_string(),
                filter,
            ],
        );
        match ctx.platform.run_command(&spec) {
            Ok(out) if out.success() => Outcome::Removed {
                id: c.id.clone(),
                bytes: c.bytes,
            },
            Ok(out) => Outcome::Skipped {
                id: c.id.clone(),
                reason: format!(
                    "`docker builder prune` exited {}: {}",
                    out.status,
                    out.stderr_string().trim()
                ),
            },
            Err(e) => Outcome::Failed {
                id: c.id.clone(),
                error: format!("running `docker builder prune`: {e}"),
            },
        }
    }
}

// ── parsing helpers ────────────────────────────────────────────────────────

/// One line of `docker ps -a --format json`.
#[derive(Debug, Deserialize)]
struct PsLine {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "Names", default)]
    names: String,
    #[serde(rename = "State", default)]
    state: String,
}

/// One line of `docker image ls --format json`.
#[derive(Debug, Deserialize)]
struct ImageLine {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Repository", default)]
    repository: String,
    #[serde(rename = "Tag", default)]
    tag: String,
    #[serde(rename = "Size", default)]
    size: String,
}

/// One element of the `docker inspect` array (only the fields we need).
#[derive(Debug, Deserialize)]
struct InspectItem {
    #[serde(rename = "State", default)]
    state: InspectState,
}

/// The `.State` object from `docker inspect`.
#[derive(Debug, Default, Deserialize)]
struct InspectState {
    #[serde(rename = "FinishedAt", default)]
    finished_at: String,
}

/// One line of `docker system df --format json`.
#[derive(Debug, Deserialize)]
struct DfLine {
    #[serde(rename = "Type", default)]
    typ: String,
    #[serde(rename = "Size", default)]
    size: String,
}

/// Parse newline-delimited JSON objects, skipping blank/garbage lines.
fn parse_ndjson<T: serde::de::DeserializeOwned>(text: &str) -> Vec<T> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<T>(line) {
            out.push(v);
        }
    }
    out
}

/// Size of the docker build cache from `docker system df --format json`, or
/// `None` if unavailable/unparseable.
fn build_cache_size(ctx: &Ctx) -> Option<u64> {
    let spec = CommandSpec::new("docker", ["system", "df", "--format", "json"]);
    let out = ctx.platform.run_command(&spec).ok()?;
    if !out.success() {
        return None;
    }
    let text = out.stdout_string();
    let lines: Vec<DfLine> = parse_ndjson(&text);
    lines
        .iter()
        .find(|l| l.typ.eq_ignore_ascii_case("build cache"))
        .and_then(|l| parse_human_size(&l.size))
}

/// Whether a container's `Image` reference names the given image.
fn image_matches_ref(repo: &str, tag: &str, id: &str, cref: &str) -> bool {
    if repo != "<none>" && tag != "<none>" {
        if cref == format!("{repo}:{tag}") {
            return true;
        }
        if tag == "latest" && cref == repo {
            return true;
        }
    }
    ref_matches_id(id, cref)
}

/// Whether `cref` is a (hex) image-id prefix of `image_id`.
fn ref_matches_id(image_id: &str, cref: &str) -> bool {
    let idn = image_id.strip_prefix("sha256:").unwrap_or(image_id);
    let crefn = cref.strip_prefix("sha256:").unwrap_or(cref);
    crefn.len() >= 12 && crefn.chars().all(|c| c.is_ascii_hexdigit()) && idn.starts_with(crefn)
}

/// Human-readable label for an image candidate.
fn image_label(img: &ObservedImage) -> String {
    if img.tags.is_empty() {
        format!("<dangling> image {}", short_id(&img.id))
    } else {
        img.tags.join(", ")
    }
}

/// Short 12-char form of an image id (with any `sha256:` prefix stripped).
fn short_id(id: &str) -> &str {
    let s = id.strip_prefix("sha256:").unwrap_or(id);
    &s[..s.len().min(12)]
}

/// Parse a docker human size string (`"4.2GB"`, `"927.7kB"`, `"0B"`) to bytes.
fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "kB" | "KB" | "k" | "K" => 1e3,
        "MB" | "M" => 1e6,
        "GB" | "G" => 1e9,
        "TB" | "T" => 1e12,
        "PB" | "P" => 1e15,
        "KiB" => 1024.0,
        "MiB" => 1024f64.powi(2),
        "GiB" => 1024f64.powi(3),
        "TiB" => 1024f64.powi(4),
        "PiB" => 1024f64.powi(5),
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// Parse an RFC3339 UTC timestamp to unix seconds. Returns `None` for the docker
/// zero value (`0001-01-01T00:00:00Z`) or anything pre-epoch/unparseable.
/// Fractional seconds and timezone suffixes are ignored (docker emits UTC `Z`).
fn parse_rfc3339_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + min * 60 + sec;
    (secs >= 0).then_some(secs)
}

/// Days since the unix epoch for a proleptic-Gregorian date
/// (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Rank a candidate id so containers execute before images before build cache.
fn candidate_rank(id: &str) -> u8 {
    if id.starts_with("container:") {
        0
    } else if id.starts_with("image:") {
        1
    } else if id == BUILD_CACHE_ID {
        2
    } else {
        3
    }
}

/// Delete KV entries under `prefix` whose id is not in `present`.
fn gc_bucket(ctx: &Ctx, prefix: &str, present: &[String]) {
    let Ok(keys) = ctx.kv.keys(prefix) else {
        return;
    };
    for key in keys {
        let id = &key[prefix.len()..];
        if !present.iter().any(|p| p == id) {
            let _ = ctx.kv.delete(&key);
        }
    }
}

/// Record a `Kept` decision (guarded so lean builds pay nothing).
fn record_kept(ctx: &Ctx, id: &str, label: &str, bytes: u64, reason: &str) {
    if DecisionLog::compiled() {
        ctx.decide(
            Decision::new(DecisionKind::Kept, NAME, id.to_string())
                .label(label.to_string())
                .bytes(bytes)
                .reason(reason.to_string()),
        );
    }
}

/// Record the terminal decision for an execute outcome.
fn record_outcome(ctx: &Ctx, c: &Candidate, outcome: &Outcome) {
    if !DecisionLog::compiled() {
        return;
    }
    let (kind, reason) = match outcome {
        Outcome::Removed { .. } => (DecisionKind::Deleted, None),
        Outcome::Skipped { reason, .. } => (DecisionKind::Skipped, Some(reason.clone())),
        Outcome::Failed { error, .. } => (DecisionKind::Failed, Some(error.clone())),
    };
    let mut d = Decision::new(kind, NAME, c.id.clone())
        .label(c.label.clone())
        .bytes(c.bytes)
        .age(c.age(ctx.now()));
    if let Some(r) = reason {
        d = d.reason(r);
    }
    ctx.decide(d);
}

/// Convert a [`SystemTime`] to whole unix seconds (saturating at the epoch).
fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convert unix seconds back to a [`SystemTime`].
fn unix_to_time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Age (`now - stored`) in whole seconds, saturating to zero.
fn age_of(now: u64, stored: u64) -> Duration {
    Duration::from_secs(now.saturating_sub(stored))
}
