#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Build starling.wasm: the StarlingMonkey runtime as a component, with
# SpiderMonkey, libc and libc++ statically linked in and dead-code eliminated. It
# runs under `wasmtime run` and `wasmtime serve`, and `starling-componentize`
# links wit-dylib bindings against its core module. Requires `wasm-tools` on PATH.
#
# `WASM_TARGET` picks the wasm target, as it does for the justfile's wasm
# recipes: `p2`, `p3`, or a full triple. Defaults to wasm32-wasip3, the target
# the componentizer links against.
set -euo pipefail

cd "$(dirname "$0")/.."

case "${WASM_TARGET:-p3}" in
    p2) TARGET=wasm32-wasip2 ;;
    p3) TARGET=wasm32-wasip3 ;;
    *) TARGET="${WASM_TARGET}" ;;
esac

# The pinned stable toolchain ships no wasm32-wasip3 std, so that target builds with the
# nightly `STARLING_NIGHTLY` names, `nightly` by default, as the justfile's wasm recipes do.
if [ "$TARGET" = wasm32-wasip3 ]; then
    CARGO=(cargo "+${STARLING_NIGHTLY:-nightly}")
else
    CARGO=(cargo)
fi

# SpiderMonkey compiles against the WASI SDK sysroot for the target. A sysroot for
# wasm32-wasip3 first ships in wasi-sdk 34, and a build against an older SDK fails deep
# in the C++ compile, so it is checked here instead.
WASI_SDK_PATH="${WASI_SDK_PATH:-/opt/wasi-sdk}"
if [ "$TARGET" = wasm32-wasip3 ] && [ ! -d "$WASI_SDK_PATH/share/wasi-sysroot/lib/wasm32-wasip3" ]; then
    echo "wasi-sdk 34+ required for $TARGET: no wasm32-wasip3 sysroot under WASI_SDK_PATH=$WASI_SDK_PATH" >&2
    echo "Point WASI_SDK_PATH at a wasi-sdk 34 or later, or build for p2 with WASM_TARGET=p2." >&2
    exit 1
fi

BUILD_DIR="${CARGO_TARGET_DIR:-target}"
OUT="$BUILD_DIR/$TARGET/release/starling.wasm"

# The `starlingmonkey` package's library is the runtime. Its `build.rs` adds the link arguments
# the componentizer needs: an exported, growable function table, and on wasm32-wasip2 the exported
# shadow stack pointer, which the wit-dylib bindings module imports from the runtime's core
# module.
#
# `--skip-wit-component` makes the link produce the core module, whose `component-type` sections
# `preserve-component-type.py` copies before `wasm-tools component new` turns it into the
# component. `starling-componentize` links against the complete worlds in those copies.
"${CARGO[@]}" rustc \
    -p starlingmonkey \
    --lib \
    --target "$TARGET" \
    --release \
    -- -C link-arg=--skip-wit-component

# The wasm header's version field: 1 for a core module, 0x1000d for a component.
wasm_version() {
    od -An -tx1 -j4 -N4 "$1" | tr -d ' \n'
}

case "$(wasm_version "$OUT")" in
    01000000)
        cp "$OUT" "$OUT.tmp"
        ./scripts/preserve-component-type.py "$OUT.tmp"
        wasm-tools component new "$OUT.tmp" -o "$OUT.tmp"
        mv "$OUT.tmp" "$OUT"
        ;;
    0d000100)
        # cargo did not relink, so this is the component an earlier run produced, unless a plain
        # `cargo build` of the same profile replaced it.
        if ! grep -q 'custom "starling:component-type' <<<"$(wasm-tools objdump "$OUT")"; then
            echo "$OUT is a component without the preserved worlds: remove it and rerun" >&2
            exit 1
        fi
        ;;
    *)
        echo "$OUT is not a wasm module or component" >&2
        exit 1
        ;;
esac

echo "built $OUT ($(du -h "$OUT" | cut -f1))"
