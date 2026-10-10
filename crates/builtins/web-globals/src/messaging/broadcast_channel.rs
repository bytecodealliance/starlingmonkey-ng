// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`BroadcastChannel`](https://html.spec.whatwg.org/multipage/web-messaging.html#broadcasting-to-other-browsing-contexts)
//! interface, within one global.
//!
//! The global's open channels are the private data of a hidden per-global object, which keeps
//! them alive until they are closed.

use std::rc::Rc;

use core_runtime::event_loop::{with_active_event_loop, Task, TaskId};
use core_runtime::{jsclass, jsmethods, webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::JSTracer;
use js::prelude::HandleValue;
use js::structured_clone::{CloneError, NoHostTransfer, Serialized};
use js::Callable;

use crate::dom_exception::throw_dom_exception;
use crate::events::algorithms::{self as event_algorithms, ScriptStackState};
use crate::events::event_target::{EventTarget, EventTargetImpl};
use crate::events::message_event::MessageEvent;

/// The open `BroadcastChannel`s of a global, in creation order.
#[jsclass(hidden)]
pub(crate) struct ChannelRegistry {
    channels: Vec<Heap<BroadcastChannelImpl>>,
}

#[jsmethods]
impl ChannelRegistry {}

/// The global's [`ChannelRegistry`].
fn registry<'s>(scope: &'s Scope<'_>) -> Result<ChannelRegistry<'s>, ExnThrown> {
    static KEY: u8 = 0;
    let object = js::class::get_or_init_shared_object(scope, &raw const KEY as usize, |scope| {
        let registry = js::class::create_instance_with::<ChannelRegistryImpl>(scope, |_| {
            ChannelRegistryImpl::default()
        })?;
        Ok(registry.as_object())
    })?;
    Ok(object
        .cast::<ChannelRegistry>()
        .expect("the shared object is a registry"))
}

/// <https://html.spec.whatwg.org/multipage/web-messaging.html#broadcastchannel>
#[webidl_interface(extends = EventTarget)]
pub struct BroadcastChannel {
    parent: EventTargetImpl,
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#channel-name>
    #[no_trace]
    channel_name: String,
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#concept-broadcastchannel-closed>
    #[no_trace]
    closed: bool,
    /// Whether `unref()` was called after the last `ref()`.
    #[no_trace]
    unreferenced: bool,
    /// The `onmessage` event handler.
    onmessage_handler: Option<Heap<js::function::Callable>>,
    /// The `onmessageerror` event handler.
    onmessageerror_handler: Option<Heap<js::function::Callable>>,
    /// The task that keeps the event loop alive while the channel is open and referenced, if
    /// ports behave as in Node.js. See [`super::set_node_ports`].
    #[no_trace]
    keep_alive: Option<super::KeepAlive>,
}

#[webidl_methods]
impl BroadcastChannel {
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-broadcastchannel>
    #[constructor]
    fn new(&self, scope: &Scope<'_>, name: String) -> Result<(), ExnThrown> {
        // Step 1: Set `this`'s `channel name` to _name_.
        self.data_mut().channel_name = name;
        // Step 2: Set `this`'s `closed flag` to false.
        self.data_mut().closed = false;
        registry(scope)?.data_mut().channels.push(Heap::from(*self));
        self.update_keep_alive();
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-broadcastchannel-name>
    #[getter]
    fn name(&self) -> String {
        // The `name` getter steps are to return `this`'s `channel name`.
        self.data().channel_name.clone()
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-broadcastchannel-postmessage>
    #[method]
    fn post_message(&self, scope: &Scope<'_>, message: HandleValue<'_>) -> Result<(), ExnThrown> {
        // Step 1: If `this` is not `eligible for messaging`, then return.
        // The global is always eligible.
        // Step 2: If `this`'s `closed flag` is true, then throw an "``InvalidStateError``"
        //         ``DOMException``.
        if self.data().closed {
            return Err(throw_dom_exception(
                scope,
                "InvalidStateError",
                "The BroadcastChannel is closed",
            ));
        }
        // Step 3: Let _serialized_ be `StructuredSerialize`(_message_). Rethrow any exceptions.
        let serialized = match Serialized::write(scope, message, &[], &mut NoHostTransfer) {
            Ok(serialized) => Rc::new(serialized),
            Err(CloneError::Thrown(thrown)) => return Err(thrown),
            Err(CloneError::DataClone(message)) => {
                return Err(throw_dom_exception(scope, "DataCloneError", &message))
            }
        };
        // Step 4: Let _sourceOrigin_ be `this`'s `relevant settings object`'s `origin`.
        let source_origin: Rc<str> = crate::worker_location::current_location_url()
            .map_or_else(String::new, |url| url.origin().ascii_serialization())
            .into();
        // Step 5: Let _sourceStorageKey_ be the result of running `obtain a storage key for
        //         non-storage purposes` with `this`'s `relevant settings object`.
        // Step 6: Let _destinations_ be a list of ``BroadcastChannel`` objects that match the
        //         following criteria:
        //   - They are `eligible for messaging`.
        //   - The result of running `obtain a storage key for non-storage purposes` with their
        //     `relevant settings object` equals _sourceStorageKey_.
        //   - Their `channel name` is `this`'s `channel name`.
        // Step 7: Remove _source_ from _destinations_.
        // Step 8: Sort _destinations_ such that all ``BroadcastChannel`` objects whose `relevant
        //         agents` are the same are sorted in creation order, oldest first. (This does not
        //         define a complete ordering. Within this constraint, user agents may sort the
        //         list in any implementation-defined manner.)
        // The destinations are the global's other open channels of the same name, in the
        // registry's creation order.
        let registry = registry(scope)?;
        let destinations: Vec<BroadcastChannel<'_>> = registry
            .data()
            .channels
            .iter()
            .map(|channel| channel.get(scope))
            .collect();
        let name = self.data().channel_name.clone();
        // Step 9: For each _destination_ in _destinations_, `queue a global task` on the `DOM
        //         manipulation task source` given _destination_'s `relevant global object` to
        //         perform the following steps:
        for destination in destinations {
            if destination.eq_stack(&self.0) || destination.data().channel_name != name {
                continue;
            }
            let task = BroadcastTask {
                destination: Heap::from(destination),
                serialized: Rc::clone(&serialized),
                source_origin: Rc::clone(&source_origin),
            };
            with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)));
        }
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-broadcastchannel-close>
    #[method]
    fn close(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // The `close()` method steps are to set `this`'s `closed flag` to true.
        self.data_mut().closed = true;
        registry(scope)?
            .data_mut()
            .channels
            .retain(|channel| !self.eq_heap(channel));
        self.update_keep_alive();
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-broadcastchannel-onmessage>
    #[getter]
    fn onmessage<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onmessage_handler.get(scope)
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-broadcastchannel-onmessage>
    #[setter]
    fn set_onmessage(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        let old = self.data().onmessage_handler.get(scope);
        let handler = crate::events::replace_event_handler(scope, self, "message", old, value);
        match handler {
            Some(handler) => self.data_mut().onmessage_handler.set(handler),
            None => self.data_mut().onmessage_handler = None,
        }
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-broadcastchannel-onmessageerror>
    #[getter]
    fn onmessageerror<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onmessageerror_handler.get(scope)
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-broadcastchannel-onmessageerror>
    #[setter]
    fn set_onmessageerror(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        let old = self.data().onmessageerror_handler.get(scope);
        let handler = crate::events::replace_event_handler(scope, self, "messageerror", old, value);
        match handler {
            Some(handler) => self.data_mut().onmessageerror_handler.set(handler),
            None => self.data_mut().onmessageerror_handler = None,
        }
    }

    /// Node.js's `broadcastChannel.ref()`: let the channel keep the event loop alive again. See
    /// <https://nodejs.org/api/worker_threads.html#broadcastchannelref>.
    #[method(name = "ref")]
    fn js_ref(&self) {
        self.data_mut().unreferenced = false;
        self.update_keep_alive();
    }

    /// Node.js's `broadcastChannel.unref()`: stop the channel from keeping the event loop alive.
    /// See <https://nodejs.org/api/worker_threads.html#broadcastchannelunref>.
    #[method]
    fn unref(&self) {
        self.data_mut().unreferenced = true;
        self.update_keep_alive();
    }

    /// Queue or cancel the task that keeps the event loop alive while ports behave as in Node.js
    /// and the channel is open and referenced.
    fn update_keep_alive(&self) {
        let wanted = super::node_ports() && {
            let data = self.data();
            !data.closed && !data.unreferenced
        };
        let held = self.data().keep_alive.is_some();
        if wanted && !held {
            let keep_alive = super::KeepAlive::hold("BroadcastChannel");
            self.data_mut().keep_alive = keep_alive;
        } else if !wanted && held {
            let keep_alive = self.data_mut().keep_alive.take();
            keep_alive.expect("checked above").release();
        }
    }

    /// The steps of a task of `postMessage` for this destination.
    fn run_message_task(
        &self,
        scope: &Scope<'_>,
        serialized: &Serialized,
        source_origin: &str,
    ) -> Result<(), ExnThrown> {
        // Step 9.1: If _destination_'s `closed flag` is true, then abort these steps.
        if self.data().closed {
            return Ok(());
        }
        // Step 9.2: Let _targetRealm_ be _destination_'s `relevant realm`.
        // The task runs in the global's realm.
        // Step 9.3: Let _data_ be `StructuredDeserialize`(_serialized_, _targetRealm_). If this
        //           throws an exception, catch it, `fire an event` named ``messageerror`` at
        //           _destination_, using ``MessageEvent``, with its ``origin`` initialized to
        //           _sourceOrigin_, and then abort these steps.
        let (event_type, data) = match serialized.read_copy(scope) {
            Ok(data) => ("message", data),
            Err(error) => {
                if let CloneError::Thrown(_) = error {
                    js::exception::clear(scope);
                }
                ("messageerror", HandleValue::null())
            }
        };
        // Step 9.4: `Fire an event` named ``message`` at _destination_, using ``MessageEvent``,
        //           with the ``data`` attribute initialized to _data_ and its ``origin``
        //           initialized to _sourceOrigin_.
        let event = MessageEvent::new_trusted(scope, event_type, data, &[])?;
        event.set_origin(source_origin);
        event_algorithms::dispatch(scope, &event, self, ScriptStackState::Empty);
        Ok(())
    }
}

/// A task of `postMessage` for one destination, which runs
/// [`BroadcastChannel::run_message_task`].
#[js::allow_unrooted_interior]
struct BroadcastTask {
    destination: Heap<BroadcastChannelImpl>,
    serialized: Rc<Serialized>,
    source_origin: Rc<str>,
}

impl Task for BroadcastTask {
    fn kind(&self) -> &'static str {
        "broadcast-message"
    }

    fn run(self: Box<Self>, scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        let serialized = Rc::clone(&self.serialized);
        let source_origin = Rc::clone(&self.source_origin);
        let destination: BroadcastChannel<'_> = self.destination.take(scope);
        destination.run_message_task(scope, &serialized, &source_origin)
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn trace(&self, trc: *mut JSTracer) {
        // SAFETY: `trc` is the tracer the event loop traces its tasks with.
        unsafe {
            self.destination.trace(trc);
        }
    }
}
