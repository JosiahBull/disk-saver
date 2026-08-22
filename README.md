# disk-saver

A small, boring, trustworthy disk janitor for **macOS and Linux**.

`disk-saver` wakes up on a schedule, checks how much disk space is free, and —
*only if space is actually needed* — deletes things that haven't been used in a
while: docker images, `node_modules/`, cargo `target/` dirs, Python caches, old
trash.

## How it works

- **Pressure-driven.** Free space on one filesystem is mapped to a tier.
- **Lowest-impact first.** Scavenging burns rebuildable build artifacts before caches, and
  caches before anything you put somewhere yourself.
- **Age-tracked.** Every adapter derives or records a *last-used* time per item.
- **Human-in-the-loop where it matters.** High-impact deletions (trash) queue for approval
  and are *never* deleted behind your back.
- **Adaptive cadence.** A cheap hourly timer fires; the binary decides whether a full run is
  due (every 12h normally, hourly under pressure).


## Install

For a one-shot interactive setup, run the installer — a small wizard that picks a cleaning
preset, lets you toggle adapters and set project roots, and then writes the config.

```sh
cargo run -p disk-saver-installer            
cargo run -p disk-saver-installer -- -y # Headless
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

## Configuration

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

[adapters.rust-target]
roots               = ["~/dev"]
max_age             = "30d"
incremental_min_age = "15m"    # scavenge-only sweep of target/**/incremental (see below)

[adapters.trash]
confirm = true                 # queue for `disk-saver review` instead of auto-deleting
```

Every `[adapters.<name>]` table takes `enabled = false` to turn an adapter off. Thresholds
accept absolute sizes (`"40GB"`) or percentages (`"10%"`); durations use `"7d"`/`"36h"` syntax.

## Development

```sh
./scripts/install-hooks.sh    # once per clone: runs the fast CI gates before each commit
./scripts/test.sh             # cargo test --workspace --all-features
./scripts/check-clippy.sh     # -D warnings
cargo fmt --all
```

## License

MIT — see [`LICENSE`](LICENSE).
