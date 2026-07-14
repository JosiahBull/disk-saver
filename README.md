# disk-saver

A small, boring, trustworthy disk janitor for **macOS and Linux**.

`disk-saver` wakes up on a schedule, checks how much disk space is free, and — *only if
space is actually needed* — deletes things that demonstrably haven't been used in a while:
docker images, `node_modules/`, cargo `target/` dirs, Python caches, old trash. Low-impact,
regenerable things it cleans on its own; for anything scarier (your trash) it queues the item
and asks you to confirm.

When space is plentiful, the most common run is a deliberate no-op.

> Status: v0.1. See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full design and rationale.

## How it works

- **Pressure-driven.** Free space on one filesystem is mapped to a tier. Above
  `start_cleaning_below` → *observe only, delete nothing*. Between thresholds → *normal*:
  delete items older than each adapter's `max_age`. Below `scavenge_below` → *scavenge*:
  delete down to `scavenge_target`, lowest-impact first, never touching anything younger than
  `min_age`.
- **Lowest-impact first.** Scavenging burns rebuildable build artifacts before caches, and
  caches before anything you put somewhere yourself.
- **Age-tracked.** Every adapter derives or records a *last-used* time per item; observation
  runs every cycle so ages never have gaps.
- **Human-in-the-loop where it matters.** High-impact deletions (trash) queue for approval
  and are *never* deleted behind your back.
- **Adaptive cadence.** A cheap hourly timer fires; the binary decides whether a full run is
  due (every 12h normally, hourly under pressure). Cadence lives in config, not the timer.

## Install

```sh
cargo install --path crates/cli          # installs the `disk-saver` binary
# or a lean build without some adapters / the audit log:
cargo install --path crates/cli --no-default-features --features docker,rust-target
```

## Quick start

```sh
disk-saver config init          # write a fully-commented ~/.disk-saver.toml
disk-saver config check         # validate it
disk-saver plan                 # dry run: show what WOULD be cleaned, change nothing
disk-saver doctor               # is docker reachable? trash readable? notifications work?

disk-saver schedule install     # install the launchd agent / systemd user timer
disk-saver status               # disk vs thresholds, cadence, pending approvals, recent runs

disk-saver run                  # one cleanup cycle (what the scheduler calls)
disk-saver review               # approve/deny queued high-impact deletions
```

Nothing is deleted unless free space is below `start_cleaning_below` **and** an item is older
than the policy. Use `disk-saver run --dry-run` or `disk-saver plan` any time to preview.

## Commands

| Command | What it does |
|---|---|
| `run [--dry-run] [--force] [--adapter <name>…] [--pressure <tier>] [--detailed]` | One cleanup cycle. `--force` bypasses the throttle gate; `--pressure` overrides measurement. |
| `plan [--pressure <tier>] [--detailed]` | Alias for `run --dry-run`: the candidate table, no changes. `--detailed` adds a per-candidate table (project/path, size, age, class) for each adapter. |
| `review [--list] [--approve <id>…] [--approve-all [--adapter <n>]] [--json]` | Inspect and resolve the approvals queue. Interactive by default. |
| `status` | Disk usage vs thresholds, cadence, pending approvals, recent run summaries. |
| `why <query>` | Replay every recorded decision about an item *(requires the `decision-log` feature)*. |
| `doctor` | Validate config and probe every enabled adapter. |
| `config init \| check \| show` | Write a default config \| validate \| print the effective config. |
| `schedule install \| uninstall \| status` | Manage the launchd agent / systemd user timer. |
| `state clear <adapter> \| show <adapter>` | Inspect or reset an adapter's stored state. |

Global flags: `--config <path>`, `--verbose`, `--json`.

## Configuration

Single file, `~/.disk-saver.toml` (override with `--config` or `$DISK_SAVER_CONFIG`). A missing
file means built-in defaults. `disk-saver config init` writes a commented starting point:

```toml
[global]
start_cleaning_below = "20%"   # more free than this → observe only
warn_below           = "12%"   # less free → notify + faster cadence
scavenge_below       = "8%"    # less free → scavenge mode
scavenge_target      = "15%"   # scavenge until this much is free

[adapters.docker]
max_age = "7d"                 # normal mode threshold
min_age = "2d"                 # scavenge floor: never touch anything newer
protect = ["postgres:*"]       # globs to never remove

[adapters.node-modules]
roots   = ["~/dev", "~/work"]
max_age = "30d"

[adapters.trash]
confirm = true                 # queue for `disk-saver review` instead of auto-deleting
```

Every `[adapters.<name>]` table takes `enabled = false` to turn an adapter off. Thresholds
accept absolute sizes (`"40GB"`) or percentages (`"10%"`); durations use `"7d"`/`"36h"` syntax.

## Adapters (v1)

| Adapter | Removes | Impact class |
|---|---|---|
| `docker` | exited containers, unused images, dangling images, build cache | Cache / Rebuildable |
| `node-modules` | `node_modules/` next to a `package.json` | Cache |
| `rust-target` | cargo `target/` (next to `Cargo.toml`, with `CACHEDIR.TAG`) | Rebuildable |
| `python-cache` | `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox` (`.venv` opt-in) | Rebuildable |
| `pnpm` | global pnpm store (via `pnpm store path`) + metadata cache | Cache |
| `cargo-registry` | `~/.cargo` registry cache/src + git db/checkouts (index kept) | Cache |
| `pip` | pip download/wheel cache under the platform cache dir | Cache |
| `trash` | OS recycle bin (XDG on Linux, `~/.Trash` on macOS) | UserData (confirm by default) |

The three global-cache adapters (`pnpm`, `cargo-registry`, `pip`) prune a whole cache directory
when it has been idle past the policy; add `paths = ["…"]` to any of them to cover a non-standard
cache location.

Each adapter is its own crate behind a cargo feature (all on by default), plus a `decision-log`
feature (default on) for the `why`-command audit trail. Trim any of them for a leaner build.

## Safety

For a tool that deletes files unattended, paranoia is a feature:

- **Comfortable = delete nothing** — the common case is a no-op.
- **`min_age` floor** — even at critical pressure, recently-used items are never touched.
- **Confirmation gating** — trash (and anything you mark `confirm = true`) is never
  auto-deleted; it waits for `disk-saver review`.
- **Strict identification** — a `target/` is only cleaned with a sibling `Cargo.toml` *and* a
  `CACHEDIR.TAG`; `node_modules/` only with a sibling `package.json`; etc.
- **`.disk-saver-keep`** in a project root exempts the whole project.
- **No symlink traversal, ever** — walking, sizing, or deleting. Walks stay on one filesystem.
- **First-sighting grace** — nothing is deleted in the run that first discovered it.
- **Dry-run is first-class**, and (with `decision-log`) every deletion *and every decision not
  to delete* is recorded — `disk-saver why <item>` replays it.

## Development

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
```

The workspace is a tiny engine (`disk-saver-core`) orchestrating independent adapters, all
talking to the OS through a single `Platform` trait — so every adapter is integration-tested
against an in-memory fake with a controllable clock, no real filesystem or docker required.

## License

MIT OR Apache-2.0.
