#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Download Spin into deps/spin-bin/, for running the Spin application that
# scripts/build-spin-app.sh builds from tests/express-apps. The archive's SHA-256 digest is checked
# against the digest listed below, from the release's checksums file.
#
# Usage:
#   ./scripts/fetch-spin.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SPIN_VERSION="v4.2.1"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) SPIN_PLATFORM="macos-aarch64" SHA256=e3d1358039f8397efc0aaf0839be66d3ad856e4ba1cae2dc4068e6700bd7bb3a ;;
    Darwin-x86_64) SPIN_PLATFORM="macos-amd64" SHA256=82d7f7af4f5f51e03d96aabea66855816b28030870dea330b257d6c07433b8d6 ;;
    Linux-aarch64) SPIN_PLATFORM="linux-aarch64" SHA256=199f55510c534cd71aaf589dcd010f19162614696830ddbdb938739c39d71b39 ;;
    Linux-x86_64) SPIN_PLATFORM="linux-amd64" SHA256=60cb9f78312acc577a1b839740dc932f2647f438461734bb96a1e374bcf6a3d1 ;;
    *) echo "Unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ "$("$ROOT_DIR/deps/spin-bin/spin" --version 2>/dev/null)" != "spin ${SPIN_VERSION#v} "* ]]; then
    echo "Downloading Spin $SPIN_VERSION..."
    curl --fail --silent --show-error --location -o "$TMP/spin.tar.gz" \
        "https://github.com/spinframework/spin/releases/download/$SPIN_VERSION/spin-$SPIN_VERSION-$SPIN_PLATFORM.tar.gz"
    "$SCRIPT_DIR/check-sha256.sh" "$TMP/spin.tar.gz" "$SHA256"
    rm -rf "$ROOT_DIR/deps/spin-bin"
    mkdir -p "$ROOT_DIR/deps/spin-bin"
    tar -xzf "$TMP/spin.tar.gz" -C "$ROOT_DIR/deps/spin-bin" spin
fi
echo "$("$ROOT_DIR/deps/spin-bin/spin" --version) at deps/spin-bin"
