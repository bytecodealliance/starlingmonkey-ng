#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Check that the SHA-256 digest of a file is the expected one. Exits with status 1 and prints both
# digests if it isn't.
#
# Usage:
#   ./scripts/check-sha256.sh <file> <expected hex digest>

set -euo pipefail

FILE="$1"
EXPECTED="$2"

if command -v sha256sum >/dev/null; then
    ACTUAL="$(sha256sum "$FILE" | cut -d' ' -f1)"
else
    ACTUAL="$(shasum -a 256 "$FILE" | cut -d' ' -f1)"
fi

if [[ "$ACTUAL" != "$EXPECTED" ]]; then
    echo "SHA-256 mismatch for $FILE:" >&2
    echo "  expected $EXPECTED" >&2
    echo "  got      $ACTUAL" >&2
    exit 1
fi
