// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Platform abstraction layer for host capabilities that need different
//! implementations on native and WASI targets.
//!
//! Builtins call the backend-agnostic APIs here; each capability holds its
//! platform-agnostic parts in one module and its backends in `<capability>_native`
//! / `<capability>_wasm` modules, selected with `#[cfg(target_arch)]`. The capabilities
//! are HTTP ([`http`]), used by `fetch`, and the monotonic clock ([`clock`]).

pub mod clock;
pub mod http;
#[cfg(not(target_arch = "wasm32"))]
mod http_native;
#[cfg(target_arch = "wasm32")]
mod http_wasm;
