#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download Spin into deps/spin-bin/, for running the Spin application that
# scripts/build-spin-app.sh builds from tests/express-apps.
#
# Usage:
#   ./scripts/fetch-spin.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SPIN_VERSION="v4.2.1"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) SPIN_PLATFORM="macos-aarch64" ;;
    Darwin-x86_64) SPIN_PLATFORM="macos-amd64" ;;
    Linux-aarch64) SPIN_PLATFORM="linux-aarch64" ;;
    Linux-x86_64) SPIN_PLATFORM="linux-amd64" ;;
    *) echo "Unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ "$("$ROOT_DIR/deps/spin-bin/spin" --version 2>/dev/null)" != "spin ${SPIN_VERSION#v} "* ]]; then
    echo "Downloading Spin $SPIN_VERSION..."
    curl --fail --silent --show-error --location -o "$TMP/spin.tar.gz" \
        "https://github.com/spinframework/spin/releases/download/$SPIN_VERSION/spin-$SPIN_VERSION-$SPIN_PLATFORM.tar.gz"
    rm -rf "$ROOT_DIR/deps/spin-bin"
    mkdir -p "$ROOT_DIR/deps/spin-bin"
    tar -xzf "$TMP/spin.tar.gz" -C "$ROOT_DIR/deps/spin-bin" spin
fi
echo "$("$ROOT_DIR/deps/spin-bin/spin" --version) at deps/spin-bin"
