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

## Developer tool caches and Node versions

The `uv` and `npm` adapters ask for approval by default. `homebrew` and `nvm` remove completed
download archives without asking, and `nvm` always asks before removing a Node version:

| Adapter | Cleanup scope |
|---|---|
| `nvm` | Completed binary/source downloads and inactive Node installations. Installed versions always require approval, including their global packages. |
| `uv` | Unused entries through `uv cache prune`, using uv's cache locks. Installed Python runtimes and tools are outside its scope. |
| `npm` | The `_cacache` download cache through npm. Keeps npx environments, logs, configuration, and installed packages. |
| `homebrew` | Completed, SHA-256-prefixed download archives. Keeps installed formulae/casks, partial downloads, and open files. |

Approval details include recorded access/activity timestamps. These are filesystem proxies;
reading a file can update its access time, so they are not execution history. Cleanup rechecks
paths and activity before acting and skips candidates accessed or changed since planning.

```toml
[adapters.nvm]
roots = ["~/work", "~/personal"] # check .nvmrc and .node-version pins here
max_age = "90d"
min_age = "30d"
include_versions = true
confirm = false                 # true also asks before removing archives; versions always ask
protect = ["v24.18.1"]           # additional version or alias specifications
# nvm_dir = "~/.nvm"             # otherwise NVM_DIR, then ~/.nvm

[adapters.uv]
confirm = true
# cache_dir = "~/.cache/uv"       # otherwise queried from uv

[adapters.npm]
confirm = true
# cache_dir = "~/.npm"            # otherwise queried from npm

[adapters.homebrew]
confirm = false
# cache_dir = "~/Library/Caches/Homebrew" # otherwise queried from brew
```

nvm protects the current and default versions, named aliases, configured `protect` entries,
project pins under `roots`, and installations with an open Node executable. Pins outside those
roots are not checked. Set `include_versions = false` to clean only downloaded archives.
An unavailable `lsof` check prevents archive deletion and Node retirement.

The cache adapters default to `max_age = "30d"` and `min_age = "7d"`; nvm defaults to 90d/30d.
uv's candidate size is the cache footprint, an upper bound on what native pruning can reclaim.
Reported freed bytes use the observed size decrease after maintenance.

Preview with `disk-saver plan --adapter nvm --adapter uv --adapter npm --adapter homebrew --pressure scavenge --detailed`.
Planning does not run cleanup commands or add items to the approval queue.

## Development

```sh
./scripts/install-hooks.sh    # once per clone: runs the fast CI gates before each commit
./scripts/test.sh             # cargo test --workspace --all-features
./scripts/check-clippy.sh     # -D warnings
cargo fmt --all
```

## License

MIT — see [`LICENSE`](LICENSE).
