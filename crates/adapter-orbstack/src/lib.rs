//! `disk-saver-adapter-orbstack` — the `orbstack` adapter.
//!
//! OrbStack runs Docker, and any Linux machines, inside one VM whose disk is a
//! sparse file: `~/Library/Group Containers/HUAQ24HBR6.dev.orbstack/data/data.img.raw`.
//! Its apparent size is the VM's ceiling (hundreds of gigabytes); its *allocated*
//! blocks are what it really costs the host. Ordinary deletions inside the VM are
//! handed back to macOS within a minute or so, but data Docker no longer reports —
//! leaked layers and snapshots — can stay allocated indefinitely. One machine's
//! image held 40.5 GB while `docker system df` accounted for 9.5 GB, and still
//! held 40.5 GB after the `docker` adapter had pruned the build cache to nothing.
//! Only `orbctl reset` got it back.
//!
//! So this adapter watches the *gap* between what the image holds and what Docker
//! accounts for. When a large gap has persisted — so it is not space still being
//! reclaimed — and a reset would lose nothing that cannot be pulled again, it
//! proposes the reset. Always for approval: a reset deletes every image, and it is
//! never automatic (ARCHITECTURE.md §9).
//!
//! It drives OrbStack exclusively through
//! [`Platform::run_command`](disk_saver_core::Platform::run_command), so it is
//! testable against a fake platform. On Linux there is no `orbctl`, so the adapter
//! is simply unavailable.
//!
//! # Phases
//!
//! * **check / availability** — `orbctl status` must print `Running`, and the data
//!   image must exist as a regular file. Anything else is
//!   [`AdapterError::Unavailable`]. It never starts OrbStack to find out.
//! * **observe** — every run, at any pressure: measure the image's allocated bytes
//!   (`du -k`) and what Docker accounts for (`docker system df`, every type
//!   summed). A gap of at least `min_gap` stamps `gap_since` in KV if it is not
//!   already set; a smaller gap clears it. That persistence is the whole signal: a
//!   gap that is merely being reclaimed closes before it can age.
//! * **plan** — once the gap has persisted past the retention policy, and only if
//!   there are no Linux machines, no containers, no volumes and no Kubernetes,
//!   proposes one candidate, `orbstack-reset`, that always requires confirmation.
//!   Anything blocking it is recorded as a kept decision with the reason.
//! * **execute** — re-checks availability, the gap and every guard (a change means
//!   [`Outcome::Skipped`]), runs `orbctl reset -y` then `orbctl start`, so OrbStack
//!   is left running as it was, and reports what the image shrank by.
//!
//! # KV state (bucket `orbstack`)
//!
//! * `gap_since → unix seconds` — when the current gap was first seen at or above
//!   `min_gap`. Absent while there is no such gap.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, CommandSpec, ConfigCx, ConfigError,
    Ctx, Decision, DecisionKind, DecisionLog, FileKind, Outcome, Pressure, RetentionPolicy,
    expand_tilde,
};
use serde::Deserialize;

/// The stable adapter name (config section, KV bucket, log target).
const NAME: &str = "orbstack";

/// The id of the one candidate this adapter ever proposes.
const RESET_ID: &str = "orbstack-reset";

/// Stable label for observed/kept decisions. The candidate's own label carries
/// sizes that change every run; keeping these rows on one label keeps the
/// decision log's item table from growing a row per run.
const ITEM_LABEL: &str = "OrbStack data image";

/// KV key holding when the current gap was first seen (unix seconds).
const GAP_SINCE: &str = "gap_since";

/// OrbStack's control CLI.
///
/// `orbctl` rather than its `orb` alias: `orb` given anything it does not
/// recognise as a subcommand runs it *inside a Linux machine*, which is not
/// something a janitor should ever be one typo or one renamed subcommand away from.
const ORBCTL: &str = "orbctl";

/// The data image relative to the home directory. `HUAQ24HBR6` is OrbStack's
/// team id, so this path is the same on every Mac.
const DEFAULT_DATA_IMAGE: &str =
    "Library/Group Containers/HUAQ24HBR6.dev.orbstack/data/data.img.raw";

/// `orbctl reset` stops every machine and deletes the whole image; give it far
/// longer than the 60s default.
const RESET_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// `orbctl start` boots the VM and waits for the Docker engine.
const START_TIMEOUT: Duration = Duration::from_secs(2 * 60);

const DAY: u64 = 24 * 60 * 60;

/// The factory the CLI registry uses to build the orbstack adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

/// Validate the configuration; any problem is a [`ConfigError::Adapter`].
fn build(config: OrbStackConfig, cx: &ConfigCx) -> Result<OrbStackAdapter, ConfigError> {
    if config.confirm == Some(false) {
        return Err(cx.err(
            "`confirm` cannot be false: an OrbStack reset deletes every image, \
             so it always waits for `disk-saver review`",
        ));
    }
    if config.min_gap == 0 {
        return Err(cx.err("min_gap must be greater than zero"));
    }
    if let Some(path) = &config.data_image
        && !(path.is_absolute() || path.starts_with("~"))
    {
        return Err(cx.err(format!(
            "data_image must be an absolute path or start with ~, got {}",
            path.display()
        )));
    }
    if config.docker_context.trim().is_empty() {
        return Err(cx.err("docker_context must not be empty"));
    }
    Ok(OrbStackAdapter {
        policy: cx.retention(config.max_age, config.min_age)?,
        min_gap: config.min_gap,
        data_image: config.data_image,
        docker_context: config.docker_context,
        observed: None,
    })
}

// ── configuration ────────────────────────────────────────────────────────────

/// Typed view of the `[adapters.orbstack]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OrbStackConfig {
    /// Normal mode: how long the gap must persist before a reset is proposed
    /// (default 3 days).
    #[serde(with = "humantime_serde")]
    max_age: Duration,
    /// Scavenge floor: never propose a reset for a gap younger than this
    /// (default 1 day). Deleted data has normally been handed back within a
    /// minute, so a day already rules out a reclaim still in flight.
    #[serde(with = "humantime_serde")]
    min_age: Duration,
    /// The smallest gap worth a reset, as a size string such as `"10GB"`
    /// (default 10 GB). Smaller gaps are noise — filesystem metadata, a fresh
    /// image's own overhead — and never start the clock.
    #[serde(deserialize_with = "de_size")]
    min_gap: u64,
    /// The data image, if not at OrbStack's standard location. `~` is expanded.
    data_image: Option<PathBuf>,
    /// The docker CLI context that reaches OrbStack's engine (default
    /// `"orbstack"`, which OrbStack creates). Named explicitly so the guards
    /// never inspect some other engine — Docker Desktop, colima — that happens to
    /// be the current context, and then reset OrbStack's containers away.
    docker_context: String,
    /// Accepted only so `confirm = false` can be refused with a clear message
    /// rather than an "unknown field" one: every other adapter has this key, and
    /// here it cannot be turned off.
    confirm: Option<bool>,
}

impl Default for OrbStackConfig {
    fn default() -> Self {
        OrbStackConfig {
            max_age: Duration::from_secs(3 * DAY),
            min_age: Duration::from_secs(DAY),
            min_gap: 10_000_000_000,
            data_image: None,
            docker_context: "orbstack".to_string(),
            confirm: None,
        }
    }
}

/// Deserialize a size string (`"10GB"`, `"512MiB"`) with the same parser as the
/// engine's absolute [`Threshold`](disk_saver_core::Threshold)s.
fn de_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.trim()
        .parse::<bytesize::ByteSize>()
        .map(|b| b.0)
        .map_err(|e| serde::de::Error::custom(format!("invalid size {s:?}: {e}")))
}

// ── the adapter ──────────────────────────────────────────────────────────────

/// What the data image holds against what Docker accounts for, at one instant.
#[derive(Debug, Clone, Copy)]
struct Measurement {
    /// Bytes the sparse image really occupies on the host.
    allocated: u64,
    /// Bytes Docker reports across images, containers, volumes and build cache.
    accounted: u64,
    /// How many images Docker reports, for the label; `None` if it did not say.
    images: Option<u64>,
}

impl Measurement {
    /// Allocated space Docker does not account for.
    fn gap(&self) -> u64 {
        self.allocated.saturating_sub(self.accounted)
    }
}

/// What `observe` found, carried to `plan` within the run.
#[derive(Debug, Clone, Copy)]
struct Observation {
    measurement: Measurement,
    /// When the current gap was first seen (unix seconds); `None` if the gap is
    /// below `min_gap`.
    gap_since: Option<u64>,
}

/// The orbstack adapter. Constructed by [`factory`]; state persists in the KV
/// bucket named `orbstack`.
struct OrbStackAdapter {
    /// Validated at construction, so no phase re-derives it.
    policy: RetentionPolicy,
    min_gap: u64,
    data_image: Option<PathBuf>,
    docker_context: String,
    observed: Option<Observation>,
}

impl OrbStackAdapter {
    /// The data image path: the configured one (`~` expanded) or the standard one.
    fn image_path(&self, ctx: &Ctx) -> PathBuf {
        let home = ctx.platform.home_dir();
        match &self.data_image {
            Some(path) => expand_tilde(path, &home),
            None => home.join(DEFAULT_DATA_IMAGE),
        }
    }

    /// OrbStack is running and its data image exists; returns the image path.
    /// The error is the reason it is unavailable.
    fn ensure_available(&self, ctx: &Ctx) -> Result<PathBuf, String> {
        let spec = CommandSpec::new(ORBCTL, ["status"]);
        let out = ctx
            .platform
            .run_command(&spec)
            .map_err(|e| format!("cannot run `orbctl status` (is OrbStack installed?): {e}"))?;
        let status = out.stdout_string();
        let status = status.trim();
        if !(out.success() && status == "Running") {
            let detail = if status.is_empty() {
                format!(
                    "`orbctl status` exited {}: {}",
                    out.status,
                    out.stderr_string().trim()
                )
            } else {
                format!("status: {status}")
            };
            return Err(format!("OrbStack is not running ({detail})"));
        }

        let image = self.image_path(ctx);
        match ctx.platform.metadata(&image) {
            Ok(meta) if meta.kind == FileKind::File => Ok(image),
            Ok(meta) => Err(format!(
                "OrbStack data image {} is not a regular file ({:?}); set `data_image` to the real file",
                image.display(),
                meta.kind
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
                "OrbStack data image not found at {}",
                image.display()
            )),
            Err(e) => Err(format!(
                "cannot read OrbStack data image {}: {e}",
                image.display()
            )),
        }
    }

    /// Measure the image against Docker's accounting. Any failure — including
    /// output that does not parse — is an error rather than a guess: reading
    /// Docker's figure as zero would make every image look like waste.
    fn measure(&self, ctx: &Ctx, image: &Path) -> Result<Measurement, String> {
        let allocated = allocated_bytes(ctx, image)?;
        let df = run(
            ctx,
            &self.docker(&["system", "df", "--format", "{{json .}}"]),
        )?;
        let (accounted, images) = parse_system_df(&df)?;
        Ok(Measurement {
            allocated,
            accounted,
            images,
        })
    }

    /// A `docker` invocation pinned to OrbStack's context.
    fn docker(&self, args: &[&str]) -> CommandSpec {
        let mut all = vec!["--context", self.docker_context.as_str()];
        all.extend_from_slice(args);
        CommandSpec::new("docker", all)
    }

    /// Everything a reset would destroy that cannot be pulled again, as
    /// human-readable reasons; empty means a reset is safe. A guard that cannot be
    /// evaluated blocks too: not knowing is not permission.
    fn blockers(&self, ctx: &Ctx) -> Vec<String> {
        let mut blockers = Vec::new();

        // `orbctl list -q` prints one machine name per line, nothing when there
        // are none.
        match run(ctx, &CommandSpec::new(ORBCTL, ["list", "-q"])) {
            Ok(text) => {
                let names = non_empty_lines(&text);
                if !names.is_empty() {
                    blockers.push(format!(
                        "{} ({})",
                        count(names.len() as u64, "Linux machine", "Linux machines"),
                        names.join(", ")
                    ));
                }
            }
            Err(e) => blockers.push(format!("cannot list Linux machines: {e}")),
        }

        // A reset deletes the Kubernetes cluster too, and its pods need not show
        // up in `docker ps`.
        match run(
            ctx,
            &CommandSpec::new(ORBCTL, ["config", "get", "k8s.enable"]),
        ) {
            Ok(text) => match text.trim() {
                "false" => {}
                "true" => blockers.push("Kubernetes is enabled".to_string()),
                other => blockers.push(format!("unexpected k8s.enable value {other:?}")),
            },
            Err(e) => blockers.push(format!("cannot read k8s.enable: {e}")),
        }

        match run(ctx, &self.docker(&["ps", "-aq"])) {
            Ok(text) => {
                let n = non_empty_lines(&text).len() as u64;
                if n > 0 {
                    blockers.push(count(n, "container", "containers"));
                }
            }
            Err(e) => blockers.push(format!("cannot list containers: {e}")),
        }

        // Volumes hold the one kind of data nothing can rebuild (§20.4).
        match run(ctx, &self.docker(&["volume", "ls", "-q"])) {
            Ok(text) => {
                let n = non_empty_lines(&text).len() as u64;
                if n > 0 {
                    blockers.push(format!(
                        "{} (data nothing can rebuild)",
                        count(n, "volume", "volumes")
                    ));
                }
            }
            Err(e) => blockers.push(format!("cannot list volumes: {e}")),
        }

        blockers
    }

    /// Why a gap of `age` is not yet eligible at `pressure`.
    fn too_young(&self, age: Duration, pressure: Pressure) -> String {
        match pressure {
            Pressure::Comfortable => "disk pressure is comfortable".to_string(),
            Pressure::Normal => format!(
                "gap seen for {}; a reset is proposed after {} (max_age)",
                human_age(age),
                humantime::format_duration(self.policy.max_age)
            ),
            Pressure::Scavenge { .. } => format!(
                "gap seen for {}; even under scavenge a reset waits {} (min_age)",
                human_age(age),
                humantime::format_duration(self.policy.min_age)
            ),
        }
    }

    /// One-line account of a measurement against `min_gap`.
    fn describe(&self, m: &Measurement) -> String {
        format!(
            "image holds {}, Docker accounts for {}: gap {} (min_gap {})",
            human_bytes(m.allocated),
            human_bytes(m.accounted),
            human_bytes(m.gap()),
            human_bytes(self.min_gap)
        )
    }

    /// Re-check everything, then reset and restart OrbStack. Returns the outcome
    /// and, for a reset that worked but left OrbStack stopped, a note for the
    /// decision log.
    fn reset(&self, ctx: &Ctx, c: &Candidate) -> (Outcome, Option<String>) {
        let skipped = |reason: String| {
            (
                Outcome::Skipped {
                    id: c.id.clone(),
                    reason,
                },
                None,
            )
        };

        // Approval may be days old: nothing planned then is trusted now.
        let image = match self.ensure_available(ctx) {
            Ok(image) => image,
            Err(e) => return skipped(e),
        };
        let before = match self.measure(ctx, &image) {
            Ok(m) => m,
            Err(e) => return skipped(e),
        };
        if before.gap() < self.min_gap {
            return skipped(format!("the gap has closed: {}", self.describe(&before)));
        }
        let blockers = self.blockers(ctx);
        if !blockers.is_empty() {
            return skipped(blocked(&blockers));
        }

        tracing::warn!(
            adapter = NAME,
            allocated = before.allocated,
            accounted = before.accounted,
            "resetting OrbStack"
        );
        let reset = run(
            ctx,
            &CommandSpec::new(ORBCTL, ["reset", "-y"]).with_timeout(RESET_TIMEOUT),
        );
        // OrbStack was running before; leave it running whatever the reset did.
        // A reset that failed part-way may have stopped it, and starting an
        // already-running OrbStack does nothing.
        let start = run(
            ctx,
            &CommandSpec::new(ORBCTL, ["start"]).with_timeout(START_TIMEOUT),
        );
        if let Err(e) = &start {
            tracing::error!(
                adapter = NAME,
                error = %e,
                "OrbStack did not start again after the reset; run `orbctl start`"
            );
        }

        if let Err(e) = reset {
            let error = match start {
                Ok(_) => e,
                Err(s) => format!("{e}; OrbStack then failed to start: {s}"),
            };
            return (
                Outcome::Failed {
                    id: c.id.clone(),
                    error,
                },
                None,
            );
        }

        // The gap went with the reset; the next observe measures afresh.
        if let Err(e) = ctx.kv.delete(GAP_SINCE) {
            tracing::warn!(adapter = NAME, error = %e, "could not clear gap_since");
        }

        let bytes = match allocated_after_reset(ctx, &image) {
            Ok(after) => before.allocated.saturating_sub(after),
            Err(e) => {
                tracing::warn!(
                    adapter = NAME,
                    error = %e,
                    "could not re-measure the data image; reporting the planned estimate"
                );
                c.bytes
            }
        };
        let note = start.err().map(|e| {
            format!("OrbStack was reset but did not start again; run `orbctl start`: {e}")
        });
        (
            Outcome::Removed {
                id: c.id.clone(),
                bytes,
            },
            note,
        )
    }
}

impl Adapter for OrbStackAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        self.ensure_available(ctx)
            .map(drop)
            .map_err(AdapterError::Unavailable)
    }

    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        self.observed = None;
        let image = self
            .ensure_available(ctx)
            .map_err(AdapterError::Unavailable)?;
        let m = self
            .measure(ctx, &image)
            .map_err(AdapterError::Unavailable)?;

        let now = to_unix(ctx.now());
        let gap_since = if m.gap() >= self.min_gap {
            match ctx.kv.get::<u64>(GAP_SINCE).map_err(failed)? {
                Some(since) => Some(since),
                None => {
                    ctx.kv.set(GAP_SINCE, &now, ctx.now()).map_err(failed)?;
                    Some(now)
                }
            }
        } else {
            ctx.kv.delete(GAP_SINCE).map_err(failed)?;
            None
        };

        if DecisionLog::compiled() {
            let age = gap_since.map_or(Duration::ZERO, |since| age_of(now, since));
            ctx.decide(
                Decision::new(DecisionKind::Observed, NAME, RESET_ID)
                    .label(ITEM_LABEL)
                    .bytes(m.allocated)
                    .age(age)
                    .reason(self.describe(&m)),
            );
        }

        self.observed = Some(Observation {
            measurement: m,
            gap_since,
        });
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let Some(obs) = self.observed else {
            return Ok(Vec::new());
        };
        let m = obs.measurement;
        let now = to_unix(ctx.now());

        let Some(since) = obs.gap_since else {
            let reason = format!("no gap worth a reset: {}", self.describe(&m));
            record_kept(ctx, m.allocated, Duration::ZERO, reason);
            return Ok(Vec::new());
        };
        let age = age_of(now, since);
        if !self.policy.eligible(age, ctx.pressure) {
            record_kept(ctx, m.allocated, age, self.too_young(age, ctx.pressure));
            return Ok(Vec::new());
        }
        let blockers = self.blockers(ctx);
        if !blockers.is_empty() {
            record_kept(ctx, m.allocated, age, blocked(&blockers));
            return Ok(Vec::new());
        }

        let candidate = Candidate::new(
            RESET_ID,
            reset_label(&m, age),
            m.allocated,
            unix_to_time(since),
            Class::Cache,
        )
        // Never configurable: a reset deletes every image, and the guards can
        // only rule out the losses they know how to look for.
        .confirm(true);

        if DecisionLog::compiled() {
            ctx.decide(
                Decision::new(DecisionKind::Planned, NAME, RESET_ID)
                    .label(candidate.label.clone())
                    .bytes(candidate.bytes)
                    .age(age),
            );
        }
        Ok(vec![candidate])
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let mut outcomes = Vec::with_capacity(batch.len());
        for c in batch {
            // Every reset is planned for approval; one that arrives unflagged
            // did not come from this adapter's plan, so it is never run.
            let (outcome, note) = if c.id == RESET_ID && c.requires_confirmation {
                self.reset(ctx, c)
            } else if c.id == RESET_ID {
                (
                    Outcome::Skipped {
                        id: c.id.clone(),
                        reason: "a reset always needs approval".to_string(),
                    },
                    None,
                )
            } else {
                (
                    Outcome::Skipped {
                        id: c.id.clone(),
                        reason: "unrecognized candidate id".to_string(),
                    },
                    None,
                )
            };
            record_outcome(ctx, c, &outcome, note);
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

// ── subprocess + measurement helpers ─────────────────────────────────────────

/// Run a command expected to succeed and return its stdout; a spawn failure or
/// non-zero exit is an error naming the command.
fn run(ctx: &Ctx, spec: &CommandSpec) -> Result<String, String> {
    let shown = format!("{} {}", spec.program, spec.args.join(" "));
    let out = ctx
        .platform
        .run_command(spec)
        .map_err(|e| format!("running `{shown}`: {e}"))?;
    if out.success() {
        Ok(out.stdout_string())
    } else {
        Err(format!(
            "`{shown}` exited {}: {}",
            out.status,
            out.stderr_string().trim()
        ))
    }
}

/// Bytes of blocks `image` occupies, from `du -k` — the [`Platform`] only
/// reports apparent sizes, which for a sparse image is the VM's ceiling, not its
/// cost. `-k` (KiB) is POSIX, so this reads the same on macOS and Linux.
///
/// [`Platform`]: disk_saver_core::Platform
fn allocated_bytes(ctx: &Ctx, image: &Path) -> Result<u64, String> {
    let path = image.to_string_lossy().into_owned();
    let out = run(ctx, &CommandSpec::new("du", ["-k".to_string(), path]))?;
    parse_du_kib(&out).ok_or_else(|| format!("unparseable `du -k` output: {:?}", out.trim()))
}

/// [`allocated_bytes`] after a reset, when an image that is gone holds nothing.
fn allocated_after_reset(ctx: &Ctx, image: &Path) -> Result<u64, String> {
    match ctx.platform.metadata(image) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        _ => allocated_bytes(ctx, image),
    }
}

/// The first field of `du -k` output, in KiB, as bytes.
fn parse_du_kib(text: &str) -> Option<u64> {
    let field = text.split_whitespace().next()?;
    field.parse::<u64>().ok()?.checked_mul(1024)
}

/// One line of `docker system df --format '{{json .}}'`.
#[derive(Debug, Deserialize)]
struct DfLine {
    #[serde(rename = "Type", default)]
    typ: String,
    #[serde(rename = "Size", default)]
    size: String,
    /// A string (`"4"`) in current docker; a number is accepted too.
    #[serde(rename = "TotalCount", default)]
    total_count: serde_json::Value,
}

/// Total bytes Docker accounts for across every type, and the image count.
///
/// Strict on purpose: a line that does not parse fails the whole measurement,
/// since a type silently dropped from the sum would show up as reclaimable gap.
fn parse_system_df(text: &str) -> Result<(u64, Option<u64>), String> {
    let mut total: u64 = 0;
    let mut images = None;
    let mut lines = 0;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let row: DfLine = serde_json::from_str(line)
            .map_err(|e| format!("unparseable `docker system df` line {line:?}: {e}"))?;
        if row.typ.is_empty() {
            return Err(format!("`docker system df` line without a Type: {line:?}"));
        }
        let size = parse_human_size(&row.size).ok_or_else(|| {
            format!(
                "unparseable `docker system df` size {:?} for {}",
                row.size, row.typ
            )
        })?;
        total = total.saturating_add(size);
        if row.typ.eq_ignore_ascii_case("images") {
            images = match &row.total_count {
                serde_json::Value::String(s) => s.trim().parse().ok(),
                serde_json::Value::Number(n) => n.as_u64(),
                _ => None,
            };
        }
        lines += 1;
    }
    if lines == 0 {
        return Err("`docker system df` printed nothing".to_string());
    }
    Ok((total, images))
}

/// Parse a docker human size string (`"4.2GB"`, `"927.7kB"`, `"0B"`) to bytes.
///
/// Copied from the `docker` adapter (`crates/adapter-docker/src/lib.rs`), where
/// it is private. Docker prints decimal (SI) units.
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

/// The trimmed, non-empty lines of a command's output.
fn non_empty_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect()
}

// ── wording ──────────────────────────────────────────────────────────────────

/// The candidate's one-line label, e.g. "reset OrbStack: Docker accounts for
/// 9.5 GB of the 40.5 GB its disk image holds (gap seen for 3 days); removes 4
/// images (re-pullable); no containers, volumes or machines".
fn reset_label(m: &Measurement, age: Duration) -> String {
    let images = match m.images {
        Some(0) => "removes no images".to_string(),
        Some(n) => format!("removes {} (re-pullable)", count(n, "image", "images")),
        None => "removes every image (re-pullable)".to_string(),
    };
    format!(
        "reset OrbStack: Docker accounts for {} of the {} its disk image holds \
         (gap seen for {}); {images}; no containers, volumes or machines",
        human_bytes(m.accounted),
        human_bytes(m.allocated),
        human_age(age),
    )
}

/// The reason recorded when guards block a reset.
fn blocked(blockers: &[String]) -> String {
    format!("reset blocked: {}", blockers.join("; "))
}

/// `"1 volume"`, `"3 volumes"`.
fn count(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Bytes as the engine prints them (`"9.5 GB"`).
fn human_bytes(b: u64) -> String {
    bytesize::ByteSize(b).display().si().to_string()
}

/// An age in its largest whole unit: `"3 days"`, `"5 hours"`, `"12 minutes"`.
fn human_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs >= DAY {
        count(secs / DAY, "day", "days")
    } else if secs >= 3600 {
        count(secs / 3600, "hour", "hours")
    } else {
        count(secs / 60, "minute", "minutes")
    }
}

// ── decisions ────────────────────────────────────────────────────────────────

/// Record a `Kept` decision (guarded so lean builds pay nothing).
fn record_kept(ctx: &Ctx, bytes: u64, age: Duration, reason: String) {
    if DecisionLog::compiled() {
        ctx.decide(
            Decision::new(DecisionKind::Kept, NAME, RESET_ID)
                .label(ITEM_LABEL)
                .bytes(bytes)
                .age(age)
                .reason(reason),
        );
    }
}

/// Record the terminal decision for an execute outcome. `note` is a reason worth
/// keeping even for a removal — a reset that left OrbStack stopped.
fn record_outcome(ctx: &Ctx, c: &Candidate, outcome: &Outcome, note: Option<String>) {
    if !DecisionLog::compiled() {
        return;
    }
    let (kind, bytes, reason) = match outcome {
        Outcome::Removed { bytes, .. } => (DecisionKind::Deleted, *bytes, note),
        Outcome::Skipped { reason, .. } => (DecisionKind::Skipped, c.bytes, Some(reason.clone())),
        Outcome::Failed { error, .. } => (DecisionKind::Failed, c.bytes, Some(error.clone())),
    };
    let mut d = Decision::new(kind, NAME, c.id.clone())
        .label(c.label.clone())
        .bytes(bytes)
        .age(c.age(ctx.now()));
    if let Some(r) = reason {
        d = d.reason(r);
    }
    ctx.decide(d);
}

// ── time + error helpers ─────────────────────────────────────────────────────

/// A KV failure is a genuine failure, not an unavailable dependency.
fn failed(e: impl std::error::Error + Send + Sync + 'static) -> AdapterError {
    AdapterError::Failed(anyhow::Error::new(e))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn du_first_field_is_kib() {
        assert_eq!(
            parse_du_kib("9502004\t/Users/x/data.img.raw\n"),
            Some(9_502_004 * 1024)
        );
        // A path with spaces after the tab does not matter.
        assert_eq!(parse_du_kib("12 /a b/c"), Some(12 * 1024));
        assert_eq!(parse_du_kib(""), None);
        assert_eq!(parse_du_kib("du: cannot access"), None);
    }

    #[test]
    fn system_df_sums_every_type_and_counts_images() {
        // Real output from OrbStack 2.2.4 / Docker 28.
        let text = concat!(
            r#"{"Active":"0","Reclaimable":"14.56MB (99%)","Size":"14.57MB","TotalCount":"1","Type":"Images"}"#,
            "\n",
            r#"{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Containers"}"#,
            "\n",
            r#"{"Active":"0","Reclaimable":"0B","Size":"1.5GB","TotalCount":"0","Type":"Local Volumes"}"#,
            "\n",
            r#"{"Active":"0","Reclaimable":"0B","Size":"9.523GB","TotalCount":"3","Type":"Build Cache"}"#,
            "\n",
        );
        let (total, images) = parse_system_df(text).unwrap();
        assert_eq!(total, 14_570_000 + 1_500_000_000 + 9_523_000_000);
        assert_eq!(images, Some(1));
    }

    #[test]
    fn system_df_accepts_a_numeric_count() {
        let (_, images) =
            parse_system_df(r#"{"Type":"Images","Size":"1GB","TotalCount":4}"#).unwrap();
        assert_eq!(images, Some(4));
    }

    #[test]
    fn system_df_refuses_to_guess() {
        assert!(parse_system_df("").is_err());
        assert!(parse_system_df("not json").is_err());
        assert!(parse_system_df(r#"{"Type":"Images","Size":"lots"}"#).is_err());
        assert!(parse_system_df(r#"{"Size":"1GB"}"#).is_err());
    }

    #[test]
    fn human_sizes_parse_decimal_and_binary_units() {
        assert_eq!(parse_human_size("14.57MB"), Some(14_570_000));
        assert_eq!(parse_human_size("9.523GB"), Some(9_523_000_000));
        assert_eq!(parse_human_size("0B"), Some(0));
        assert_eq!(parse_human_size("927.7kB"), Some(927_700));
        assert_eq!(parse_human_size("1GiB"), Some(1 << 30));
        assert_eq!(parse_human_size("12 parsecs"), None);
    }

    #[test]
    fn ages_read_in_their_largest_unit() {
        assert_eq!(human_age(Duration::from_secs(3 * DAY + 7200)), "3 days");
        assert_eq!(human_age(Duration::from_secs(DAY)), "1 day");
        assert_eq!(human_age(Duration::from_secs(5 * 3600 + 59)), "5 hours");
        assert_eq!(human_age(Duration::from_secs(60)), "1 minute");
        assert_eq!(human_age(Duration::ZERO), "0 minutes");
    }

    #[test]
    fn label_names_sizes_age_and_images() {
        let m = Measurement {
            allocated: 40_500_000_000,
            accounted: 9_500_000_000,
            images: Some(4),
        };
        assert_eq!(
            reset_label(&m, Duration::from_secs(3 * DAY)),
            "reset OrbStack: Docker accounts for 9.5 GB of the 40.5 GB its disk image holds \
             (gap seen for 3 days); removes 4 images (re-pullable); no containers, volumes or \
             machines"
        );
        let one = Measurement {
            images: Some(1),
            ..m
        };
        assert!(reset_label(&one, Duration::ZERO).contains("removes 1 image (re-pullable)"));
        let none = Measurement {
            images: Some(0),
            ..m
        };
        assert!(reset_label(&none, Duration::ZERO).contains("removes no images;"));
        let unknown = Measurement { images: None, ..m };
        assert!(reset_label(&unknown, Duration::ZERO).contains("removes every image"));
    }
}
