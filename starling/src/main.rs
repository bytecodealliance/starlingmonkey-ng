// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! StarlingMonkey — a JavaScript runtime built on SpiderMonkey.
//!
//! Parses command-line arguments into a [`RuntimeConfig`](libstarling::config::RuntimeConfig)
//! and delegates execution to [`libstarling::run`]. The wasm build is the `starling` cdylib in
//! `lib.rs`. On wasm targets this binary is built but does nothing.

#[cfg(not(target_arch = "wasm32"))]
use std::process::exit;

#[cfg(not(target_arch = "wasm32"))]
use libstarling::config::RuntimeConfig;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    let config = match RuntimeConfig::from_args(std::env::args()) {
        Ok(config) => config,
        // `--help` and `--version` print to stdout and exit with 0. A usage error prints to
        // stderr and exits with 2.
        Err(e) => e.exit(),
    };

    if let Err(e) = libstarling::run(config) {
        eprintln!("{e}");
        exit(1);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    unreachable!("On wasm targets the `starling` cdylib is the entry point, not this binary");
}
