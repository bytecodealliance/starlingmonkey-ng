// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Running futures on the wasm host's executor.
//!
//! [`spawn`] runs a future that belongs to the current export call, such as writing a response
//! body. [`spawn_background`] runs one that may need later export calls to complete, such as the
//! work a content script's top level left behind.
//!
//! On WASIp3, both run on wit-bindgen's executor as part of the current export call's task, which
//! keeps running them after the call has returned its result. On WASIp2, an export call runs its
//! work with `block_on`, which waits for every [`spawn`]ed future and leaves
//! [`spawn_background`]ed ones queued for later calls.

#[cfg(target_env = "p2")]
pub use crate::task_p2::{block_on, spawn, spawn_background, wait_for, WaitFor};

/// Run `future` to completion on a task of its own, as part of the current export call's task.
#[cfg(target_env = "p3")]
pub fn spawn(future: impl std::future::Future<Output = ()> + 'static) {
    wasip3::wit_bindgen::spawn_local(future);
}

/// Run `future` to completion on a task of its own. On WASIp3, this is [`spawn`].
#[cfg(target_env = "p3")]
pub fn spawn_background(future: impl std::future::Future<Output = ()> + 'static) {
    spawn(future);
}
