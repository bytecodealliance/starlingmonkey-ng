// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

pub mod abort;
pub mod algorithms;
pub mod body_mixin;
pub mod body_proxy;
pub mod byte_string;
pub mod consume;
pub mod globals;
pub mod headers;
pub mod incoming_body;
pub mod outgoing_body;
pub mod request;
pub mod response;
pub mod transport;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    headers::add_to_global(scope, global);
    request::Request::add_to_global(scope, global);
    response::Response::add_to_global(scope, global);
    globals::globals::add_to_global(scope, global);

    // `fetchLater()` / `FetchLaterResult` are deliberately not exposed: the API is
    // `[Exposed=Window]`-only and unimplemented here.

    // The internal classes below are registered so instances can be created, and are not
    // web-exposed.
    abort::AbortFetchState::add_to_global(scope, global);
    incoming_body::HostBodySource::add_to_global(scope, global);
    outgoing_body::OutgoingBodyPump::add_to_global(scope, global);
    body_proxy::BodyProxySource::add_to_global(scope, global);
}
