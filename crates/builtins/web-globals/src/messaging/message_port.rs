// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`MessagePort`](https://html.spec.whatwg.org/multipage/web-messaging.html#message-ports)
//! interface.
//!
//! A message is serialized when it is posted and deserialized by a task of the event loop, one
//! task for each message. Transferring a port moves its state to a new port when the message is
//! posted, and the message's event gets that port.

use std::collections::VecDeque;

use core_runtime::event_loop::{with_active_event_loop, InterestHandle, Task, TaskId};
use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::{ConversionError, FromJSVal};
use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::heap::{RootedTraceableBox, Trace};
use js::native::JSTracer;
use js::prelude::HandleValue;
use js::structured_clone::{CloneError, HostTransfer, Serialized};
use js::{Callable, Object};

use crate::dom_exception::throw_dom_exception;
use crate::events::algorithms::{self as event_algorithms, ScriptStackState};
use crate::events::event_target::{EventTarget, EventTargetImpl};
use crate::events::message_event::MessageEvent;
use crate::structured_clone::StructuredSerializeOptions;

/// A posted message waiting in a port message queue.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct Message {
    /// The serialized message, until its task deserializes it.
    #[no_trace]
    data: Option<Serialized>,
    /// The ports the message transferred, in the order of its transfer list.
    ports: Vec<Heap<MessagePortImpl>>,
}

/// <https://html.spec.whatwg.org/multipage/web-messaging.html#messageport>
#[webidl_interface(extends = EventTarget)]
pub struct MessagePort {
    parent: EventTargetImpl,
    /// The port this port is entangled with.
    entangled: Option<Heap<MessagePortImpl>>,
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#port-message-queue>: the
    /// messages whose tasks haven't run. While the queue is enabled, each message has a task in
    /// the event loop.
    queue: VecDeque<Message>,
    /// Whether the port message queue is enabled.
    #[no_trace]
    enabled: bool,
    /// The `[[Detached]]` internal slot, which `close()` and transferring the port set.
    #[no_trace]
    detached: bool,
    /// Whether `unref()` was called after the last `ref()`.
    #[no_trace]
    unreferenced: bool,
    /// The `onmessage` event handler.
    onmessage_handler: Option<Heap<js::function::Callable>>,
    /// The `onmessageerror` event handler.
    onmessageerror_handler: Option<Heap<js::function::Callable>>,
    /// The interest that keeps the event loop alive while the port can receive messages. See
    /// [`super::set_node_ports`].
    #[no_trace]
    keep_alive: Option<InterestHandle>,
}

#[webidl_methods]
impl MessagePort {
    /// A new port, which isn't entangled and whose port message queue is disabled.
    fn new() -> Self {
        MessagePortImpl::default()
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-messageport-postmessage>
    /// and <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-messageport-postmessage-options>
    #[method(length = 1)]
    fn post_message(
        &self,
        scope: &Scope<'_>,
        message: HandleValue<'_>,
        options: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let transfer = transfer_list(scope, options)?;
        // Step 1: Let _targetPort_ be the port with which `this` is entangled, if any; otherwise
        //         let it be null.
        let target_port = self.data().entangled.get(scope);
        // Step 2: Run the `message port post message steps` providing `this`, _targetPort_,
        //         _message_ and _options_.
        self.message_port_post_message_steps(scope, target_port, message, &transfer)
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#message-port-post-message-steps>
    fn message_port_post_message_steps(
        &self,
        scope: &Scope<'_>,
        target_port: Option<MessagePort<'_>>,
        message: HandleValue<'_>,
        transfer: &[Object<'_>],
    ) -> Result<(), ExnThrown> {
        // Step 1: Let _transfer_ be _options_["``transfer``"].
        // Step 2: If _transfer_ `contains` _sourcePort_, then throw a "``DataCloneError``"
        //         ``DOMException``.
        if transfer.iter().any(|object| self.eq_object(object)) {
            return Err(throw_dom_exception(
                scope,
                "DataCloneError",
                "The transfer list contains the source port",
            ));
        }
        // Step 3: Let _doomed_ be false.
        // Step 4: If _targetPort_ is not null and _transfer_ `contains` _targetPort_, then set
        //         _doomed_ to true and optionally report to a developer console that the target
        //         port was posted to itself, causing the communication channel to be lost.
        let doomed = target_port
            .as_ref()
            .is_some_and(|target| transfer.iter().any(|object| target.eq_object(object)));
        // Step 5: Let _serializeWithTransferResult_ be
        //         `StructuredSerializeWithTransfer`(_message_, _transfer_). Rethrow any
        //         exceptions.
        let mut ports = PortTransfer {
            ports: RootedTraceableBox::new(Vec::new()),
        };
        let data = match Serialized::write(scope, message, transfer, &mut ports) {
            Ok(data) => data,
            Err(CloneError::Thrown(thrown)) => return Err(thrown),
            Err(CloneError::DataClone(message)) => {
                return Err(throw_dom_exception(scope, "DataCloneError", &message))
            }
        };
        // Step 6: If _targetPort_ is null, or if _doomed_ is true, then return.
        let Some(target_port) = target_port.filter(|_| !doomed) else {
            return Ok(());
        };
        // Step 7: Add a task that runs the following steps to the `port message queue` of
        //         _targetPort_:
        // `MessagePort::run_message_task` runs the task's steps.
        target_port.data_mut().queue.push_back(Message {
            data: Some(data),
            ports: std::mem::take(&mut *ports.ports),
        });
        if target_port.data().enabled {
            target_port.queue_message_task();
        }
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-messageport-start>
    #[method]
    fn start(&self) {
        // The `start()` method steps are to `enable` `this`'s `port message queue`, if it is not
        // already enabled.
        self.enable();
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-messageport-close>
    #[method]
    fn close(&self, scope: &Scope<'_>) {
        // Step 1: Set `this`'s `[[Detached]]` internal slot value to true.
        self.data_mut().detached = true;
        // Step 2: If `this` is `entangled`, `disentangle` it.
        if self.data().entangled.is_some() {
            self.disentangle(scope);
        }
        self.update_keep_alive();
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-messageport-onmessage>
    #[getter]
    fn onmessage<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onmessage_handler.get(scope)
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-messageport-onmessage>
    ///
    /// Setting it the first time enables the port message queue, as `start()` does.
    #[setter]
    fn set_onmessage(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        self.set_event_handler(scope, "message", value);
        self.enable();
        self.update_keep_alive();
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-messageport-onmessageerror>
    #[getter]
    fn onmessageerror<'r>(&self, scope: &'r Scope<'_>) -> Option<Callable<'r>> {
        self.data().onmessageerror_handler.get(scope)
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#handler-messageport-onmessageerror>
    #[setter]
    fn set_onmessageerror(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        self.set_event_handler(scope, "messageerror", value);
    }

    /// Node.js's `port.ref()`: let the port keep the event loop alive again. See
    /// <https://nodejs.org/api/worker_threads.html#portref>.
    #[method(name = "ref")]
    fn js_ref(&self) {
        self.data_mut().unreferenced = false;
        self.update_keep_alive();
    }

    /// Node.js's `port.unref()`: stop the port from keeping the event loop alive. See
    /// <https://nodejs.org/api/worker_threads.html#portunref>.
    #[method]
    fn unref(&self) {
        self.data_mut().unreferenced = true;
        self.update_keep_alive();
    }

    /// Node.js's `port.hasRef()`: whether the port keeps the event loop alive. See
    /// <https://nodejs.org/api/worker_threads.html#porthasref>.
    #[method]
    fn has_ref(&self) -> bool {
        self.data().keep_alive.is_some()
    }

    /// Whether `object` is this port.
    fn eq_object(&self, object: &Object<'_>) -> bool {
        // SAFETY: the pointers are only compared.
        unsafe { self.as_raw() == object.as_raw() }
    }

    /// Replace the event handler for `event_type` with `value` if it is callable, or remove it.
    fn set_event_handler(&self, scope: &Scope<'_>, event_type: &str, value: HandleValue<'_>) {
        let old = match event_type {
            "message" => self.data().onmessage_handler.get(scope),
            _ => self.data().onmessageerror_handler.get(scope),
        };
        let handler = super::replace_event_handler(scope, self, event_type, old, value);
        let mut data = self.data_mut();
        let slot = match event_type {
            "message" => &mut data.onmessage_handler,
            _ => &mut data.onmessageerror_handler,
        };
        match handler {
            Some(handler) => slot.set(handler),
            None => *slot = None,
        }
    }

    /// Enable the port message queue if it isn't enabled, which queues a task for each message
    /// in it.
    fn enable(&self) {
        if self.data().enabled {
            return;
        }
        self.data_mut().enabled = true;
        let count = self.data().queue.len();
        for _ in 0..count {
            self.queue_message_task();
        }
        self.update_keep_alive();
    }

    /// Queue the task of the message at the front of the port message queue in the event loop.
    fn queue_message_task(&self) {
        let task = PortMessageTask {
            port: Heap::from(*self),
        };
        with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)));
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#entangle>
    pub(crate) fn entangle(&self, scope: &Scope<'_>, other: &MessagePort<'_>) {
        // Step 1: If one of the ports is already entangled, then disentangle it and the port that
        //         it was entangled with.
        for port in [self, other] {
            if port.data().entangled.is_some() {
                port.disentangle(scope);
            }
        }
        // Step 2: Associate the two ports to be entangled, so that they form the two parts of a
        //         new channel. (There is no ``MessageChannel`` object that represents this
        //         channel.)
        self.data_mut().entangled.set(*other);
        other.data_mut().entangled.set(*self);
        self.update_keep_alive();
        other.update_keep_alive();
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#disentangle>
    fn disentangle(&self, scope: &Scope<'_>) {
        // Step 1: Let _otherPort_ be the ``MessagePort`` which _initiatorPort_ was entangled
        //         with.
        let other_port = self.data().entangled.get(scope);
        // Step 2: Assert: _otherPort_ exists.
        let other_port = other_port.expect("the port is entangled");
        // Step 3: Disentangle _initiatorPort_ and _otherPort_, so that they are no longer
        //         entangled or associated with each other.
        self.data_mut().entangled = None;
        other_port.data_mut().entangled = None;
        self.update_keep_alive();
        other_port.update_keep_alive();
    }

    /// The transfer steps and transfer-receiving steps of a port in a transfer list: a new port
    /// that takes over the port message queue and the entanglement of this port, which is
    /// detached.
    ///
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#transferMessagePort>
    fn transfer<'r>(&self, scope: &'r Scope<'_>) -> Result<MessagePort<'r>, ExnThrown> {
        let value = MessagePort::new(scope)?;
        // The transfer steps:
        // Step 1: Set _value_'s `has been shipped` flag to true.
        // Step 2: Set _dataHolder_.[[PortMessageQueue]] to _value_'s `port message queue`.
        // Step 3: If _value_ is entangled with another port _remotePort_:
        // Step 3.1: Set _remotePort_'s `has been shipped` flag to true.
        // Step 3.2: Set _dataHolder_.[[RemotePort]] to _remotePort_.
        // Step 4: Otherwise, set _dataHolder_.[[RemotePort]] to null.
        // The transfer-receiving steps:
        // Step 1: Set _value_'s `has been shipped` flag to true.
        // Step 2: Move all the tasks that are to fire ``message`` events in
        //         _dataHolder_.[[PortMessageQueue]] to the `port message queue` of _value_, if
        //         any, leaving _value_'s `port message queue` in its initial disabled state.
        // Step 3: If _dataHolder_.[[RemotePort]] is not null, then `entangle`
        //         _dataHolder_.[[RemotePort]] and _value_. (This will disentangle
        //         _dataHolder_.[[RemotePort]] from the original port that was transferred.)
        // The `has been shipped` flag only selects a task source, which has no effect within one
        // event loop. The tasks of the moved messages stay queued for this port and find its
        // queue empty.
        value.data_mut().queue = std::mem::take(&mut self.data_mut().queue);
        let remote_port = self.data().entangled.get(scope);
        if let Some(remote_port) = remote_port {
            remote_port.entangle(scope, &value);
        }
        self.data_mut().detached = true;
        self.update_keep_alive();
        Ok(value)
    }

    /// The steps of the task of the message at the front of the port message queue, from the
    /// `message port post message steps`.
    fn run_message_task(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        if self.data().queue.is_empty() {
            return Ok(());
        }
        let mut message = RootedTraceableBox::new(
            self.data_mut()
                .queue
                .pop_front()
                .expect("the queue isn't empty"),
        );
        // Step 7.1: Let _finalTargetPort_ be the ``MessagePort`` in whose `port message queue`
        //           the task now finds itself.
        // Step 7.2: Let _messageEventTarget_ be _finalTargetPort_'s `message event target`.
        // Step 7.3: Let _targetRealm_ be _finalTargetPort_'s `relevant realm`.
        // The port is the message event target, and the task runs in its realm.
        let new_ports: Vec<MessagePort<'_>> =
            message.ports.iter().map(|port| port.get(scope)).collect();
        // Step 7.4: Let _deserializeRecord_ be
        //           `StructuredDeserializeWithTransfer`(_serializeWithTransferResult_,
        //           _targetRealm_). If this throws an exception, catch it, `fire an event` named
        //           ``messageerror`` at _messageEventTarget_, using ``MessageEvent``, and then
        //           return.
        let data = message.data.take().expect("a message is deserialized once");
        let mut receiver = PortReceiver { ports: &new_ports };
        let message_clone = match data.read(scope, &mut receiver) {
            Ok(message_clone) => message_clone,
            Err(error) => {
                if let CloneError::Thrown(_) = error {
                    js::exception::clear(scope);
                }
                let event =
                    MessageEvent::new_trusted(scope, "messageerror", HandleValue::null(), &[])?;
                event_algorithms::dispatch(scope, &event, self, ScriptStackState::Empty);
                return Ok(());
            }
        };
        // Step 7.5: Let _messageClone_ be _deserializeRecord_.[[Deserialized]].
        // Step 7.6: Let _newPorts_ be a new `frozen array` consisting of all ``MessagePort``
        //           objects in _deserializeRecord_.[[TransferredValues]], if any, maintaining
        //           their relative order.
        // Step 7.7: `Fire an event` named ``message`` at _messageEventTarget_, using
        //           ``MessageEvent``, with the ``data`` attribute initialized to _messageClone_
        //           and the ``ports`` attribute initialized to _newPorts_.
        let event = MessageEvent::new_trusted(scope, "message", message_clone, &new_ports)?;
        event_algorithms::dispatch(scope, &event, self, ScriptStackState::Empty);
        Ok(())
    }

    /// Acquire or release the interest that keeps the event loop alive while the port can receive
    /// messages: while ports behave as in Node.js, and the port is started, referenced, entangled
    /// and not detached, and has a `message` listener.
    pub(crate) fn update_keep_alive(&self) {
        let wanted = super::node_ports() && {
            let data = self.data();
            data.enabled
                && !data.unreferenced
                && !data.detached
                && data.entangled.is_some()
                && data
                    .parent
                    .event_listener_list
                    .iter()
                    .any(|listener| listener.event_type == "message")
        };
        let held = self.data().keep_alive.is_some();
        if wanted && !held {
            let interest =
                with_active_event_loop(|event_loop| event_loop.acquire_interest_handle());
            self.data_mut().keep_alive = interest;
        } else if !wanted && held {
            self.data_mut().keep_alive = None;
        }
    }
}

/// The transfer list of the second argument of `postMessage`, which is a sequence of objects or a
/// `StructuredSerializeOptions` dictionary. An object with an `@@iterator` method is a sequence.
fn transfer_list<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'_>,
) -> Result<Vec<Object<'s>>, ExnThrown> {
    let iterable =
        js::conversion::is_iterable_value(scope, options).map_err(|error| error.throw(scope))?;
    if iterable {
        return Vec::<Object<'s>>::from_jsval_throwing(scope, options, ());
    }
    let options =
        StructuredSerializeOptions::from_jsval(scope, options, ()).map_err(
            |error| match error {
                ConversionError::ExnPending => ExnThrown,
                failure => failure.throw(scope),
            },
        )?;
    Ok(options.transfer)
}

/// Update the keep-alive interest of `target` if it is a `MessagePort`, after a `message` listener
/// was added or removed. While ports behave as in Node.js, adding one starts the port.
pub(crate) fn message_listeners_changed(target: &EventTarget<'_>, added: bool) {
    if let Ok(port) = target.as_object().cast::<MessagePort>() {
        if added && super::node_ports() {
            port.enable();
        }
        port.update_keep_alive();
    }
}

/// The value of `structuredClone(value, { transfer })`: `value` serialized with `transfer`, which
/// can hold `ArrayBuffer`s and `MessagePort`s, and deserialized. Throws a "`DataCloneError`"
/// `DOMException` for a value or transfer list that can't be serialized.
pub(crate) fn structured_clone<'r>(
    scope: &'r Scope<'_>,
    value: HandleValue<'_>,
    transfer: &[Object<'_>],
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut ports = PortTransfer {
        ports: RootedTraceableBox::new(Vec::new()),
    };
    let data_clone_error = |scope: &Scope<'_>, error| match error {
        CloneError::Thrown(thrown) => thrown,
        CloneError::DataClone(message) => throw_dom_exception(scope, "DataCloneError", &message),
    };
    let data = Serialized::write(scope, value, transfer, &mut ports)
        .map_err(|error| data_clone_error(scope, error))?;
    let new_ports: Vec<MessagePort<'_>> = ports.ports.iter().map(|port| port.get(scope)).collect();
    let mut receiver = PortReceiver { ports: &new_ports };
    data.read(scope, &mut receiver)
        .map_err(|error| data_clone_error(scope, error))
}

/// The task of a message in a port message queue, which runs
/// [`MessagePort::run_message_task`].
#[js::allow_unrooted_interior]
struct PortMessageTask {
    port: Heap<MessagePortImpl>,
}

impl Task for PortMessageTask {
    fn kind(&self) -> &'static str {
        "message"
    }

    fn run(self: Box<Self>, scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        let port: MessagePort<'_> = self.port.take(scope);
        port.run_message_task(scope)
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn trace(&self, trc: *mut JSTracer) {
        // SAFETY: `trc` is the tracer the event loop traces its tasks with.
        unsafe {
            self.port.trace(trc);
        }
    }
}

/// The ports of a transfer list that a message being posted transfers.
struct PortTransfer {
    /// The ports [`MessagePort::transfer`] created, by the index `transfer` returned.
    ports: RootedTraceableBox<Vec<Heap<MessagePortImpl>>>,
}

impl HostTransfer for PortTransfer {
    fn can_transfer(&mut self, _scope: &Scope<'_>, object: Object<'_>) -> bool {
        object
            .cast::<MessagePort>()
            .is_ok_and(|port| !port.data().detached)
    }

    fn transfer(&mut self, scope: &Scope<'_>, object: Object<'_>) -> Result<u64, ExnThrown> {
        let port = object
            .cast::<MessagePort>()
            .expect("`can_transfer` accepted only ports");
        let value = port.transfer(scope)?;
        self.ports.push(Heap::from(value));
        Ok(self.ports.len() as u64 - 1)
    }

    fn receive<'s>(&mut self, _scope: &'s Scope<'_>, _index: u64) -> Result<Object<'s>, ExnThrown> {
        unreachable!("a message is deserialized by its task")
    }
}

/// The ports a message transferred, which its deserialization receives.
struct PortReceiver<'a, 'p> {
    ports: &'a [MessagePort<'p>],
}

impl HostTransfer for PortReceiver<'_, '_> {
    fn can_transfer(&mut self, _scope: &Scope<'_>, _object: Object<'_>) -> bool {
        false
    }

    fn transfer(&mut self, _scope: &Scope<'_>, _object: Object<'_>) -> Result<u64, ExnThrown> {
        unreachable!("a deserialization transfers nothing")
    }

    fn receive<'s>(&mut self, scope: &'s Scope<'_>, index: u64) -> Result<Object<'s>, ExnThrown> {
        let port = usize::try_from(index)
            .ok()
            .and_then(|index| self.ports.get(index))
            .ok_or(ExnThrown)?;
        // SAFETY: the port is rooted, and the pointer is rooted again in `scope` right away.
        unsafe { Object::from_raw(scope, port.as_raw()) }.ok_or(ExnThrown)
    }
}
