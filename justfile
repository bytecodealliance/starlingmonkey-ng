# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Starling-NG justfile
#
# Usage:
#   just build           Build the project (debug mode)
#   just test            Run all Rust tests
#   just clone-wpt-tests Clone the WPT test suite
#   just wpt-setup       Add the WPT hosts entries to /etc/hosts
#   just wpt-test        Run all WPT tests
#   just wpt-test base64 Run WPT tests matching "base64"
#   just wpt-update      Run WPT tests and update expectations
#   just test-wizer      Snapshot the wasm component with wizer and serve it
#   just fmt             Format all code
#   just clippy          Run clippy lints
#   just check           Run fmt check + clippy + tests

# The wasm target the `-wasm` recipes build for. `WASM_TARGET=p3 just test-wasm` retargets them;
# `p2`, `p3` and full triples are all accepted. The pinned toolchain in `rust-toolchain.toml`
# ships no wasm32-wasip3 std, so that target builds through the toolchain `STARLING_NIGHTLY` names,
# `nightly` by default.
_wasm_target_arg := env_var_or_default("WASM_TARGET", "p2")
WASM_TARGET := if _wasm_target_arg == "p2" { "wasm32-wasip2" } else if _wasm_target_arg == "p3" { "wasm32-wasip3" } else { _wasm_target_arg }
NIGHTLY := env_var_or_default("STARLING_NIGHTLY", "nightly")
_wasm_cargo := if WASM_TARGET == "wasm32-wasip3" { "cargo +" + NIGHTLY } else { "cargo" }
_wasm_out := "${CARGO_TARGET_DIR:-" + justfile_directory() + "/target}/" + WASM_TARGET

# The wasm target the componentizer's runtime builds and suites use. `WASM_TARGET` retargets these
# too, but they default to wasm32-wasip3, the target a componentized guest links against. The
# recipes above keep their wasm32-wasip2 default.
_componentize_target_arg := env_var_or_default("WASM_TARGET", "p3")
COMPONENTIZE_TARGET := if _componentize_target_arg == "p2" { "wasm32-wasip2" } else if _componentize_target_arg == "p3" { "wasm32-wasip3" } else { _componentize_target_arg }
_componentize_out := "${CARGO_TARGET_DIR:-" + justfile_directory() + "/target}/" + COMPONENTIZE_TARGET

# Cargo for the `componentize` workspace, which is separate from the runtime's. Run from inside
# its directory: cargo reads `componentize/.cargo/config.toml`, which points the build directory
# at the repository's `target/`, only from there.
_componentize := "cd " + justfile_directory() + "/componentize && cargo"

# Build in debug mode.
build *TARGET:
    cargo build --features debugmozjs {{TARGET}}

# Build in release mode.
build-release *TARGET:
    cargo build --release {{TARGET}}

# Run all Rust tests.
test *TARGET:
    cargo test --features debugmozjs --workspace {{TARGET}}

# Run all Rust tests in the `test-release` profile: release optimizations without LTO.
test-release *TARGET:
    cargo test --profile test-release --workspace {{TARGET}}

# Run the starling shell with the given args.
run *ARGS:
    cargo run --features debugmozjs -- {{ARGS}}

# Clone the WPT test suite (shallow clone, ~200MB).
[group('wpt')]
clone-wpt-tests *ARGS:
    ./scripts/clone-wpt.sh {{ARGS}}

# Add the hosts entries the WPT server needs to /etc/hosts.
[group('wpt')]
wpt-setup *ARGS:
    cat deps/wpt-hosts | sudo tee -a /etc/hosts

# Run WPT tests, optionally filtering by pattern.
[group('wpt')]
wpt-test *PATTERN:
    @just build
    node tests/wpt-harness/run-wpt.mjs {{PATTERN}}

# Run WPT tests, optionally filtering by pattern.
[group('wpt')]
wpt-test-release *PATTERN:
    @just build-release
    node tests/wpt-harness/run-wpt.mjs --runtime=target/release/starlingmonkey {{PATTERN}}

# Run WPT tests with verbose output.
[group('wpt')]
wpt-test-verbose *PATTERN:
    @just build
    node tests/wpt-harness/run-wpt.mjs -vv {{PATTERN}}

# Run WPT tests and update expectation files.
[group('wpt')]
wpt-update *PATTERN:
    @just build
    node tests/wpt-harness/run-wpt.mjs --update-expectations {{PATTERN}}

# Run WPT tests with request restrictions disabled (the non-WPT default).
[group('wpt')]
wpt-test-permissive *PATTERN:
    @just build
    node tests/wpt-harness/run-wpt.mjs --permissive {{PATTERN}}

# Run permissive WPT tests and update `permissive_status` expectations.
[group('wpt')]
wpt-update-permissive *PATTERN:
    @just build
    node tests/wpt-harness/run-wpt.mjs --permissive --update-expectations {{PATTERN}}

# Format all code.
fmt:
    cargo fmt
    {{_componentize}} fmt

# Check formatting without modifying files. ARGS go to the runtime workspace's `cargo fmt`.
fmt-check *ARGS:
    cargo fmt --check {{ARGS}}
    {{_componentize}} fmt --check

# Run clippy lints. ARGS go to the runtime workspace's `cargo clippy`.
clippy *ARGS:
    cargo clippy --features debugmozjs --all-targets {{ARGS}}
    {{_componentize}} clippy --all-targets

# Run GC zeal stress tests.
# Defaults to quick tests on the `js` and `core-runtime` packages.
# See `./scripts/test-gc-zeal.sh` for usage info.
gc-zeal *ARGS:
    ./scripts/test-gc-zeal.sh {{ARGS}}

# Run crown static GC analysis.
check-gc:
    ./scripts/check-crown.sh --workspace --all --examples

# Type-check, lint and bundle the debugger's DAP adapter into `crates/debugger/adapter/dist`.
# Needs Node and npm.
build-debugger-adapter:
    cd crates/debugger/adapter && npm ci --silent && npm run --silent check && npm run --silent bundle

# Fail if the committed adapter bundle differs from a fresh build of its source.
check-debugger-adapter:
    just build-debugger-adapter
    git diff --exit-code -- crates/debugger/adapter/dist/adapter.js

# Run basic checks: formatting, clippy, tests.
check:
    just fmt-check --all
    just clippy --all
    just test --examples

# Run most checks: `check` + `check-gc` + `gc-zeal`.
check-all:
    just check-debugger-adapter
    just check
    just check-gc
    just gc-zeal

# Run basic checks: formatting, clippy, tests.
check-wasm:
    just fmt-check --all
    just clippy --all
    just test-wasm --examples

# Build for the wasm target (`WASM_TARGET`, default wasm32-wasip2).
build-wasm *TARGET:
    {{_wasm_cargo}} build --target {{WASM_TARGET}} --features debugmozjs {{TARGET}}

# Build for the wasm target in release mode.
build-wasm-release *TARGET:
    {{_wasm_cargo}} build --target {{WASM_TARGET}} --release {{TARGET}}

# Run all Rust tests. A run with no arguments also runs the wasm serve end-to-end suite;
# arguments (a filter, `--examples`) reach only the cargo tests and leave that suite out.
test-wasm *TARGET:
    {{_wasm_cargo}} test --target {{WASM_TARGET}} --features debugmozjs --workspace {{TARGET}}
    @{{ if TARGET == "" { "just test-serve-wasm" } else { "echo 'Skipped the wasm serve end-to-end suite; run it with: just test-serve-wasm'" } }}

# Run all Rust tests in the `test-release` profile: release optimizations without LTO. A run with
# no arguments also runs the wasm serve end-to-end suite against the full release build.
test-wasm-release *TARGET:
    {{_wasm_cargo}} test --target {{WASM_TARGET}} --profile test-release --workspace {{TARGET}}
    @{{ if TARGET == "" { "just test-serve-wasm-release" } else { "echo 'Skipped the wasm serve end-to-end suite; run it with: just test-serve-wasm-release'" } }}

# Snapshot the component with `wasmtime wizer` and serve the result. Needs wasmtime on PATH.
test-wizer *ARGS:
    ./scripts/test-wizer.sh {{ARGS}}

# Serve the component under `wasmtime serve` and assert the same observables the native serve
# suite does, plus the behaviors only a real `wasi:http` host exercises. Builds the component
# first, so the suite never tests a stale one. Needs wasmtime on PATH; without it the suite
# skips loudly.
test-serve-wasm *ARGS:
    @just build-wasm -p starlingmonkey
    STARLING_WASM_COMPONENT="{{_wasm_out}}/debug/starling.wasm" \
        cargo test -p serve-test-support --test serve_wasm_e2e {{ARGS}}

# `test-serve-wasm` against the release component. The harness itself stays a debug build.
test-serve-wasm-release *ARGS:
    @just build-wasm-release -p starlingmonkey
    STARLING_WASM_COMPONENT="{{_wasm_out}}/release/starling.wasm" \
        cargo test -p serve-test-support --test serve_wasm_e2e {{ARGS}}

# Run the wasm debugger tests: a DAP client debugs the p3 component under `wasmtime run` over
# stdio and TCP. Builds the component first. Needs wasmtime on PATH, and `WASI_SDK_PATH` naming a
# wasi-sdk that builds p3.
test-debugger-wasm *ARGS:
    @WASM_TARGET=p3 just build-wasm -p starlingmonkey
    STARLING_WASM_COMPONENT="${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/wasm32-wasip3/debug/starling.wasm" \
        cargo test --features debugmozjs -p starlingmonkey --test debugger_wasm {{ARGS}}

# Build the `starling-dap-relay` component for wasm32-wasip3 in release mode.
build-dap-relay:
    cargo +{{NIGHTLY}} build -p dap-relay --target wasm32-wasip3 --release

# Run the relay tests: a DAP client debugs a native debuggee through the relay component under
# `wasmtime run`. Builds the relay first. Needs wasmtime on PATH.
test-dap-relay *ARGS:
    @just build-dap-relay
    STARLING_DAP_RELAY="${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/wasm32-wasip3/release/dap_relay.wasm" \
        cargo test --features debugmozjs -p starlingmonkey --test debugger_relay {{ARGS}}

# Run WPT tests against the wasm binary.
[group('wpt')]
wpt-test-wasm *PATTERN:
    @just build-wasm
    node tests/wpt-harness/run-wpt.mjs --target=wasm --runtime="{{_wasm_out}}/debug/starling.wasm" {{PATTERN}}

# Run WPT tests against the wasm binary with verbose output.
[group('wpt')]
wpt-test-wasm-verbose *PATTERN:
    @just build-wasm
    node tests/wpt-harness/run-wpt.mjs --target=wasm --runtime="{{_wasm_out}}/debug/starling.wasm" -vv {{PATTERN}}

# Run WPT tests against the wasm binary and update expectations.
[group('wpt')]
wpt-update-wasm *PATTERN:
    @just build-wasm
    node tests/wpt-harness/run-wpt.mjs --target=wasm --runtime="{{_wasm_out}}/debug/starling.wasm" --update-expectations {{PATTERN}}

# Run WPT tests through a serve-mode runtime: each test runs inside a `fetch` handler rather than
# as a one-shot command, which is the shape a deployed server has. Same binary as the command-mode
# recipes above — the component exports `wasi:cli/run` and `wasi:http/handler` both.
#
# The native server runs each request in its own global (`--serve-isolated`) and one at a time, so
# tests can't collide through shared global state. The wasm server asks its host for the same
# property with `--max-instance-reuse-count 1`, which it has to: a WASIp3 host reuses an instance
# for many requests by default.
[group('wpt')]
wpt-test-serve *PATTERN:
    @just build
    node tests/wpt-harness/run-wpt.mjs --mode=serve {{PATTERN}}

# Run WPT tests against the wasm binary through a serve-mode runtime, pre-initialized with Wizer.
# This is the configuration a deployed server has: inside a request handler, against a snapshot
# whose engine and content script are already stood up. Drop `--wizen` to skip the snapshot step.
[group('wpt')]
wpt-test-wasm-serve *PATTERN:
    @just build-wasm
    node tests/wpt-harness/run-wpt.mjs --target=wasm --runtime="{{_wasm_out}}/debug/starling.wasm" --mode=serve --wizen {{PATTERN}}

# Run WPT across every configuration: both targets, each as a command and as a server, from one
# build per target.
[group('wpt')]
wpt-test-all *PATTERN:
    @just wpt-test {{PATTERN}}
    @just wpt-test-wasm {{PATTERN}}
    @just wpt-test-wasm-serve {{PATTERN}}
    @just wpt-test-serve {{PATTERN}}

# Build the statically linked runtime component (starling.wasm).
build-runtime:
    ./scripts/build-runtime.sh

# Build the static runtime, then install `starling-componentize` with it embedded. The runtime
# is built for wasm32-wasip3 regardless of `WASM_TARGET`, since that is the one it embeds.
install-componentize:
    WASM_TARGET=p3 ./scripts/build-runtime.sh
    STARLING_EMBED_RUNTIME="${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/wasm32-wasip3/release/starling.wasm" \
        cargo install --path {{justfile_directory()}}/componentize --locked

# The componentizer is built from this checkout, and embeds the runtime `just build-runtime` last
# built, or none if there is no such build. ARGS are passed through unchanged, and it runs in the
# directory `just` was invoked from, so relative paths in them resolve there. For example:
#
#   just componentize --serve componentize app.js -o app.wasm
#   just componentize -d wit -w app types -o app.d.ts
#
# Run `starling-componentize` with ARGS.
[no-cd]
[positional-arguments]
componentize *ARGS:
    @{{_componentize}} build -q --bin starling-componentize
    @"${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/debug/starling-componentize" "$@"

# The packages are the one resolving the binary, and this platform's, holding a release build that
# embeds the static runtime. `componentize/npm/package.mjs` documents the layout, and
# `.github/workflows/release.yml` builds and publishes every platform's packages. VERSION defaults
# to the workspace's version, in `Cargo.toml`. The packages' tarballs go to
# `target/npm/tarballs`.
#
# Assemble the npm packages of `starling-componentize` for this platform in `target/npm`.
npm-componentize VERSION="":
    WASM_TARGET=p3 ./scripts/build-runtime.sh
    cd componentize && STARLING_EMBED_RUNTIME="${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/wasm32-wasip3/release/starling.wasm" \
        cargo build --release --locked --bin starling-componentize
    rm -rf "${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/npm"
    node componentize/npm/package.mjs \
        --version "$(v='{{VERSION}}'; [ -n "$v" ] && echo "$v" || sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)" \
        --out "${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/npm" \
        "$(node -p 'process.platform + "-" + process.arch')=${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/release/starling-componentize"
    cd "${CARGO_TARGET_DIR:-{{justfile_directory()}}/target}/npm" && mkdir tarballs && \
        for dir in starling-componentize*; do (cd "$dir" && npm pack --silent --pack-destination ../tarballs); done

# End-to-end test of the static runtime (build, run scripts under wasmtime).
test-runtime:
    ./scripts/test-runtime.sh

# The componentization suites, one test target each in `componentize/tests/`. Each file's
# module doc describes what it covers. `componentize` is a workspace of its own (see its
# `Cargo.toml`), so its recipes run cargo from its directory rather than with `-p`.
#   - `integration`: the smoke test, one tiny world end to end.
#   - `sync_suite`: every WIT value type, resources, and sync imports.
#   - `async_export`: an `async` export driven through a per-call event loop.
#   - `parity_demo`: an `async` export mixing a timer with a `ReadableStream`,
#                      and two concurrent calls.
#   - `async_import`: an `async` export awaiting a suspending `async` import.
#   - `streams`: the Component Model `stream<T>` and `future<T>` bridge.
#   - `exported_resource`: a guest-owned resource used and dropped by the host.
#   - `cli_run`: a JS module exporting `run`, run via `wasi:cli/run.run()`.
#   - `serve`: a JS module registering a `fetch` listener, served via
#                      `wasi:http/handler.handle()`, and one implementing `handle` itself.
#   - `fetch_streams`: `fetch` with received and returned `stream<u8>` bodies.
#   - `resources`: imported and exported resources across interfaces.
#   - `world_resource`: a resource declared in the world itself.
#   - `surface`: the `--disable` flag and the component's imports.
#   - `values`: flags, keywords, payload-punning variants, and value checks.
#   - `wit_dir`: a WIT directory with `deps/` and two versions of one interface.
#   - `top_level`: relative imports and a top-level `await` during componentization.
#   - `wit_to_ts`: the `.d.ts` generator's snapshots, type-checked with `tsc`.
#
# The suites link every world against one of the two runtime builds, selected
# by STARLING_LINK_MODE: `static` (the default) reads the runtime from
# STARLING_RUNTIME (default target/wasm32-wasip3/release/starling.wasm);
# `dynamic` reads the dylib from STARLING_DYLIB (default
# target/dylib/wasm32-wasip2/libstarling_rt.so) and the wasi-sdk shared libraries from
# WASI_SDK_PATH, which must point at a wasi-sdk 33+ for the `noeh/` ones.
#
# Building the static runtime needs a wasi-sdk 34+, the first with a wasm32-wasip3 sysroot.
#
# ARGS is forwarded to test-componentize-only, and on to libtest, so a name
# filter runs just the matching suite. See that recipe.
#
# Build the static runtime, then run every componentization suite against it.
test-componentize *ARGS:
    just build-runtime
    just test-componentize-only {{ARGS}}

# Assumes a runtime build in the mode STARLING_LINK_MODE selects (see
# `test-componentize`). Use this to iterate on the tests fast: editing a test
# never rebuilds the runtime or SpiderMonkey. The tests error clearly if the
# runtime build is missing.
#
# `--tests` runs every integration-test target in the crate in one cargo
# invocation, so cargo builds them in parallel and checks the workspace once
# rather than once per suite. Each suite still componentizes its own world once
# via a `OnceCell` and shares it across that suite's tests.
#
# ARGS is forwarded to libtest, so a name filter runs just the matching suite.
# For example, `just test-componentize-only echo_stream_u8` only componentizes
# the `streams` world, and the other suites' caches never initialize.
#
# Each world's componentization is inherent and cannot be shared across distinct
# worlds, but the two cranelift passes over the component that dominate it are
# cached on disk (`shared::engine`), so only a run that rebuilt the runtime pays
# them again.
#
# Run the componentization suites against a prebuilt runtime (no runtime build).
test-componentize-only *ARGS:
    cd {{justfile_directory()}}/componentize && \
        STARLING_RUNTIME="${STARLING_RUNTIME:-{{_componentize_out}}/release/starling.wasm}" \
        cargo test --tests -- --nocapture {{ARGS}}

# Generate a TypeScript declaration (`.d.ts`) describing the guest JS module a
# WIT world expects: the named exports the guest implements and the modules it
# may import, with this runtime's WIT->JS value mapping. A world exporting
# `wasi:http/handler` is described with the runtime's `handle`, so it needs the
# runtime build STARLING_RUNTIME names, or the one the componentizer embeds.
#
# Usage:
#   just ts-bindings <wit-path> [<world>]   # print the .d.ts to stdout
#   just ts-bindings --cli                  # the built-in cli/run world
#   just ts-bindings --serve                # the built-in serve world
#
# Omitting <world> selects the WIT's default world. `-w` is passed only when a
# world is named, since an empty one would make clap read the `types` subcommand
# as its value.
#
# Examples:
#   just ts-bindings componentize/tests/wit_to_ts/primitives.wit primitives
ts-bindings WIT WORLD="":
    {{_componentize}} run -q --bin starling-componentize -- \
        {{ if WIT == "--cli" { "--cli" } else if WIT == "--serve" { "--serve" } else { "-d " + join(invocation_directory(), WIT) + if WORLD == "" { "" } else { " -w " + WORLD } } }} \
        types
