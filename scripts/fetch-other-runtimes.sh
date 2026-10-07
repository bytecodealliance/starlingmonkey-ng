#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download Deno and Bun into deps/deno-bin/ and deps/bun-bin/, for comparing their results on the
# Node.js tests with tests/node-harness/run-node-tests.mjs (`--target=deno` and `--target=bun`).
# Each archive's SHA-256 digest is checked against the digest listed below, from the release's
# `.sha256sum` files for Deno and its SHASUMS256.txt for Bun.
#
# Usage:
#   ./scripts/fetch-other-runtimes.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DENO_VERSION="v2.9.7"
BUN_VERSION="bun-v1.4.2"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)
        DENO_PLATFORM="aarch64-apple-darwin" BUN_PLATFORM="darwin-aarch64"
        DENO_SHA256=5cd46d6268f6f78f5d88bdc7159d20bd44cdaa4b3303474839f87ec6fe7ae25c
        BUN_SHA256=90987a3a16d7db556d886ac3d551e7b6d3edf0a1cf43acaed622e8676be1d12f
        ;;
    Darwin-x86_64)
        DENO_PLATFORM="x86_64-apple-darwin" BUN_PLATFORM="darwin-x64"
        DENO_SHA256=95daaff11c116a52ad54785e7914c8e9c9cdcaba793c5ed929c74ca2d8e6259a
        BUN_SHA256=80520d7e17526308c9185d261679ac6d27798d3803a0e9f7ff9121ab8affb012
        ;;
    Linux-aarch64)
        DENO_PLATFORM="aarch64-unknown-linux-gnu" BUN_PLATFORM="linux-aarch64"
        DENO_SHA256=c832298b1ad4422481334855f6003e0f54145762c5a134f20a489511d2f65bbf
        BUN_SHA256=54328bbc2d9c8e0c9f892c544d66c57a83b84139e34909e5ee81758f1ac8fda7
        ;;
    Linux-x86_64)
        DENO_PLATFORM="x86_64-unknown-linux-gnu" BUN_PLATFORM="linux-x64"
        DENO_SHA256=c6527f24f4b16031d3ae4fa9f658d5f11534c8d84ce7dc8502420280919c3490
        BUN_SHA256=36368faef7527875d5ffa52e53cd48021741f2a83eb6208a8dd64068d422a913
        ;;
    *) echo "Unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ "$("$ROOT_DIR/deps/deno-bin/deno" --version 2>/dev/null | head -1)" != "deno ${DENO_VERSION#v} "* ]]; then
    echo "Downloading Deno $DENO_VERSION..."
    curl --fail --silent --show-error --location -o "$TMP/deno.zip" \
        "https://github.com/denoland/deno/releases/download/$DENO_VERSION/deno-$DENO_PLATFORM.zip"
    "$SCRIPT_DIR/check-sha256.sh" "$TMP/deno.zip" "$DENO_SHA256"
    rm -rf "$ROOT_DIR/deps/deno-bin"
    mkdir -p "$ROOT_DIR/deps/deno-bin"
    unzip -q "$TMP/deno.zip" -d "$ROOT_DIR/deps/deno-bin"
fi
echo "$("$ROOT_DIR/deps/deno-bin/deno" --version | head -1) at deps/deno-bin"

if [[ "$("$ROOT_DIR/deps/bun-bin/bun" --version 2>/dev/null)" != "${BUN_VERSION#bun-v}" ]]; then
    echo "Downloading Bun $BUN_VERSION..."
    curl --fail --silent --show-error --location -o "$TMP/bun.zip" \
        "https://github.com/oven-sh/bun/releases/download/$BUN_VERSION/bun-$BUN_PLATFORM.zip"
    "$SCRIPT_DIR/check-sha256.sh" "$TMP/bun.zip" "$BUN_SHA256"
    rm -rf "$ROOT_DIR/deps/bun-bin"
    mkdir -p "$ROOT_DIR/deps/bun-bin"
    unzip -q -j "$TMP/bun.zip" "*/bun" -d "$ROOT_DIR/deps/bun-bin"
fi
echo "Bun $("$ROOT_DIR/deps/bun-bin/bun" --version) at deps/bun-bin"
