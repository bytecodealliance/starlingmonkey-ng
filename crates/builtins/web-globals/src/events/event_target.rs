// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`EventTarget`](https://dom.spec.whatwg.org/#interface-eventtarget) interface.
//!
//! EventTarget is the base interface for all objects that can receive events
//! and have listeners attached. This includes standalone `EventTarget` objects,
//! and types that inherit from it (e.g., AbortSignal).

use core_runtime::webidl_interface;
use core_runtime::webidl_methods;
use js::error::{throw_type_error, ExnThrown};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use super::algorithms;
use super::event::Event;
use crate::dom_exception::DOMExceptionError;
use crate::events::event::EventFlags;

/// An event listener stored in an EventTarget's event listener list.
///
/// <https://dom.spec.whatwg.org/#concept-event-listener>
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct EventListener {
    /// Identity used to re-locate this listener in the live list during
    /// dispatch.
    #[no_trace]
    pub(crate) id: u64,
    #[no_trace]
    pub(crate) event_type: String,
    /// The `EventListener` callback object: a callable, or an object with a
    /// `handleEvent` method. Listener identity is compared by reading this
    /// Heap's live pointer ([`Heap::as_ptr`]), so it stays correct across a
    /// compacting GC.
    pub(crate) callback: Heap<js::object::Object>,
    #[no_trace]
    pub(crate) capture: bool,
    #[no_trace]
    pub(crate) passive: Option<bool>,
    #[no_trace]
    pub(crate) once: bool,
    /// Whether this is the listener of an event handler, whose `callback` is the handler's value.
    /// Such a listener is never `once`, `capture` or `passive`, and `addEventListener` and
    /// `removeEventListener` neither match nor remove it.
    #[no_trace]
    pub(crate) handler: bool,
}

/// Convert an `EventListener?` argument: `None` for `null` and `undefined`, the
/// object for any object, and a TypeError for anything else.
fn event_listener_argument<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    if callback.is_null_or_undefined() {
        return Ok(None);
    }
    Object::from_raw_value(scope, callback.get())
        .map(Some)
        .map_err(|_| throw_type_error(scope, c"event listener must be an object"))
}

/// <https://dom.spec.whatwg.org/#interface-eventtarget>
#[webidl_interface]
pub struct EventTarget {
    /// <https://dom.spec.whatwg.org/#eventtarget-event-listener-list>
    pub(crate) event_listener_list: Vec<EventListener>,
}

#[webidl_methods]
impl EventTarget {
    /// <https://dom.spec.whatwg.org/#dom-eventtarget-eventtarget>
    #[constructor]
    pub fn new() -> Self {
        // Step 1: Do nothing.
        EventTargetImpl {
            event_listener_list: Vec::new(),
        }
    }

    /// <https://dom.spec.whatwg.org/#dom-eventtarget-addeventlistener>
    #[method]
    pub fn add_event_listener(
        &self,
        scope: &Scope<'_>,
        event_type: String,
        callback: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Let _capture_, _passive_, _once_, and _signal_ be the result of `flattening more`
        //         _options_. This converts the `signal` member and may throw a TypeError, which
        //         must happen even when `callback` is null.
        let options = algorithms::flatten_more_options(scope, options)?;

        let Some(callback) = event_listener_argument(scope, callback)? else {
            return Ok(());
        };

        // Step 2: `Add an event listener` with `this` and an `event listener` whose `type` is
        //         _type_, `callback` is _callback_, `capture` is _capture_, `passive` is _passive_,
        //         `once` is _once_, and `signal` is _signal_.
        algorithms::add_an_event_listener(
            self,
            event_type,
            callback,
            options.capture,
            options.passive,
            options.once,
            options.signal,
        );
        Ok(())
    }

    /// <https://dom.spec.whatwg.org/#dom-eventtarget-removeeventlistener>
    #[method]
    pub fn remove_event_listener(
        &self,
        scope: &Scope<'_>,
        event_type: String,
        callback: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Let _capture_ be the result of `flattening` _options_.
        let capture = algorithms::flatten_options(scope, options)?;

        let Some(callback) = event_listener_argument(scope, callback)? else {
            return Ok(());
        };

        // Step 2: If `this`'s `event listener list` `contains` an `event listener` whose `type`
        //         is _type_, `callback` is _callback_, and `capture` is _capture_, then `remove an
        //         event listener` with `this` and that `event listener`.
        algorithms::remove_an_event_listener(&mut self.data_mut(), &event_type, &callback, capture);
        Ok(())
    }

    /// <https://dom.spec.whatwg.org/#dom-eventtarget-dispatchevent>
    #[method]
    pub fn dispatch_event(
        &self,
        scope: &Scope<'_>,
        event: Event<'_>,
    ) -> Result<bool, DOMExceptionError> {
        // Step 1: If _event_'s `dispatch flag` is set, or if its `initialized flag` is not set,
        //         then `throw` an "``InvalidStateError``" ``DOMException``.
        if event.is_dispatching() || !event.is_initialized() {
            return Err(DOMExceptionError::new(
                "InvalidStateError",
                "The event is already being dispatched or has not been initialized",
            ));
        }

        // Step 2: Initialize _event_'s ``isTrusted`` attribute to false.
        event.data_mut().flags.remove(EventFlags::TRUSTED);

        // Step 3: Return the result of `dispatching` _event_ to `this`.
        Ok(algorithms::dispatch(
            scope,
            &event,
            self,
            algorithms::ScriptStackState::NonEmpty,
        ))
    }

    /// Dispatches `event` on `self` without marking it as untrusted.
    ///
    /// Use this when implementing other builtins that need to dispatch events as part of
    /// their function.
    pub fn dispatch_trusted(
        &self,
        scope: &Scope<'_>,
        event: Event<'_>,
        script_stack_state: algorithms::ScriptStackState,
    ) {
        // Clients can use `event.is_canceled()` to check if the event was canceled.
        let _ = algorithms::dispatch(scope, &event, self, script_stack_state);
    }

    /// Whether any listener for `event_type` would be invoked by a dispatch.
    pub fn has_listener_for(&self, event_type: &str) -> bool {
        self.data()
            .event_listener_list
            .iter()
            .any(|listener| listener.event_type == event_type)
    }

    /// The number of entries in the event listener list, including an event handler's.
    ///
    /// Used in some tests, but not part of the stable public API.
    #[doc(hidden)]
    pub fn listener_count(&self) -> usize {
        self.data().event_listener_list.len()
    }
}
