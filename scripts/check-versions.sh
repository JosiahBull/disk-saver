#!/usr/bin/env bash
# Assert the workspace version is declared once, inherited by every member, mirrored by every
# internal dependency requirement and by Cargo.lock — and, when running on a tag, that the tag
# matches it.
#
# Pure text parsing, so it needs no toolchain and no cache. Run by the Versions job in
# .github/workflows/ci.yml. Because release.yml calls that workflow on a tag push, this is what
# makes it impossible to publish a signed v0.2.0 from a tree whose Cargo.toml still says 0.1.0
# — the check runs before anything builds.
#
# Usage:
#   ./scripts/check-versions.sh          verify
#   ./scripts/check-versions.sh --print  print the version, for release.yml

set -euo pipefail
cd "$(dirname "$0")/.."

# ::error:: renders as an annotation on the PR diff; plain text reads better in a terminal, and
# the hook runs in a terminal.
fail() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::error::$*" >&2
    else
        echo "error: $*" >&2
    fi
    exit 1
}

usage() { sed -n '2,/^$/p' "$0" | sed 's/^#\( \|$\)//'; }

case "${1:-}" in
-h | --help)
    usage
    exit 0
    ;;
--print | "") ;;
*) fail "unknown argument '$1' (try --help)" ;;
esac

# Read the [workspace.package] block only, and print its version line. Scoping to the block
# matters: a bare `grep '^version' Cargo.toml | head -1` is correct only by accident of which
# table happens to come first.
version=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p;}' Cargo.toml)
[ -n "$version" ] || fail "could not parse [workspace.package] version from Cargo.toml"

case "$version" in
[0-9]*.[0-9]*.[0-9]*) ;;
*) fail "version '$version' is not of the form X.Y.Z" ;;
esac

if [ "${1:-}" = "--print" ]; then
    echo "$version"
    exit 0
fi

# A member pinning its own version would ship a number the tag never mentions.
for manifest in crates/*/Cargo.toml; do
    grep -Eq '^version(\.workspace[[:space:]]*=[[:space:]]*true|[[:space:]]*=[[:space:]]*\{[[:space:]]*workspace)' "$manifest" ||
        fail "$manifest does not inherit its version from [workspace.package]"
done

# Every internal crate carries a `version` next to its `path` in [workspace.dependencies],
# because cargo refuses to package a crate whose path dependency has no version requirement —
# without it nothing here is publishable at all. That makes the version a second place the
# number lives, so it is a second place it can go stale: a bump that misses these produces a
# workspace that builds perfectly from the path and a published crate that resolves the *old*
# release of every sibling.
while IFS= read -r line; do
    name=${line%% *}
    req=$(printf '%s\n' "$line" | sed -n 's/.*version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
    [ -n "$req" ] ||
        fail "internal dependency '$name' in [workspace.dependencies] has a path but no version — cargo cannot package a crate that depends on it"
    [ "$req" = "$version" ] ||
        fail "internal dependency '$name' requires $req but the workspace is $version"
done < <(grep -E '^disk-saver[A-Za-z0-9_-]* = \{ path = ' Cargo.toml)

# Cargo.lock carries a copy of every workspace member's version. Nothing else in CI would
# notice it going stale: a job that does not pass --locked lets cargo silently fix it up in the
# runner and go green.
for manifest in crates/*/Cargo.toml; do
    name=$(sed -n 's/^name[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$manifest" | head -n1)
    [ -n "$name" ] || fail "could not parse package name from $manifest"
    locked=$(awk -v pkg="name = \"$name\"" '
        $0 == pkg { found = 1; next }
        found && /^version = / { gsub(/^version = "|"$/, ""); print; exit }
    ' Cargo.lock)
    [ -n "$locked" ] || fail "$name is missing from Cargo.lock — run: cargo check --workspace && git add Cargo.lock"
    [ "$locked" = "$version" ] ||
        fail "Cargo.lock has $name at $locked but the workspace is $version — run: cargo check --workspace && git add Cargo.lock"
done

echo "workspace version: $version"

# On a tag build the tag is the published name, so a mismatch would ship a release whose
# binaries disagree with their own version number.
if [ "${GITHUB_REF_TYPE:-}" = "tag" ]; then
    tag="${GITHUB_REF_NAME:-}"
    case "$tag" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "tag '$tag' is not of the form vX.Y.Z" ;;
    esac
    [ "${tag#v}" = "$version" ] || fail "tag $tag does not match the declared version $version"
    echo "tag              : $tag (matches)"
fi

echo "✓ versions OK"
