// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/ServiceWorker/#extendableevent-interface>

use core_runtime::event_loop::{with_active_event_loop, InterestHandle};
use core_runtime::{jsclass, jsmethods};
use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::error::{ExnThrown, ThrowException};
use js::function::cast_payload;
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{value, Function, Object, Promise};
use web_globals::dom_exception::DOMExceptionError;
use web_globals::events::event::{EventImpl, EventInit};
use web_globals::events::Event;

/// <https://w3c.github.io/ServiceWorker/#dictdef-extendableeventinit>
/// It declares no members of its own; everything it has comes from `EventInit`.
#[webidl_dictionary(extends = EventInit)]
#[derive(Default, Clone, Copy)]
pub struct ExtendableEventInit {
    parent: EventInit,
}

impl ExtendableEventInit {
    pub fn new(cancelable: bool) -> Self {
        Self {
            parent: EventInit {
                cancelable,
                ..EventInit::default()
            },
        }
    }
}

#[webidl_interface(extends = Event)]
pub struct ExtendableEvent {
    parent: EventImpl,
    /// <https://w3c.github.io/ServiceWorker/#extendableevent-pending-promises-count>
    /// The number of pending promises in the `extend lifetime promises` set. The set itself
    /// isn't kept here, since lifetime management is done via the event loop's interest.
    pending_promises_count: usize,
}

#[webidl_methods]
impl ExtendableEvent {
    /// <https://w3c.github.io/ServiceWorker/#dom-extendableevent-extendableevent>
    #[constructor]
    pub fn new(event_type: &str, event_init: Option<&ExtendableEventInit>) -> Self {
        let event_init = event_init.copied().unwrap_or_default();
        ExtendableEventImpl {
            parent: EventImpl::new_untrusted(event_type.to_string(), Some(event_init.parent)),
            pending_promises_count: 0,
        }
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-extendableevent-waituntil>
    #[method]
    pub fn wait_until(&self, scope: &Scope, f: Promise) -> Result<(), ExnThrown> {
        self.add_lifetime_promise(scope, f, "waitUntil")
    }

    /// <https://w3c.github.io/ServiceWorker/#extendableevent-active>
    ///
    /// "An `ExtendableEvent` object is said to be active when its `timed out flag` is unset and
    /// either its `pending promises count` is greater than zero or its `dispatch flag` is set."
    /// The `timed out flag` is never set here (see [`ExtendableEventImpl`]), so it drops out.
    pub fn is_active(&self) -> bool {
        self.data().pending_promises_count > 0 || self.is_dispatching()
    }

    /// <https://w3c.github.io/ServiceWorker/#extendableevent-add-lifetime-promise>
    pub fn add_lifetime_promise(
        &self,
        scope: &Scope,
        promise: Promise,
        caller: &str,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _event_’s `isTrusted` attribute is false, `throw` an "`InvalidStateError`"
        //     `DOMException`.
        // Step 2: If _event_ is not `active`, `throw` an "`InvalidStateError`" `DOMException`.
        //     Spec note: Note: If no lifetime extension promise has been added in the task that
        //     called the event handlers, calling `waitUntil()` in subsequent asynchronous tasks
        //     will throw.
        self.check_can_extend_lifetime(scope, caller)?;
        // Step 3: Add _promise_ to _event_’s `extend lifetime promises`.
        // Skipped in favor of event loop interest-based lifetime tracking.
        // Note: only an active event loop tracks the lifetime, so without one, as in a
        // synchronous component export, a `TypeError` is thrown.
        let Some(handle) = with_active_event_loop(|el| el.acquire_interest_handle()) else {
            return Err(core_runtime::event_loop::throw_no_event_loop(
                scope,
                &format!("`{caller}`"),
            ));
        };
        let holder = LifetimePromisePayload::new(scope, handle, *self)?;
        let on_settled =
            Function::new_callback(scope, c"", 1, lifetime_promise_settled_cb, holder)?;
        // Step 5: Upon `fulfillment` or `rejection` of _promise_, `queue a microtask` to run these
        //     substeps:
        // A promise settles once, so exactly one handler runs, releasing the interest exactly once.
        // Like any reaction, these mark the lifetime promise handled, so its rejection is not
        // reported as unhandled.
        promise.add_reactions_ignoring_unhandled_rejection(
            scope,
            Some(*on_settled),
            Some(*on_settled),
        )?;
        // Step 4: Increment _event_’s `pending promises count` by one.
        //     Spec note: Note: The `pending promises count` is incremented even if the given
        //     promise has already been settled. The corresponding count decrement is done in the
        //     microtask queued by the reaction to the promise.
        // Last, so a failure above leaves the count matching the reactions actually attached: an
        // over-count would keep the event `active` forever.
        self.note_lifetime_promise_pending();
        Ok(())
    }

    /// Steps 1–2 of
    /// [add lifetime promise](https://w3c.github.io/ServiceWorker/#extendableevent-add-lifetime-promise),
    /// shared with `FetchEvent.respondWith`, whose step 4 is this algorithm but which inlines the
    /// rest of it.
    pub(crate) fn check_can_extend_lifetime(
        &self,
        scope: &Scope,
        caller: &str,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _event_’s `isTrusted` attribute is false, `throw` an "`InvalidStateError`"
        //     `DOMException`.
        if !self.is_trusted() {
            return Err(DOMExceptionError::new(
                "InvalidStateError",
                format!(
                    "{caller} can only be called on a trusted event, not on one constructed and \
                     dispatched by script"
                ),
            )
            .throw(scope));
        }
        // Step 2: If _event_ is not `active`, `throw` an "`InvalidStateError`" `DOMException`.
        //     Spec note: Note: If no lifetime extension promise has been added in the task that
        //     called the event handlers, calling `waitUntil()` in subsequent asynchronous tasks
        //     will throw.
        if !self.is_active() {
            return Err(DOMExceptionError::new(
                "InvalidStateError",
                format!(
                    "{caller} must be called while the event is active: during its dispatch, or \
                     while an earlier lifetime promise is still pending"
                ),
            )
            .throw(scope));
        }
        Ok(())
    }

    /// Count one more pending lifetime promise, per `add lifetime promise` step 4.
    pub(crate) fn note_lifetime_promise_pending(&self) {
        debug_assert!(self.data().pending_promises_count < usize::MAX);
        self.data_mut().pending_promises_count += 1;
    }

    /// `add lifetime promise` step 5.1: Decrement _event_’s `pending promises count` by one.
    pub(crate) fn note_lifetime_promise_settled(&self) {
        debug_assert!(self.data().pending_promises_count > 0);
        self.data_mut().pending_promises_count -= 1;
    }
}

/// A lifetime promise settled: drop the handle acquired in [`add_lifetime_promise`], letting the
/// acquiring request's event loop finish once nothing else is pending, and decrement the count that
/// keeps the event `active`.
fn lifetime_promise_settled_cb(
    scope: &Scope,
    _args: CallbackArgs,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let holder = cast_payload::<LifetimePromisePayload>(scope, payload);
    holder
        .data()
        .event
        .get(scope)
        .note_lifetime_promise_settled();
    drop(holder.data_mut().handle.take());
    Ok(value::undefined())
}

/// Reaction payload owning the loop-interest handle for one lifetime promise.
/// The settle reaction takes the handle, releasing the acquiring loop's
/// interest. If the promise never settles and the holder becomes garbage, the
/// impl's drop releases it too, so an unreachable lifetime promise cannot keep
/// its loop alive forever.
#[jsclass(hidden)]
struct LifetimePromisePayload {
    /// `None` once the settle reaction has released it: each `InterestHandle` answers for an
    /// increment, so a placeholder would underflow the loop's count when the holder is collected.
    #[no_trace]
    handle: Option<InterestHandle>,
    /// The event this promise extends, so settling can decrement its pending-promise count.
    event: Heap<ExtendableEventImpl>,
}

#[jsmethods]
impl LifetimePromisePayload {
    fn new(handle: InterestHandle, event: ExtendableEvent) -> Self {
        LifetimePromisePayloadImpl {
            handle: Some(handle),
            event: Heap::from(event),
        }
    }
}

pub fn add_to_global(scope: &Scope, global: Object) {
    ExtendableEvent::add_to_global(scope, global);
    LifetimePromisePayload::add_to_global(scope, global);
}
