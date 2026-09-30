// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The value stack of a component call, and its GC tracing.
//!
//! A [`CallStack`] holds the values of one component call: lifted values are
//! pushed onto it, and values to be lowered are popped off it. Other JS objects
//! don't reference these values, so the stack is traced as a GC root.
//!
//! Values to be lowered can also be pushed as [`TypeShapeValue`]s, which are
//! held separately (see [`CallStack::push_shaped`]).
//!
//! Each `CallStack` points to a heap-allocated interior, whose address is
//! registered in `LIVE_STACKS`. `trace_stack_values` traces the values of every
//! registered interior in every collection, including minor ones. Moving a
//! `CallStack` doesn't move the interior, and the interior's type is private,
//! so other code can't move it out of its allocation. The interior is held as a
//! raw pointer instead of a `Box`, so that accesses through the registered
//! pointer don't invalidate the stack's own pointer. Borrows are held in
//! [`RootedHeap`]s, which root themselves.

use std::alloc::Layout;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ptr::NonNull;

use js::conversion::FromJSVal;
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::gc::trace_value_root;
use js::native::{JSTracer, Value};
use js::prelude::HandleValue;

use crate::value::TypeShapeValue;

js::instance_local! {
    /// Every live [`CallStackInner`] in this runtime, by address. Entries are
    /// added in [`CallStack::new`] and removed in `CallStackInner::drop`. The
    /// interiors are heap-allocated and never moved, so a raw pointer remains
    /// valid for as long as its entry is present.
    static LIVE_STACKS: RefCell<HashSet<*mut CallStackInner>> =
        RefCell::new(HashSet::new());

    /// Emptied interiors kept for [`CallStack::new`] to reuse, at most
    /// [`MAX_FREE_STACKS`]. They are kept in [`LIVE_STACKS`] and hold nothing to trace.
    static FREE_STACKS: RefCell<FreeStacks> = RefCell::new(FreeStacks(Vec::new()));
}

/// The emptied interiors in [`FREE_STACKS`], freed when the instance-local is
/// destroyed, which happens at thread exit on native targets only.
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

/// The maximum number of emptied interiors kept in [`FREE_STACKS`].
const MAX_FREE_STACKS: usize = 16;

/// The maximum capacity, in elements, kept by each vector of an interior in [`FREE_STACKS`].
const MAX_KEPT_CAPACITY: usize = 64;

/// The mutable interior of a [`CallStack`], private so that external code cannot
/// move these fields out of the allocation the registry points at.
#[js::allow_unrooted_interior]
struct CallStackInner {
    /// Lifted and to-be-lowered JS values, traced as roots by [`trace_stack_values`]
    /// in every collection, so they need no write barriers.
    values: Vec<Value>,
    /// The values pushed by [`CallStack::push_shaped`]. Their JS values are held
    /// in `RootedHeap`s, which root themselves.
    shaped: Vec<TypeShapeValue>,
    /// The cursor of each list being lowered.
    iters: Vec<ListCursor>,
    /// The next index and the element count of each list being lifted. It is
    /// separate from `iters`, so that a lift nested in a lowering doesn't change
    /// `iters`.
    lift_iters: Vec<(usize, usize)>,
    /// The strings stored by [`CallStack::store_string`].
    strings: Vec<String>,
    /// The allocations that are freed when the stack is dropped (see
    /// [`CallStack::defer_deallocate`]).
    deferred: Vec<(*mut u8, Layout)>,
    /// The resource borrows of this call, released by
    /// `resources::release_borrows` when the call ends.
    borrows: Vec<Borrow>,
    /// The result of the export with this index, kept while it is lowered to
    /// describe a failure to lower it. Traced by [`trace_stack_values`].
    lowering_source: Option<(Value, usize)>,
    /// Whether a lift may take back the payload this stack lowers (see
    /// [`CallStack::set_adoptable`]).
    adoptable: bool,
}

/// The position in a list being lowered.
enum ListCursor {
    /// The index of the next element of the array on top of the JS values.
    Js(usize),
    /// The elements of a [`TypeShapeValue::List`] not yet pushed, in reverse
    /// order.
    Shaped(Vec<TypeShapeValue>),
}

/// One resource borrow recorded for the duration of a call, released by
/// `resources::release_borrows` when the call ends.
pub enum Borrow {
    /// A new wrapper of an imported resource lifted as a `borrow`.
    Imported {
        wrapper: RootedHeap<js::object::Object>,
    },
    /// A wrapper of an imported resource lowered as a `borrow`. Keeping it
    /// reachable until the call ends prevents its finalizer from dropping the
    /// handle during the call.
    Lent {
        wrapper: RootedHeap<js::object::Object>,
    },
    /// An exported resource lowered as a `borrow`, with a handle created for
    /// this call.
    Exported {
        object: RootedHeap<js::object::Object>,
        /// The resource type index, which selects the type's `drop`.
        type_idx: u32,
        /// The object's rep.
        rep: u32,
        /// The handle created for this call.
        handle: u32,
    },
}

impl Borrow {
    /// The object held by this borrow.
    pub(crate) fn heap(&self) -> &RootedHeap<js::object::Object> {
        match self {
            Borrow::Imported { wrapper } | Borrow::Lent { wrapper } => wrapper,
            Borrow::Exported { object, .. } => object,
        }
    }
}

/// The value stack of a component call (see the module documentation).
pub struct CallStack(NonNull<CallStackInner>);

impl CallStack {
    /// Create a stack, reusing an emptied interior if one is available.
    pub fn new() -> CallStack {
        if let Some(inner) = FREE_STACKS.with(|free| free.borrow_mut().0.pop()) {
            return CallStack(inner);
        }
        let inner = Box::new(CallStackInner {
            values: Vec::new(),
            shaped: Vec::new(),
            iters: Vec::new(),
            lift_iters: Vec::new(),
            strings: Vec::new(),
            deferred: Vec::new(),
            borrows: Vec::new(),
            lowering_source: None,
            adoptable: false,
        });
        let inner = NonNull::from(Box::leak(inner));
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

    /// Whether the stack holds strings, deferred allocations or borrows.
    pub fn holds_lowered_data(&self) -> bool {
        let inner = self.inner();
        !inner.strings.is_empty() || !inner.deferred.is_empty() || !inner.borrows.is_empty()
    }

    /// Push a JavaScript value onto the stack, which roots it.
    #[js::allow_unrooted]
    pub fn push_value(&mut self, value: Value) {
        self.inner_mut().values.push(value);
    }

    /// Push a JavaScript object onto the stack.
    pub fn push_object(&mut self, obj: &js::Object<'_>) {
        self.push_value(obj.as_value());
    }

    /// Pop the top JavaScript value off the stack. While the stack holds
    /// [`TypeShapeValue`]s, this pops the top one, which must be a
    /// [`TypeShapeValue::Js`], and returns its value.
    ///
    /// The returned [`Value`] is not rooted: the caller must root or otherwise
    /// consume it before the next allocation can trigger a GC.
    ///
    /// # Panics
    ///
    /// Panics if the stack is empty, or if the top [`TypeShapeValue`] is of
    /// another variant. Both indicate a bug in the calling convention rather
    /// than untrusted input.
    #[js::allow_unrooted]
    pub fn pop_value(&mut self) -> Value {
        let inner = self.inner_mut();
        match inner.shaped.pop() {
            // SAFETY: the value is returned unrooted, as documented.
            Some(TypeShapeValue::Js(value)) => unsafe { value.as_raw() },
            Some(_) => panic!("pop_value with a shaped value other than `Js` on top"),
            None => inner.values.pop().expect("pop from empty CallStack"),
        }
    }

    /// The value [`pop_value`](Self::pop_value) would return, without removing
    /// it.
    ///
    /// # Panics
    ///
    /// Panics where [`pop_value`](Self::pop_value) would.
    #[js::allow_unrooted]
    pub fn last(&self) -> Value {
        let inner = self.inner();
        match inner.shaped.last() {
            // SAFETY: the value is returned unrooted, as `pop_value` returns it.
            Some(TypeShapeValue::Js(value)) => unsafe { value.as_raw() },
            Some(_) => panic!("last with a shaped value other than `Js` on top"),
            None => *inner.values.last().expect("last on empty CallStack"),
        }
    }

    /// Push `value` for lowering. While the stack holds [`TypeShapeValue`]s,
    /// the lowering functions in [`crate::value`] and the list functions of
    /// this type pop them instead of JS values. A [`TypeShapeValue::Js`] on top
    /// is lowered from its JS value, which [`pop_value`](Self::pop_value) and
    /// [`last`](Self::last) return.
    pub fn push_shaped(&mut self, value: TypeShapeValue) {
        self.inner_mut().shaped.push(value);
    }

    /// Pop the top [`TypeShapeValue`]. Returns `None` if there is none, or if
    /// it is a [`TypeShapeValue::Js`], which is left on the stack.
    pub fn pop_shaped(&mut self) -> Option<TypeShapeValue> {
        let shaped = &mut self.inner_mut().shaped;
        match shaped.last()? {
            TypeShapeValue::Js(_) => None,
            _ => shaped.pop(),
        }
    }

    /// The value [`pop_shaped`](Self::pop_shaped) would return, without removing
    /// it.
    pub fn last_shaped(&self) -> Option<&TypeShapeValue> {
        match self.inner().shaped.last()? {
            TypeShapeValue::Js(_) => None,
            shaped => Some(shaped),
        }
    }

    /// Number of values currently on the stack, including [`TypeShapeValue`]s.
    pub fn len(&self) -> usize {
        let inner = self.inner();
        inner.values.len() + inner.shaped.len()
    }

    /// Whether the stack holds no values and no [`TypeShapeValue`]s.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Store `s`, and return a reference to it that is valid until the stack is
    /// dropped. If [`CallStack::set_adoptable`] was called, the string's
    /// capacity is reduced to its length.
    pub fn store_string(&mut self, mut s: String) -> &str {
        let inner = self.inner_mut();
        if inner.adoptable {
            s.shrink_to_fit();
        }
        inner.strings.push(s);
        inner.strings.last().expect("just pushed")
    }

    /// Allow a later lift to take over the memory of the value lowered on this
    /// stack. That lift takes ownership of the strings, and requires that each
    /// string's capacity equals its length, so the strings stored after this call
    /// get such a capacity. See [`CallStack::release_adopted`].
    pub fn set_adoptable(&mut self) {
        self.inner_mut().adoptable = true;
    }

    /// Forget the strings and deferred allocations of a value whose memory was
    /// taken over by a lift, without freeing them.
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

    /// Return the value kept by [`set_lowering_source`](Self::set_lowering_source),
    /// rooted on `scope`, and the index of its export.
    #[js::allow_unrooted]
    pub fn lowering_source<'s>(&self, scope: &'s Scope<'_>) -> Option<(HandleValue<'s>, usize)> {
        let (value, export) = self.inner().lowering_source?;
        Some((scope.root_value(value), export))
    }

    // --- Per-call borrow tracking ---

    /// Record `wrapper`, created for an imported resource lifted as a `borrow`.
    /// It is disposed of by `resources::release_borrows` when the call ends.
    pub fn push_imported_borrow(&mut self, wrapper: &js::Object<'_>) {
        self.inner_mut().borrows.push(Borrow::Imported {
            wrapper: RootedHeap::new(*wrapper),
        });
    }

    /// Record `wrapper`, a wrapper of an imported resource lowered as a
    /// `borrow`. It is kept reachable until `resources::release_borrows` runs.
    pub fn push_lent_wrapper(&mut self, wrapper: &js::Object<'_>) {
        self.inner_mut().borrows.push(Borrow::Lent {
            wrapper: RootedHeap::new(*wrapper),
        });
    }

    /// Record `object`, an exported resource lowered as a `borrow` with the
    /// handle `handle`. `resources::release_borrows` removes it from the slab and
    /// drops the handle when the call ends.
    pub fn push_exported_borrow(
        &mut self,
        object: &js::Object<'_>,
        type_idx: u32,
        rep: u32,
        handle: u32,
    ) {
        self.inner_mut().borrows.push(Borrow::Exported {
            object: RootedHeap::new(*object),
            type_idx,
            rep,
            handle,
        });
    }

    /// Pop the most recently recorded borrow, or `None` if none remain.
    pub fn pop_borrow(&mut self) -> Option<Borrow> {
        self.inner_mut().borrows.pop()
    }

    // --- List iterator protocol, matching wit-dylib-ffi's pop_list/next/iter ---

    /// Begin lowering the list on top of the stack, and return its length.
    ///
    /// A JS array stays on the stack until
    /// [`lower_list_end`](Self::lower_list_end). Throws a `TypeError` if the value
    /// isn't an array. A [`TypeShapeValue::List`] is popped, and its elements are
    /// kept in the list's cursor.
    ///
    /// # Panics
    ///
    /// Panics if the top [`TypeShapeValue`] isn't a [`TypeShapeValue::List`].
    pub fn lower_list_begin(&mut self, scope: &Scope<'_>) -> Result<usize, js::error::ExnThrown> {
        if let Some(shaped) = self.pop_shaped() {
            let TypeShapeValue::List(mut elements) = shaped else {
                panic!("lower_list_begin with a shaped value other than a list on top")
            };
            let len = elements.len();
            elements.reverse();
            self.inner_mut().iters.push(ListCursor::Shaped(elements));
            return Ok(len);
        }
        let list = self.list_being_lowered(scope)?;
        let len = list.length(scope)?;
        self.inner_mut().iters.push(ListCursor::Js(0));
        Ok(len as usize)
    }

    /// Push the next element of the list currently being lowered onto the stack.
    pub fn lower_list_next(&mut self, scope: &Scope<'_>) -> Result<(), js::error::ExnThrown> {
        let index = match self
            .inner_mut()
            .iters
            .last_mut()
            .expect("lower_list_next without begin")
        {
            ListCursor::Js(index) => *index,
            ListCursor::Shaped(remaining) => {
                let next = remaining
                    .pop()
                    .expect("lower_list_next past the list's end");
                self.push_shaped(next);
                return Ok(());
            }
        };
        let list = self.list_being_lowered(scope)?;
        let value = list.get_element(scope, index as u32)?;
        if let Some(ListCursor::Js(next)) = self.inner_mut().iters.last_mut() {
            *next = index + 1;
        }
        self.push_value(value.get());
        Ok(())
    }

    /// Finish lowering the current list: pop its cursor, and its array if it is
    /// a JS array.
    pub fn lower_list_end(&mut self) {
        let cursor = self
            .inner_mut()
            .iters
            .pop()
            .expect("lower_list_end without begin");
        if let ListCursor::Js(_) = cursor {
            self.pop_value();
        }
    }

    // --- List lifting cursor, matching wit-dylib-ffi's push_list/list_append ---

    /// Begin tracking the next index of a list of `capacity` elements that is
    /// being lifted. An empty list isn't tracked, because no elements are added
    /// to it.
    pub fn lift_list_begin(&mut self, capacity: usize) {
        if capacity > 0 {
            self.inner_mut().lift_iters.push((0, capacity));
        }
    }

    /// Return the index for the next element of the list being lifted. Tracking
    /// of the list ends after its last element.
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
        js::Array::from_jsval(scope, scope.root_value(self.last()), ())
            .map_err(|_| js::error::throw_type_error(scope, c"expected a list (array)"))
    }
}

impl Default for CallStack {
    fn default() -> Self {
        CallStack::new()
    }
}

impl CallStackInner {
    /// Shrink each vector's capacity to at most [`MAX_KEPT_CAPACITY`], so that a
    /// reused interior doesn't keep the capacity of the largest call that used
    /// it.
    fn shrink(&mut self) {
        self.values.shrink_to(MAX_KEPT_CAPACITY);
        self.shaped.shrink_to(MAX_KEPT_CAPACITY);
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
        self.shaped.clear();
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
        // On native targets, a stack can drop after the instance-locals are
        // destroyed at thread exit, so this uses `try_with`.
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
    // On native targets, a collection can run after the instance-locals are
    // destroyed at thread exit, so this uses `try_with`.
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
