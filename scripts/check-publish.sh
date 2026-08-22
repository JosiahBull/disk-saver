#!/usr/bin/env bash
# Assert every crate in the workspace can actually be packaged for crates.io, and that the
# tarballs carry the metadata and the license text a published crate is expected to carry.
#
# This catches the mistakes that only ever surface at `cargo publish` time, when the version is
# already tagged and the release already built:
#
#   * a path dependency with no `version` — cargo refuses to package a crate that has one,
#     because a consumer resolving from the registry has no path to follow. Every internal
#     requirement therefore carries a version, and scripts/check-versions.sh keeps it in step
#     with the workspace.
#   * a missing `description` or `license`, both of which crates.io rejects.
#   * a file that is present locally but not in the tarball — the README is a symlink to the
#     workspace one, and the LICENSE text likewise, so "it renders on my machine" proves
#     nothing about what a consumer downloads.
#
# `--no-verify`, deliberately: the verification build compiles each packaged crate against the
# *registry* versions of its dependencies, and this workspace's own crates are not published, so
# it cannot resolve them. Whether the code compiles is the whole rest of CI's job.
#
# The gate behind the Package job in .github/workflows/ci.yml. Not in .githooks/pre-commit: it
# needs the crates.io index and writes seventeen tarballs.
#
# Usage:
#   ./scripts/check-publish.sh

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

version=$(bash scripts/check-versions.sh --print)

# `--allow-dirty` so this is runnable on a working tree with edits in it. Publishing from a
# dirty tree is a separate concern, and one the tag-vs-manifest check in check-versions.sh and
# a required Gate already cover.
cargo package --workspace --no-verify --allow-dirty --locked

# Every member, by the package name cargo writes into the tarball name.
#
# The tarball listing goes into a variable rather than down a pipe into `grep -q`. Under
# `set -o pipefail` that pipe is a race: `grep -q` exits the moment it matches, `tar` then
# writes into a closed pipe, and the pipeline reports *tar's* SIGPIPE failure — so a crate
# whose LICENSE is present fails the check whenever grep happens to win. It is timing
# dependent, which is the worst kind of release gate: it passed locally and failed on CI for
# 4 of 17 crates in the same run.
missing=0
for manifest in crates/*/Cargo.toml; do
    # Quit inside sed rather than `| head -n1`, which is the same SIGPIPE race in miniature.
    name=$(sed -n '/^name[[:space:]]*=/{s/^name[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p;q;}' "$manifest")
    crate="target/package/$name-$version.crate"
    [ -f "$crate" ] || fail "$name produced no tarball at $crate"

    listing=$(tar -tzf "$crate")

    # The license text itself, not just the SPDX expression in the manifest. Each crate
    # directory holds a symlink to the workspace LICENSE, which cargo follows when packaging —
    # so a deleted symlink is a crate published without its own license file.
    # A here-string, not a pipe: `grep -q <<<"$listing"` is a single command, so there is no
    # pipeline for pipefail to report and no writer to receive SIGPIPE.
    grep -qxF "$name-$version/LICENSE" <<<"$listing" || {
        echo "  ✗ $name: no LICENSE in the tarball" >&2
        missing=1
    }

    # The README is what crates.io renders as the crate's front page, and only the binary
    # crate declares one.
    if [ "$name" = "disk-saver" ]; then
        grep -qxF "$name-$version/README.md" <<<"$listing" ||
            fail "disk-saver would publish with no README (crates/cli/README.md is a symlink to the workspace one — is it still there?)"
    fi
done
[ "$missing" -eq 0 ] || fail "at least one crate would publish without its license text"

echo "✓ every crate packages cleanly, with its license and README"
