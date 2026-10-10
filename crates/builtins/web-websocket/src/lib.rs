// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The [WebSockets standard](https://websockets.spec.whatwg.org/): `WebSocket` and `CloseEvent`.

pub mod close_event;
pub mod session;
pub mod websocket;

use js::gc::scope::Scope;
use js::Object;

pub use websocket::set_node_compat;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    close_event::CloseEvent::add_to_global(scope, global);
    websocket::WebSocket::add_to_global(scope, global);
}
