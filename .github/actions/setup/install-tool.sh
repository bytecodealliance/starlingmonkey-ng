#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download the x86_64 Linux release of a tool at the version `.github/tool-versions.env` names,
# check its SHA-256 digest against the digest listed below for that version, install it, and add
# it to `GITHUB_PATH`. The WASI SDK goes to /opt/wasi-sdk, which `WASI_SDK_PATH` is set to in
# `GITHUB_ENV`, and the other tools go to `~/.<tool>/bin`. A version without a listed digest
# fails.
#
# Usage:
#   install-tool.sh <just|wasi-sdk|wasmtime|wasm-tools>

set -euo pipefail

TOOL="$1"
# shellcheck source=../../tool-versions.env
. "$(dirname "$0")/../../tool-versions.env"
case "$TOOL" in
    just) VERSION="$JUST_VERSION" ;;
    wasi-sdk) VERSION="$WASI_SDK_VERSION" ;;
    wasmtime) VERSION="$WASMTIME_VERSION" ;;
    wasm-tools) VERSION="$WASM_TOOLS_VERSION" ;;
    *) echo "unknown tool $TOOL" >&2; exit 1 ;;
esac
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
    wasmtime-50.0.0)
        URL="$GH/bytecodealliance/wasmtime/releases/download/v$VERSION/wasmtime-v$VERSION-x86_64-linux.tar.xz"
        SHA256=77c19ee0437991f9ac7837b9c269d9c7076744f85dad263fea5214cc2723c7ff
        ;;
    wasm-tools-1.262.0)
        URL="$GH/bytecodealliance/wasm-tools/releases/download/v$VERSION/wasm-tools-$VERSION-x86_64-linux.tar.gz"
        SHA256=655b8df67d54c3ef9b52fc237c4e94bbfbb5f5f43825245cb0940a7002a9ed65
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
ACTUAL="$(sha256sum "$ARCHIVE" | cut -d' ' -f1)"
if [[ "$ACTUAL" != "$SHA256" ]]; then
    echo "SHA-256 mismatch for $URL:" >&2
    echo "  expected $SHA256" >&2
    echo "  got      $ACTUAL" >&2
    exit 1
fi

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
