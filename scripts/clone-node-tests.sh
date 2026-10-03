#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Clone Node.js's own tests (test/common, test/fixtures and test/parallel) at a specific tag into
# deps/node/, for tests/node-harness/run-node-tests.mjs. The clone must be at the commit
# NODE_COMMIT pins the tag to, or it is removed and the script fails.
#
# Usage:
#   ./scripts/clone-node-tests.sh [--force]
#
# Uses a shallow, sparse clone, which downloads about 80MB instead of the whole repository.

set -euo pipefail

NODE_TAG="v24.21.0"
NODE_COMMIT="955266bfdd854cd280dffd47548673914484e4c0"
NODE_REPO="https://github.com/nodejs/node.git"
NODE_DIR="deps/node"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="$ROOT_DIR/$NODE_DIR"

if [[ -d "$TARGET_DIR" ]] && [[ "${1:-}" != "--force" ]]; then
    CLONED_TAG="$(git -C "$TARGET_DIR" describe --tags 2>/dev/null || true)"
    CLONED_COMMIT="$(git -C "$TARGET_DIR" rev-parse HEAD 2>/dev/null || true)"
    if [[ "$CLONED_COMMIT" == "$NODE_COMMIT" ]] && [[ -d "$TARGET_DIR/test/parallel" ]]; then
        echo "Node.js tests already cloned at $TARGET_DIR"
        echo "Use --force to re-clone."
        exit 0
    fi
    echo "The clone at $TARGET_DIR is at ${CLONED_TAG:-an unknown revision}, or incomplete; re-cloning $NODE_TAG..."
    rm -rf "$TARGET_DIR"
fi

if [[ "${1:-}" == "--force" ]] && [[ -d "$TARGET_DIR" ]]; then
    echo "Removing existing clone..."
    rm -rf "$TARGET_DIR"
fi

echo "Cloning Node.js tests (shallow and sparse, tag: $NODE_TAG)..."
mkdir -p "$(dirname "$TARGET_DIR")"
git clone --quiet --depth 1 --filter=blob:none --sparse --branch "$NODE_TAG" \
    -c advice.detachedHead=false "$NODE_REPO" "$TARGET_DIR"
CLONED_COMMIT="$(git -C "$TARGET_DIR" rev-parse HEAD)"
if [[ "$CLONED_COMMIT" != "$NODE_COMMIT" ]]; then
    echo "The tag $NODE_TAG is at $CLONED_COMMIT, not at the pinned $NODE_COMMIT; removing the clone." >&2
    rm -rf "$TARGET_DIR"
    exit 1
fi
git -C "$TARGET_DIR" sparse-checkout set test/common test/fixtures test/parallel

echo "Node.js tests cloned to $TARGET_DIR"
echo "Disk usage: $(du -sh "$TARGET_DIR" | cut -f1)"
