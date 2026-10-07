#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download the Node.js release that scripts/clone-node-tests.sh takes the tests from into
# deps/node-bin/, as the oracle for tests/node-diff/fuzz.mjs. The archive's SHA-256 digest is
# checked against the digest listed below for that release, from its SHASUMS256.txt.
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

ARCHIVE="node-$NODE_TAG-$PLATFORM.tar.gz"
case "$ARCHIVE" in
    node-v24.21.0-darwin-arm64.tar.gz) SHA256=bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057 ;;
    node-v24.21.0-darwin-x64.tar.gz) SHA256=1462cb3b3046b815cf8ea436d3da450ec1a9f11dac7e5a46b0ada5305d7e8097 ;;
    node-v24.21.0-linux-arm64.tar.gz) SHA256=724282c3b43aec998aa9527380465b45d229e021b58035f5f4f63095eabfe5d5 ;;
    node-v24.21.0-linux-x64.tar.gz) SHA256=6e1db87ef58b8819e5d5402eff1536491b18edd8eb7bee5ef7897876e88dc5ff ;;
    *)
        echo "No SHA-256 digest for $ARCHIVE. Add it from https://nodejs.org/dist/$NODE_TAG/SHASUMS256.txt" >&2
        exit 1
        ;;
esac

if [[ -x "$TARGET_DIR/bin/node" ]] && [[ "$("$TARGET_DIR/bin/node" --version)" == "$NODE_TAG" ]]; then
    echo "Node.js $NODE_TAG already at $TARGET_DIR"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $ARCHIVE..."
curl --fail --silent --show-error --location -o "$TMP/$ARCHIVE" "https://nodejs.org/dist/$NODE_TAG/$ARCHIVE"
"$SCRIPT_DIR/check-sha256.sh" "$TMP/$ARCHIVE" "$SHA256"
rm -rf "$TARGET_DIR"
mkdir -p "$TARGET_DIR"
tar -xzf "$TMP/$ARCHIVE" -C "$TARGET_DIR" --strip-components=1
echo "Node.js $("$TARGET_DIR/bin/node" --version) installed at $TARGET_DIR"
