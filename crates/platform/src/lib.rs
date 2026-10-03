// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Platform abstraction layer for host capabilities that need different
//! implementations on native and WASI targets.
//!
//! Builtins call the backend-agnostic APIs here; each capability holds its
//! platform-agnostic parts in one module and its backends in `<capability>_native`
//! / `<capability>_wasm` modules, selected with `#[cfg(target_arch)]`. The capabilities
//! are HTTP ([`http`]), used by `fetch`, the monotonic clock ([`clock`]), the filesystem
//! ([`fs`]), the process ([`process`]), and TCP sockets and name lookup ([`net`]).
//! [`stream`] holds the WASIp3 stream pumps those build on.

pub mod clock;
pub mod fs;
pub mod http;
#[cfg(not(target_arch = "wasm32"))]
mod http_native;
#[cfg(target_arch = "wasm32")]
mod http_wasm;
pub mod net;
#[cfg(not(target_arch = "wasm32"))]
mod net_native;
#[cfg(target_arch = "wasm32")]
mod net_wasm;
pub mod process;
pub mod random;
#[cfg(target_arch = "wasm32")]
pub mod stream;

/// Run `future` to completion, blocking the current task.
///
/// Only `future` runs: tasks spawned with `spawn_local` before or during the call stay queued,
/// and the executor of the enclosing task runs them after this returns. So no JavaScript runs
/// while this blocks, unless `future` itself runs it.
///
/// Blocking holds the component instance's lock, which stalls every other task of the instance.
/// Callers must not block inside a synchronous export, where blocking traps.
#[cfg(target_arch = "wasm32")]
pub fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    wit_bindgen::block_on_detached(future)
}
