// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Per-invocation state for the event loop.
//!
//! An [`InvocationState`] bundles an [`EventLoop`] with per-invocation
//! metadata. Each incoming request, export call, or CLI script execution
//! gets its own `InvocationState`. The embedding layer (starling binary,
//! libstarling, componentize-js) decides how many invocations run
//! concurrently and schedules them.
//!
//! [`InvocationRegistry`] tracks all live `InvocationState` instances so
//! the GC can trace their event loops during garbage collection.

use js::native::JSTracer;

use crate::event_loop::EventLoop;

// ---------------------------------------------------------------------------
// InvocationState
// ---------------------------------------------------------------------------

/// Per-invocation state containing an event loop and associated metadata.
///
/// Each invocation (HTTP request, CLI script, WASIp3 export call) gets its
/// own `InvocationState`. The event loop tracks that invocation's tasks,
/// timers, and interest independently.
pub struct InvocationState {
    event_loop: EventLoop,
}

impl InvocationState {
    /// Create a new invocation with an empty event loop.
    pub fn new() -> Self {
        Self {
            event_loop: EventLoop::new(),
        }
    }

    /// Returns a shared reference to this invocation's event loop.
    pub fn event_loop(&self) -> &EventLoop {
        &self.event_loop
    }

    /// Trace all GC-managed objects in this invocation's event loop.
    ///
    /// # Safety
    ///
    /// `trc` must be a valid `JSTracer` pointer provided by SpiderMonkey.
    pub unsafe fn trace(&self, trc: *mut JSTracer) {
        self.event_loop.trace(trc);
    }
}

impl Default for InvocationState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// InvocationRegistry
// ---------------------------------------------------------------------------

/// Tracks live [`InvocationState`] instances for GC tracing.
///
/// The runtime holds an `InvocationRegistry` so the GC trace callback can
/// iterate over all invocations and trace their event loops. Registration
/// and unregistration are manual — the caller must ensure each registered
/// pointer remains valid until it is unregistered.
///
/// # Safety invariant
///
/// Every pointer in `invocations` must be valid for reads (specifically
/// for calling [`InvocationState::trace`]) whenever the GC trace callback
/// runs. Since GC tracing happens with JS execution paused, this is
/// satisfied as long as the `InvocationState` is alive and pinned in
/// memory for the duration of its registration.
pub struct InvocationRegistry {
    invocations: Vec<*const InvocationState>,
}

impl InvocationRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            invocations: Vec::new(),
        }
    }

    /// Register an invocation for GC tracing.
    ///
    /// # Safety
    ///
    /// The caller must ensure `state` remains valid and at a stable address
    /// until [`unregister`](Self::unregister) is called with the same pointer.
    pub unsafe fn register(&mut self, state: *const InvocationState) {
        debug_assert!(
            !self.invocations.contains(&state),
            "InvocationState registered twice"
        );
        self.invocations.push(state);
    }

    /// Unregister a previously registered invocation.
    ///
    /// Removes the pointer from the registry. Asserts that the pointer is currently registered.
    pub fn unregister(&mut self, state: *const InvocationState) {
        debug_assert!(
            self.invocations.contains(&state),
            "Unregistering InvocationState that was not registered or has been moved or already unregistered"
        );
        self.invocations.retain(|&p| p != state);
    }

    /// Returns `true` if no invocations are registered.
    pub fn is_empty(&self) -> bool {
        self.invocations.is_empty()
    }

    /// Returns the number of registered invocations.
    pub fn len(&self) -> usize {
        self.invocations.len()
    }

    /// Trace all registered invocations for GC.
    ///
    /// # Safety
    ///
    /// - `trc` must be a valid `JSTracer` pointer.
    /// - All registered pointers must be valid (see struct-level docs).
    pub unsafe fn trace(&self, trc: *mut JSTracer) {
        for &ptr in &self.invocations {
            (*ptr).trace(trc);
        }
    }
}

impl Default for InvocationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Keeps an [`InvocationState`] registered for GC tracing in a [`Runtime`],
/// unregistering when dropped.
///
/// [`Runtime`]: crate::runtime::Runtime
pub struct InvocationGuard<'rt> {
    runtime: &'rt crate::runtime::Runtime,
    state: *const InvocationState,
}

impl<'rt> InvocationGuard<'rt> {
    /// Register `state` for GC tracing until the guard drops.
    ///
    /// # Safety
    ///
    /// `state` must stay alive and at a stable address for the guard's whole
    /// lifetime. In an async fn, keep the guard and the state in the same
    /// frame and declare the guard after the state, so the guard drops
    /// first.
    pub unsafe fn new(runtime: &'rt crate::runtime::Runtime, state: &InvocationState) -> Self {
        runtime.register_invocation(state);
        Self {
            runtime,
            state: state as *const _,
        }
    }
}

impl Drop for InvocationGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: per `new`'s contract, the state outlives the guard.
        self.runtime.unregister_invocation(unsafe { &*self.state });
    }
}

/// An [`InvocationState`] owned together with its GC registration: the state is
/// boxed (its address is stable no matter where the handle moves) and stays
/// registered until the handle drops. Unlike [`InvocationGuard`], this is fully
/// safe and freely movable, e.g. into a spawned task that keeps driving the
/// loop after the request handler returned (a streamed response body's pump).
pub struct OwnedInvocation {
    runtime: std::rc::Rc<crate::runtime::Runtime>,
    state: Box<InvocationState>,
}

impl OwnedInvocation {
    /// Box `state` and register it for GC tracing until the handle drops.
    pub fn new(runtime: std::rc::Rc<crate::runtime::Runtime>, state: InvocationState) -> Self {
        let state = Box::new(state);
        // SAFETY: the boxed state's heap address is stable for the handle's
        // lifetime; `Drop` unregisters before the box is freed.
        unsafe { runtime.register_invocation(&state) };
        Self { runtime, state }
    }

    /// The invocation state (e.g. to drive its event loop).
    pub fn state(&self) -> &InvocationState {
        &self.state
    }
}

impl Drop for OwnedInvocation {
    fn drop(&mut self) {
        self.runtime.unregister_invocation(&self.state);
    }
}

// Nothing platform-specific in these tests, so skip them on wasm32.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn registry_register_unregister() {
        let mut registry = InvocationRegistry::new();
        assert!(registry.is_empty());

        let state1 = InvocationState::new();
        let state2 = InvocationState::new();

        unsafe {
            registry.register(&state1 as *const _);
            registry.register(&state2 as *const _);
        }
        assert_eq!(registry.len(), 2);

        registry.unregister(&state1 as *const _);
        assert_eq!(registry.len(), 1);

        registry.unregister(&state2 as *const _);
        assert!(registry.is_empty());
    }
}
