#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download Deno and Bun into deps/deno-bin/ and deps/bun-bin/, for comparing their results on the
# Node.js tests with tests/node-harness/run-node-tests.mjs (`--target=deno` and `--target=bun`).
#
# Usage:
#   ./scripts/fetch-other-runtimes.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DENO_VERSION="v2.9.7"
BUN_VERSION="bun-v1.4.2"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) DENO_PLATFORM="aarch64-apple-darwin" BUN_PLATFORM="darwin-aarch64" ;;
    Darwin-x86_64) DENO_PLATFORM="x86_64-apple-darwin" BUN_PLATFORM="darwin-x64" ;;
    Linux-aarch64) DENO_PLATFORM="aarch64-unknown-linux-gnu" BUN_PLATFORM="linux-aarch64" ;;
    Linux-x86_64) DENO_PLATFORM="x86_64-unknown-linux-gnu" BUN_PLATFORM="linux-x64" ;;
    *) echo "Unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ "$("$ROOT_DIR/deps/deno-bin/deno" --version 2>/dev/null | head -1)" != "deno ${DENO_VERSION#v}"* ]]; then
    echo "Downloading Deno $DENO_VERSION..."
    curl --fail --silent --show-error --location -o "$TMP/deno.zip" \
        "https://github.com/denoland/deno/releases/download/$DENO_VERSION/deno-$DENO_PLATFORM.zip"
    rm -rf "$ROOT_DIR/deps/deno-bin"
    mkdir -p "$ROOT_DIR/deps/deno-bin"
    unzip -q "$TMP/deno.zip" -d "$ROOT_DIR/deps/deno-bin"
fi
echo "$("$ROOT_DIR/deps/deno-bin/deno" --version | head -1) at deps/deno-bin"

if [[ "$("$ROOT_DIR/deps/bun-bin/bun" --version 2>/dev/null)" != "${BUN_VERSION#bun-v}" ]]; then
    echo "Downloading Bun $BUN_VERSION..."
    curl --fail --silent --show-error --location -o "$TMP/bun.zip" \
        "https://github.com/oven-sh/bun/releases/download/$BUN_VERSION/bun-$BUN_PLATFORM.zip"
    rm -rf "$ROOT_DIR/deps/bun-bin"
    mkdir -p "$ROOT_DIR/deps/bun-bin"
    unzip -q -j "$TMP/bun.zip" "*/bun" -d "$ROOT_DIR/deps/bun-bin"
fi
echo "Bun $("$ROOT_DIR/deps/bun-bin/bun" --version) at deps/bun-bin"
