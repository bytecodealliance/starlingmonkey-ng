#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# End-to-end test of the runtime component: build the debug starling.wasm and run JS scripts with
# it under wasmtime: a hello-world that logs before and after a timer, and a top level that
# rejects after a timer, which fails the run. `RUNTIME_TARGET` picks the target, as for
# `scripts/build-runtime.sh`.
set -euo pipefail

cd "$(dirname "$0")/.."
. scripts/wasm-target.sh

./scripts/build-runtime.sh --debug
RUNTIME="$(runtime_path "$(wasm_target "${RUNTIME_TARGET:-p3}")" debug)"
RUNTIME="$(cd "$(dirname "$RUNTIME")" && pwd)/$(basename "$RUNTIME")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

cat >hello.js <<'JS'
console.log("hello from JS");
setTimeout(() => {
  console.log("timer fired");
}, 10);
JS

cat >late_rejection.js <<'JS'
await new Promise((resolve) => setTimeout(resolve, 10));
throw new Error("late top-level failure");
JS

run_script() {
    wasmtime run --dir=.::/cwd --dir=. -Shttp=y "$RUNTIME" "$@"
}

output=$(run_script hello.js)
echo "$output"
[[ "$output" == *"hello from JS"* ]] || { echo "FAIL: missing console output"; exit 1; }
[[ "$output" == *"timer fired"* ]] || { echo "FAIL: missing timer output"; exit 1; }
[[ "$output" == *"hello from JS"*"timer fired"* ]] || { echo "FAIL: console/timer output out of order"; exit 1; }

if errors=$(run_script late_rejection.js 2>&1 >/dev/null); then
    echo "FAIL: a late top-level rejection must fail the run"; exit 1
fi
echo "$errors"
[[ "$errors" == *"late top-level failure"* ]] || { echo "FAIL: missing rejection message"; exit 1; }

echo "runtime e2e OK"
