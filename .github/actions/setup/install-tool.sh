#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download the x86_64 Linux release of a tool, check its SHA-256 digest against the digest listed
# below for that version, install it, and add it to `GITHUB_PATH`. The WASI SDK goes to
# /opt/wasi-sdk, which `WASI_SDK_PATH` is set to in `GITHUB_ENV`, and the other tools go to
# `~/.<tool>/bin`. A version without a listed digest fails.
#
# Usage:
#   install-tool.sh <just|wasi-sdk|wasmtime|wasm-tools> <version>

set -euo pipefail

TOOL="$1"
VERSION="$2"
GH=https://github.com

case "$TOOL-$VERSION" in
    just-1.58.0)
        URL="$GH/casey/just/releases/download/$VERSION/just-$VERSION-x86_64-unknown-linux-musl.tar.gz"
        SHA256=4a5cc2f53e6f0f8c59092a6cc38291eb729d46a7dd95d3ae582008881b84931d
        ;;
    wasi-sdk-34)
        URL="$GH/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$VERSION/wasi-sdk-$VERSION.0-x86_64-linux.tar.gz"
        SHA256=b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4
        ;;
    wasmtime-49.0.0)
        URL="$GH/bytecodealliance/wasmtime/releases/download/v$VERSION/wasmtime-v$VERSION-x86_64-linux.tar.xz"
        SHA256=a956c279ac6e80109369a285db30fc46f68239fa1bbeb03ccfa8e913bbca85fc
        ;;
    wasm-tools-1.258.0)
        URL="$GH/bytecodealliance/wasm-tools/releases/download/v$VERSION/wasm-tools-$VERSION-x86_64-linux.tar.gz"
        SHA256=b52d14eb74a4852cc249369bd4480c2b2fdd876145f41db51ff52269ded240ce
        ;;
    *)
        echo "No SHA-256 digest for $TOOL $VERSION. Add it to $0." >&2
        exit 1
        ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
ARCHIVE="$TMP/$(basename "$URL")"
curl --fail --silent --show-error --location -o "$ARCHIVE" "$URL"
"$(dirname "$0")/../../../scripts/check-sha256.sh" "$ARCHIVE" "$SHA256"

case "$TOOL" in
    just)
        mkdir -p "$HOME/.just/bin"
        tar -xzf "$ARCHIVE" -C "$HOME/.just/bin" just
        echo "$HOME/.just/bin" >>"$GITHUB_PATH"
        ;;
    wasi-sdk)
        sudo rm -rf /opt/wasi-sdk
        sudo mkdir /opt/wasi-sdk
        sudo tar -xzf "$ARCHIVE" --no-same-owner --strip-components=1 -C /opt/wasi-sdk
        echo "WASI_SDK_PATH=/opt/wasi-sdk" >>"$GITHUB_ENV"
        ;;
    wasmtime)
        mkdir -p "$HOME/.wasmtime/bin"
        tar -xJf "$ARCHIVE" --strip-components=1 -C "$HOME/.wasmtime/bin" "wasmtime-v$VERSION-x86_64-linux/wasmtime"
        echo "$HOME/.wasmtime/bin" >>"$GITHUB_PATH"
        ;;
    wasm-tools)
        mkdir -p "$HOME/.wasm-tools/bin"
        tar -xzf "$ARCHIVE" --strip-components=1 -C "$HOME/.wasm-tools/bin" "wasm-tools-$VERSION-x86_64-linux/wasm-tools"
        echo "$HOME/.wasm-tools/bin" >>"$GITHUB_PATH"
        ;;
esac
