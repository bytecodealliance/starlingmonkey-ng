// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The interpreter's value stack and its GC integration.
//!
//! A [`CallStack`] is the scratch space for one component call: WIT values
//! lifted into JavaScript are pushed onto it, and JavaScript values to be lowered
//! back into WIT are popped off it. Those values are otherwise unreachable from
//! the JS object graph during a call, so the stack must be a GC root.
//!
//! Each live `CallStack` registers the address of its heap-allocated interior in
//! a thread-local registry. [`trace_stack_values`] walks the registry's values in
//! every collection, minor ones included, and the crate's extra-GC-roots tracer
//! walks its borrows (see [`crate::tracing`]).
//!
//! Since the registry stores a raw pointer, that address must stay valid for as
//! long as the stack is live. [`CallStack`] is therefore a newtype around a
//! pointer to a private, unnameable, heap-allocated [`CallStackInner`]: moving a
//! `CallStack` moves only the pointer, and external code can neither name the
//! interior type nor reach the private field to move it out from under the
//! registry. The interior is held as a raw pointer rather than a `Box` from its
//! allocation until it is freed, so the tracers' accesses through the registry
//! pointer stay valid alongside the stack's own accesses.

use std::alloc::Layout;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ptr::NonNull;

use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::gc::trace_value_root;
use js::heap::Trace;
use js::native::{JSTracer, Value};

js::instance_local! {
    /// Every live [`CallStackInner`] in this runtime, by address. Entries are
    /// added in [`CallStack::new`] and removed in `CallStackInner::drop`. The
    /// interiors are heap-allocated and never moved, so a raw pointer remains
    /// valid for as long as its entry is present.
    ///
    /// A set rather than a list, so dropping one of the many transient stacks a
    /// generic stream write creates is a hash lookup rather than a scan.
    static LIVE_STACKS: RefCell<HashSet<*mut CallStackInner>> =
        RefCell::new(HashSet::new());

    /// Emptied interiors kept for [`CallStack::new`] to reuse, at most
    /// [`MAX_FREE_STACKS`]. They stay in [`LIVE_STACKS`], holding nothing to trace.
    static FREE_STACKS: RefCell<FreeStacks> = RefCell::new(FreeStacks(Vec::new()));
}

/// The emptied interiors in [`FREE_STACKS`], freed when the thread-local is destroyed.
struct FreeStacks(Vec<NonNull<CallStackInner>>);

impl Drop for FreeStacks {
    fn drop(&mut self) {
        for inner in self.0.drain(..) {
            // SAFETY: each entry came from `CallStack::drop`, which gave up its only pointer to it.
            unsafe { free_interior(inner) };
        }
    }
}

/// Free an interior allocated by [`CallStack::new`], which removes it from [`LIVE_STACKS`].
///
/// # Safety
///
/// `inner` must come from [`CallStack::new`]'s `Box::into_raw`, and must not be used again.
unsafe fn free_interior(inner: NonNull<CallStackInner>) {
    // SAFETY: guaranteed by the caller.
    drop(unsafe { Box::from_raw(inner.as_ptr()) });
}

/// The most emptied interiors [`FREE_STACKS`] keeps.
const MAX_FREE_STACKS: usize = 16;

/// The most elements each vector of an interior in [`FREE_STACKS`] keeps capacity for.
const MAX_KEPT_CAPACITY: usize = 64;

/// The mutable interior of a [`CallStack`], private so that external code cannot
/// move these fields out of the allocation the registry points at.
#[js::allow_unrooted_interior]
struct CallStackInner {
    /// Lifted and to-be-lowered JS values, traced as roots by [`trace_stack_values`]
    /// in every collection, so they need no write barriers.
    values: Vec<Value>,
    /// Per-list cursor positions for the `lower_list_*` iterator protocol.
    iters: Vec<usize>,
    /// Per-list `(next index, element count)` for the list-lifting protocol, kept
    /// apart from `iters` so a lift nested inside a lower cannot disturb it.
    lift_iters: Vec<(usize, usize)>,
    /// Owned copies of strings popped off the JS stack, so a borrowed `&str` view
    /// stays valid for the duration of a call.
    strings: Vec<String>,
    /// Allocations whose freeing is deferred until the stack drops, matching
    /// `wit_dylib_ffi`'s `defer_deallocate` contract.
    deferred: Vec<(*mut u8, Layout)>,
    /// Imported-resource wrappers borrowed for this call, released by
    /// `resources::release_borrows` when the call ends.
    borrows: Vec<Borrow>,
    /// The result of the export with this index, kept while it is lowered to
    /// describe a failure to lower it. Traced by [`trace_stack_values`].
    lowering_source: Option<(Value, usize)>,
    /// Whether a lift may take back the payload this stack lowers (see
    /// [`CallStack::set_adoptable`]).
    adoptable: bool,
}

/// One resource borrow recorded for the duration of a call, released by
/// `resources::release_borrows` when the call ends. The object is traced by
/// `trace_live_stacks` while the borrow sits on the stack.
#[js::allow_unrooted_interior]
pub enum Borrow {
    /// A host-owned resource lifted into a fresh wrapper for this call. The
    /// wrapper's drop callback releases the handle.
    Imported { wrapper: Heap<js::object::Object> },
    /// A host-owned resource's wrapper lent to the host for this call. It is
    /// kept reachable until the call ends, so its finalizer cannot drop the
    /// handle while the host holds the lend.
    Lent { wrapper: Heap<js::object::Object> },
    /// A guest-owned object lowered to a fresh canonical handle for this call.
    /// Both the handle and the slab entry backing it go away at call end.
    Exported {
        object: Heap<js::object::Object>,
        /// The resource type index, which selects the type's `drop`.
        type_idx: u32,
        /// The slab index the object was inserted at, which is the rep the
        /// handle was minted from.
        rep: u32,
        /// The canonical handle minted for this call.
        handle: u32,
    },
}

impl Borrow {
    /// The JS object this borrow holds, whichever kind it is.
    pub(crate) fn heap(&self) -> &Heap<js::object::Object> {
        match self {
            Borrow::Imported { wrapper } | Borrow::Lent { wrapper } => wrapper,
            Borrow::Exported { object, .. } => object,
        }
    }
}

/// The per-call value stack, plus the bookkeeping the list iteration protocol and
/// deferred-deallocation contract need. The heap-allocated interior keeps its
/// GC-registered address fixed however the `CallStack` itself is moved (see the
/// module docs).
pub struct CallStack(NonNull<CallStackInner>);

impl CallStack {
    /// Create a GC-registered call stack, reusing an emptied interior when one is
    /// available. The registry entry is removed when the interior drops.
    pub fn new() -> CallStack {
        if let Some(inner) = FREE_STACKS.with(|free| free.borrow_mut().0.pop()) {
            return CallStack(inner);
        }
        let inner = Box::new(CallStackInner {
            values: Vec::new(),
            iters: Vec::new(),
            lift_iters: Vec::new(),
            strings: Vec::new(),
            deferred: Vec::new(),
            borrows: Vec::new(),
            lowering_source: None,
            adoptable: false,
        });
        // SAFETY: `Box::into_raw` never returns null.
        let inner = unsafe { NonNull::new_unchecked(Box::into_raw(inner)) };
        LIVE_STACKS.with(|stacks| stacks.borrow_mut().insert(inner.as_ptr()));
        CallStack(inner)
    }

    /// The interior, for the duration of a shared borrow of the stack.
    fn inner(&self) -> &CallStackInner {
        // SAFETY: the interior lives until `drop`, and the tracers only access it while JS
        // execution is paused, when no borrow of it is in use.
        unsafe { self.0.as_ref() }
    }

    /// The interior, for the duration of a mutable borrow of the stack.
    fn inner_mut(&mut self) -> &mut CallStackInner {
        // SAFETY: as for `inner`, and `&mut self` excludes every other borrow through `self`.
        unsafe { self.0.as_mut() }
    }

    /// Whether the stack holds anything a lowered canonical payload can point into
    /// or a later release needs: owned strings, deferred allocations or borrows.
    pub fn holds_lowered_data(&self) -> bool {
        let inner = self.inner();
        !inner.strings.is_empty() || !inner.deferred.is_empty() || !inner.borrows.is_empty()
    }

    /// Push a JavaScript value onto the stack, which roots it.
    #[js::allow_unrooted]
    pub fn push_value(&mut self, value: Value) {
        self.inner_mut().values.push(value);
    }

    /// Push a JavaScript object onto the stack as an object-typed value. Takes an
    /// `Object` or any type that derefs to one, such as `Array`.
    pub fn push_object(&mut self, obj: &js::Object<'_>) {
        self.push_value(obj.as_value());
    }

    /// Pop the top JavaScript value off the stack.
    ///
    /// The returned [`Value`] is not rooted: the caller must root or otherwise
    /// consume it before the next allocation can trigger a GC.
    ///
    /// # Panics
    ///
    /// Panics if the stack is empty, which indicates a bug in the calling
    /// convention rather than untrusted input.
    #[js::allow_unrooted]
    pub fn pop_value(&mut self) -> Value {
        self.inner_mut()
            .values
            .pop()
            .expect("pop from empty CallStack")
    }

    /// The top value without removing it.
    ///
    /// # Panics
    ///
    /// Panics if the stack is empty.
    #[js::allow_unrooted]
    pub fn last(&self) -> Value {
        *self.inner().values.last().expect("last on empty CallStack")
    }

    /// Number of values currently on the stack.
    pub fn len(&self) -> usize {
        self.inner().values.len()
    }

    /// Whether the value stack is empty.
    pub fn is_empty(&self) -> bool {
        self.inner().values.is_empty()
    }

    /// Store an owned string and return a borrow valid until this stack drops.
    ///
    /// `wit-dylib-ffi`'s `pop_string` hands back a `&str` borrowed for the
    /// duration of the call, while the value-conversion core produces owned
    /// `String`s.
    pub fn store_string(&mut self, mut s: String) -> &str {
        let inner = self.inner_mut();
        if inner.adoptable {
            s.shrink_to_fit();
        }
        inner.strings.push(s);
        inner.strings.last().expect("just pushed")
    }

    /// Mark the payload this stack lowers as one a later lift may take back, as
    /// wit-bindgen does with a stream or future element it did not send. That
    /// lift takes ownership of the payload's strings, assuming each one's
    /// capacity equals its length, so the strings stored from here on get such a
    /// capacity. See [`CallStack::release_adopted`].
    pub fn set_adoptable(&mut self) {
        self.inner_mut().adoptable = true;
    }

    /// Give up the strings and deferred allocations of a payload a lift took
    /// back, which owns them now, without freeing them.
    pub fn release_adopted(&mut self) {
        let inner = self.inner_mut();
        for string in inner.strings.drain(..) {
            std::mem::forget(string);
        }
        inner.deferred.clear();
    }

    /// Defer freeing `ptr` (with `layout`) until this stack drops.
    ///
    /// # Safety
    ///
    /// `ptr` must have been allocated with the global allocator using exactly
    /// `layout`, and ownership of it is transferred here: it must not be used or
    /// freed elsewhere after this call.
    pub unsafe fn defer_deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        self.inner_mut().deferred.push((ptr, layout));
    }

    /// Keep `value`, the result of the export with index `export` that is about
    /// to be lowered, so a failure to lower it can be described.
    #[js::allow_unrooted]
    pub fn set_lowering_source(&mut self, value: Value, export: usize) {
        self.inner_mut().lowering_source = Some((value, export));
    }

    /// The value [`set_lowering_source`](Self::set_lowering_source) kept, and
    /// the index of its export. The value is not rooted: the caller must root it
    /// before the next allocation can trigger a GC.
    #[js::allow_unrooted]
    pub fn lowering_source(&self) -> Option<(Value, usize)> {
        self.inner().lowering_source
    }

    // --- Per-call borrow tracking ---

    /// Record an imported-resource wrapper borrowed for this call, to be released
    /// by `resources::release_borrows` when the call ends.
    pub fn push_imported_borrow(&mut self, wrapper: &js::Object<'_>) {
        self.inner_mut().borrows.push(Borrow::Imported {
            wrapper: Heap::from(*wrapper),
        });
    }

    /// Record a host-owned resource's wrapper lent to the host for this call, to
    /// be kept reachable until `resources::release_borrows` runs.
    pub fn push_lent_wrapper(&mut self, wrapper: &js::Object<'_>) {
        self.inner_mut().borrows.push(Borrow::Lent {
            wrapper: Heap::from(*wrapper),
        });
    }

    /// Record a guest-owned object lowered to a canonical handle for this call,
    /// so `resources::release_borrows` drops the handle and unpins the slab
    /// entry when the call ends.
    pub fn push_exported_borrow(
        &mut self,
        object: &js::Object<'_>,
        type_idx: u32,
        rep: u32,
        handle: u32,
    ) {
        self.inner_mut().borrows.push(Borrow::Exported {
            object: Heap::from(*object),
            type_idx,
            rep,
            handle,
        });
    }

    /// Pop the most recently recorded borrow, or `None` if none remain.
    ///
    /// Releasing one at a time, rather than draining all into a detached `Vec`,
    /// keeps the undrained borrows in the traced list, so a GC during release
    /// still sees and updates them.
    pub fn pop_borrow(&mut self) -> Option<Borrow> {
        self.inner_mut().borrows.pop()
    }

    // --- List iterator protocol, matching wit-dylib-ffi's pop_list/next/iter ---

    /// Begin lowering a list whose top-of-stack value is a JS array, returning the
    /// list's length. The array stays on the stack so
    /// [`lower_list_next`](Self::lower_list_next) can index it.
    ///
    /// A value that is not an array throws. `{}` or `new Map()` would otherwise
    /// read `length` as `undefined` and trip `Value::to_number`'s assertion.
    pub fn lower_list_begin(&mut self, scope: &Scope<'_>) -> Result<usize, js::error::ExnThrown> {
        let list = self.list_being_lowered(scope)?;
        let len = list.length(scope)?;
        self.inner_mut().iters.push(0);
        Ok(len as usize)
    }

    /// Push the next element of the list currently being lowered onto the stack.
    pub fn lower_list_next(&mut self, scope: &Scope<'_>) -> Result<(), js::error::ExnThrown> {
        let index = *self
            .inner()
            .iters
            .last()
            .expect("lower_list_next without begin");
        let list = self.list_being_lowered(scope)?;
        let value = list.get_element(scope, index as u32)?;
        *self.inner_mut().iters.last_mut().expect("iters non-empty") = index + 1;
        self.push_value(value.get());
        Ok(())
    }

    /// Finish lowering the current list: pop its cursor and its array value.
    pub fn lower_list_end(&mut self) {
        self.inner_mut()
            .iters
            .pop()
            .expect("lower_list_end without begin");
        self.pop_value();
    }

    // --- List lifting cursor, matching wit-dylib-ffi's push_list/list_append ---

    /// Open a cursor for a list being lifted, whose `capacity` elements the
    /// following `list_append` calls fill. A list of no elements gets no cursor,
    /// since no `list_append` follows it.
    pub fn lift_list_begin(&mut self, capacity: usize) {
        if capacity > 0 {
            self.inner_mut().lift_iters.push((0, capacity));
        }
    }

    /// The index the next lifted element goes at, closing the cursor once the
    /// last element of the list has been placed.
    ///
    /// # Panics
    ///
    /// Panics if no cursor is open, which indicates a bug in the calling
    /// convention rather than untrusted input.
    pub fn lift_list_next(&mut self) -> usize {
        let lift_iters = &mut self.inner_mut().lift_iters;
        let (index, capacity) = lift_iters
            .last_mut()
            .expect("list_append without push_list");
        let at = *index;
        *index += 1;
        if *index == *capacity {
            lift_iters.pop();
        }
        at
    }

    /// The array on top of the stack, which is the list currently being lowered.
    fn list_being_lowered<'s>(
        &self,
        scope: &'s Scope<'_>,
    ) -> Result<js::Array<'s>, js::error::ExnThrown> {
        js::Object::from_value(scope, self.last())
            .ok()
            .and_then(|obj| obj.cast::<js::Array>().ok())
            .ok_or_else(|| js::error::throw_type_error(scope, c"expected a list (array)"))
    }
}

impl Default for CallStack {
    fn default() -> Self {
        CallStack::new()
    }
}

impl CallStackInner {
    /// Shrink each vector's capacity to at most [`MAX_KEPT_CAPACITY`], so a
    /// pooled interior doesn't keep the peak size of the largest call it served.
    fn shrink(&mut self) {
        self.values.shrink_to(MAX_KEPT_CAPACITY);
        self.iters.shrink_to(MAX_KEPT_CAPACITY);
        self.lift_iters.shrink_to(MAX_KEPT_CAPACITY);
        self.strings.shrink_to(MAX_KEPT_CAPACITY);
        self.borrows.shrink_to(MAX_KEPT_CAPACITY);
        self.deferred.shrink_to(MAX_KEPT_CAPACITY);
    }

    /// Drop every value and borrow, and free the deferred allocations, leaving the
    /// vectors empty with their capacity.
    fn clear(&mut self) {
        self.values.clear();
        self.iters.clear();
        self.lift_iters.clear();
        self.strings.clear();
        self.borrows.clear();
        self.lowering_source = None;
        self.adoptable = false;
        for (ptr, layout) in self.deferred.drain(..) {
            // SAFETY: each entry was registered via `defer_deallocate`, which
            // transfers ownership of a global-allocator allocation matching
            // `layout`. Nothing else frees it.
            unsafe {
                std::alloc::dealloc(ptr, layout);
            }
        }
    }
}

impl Drop for CallStack {
    fn drop(&mut self) {
        self.inner_mut().clear();
        let inner = self.0;
        // `try_with`, since a stack can drop during thread-local teardown.
        let kept = FREE_STACKS
            .try_with(|free| {
                let mut free = free.borrow_mut();
                let keep = free.0.len() < MAX_FREE_STACKS;
                if keep {
                    // SAFETY: `inner` came from `CallStack::new`, and `self` no longer uses it.
                    unsafe { (*inner.as_ptr()).shrink() };
                    free.0.push(inner);
                }
                keep
            })
            .unwrap_or(false);
        if !kept {
            // SAFETY: `inner` came from `CallStack::new`, and `self` is not used afterwards.
            unsafe { free_interior(inner) };
        }
    }
}

impl Drop for CallStackInner {
    fn drop(&mut self) {
        // `self` is the interior at exactly the address registered in
        // `CallStack::new`, and this runs before the allocation is freed.
        let ptr: *mut CallStackInner = self;
        let _ = LIVE_STACKS.try_with(|stacks| stacks.borrow_mut().remove(&ptr));
        self.clear();
    }
}

/// Trace every value on every live call stack. Registered with
/// [`js::gc::add_root_tracer`] by [`crate::tracing::install_tracer`].
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run with JS execution paused.
pub(crate) unsafe fn trace_stack_values(trc: *mut JSTracer) {
    // `try_with`, since a collection can run during thread-local teardown.
    let _ = LIVE_STACKS.try_with(|stacks| {
        // SAFETY: GC runs with JS execution paused, so no code mutates the registry or the
        // stacks while they are traced, and every registered interior is alive.
        unsafe {
            for &inner in &*stacks.as_ptr() {
                for value in &mut (*inner).values {
                    trace_value_root(trc, value);
                }
                if let Some((value, _)) = &mut (*inner).lowering_source {
                    trace_value_root(trc, value);
                }
            }
        }
    });
}

/// Trace the borrowed resource objects of every live call stack.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_live_stacks(trc: *mut JSTracer) {
    // `try_with`, since a collection can run during thread-local teardown.
    let _ = LIVE_STACKS.try_with(|stacks| {
        let stacks = &*stacks.as_ptr();
        for &inner in stacks {
            for borrow in &(*inner).borrows {
                borrow.heap().trace(trc);
            }
        }
    });
}
