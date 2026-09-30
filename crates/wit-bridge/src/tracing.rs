// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! GC rooting for the JS values held by this crate.
//!
//! The values on call stacks are traced by `stack::trace_stack_values`, which
//! is registered by [`install_tracer`]. The crate holds every other value in a
//! [`RootedHeap`](js::gc::handle::RootedHeap) in instance-local state.
//!
//! [`install_tracer`] must run before a call stack can hold a value across a
//! collection.

/// Register the call-stack value tracer with the current runtime's root
/// tracers.
pub fn install_tracer() {
    js::gc::add_root_tracer(crate::stack::trace_stack_values);
}

/// Drop every JS value held by this crate in instance-local state. Must be
/// called before the runtime is dropped.
pub fn clear_traced_roots() {
    crate::resources::clear_traced_roots();
    crate::exports::clear_traced_roots();
    crate::component_error::clear_traced_roots();
    #[cfg(target_arch = "wasm32")]
    crate::streams::clear_traced_roots();
}
