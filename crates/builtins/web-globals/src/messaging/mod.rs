// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [Channel messaging](https://html.spec.whatwg.org/multipage/web-messaging.html#channel-messaging)
//! and [broadcasting](https://html.spec.whatwg.org/multipage/web-messaging.html#broadcasting-to-other-browsing-contexts)
//! within one global: `MessageChannel`, `MessagePort` and `BroadcastChannel`.

pub mod broadcast_channel;
pub mod message_channel;
pub mod message_port;

use std::cell::Cell;

use core_runtime::event_loop::{with_active_event_loop, Task, TaskId};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;

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

/// A task that never becomes ready. While it is queued, it keeps its event loop running, as a
/// referenced timer does, and like a timer it doesn't extend the lifetime of a served request.
struct KeepAliveTask(&'static str);

impl Task for KeepAliveTask {
    fn kind(&self) -> &'static str {
        self.0
    }

    fn run(self: Box<Self>, _scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        Ok(())
    }
}

/// The [`KeepAliveTask`] of a port or channel: the id of the event loop it is queued in, and its
/// own id there.
struct KeepAlive {
    loop_id: u64,
    task: TaskId,
}

impl KeepAlive {
    /// Queue a [`KeepAliveTask`] of `kind` in the active event loop. `None` if there is none.
    fn hold(kind: &'static str) -> Option<KeepAlive> {
        with_active_event_loop(|event_loop| KeepAlive {
            loop_id: event_loop.id(),
            task: event_loop.queue(Box::new(KeepAliveTask(kind))),
        })
    }

    /// Cancel the task if the active event loop is the one it is queued in. A task queued in
    /// another loop stays queued until that loop ends.
    fn release(self) {
        with_active_event_loop(|event_loop| {
            if event_loop.id() == self.loop_id {
                event_loop.cancel_if_queued(self.task);
            }
        });
    }
}

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    message_port::MessagePort::add_to_global(scope, global);
    message_channel::MessageChannel::add_to_global(scope, global);
    broadcast_channel::BroadcastChannel::add_to_global(scope, global);
    broadcast_channel::ChannelRegistry::add_to_global(scope, global);
}
