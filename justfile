# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

# The wasm target the `-wasm` recipes build for. `WASM_TARGET=p3 just test-wasm` retargets them;
# `p2`, `p3` and full triples are all accepted. The pinned toolchain in `rust-toolchain.toml`
# ships no wasm32-wasip3 std, so that target builds through the toolchain `STARLING_NIGHTLY` names,
# `nightly` by default.
_wasm_target_arg := env_var_or_default("WASM_TARGET", "p2")
WASM_TARGET := if _wasm_target_arg == "p2" { "wasm32-wasip2" } else if _wasm_target_arg == "p3" { "wasm32-wasip3" } else { _wasm_target_arg }
NIGHTLY := env_var_or_default("STARLING_NIGHTLY", "nightly")
_wasm_cargo := if WASM_TARGET == "wasm32-wasip3" { "cargo +" + NIGHTLY } else { "cargo" }

# The cargo build directory, as a shell expression.
_target := "${CARGO_TARGET_DIR:-" + justfile_directory() + "/target}"
_wasm_out := _target + "/" + WASM_TARGET

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
    cargo fmt --manifest-path crown/Cargo.toml

# Check formatting without modifying files.
fmt-check *ARGS:
    cargo fmt --check {{ARGS}}
    cargo fmt --manifest-path crown/Cargo.toml --check

# Run clippy lints, failing on any warning.
clippy *ARGS:
    cargo clippy --features debugmozjs --all-targets {{ARGS}} -- -D warnings

# Defaults to quick tests on the `js` and `core-runtime` packages. `scripts/test-gc-zeal.sh`
# documents the arguments.
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

# Run basic checks: formatting, clippy, tests.
check:
    just fmt-check --all
    just clippy --all
    just test --examples

# Run most checks: `check` + `check-gc` + `gc-zeal`.
check-all:
    just check
    just check-gc
    just gc-zeal

# Run basic checks: formatting, clippy, and the tests on the wasm target.
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
