// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [Channel messaging](https://html.spec.whatwg.org/multipage/web-messaging.html#channel-messaging)
//! and [broadcasting](https://html.spec.whatwg.org/multipage/web-messaging.html#broadcasting-to-other-browsing-contexts)
//! within one global: `MessageChannel`, `MessagePort` and `BroadcastChannel`.

pub mod broadcast_channel;
pub mod message_channel;
pub mod message_port;

use std::cell::Cell;

use js::conversion::FromJSVal;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Callable, Object};

use crate::events::algorithms as event_algorithms;
use crate::events::event_target::EventTarget;

js::instance_local! {
    /// Whether ports behave as in Node.js. See [`set_node_ports`].
    static NODE_PORTS: Cell<bool> = const { Cell::new(false) };
}

/// Set whether `MessagePort`s behave as in Node.js, which they don't by default:
///
/// - Adding a `message` listener with `addEventListener` starts the port, as `start()` does.
/// - A port keeps the event loop alive while its port message queue is enabled, it is entangled,
///   it isn't closed or transferred, it has a `message` listener, and it wasn't `unref()`ed.
pub fn set_node_ports(node_ports: bool) {
    NODE_PORTS.with(|cell| cell.set(node_ports));
}

fn node_ports() -> bool {
    NODE_PORTS.with(Cell::get)
}

/// Replace the listener of the event handler of `target` for `event_type`: remove the listener of
/// `old`, the current handler, and add `value` as a listener if it is callable. Returns the new
/// handler, which is `value` if it is callable and `None` otherwise.
pub(crate) fn replace_event_handler<'s>(
    scope: &'s Scope<'_>,
    target: &EventTarget<'_>,
    event_type: &str,
    old: Option<Callable<'_>>,
    value: HandleValue<'_>,
) -> Option<Callable<'s>> {
    if let Some(old) = old {
        event_algorithms::remove_an_event_listener(&mut target.data_mut(), event_type, &old, false);
    }
    let handler = Callable::from_jsval(scope, value, ()).ok();
    if let Some(handler) = handler {
        event_algorithms::add_an_event_listener(
            target,
            event_type.to_string(),
            *handler,
            false,
            None,
            false,
            None,
        );
    }
    handler
}

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    message_port::MessagePort::add_to_global(scope, global);
    message_channel::MessageChannel::add_to_global(scope, global);
    broadcast_channel::BroadcastChannel::add_to_global(scope, global);
    broadcast_channel::ChannelRegistry::add_to_global(scope, global);
}
