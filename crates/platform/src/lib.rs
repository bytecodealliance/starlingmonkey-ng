// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Platform abstraction layer for host capabilities that need different
//! implementations natively, on WASIp2 and on WASIp3.
//!
//! Builtins call the backend-agnostic APIs here. Each capability holds its
//! platform-agnostic parts in one module and its backends in `<capability>_native`,
//! `<capability>_p2` and `<capability>_p3` modules, selected with `#[cfg]` on
//! `target_arch` and `target_env`. The capabilities are HTTP ([`http`]), used by `fetch`,
//! the monotonic clock ([`clock`]), and on wasm, running futures on the host's executor
//! ([`task`]).

pub mod clock;
pub mod http;
#[cfg(not(target_arch = "wasm32"))]
mod http_native;
#[cfg(target_env = "p2")]
mod http_p2;
#[cfg(target_env = "p3")]
mod http_p3;
#[cfg(target_arch = "wasm32")]
mod http_wasm;
#[cfg(target_arch = "wasm32")]
pub mod task;
#[cfg(target_env = "p2")]
mod task_p2;
