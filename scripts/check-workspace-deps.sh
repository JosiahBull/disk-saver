#!/usr/bin/env bash
# Assert every dependency requirement lives in the root [workspace.dependencies] and every
# member manifest says only `foo.workspace = true`.
#
# `cargo autoinherit` is the check and the fix in one: it hoists any requirement it finds
# declared in a member into the root table and rewrites the member to inherit it. So the test
# is whether it changes anything — a diff here is the patch to apply.
#
# Why it matters for a workspace of seventeen crates: two members declaring `rusqlite = "0.32"`
# and `rusqlite = "0.33"` compile fine and link two copies, and a Dependabot bump then updates
# one of them. The single table is also what scripts/check-dependabot.sh parses, so a
# requirement hidden in a member manifest is one the 0.x guard never sees.
#
# The gate behind the Workspace dependencies job in .github/workflows/ci.yml, and the same
# script .githooks/pre-commit runs.
#
# Usage:
#   ./scripts/check-workspace-deps.sh

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

command -v cargo-autoinherit >/dev/null 2>&1 ||
    fail "cargo-autoinherit is not installed (cargo install --locked cargo-autoinherit)"

# Snapshotted rather than left to `git diff --exit-code`, which CI can use because a runner's
# tree is pristine. Locally it is not: this also runs from the pre-commit hook, where an
# unrelated unstaged edit would fail the gate and a stray `git checkout` would destroy work.
manifests() { cat Cargo.toml crates/*/Cargo.toml; }

before=$(manifests)
cargo autoinherit
after=$(manifests)

if [ "$before" != "$after" ]; then
    # The rewrite has already happened, so the fix is to review it, not to re-run anything.
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        git --no-pager diff -- Cargo.toml 'crates/*/Cargo.toml' || true
    fi
    fail "cargo autoinherit hoisted a dependency into [workspace.dependencies] — review \`git diff\` and stage it"
fi

echo "✓ every dependency is declared once, in [workspace.dependencies]"
