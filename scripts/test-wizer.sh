#!/usr/bin/env bash
#
# Test the Wizer pre-initialization path end to end.
#
# The component exports `wizer-initialize`, which `wasmtime wizer` runs before snapshotting the
# instance's state. That path has no unit-test seam: it only exists as a component export, run by
# an external tool, and what it produces is only meaningfully checked by serving it. So this
# builds the component, snapshots it, and serves the snapshot.
#
# Usage:
#   ./scripts/test-wizer.sh            # build the component, then run every case
#   ./scripts/test-wizer.sh --no-build # reuse the component already in target/
#
# `WASM_TARGET` selects the target, as `p2`, `p3` or a full triple. It defaults to `p2`.
#
# Requires `wasmtime` on PATH (for both the `wizer` subcommand and `serve`).

set -euo pipefail

cd "$(dirname "$0")/.."

case "${WASM_TARGET:-p2}" in
    p2) TARGET=wasm32-wasip2 ;;
    p3) TARGET=wasm32-wasip3 ;;
    *) TARGET="${WASM_TARGET}" ;;
esac
# The pinned toolchain ships no wasm32-wasip3 std.
if [ "$TARGET" = wasm32-wasip3 ]; then
    CARGO=(cargo "+${STARLING_NIGHTLY:-nightly}")
else
    CARGO=(cargo)
fi
COMPONENT="${CARGO_TARGET_DIR:-target}/$TARGET/debug/starling.wasm"
WASI_FLAGS=(-Scli=y,inherit-env=y,http=y)
if [ "$TARGET" = wasm32-wasip3 ]; then
    WASI_FLAGS+=(-Sp3=y -Wcomponent-model-async=y)
fi

if [[ "${1:-}" != "--no-build" ]]; then
    "${CARGO[@]}" build --target "$TARGET" --features debugmozjs -p starlingmonkey
fi

if ! command -v wasmtime >/dev/null; then
    echo "wasmtime is not on PATH" >&2
    exit 1
fi
if [[ ! -f "$COMPONENT" ]]; then
    echo "no component at $COMPONENT; run without --no-build" >&2
    exit 1
fi
COMPONENT="$(cd "$(dirname "$COMPONENT")" && pwd)/$(basename "$COMPONENT")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

failures=0
check() {
    local name="$1" expected="$2" actual="$3"
    if [[ "$actual" == *"$expected"* ]]; then
        echo "ok   - $name"
    else
        echo "FAIL - $name"
        echo "         expected to contain: $expected"
        echo "         got:                 $actual"
        failures=$((failures + 1))
    fi
}

# Snapshot $2 (a config for STARLINGMONKEY_CONFIG) into $1. Prints wizer's own output; returns
# wizer's exit status.
#
# `--keep-init-func` because dropping the init export — wizer's default — leaves the component
# referring to a core export that is no longer there ("core instance N has no export named
# `wizer-initialize`"), and the snapshot then fails to load at all.
snapshot() {
    local out="$1" config="$2"
    wasmtime wizer --keep-init-func=true -o "$out" --dir=.::/cwd --dir=. "${WASI_FLAGS[@]}" \
        --env STARLINGMONKEY_CONFIG="$config" "$COMPONENT" 2>&1
}

# Serve $1 on a free-ish port and GET /; prints the response body, or a marker if it never came.
serve_and_get() {
    local component="$1" config="$2" port="$3" path="${4:-/}"
    STARLINGMONKEY_CONFIG="$config" wasmtime serve --dir=.::/cwd --dir=. "${WASI_FLAGS[@]}" \
        --addr "127.0.0.1:$port" "$component" >"serve-$port.log" 2>&1 &
    local server=$!
    local body="<no response>"
    for _ in $(seq 1 100); do
        if body="$(curl -sf --max-time 5 "http://127.0.0.1:$port$path" 2>/dev/null)"; then
            break
        fi
        body="<no response>"
        sleep 0.2
    done
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
    printf '%s' "$body"
}

echo "# wizer snapshots"

# A snapshot has to produce a component that still loads. It did not always: dropping the init
# export leaves a dangling reference, and the failure only shows when something tries to run it.
cat >handler.js <<'JS'
addEventListener('fetch', (event) => event.respondWith(new Response('from-snapshot')));
JS
snapshot snap-handler.wasm "--legacy-script handler.js" >/dev/null
check "a snapshot serves the handler it was built with" \
    "from-snapshot" \
    "$(serve_and_get snap-handler.wasm "--legacy-script handler.js" 18351)"

# The startup gate, at the point where it keeps a broken build from being deployed at all.
cat >no-listener.js <<'JS'
globalThis.ready = true;
JS
gate_output="$(snapshot snap-no-listener.wasm "--legacy-script no-listener.js" || true)"
check "a script with no fetch listener fails the snapshot" \
    "no \`fetch\` listener added during evaluation" \
    "$gate_output"

# A top-level await runs to completion while the snapshot is being taken, so the handler it
# registers afterwards is in the snapshot like any other.
cat >awaited.mjs <<'JS'
await new Promise((resolve) => setTimeout(resolve, 50));
addEventListener('fetch', (event) => event.respondWith(new Response('after-await')));
JS
snapshot snap-awaited.wasm "awaited.mjs" >/dev/null
check "a top-level await is wizened rather than refused" \
    "after-await" \
    "$(serve_and_get snap-awaited.wasm "awaited.mjs" 18352)"

# Evaluation having finished by snapshot time is what lets the gate judge this one at all: before,
# a module that awaited first had registered nothing *yet* and had to be let through.
cat >awaited-no-listener.mjs <<'JS'
await new Promise((resolve) => setTimeout(resolve, 50));
globalThis.ready = true;
JS
gate_output="$(snapshot snap-awaited-no-listener.wasm "awaited-no-listener.mjs" || true)"
check "a listener-less script is refused even when it awaits first" \
    "no \`fetch\` listener added during evaluation" \
    "$gate_output"

# A snapshot holds memory, not a running event loop, so work the top level leaves behind once it
# has finished evaluating is refused, with the stack that created it.
cat >leftover.mjs <<'JS'
globalThis.ticks = 0;
setInterval(() => { globalThis.ticks++; }, 10);
await new Promise((resolve) => setTimeout(resolve, 50));
addEventListener('fetch', (event) => event.respondWith(new Response('ok')));
JS
leftover_output="$(snapshot snap-leftover.wasm "leftover.mjs" || true)"
check "background work left by the top level fails the snapshot" \
    "a pending \`interval\` timer, created at:" \
    "$leftover_output"
check "the refusal names where the work was created" \
    "leftover.mjs:2:" \
    "$leftover_output"

# The snapshot holds no host handles: libc's stdio streams, opened by a top-level write, are closed
# before it is taken, and the resumed instance opens them again on its first write.
cat >stdio.mjs <<'JS'
console.log('written under Wizer');
addEventListener('fetch', (event) => {
  console.log('written by the handler');
  event.respondWith(new Response('logged'));
});
JS
snapshot snap-stdio.wasm "stdio.mjs" >/dev/null
check "a snapshot taken after a top-level write can write again" \
    "logged" \
    "$(serve_and_get snap-stdio.wasm "stdio.mjs" 18354)"

# Host I/O still running when evaluation ends is a handle belonging to the snapshotting process,
# and the snapshot carries memory rather than handles — so it is refused rather than resumed into a
# dangling reference. The upstream is this same component, answering with a promise that never
# settles, which leaves the fetch genuinely in flight instead of failing fast.
cat >hang.js <<'JS'
addEventListener('fetch', (event) => event.respondWith(new Promise(() => {})));
JS
STARLINGMONKEY_CONFIG="--legacy-script hang.js" wasmtime serve --dir=.::/cwd --dir=. \
    "${WASI_FLAGS[@]}" --addr "127.0.0.1:18356" "$COMPONENT" >hang.log 2>&1 &
hang_server=$!
# Listening but never answering is exit code 28 (timeout); 7 means nothing is there yet.
hang_ready=0
for _ in $(seq 1 100); do
    curl -s --max-time 1 "http://127.0.0.1:18356/" >/dev/null 2>&1 && break
    if [[ $? -eq 28 ]]; then hang_ready=1; break; fi
    sleep 0.2
done
if (( hang_ready )); then
    cat >inflight.mjs <<'JS'
globalThis.pending = fetch('http://127.0.0.1:18356/');
await new Promise((resolve) => setTimeout(resolve, 100));
addEventListener('fetch', (event) => event.respondWith(new Response('ok')));
JS
    check "host I/O still in flight fails the snapshot" \
        "a host operation in flight, created at:" \
        "$(snapshot snap-inflight.wasm "inflight.mjs" || true)"
else
    echo "SKIP - host I/O still in flight fails the snapshot (no hanging upstream)"
fi
kill "$hang_server" 2>/dev/null || true
wait "$hang_server" 2>/dev/null || true

# A resumed instance starts from state captured in another process, so anything anchored to that
# process has to be re-established (see `register_resume_fixup`). The monotonic clock is the one
# with a visible symptom: `performance.now()` reading from the snapshot's time origin.
cat >clock.js <<'JS'
const atStartup = performance.now();
addEventListener('fetch', (event) => {
  const now = performance.now();
  const sane = (t) => (t >= 0 && t < 60000 ? 'sane' : 'stale:' + t);
  event.respondWith(new Response('startup=' + sane(atStartup) + ' now=' + sane(now)));
});
JS
snapshot snap-clock.wasm "--legacy-script clock.js" >/dev/null
check "a resumed instance's performance.now() is rebased" \
    "startup=sane now=sane" \
    "$(serve_and_get snap-clock.wasm "--legacy-script clock.js" 18353)"

# Post-`await` startup code is startup code: it runs in the snapshotting process, so it reads that
# process's clock, and the resumed instance's own reading is rebased. Both are small numbers —
# what must not happen is either of them carrying the other process's origin.
cat >clock-awaited.mjs <<'JS'
await new Promise((resolve) => setTimeout(resolve, 50));
const afterAwait = performance.now();
addEventListener('fetch', (event) => {
  const now = performance.now();
  const sane = (t) => (t >= 0 && t < 60000 ? 'sane' : 'stale:' + t);
  event.respondWith(new Response('afterAwait=' + sane(afterAwait) + ' now=' + sane(now)));
});
JS
snapshot snap-clock-awaited.wasm "clock-awaited.mjs" >/dev/null
check "post-await startup code reads a sane clock on both sides of a snapshot" \
    "afterAwait=sane now=sane" \
    "$(serve_and_get snap-clock-awaited.wasm "clock-awaited.mjs" 18355)"

echo "# CLI tools"

# A snapshot of a main module that exports `run` and registers no `fetch` listener is a CLI tool:
# `wasi:cli/run` calls `run` in the resumed instance, whose top level ran once, under Wizer.
cat >tool.mjs <<'JS'
globalThis.evaluations = (globalThis.evaluations ?? 0) + 1;
await new Promise((resolve) => setTimeout(resolve, 10));
export async function run() {
  await new Promise((resolve) => setTimeout(resolve, 10));
  console.log('run called after ' + globalThis.evaluations + ' evaluation(s)');
}
JS
snapshot snap-tool.wasm "tool.mjs" >/dev/null
check "a snapshot's wasi:cli/run calls the main module's run export" \
    "run called after 1 evaluation(s)" \
    "$(wasmtime run --dir=. "${WASI_FLAGS[@]}" snap-tool.wasm 2>&1)"

# The same holds for an `-e` module, which has no file.
snapshot snap-eval-tool.wasm "-e 'export function run() { console.log(\"eval run called\"); }'" >/dev/null
check "a snapshot of an -e module calls its run export" \
    "eval run called" \
    "$(wasmtime run --dir=. "${WASI_FLAGS[@]}" snap-eval-tool.wasm 2>&1)"

# Running the same module as a plain script evaluates its top level and calls no export.
plain_output="$(wasmtime run --dir=. "${WASI_FLAGS[@]}" "$COMPONENT" tool.mjs 2>&1)"
if [[ "$plain_output" == *"run called"* ]]; then
    echo "FAIL - a plain script's run export is not called"
    echo "         got: $plain_output"
    failures=$((failures + 1))
else
    echo "ok   - a plain script's run export is not called"
fi

echo
if (( failures > 0 )); then
    echo "$failures failure(s)"
    exit 1
fi
echo "all wizer tests passed"
