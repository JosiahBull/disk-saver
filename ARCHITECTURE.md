# disk-saver — Architecture & Design

**Status:** Implemented; shipping as v0.1. This is the design of record — it describes what
the code does and why it does it that way. §20 records the decisions that moved between the
original design draft and the shipped program.
**Targets:** macOS + Linux. Rust 2024 edition, MSRV 1.88.

`disk-saver` is a small, boring, trustworthy janitor. It wakes up on a schedule, looks at
how much disk space is free, and — only if space is actually needed — deletes things that
demonstrably haven't been used in a while. It is built as a set of independent *adapters*
(docker, node_modules, rust target/, python caches, trash, …) orchestrated by a tiny
engine. It deletes low-impact, regenerable things on its own; for anything scarier it
queues the item and asks the user to confirm.

---

## Table of contents

1. [Goals & non-goals](#1-goals--non-goals)
2. [System overview](#2-system-overview)
3. [Workspace layout](#3-workspace-layout)
4. [Core concepts](#4-core-concepts)
5. [Platform abstraction](#5-platform-abstraction)
6. [State: the KV store](#6-state-the-kv-store)
7. [Configuration](#7-configuration)
8. [Engine: the run lifecycle](#8-engine-the-run-lifecycle)
9. [User confirmation](#9-user-confirmation)
10. [Robustness & failure handling](#10-robustness--failure-handling)
11. [Safety guardrails](#11-safety-guardrails)
12. [Adapters](#12-adapters)
13. [CLI surface](#13-cli-surface)
14. [Scheduling & adaptive cadence](#14-scheduling--adaptive-cadence)
15. [Observability: logs, decision records, notifications](#15-observability-logs-decision-records-notifications)
16. [Testing strategy](#16-testing-strategy)
17. [Dependencies](#17-dependencies)
18. [Design decisions & tradeoffs](#18-design-decisions--tradeoffs)
19. [Delivery history](#19-delivery-history)
20. [Decisions taken since the draft](#20-decisions-taken-since-the-draft)
21. [Future adapter ideas](#21-future-adapter-ideas)

---

## 1. Goals & non-goals

### Goals

- **Reclaim disk space automatically** on macOS and Linux, on an adjustable cadence
  (default: a full run twice a day, automatically more frequent under disk pressure).
- **Only delete what is no longer in use.** Every adapter tracks (or derives) a
  *last-used* time per item and applies age thresholds to it.
- **Do nothing when space is plentiful.** Deletion only happens below a configurable
  free-space threshold; an aggressive *scavenge* mode kicks in below a second, lower one.
- **Lowest-impact first.** Scavenging burns regenerable build artifacts before caches,
  and caches before anything a human put somewhere (trash, downloads).
- **Keep the human in the loop where it matters.** High-impact deletions (trash,
  downloads) can be configured to *request confirmation*: they queue up, the user gets a
  desktop notification, and nothing is deleted until `disk-saver review` approves it.
- **Notify, don't surprise.** Desktop notifications for "space is getting low, scavenge
  will begin soon", scavenge summaries, and pending approvals — all configurable.
- **Explainable.** A structured decision record (why was X deleted? why was Y kept?) —
  compiled in behind a cargo feature so lean builds can drop it entirely.
- **Pluggable adapters**, each in its own crate, each fully self-contained: its config
  schema, its usage-tracking, and its deletion logic all live in one place.
- **OS-agnostic adapters.** Adapters talk to the world exclusively through a
  `trait Platform` (filesystem, subprocesses, clock, notifications, well-known
  directories, disk stats), so every adapter is integration-testable against an
  in-memory fake.
- **Robust to partial failure.** A broken/unavailable adapter (e.g. OrbStack not running)
  is skipped for that run and retried next time; it never takes down the run.
- **Durable state** via a small sqlite-backed key-value store (e.g. "when did we last see
  docker image X in use?").

### Non-goals (v1)

- Not a daemon. One-shot binary; launchd/systemd own the schedule (the binary
  self-throttles, §14).
- No GUI / menu-bar app. CLI + logs + `status` + desktop notifications.
- No dynamic plugin loading. Adapters are compiled in (cargo features to trim).
- No Windows support (Platform trait keeps the door open).
- No cross-machine sync, no telemetry, nothing leaves the machine.

---

## 2. System overview

```
              launchd (macOS) / systemd user timer (Linux)
                          │  fires every `check_every` (default 1h)
                          ▼
 ┌────────────────────────── disk-saver run ──────────────────────────────┐
 │                                                                        │
 │  1. acquire single-instance lock                                       │
 │  2. THROTTLE GATE: cheap disk probe + "time since last full run".      │
 │     Not due yet? exit 0. (Due every 12h normally; every 1h under       │
 │     pressure — this is how cadence adapts, §14.)                       │
 │  3. load ~/.disk-saver.toml   ──► [global] + opaque per-adapter tables │
 │  4. open sqlite KV store                                               │
 │  5. measure free space        ──► Pressure: Comfortable│Normal│Scavenge│
 │     (+ warn flag → "scavenge soon" notification)                       │
 │                                                                        │
 │  6. for each enabled adapter (isolated; errors don't propagate):       │
 │        observe(ctx)      ← ALWAYS runs, keeps last-used state fresh    │
 │        plan(ctx)         ← only if pressure > Comfortable              │
 │                                                                        │
 │  7. candidates needing confirmation → approvals queue + notification;  │
 │     the rest: engine orders by impact class (Rebuildable → Cache →     │
 │     UserData), oldest first, and calls adapter.execute(batch) until    │
 │     done / free-space target met                                       │
 │                                                                        │
 │  8. write run report + decision records, notify if noteworthy          │
 └────────────────────────────────────────────────────────────────────────┘

         adapters ──────────► trait Platform ──────────► real OS
        (no OS knowledge)     (fs, exec, clock, notify,  (or FakePlatform
                               trash dirs, disk stats)    in tests)

         user ◄── notifications ("low space", "12 items await approval")
         user ──► disk-saver review  ──► approve/deny queued deletions
```

Three invariants worth calling out early:

- **`observe` runs every full cycle, even when nothing will be deleted.** Age tracking
  must not have gaps: if the docker adapter only looked at containers when space was low,
  an image in daily use would look "unused for 30 days" the first time pressure hit.
  Observation and deletion are deliberately decoupled phases.
- **The engine never interprets adapter configuration or adapter state.** It reads one
  reserved key (`enabled`) from each `[adapters.<name>]` table and passes the rest through
  as an opaque `toml::Value`.
- **A candidate flagged `requires_confirmation` is never auto-deleted** — not in normal
  mode, not at the bottom of scavenge. Confirmation means confirmation.

---

## 3. Workspace layout

```
crates/
  cli/                     package: disk-saver            (the binary)
  core/                    package: disk-saver-core       Adapter trait, engine, pressure model
  platform/                package: disk-saver-platform   trait Platform + RealPlatform + FakePlatform
  kv/                      package: disk-saver-kv         sqlite key-value store
  scan/                    package: disk-saver-scan       shared project/artifact-dir walker
  cachedir/                package: disk-saver-cachedir   shared global-cache-dir engine
  installer/               package: disk-saver-installer   standalone interactive TUI installer
  adapter-docker/          package: disk-saver-adapter-docker
  adapter-node-modules/    package: disk-saver-adapter-node-modules
  adapter-rust-target/     package: disk-saver-adapter-rust-target
  adapter-python-cache/    package: disk-saver-adapter-python-cache
  adapter-trash/           package: disk-saver-adapter-trash
  adapter-pnpm/            package: disk-saver-adapter-pnpm            (on cachedir)
  adapter-cargo-registry/  package: disk-saver-adapter-cargo-registry  (on cachedir)
  adapter-pip/             package: disk-saver-adapter-pip             (on cachedir)
  adapter-git-gc/          package: disk-saver-adapter-git-gc          (on scan)
  adapter-git-ignored/     package: disk-saver-adapter-git-ignored     (on scan)
```

Two of those are shaped unlike the rest. `cachedir` is a second shared engine alongside
`scan`: where `scan` walks `roots` hunting for artifact directories, `cachedir` prunes a
small fixed set of directories a developer tool owns, so the three adapters built on it
supply only a name and a resolver (§12.6). And `installer` is a binary, not a library — it
is excluded from `default-members` and is the *only* crate pulling `ratatui`, so the shipped
`disk-saver` binary stays TUI-free and a plain `cargo build` never compiles it.

Dependency graph (arrows = "depends on"):

```
cli ─► every adapter ─► core ─► kv
  │        │              └───► platform
  │        ├─(fs adapters)───► scan ─────► platform
  │        └─(cache adapters)► cachedir ─► platform
  └─► core, kv, platform
```

- `kv` and `platform` are leaf crates with no internal dependencies.
- `core` defines `trait Adapter`, `Candidate`, `Pressure`, the `RetentionPolicy` helper,
  the error taxonomy, the approvals queue, the decision log, and the engine
  (orchestration lives here, not in the CLI).
- `scan` holds the walker shared by the filesystem adapters (node_modules / rust target /
  python caches are the same problem with different markers), plus the `FsConfig` those
  adapters share for root resolution, the built-in denylist and the retention thresholds.
  The two git adapters reuse it with a `.git` rule.
- `cachedir` holds the whole lifecycle for a global cache directory — config, age, size,
  eligibility, deletion, decision records — so a cache adapter is a name and a resolver.
- `cli` is thin: argument parsing, logging setup, config loading, the static adapter
  registry, the interactive `review` flow, and scheduling install/uninstall.

The CLI exposes one cargo feature per adapter (all on by default), plus `decision-log`
(§15.2), so a devbox build can drop, say, the trash adapter or the audit machinery:

```toml
# crates/cli/Cargo.toml
[features]
default = [
    "docker", "node-modules", "rust-target", "python-cache", "trash",
    "pnpm", "cargo-registry", "pip", "git-gc", "git-ignored",
    "decision-log",
]
docker       = ["dep:disk-saver-adapter-docker"]
decision-log = ["disk-saver-core/decision-log"]
# ...one feature per adapter; each gates a `dep:` on its crate.
```

---

## 4. Core concepts

### 4.1 The `Adapter` trait

Illustrative sketch — exact signatures will evolve, the *shape* is the contract:

```rust
/// One per adapter crate. Constructed by a factory from its opaque config.
pub trait Adapter: Send {
    /// Stable machine name. Doubles as: config section `[adapters.<name>]`,
    /// KV bucket name, and log target.
    fn name(&self) -> &'static str;

    /// Cheap dependency probe (is the docker daemon reachable? is ~/.Trash readable?).
    /// Used by `disk-saver doctor`. Default impl returns Ok.
    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> { Ok(()) }

    /// Record whatever is needed for age tracking (e.g. "image X in use now").
    /// Runs EVERY full cycle, including when pressure is Comfortable.
    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError>;

    /// Return deletion candidates, already filtered by the adapter's own policy
    /// for the current pressure level. Must not mutate anything.
    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError>;

    /// Delete a batch of previously-planned candidates. Partial failure is normal:
    /// outcomes are reported per candidate, not all-or-nothing. Called both by
    /// scheduled runs (unflagged candidates) and by `disk-saver review` (approved ones).
    fn execute(&mut self, ctx: &mut Ctx, batch: &[Candidate]) -> Result<Vec<Outcome>, AdapterError>;
}

/// Everything an adapter may touch. Note: no direct fs/process/clock access.
pub struct Ctx<'run> {
    pub platform: &'run dyn Platform,
    pub kv: Bucket<'run>,        // KV handle pre-namespaced to this adapter
    pub pressure: Pressure,
}
```

Adapter instantiation is a **factory registry** in the CLI. The adapter names the type
its config deserializes into and core does the parsing; core never sees the *schema*, and
the adapter never sees the raw table:

```rust
pub struct AdapterFactory {
    pub name: &'static str,
    make: BuildFn,   // private: built only through `typed`
}

impl AdapterFactory {
    /// `make` gets `C` — deserialized from the `[adapters.<name>]` table, or
    /// `C::default()` if the section is absent → adapter defaults — plus a
    /// `ConfigCx` carrying the adapter's name for error attribution and the
    /// checks every adapter would otherwise repeat (`retention`, `globs`).
    pub fn typed<C, A, F>(name: &'static str, make: F) -> Self
    where
        C: DeserializeOwned + Default,
        A: Adapter + 'static,
        F: Fn(C, &ConfigCx) -> Result<A, ConfigError> + Send + Sync + 'static;

    pub fn build(&self, raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError>;
}

// crates/cli/src/registry.rs
pub fn registry() -> Vec<AdapterFactory> {
    vec![
        #[cfg(feature = "docker")]        disk_saver_adapter_docker::factory(),
        #[cfg(feature = "node-modules")]  disk_saver_adapter_node_modules::factory(),
        // ...
    ]
}
```

Why four phases instead of one `clean()`?

- `observe`/`plan` split ⇒ observation never has gaps (see §2 invariant).
- `plan`/`execute` split ⇒ `--dry-run` is free, the confirmation queue is free (flagged
  candidates are simply routed to the queue instead of `execute`), and the engine can
  make *global* decisions in scavenge mode ("delete lowest-impact-and-oldest across all
  adapters until the free-space target is met, then stop").

Methods take `&mut self` so an adapter can carry in-memory notes from `observe` to `plan`
within a run (durable state goes to KV). Adapters run sequentially, so this costs nothing.

### 4.2 Candidates

```rust
pub struct Candidate {
    /// Adapter-scoped stable id (path, docker image id, …). Round-trips to execute()
    /// and identifies the item in the approvals queue.
    pub id: String,
    /// Human-readable one-liner for --dry-run output, `review`, and logs.
    pub label: String,
    /// Estimated reclaimable bytes. An estimate only — shared docker layers,
    /// hardlinked pnpm stores etc. make exactness impossible; the engine
    /// re-measures real free space rather than trusting these numbers.
    pub bytes: u64,
    /// When this item was last known to be used. Drives ordering within a class.
    pub last_used: SystemTime,
    /// Impact of deleting this. Drives scavenge wave ordering (§8).
    pub class: Class,
    /// If true, the engine NEVER auto-deletes this candidate: it goes to the
    /// approvals queue and waits for `disk-saver review` (§9). Set by the
    /// adapter, typically from its `confirm` config key.
    pub requires_confirmation: bool,
}

/// Impact classes, lowest impact first. Scavenge exhausts one class before
/// touching the next (§8).
pub enum Class {
    /// Regenerated automatically by a tool (target/, __pycache__, build cache).
    Rebuildable,
    /// Re-fetchable but costs time/bandwidth (node_modules, docker images).
    Cache,
    /// Gone is gone (trash contents, downloads).
    UserData,
}

pub enum Outcome {
    Removed { id: String, bytes: u64 },
    Skipped { id: String, reason: String },   // e.g. "image now in use by container"
    Failed  { id: String, error: String },
}
```

Granularity is the adapter's choice: one candidate per node_modules directory, but the
docker build cache may be a single coarse candidate ("build cache entries older than 7d,
~4.2 GB") when the underlying tool only supports bulk pruning.

### 4.3 The disk-pressure model

Free space on one configured filesystem (default: the one containing `$HOME`) is measured
at the start of the run and mapped to a tier:

```rust
pub enum Pressure {
    /// Free space above `start_cleaning_below`: observe only. DELETE NOTHING.
    Comfortable,
    /// Between the thresholds: delete items older than their adapter's `max_age`.
    Normal,
    /// Free space below `scavenge_below`: delete items older than `min_age`,
    /// lowest-impact & oldest first, until free space reaches `scavenge_target`.
    Scavenge { need: u64 },
}
```

```
 free space ──────────────────────────────────────────────────────────────►
    0%      scavenge_below   warn_below      start_cleaning_below     100%
     ├── SCAVENGE ──┼─────────┼── NORMAL ──────────┼── COMFORTABLE ────┤
     │ delete ≥ min_age,      │                    │ observe only,
     │ until scavenge_target  │                    │ delete nothing
     │                        └─ "scavenge soon"   │
     │                           notification +    │
     │                           faster cadence    │
     └── delete ≥ max_age ─────────────────────────┘
```

`warn_below` is **engine-level only** — adapters still see the three-tier `Pressure`.
Crossing it triggers the "space is getting low, scavenge will begin soon" notification
(§15.3) and switches the scheduler gate to the faster `pressure_run_every` cadence (§14).

Thresholds accept absolute sizes (`"40GB"`) or percentages of the disk (`"10%"`), and must
satisfy `scavenge_below < warn_below ≤ start_cleaning_below` and
`scavenge_below < scavenge_target ≤ start_cleaning_below` (validated at startup).

### 4.4 The retention-policy convention

Age thresholds are *adapter* config (opaque to core), but every adapter wants the same
knobs, so `core` ships helpers adapters opt into:

```rust
#[derive(Deserialize)]
pub struct RetentionPolicy {
    /// Normal mode: delete items unused for at least this long.
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,      // e.g. "7d"
    /// Scavenge floor: NEVER delete items younger than this, no matter the pressure.
    #[serde(with = "humantime_serde")]
    pub min_age: Duration,      // e.g. "1d"; must be ≤ max_age
}

impl RetentionPolicy {
    pub fn eligible(&self, age: Duration, pressure: Pressure) -> bool {
        match pressure {
            Pressure::Comfortable   => false,
            Pressure::Normal        => age >= self.max_age,
            Pressure::Scavenge {..} => age >= self.min_age,
        }
    }
}
```

So "delete docker images unused >7 days, but under disk pressure allow >2 days" is just
`max_age = "7d"`, `min_age = "2d"`. The engine never sees these values — adapters apply
them inside `plan()` and only return already-eligible candidates.

By the same convention, adapters whose deletions are high-impact expose a `confirm`
boolean in their config (trash defaults it to `true`, build-artifact adapters to `false`)
and map it onto `Candidate::requires_confirmation`. The engine only ever sees the flag on
the candidate, never the config key.

---

## 5. Platform abstraction

Adapters have **zero** OS knowledge — no `#[cfg(target_os)]`, no `std::fs`, no
`std::process`, no `SystemTime::now()`. Everything flows through:

```rust
pub trait Platform: Send + Sync {
    // ── clock (mockable time is what makes age-based tests trivial) ──
    fn now(&self) -> SystemTime;

    // ── well-known locations: OS-specific path knowledge lives HERE ──
    fn home_dir(&self) -> PathBuf;
    /// macOS: [~/.Trash]   Linux: [~/.local/share/Trash]  (XDG)
    fn trash_dirs(&self) -> Vec<PathBuf>;
    /// macOS: ~/Library/Caches   Linux: ~/.cache
    fn user_cache_dir(&self) -> PathBuf;

    // ── filesystem (never follows symlinks) ──
    fn metadata(&self, path: &Path) -> io::Result<FileMeta>;       // kind, len, mtime
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// Recursive apparent size; stays on one device, doesn't follow symlinks.
    fn dir_size(&self, path: &Path) -> io::Result<u64>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;

    // ── disk stats (statvfs) ──
    fn disk_usage(&self, path: &Path) -> io::Result<DiskUsage>;    // { total, available }

    // ── desktop notifications (macOS: osascript, Linux: notify-send) ──
    /// Best-effort: failures are logged and swallowed, never fail a run.
    /// By convention only the ENGINE notifies; adapters shouldn't.
    fn notify(&self, n: &Notification) -> io::Result<()>;

    // ── subprocesses (how the docker adapter talks to the daemon) ──
    /// Runs to completion with a hard timeout (default from global config).
    /// A hung `docker ps` must not hang the whole run.
    fn run_command(&self, cmd: &CommandSpec) -> io::Result<CommandOutput>;
}

pub struct Notification { pub title: String, pub body: String, pub urgency: Urgency }
pub enum Urgency { Low, Normal, Critical }

pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Option<Duration>,    // None → global default (60s)
}
pub struct CommandOutput { pub status: i32, pub stdout: Vec<u8>, pub stderr: Vec<u8> }
```

Two families of implementation ship in the `platform` crate:

- **`MacOsPlatform` / `LinuxPlatform`** — the std/rustix-backed production platforms, one
  per file (`macos.rs`, `linux.rs`). All OS-agnostic logic (filesystem walking, sizing,
  safe deletion, statvfs, subprocess handling) lives once in the private `sys` module and
  is shared; each OS file implements only its genuinely divergent surface — well-known
  dirs and notification delivery (`osascript -e 'display notification …'` on macOS,
  `notify-send` on Linux, both downgraded to a log line if unavailable) — so *no method
  body branches on `target_os`*. The `RealPlatform` type alias resolves to the right one
  for the target OS (`MacOsPlatform` on macOS, `LinuxPlatform` elsewhere), so downstream
  crates construct `RealPlatform::new()` and never see a `cfg`. Command timeouts are
  enforced by spawn + wait-with-deadline + kill.
- **`FakePlatform`** (behind a `test-util` feature) — an in-memory filesystem
  (`BTreeMap<PathBuf, FakeNode>` with contents/sizes/mtimes), a scripted command
  responder (match on program+args → canned output, or an error to simulate a dead
  daemon), a manually-advanceable clock, and a settable free-space value. It records
  every destructive call **and every notification** so tests can assert exactly what
  was deleted and what the user was told.

```rust
// The shape of every adapter integration test:
let fake = FakePlatform::new()
    .with_file("~/dev/app/package.json", ..., mtime_days_ago(40))
    .with_dir("~/dev/app/node_modules", size_gb(1.2), mtime_days_ago(40))
    .with_free_space(gb(30), total: gb(500));
// run observe/plan → advance clock 10 days → run again → assert deletion
```

Deletion going through `Platform` (not `std::fs`) is also a safety chokepoint: the real
implementation refuses relative paths, refuses to traverse symlinks, and logs every
removal before performing it.

---

## 6. State: the KV store

`disk-saver-kv` is a deliberately tiny sqlite wrapper — a persistent
`Map<(bucket, key), json>`:

```rust
pub struct Store { /* rusqlite::Connection */ }

impl Store {
    pub fn open(path: &Path) -> Result<Store>;
    pub fn open_in_memory() -> Result<Store>;                  // tests
    pub fn bucket(&self, name: &str) -> Bucket<'_>;            // namespace handle
}

impl Bucket<'_> {
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>>;
    pub fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()>;
    pub fn delete(&self, key: &str) -> Result<()>;
    pub fn keys(&self, prefix: &str) -> Result<Vec<String>>;
    pub fn iter<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>>;
}
```

Schema and settings:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA user_version = 1;          -- bump for migrations

CREATE TABLE kv (
    bucket     TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,     -- JSON: debuggable with the sqlite3 CLI
    updated_at INTEGER NOT NULL,  -- unix seconds, from Platform::now()
    PRIMARY KEY (bucket, key)
) WITHOUT ROWID;
```

- Values are JSON via serde — human-inspectable (`sqlite3 state.db 'select * from kv'`).
- Each adapter gets a bucket named after itself via `Ctx.kv`; adapters cannot see or
  clobber each other's state.
- The engine owns two reserved buckets: **`_engine`** (run reports, `last_full_run`
  timestamp for the throttle gate, last-notified timestamps for notification dedupe)
  and **`_approvals`** (the confirmation queue, §9).
- Adapters are responsible for garbage-collecting their own entries during `observe`
  (e.g. drop KV records for docker images that no longer exist).
- sqlite is compiled in (`rusqlite` `bundled`) — zero system dependencies.
- Location: `~/Library/Application Support/disk-saver/state.db` (macOS),
  `~/.local/state/disk-saver/state.db` (Linux); overridable via `state_dir` config.
  Note: this path choice sits *outside* the Platform abstraction — it's engine/CLI
  plumbing, and the Platform trait exists for adapters.

Timestamps stored in KV are wall-clock unix seconds derived from `Platform::now()`, so
`FakePlatform`'s clock controls everything in tests.

---

## 7. Configuration

Single file: **`~/.disk-saver.toml`** (override with `--config` / `$DISK_SAVER_CONFIG`).
Missing file ⇒ built-in defaults for everything.

```toml
[global]
# Filesystem whose free space drives the pressure model.
# Default: the filesystem containing your home directory.
disk = "/"

# Thresholds: absolute ("40GB") or percentage of total disk ("10%").
start_cleaning_below = "20%"   # more free than this → observe only, delete nothing
warn_below           = "12%"   # less free than this → notify + faster cadence (§14)
scavenge_below       = "8%"    # less free than this → scavenge mode
scavenge_target      = "15%"   # scavenge until this much is free, then stop

command_timeout = "60s"        # default hard timeout for adapter subprocesses
# state_dir = "..."            # optional override for sqlite/logs/lock location

[schedule]                     # see §14 — cadence is config, not baked into timers
check_every        = "1h"      # how often the OS timer fires (cheap probe, usually exits)
run_every          = "12h"     # min interval between full runs normally → twice a day
pressure_run_every = "1h"      # min interval once free space < warn_below

[notifications]                # see §15.3
enabled = true
min_gap = "12h"                # don't repeat the same event more often than this
events  = ["scavenge_warning", "scavenge_ran", "approvals_pending", "adapter_failing"]

# ── everything below this line is OPAQUE to the engine ─────────────────
# The engine reads exactly one reserved key per section: `enabled`
# (default true). The rest of each table is handed verbatim to the adapter.

[adapters.docker]
max_age = "7d"                 # normal mode: unused ≥ 7 days
min_age = "2d"                 # scavenge floor: never touch anything used < 2 days ago
protect = ["postgres:*"]       # glob patterns for images/containers to never remove

[adapters.node-modules]
roots = ["~/dev", "~/work"]
max_age = "30d"
min_age = "7d"

[adapters.rust-target]
roots = ["~/dev"]
max_age = "30d"
min_age = "7d"
incremental_min_age = "15m"    # scavenge-only: target/*/incremental has its own, shorter floor

[adapters.python-cache]
roots = ["~/dev"]
max_age = "30d"
min_age = "7d"
include_venvs = false          # .venv deletion is opt-in

[adapters.trash]
max_age = "60d"
min_age = "7d"
confirm = true                 # queue for `disk-saver review` instead of auto-deleting
                               # (trash's default even if omitted — it's UserData)
```

Loading pipeline (in `core`):

```rust
pub struct Config {
    pub global: GlobalConfig,                     // fully typed, owned by core
    pub schedule: ScheduleConfig,
    pub notifications: NotificationConfig,
    pub adapters: BTreeMap<String, AdapterSection>,
}
pub struct AdapterSection {
    pub enabled: bool,                            // the ONE reserved key
    pub rest: Option<toml::Value>,                // opaque; handed to the factory
}
```

Rules:

- A `[adapters.<name>]` section with no matching registered adapter ⇒ **warning**
  (catches typos like `[adapters.dokcer]`); the run continues.
- A registered adapter with no section ⇒ built into the run with its own defaults.
- An adapter section that fails the adapter's own deserialization ⇒ **hard startup
  error** for the whole run. Rationale: config is user-authored; silently skipping a
  misconfigured adapter for weeks is worse than failing loudly (and `disk-saver config
  check` + the pre-flight check in `schedule install` make this hard to hit).
- Durations use humantime syntax (`"7d"`, `"36h"`); sizes use `bytesize` syntax
  (`"40GB"`); `~` is expanded via `Platform::home_dir()`.

---

## 8. Engine: the run lifecycle

`disk-saver run` (also the scheduled entry point):

```
 1. Acquire single-instance file lock (state_dir/lock).
    Held? → log "another run in progress", exit 0.
 2. THROTTLE GATE (skipped with --force): cheap statvfs + read _engine:last_full_run.
       due_interval = free < warn_below ? pressure_run_every : run_every
       elapsed < due_interval → exit 0 silently.        ← most timer fires end here
 3. Load + validate config; build enabled adapters via the factory registry.
 4. Open KV store.
 5. pressure = classify(platform.disk_usage(global.disk))
       crossed below warn_below since last run? → "scavenge soon" notification (§15.3)
 6. OBSERVE   for each adapter, in order, isolated (see §10):
                 adapter.observe(ctx)
 7. If pressure == Comfortable → GC approvals queue (§9), write report, exit 0.
 8. PLAN      for each adapter: candidates += adapter.plan(ctx)
              (--dry-run stops here and pretty-prints the table)
 9. ROUTE     candidates with requires_confirmation → approvals queue (§9);
              queue changed? → "approvals pending" notification.
              Everything else proceeds to EXECUTE.
10. EXECUTE
      Normal:   every adapter executes all of its own unflagged candidates
                (they are all past max_age — no reason to keep any).
      Scavenge: process in IMPACT WAVES — all Rebuildable candidates first,
                then Cache, then UserData; oldest-first within each wave;
                chunked into per-adapter batches (~10 candidates);
                after each batch: re-measure REAL free space;
                    free ≥ scavenge_target → stop early, later waves never run.
                (Real measurement, not candidate byte-estimates — estimates lie:
                 shared docker layers, hardlinks, pnpm stores.)
11. REPORT    aggregate outcomes → decision records (§15.2) → log summary →
              persist to KV `_engine` bucket (ring buffer of the last ~50 run
              reports, for `disk-saver status`).
              Scavenge ran, or target unreachable? → notification.
12. Release lock.
```

Notes:

- The wave ordering is the "lower-impact places first" rule: a 30-day-old `node_modules`
  (Cache) is sacrificed before a 90-day-old trash item (UserData) is even considered —
  and in practice the UserData wave is usually confirmation-gated anyway (§9), so
  unattended scavenging typically ends at caches. If the target is met mid-wave, higher
  -impact waves never run.
- Everything is idempotent. A run killed halfway (power loss, sleep) needs no recovery:
  the next run re-observes, re-plans, and re-deletes whatever still qualifies. The KV
  store is transactional per write; there is no multi-step state to corrupt.
- Exit codes: `0` clean (including "throttled" and "skipped adapters were unavailable"),
  `1` config or environment error (nothing ran), `2` one or more adapters *failed*
  (rest of run completed) — lets the scheduler/monitoring distinguish severities.

---

## 9. User confirmation

Some deletions should never happen behind the user's back — emptying old trash, or (a
future adapter) pruning `~/Downloads`. But a scheduled job has no terminal to prompt on.
The design: **an approvals queue + a notification + an interactive `review` command.**

```
 scheduled run                              user, later
 ─────────────                              ───────────
 plan() → candidate{requires_confirmation}  $ disk-saver review
        │                                     trash  Screenshot 2024-…png   1.2 GB  aged 74d
        ▼                                     trash  old-backup.tar         8.0 GB  aged 90d
 _approvals bucket (KV)                       [y]es / [n]ot now / [N]ever / [a]ll / [q]uit
        │                                            │
        ▼                                            ▼
 notification: "disk-saver: 12 items        approved → adapter.execute(batch) NOW
 (9.4 GB) await approval — run              denied   → snoozed or permanently skipped
 `disk-saver review`"
```

Mechanics:

- **Flagging** is per-candidate (`requires_confirmation`), set by the adapter from its
  own `confirm` config key (§4.4). Trash defaults to `confirm = true`; build-artifact
  adapters to `false`. Any adapter can be flipped either way in config — a cautious user
  can set `confirm = true` on *everything* and turn disk-saver into a pure
  propose-and-review tool.
- **The queue mirrors the latest plan.** Each full run rebuilds `_approvals` from the
  current plan output: new flagged candidates are added (keyed `adapter/candidate-id`,
  preserving `first_queued`), candidates no longer planned (item vanished, no longer
  eligible, or pressure returned to Comfortable) are GC'd. The queue therefore never
  goes stale and never asks about something that no longer qualifies.
- **Hard rule:** flagged candidates are *never* auto-deleted, even at the bottom of
  scavenge. If scavenge exhausts unflagged candidates without reaching
  `scavenge_target`, the run notifies: "still low on space — 9.4 GB awaits your
  approval".
- **`disk-saver review`** takes the same single-instance lock, loads the queue, and
  prompts per item (grouped by adapter, biggest first): **yes** → executed immediately
  via the adapter's `execute()`, outcome shown; **not now** → snoozed (`snooze_for`,
  default `"14d"` — won't be re-asked, still GC'd normally); **Never** → recorded as a
  permanent deny in `_approvals` (engine-level; `review` suggests adding a `protect`
  glob to the adapter's config for a durable, visible version); **all** → approve the
  rest for this adapter. Non-interactive escape hatches for scripting:
  `review --list --json`, `review --approve <id>…`, `review --approve-all [--adapter <n>]`.
- Approval is explicit user intent, so approved items are executed even if pressure has
  since eased. Adapters may still re-validate in `execute()` and return
  `Skipped { reason }` (e.g. the file's mtime changed since planning).

---

## 10. Robustness & failure handling

Error taxonomy (`core`):

```rust
pub enum AdapterError {
    /// An external dependency is missing or unreachable: docker daemon down,
    /// ~/.Trash unreadable without Full Disk Access, binary not on PATH.
    /// This is an EXPECTED condition, not a failure: log at info, skip the
    /// adapter this run, retry next run. `status` surfaces it.
    Unavailable(String),
    /// Genuine failure. Log at error, skip the adapter, keep running the rest,
    /// reflect in the exit code and the run report.
    Failed(anyhow::Error),
}
```

Isolation mechanics, per adapter, per phase:

| Failure mode | Handling |
|---|---|
| `Unavailable` (OrbStack not running) | Skip adapter for this run; info log; retried next run. Ages keep growing in KV, which is *semantically correct* — nothing could have used those images while the daemon was down. |
| `Failed` error | Skip adapter; error log; run report notes it; exit code 2. Other adapters unaffected. After 3 consecutive failing runs → `adapter_failing` notification (§15.3). |
| Panic | Every adapter call is wrapped in `catch_unwind` (`AssertUnwindSafe`); converted to `Failed`. A logic bug in one adapter cannot abort the janitor. |
| Hung subprocess | `Platform::run_command` enforces a hard timeout (default 60s) with kill; surfaces as `Failed`. |
| Partial `execute` failure | `execute` returns per-candidate `Outcome`s; one un-deletable image doesn't stop its siblings. |
| Concurrent runs (hourly timer overlap, manual + scheduled, `review` during a run) | Single-instance advisory file lock; the loser exits 0 immediately. |
| Notification delivery failure | Logged at debug and swallowed — cosmetics never fail a run. |
| Bad adapter config | Hard startup error (see §7 rationale). |
| First run / cold state | Grace period: KV-tracked entities get `first_seen = now` when initially observed and are never deleted in the same run they were discovered (see §11). |

---

## 11. Safety guardrails

For a tool whose job is deleting files unattended, paranoia is a feature:

1. **Comfortable = delete nothing.** The most common run is a no-op by design.
2. **`min_age` floor.** Even at critical disk pressure, nothing used more recently than
   the adapter's `min_age` is touched.
3. **Confirmation gating.** High-impact adapters (UserData) default to
   `requires_confirmation`; flagged items are never auto-deleted, period (§9).
4. **Impact waves.** Scavenge only reaches user-visible data after regenerable
   artifacts and caches have already been exhausted (§8).
5. **First-sighting grace.** Anything an adapter sees for the first time is stamped
   `first_seen = now` and treated as *used now*. Nothing is ever deleted in the run that
   discovered it — an item must age through the policy window under observation.
6. **Strict identification.** Filesystem adapters only delete directories that match a
   marker pattern *and* a validity check, e.g. `target/` only with a sibling `Cargo.toml`
   **and** a `CACHEDIR.TAG` inside (cargo writes one); `node_modules/` only with a
   sibling `package.json`; `__pycache__/` only containing `*.pyc`.
7. **Opt-out sentinel.** A `.disk-saver-keep` file in a project root exempts the whole
   project from all filesystem adapters.
8. **No symlink traversal, ever** — walking, sizing, or deleting. Walks also never cross
   filesystem boundaries (device check), so a mounted volume can't be swept by accident.
9. **No force flags.** The docker adapter never passes `-f`: an image that grew a
   container between plan and execute makes `docker rmi` fail → reported as `Skipped`.
10. **Protect lists.** Glob-based `protect = [...]` config honored by adapters (images,
    paths).
11. **Dry-run is first-class.** `disk-saver run --dry-run` / `disk-saver plan` prints the
    full candidate table (adapter, label, size, age, class, confirm?) and touches nothing.
12. **Audit trail.** With the `decision-log` feature (default on, §15.2), every deletion
    — and every *decision not to delete* — is recorded before it happens: adapter, id,
    size, age, pressure, and reason. `disk-saver why <item>` replays the history.

---

## 12. Adapters

Ten of them ship, in four families: docker (§12.1) stands alone on KV-tracked usage; five
walk `roots` on `disk-saver-scan` looking for a marker (§12.2–12.4, §12.7–12.8); three prune
a global cache directory on `disk-saver-cachedir` (§12.6); and trash (§12.5) is its own
cross-platform case. Numbering follows the order they were built, not the order the engine
runs them — that is by impact class (§8).

Common shape: each crate exports `factory() -> AdapterFactory` built with
`AdapterFactory::typed`, defines its own serde config struct (embedding `RetentionPolicy`),
and keeps its state in its own KV bucket. The constructor `typed` calls receives that
config already deserialized, so no adapter depends on `toml` outside its tests. The three
global-cache adapters supply less still — just a name and a resolver, to
`disk_saver_cachedir::factory`.

Where a default of the adapter's own has to be reconciled with a threshold the user set,
the config field is a `Configured<T>` (`Set` / `Default` / `Unset`) rather than a bare
value: which of the three it is decides who gives way. Our default yields to a shorter
`max_age`; a value the user wrote does not, and a contradictory pair is reported. See
`rust-target`'s `incremental_min_age` and `git-gc`'s `min_age`.

### 12.1 `docker` — containers, images, build cache

*The reference adapter for KV-based usage tracking. Works with Docker Desktop, OrbStack,
colima — anything the `docker` CLI talks to. OS-agnostic by construction (subprocess only).*

| | |
|---|---|
| Availability | `docker version` via `run_command`; non-zero/missing ⇒ `Unavailable` |
| Observes | Containers (`docker ps -a --format json`): running ⇒ image *used now*; exited ⇒ image used at `FinishedAt` (via `docker inspect`), which captures usage that happened **between** our runs. Images (`docker image ls --format json`): reconcile KV — new image ⇒ `first_seen = now`; vanished ⇒ drop KV entry. |
| KV state | `image:<id> → { first_seen, last_used, tags }`, `container:<id> → { last_running }` |
| Plans | ① exited containers idle past policy → ② images referenced by no container (running *or* stopped) whose `last_used` is past policy (class: Cache) → ③ dangling images → ④ build cache as one coarse candidate, size from `docker system df` (class: Rebuildable) |
| Executes | `docker container rm <id>`, `docker image rm <id>` (never `-f`), `docker builder prune --force --filter until=<age>` |
| Config | `RetentionPolicy` + `protect = ["postgres:*", …]` + `remove_exited_containers = true`, `confirm = false` |
| Explicitly not v1 | **Volumes** (real data-loss risk — future opt-in), networks, podman |
| Defaults | `max_age = "7d"`, `min_age = "2d"` |

### 12.2 `node-modules`, 12.3 `rust-target`, 12.4 `python-cache` — project artifacts

Three thin crates over one shared engine in `disk-saver-scan`. The scan crate provides:

```rust
pub struct Rule {
    pub artifact_dir: &'static str,        // "node_modules" | "target" | "__pycache__" …
    pub required_sibling: &'static [&'static str],  // ["package.json"] | ["Cargo.toml"]
    pub validate: fn(&dyn Platform, &Path) -> bool, // e.g. CACHEDIR.TAG check
}
/// Walk `roots`, find matches, compute each project's `last_active`, don't
/// descend into matched artifact dirs (or `.git` internals), skip dot-dirs,
/// stay on one device, honor `.disk-saver-keep` and `exclude` globs.
pub fn find_artifacts(p: &dyn Platform, roots: &[PathBuf], rules: &[Rule], excl: &[Glob]) -> Vec<Found>;
```

**"Last used" heuristic** (the honest, documented compromise — atime is unreliable/noatime):
`project_last_active = max(mtime of entries in the project root excluding artifact dirs,
mtime of .git/HEAD if present)`. Editing any file, committing, or switching branches
refreshes it. The artifact dir's own mtime is deliberately ignored (builds touch it, but
a build *is* project activity, so its effect shows up via source files anyway; and we
don't want `disk-saver`'s own scanning to matter either — it never writes into projects).

| Adapter | Deletes | Class | Marker + validation | Notes |
|---|---|---|---|---|
| `node-modules` | `node_modules/` | Cache | sibling `package.json` | doesn't descend into nested node_modules; pnpm symlink layouts safe (no-follow) |
| `rust-target` | `target/`; under scavenge also `target/**/incremental` inside a `target/` too young to delete | Rebuildable | sibling `Cargo.toml` **and** `CACHEDIR.TAG` inside; an incremental cache is validated through the `target/` enclosing it | prior art: cargo-sweep (per-file granularity is a possible future refinement) |
| `python-cache` | `__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.ruff_cache/`, `.tox/`; `.venv/` only if `include_venvs = true` | Rebuildable | dir-specific (e.g. `__pycache__` must contain only `*.pyc*`) | global pip/uv caches: future |
| shared config | `roots` (required-ish; default `["~"]` with a built-in denylist: `~/Library`, `~/.Trash`, cache dirs), `exclude` globs, `RetentionPolicy`, `max_depth = 8`, `confirm = false` | | | |

These adapters need no KV state (age derives from the filesystem), but get a bucket
anyway — future use: caching walk results, remembering "size at last sighting" for
better reporting.

Defaults: `max_age = "30d"`, `min_age = "7d"`.

**The `rust-target` incremental sweep.** The whole-`target/` rule only ever fires on a
project nobody has touched lately, which is the wrong shape for the directories that
actually get large: an *active* cargo workspace rebuilds hourly, so its `last_active`
never ages past any floor worth having, and it grows the whole time. Cargo never reclaims
anything — every distinct build configuration (a different feature union from `-p one-crate`
versus `--workspace --all-features`, clippy's rustc wrapper versus cargo's) gets its own
`-C metadata` and its own artifacts, and the previous set stays forever. `incremental` is
the worst of it, because rustc garbage-collects sessions *within* one keyed directory but
never removes a directory whose key changed. Measured on one 14-crate workspace after eight
days of ordinary work: `target/` at 142 GB, 60 GB of it `debug/incremental` across 1,691
session directories.

So under `Pressure::Scavenge` only, the adapter also proposes the `incremental` directories
inside a `target/` it may not delete outright. That is strictly less loss than the parent —
every compiled rlib stays, so nothing has to be *re*built; the next edit to a crate just
recompiles it in full rather than by codegen unit. Being cheap, it gets its own much shorter
floor (`incremental_min_age`, 15m) instead of the `min_age` guarding the whole directory;
the default lowers itself to `max_age` rather than erroring when a config sets a shorter one.
The two never overlap — a `target/` that is itself eligible is not also swept, so the same
bytes are never proposed twice.

### 12.5 `trash` — the OS recycle bin

*The interesting cross-platform case; all divergence hides behind `Platform::trash_dirs()`.
Also the first confirmation-gated adapter.*

| | |
|---|---|
| Linux | XDG spec: `~/.local/share/Trash/files/*` with `info/<name>.trashinfo` carrying `DeletionDate` — exact ages for free. Delete `files/X` + `info/X.trashinfo` together. |
| macOS | `~/.Trash/*`. Deletion dates aren't exposed, so we use the KV pattern: first time an entry is seen ⇒ `first_seen = now`; age = `now − first_seen` (a strict *lower bound* on trash-age — safe direction, always under-estimates). First-sighting grace applies (§11.5). |
| Permissions | On macOS, reading `~/.Trash` from a background job requires Full Disk Access. Denied ⇒ `Unavailable` with a hint that `status`/`doctor` display ("grant Full Disk Access to disk-saver"). |
| Class | **UserData** — the last scavenge wave, after all rebuildables/caches. |
| Confirmation | `confirm = true` **by default**: trash items queue for `disk-saver review` rather than auto-deleting. Users who want the old fully-automatic behaviour set `confirm = false`. |
| Not v1 | External-volume trashes (`/Volumes/*/.Trashes`), network trash. |
| Defaults | `max_age = "60d"`, `min_age = "7d"`, `confirm = true` |

### 12.6 `pnpm`, `cargo-registry`, `pip` — global tool caches

*Three thin crates over one shared engine in `disk-saver-cachedir`.*

Where the filesystem adapters walk `roots` hunting for a marker, these target a small fixed
set of directories that a developer tool owns. Each such directory is one coarse candidate:
sized with `Platform::dir_size`, aged by its most recent activity, classed `Class::Cache`, and
removed wholesale once past policy. An adapter crate supplies a name and a resolver:

```rust
pub type Resolver = fn(&dyn Platform) -> Vec<PathBuf>;
pub fn factory(name: &'static str, resolver: Resolver) -> AdapterFactory;
```

Config, age, size, eligibility, deletion and decision records all live in the engine, so each
of these three crates is a few dozen lines. A resolver returns candidate paths for *every*
supported OS layout and lets `plan` filter to the ones that exist — so an adapter finds the
cache regardless of which OS created it, without branching on `target_os`. Discovery is
best-effort by design: a resolver whose tool is missing returns what it can.

| Adapter | Prunes | Discovery |
|---|---|---|
| `pnpm` | content-addressable store + metadata cache | `pnpm store path` when pnpm is installed (authoritative), else the known store/cache layouts. The `store` subdirectory only — the pnpm *binary* can live in the parent data dir. |
| `cargo-registry` | `registry/cache`, `registry/src`, `git/db`, `git/checkouts` under `~/.cargo` | fixed paths, each aged independently, so an active `registry/src` survives while a stale `git/checkouts` is reclaimed. The registry **index** is deliberately left alone: small, and slow to refetch. |
| `pip` | pip's HTTP/wheel cache | `~/Library/Caches/pip`, `~/.cache/pip`, plus `Platform::user_cache_dir()` so an `$XDG_CACHE_HOME` override is still honored. |

| | |
|---|---|
| Class | **Cache** — regenerable, but costs bandwidth and time |
| Age | `max(dir mtime, newest child mtime)`, one level deep — *not* a recursive walk |
| KV state | none; age derives from the filesystem |
| Config | `max_age`, `min_age`, `confirm`, plus `paths = [...]` for a non-standard location |
| Defaults | `max_age = "30d"`, `min_age = "6h"`, `confirm = false` |
| Not v1 | `$CARGO_HOME` / `$PIP_CACHE_DIR` are not read — point `paths` at a relocated cache |

Two deliberate choices. The **age probe stops at one level**: these directories hold thousands
of entries and their top level moves whenever the tool writes, so a recursive walk would cost
a great deal to learn the same thing. And **`min_age` is 6h** against the build-artifact
adapters' 1h (§12.2–12.4), because refilling one of these means re-downloading from a
registry — a cache touched earlier today is worth strictly more than a `target/` of the same
size, which rebuilds from sources already on disk.

### 12.7 `git-gc` — repacking idle repositories

*The only adapter that reclaims space without deleting its candidate: it shrinks `.git` in
place.*

| | |
|---|---|
| Availability | `git --version` via `run_command`; missing or non-zero ⇒ `Unavailable` |
| Discovers | repos under `roots` via `disk-saver-scan` with a `.git` rule (no required sibling), sharing the fs-adapter root/denylist resolution (`FsConfig::resolve_walk`) |
| Sizes | `git count-objects -v` → loose-object size + garbage (KiB). Zero ⇒ nothing worth packing, so no candidate is produced |
| Executes | `git gc` (plus `--aggressive` if configured), reporting the bytes `.git` actually shrank |
| Class | **Rebuildable** — only packfile layout and *unreachable* objects change; reachable history is never touched |
| Config | `FsConfig` (`roots`, `exclude`, `max_depth`, `max_age`, `min_age`, `confirm`) + `aggressive = false` |
| Defaults | `max_age = "30d"`, `min_age = "6h"`, `confirm = false` |

The **`min_age` floor is 6h**, not the shared `FsConfig::MIN_AGE` of 1h: `git gc` repacks a
*live* repo rather than deleting something rebuildable, so running it against a repo touched
an hour ago is work the next commit undoes, and it competes with the developer for IO. That
floor is *ours* rather than the user's — a `Configured::Default`, so it yields to a shorter
`max_age` somebody actually wrote, while still reporting a `min_age` they set above it (§4.4).

Age-gating also means the repos git would auto-gc anyway — the ones being worked in — are left
alone. The win here is one-time, on a repo untouched for a month.

### 12.8 `git-ignored` — ignored objects in repositories

*The second confirmation-gated adapter, and the one with the most room to do harm.*

A `.gitignore` covers exactly the things worth reclaiming — build outputs, dependency dirs,
logs, local databases — and also things that would hurt to lose: a local `.env`, a dev
database, uncommitted scratch work. So this adapter proposes and never disposes.

| | |
|---|---|
| Availability | `git --version`; missing or non-zero ⇒ `Unavailable` |
| Discovers | repos under `roots` — same `.git` rule and shared resolution as §12.7 |
| Plans | `git clean -Xdn` per repo — **ignored objects only**, never tracked and never plain-untracked files — filtered by policy age and by the `protect` globs |
| Class | **UserData** — the last scavenge wave, after every rebuildable and every cache |
| Confirmation | `confirm = true` **by default**: candidates queue for `disk-saver review` (§9) |
| Config | `roots`, `exclude`, `max_depth`, `max_age`, `min_age`, `confirm`, `protect` |
| Defaults | `max_age = "30d"`, `min_age = "7d"`, `confirm = true`, `protect = ["*env*"]` |

`protect` matches an entry's **final path component** and defaults to `["*env*"]`, so `.env`,
`venv`, `env/` and friends are never proposed. It is checked twice: once while planning, and
again in `execute` immediately before removal — so an approval that has been sitting in the
queue cannot delete a path that a config change has since protected. The second check is
redundant by construction, which is the point.

---

## 13. CLI surface

```
disk-saver run [--dry-run] [--force] [--adapter <name>…]
               [--pressure <comfortable|normal|scavenge>]
    One-shot cycle (the scheduled entry point). --force bypasses the throttle gate;
    --pressure overrides measurement — for testing and for "scavenge NOW".
disk-saver plan
    Alias for run --dry-run: candidate table (adapter, item, size, age, class,
    confirm?), no changes.
disk-saver review [--list] [--approve <id>…] [--approve-all [--adapter <n>]] [--json]
    Inspect and resolve the approvals queue (§9). Interactive by default.
disk-saver status
    Disk usage vs thresholds, cadence state (normal/pressure, next full run due),
    pending approvals (count + bytes), last-run summaries from KV (freed, errors,
    skipped-unavailable adapters and why).
disk-saver why <id-or-path-substring>            [only with the decision-log feature]
    Replay every recorded decision about an item: observed, kept-because, queued,
    deleted (§15.2).
disk-saver doctor
    Config validation + every adapter's check(): docker reachable? trash readable?
    notifications deliverable?
disk-saver config init | check | show
    Write a fully-commented default ~/.disk-saver.toml | validate | print effective config.
disk-saver schedule install | uninstall | status
    Manage the launchd agent / systemd user timer (§14). install runs `config check`
    first; status flags drift between the installed unit and [schedule] config.
disk-saver state clear <adapter> | show <adapter>
    Escape hatch: inspect or reset an adapter's KV bucket.
```

Global flags: `--config <path>`, `--verbose`, `--json` (machine-readable output for
`run`/`plan`/`status`/`review --list`).

---

## 14. Scheduling & adaptive cadence

The binary stays one-shot, but cadence lives in **config, not in the timer**. The OS
timer fires *often*; the binary decides *cheaply* whether a full run is due:

```
 timer (check_every = 1h)          throttle gate                     full run
 ──────────────────────►  statvfs + one KV read (~1ms)  ──────────►  observe/plan/…
                          │
                          ├─ free ≥ warn_below: due every run_every          (12h)
                          ├─ free <  warn_below: due every pressure_run_every (1h)
                          └─ not due → exit 0
```

Why this shape instead of rewriting timer schedules dynamically:

- **"Run more frequently" = edit one config line.** `run_every = "6h"` takes effect at
  the next timer fire — no `schedule install` re-run, no unit rewriting, no OS-specific
  code path. Only changing `check_every` (the timer itself) needs a reinstall, and
  `schedule status` flags that drift.
- **Pressure-adaptive for free.** When free space dips below `warn_below`, the gate
  switches to `pressure_run_every` automatically — disk-saver reacts within an hour of
  a disk filling up, then calms back down, with zero scheduler mutation. Doing this by
  installing/removing transient timers (`systemd-run`, launchctl juggling) would be two
  OS-specific, failure-prone mechanisms.
- **Cheap.** A throttled fire is one statvfs + one sqlite read; battery impact is noise.
- Classic anacron trick, well proven.

`disk-saver schedule install` detects the OS and renders the unit from `[schedule]` —
the only OS-branching code outside `platform`, and it's CLI-level (not adapter-level):

- **macOS** — `~/Library/LaunchAgents/com.disk-saver.plist`: `StartInterval = check_every`,
  stderr into the log dir. launchd coalesces fires missed while asleep into one on wake —
  exactly right for laptops.
- **Linux** — `~/.config/systemd/user/disk-saver.{service,timer}`:
  `OnBootSec=5m`, `OnUnitActiveSec = check_every`, `Persistent=true`,
  `RandomizedDelaySec=5m`. Fallback for cron-only boxes: a documented crontab line
  (`0 * * * * disk-saver run`) — the throttle gate makes a dumb hourly cron equivalent
  to the real timers, and the single-instance lock makes overlap harmless.

Timers always fire plain `disk-saver run` — pressure measurement and the gate decide
everything else. `run --force` exists for "I want a full run *now*".

---

## 15. Observability: logs, decision records, notifications

A tool that deletes files while nobody watches must be able to explain itself later —
and speak up *before* doing something dramatic.

### 15.1 Operational logging (always compiled)

`tracing`. Interactive: pretty stderr. Scheduled: daily-rotated files in
`<state_dir>/logs/` (keep ~14 days). One INFO line per deletion, one summary line per
run (including throttled exits at DEBUG). This layer is for "is it healthy?".

### 15.2 Decision records (cargo feature `decision-log`, default **on**)

The "when/why" audit layer, compiled fully in or out:

- **On:** every candidate-affecting decision is appended as one JSON line to
  `<state_dir>/decisions.jsonl` (size-capped, rotated):
  `observed`, `kept { reason: "age 3d < min_age 7d" }`, `planned`,
  `queued_for_confirmation`, `approved { by: "review" }`, `denied`, `snoozed`,
  `deleted { bytes }`, `skipped`, `failed` — each with timestamp, adapter, id, size,
  age, and the pressure at the time. `disk-saver why <item>` greps and renders an
  item's timeline. This is the layer that answers "where did my node_modules go, and
  why did it think that was OK?"
- **Off (`--no-default-features …`):** `DecisionLog` compiles to a zero-sized struct
  with empty inlined methods — call sites are unchanged and the optimizer removes the
  code, the strings, and the I/O entirely. The `why` subcommand disappears from the
  CLI. For minimal builds on constrained devboxes.

Engine and adapters record through the same handle (`ctx.decisions.record(…)` style),
so the feature flag lives in `core` and every crate inherits it.

### 15.3 Desktop notifications (runtime config, §7 `[notifications]`)

Delivered via `Platform::notify` (macOS `osascript`, Linux `notify-send`; downgraded to
a log line if unavailable). Best-effort, never fail a run. Events:

| Event | Trigger | Example |
|---|---|---|
| `scavenge_warning` | free space crossed below `warn_below` | "Disk space getting low (11% free). Scavenge begins below 8% — cleanup now running hourly." |
| `scavenge_ran` | a scavenge-mode run executed | "Freed 14.2 GB (rust targets, docker images). 16% free." — or, if the target wasn't reached: "Freed 3.1 GB but still below target." |
| `approvals_pending` | queue gained items / still non-empty | "12 items (9.4 GB) await approval — run `disk-saver review`." |
| `adapter_failing` | same adapter `Failed` 3 consecutive runs | "docker adapter failing since Tue — see `disk-saver status`." |

Dedupe: per-event `last_notified` timestamps in `_engine`, repeat suppressed within
`min_gap` (default 12h); tier *transitions* (e.g. dropping below `warn_below`) always
notify. Which events fire is the `events` list; `enabled = false` silences everything.

### 15.4 Run reports

Persisted per full run to KV `_engine` (ring buffer, last ~50): timestamp, pressure,
free before/after, per-adapter {candidates, removed, bytes, skipped, queued, error}.
`disk-saver status` renders these; `--json` for scripting. Exit codes as in §8 —
monitorable by launchd/systemd without parsing output.

---

## 16. Testing strategy

| Layer | Approach |
|---|---|
| `kv` | Unit tests on in-memory sqlite: round-trips, bucket isolation, prefix iteration. |
| `platform` | `FakePlatform` unit-tested itself (it's load-bearing test infra). `RealPlatform` gets a tempdir-based suite covering the invariants that only bite on a real filesystem: symlinks are never followed when sizing or deleting, a cross-device child is left intact, relative paths are refused, and `run_command` survives a chatty child and enforces its timeout. Both OSes run it — clippy and tests are a `[ubuntu-latest, macos-latest]` matrix, because roughly every adapter has a `cfg(target_os)` branch and a single-OS run compiles one side of each. |
| **Adapters** | **The payoff of the Platform design.** Pure-Rust integration tests, no OS, no docker, deterministic: build a fake world → `observe` → advance the fake clock → `plan`/`execute` → assert the exact set of `remove_*` calls, KV contents, and outcomes. Table-driven scenario tests: cold state + grace period, daemon down (`Unavailable`), pressure transitions, `min_age` floor under scavenge, protect globs, `.disk-saver-keep`, `confirm = true` producing flagged candidates, partial execute failure. Docker fixtures = canned CLI JSON in `tests/fixtures/`. Every adapter's suite lives in `tests/<adapter>.rs` and reaches the crate only through `factory()` + the `Adapter` trait — no `use super::*`, so a test can't be satisfied by an internal it shouldn't know about. The sole exception is `adapter-trash`, whose `.trashinfo` date parser and `io::Error`→`AdapterError` mapping have no route in from outside (`FakePlatform` cannot inject `PermissionDenied`); those three keep a small `#[cfg(test)]` module in `src/tests.rs`. Adding a private-helper unit test means arguing for that exception. |
| Engine (`core`) | Scripted stub adapters: panic isolation, error isolation, comfortable ⇒ zero plan/execute calls, **wave ordering** (Rebuildable exhausted before Cache before UserData) and early-stop when the fake free space rises past target, **flagged candidates land in `_approvals` and are never executed**, queue rebuild/GC semantics, **throttle gate** (fake clock: due/not-due/pressure-cadence), notification events + `min_gap` dedupe (FakePlatform records them), lock contention, exit codes. |
| Confirmation flow | e2e: seed a queue → `review --list --json` asserts contents → `review --approve <id>` deletes via the adapter and records outcomes; interactive path smoke-tested with scripted stdin. |
| End-to-end | `assert_cmd` tests of the binary against a tempdir config/state: `config init` → `plan --json` → asserts on output; plus the `review`/`schedule` flows in `tests/stage2.rs`. There are no `#[ignore]`d tests and nothing in the suite touches a real docker, filesystem root or scheduler — the whole suite runs on a clean machine with no daemons, which is what makes it worth running on every commit. |
| CI | Thirteen jobs in `ci.yml`, aggregated behind a single required `Gate` context that `needs:` every one of them — so adding a job makes it a merge gate automatically, with no list of check names to keep in sync. Beyond fmt/clippy/test: a **feature matrix** (the unions `--all-features` cannot reach, so the `decision-log`-off ZST and an adapter-trimmed CLI can't rot), rustdoc under `-D warnings`, an MSRV compile on the declared `rust-version`, `cargo-udeps`, `cargo-autoinherit`, a release-profile build (fat LTO is not something a debug build exercises), `cargo package` for every crate, and actionlint + shellcheck over the workflows and scripts. Every gate is a script in `scripts/`, run identically by CI and by the pre-commit hook, so the two cannot drift on flags. `release.yml` *calls* `ci.yml` on a tag, so a release runs the same suite a pull request does. |

---

## 17. Dependencies

Kept deliberately lean; all versions pinned once in `[workspace.dependencies]`:

| Crate | Why |
|---|---|
| `rusqlite` (bundled) | KV store; zero system deps |
| `serde`, `serde_json`, `toml` | config + KV values (JSON) + opaque `toml::Value` handoff |
| `clap` (derive) | CLI |
| `thiserror` / `anyhow` | error taxonomy in libraries / propagation at the edges |
| `tracing`, `tracing-subscriber`, `tracing-appender` | logging + rotation |
| `humantime`, `humantime-serde` | `"7d"` durations |
| `bytesize` | `"40GB"` sizes |
| `rustix` | statvfs (disk usage) in `RealPlatform` |
| `fd-lock` | single-instance lock |
| `globset` | protect/exclude patterns |
| `ratatui`, `similar` | the TUI installer only (§3) — never linked into the shipped binary |
| `assert_cmd`, `predicates`, `tempfile` (dev) | e2e tests |

Notably absent: no async runtime (§18), no `chrono` (std `SystemTime` + humantime
suffice), no `sysinfo` (statvfs is 20 lines), no notification crate (osascript/
notify-send via `run_command` machinery), no TUI crate in the shipped binary (`review` is
plain stdin prompts; the separate, on-demand `disk-saver-installer` is the only thing that
depends on `ratatui`).

---

## 18. Design decisions & tradeoffs

1. **Sync, not async.** A periodic batch job that walks filesystems and shells out
   has nothing to win from tokio: `async` would infect every trait (`async_trait`,
   `Send` bounds) and complicate the Platform fake, while filesystem walking is
   blocking anyway. Hang-protection comes from subprocess timeouts, not task
   cancellation. Revisit only if adapter wall-time ever matters (then: thread pool, not
   async — the trait stays sync).
2. **Sequential adapters.** Simpler (`&mut self`), and I/O contention between parallel
   walkers can easily make things *slower*. The engine owns iteration, so parallelism
   is a contained future change.
3. **observe/plan/execute, not one `clean()`.** Costs a slightly bigger trait; buys
   gap-free usage tracking, free dry-run, a confirmation queue that's just candidate
   routing, and centrally-ordered scavenging with an early stop. This is the core of
   the design.
4. **Frequent timer + self-throttling, not dynamic timer rewriting.** Cadence belongs
   to the program, not the scheduler: one portable mechanism gives config-adjustable
   frequency *and* automatic pressure-adaptive speed-up, at the cost of a ~1ms no-op
   fire per hour. Rewriting launchd/systemd units at runtime would be two fragile,
   OS-specific mechanisms.
5. **Confirmation via queue + notification + `review`, not blocking prompts.** A
   scheduled job has no TTY; the only honest designs are "don't delete" or "ask
   asynchronously". Flagged candidates are *never* auto-deleted — a hard rule, so the
   answer to "can scavenge empty my trash behind my back?" is provably "no" rather
   than "usually not". (macOS actionable notifications need a signed app bundle —
   out of scope; the notification just points at `review`.)
6. **Adapter-opaque config with one reserved key (`enabled`).** Full opacity would put
   even enablement inside adapters; one blessed key keeps "what runs" an engine concern
   while every real knob (including `confirm`) stays compartmentalized — the engine
   sees its effect only as a candidate flag.
7. **KV over per-adapter tables.** Adapters get schemaless persistence with zero
   migration ceremony; JSON values keep it debuggable. If some adapter someday needs
   relational queries, it can graduate without breaking others (the Store can hand out
   dedicated tables later).
8. **Static registry + cargo features, no plugin system.** Compiled-in adapters are
   type-safe, testable, and shippable as one binary. Dynamic loading (dlopen/wasm) is a
   lot of machinery for a personal janitor; the trait boundary means it could be added
   later without redesign.
9. **Decision records as a compile-time feature, default on.** Explainability is a core
   value of the tool, so it ships enabled; making it a *feature* (not a runtime toggle)
   means lean builds pay zero code/size/runtime cost, and the ZST-with-empty-methods
   pattern keeps call sites identical either way. The risk (feature rot) is covered by
   the CI feature-matrix build.
10. **Policy filtering inside adapters, ordering inside the engine.** Adapters apply
    `min_age`/`max_age`/`confirm` themselves (config opacity preserved); the engine only
    needs `(class, last_used, bytes, requires_confirmation)` on candidates to order
    globally, gate confirmations, and stop at the target.
11. **mtime heuristics over atime.** atime is off (`noatime`) or smeared on most modern
    systems; mtime-of-project + `.git/HEAD` is predictable and explainable to a user
    staring at `plan` output.
12. **Hard-fail on bad config** rather than skip-and-continue: a janitor silently not
    janitoring is the failure mode users notice weeks later, at 100% disk.

---

## 19. Delivery history

All six landed; v0.1 is cut. Kept because the ordering is the argument for the architecture:
each milestone was green and independently reviewable, which is only possible because the
`Platform` trait let every adapter be tested before any real OS integration existed.

1. **Foundations** — `kv` crate; `platform` crate with `Platform` (incl. `notify`),
   `RealPlatform` (Linux+macOS), `FakePlatform`; `core` with config loading (global +
   schedule + notifications), pressure model incl. `warn_below`, `Adapter`/`Candidate`/
   errors, throttle gate, `decision-log` feature skeleton; engine skeleton; `cli` with
   `run --dry-run`, `plan`, `config init|check`, lock, logging; one trivial in-tree
   demo adapter to prove the loop. Workspace deps filled in.
2. **Filesystem adapters** — `scan` crate + `rust-target`, `node-modules`,
   `python-cache`; full fake-platform test suites; guardrails §11.5–8; wave-ordered
   scavenge with early stop.
3. **Docker adapter** — observation/KV tracking, grace period, Unavailable path,
   fixtures; `doctor`.
4. **Trash adapter + confirmation** — XDG + macOS KV-tracked variant; FDA detection &
   messaging; `_approvals` queue, `review` command, `approvals_pending` notification.
5. **Scheduling & notifications** — `schedule install|uninstall|status` rendering units
   from `[schedule]`, launchd/systemd units, remaining notification events, run reports
   + `status`, `why`, exit codes, log rotation.
6. **Hardening** — macOS CI job, feature-matrix CI, e2e tests, soak on real machines
   behind `--dry-run`, then README + `config init` polish. Cut v0.1.

Since v0.1, in the same shapes and without touching the engine — which is the claim the
trait boundary was making: the `cachedir` engine and the `pnpm`/`cargo-registry`/`pip`
adapters on top of it (§12.6); the two git adapters (§12.7–12.8); the `rust-target`
incremental sweep; the `Configured<T>` three-state config so an adapter default and a user
threshold can be reconciled rather than silently ranked (§4.4); the TUI installer; and the
release pipeline — cosign-signed archives for six targets, gated on the same `ci.yml` a
pull request runs.

---

## 20. Decisions taken since the draft

The design draft closed with nine open questions. All nine are settled in the shipped
program. They are recorded as answers rather than deleted, because otherwise a reader of the
code cannot tell which defaults were chosen and which were merely inherited.

1. **Default `roots` is `["~"]`.** Zero-config beat safe-but-inert: an adapter that does
   nothing until configured is the failure mode nobody notices until the disk is full. The
   walk is bounded instead — `max_depth = 8`, dot-directories skipped, and a built-in
   denylist (`~/Library`, `~/.Trash`, cache dirs) applied to *both* roots and excludes, so a
   configured root at or under a denied path is dropped rather than quietly walked.
2. **Trash keeps `confirm = true`.** The alternative — auto-delete at `max_age`, confirm only
   under scavenge — was rejected: it makes the answer to "can this empty my trash behind my
   back?" *"usually not"* instead of *"no"*, and that hard guarantee is worth more than the
   bytes. Advisory-until-reviewed is the real cost; `approvals_pending` notifications
   (§15.3) are what keep it from being silent. `confirm = false` remains available.
3. **`.venv` is opt-in** (`include_venvs = true`). Regenerable in principle, expensive in
   practice, and frequently the only copy of a pinned wheel set.
4. **Docker volumes are permanently out**, not deferred. A volume is the one docker object
   holding data nothing can rebuild, and no age heuristic separates a stale volume from a
   database somebody needs next month.
5. **External-volume trash stays out.** Scope creep, and an unmounted volume makes the
   KV age record meaningless in the unsafe direction.
6. **Cadence unchanged** — `check_every = 1h`, `run_every = 12h`, `pressure_run_every = 1h`.
   The 30m option was not taken: the hourly wake already costs about a millisecond, and what
   actually bounds response time under pressure is `pressure_run_every`, not the timer.
7. **All four notification events ship on, `min_gap = 12h`.** A janitor that deletes without
   saying so is the surprise this design exists to avoid; the gap keeps that from becoming
   noise.
8. **`decision-log` ships on.** Explainability is a core value of the tool, not something a
   user should have to know to opt into. Lean builds still pay nothing — the ZST pattern
   (§15.2) keeps call sites identical, and the CI feature matrix stops the compiled-out path
   from rotting.
9. **Per-file `target/` trimming was not built.** The scavenge-only `incremental` sweep
   (§12.2–12.4) answered the same need for a fraction of the machinery: since cargo never
   reclaims anything, the win was in the one directory cargo *rewrites* under a changing key,
   not in per-file granularity across all the ones it keeps.

---

## 21. Future adapter ideas

Nothing here is committed to. The two shared engines are what make most of them small: a new
global cache is a name and a resolver against `cachedir` (§12.6), and anything keyed off a
repo or project marker is a `Rule` against `scan`.

In rough priority order: Homebrew
(`brew cleanup` + cache), Xcode `DerivedData`, npm/yarn global stores, sccache, `uv`
global cache, Go module/build cache (`~/go/pkg/mod`, `go clean -cache`), container image
stores for podman/nerdctl, JetBrains/VS Code caches, `journalctl --vacuum-size` (Linux),
old kernels/apt/dnf caches (Linux, needs root story), generic `CACHEDIR.TAG` sweeper, and
an opt-in `~/Downloads` adapter (UserData class, `confirm = true` always, long ages).
