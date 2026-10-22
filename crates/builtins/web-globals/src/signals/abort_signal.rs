// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`AbortSignal`](https://dom.spec.whatwg.org/#interface-abortsignal) interface.
//!
//! An AbortSignal represents a signal that can communicate that an operation
//! should be aborted. It extends EventTarget so it can fire "abort" events.

use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::{ExceptionStackBehavior, Value};
use js::prelude::HandleValue;
use js::Object;

use super::algorithms;
use crate::dom_exception::DOMException;
use crate::events::algorithms as event_algorithms;
use crate::events::event_target::{EventTarget, EventTargetImpl};

/// An algorithm registered to run when the signal is aborted.
///
/// <https://dom.spec.whatwg.org/#abortsignal-abort-algorithms>
///
/// Algorithms are stored as data rather than as boxed closures, so that their GC pointers are
/// traced.
#[js::must_root]
pub(crate) enum AbortAlgorithm {
    /// Remove the event listener identified by `listener_id` from `held` (an
    /// EventTarget) when the signal aborts. Registered by "add an event listener".
    RemoveListener {
        /// The EventTarget whose listener is removed. Traced so it stays alive
        /// until then.
        held: Heap<EventTargetImpl>,
        /// Identity of the listener to remove.
        listener_id: u64,
    },
    /// Call `callback` with the signal's abort reason when the signal aborts.
    /// Registered by callers such as `fetch` that need to react to abort.
    RunSteps(Heap<js::function::Function>),
}

// Safety: `trc` must be the JS engine's `JSTracer` function.
unsafe impl js::heap::Trace for AbortAlgorithm {
    unsafe fn trace(&self, trc: *mut js::native::JSTracer) {
        match self {
            AbortAlgorithm::RemoveListener { held, .. } => held.trace(trc),
            AbortAlgorithm::RunSteps(callback) => callback.trace(trc),
        }
    }
}

/// <https://dom.spec.whatwg.org/#interface-abortsignal>
#[webidl_interface(extends = EventTarget)]
pub struct AbortSignal {
    parent: EventTargetImpl,

    /// <https://dom.spec.whatwg.org/#abortsignal-abort-reason>
    pub(crate) abort_reason: Heap<Value>,

    /// <https://dom.spec.whatwg.org/#abortsignal-dependent>
    #[no_trace]
    pub(crate) dependent: bool,

    /// <https://dom.spec.whatwg.org/#abortsignal-source-signals>
    pub(crate) source_signals: Vec<Heap<AbortSignalImpl>>,

    /// <https://dom.spec.whatwg.org/#abortsignal-dependent-signals>
    pub(crate) dependent_signals: Vec<Heap<AbortSignalImpl>>,

    /// <https://dom.spec.whatwg.org/#abortsignal-abort-algorithms>
    pub(crate) abort_algorithms: Vec<AbortAlgorithm>,
}

#[webidl_methods]
impl AbortSignal {
    fn new() -> Self {
        AbortSignalImpl {
            parent: EventTargetImpl::default(),
            abort_reason: Heap::default(),
            dependent: false,
            source_signals: Vec::new(),
            dependent_signals: Vec::new(),
            abort_algorithms: Vec::new(),
        }
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-aborted>
    #[getter]
    pub fn aborted(&self) -> bool {
        // Step 1: Return true if this is aborted; otherwise false.
        !self.data().abort_reason.is_undefined()
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-reason>
    #[getter]
    pub fn reason<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        // Step 1: Return this's abort reason.
        self.data().abort_reason.get(scope)
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-throwifaborted>
    #[method]
    fn throw_if_aborted(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Throw this's abort reason, if this is aborted.
        if !self.data().abort_reason.is_undefined() {
            let reason = self.data().abort_reason.get(scope);
            return Err(js::exception::set_pending(
                scope,
                reason,
                ExceptionStackBehavior::DoNotCapture,
            ));
        }
        Ok(())
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-onabort>
    ///
    /// IDL event handler attribute: `attribute EventHandler onabort;`
    #[getter]
    fn onabort<'r>(&self, scope: &'r Scope<'_>) -> Option<Object<'r>> {
        event_algorithms::event_handler(scope, self, "abort")
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-onabort>
    #[setter]
    fn set_onabort(&self, scope: &Scope<'_>, val: HandleValue<'_>) {
        event_algorithms::set_event_handler(scope, self, "abort", val);
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-abort>
    #[static_method]
    pub fn abort<'r>(
        scope: &'r Scope<'_>,
        reason: Option<HandleValue<'_>>,
    ) -> Result<AbortSignal<'r>, ExnThrown> {
        // Step 1: Let _signal_ be a new ``AbortSignal`` object.
        let signal = AbortSignal::new(scope)?;

        // Step 2: Set _signal_'s `abort reason` to _reason_ if it is given; otherwise to a new
        //         "``AbortError``" ``DOMException``.
        match reason {
            Some(r) => signal.data_mut().abort_reason.set(r.get()),
            None => {
                let err_val = DOMException::new(
                    scope,
                    "signal is aborted without reason".into(),
                    "AbortError".into(),
                )?;
                signal.data_mut().abort_reason.set(err_val.as_value());
            }
        }

        // Step 3: Return _signal_.
        Ok(signal)
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-timeout>
    #[static_method]
    pub fn timeout<'r>(
        scope: &'r Scope<'_>,
        milliseconds: f64,
    ) -> Result<AbortSignal<'r>, ExnThrown> {
        // Step 1: Let _signal_ be a new ``AbortSignal`` object.
        let signal = AbortSignal::new(scope)?;

        // Step 2: Let _global_ be _signal_'s `relevant global object`.
        // Step 3: `Run steps after a timeout` given _global_, "`AbortSignal-timeout`",
        //         _milliseconds_, and the following step: `Queue a global task` on the `timer task
        //         source` given _global_ to `signal abort` given _signal_ and a new
        //         "``TimeoutError``" ``DOMException``. For the duration of this timeout, if
        //         _signal_ has any event listeners registered for its ``abort`` event, there must
        //         be a strong reference from _global_ to _signal_.
        algorithms::schedule_abort_timeout(&signal, milliseconds);

        // Step 4: Return _signal_.
        Ok(signal)
    }

    /// <https://dom.spec.whatwg.org/#dom-abortsignal-any>
    #[static_method]
    pub fn any<'r>(
        scope: &'r Scope<'_>,
        // TODO: use a WebIDL sequence for this, after making that usable outside of unions.
        signals: HandleValue<'_>,
    ) -> Result<AbortSignal<'r>, ExnThrown> {
        let signals = Vec::<AbortSignal>::from_jsval_throwing(scope, signals, ())?;

        // Step 1: Return the result of creating a dependent abort signal from signals using
        //         AbortSignal and the current realm.
        algorithms::create_dependent_abort_signal(scope, &signals)
    }
}
