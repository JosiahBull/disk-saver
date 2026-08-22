#!/usr/bin/env bash
# Find dependencies that are declared but never used.
#
# Part of the Dependencies job in .github/workflows/ci.yml. cargo-udeps needs a nightly
# toolchain because it reads unstable compiler output, which is why this is not in
# .githooks/pre-commit — a commit hook has no business assuming a nightly is installed.
#
# Usage:
#   ./scripts/check-unused-deps.sh

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
"") ;;
*) fail "unknown argument '$1' (try --help)" ;;
esac

command -v cargo-udeps >/dev/null 2>&1 ||
    fail "cargo-udeps is not installed (cargo +nightly install --locked cargo-udeps)"

cargo +nightly udeps --workspace --all-targets --all-features
echo "✓ no unused dependencies"
