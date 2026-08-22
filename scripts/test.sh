#!/usr/bin/env bash
# Run the test suite. The gate behind the Tests job in .github/workflows/ci.yml, on both
# runner OSes.
#
# `--all-features` matches check-clippy.sh, so a feature-gated test is built rather than
# silently skipped. Deliberately not `--all-targets`: for `cargo test` that *excludes*
# doctests rather than adding to them, which would drop every documented example.
#
# Usage:
#   ./scripts/test.sh

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

# Nudge, once, if this clone has no hooks. `core.hooksPath` is per clone, so it survives no
# amount of committing the hooks — someone has to run the installer, and until this line there
# was nothing anywhere that said so.
#
# Here rather than in the check-* scripts because the pre-commit hook runs several of those in
# a row, and a reminder printed once per script would be noise aimed at the one person who does
# not need it.
#
# A note, never an error. Hooks are the convenience and ci.yml is the gate, so missing them
# must not be the reason a command fails. Silent under CI, where `core.hooksPath` is meant to
# be unset, and outside a git repository at all.
if [ -z "${CI:-}" ] && [ "$(git config --get core.hooksPath 2>/dev/null || true)" != ".githooks" ]; then
    echo "note: git hooks are not installed in this clone." >&2
    echo "      ./scripts/install-hooks.sh runs these same checks before each commit." >&2
fi

cargo test --workspace --all-features --locked
echo "✓ tests OK"
