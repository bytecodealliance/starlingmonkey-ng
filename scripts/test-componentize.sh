#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Run the componentization suites of `componentize/tests/`, several test binaries at a time.
#
# The script builds every test binary first, then runs `COMPONENTIZE_JOBS` of them at a time. The
# default is one per 8 GiB of memory, at least one and at most one per CPU. `serve` and
# `top_level`, which take longest, start first. Each binary runs `RUST_TEST_THREADS` tests at a
# time, by default twice the number of CPUs divided by the number of jobs, but at most one per GiB
# of memory divided by the number of jobs, and at least one.
#
# Each binary's output goes to a log file. Once the binary exits, the script prints its
# `test result:` line if it passed, and the whole log if it failed. A binary that exits without a
# `test result:` line failed too. The script fails if any binary did.
#
# The suites use the runtime build `STARLING_RUNTIME` names. The arguments go to every test
# binary, so `./scripts/test-componentize.sh echo_stream` runs only the matching tests. Needs `jq`
# on PATH.
#
# Usage:
#   ./scripts/test-componentize.sh [<libtest argument>...]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/../componentize"

cpus=$(getconf _NPROCESSORS_ONLN)
if [[ -r /proc/meminfo ]]; then
    memory_kib=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo)
else
    memory_kib=$(( $(sysctl -n hw.memsize) / 1024 ))
fi
jobs=$(( memory_kib / (8 * 1024 * 1024) ))
(( jobs > cpus )) && jobs=$cpus
(( jobs < 1 )) && jobs=1
jobs=${COMPONENTIZE_JOBS:-$jobs}
threads=$(( 2 * cpus / jobs ))
memory_threads=$(( memory_kib / (1024 * 1024) / jobs ))
(( threads > memory_threads )) && threads=$memory_threads
(( threads < 1 )) && threads=1
export RUST_TEST_THREADS=${RUST_TEST_THREADS:-$threads}

echo "=== Building the componentization suites"
binaries=$(cargo test --tests --no-run --message-format=json-render-diagnostics |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null)
        | .executable')

# The binaries, `serve` and `top_level` first. A binary's name is its file name without the hash
# cargo appends.
ordered=$(printf '%s\n' "$binaries" | awk '{
    name = $0
    sub(/.*\//, "", name)
    sub(/-[0-9a-f]+$/, "", name)
    print (name == "serve" || name == "top_level" ? 0 : 1) "\t" $0
}' | sort -s -k1,1 | cut -f2-)
if [[ -z "$ordered" ]]; then
    echo "The build produced no test binaries." >&2
    exit 1
fi
count=$(printf '%s\n' "$ordered" | wc -l | tr -d ' ')

LOGS=$(mktemp -d)
trap 'rm -rf "$LOGS"' EXIT
export LOGS

# Run the test binary `$1` with the remaining arguments, logging to `$LOGS/<name>.log`. Prints
# the binary's `test result:` line if it passed, and its log otherwise, in which case it creates
# `$LOGS/<name>.failed`.
run_binary() {
    local binary=$1
    shift
    local name
    name=$(basename "$binary" | sed -E 's/-[0-9a-f]+$//')
    local log="$LOGS/$name.log"
    if "$binary" "$@" > "$log" 2>&1 && grep -q '^test result: ' "$log"; then
        echo "$name: $(grep '^test result: ' "$log" | tail -n 1)"
    else
        touch "$LOGS/$name.failed"
        echo "=== $name failed"
        cat "$log"
        echo "=== end of $name"
    fi
}
export -f run_binary

echo "=== Running $count test binaries, $jobs at a time, $RUST_TEST_THREADS tests each"
# xargs appends the binary after the libtest arguments, and the inner shell moves it to the front.
# shellcheck disable=SC2016
printf '%s\n' "$ordered" | tr '\n' '\0' |
    xargs -0 -n 1 -P "$jobs" bash -c 'run_binary "${@: -1}" "${@:1:$#-1}"' _ "$@"

failed=""
for marker in "$LOGS"/*.failed; do
    [[ -e "$marker" ]] && failed="$failed $(basename "$marker" .failed)"
done
if [[ -n "$failed" ]]; then
    echo "Failed test binaries: $failed" >&2
    exit 1
fi
echo "All $count test binaries passed."
