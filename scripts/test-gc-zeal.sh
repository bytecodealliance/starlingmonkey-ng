#!/usr/bin/env bash
#
# Run tests under various SpiderMonkey GC zeal configurations.
#
# SpiderMonkey's GC zeal is controlled by the JS_GC_ZEAL environment variable:
#   JS_GC_ZEAL="mode,frequency" or "mode1;mode2,frequency"
#
# Usage:
#   ./scripts/test-gc-zeal.sh                             # Defaults: quick zeal, js and core-runtime tests
#   ./scripts/test-gc-zeal.sh quick -p js -p core-runtime # Same as above
#   ./scripts/test-gc-zeal.sh full                        # Full set of zeal modes, quite slow indeed
#   ./scripts/test-gc-zeal.sh "2,1"                       # Specific zeal setting
#   ./scripts/test-gc-zeal.sh full --workspace --examples # All the modes, all the tests, none of the quickness
set -euo pipefail

# Zeal configurations, ordered from fastest to slowest.
QUICK_CONFIGS=(
    "1,1"          # GC on roots change
    "4,1"          # Verify pre-barriers
    "15,1"         # Check heap after GC
    "18,1"         # Check gray marking
)

FULL_CONFIGS=(
    "${QUICK_CONFIGS[@]}"
    "2,1"          # GC on every allocation
    "5,1"          # Verify post-barriers
    "7,1"          # Generational GC stress
    "10,1"         # Incremental GC (multiple slices)
    "14,1"         # Compact on every GC
    "2;14,1"       # Allocate + compact (catches untraced Heap pointers)
    "1;2;4;5,1"    # Combined: roots + alloc + barriers
    "1;2;4;5;7;10;14,1"  # All productive modes at once
)

# --- Parse arguments ---
# Extract the zeal mode from the argument list; everything else is forwarded
# to `cargo test`. The mode is recognized as `--zeal <mode>`, as a bare
# `quick`/`full`, or as a bare JS_GC_ZEAL string such as "2,1" or "1;2;4,1".
MODE="quick"
CARGO_ARGS=()

# A zeal string is `mode[;mode...],frequency`. Cargo test filters and flags
# never look like this, so matching it as a bare argument is unambiguous.
ZEAL_RE='^[0-9]+(;[0-9]+)*,[0-9]+$'

while [[ $# -gt 0 ]]; do
    case "$1" in
        --zeal)
            MODE="${2:?--zeal requires a value (quick, full, or a JS_GC_ZEAL string)}"
            shift 2
            ;;
        quick|full)
            MODE="$1"
            shift
            ;;
        *)
            if [[ "$1" =~ $ZEAL_RE ]]; then
                MODE="$1"
            else
                CARGO_ARGS+=("$1")
            fi
            shift
            ;;
    esac
done

# Select the js and core-runtime packages unless the caller selected packages
# themselves.
PACKAGE_SELECTED=false
for arg in ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"}; do
    case "$arg" in
        -p*|--package|--package=*|--workspace|--all)
            PACKAGE_SELECTED=true
            break
            ;;
    esac
done

if [[ "$PACKAGE_SELECTED" == false ]]; then
    CARGO_ARGS=(-p js -p core-runtime ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"})
fi

# Ensure debugmozjs is enabled for GC zeal testing
CARGO_ARGS=("${CARGO_ARGS[@]}" --features debugmozjs)

PASSED=0
FAILED=0
FAILURES=()

if [[ "$MODE" == "quick" ]]; then
    CONFIGS=("${QUICK_CONFIGS[@]}")
elif [[ "$MODE" == "full" ]]; then
    CONFIGS=("${FULL_CONFIGS[@]}")
else
    # User-provided zeal setting
    CONFIGS=("$MODE")
fi

echo "Running tests with ${#CONFIGS[@]} GC zeal configuration(s)..."
echo "  cargo test ${CARGO_ARGS[*]}"
echo ""

for zeal in "${CONFIGS[@]}"; do
    if JS_GC_ZEAL="$zeal" cargo test "${CARGO_ARGS[@]}"; then
        PASSED=$((PASSED + 1))
    else
        FAILED=$((FAILED + 1))
        FAILURES+=("$zeal")
    fi
done

echo ""
echo "Results: $PASSED passed, $FAILED failed out of ${#CONFIGS[@]} configurations"

if [[ $FAILED -gt 0 ]]; then
    echo ""
    echo "Failed configurations:"
    for z in "${FAILURES[@]}"; do
        echo "  JS_GC_ZEAL=\"$z\""
    done
    echo ""
    echo "To debug a failure, run:"
    echo "  JS_GC_ZEAL=\"${FAILURES[0]}\" cargo test ${CARGO_ARGS[*]} -- --nocapture"
    exit 1
fi
