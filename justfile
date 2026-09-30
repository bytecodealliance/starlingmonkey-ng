# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

# The wasm target the `-wasm` recipes build for. `WASM_TARGET=p3 just test-wasm` retargets them;
# `p2`, `p3` and full triples are all accepted. `scripts/wasm-target.sh` maps the names to triples
# and picks the toolchain.
WASM_TARGET := `. ./scripts/wasm-target.sh && wasm_target "${WASM_TARGET:-p2}"`
_wasm_cargo := `. ./scripts/wasm-target.sh && wasm_cargo "$(wasm_target "${WASM_TARGET:-p2}")"`

# The cargo build directory, as a shell expression.
_target := "${CARGO_TARGET_DIR:-" + justfile_directory() + "/target}"
_wasm_out := _target + "/" + WASM_TARGET

# The wasm target of the runtime `scripts/build-runtime.sh` builds, which the componentizer embeds
# and its suites link against. `RUNTIME_TARGET` retargets it, and it defaults to wasm32-wasip3.
RUNTIME_TARGET := `. ./scripts/wasm-target.sh && wasm_target "${RUNTIME_TARGET:-p3}"`
_runtime_out := _target + "/" + RUNTIME_TARGET

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

# Run WPT tests against the release build, optionally filtering by pattern.
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

# Crown is formatted with the runtime workspace's toolchain.
#
# Format all code.
fmt:
    cargo fmt
    {{_componentize}} fmt --all
    cargo fmt --manifest-path crown/Cargo.toml

# Check formatting without modifying files. ARGS go to the runtime workspace's `cargo fmt`.
fmt-check *ARGS:
    cargo fmt --check {{ARGS}}
    {{_componentize}} fmt --all --check
    cargo fmt --manifest-path crown/Cargo.toml --check

# Run clippy lints, failing on any warning. ARGS go to the runtime workspace's `cargo clippy`.
clippy *ARGS:
    cargo clippy --features debugmozjs --all-targets {{ARGS}} -- -D warnings
    {{_componentize}} clippy --workspace --all-targets -- -D warnings

# Run clippy lints for the wasm target (`WASM_TARGET`), failing on any warning. ARGS go to
# `cargo clippy`.
clippy-wasm *ARGS:
    {{_wasm_cargo}} clippy --target {{WASM_TARGET}} --features debugmozjs --workspace \
        --lib --bins --tests --examples {{ARGS}} -- -D warnings

# Without arguments, it runs the quick zeal modes over `js`, `core-runtime`, `wit-bridge` and
# the builtin crates. `scripts/test-gc-zeal.sh` documents the arguments.
#
# Run GC zeal stress tests.
gc-zeal *ARGS:
    ./scripts/test-gc-zeal.sh {{ARGS}}

# It checks every workspace library and example natively, then the runtime and its dependencies
# for wasm32-wasip2, which includes the code only wasm builds compile. Then it runs crown's own
# tests.
#
# Run crown static GC analysis.
check-gc:
    ./scripts/check-crown.sh --workspace --lib --examples
    ./scripts/check-crown.sh -p starlingmonkey --target wasm32-wasip2
    cd crown && RUSTC_BOOTSTRAP=1 cargo test

# Run basic checks: formatting, clippy, tests, and the componentizer's unit tests.
check:
    just fmt-check --all
    just clippy --all
    just test --examples
    just test-componentize-unit

# Run most checks: `check` + `check-gc` + `gc-zeal`.
check-all:
    just check
    just check-gc
    just gc-zeal

# Run basic checks: formatting, clippy, and the tests on the wasm target.
check-wasm:
    just fmt-check --all
    just clippy-wasm
    just test-wasm --examples

# Build for the wasm target (`WASM_TARGET`, default wasm32-wasip2).
build-wasm *TARGET:
    {{_wasm_cargo}} build --target {{WASM_TARGET}} --features debugmozjs {{TARGET}}

# Build for the wasm target in release mode.
build-wasm-release *TARGET:
    {{_wasm_cargo}} build --target {{WASM_TARGET}} --release {{TARGET}}

# A run with no arguments also runs the wasm serve end-to-end suite. Arguments (a filter,
# `--examples`) reach only the cargo tests and leave that suite out.
#
# Run all Rust tests on the wasm target.
test-wasm *TARGET:
    {{_wasm_cargo}} test --target {{WASM_TARGET}} --features debugmozjs --workspace {{TARGET}}
    @{{ if TARGET == "" { "just test-serve-wasm" } else { "echo 'Skipped the wasm serve end-to-end suite; run it with: just test-serve-wasm'" } }}

# A run with no arguments also runs the wasm serve end-to-end suite against the release build.
#
# Run all Rust tests on the wasm target in the `test-release` profile.
test-wasm-release *TARGET:
    {{_wasm_cargo}} test --target {{WASM_TARGET}} --profile test-release --workspace {{TARGET}}
    @{{ if TARGET == "" { "just test-serve-wasm-release" } else { "echo 'Skipped the wasm serve end-to-end suite; run it with: just test-serve-wasm-release'" } }}

# Snapshot the component with `wasmtime wizer` and serve the result. Needs wasmtime on PATH.
test-wizer *ARGS:
    ./scripts/test-wizer.sh {{ARGS}}

# The recipe builds the component first. The suite runs it with the wasmtime on PATH, and is
# skipped if there is none.
#
# Run the serve end-to-end suite against the component under `wasmtime serve`.
test-serve-wasm *ARGS:
    @just build-wasm -p starlingmonkey
    STARLING_WASM_COMPONENT="{{_wasm_out}}/debug/starling.wasm" \
        cargo test -p serve-test-support --test serve_wasm_e2e {{ARGS}}

# `test-serve-wasm` against the release component. The harness itself stays a debug build.
test-serve-wasm-release *ARGS:
    @just build-wasm-release -p starlingmonkey
    STARLING_WASM_COMPONENT="{{_wasm_out}}/release/starling.wasm" \
        cargo test -p serve-test-support --test serve_wasm_e2e {{ARGS}}

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

# Each test runs inside a `fetch` handler rather than as a one-shot command. The server runs each
# request in its own global (`--serve-isolated`) and one at a time, so tests can't collide through
# shared global state.
#
# Run WPT tests through a native serve-mode runtime.
[group('wpt')]
wpt-test-serve *PATTERN:
    @just build
    node tests/wpt-harness/run-wpt.mjs --mode=serve {{PATTERN}}

# Each test runs inside a `fetch` handler of a Wizer snapshot. The harness passes
# `--max-instance-reuse-count 1` to `wasmtime serve`, since a WASIp3 host reuses an instance for
# many requests by default. Drop `--wizen` to skip the snapshot step.
#
# Run WPT tests against the wasm binary through a serve-mode runtime, pre-initialized with Wizer.
[group('wpt')]
wpt-test-wasm-serve *PATTERN:
    @just build-wasm
    node tests/wpt-harness/run-wpt.mjs --target=wasm --runtime="{{_wasm_out}}/debug/starling.wasm" --mode=serve --wizen {{PATTERN}}

# Run WPT across every configuration: both targets, each as a command and as a server.
[group('wpt')]
wpt-test-all *PATTERN:
    @just wpt-test {{PATTERN}}
    @just wpt-test-wasm {{PATTERN}}
    @just wpt-test-wasm-serve {{PATTERN}}
    @just wpt-test-serve {{PATTERN}}

# Build the runtime component (starling.wasm) that releases ship, in release mode.
build-runtime:
    ./scripts/build-runtime.sh

# The componentize suites and `test-runtime` run against this build.
#
# Build the runtime component (starling.wasm) in debug mode, with `debugmozjs`.
build-runtime-debug:
    ./scripts/build-runtime.sh --debug

# Build the release runtime, then install `starling-componentize` with it embedded.
install-componentize:
    ./scripts/build-runtime.sh
    export STARLING_EMBED_RUNTIME="{{_runtime_out}}/release/starling.wasm" && \
        {{_componentize}} install --path . --locked

# The componentizer is built from this checkout, and embeds the wasm32-wasip3 runtime
# `just build-runtime` writes, or none if there is no such build. ARGS are passed through
# unchanged, and it runs in the directory `just` was invoked from, so relative paths in them
# resolve there. For example:
#
#   just componentize --export-http componentize app.js -o app.wasm
#   just componentize -w wit -n app types -o app.d.ts
#
# Run `starling-componentize` with ARGS.
[no-cd]
[positional-arguments]
componentize *ARGS:
    @{{_componentize}} build -q --bin starling-componentize
    @"{{_target}}/debug/starling-componentize" "$@"

# The packages are the one that resolves the binary and the one for this platform, which holds a
# release build with the release runtime embedded. `componentize/npm/package.mjs` documents the
# layout, and `.github/workflows/release.yml` builds and publishes every platform's packages.
# VERSION defaults to the workspace's version, in `Cargo.toml`. The packages' tarballs go to
# `target/npm/tarballs`.
#
# Assemble the npm packages of `starling-componentize` for this platform in `target/npm`.
npm-componentize VERSION="":
    ./scripts/build-runtime.sh
    export STARLING_EMBED_RUNTIME="{{_runtime_out}}/release/starling.wasm" && \
        {{_componentize}} build --release --locked --bin starling-componentize
    rm -rf "{{_target}}/npm"
    node componentize/npm/package.mjs {{ if VERSION == "" { "" } else { "--version " + VERSION } }} \
        --out "{{_target}}/npm" --pack "{{_target}}/npm/tarballs" \
        "$(node -p 'process.platform + "-" + process.arch')={{_target}}/release/starling-componentize"

# End-to-end test of the debug runtime component (build, run scripts under wasmtime).
test-runtime:
    ./scripts/test-runtime.sh

# Each suite is a test target in `componentize/tests/`, and each file's head comment describes
# what it covers. `componentize` is a workspace of its own (see its `Cargo.toml`), so its recipes
# run cargo from its directory rather than with `-p`. ARGS go to `test-componentize-only`.
#
# Build the debug runtime, then run every componentization suite against it.
test-componentize *ARGS:
    just build-runtime-debug
    just test-componentize-only {{ARGS}}

# The suites use the runtime build STARLING_RUNTIME names, by default the one
# `just build-runtime-debug` writes, and this recipe never rebuilds it, so editing a test rebuilds
# neither the runtime nor SpiderMonkey. A missing runtime build fails the suites with its path.
# ARGS go to libtest, so `just test-componentize-only echo_stream` runs only the matching tests
# and componentizes only the `streams` world. `scripts/test-componentize.sh` runs several suites
# at a time and describes how many.
#
# Run the componentization suites against a prebuilt runtime (no runtime build).
test-componentize-only *ARGS:
    export STARLING_RUNTIME="${STARLING_RUNTIME:-{{_runtime_out}}/debug/starling.wasm}" && \
        ./scripts/test-componentize.sh --nocapture {{ARGS}}

# Run the unit tests of the componentizer's workspace. They need no runtime build.
test-componentize-unit:
    {{_componentize}} test --workspace --lib

# For a world exporting `wasi:http/handler`, the interface's declarations come from the runtime
# build STARLING_RUNTIME names, or else from the one the componentizer embeds.
#
#   just ts-bindings <wit-path> [<world>]   # print the .d.ts to stdout
#   just ts-bindings --export-cli           # the built-in cli/run world
#   just ts-bindings --export-http          # the built-in serve world
#   just ts-bindings componentize/tests/wit_to_ts/primitives.wit primitives
#
# Omitting <world> selects the only world of the last package the WIT path loads.
#
# Print the TypeScript declaration (`.d.ts`) of the guest JS module a WIT world expects.
[no-cd]
ts-bindings WIT WORLD="":
    @just --justfile "{{justfile()}}" componentize \
        {{ if WIT == "--export-cli" { "--export-cli" } else if WIT == "--export-http" { "--export-http" } else { "-w '" + WIT + "'" + if WORLD == "" { "" } else { " -n '" + WORLD + "'" } } }} \
        types
