// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`Event`](https://dom.spec.whatwg.org/#interface-event) interface.
//!
//! Implements the core DOM Event type following the WHATWG DOM Living Standard.

use bitflags::bitflags;
use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::class::Ref;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;

use crate::performance;

use super::event_target::EventTargetImpl;

bitflags! {
    #[derive(Debug, Default)]
    pub struct EventFlags: u16 {
        const STOP_PROPAGATION = 1 << 0;
        const STOP_IMMEDIATE_PROPAGATION = 1 << 1;
        const CANCELED = 1 << 2;
        const IN_PASSIVE_LISTENER = 1 << 3;
        const DISPATCH = 1 << 4;
        const INITIALIZED = 1 << 5;
        const BUBBLES = 1 << 6;
        const CANCELABLE = 1 << 7;
        const COMPOSED = 1 << 8;
        const TRUSTED = 1 << 9;
    }
}

#[webidl_interface]
pub struct Event {
    /// The event type (e.g. "click", "abort").
    #[no_trace]
    pub(crate) event_type: String,
    /// <https://dom.spec.whatwg.org/#dom-event-target>
    pub(crate) target: Option<Heap<EventTargetImpl>>,
    /// <https://dom.spec.whatwg.org/#dom-event-timestamp>
    #[no_trace]
    pub(crate) time_stamp: f64,

    // Internal flags per spec.
    #[no_trace]
    pub(crate) flags: EventFlags,
}

#[webidl_methods]
impl Event {
    // TODO: the macro should install these on `Event` itself, not `EventImpl`.
    pub const NONE: u16 = 0;
    pub const CAPTURING_PHASE: u16 = 1;
    pub const AT_TARGET: u16 = 2;
    pub const BUBBLING_PHASE: u16 = 3;

    /// <https://dom.spec.whatwg.org/#concept-event-constructor>
    #[constructor]
    pub fn new_untrusted(event_type: String, event_init_dict: Option<EventInit>) -> Self {
        let mut flags = EventFlags::INITIALIZED;
        if let Some(init) = event_init_dict {
            flags.set(EventFlags::BUBBLES, init.bubbles);
            flags.set(EventFlags::CANCELABLE, init.cancelable);
            flags.set(EventFlags::COMPOSED, init.composed);
        }
        Self {
            event_type,
            target: None,
            time_stamp: performance::coarse_now(),
            flags,
        }
    }

    /// Create a trusted event, treated as platform-, not content-created.
    ///
    /// This is essentially https://dom.spec.whatwg.org/#concept-event-create.
    pub fn new_trusted(event_type: String, event_init_dict: Option<EventInit>) -> Self {
        Self::new_untrusted(event_type, event_init_dict).trusted()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-type>
    #[getter(name = "type")]
    pub fn get_type(&self) -> Ref<'_, str> {
        Ref::map(self.data(), |data| data.event_type.as_str())
    }

    /// <https://dom.spec.whatwg.org/#dom-event-target>
    #[getter]
    pub fn target<'r>(&self, scope: &'r Scope<'_>) -> Option<super::event_target::EventTarget<'r>> {
        // Step 1: Return this's target.
        self.data().target.get(scope)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-srcelement>
    #[getter]
    pub fn src_element<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Option<super::event_target::EventTarget<'r>> {
        // Step 1: Return this's target.
        self.data().target.get(scope)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-currenttarget>
    #[getter]
    pub fn current_target<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Option<super::event_target::EventTarget<'r>> {
        // Since we don't implement event propagation, `currentTarget` is `target` during dispatch,
        // and `null` otherwise.
        if self.is_dispatching() {
            self.data().target.get(scope)
        } else {
            None
        }
    }

    /// <https://dom.spec.whatwg.org/#dom-event-eventphase>
    #[getter]
    pub fn event_phase(&self) -> u16 {
        // We only implement the NONE phase and AT_TARGET phase, so if we're dispatching, we're
        // AT_TARGET.
        if self.is_dispatching() {
            EventImpl::AT_TARGET
        } else {
            EventImpl::NONE
        }
    }

    /// <https://dom.spec.whatwg.org/#dom-event-stoppropagation>
    #[method]
    pub fn stop_propagation(&self) {
        // Step 1: Set this's stop propagation flag.
        self.data_mut().flags.insert(EventFlags::STOP_PROPAGATION);
    }

    pub fn is_propagation_stopped(&self) -> bool {
        self.data().flags.contains(EventFlags::STOP_PROPAGATION)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-cancelbubble>
    #[getter]
    fn cancel_bubble(&self) -> bool {
        // Step 1: Return true if this's stop propagation flag is set; otherwise false.
        self.is_propagation_stopped()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-cancelbubble>
    #[setter]
    fn set_cancel_bubble(&self, value: bool) {
        // Step 1: Set this's stop propagation flag if the given value is true; otherwise do
        //         nothing.
        if value {
            self.stop_propagation();
        }
    }

    /// <https://dom.spec.whatwg.org/#dom-event-stopimmediatepropagation>
    #[method]
    pub fn stop_immediate_propagation(&self) {
        // Step 1: Set this's stop propagation flag and this's stop immediate propagation flag.
        self.data_mut()
            .flags
            .insert(EventFlags::STOP_PROPAGATION | EventFlags::STOP_IMMEDIATE_PROPAGATION);
    }

    pub fn is_immediate_propagation_stopped(&self) -> bool {
        self.data()
            .flags
            .contains(EventFlags::STOP_IMMEDIATE_PROPAGATION)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-bubbles>
    #[getter]
    pub fn bubbles(&self) -> bool {
        self.data().flags.contains(EventFlags::BUBBLES)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-cancelable>
    #[getter]
    pub fn cancelable(&self) -> bool {
        self.data().flags.contains(EventFlags::CANCELABLE)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-returnvalue>
    #[getter]
    fn return_value(&self) -> bool {
        // Step 1: Return false if this's canceled flag is set; otherwise true.
        !self.is_canceled()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-returnvalue>
    #[setter]
    fn set_return_value(&self, value: bool) {
        // Step 1: Set the canceled flag with this if the given value is false; otherwise do
        //         nothing.
        if !value {
            self.cancel();
        }
    }

    /// <https://dom.spec.whatwg.org/#set-the-canceled-flag>
    ///
    /// To set the canceled flag, if event's cancelable attribute value is true
    /// and event's in passive listener flag is unset, then set event's canceled
    /// flag.
    pub fn cancel(&self) {
        let flags = &mut self.data_mut().flags;
        if flags.contains(EventFlags::CANCELABLE)
            && !flags.contains(EventFlags::IN_PASSIVE_LISTENER)
        {
            flags.insert(EventFlags::CANCELED);
        }
    }

    pub fn is_canceled(&self) -> bool {
        self.data().flags.contains(EventFlags::CANCELED)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-preventdefault>
    #[method]
    pub fn prevent_default(&self) {
        // Step 1: Set the canceled flag with this.
        self.cancel();
    }

    /// <https://dom.spec.whatwg.org/#dom-event-defaultprevented>
    #[getter]
    pub fn default_prevented(&self) -> bool {
        // Step 1: Return true if this's canceled flag is set; otherwise false.
        self.is_canceled()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-composed>
    #[getter]
    fn composed(&self) -> bool {
        // Step 1: Return true if this's composed flag is set; otherwise false.
        self.data().flags.contains(EventFlags::COMPOSED)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    ///
    /// `[LegacyUnforgeable]`: installed as an own, non-configurable accessor on
    /// each instance rather than on the prototype.
    #[getter(unforgeable)]
    pub fn is_trusted(&self) -> bool {
        self.data().flags.contains(EventFlags::TRUSTED)
    }

    /// <https://dom.spec.whatwg.org/#dom-event-timestamp>
    #[getter]
    pub fn time_stamp(&self) -> f64 {
        self.data().time_stamp
    }

    /// <https://dom.spec.whatwg.org/#dom-event-composedpath>
    ///
    /// Without shadow DOM support, this returns an empty array (no path entries
    /// to traverse).
    #[method]
    fn composed_path(&self) -> Vec<()> {
        // Step 1: Let _composedPath_ be an empty `list`.
        // Step 2: Let _path_ be `this`'s `path`.
        // Step 3: If _path_ `is empty`, then return _composedPath_.
        // Steps 4-16 involve shadow DOM path traversal — not applicable.
        // Step 17: Return _composedPath_.
        Vec::new()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-initevent>
    #[method]
    fn init_event(&self, event_type: String, bubbles: Option<bool>, cancelable: Option<bool>) {
        // Step 1: If `this`'s `dispatch flag` is set, then return.
        if self.data().flags.contains(EventFlags::DISPATCH) {
            return;
        }
        // Step 2: `Initialize` `this` with _type_, _bubbles_, and _cancelable_.
        self.data_mut().initialize(
            event_type,
            bubbles.unwrap_or(false),
            cancelable.unwrap_or(false),
        );
    }

    /// Whether the event's dispatch flag is set (it is currently being
    /// dispatched).
    pub fn is_dispatching(&self) -> bool {
        self.data().flags.contains(EventFlags::DISPATCH)
    }

    /// Start dispatching the event by setting its dispatch flag.
    ///
    /// Used in some tests, but not part of the stable public API.
    #[doc(hidden)]
    pub fn start_dispatching(&self) {
        debug_assert!(!self.data().flags.intersects(
            EventFlags::DISPATCH
                | EventFlags::STOP_PROPAGATION
                | EventFlags::STOP_IMMEDIATE_PROPAGATION,
        ));
        self.data_mut().flags.set(EventFlags::DISPATCH, true);
    }

    /// Stop dispatching the event by resetting its dispatch flag and propagation flags.
    ///
    /// Used in some tests, but not part of the stable public API.
    #[doc(hidden)]
    pub fn stop_dispatching(&self) {
        self.data_mut().flags.remove(
            EventFlags::DISPATCH
                | EventFlags::STOP_PROPAGATION
                | EventFlags::STOP_IMMEDIATE_PROPAGATION,
        );
    }

    /// Whether the event has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.data().flags.contains(EventFlags::INITIALIZED)
    }

    /// Whether the event is currently dispatching to a passive listener.
    pub fn in_passive_listener(&self) -> bool {
        self.data().flags.contains(EventFlags::IN_PASSIVE_LISTENER)
    }

    /// Set whether the event is currently dispatching to a passive listener.
    pub(crate) fn set_in_passive_listener(&self, value: bool) {
        debug_assert!(self.is_dispatching());
        self.data_mut()
            .flags
            .set(EventFlags::IN_PASSIVE_LISTENER, value);
    }
}

impl EventImpl {
    pub fn set_trusted(&mut self, trusted: bool) {
        self.flags.set(EventFlags::TRUSTED, trusted);
    }

    /// Marks `self` as trusted and returns it.
    #[js::allow_unrooted]
    pub(crate) fn trusted(mut self) -> Self {
        self.set_trusted(true);
        self
    }

    /// <https://dom.spec.whatwg.org/#concept-event-initialize>
    ///
    /// To initialize an event, with type, bubbles, and cancelable, run these steps:
    pub(crate) fn initialize(&mut self, event_type: String, bubbles: bool, cancelable: bool) {
        // Step 1: Set _event_'s `initialized flag`.
        self.flags.insert(EventFlags::INITIALIZED);
        // Step 2: Unset _event_'s `stop propagation flag`, `stop immediate propagation flag`, and
        //         `canceled flag`.
        self.flags.remove(
            EventFlags::STOP_PROPAGATION
                | EventFlags::STOP_IMMEDIATE_PROPAGATION
                | EventFlags::CANCELED,
        );
        // Step 3: Set _event_'s ``isTrusted`` attribute to false.
        // (Implicit)
        // Step 4: Set _event_'s `target` to null.
        self.target = None;
        // Step 5: Set _event_'s ``type`` attribute to _type_.
        self.event_type = event_type;
        // Step 6: Set _event_'s ``bubbles`` attribute to _bubbles_.
        self.flags.set(EventFlags::BUBBLES, bubbles);
        // Step 7: Set _event_'s ``cancelable`` attribute to _cancelable_.
        self.flags.set(EventFlags::CANCELABLE, cancelable);
    }
}

/// <https://dom.spec.whatwg.org/#dictdef-eventinit>
///
/// `Copy`, so an inherited dictionary reached by reference can hand its
/// `EventInit` on by value without cloning three bools.
#[derive(Default, Clone, Copy)]
#[webidl_dictionary]
pub struct EventInit {
    #[webidl(default = false)]
    pub bubbles: bool,
    #[webidl(default = false)]
    pub cancelable: bool,
    #[webidl(default = false)]
    pub composed: bool,
}
