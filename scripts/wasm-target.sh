# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Sourced by the scripts and the justfile to name wasm targets and their builds.

# Print the target triple `$1` names: `p2`, `p3`, or a full triple.
wasm_target() {
    case "$1" in
        p2) echo wasm32-wasip2 ;;
        p3) echo wasm32-wasip3 ;;
        *) echo "$1" ;;
    esac
}

# Print the cargo command that builds for the target triple `$1`. The pinned stable toolchain
# ships no wasm32-wasip3 std, so that target builds with the beta toolchain `STARLING_BETA` names,
# `beta` by default.
wasm_cargo() {
    if [ "$1" = wasm32-wasip3 ]; then
        echo "cargo +${STARLING_BETA:-beta}"
    else
        echo cargo
    fi
}

# Print the path of the runtime component `scripts/build-runtime.sh` builds for the target triple
# `$1` in the cargo profile directory `$2`, `debug` or `release`.
runtime_path() {
    echo "${CARGO_TARGET_DIR:-target}/$1/$2/starling.wasm"
}
