#!/usr/bin/env bash
# Build and test the feature combinations that no other gate reaches.
#
# Every adapter is its own crate behind a cargo feature and the audit trail is behind
# `decision-log`, so the default `--all-features` clippy and test runs only ever exercise the
# everything-on build. What that never compiles is the *absence* of a feature: the
# `decision-log`-off path in core, where `DecisionLog` becomes a zero-sized no-op, and a CLI
# with adapters trimmed out. Both are advertised in the README as supported builds, and both
# rot silently — a `why`-recording call added inside a `#[cfg(feature = "decision-log")]`
# island compiles green everywhere except the build nobody runs.
#
# The gate behind the Features job in .github/workflows/ci.yml. Not in .githooks/pre-commit:
# each line is a distinct feature union, so each is a fresh compile of most of the workspace.
#
# Usage:
#   ./scripts/check-features.sh

set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
-h | --help)
    sed -n '2,/^$/p' "$0" | sed 's/^#\( \|$\)//'
    exit 0
    ;;
"") ;;
*)
    echo "error: unknown argument '$1' (try --help)" >&2
    exit 1
    ;;
esac

run() {
    echo "▶ $*"
    "$@"
}

# Every adapter, so the list below stays in step with crates/cli/Cargo.toml's `default`.
ALL_ADAPTERS="docker,node-modules,rust-target,python-cache,pnpm,cargo-registry,pip,git-gc,git-ignored,trash"

# Leanest possible build: no adapters, no decision-log.
run cargo build --workspace --no-default-features --locked
run cargo clippy -p disk-saver --no-default-features --all-targets --locked -- -D warnings

# decision-log compiled OUT of core — the ZST path.
run cargo test -p disk-saver-core --no-default-features --locked

# All adapters with decision-log OFF, which is the combination that exercises every adapter's
# compiled-out `why` recording.
run cargo test -p disk-saver --no-default-features --features "$ALL_ADAPTERS" --locked

echo "✓ feature matrix OK"
