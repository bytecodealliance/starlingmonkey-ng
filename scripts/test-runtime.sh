#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# End-to-end test of the static runtime: build starling.wasm and run JS scripts
# with it under wasmtime: a hello-world, and a top level that rejects late.
set -euo pipefail

cd "$(dirname "$0")/.."

case "${WASM_TARGET:-p3}" in
    p2) TARGET=wasm32-wasip2 ;;
    p3) TARGET=wasm32-wasip3 ;;
    *) TARGET="${WASM_TARGET}" ;;
esac
RUNTIME="${CARGO_TARGET_DIR:-target}/$TARGET/release/starling.wasm"

./scripts/build-runtime.sh

run_script() {
    wasmtime run --dir=.::/cwd --dir=. --dir=/tmp \
        -Sinherit-env=y,inherit-network=y,http=y,tcp=y,udp=y,p3=y \
        -Wcomponent-model-async=y \
        "$RUNTIME" "$@"
}

output=$(run_script tests/dylib/hello.js)
echo "$output"
[[ "$output" == *"hello from JS"* ]] || { echo "FAIL: missing console output"; exit 1; }
[[ "$output" == *"timer fired"* ]] || { echo "FAIL: missing timer output"; exit 1; }
[[ "$output" == *"hello from JS"*"timer fired"* ]] || { echo "FAIL: console/timer output out of order"; exit 1; }

if errors=$(run_script tests/dylib/late_rejection.js 2>&1 >/dev/null); then
    echo "FAIL: a late top-level rejection must fail the run"; exit 1
fi
echo "$errors"
[[ "$errors" == *"late top-level failure"* ]] || { echo "FAIL: missing rejection message"; exit 1; }

echo "static runtime e2e OK"
