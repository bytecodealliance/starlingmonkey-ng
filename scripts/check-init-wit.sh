#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Fail if `starling/wit/init.wit` changed since a release without a version of its package that
# is not compatible with the release's. `starling-componentize` accepts a runtime whose version of
# the `init` interface has the same canonical version as its own, so a change to the interface
# needs a different canonical version. Doc comments are ignored.
#
# Usage: scripts/check-init-wit.sh [<revision>]
#
# The revision defaults to the latest release tag (`v*`) reachable from HEAD. Without one, there is
# nothing to compare with, and the check passes. Needs `wasm-tools` on PATH.
set -euo pipefail

file=starling/wit/init.wit
base=${1:-}
if [ -z "$base" ]; then
    if ! base=$(git describe --tags --abbrev=0 --match 'v[0-9]*' HEAD 2>/dev/null); then
        echo "No release tag is reachable from HEAD, so there is nothing to compare $file with."
        exit 0
    fi
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if ! git show "$base:$file" > "$tmp/base.wit" 2>/dev/null; then
    echo "$base has no $file, so there is nothing to compare it with."
    exit 0
fi
wasm-tools component wit --no-docs "$tmp/base.wit" > "$tmp/base.txt"
wasm-tools component wit --no-docs "$file" > "$tmp/current.txt"

# The package's version, from its `package name@version;` line.
version() {
    sed -n 's/^package [^@;]*@\([^;]*\);$/\1/p' "$1"
}
# The canonical version of a version, which the componentizer compares: the major number from
# `1.0.0` on, `0.` and the minor number for a `0.x` version whose minor number isn't 0, and the
# version without its build metadata for a `0.0.x` version or a pre-release.
canonical() {
    local version=${1%%+*}
    local major=${version%%.*} minor
    minor=${version#*.}
    minor=${minor%%.*}
    if [[ $version == *-* ]]; then
        echo "$version"
    elif [[ $major != 0 ]]; then
        echo "$major"
    elif [[ $minor != 0 ]]; then
        echo "0.$minor"
    else
        echo "$version"
    fi
}

old=$(version "$tmp/base.txt")
new=$(version "$tmp/current.txt")
if diff -q <(grep -v '^package ' "$tmp/base.txt") <(grep -v '^package ' "$tmp/current.txt") \
    > /dev/null; then
    echo "$file is unchanged since $base, apart from doc comments and its version."
    exit 0
fi
if [ -n "$old" ] && [ "$(canonical "$old")" = "$(canonical "$new")" ]; then
    echo "$file changed since $base, but its version, $new, has the canonical version" \
        "$(canonical "$new") of $old there. Give it a version with a different canonical version." >&2
    diff -u --label "$base" --label "HEAD" "$tmp/base.txt" "$tmp/current.txt" >&2 || true
    exit 1
fi
echo "$file changed since $base, and its version went from ${old:-none} to $new."
