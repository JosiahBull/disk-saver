# Contributing

## First, install the hooks

```bash
./scripts/install-hooks.sh
```

Hooks live in `.githooks/` so they are reviewed like any other code, but git will not use them
until `core.hooksPath` points there — and that setting is per clone, so committing the hooks is
not enough. Every fresh clone needs this once.

The pre-commit hook runs the deterministic CI gates, cheapest first, so most failures never
reach a runner. Bypass a single commit with `git commit --no-verify`; remove it entirely with
`./scripts/install-hooks.sh --uninstall`.

It is a convenience, not a guarantee — `Gate` in CI is what actually protects `main`. The hook
only saves you the round trip.

## Building

```bash
cargo build                  # the shipped `disk-saver` binary and its libraries
cargo build --workspace      # …plus the TUI installer
```

`default-members` is `crates/cli`, so a plain `cargo build`/`run`/`test` never compiles
`disk-saver-installer` and never pulls `ratatui`. Anything that must cover it says `--workspace`,
which is what every CI job does.

Two other things worth knowing:

* **`cargo install` needs `--locked`.** Without it cargo re-resolves the whole graph, and the
  version you get is not the one CI tested.
* **`rusqlite` is built `bundled`**, so every build compiles SQLite from C source. That is why
  cross-compiling in the release workflow goes through `cross` rather than bare `cargo` — the
  container carries the cross C toolchain.

## Checks

Every gate is a script in `scripts/`, and each CI job runs the same script you do — so the two
cannot drift on flags.

```bash
./scripts/check-fmt.sh             # cargo fmt --all --check
./scripts/check-clippy.sh          # -D warnings, over --all-targets --all-features
./scripts/test.sh                  # cargo test --workspace --all-features
./scripts/check-features.sh        # the feature unions --all-features can never reach
./scripts/check-docs.sh            # rustdoc -D warnings
./scripts/check-msrv.sh            # compiles on the declared rust-version
./scripts/check-versions.sh        # one version, inherited everywhere, matching the tag
./scripts/check-workspace-deps.sh  # cargo autoinherit: one dependency table
./scripts/check-dependabot.sh      # every 0.x direct dep held back from minor bumps
./scripts/check-unused-deps.sh     # cargo-udeps (needs nightly)
./scripts/check-publish.sh         # every crate packages cleanly for crates.io
./scripts/check-workflows.sh       # actionlint + shellcheck + Gate covers every job
```

Each takes `--help`, which is where the reasoning lives — why `check-clippy.sh` deliberately
does *not* export `RUSTFLAGS`, why the 0.x list exists at all, and so on.

Four of them need something extra installed, which is why they are not in the hook:

```bash
cargo install --locked cargo-autoinherit   # check-workspace-deps.sh
cargo +nightly install --locked cargo-udeps
rustup toolchain install 1.88 --profile minimal   # check-msrv.sh, or whatever rust-version says
brew install actionlint shellcheck          # check-workflows.sh (skipped when absent)
```

## Lints

The lint set lives in `[workspace.lints]` in the root `Cargo.toml` and every member inherits it
with `[lints] workspace = true`, so a plain local `cargo clippy` enforces the same bar as CI
rather than the difference surfacing on a pull request.

It is deliberately small and bug-shaped rather than a blanket `pedantic`: the lints that are
there each describe a way *this* program could delete the wrong thing or panic mid-run —
`path_buf_push_overwrite`, `filetype_is_file`, `string_slice`, `float_cmp`. Each entry carries
the reason, and the two `#[expect(clippy::filetype_is_file)]` exemptions in
`crates/platform/src/sys.rs` explain why the lint is wrong at exactly those two sites. Prefer
adding an `#[expect(…, reason = "…")]` over dropping a lint from the set.

## CI

`Gate` is the only required status check on `main`. It is an aggregate job depending on every
other job in `ci.yml`, so adding a job makes it a merge gate automatically; there is no list of
check names to keep in sync. `scripts/check-workflows.sh` fails the build if a job is missing
from `gate.needs`, which is the one way that design can silently spring a leak.

Branch protection matches by exact job display name, so renaming a job breaks a check that then
never reports, and nothing can merge. Add the new job alongside the old, merge, update
protection, then remove the old one.

Note `ci.yml` triggers on `pull_request: branches: [main]`, so a pull request based on another
branch gets no CI at all. Stacked branches need retargeting to `main` before they can be checked.

### Repository settings this expects

Once the repository is on GitHub:

* branch protection on `main` requiring the single `Gate` context, with `enforce_admins` on
* **Allow auto-merge** enabled, which `dependabot-auto-merge.yml` needs
* **Allow GitHub Actions to create and approve pull requests** left *off* — nothing here needs it

## Releasing

The version lives in exactly one place, `[workspace.package] version`, and every member
inherits it. Internal dependencies in `[workspace.dependencies]` carry a copy of it (cargo
cannot package a path dependency without a version requirement), and so does `Cargo.lock` —
`./scripts/check-versions.sh` asserts all three agree, and fails the release if the tag
disagrees with any of them.

```bash
# 1. bump [workspace.package] version, then refresh the lockfile's copy
$EDITOR Cargo.toml
cargo check --workspace
./scripts/check-versions.sh

# 2. commit, tag, push
git commit -am "chore: release v0.2.0"
git tag v0.2.0
git push origin main v0.2.0
```

The tag push runs `release.yml`, which *calls `ci.yml`* — so a release runs the identical gate
suite a pull request does, and cannot publish anything a pull request would have rejected. On a
tag, `Versions` additionally asserts `v0.2.0` matches the declared `0.2.0`. Only then does it
build the six target archives, sign every one with keyless cosign, and create the GitHub release.

`workflow_dispatch` with `release_type: dev` produces a timestamped prerelease from whatever is
on the branch — useful for handing someone a build without spending a version number.

### Publishing to crates.io

Not wired up, deliberately: `release.yml` publishes GitHub releases only, and nothing in this
repository holds a crates.io token. The repository is *ready* for it — `check-publish.sh` proves
every crate packages with its metadata, license and README — but the first publish is a manual,
irreversible act, and crate names cannot be reused.

When you do it, order matters: `cargo publish` resolves each crate's dependencies from the
registry, so leaves go first.

```bash
./scripts/check-publish.sh   # what CI already ran, to be sure

# Leaves first, then their dependents. Each `publish` has to wait for the previous crate to
# appear in the index before the next one can resolve it:
#   disk-saver-kv, disk-saver-platform      (no internal dependencies)
#   disk-saver-core                         (kv, platform)
#   disk-saver-scan, disk-saver-cachedir
#   disk-saver-adapter-*                    (all ten)
#   disk-saver, disk-saver-installer        (last)
cargo publish -p disk-saver-kv
cargo publish -p disk-saver-platform
cargo publish -p disk-saver-core
# …and so on, finishing with `-p disk-saver`.
```
