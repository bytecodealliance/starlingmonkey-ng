// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The integration tests that also run on wasm targets. They share one test binary, so a wasm test
//! run compiles the engine once for all of them.

mod event_loop;
mod js_api;
mod snapshot_idle;
