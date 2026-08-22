#!/usr/bin/env bash
# Compile the workspace on exactly the toolchain [workspace.package] rust-version names.
#
# A declared MSRV that nothing checks is a guess, and this repository shipped a wrong one: the
# manifest said 1.85 while crates/platform/src/fake.rs used let-chains, stable only since 1.88.
# Nothing catches that locally, because everyone's toolchain is newer — and cargo's resolver
# reads `rust-version` when picking dependency versions, so the wrong number also silently
# changes what a consumer resolves.
#
# `cargo check`, not a full build: this is about whether the language and the dependency graph
# are within the declared floor, and every later gate covers whether it works.
#
# The gate behind the MSRV job in .github/workflows/ci.yml. Not in .githooks/pre-commit — it
# needs a second toolchain installed and compiles the workspace from scratch in its own
# target directory.
#
# Usage:
#   ./scripts/check-msrv.sh          verify
#   ./scripts/check-msrv.sh --print  print the declared MSRV, for ci.yml's toolchain input

set -euo pipefail
cd "$(dirname "$0")/.."

fail() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::error::$*" >&2
    else
        echo "error: $*" >&2
    fi
    exit 1
}

case "${1:-}" in
-h | --help)
    sed -n '2,/^$/p' "$0" | sed 's/^#\( \|$\)//'
    exit 0
    ;;
--print | "") ;;
*) fail "unknown argument '$1' (try --help)" ;;
esac

msrv=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^rust-version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p;}' Cargo.toml)
[ -n "$msrv" ] || fail "could not parse [workspace.package] rust-version from Cargo.toml"

# The workflow installs the toolchain before it can run the check, so it asks for the number
# first. Parsing it in one place is the point: a second copy of this sed in ci.yml would be a
# second thing to update, and it would fail by installing the wrong toolchain and passing.
if [ "${1:-}" = "--print" ]; then
    echo "$msrv"
    exit 0
fi

echo "declared MSRV: $msrv"

rustup toolchain list 2>/dev/null | grep -q "^$msrv" ||
    fail "toolchain $msrv is not installed (rustup toolchain install $msrv --profile minimal)"

# A separate target directory, deliberately. Artifacts are keyed by rustc version, so sharing
# target/ would make this run evict the stable build and the next `cargo build` recompile the
# world — the same reason check-clippy.sh does not export RUSTFLAGS.
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/msrv" \
    cargo "+$msrv" check --workspace --all-targets --all-features --locked

echo "✓ compiles on $msrv"
