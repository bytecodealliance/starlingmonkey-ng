// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Web platform global functions and interfaces.
//!
//! ## Globals
//!
//! - `btoa` and `atob`: [WHATWG Forgiving Base64] encoding/decoding
//! - `DOMException`: [WebIDL DOMException] interface
//!
//! [WHATWG Forgiving Base64]: https://infra.spec.whatwg.org/#forgiving-base64
//! [WebIDL DOMException]: https://webidl.spec.whatwg.org/#idl-DOMException

pub mod base64;
pub mod dom_exception;
pub mod encoding;
pub mod events;
pub mod navigator;
pub mod performance;
pub mod signals;
mod structured_clone;
pub mod worker_location;

pub fn add_to_global(scope: &js::prelude::Scope<'_>, global: js::Object<'_>) {
    dom_exception::DOMException::add_to_global(scope, global);
    encoding::add_to_global(scope, global);
    events::add_to_global(scope, global);
    performance::add_to_global(scope, global);
    signals::add_to_global(scope, global);
    base64::base64_globals::add_to_global(scope, global);
    structured_clone::structured_clone_globals::add_to_global(scope, global);
    worker_location::add_to_global(scope, global);
    navigator::add_to_global(scope, global);
}
