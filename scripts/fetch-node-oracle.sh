#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download the Node.js release that scripts/clone-node-tests.sh takes the tests from into
# deps/node-bin/, as the oracle for tests/node-diff/fuzz.mjs.
#
# Usage:
#   ./scripts/fetch-node-oracle.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
NODE_TAG="$(sed -n 's/^NODE_TAG="\(.*\)"$/\1/p' "$SCRIPT_DIR/clone-node-tests.sh")"
TARGET_DIR="$ROOT_DIR/deps/node-bin"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) PLATFORM="darwin-arm64" ;;
    Darwin-x86_64) PLATFORM="darwin-x64" ;;
    Linux-aarch64) PLATFORM="linux-arm64" ;;
    Linux-x86_64) PLATFORM="linux-x64" ;;
    *) echo "Unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if [[ -x "$TARGET_DIR/bin/node" ]] && [[ "$("$TARGET_DIR/bin/node" --version)" == "$NODE_TAG" ]]; then
    echo "Node.js $NODE_TAG already at $TARGET_DIR"
    exit 0
fi

ARCHIVE="node-$NODE_TAG-$PLATFORM.tar.gz"
echo "Downloading $ARCHIVE..."
rm -rf "$TARGET_DIR"
mkdir -p "$TARGET_DIR"
curl --fail --silent --show-error --location "https://nodejs.org/dist/$NODE_TAG/$ARCHIVE" |
    tar -xz -C "$TARGET_DIR" --strip-components=1
echo "Node.js $("$TARGET_DIR/bin/node" --version) installed at $TARGET_DIR"
