// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TCP sockets and host name lookup, over tokio natively ([`net_native`](crate::net_native)) and
//! over `wasi:sockets` on WASIp3 ([`net_wasm`](crate::net_wasm)), whose items are re-exported
//! here.
//!
//! Both backends provide `resolve`, `connect_stream`, `listen_stream`, `Connection` with its
//! `ConnectionReader` and `ConnectionWriter`, `StreamListener`, and `NetError` with its Node.js
//! error `code`. The WASIp3 backend also provides the raw `wasi:sockets` sockets.

#[cfg(not(target_arch = "wasm32"))]
pub use crate::net_native::*;
#[cfg(target_arch = "wasm32")]
pub use crate::net_wasm::*;
