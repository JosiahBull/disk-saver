#!/usr/bin/env bash
# Lint the workspace. The gate behind the Clippy job in .github/workflows/ci.yml, and the same
# command .githooks/pre-commit runs.
#
# Deliberately does not export RUSTFLAGS="-D warnings" the way the workflow does. RUSTFLAGS is
# part of cargo's fingerprint, so setting it here would invalidate every local build artifact
# on each commit and again on the next plain `cargo build` — which is how a hook earns a
# permanent `--no-verify`. Nothing is lost: clippy-driver reports rustc lints too, and
# --all-targets covers the same targets the workflow denies on.
#
# The lint set itself lives in [workspace.lints] in Cargo.toml, so a plain local
# `cargo clippy` enforces the same bar as this script.
#
# Usage:
#   ./scripts/check-clippy.sh

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

cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
echo "✓ clippy OK"
