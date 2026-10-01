// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Script debugging over the Debug Adapter Protocol.
//!
//! [`register`] installs the runtime hooks. With `--debug` set, the session starts before the
//! content script, and the content script waits until the DAP client sends `configurationDone`.
//! [`finish`] ends the session before the process exits. In an instance resumed from a snapshot,
//! `start_after_resume` starts the session from `STARLING_DEBUG` instead.

pub mod transport;

mod connection;
mod host;
#[cfg(not(target_arch = "wasm32"))]
mod native;
mod session;
#[cfg(target_arch = "wasm32")]
mod wasm;

pub use session::{
    allow_debugging_in_snapshot, before_request, finish, register, register_for_serve,
    serve_while_running, RunningLoop,
};
#[cfg(target_arch = "wasm32")]
pub use session::{before_export, start_after_resume};
