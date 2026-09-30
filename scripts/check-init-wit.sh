#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Fail if `starling/wit/init.wit` changed since a release without a new major or minor version of
# its package. `starling-componentize` accepts a runtime whose version of the `init` interface
# differs from its own only in the patch number, so a change to the interface needs a version
# that differs in the major or minor number or the pre-release suffix. Doc comments are ignored.
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
# The major and minor numbers and the pre-release suffix of a version, which the componentizer
# compares.
compared() {
    local version=${1%%+*} pre=
    if [[ $version == *-* ]]; then
        pre=${version#*-}
        version=${version%%-*}
    fi
    echo "${version%.*} $pre"
}

old=$(version "$tmp/base.txt")
new=$(version "$tmp/current.txt")
if diff -q <(grep -v '^package ' "$tmp/base.txt") <(grep -v '^package ' "$tmp/current.txt") \
    > /dev/null; then
    echo "$file is unchanged since $base, apart from doc comments and its version."
    exit 0
fi
if [ -n "$old" ] && [ "$(compared "$old")" = "$(compared "$new")" ]; then
    echo "$file changed since $base, but its version, $new, has the major and minor numbers" \
        "and the pre-release suffix of $old there. Give it a new major or minor version." >&2
    diff -u --label "$base" --label "HEAD" "$tmp/base.txt" "$tmp/current.txt" >&2 || true
    exit 1
fi
echo "$file changed since $base, and its version went from ${old:-none} to $new."
