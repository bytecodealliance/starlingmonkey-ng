// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The canonical-ABI layer of the Component Model `stream<T>`/`future<T>` bridge
//! (wasm-only).
//!
//! [`crate::readable`] presents a `stream<T>` to JS as a `ReadableStream` and
//! [`crate::promises`] a `future<T>` as a `Promise`. Both read and write through
//! wit-bindgen's async [`StreamReader`], [`StreamWriter`], [`FutureReader`] and
//! [`FutureWriter`], which run on the same wit-bindgen executor as the async
//! exports and imports, using the vtables and element conversions defined here.
//!
//! # The per-type vtable registry
//!
//! wit-bindgen's reader and writer are generic over the element type `T`, driven
//! by a `&'static StreamVtable<T>`/`FutureVtable<T>` built from the wit-dylib
//! `Stream`/`Future` metadata fn-pointers. The metadata differs per type index,
//! so the vtable is built and leaked on first use and cached by type index.
//!
//! Two element strategies coexist. `stream<u8>` takes a fast path where `lift`
//! and `lower` are `None`, since `u8`'s in-memory Rust layout is its canonical
//! ABI layout and the `Vec<u8>` wit-bindgen reads and writes is already
//! canonical. Every other element type takes the generic path, carrying each
//! element as a GC-rooted [`JsElem`] and routing it through the metadata's own
//! `lift`/`lower` fn-pointers against a transient
//! [`CallCx`](crate::interpreter::CallCx). See [`elem_lift`] and [`elem_lower`].

#![cfg(target_arch = "wasm32")]

use std::alloc::Layout;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::{JSTracer, Value};
use js::Object;

use wit_bindgen::rt::async_support::{FutureVtable, FutureWriter, StreamVtable};
use wit_dylib_ffi::{Future as WitFuture, Stream as WitStream, Type};

use crate::init;
use crate::interpreter::{trap, CallCx};
use crate::resources::{read_u32_field, DISPOSE_FIELD, HANDLE_FIELD};
use crate::stack::CallStack;

js::instance_local! {
    /// Every live [`JsElem`]'s boxed `Heap<Value>` interior, by address, walked
    /// by [`trace_live_elems`] on every GC.
    ///
    /// A `JsElem` may sit in a `Vec<JsElem>` owned by a wit-bindgen reader or
    /// writer across an `await`, which is not reachable from the JS object graph.
    /// Boxing the `Heap<Value>` keeps the registered address valid even when the
    /// `JsElem` is moved.
    ///
    /// A set rather than a list, so removing one of a write's N elements is
    /// a hash lookup rather than a scan.
    static LIVE_ELEMS: RefCell<std::collections::HashSet<*const Heap<Value>>> =
        RefCell::new(std::collections::HashSet::new());
}

/// A generic stream/future element: one JS value, GC-rooted for as long as it is
/// held by a wit-bindgen reader or writer buffer. This is the carrier `T` for the
/// generic `StreamVtable` and `FutureVtable`.
pub(crate) struct JsElem {
    value: Box<Heap<Value>>,
}

impl JsElem {
    /// Build an element from a raw JS value, boxing it into a traced `Heap`
    /// immediately, so the unrooted argument never outlives this call.
    #[js::allow_unrooted]
    pub(crate) fn new(value: Value) -> JsElem {
        let boxed = Box::new(Heap::from(value));
        let ptr: *const Heap<Value> = &*boxed;
        // `try_with`, since at thread-local teardown wit-bindgen's
        // `FutureWriter::Drop` can construct the placeholder default after
        // `LIVE_ELEMS` is destroyed. Tracing is moot then.
        let _ = LIVE_ELEMS.try_with(|elems| elems.borrow_mut().insert(ptr));
        JsElem { value: boxed }
    }

    /// Read the carried value for immediate consumption.
    ///
    /// # Safety
    ///
    /// The returned [`Value`] is not separately rooted, so the caller must
    /// consume or root it before the next allocation can trigger a GC.
    pub(crate) unsafe fn get(&self) -> Value {
        // SAFETY: read for immediate consumption. The backing `Heap` stays
        // registered and traced, so no GC can invalidate it before use.
        unsafe { self.value.as_raw() }
    }
}

impl Drop for JsElem {
    fn drop(&mut self) {
        // `self.value` is the boxed interior at the address registered in
        // `JsElem::new`, and this runs before the box is freed.
        let ptr: *const Heap<Value> = &*self.value;
        // `try_with` so a drop during thread-local teardown cannot touch a
        // destroyed `LIVE_ELEMS`.
        let _ = LIVE_ELEMS.try_with(|elems| elems.borrow_mut().remove(&ptr));
    }
}

/// Trace every value held by every live [`JsElem`], and every wrapper waiting
/// in [`LOWERED_ELEM_ORIGINALS`].
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_live_elems(trc: *mut JSTracer) {
    LIVE_ELEMS.with(|elems| {
        // `as_ptr` bypasses RefCell borrow tracking: GC runs with JS execution
        // paused.
        let elems = &*elems.as_ptr();
        for &heap in elems {
            (*heap).trace(trc);
        }
    });
    LOWERED_ELEM_ORIGINALS.with(|originals| {
        let originals = &*originals.as_ptr();
        for heap in originals.values() {
            heap.trace(trc);
        }
    });
}

/// Drop the wrappers in [`LOWERED_ELEM_ORIGINALS`]. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    LOWERED_ELEM_ORIGINALS.with(|cell| cell.borrow_mut().clear());
}

js::instance_local! {
    /// The imported-resource wrapper each in-flight lowered element came from,
    /// keyed by the canonical payload address [`elem_lower`] wrote, so a re-lift
    /// of an unsent element can hand the handle back to the object the guest
    /// still holds. An entry is removed by [`elem_dealloc`] once the element was
    /// sent, or by [`elem_lift`] when it hands the handle back.
    static LOWERED_ELEM_ORIGINALS: RefCell<HashMap<*mut u8, Heap<js::object::Object>>> =
        RefCell::new(HashMap::new());
}

/// Whether `value` is a live imported-resource wrapper: an object with the
/// drop callback and a handle. An exported (guest-owned) resource object has no
/// drop callback, and `push_own` hands that same object back on a re-lift.
fn imported_wrapper<'s>(scope: &'s Scope<'_>, value: Value) -> Option<Object<'s>> {
    let obj = Object::from_value(scope, value).ok()?;
    let dispose = obj.get_property(scope, DISPOSE_FIELD).ok()?;
    let handle = obj.get_property(scope, HANDLE_FIELD).ok()?;
    (dispose.get().is_object() && handle.get().is_number()).then_some(obj)
}

/// Move the canonical handle a re-lift minted `fresh` for back onto `original`,
/// the wrapper the guest lowered, so the guest's object is live again and the
/// fresh wrapper can neither drop the handle nor be used.
fn hand_handle_back(
    scope: &Scope<'_>,
    original: &Object<'_>,
    fresh: &Object<'_>,
) -> Result<(), ExnThrown> {
    let Some(handle) = read_u32_field(scope, fresh, HANDLE_FIELD)? else {
        return Ok(());
    };
    crate::resources::unregister_resource(scope, fresh)?;
    fresh.delete_property(scope, HANDLE_FIELD)?;
    crate::resources::register_resource(scope, original, handle)
}

js::instance_local! {
    /// The wit-dylib `Stream`/`Future` metadata for the element conversion
    /// currently in flight, read by [`elem_lift`] and [`elem_lower`], which are
    /// bare fns and cannot capture it. One active type per nesting level suffices,
    /// since a write lowers one type's elements and a read lifts one type's.
    static ELEM_TYPE: RefCell<Vec<ElemType>> = const { RefCell::new(Vec::new()) };
}

/// The wit-dylib metadata driving the in-flight element conversion.
#[derive(Clone, Copy)]
pub(crate) enum ElemType {
    Stream(WitStream),
    Future(WitFuture),
}

impl ElemType {
    /// Whether an element of this type contains an owned resource handle.
    fn owns_resources(&self) -> bool {
        match self {
            ElemType::Stream(s) => {
                crate::interpreter::stream_element(s.index() as u32).owns_resources
            }
            ElemType::Future(f) => {
                crate::interpreter::future_payload(f.index() as u32).owns_resources
            }
        }
    }

    /// Lift one element from the canonical ABI buffer at `ptr` onto `cx`'s stack.
    ///
    /// # Safety
    ///
    /// `ptr` must point at one initialized canonical-ABI payload of this type.
    unsafe fn lift(&self, cx: &mut CallCx<'_>, ptr: *mut u8) {
        match self {
            ElemType::Stream(s) => unsafe { s.lift(cx, ptr) },
            ElemType::Future(f) => unsafe { f.lift(cx, ptr) },
        }
    }

    /// Lower one element off `cx`'s stack into the canonical ABI buffer at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must point at writable space for one canonical-ABI payload.
    unsafe fn lower(&self, cx: &mut CallCx<'_>, ptr: *mut u8) {
        match self {
            ElemType::Stream(s) => unsafe { s.lower(cx, ptr) },
            ElemType::Future(f) => unsafe { f.lower(cx, ptr) },
        }
    }
}

/// Run `f` with `ty` installed as the active element-conversion type, restoring
/// the previous one on the way out so nested conversions stay correct.
fn with_elem_type<R>(ty: ElemType, f: impl FnOnce() -> R) -> R {
    ELEM_TYPE.with(|cell| cell.borrow_mut().push(ty));
    let r = f();
    ELEM_TYPE.with(|cell| {
        cell.borrow_mut().pop().expect("element type was pushed");
    });
    r
}

/// A future adapter that installs an active element type for the duration of each
/// `poll` of the wrapped future, and while the wrapped future is dropped.
///
/// wit-bindgen invokes the vtable `lift` and `lower` synchronously inside the poll
/// that completes the canonical-ABI transfer, so the type must be set across the
/// poll rather than only while the future is constructed. Setting and clearing per
/// poll also keeps interleaved transfers of different element types correct.
/// Dropping an unfinished read or write cancels it, which lifts the elements
/// the cancelled transfer moved or left unsent.
pub(crate) struct WithElemType<F> {
    ty: ElemType,
    future: std::mem::ManuallyDrop<F>,
}

impl<F: std::future::Future> std::future::Future for WithElemType<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        // SAFETY: structural pin projection. `ty` is `Copy` and never moved, and
        // `future` is pinned along with `self`, which is the only way it is ever
        // accessed.
        let this = unsafe { self.get_unchecked_mut() };
        let ty = this.ty;
        let future = unsafe { std::pin::Pin::new_unchecked(&mut *this.future) };
        with_elem_type(ty, || future.poll(cx))
    }
}

impl<F> Drop for WithElemType<F> {
    fn drop(&mut self) {
        let ty = self.ty;
        // SAFETY: `future` is dropped in place exactly once, here, and never used
        // again. Dropping in place keeps the pinning guarantee.
        with_elem_type(ty, || unsafe {
            std::mem::ManuallyDrop::drop(&mut self.future)
        });
    }
}

/// Wrap `future` so the active element type is installed for each of its polls
/// and while it is dropped.
pub(crate) fn with_elem_type_future<F: std::future::Future>(
    ty: ElemType,
    future: F,
) -> WithElemType<F> {
    WithElemType {
        ty,
        future: std::mem::ManuallyDrop::new(future),
    }
}

/// The active element-conversion type, or `None` if `lift`/`lower` ran outside any
/// [`with_elem_type`] span. Every read and write runs in a [`WithElemType`], which
/// also covers the drop that cancels it. The one conversion outside a span is
/// the default value wit-bindgen writes when a `FutureWriter` is dropped
/// unwritten, which [`FUTURE_WRITERS`] prevents while the instance runs.
fn active_elem_type() -> Option<ElemType> {
    // `try_with`, since at thread-local teardown wit-bindgen's drop-default
    // lowering can run after `ELEM_TYPE` is destroyed.
    ELEM_TYPE
        .try_with(|cell| cell.borrow().last().copied())
        .ok()
        .flatten()
}

js::instance_local! {
    /// The per-element `CallStack` backing a lowered canonical payload, keyed by
    /// the payload's `dst` pointer. The wit-dylib lower for a `string`, and any
    /// composite containing one, writes `(ptr, len)` straight at the `pop_string`
    /// arena `&str` without copying, so the canonical buffer aliases the element
    /// stack's temp storage and the stack must outlive it.
    ///
    /// Each lowered `dst` is consumed by exactly one of two callbacks:
    /// [`elem_dealloc`] for an element that was sent, and [`elem_lift`] for one
    /// that was not and is being re-acquired. A `dst` is either advanced and then
    /// deallocated, or stays in the buffer and is re-lifted, so the two removal
    /// sites cannot double-free.
    static LOWERED_ELEM_STACKS: RefCell<HashMap<*mut u8, CallStack>> =
        RefCell::new(HashMap::new());
}

/// The vtable `lower` callback: lower one [`JsElem`] into the canonical ABI
/// buffer at `dst`, routing the element through the wit-dylib metadata's `lower`
/// and the `value.rs` conversion core.
///
/// The element stack is stashed in [`LOWERED_ELEM_STACKS`] keyed by `dst`, since
/// the lowered canonical payload may alias the stack's temp storage.
///
/// # Safety
///
/// `dst` must point at writable space for one canonical-ABI payload of the
/// active element type, and a [`with_elem_type`] span must be active.
unsafe fn elem_lower(value: JsElem, dst: *mut u8) {
    let Some(ty) = active_elem_type() else {
        // No active type (see `active_elem_type`). The value is dropped and `dst`
        // left as wit-bindgen allocated it.
        drop(value);
        return;
    };
    // A fresh transient stack holding just this one value, GC-traced via the
    // live-stacks registry. A lift of the element if it is not sent takes its
    // strings and lists.
    let mut stack = CallStack::new();
    stack.set_adoptable();
    // SAFETY: `value.get()` is read for immediate consumption by `push_value`,
    // which boxes it into a traced `Heap`. The `JsElem` is dropped right after,
    // before the lower below can allocate.
    stack.push_value(unsafe { value.get() });
    drop(value);
    // Lowering an owned imported resource strips the wrapper's handle. The
    // wrapper is kept so a re-lift of the unsent element restores it. Only an
    // element that is a wrapper itself is kept: one nested in a record, tuple,
    // option or list comes back from the re-lift as a new wrapper.
    if ty.owns_resources() {
        init::with_scope(|scope| {
            if let Some(original) = imported_wrapper(scope, stack.last()) {
                LOWERED_ELEM_ORIGINALS
                    .with(|cell| cell.borrow_mut().insert(dst, Heap::from(original)));
            }
        });
    }
    {
        let mut cx = CallCx::Borrowed(&mut stack);
        // SAFETY: `dst` is a valid canonical-ABI payload slot per the
        // precondition, and the active element type matches the buffer `dst` points
        // into.
        unsafe { ty.lower(&mut cx, dst) };
    }
    // A stack whose `strings` arena or deferred allocations may back `dst` is kept
    // alive until `dealloc_lists(dst)` frees it. Any other stack is dropped here.
    if stack.holds_lowered_data() {
        LOWERED_ELEM_STACKS.with(|cell| cell.borrow_mut().insert(dst, stack));
    }
}

/// The vtable `dealloc_lists` callback: free the element stack [`elem_lower`]
/// stashed for `dst` after the canonical payload there has been consumed by a
/// successful send/write. wit-bindgen calls this per element from
/// `AbiBuffer::advance` (stream) and on the `COMPLETED` write path (future).
///
/// # Safety
///
/// `dst` must be a `dst` a prior [`elem_lower`] wrote (or a no-op if not).
unsafe fn elem_dealloc(dst: *mut u8) {
    drop(LOWERED_ELEM_STACKS.with(|cell| cell.borrow_mut().remove(&dst)));
    drop(LOWERED_ELEM_ORIGINALS.with(|cell| cell.borrow_mut().remove(&dst)));
}

/// Remove `key`'s entry from `map`, without searching a map that is empty, as
/// both maps are whenever no lowered element waits to be sent.
fn remove_entry<V>(map: &RefCell<HashMap<*mut u8, V>>, key: *mut u8) -> Option<V> {
    let mut map = map.borrow_mut();
    if map.is_empty() {
        None
    } else {
        map.remove(&key)
    }
}

/// The vtable `lift` callback: lift one element from the canonical ABI buffer at
/// `src` into a [`JsElem`], routing through the wit-dylib metadata's `lift` and
/// the `value.rs` conversion core.
///
/// # Safety
///
/// `src` must point at one initialized canonical-ABI payload of the active
/// element type, and a [`with_elem_type`] span must be active.
unsafe fn elem_lift(src: *mut u8) -> JsElem {
    let Some(ty) = active_elem_type() else {
        // No active type (see `active_elem_type`).
        return JsElem::new(js::value::undefined());
    };
    let mut stack = CallStack::new();
    {
        let mut cx = CallCx::Borrowed(&mut stack);
        // SAFETY: `src` is a valid initialized payload per the precondition, and
        // the active element type matches the buffer `src` points into. The
        // metadata `lift` pushes one JS value onto the stack.
        unsafe { ty.lift(&mut cx, src) };
    }
    // The pop is read for immediate consumption by `JsElem::new`, which boxes it
    // into a traced, registered `Heap` before the next allocation can GC.
    let mut elem = JsElem::new(stack.pop_value());
    // Re-lifting `src` means this element was not sent, so the element stack a
    // prior `elem_lower` stashed for the same `dst` will never see
    // `dealloc_lists`. The lift took ownership of the payload's strings and
    // lists, so the stack gives them up rather than freeing them. A fresh-read
    // `src` has no entry.
    if let Some(mut stack) = LOWERED_ELEM_STACKS.with(|cell| remove_entry(cell, src)) {
        stack.release_adopted();
    }
    // An unsent imported resource lifts to a fresh wrapper. The guest still
    // holds the wrapper it wrote, so the handle goes back onto that one and the
    // element becomes it.
    let original = LOWERED_ELEM_ORIGINALS.with(|cell| remove_entry(cell, src));
    if let Some(original) = original {
        init::with_scope(|scope| {
            let original = original.get(scope);
            // SAFETY: read for immediate consumption while `elem` stays rooted.
            if let Ok(fresh) = Object::from_value(scope, unsafe { elem.get() }) {
                if hand_handle_back(scope, &original, &fresh).is_err() {
                    trap(scope, "handing an unsent resource back to the guest")
                }
            }
            elem = JsElem::new(original.as_value());
        });
    }
    elem
}

// Every vtable below wires the wit-dylib `cancel-read`/`cancel-write` externs into
// wit-bindgen's `StreamVtable`/`FutureVtable`. wit-bindgen cancels an in-flight
// read or write when its operation future is dropped, then re-acquires the
// un-transferred value via `lift`.

/// Whether a [`PendingSend`](core_runtime::event_loop::PendingSend) of a
/// stream pump or a future write is done, and the task waiting for that.
#[derive(Clone, Default)]
pub(crate) struct SendDone(std::rc::Rc<RefCell<(bool, Option<std::task::Waker>)>>);

impl SendDone {
    /// Mark the send done, waking the task waiting for that.
    pub(crate) fn set(&self) {
        let waker = {
            let mut state = self.0.borrow_mut();
            state.0 = true;
            state.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Whether the send is done.
    pub(crate) fn is_set(&self) -> bool {
        self.0.borrow().0
    }

    /// Register a pending send on the active event loop that completes once
    /// this is set, and calls `abandon` once the loop has no other work left.
    /// Returns `false` if no event loop is active, and registers nothing then.
    pub(crate) fn register(&self, abandon: impl FnOnce() + 'static) -> bool {
        let state = std::rc::Rc::clone(&self.0);
        let done = std::future::poll_fn(move |cx| {
            let mut state = state.borrow_mut();
            if state.0 {
                std::task::Poll::Ready(())
            } else {
                state.1 = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        });
        let send = core_runtime::event_loop::PendingSend {
            done: Box::pin(done),
            abandon: Box::new(abandon),
        };
        core_runtime::event_loop::with_active_event_loop(|el| el.register_pending_send(send))
            .is_some()
    }
}

js::instance_local! {
    /// The next [`FUTURE_WRITERS`] id.
    static NEXT_FUTURE_WRITER_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

js::instance_local! {
    /// `&'static StreamVtable<u8>` per `stream<u8>` type index, built and leaked
    /// on first use. The leak is bounded at one entry per `stream<u8>` type in the
    /// world, borrowed for the `'static` lifetime of a reader or writer.
    static U8_STREAM_VTABLES: RefCell<HashMap<u32, &'static StreamVtable<u8>>> =
        RefCell::new(HashMap::new());

    /// `&'static StreamVtable<JsElem>` per generic (non-`u8`) stream type index.
    static GENERIC_STREAM_VTABLES: RefCell<HashMap<u32, &'static StreamVtable<JsElem>>> =
        RefCell::new(HashMap::new());

    /// `&'static FutureVtable<JsElem>` per future type index. The future path is
    /// always generic, with no fast path.
    static FUTURE_VTABLES: RefCell<HashMap<u32, &'static FutureVtable<JsElem>>> =
        RefCell::new(HashMap::new());

    /// `FutureWriter<JsElem>`s waiting for the value to write, keyed by an id
    /// [`register_future_writer`] hands out.
    ///
    /// A writer dropped unwritten would run wit-bindgen's own drop-default write,
    /// whose `elem_lower` runs outside a [`with_elem_type`] span and would leave the
    /// canonical payload uninitialized. Holding a writer here until its value is
    /// known keeps it from being dropped with a JS object that is collected first.
    /// The writer of a promise that never settles stays here for the instance's
    /// lifetime, since there is no value to write that its type is sure to hold.
    static FUTURE_WRITERS: RefCell<HashMap<u64, FutureWriter<JsElem>>> =
        RefCell::new(HashMap::new());
}

/// Hold `writer` in [`FUTURE_WRITERS`], returning the id to take it back by.
pub(crate) fn register_future_writer(writer: FutureWriter<JsElem>) -> u64 {
    let id = NEXT_FUTURE_WRITER_ID.with(|c| {
        let id = c.get();
        c.set(id + 1);
        id
    });
    FUTURE_WRITERS.with(|cell| cell.borrow_mut().insert(id, writer));
    id
}

/// Spawn `future`, a stream's or a future's transfer, for `promise`. With an
/// event loop active, it is polled once now (see
/// [`js::Promise::spawn_polled`]), so the transfer lowers its values right
/// away. Without one, as in a synchronous export, no task is current to
/// register a waker with, so the first poll waits for the next loop.
pub(crate) fn spawn_transfer(promise: &js::Promise<'_>, future: js::promise::PromiseFuture) {
    if core_runtime::event_loop::with_active_event_loop(|_| ()).is_some() {
        promise.spawn_polled(future);
    } else {
        promise.spawn(future);
    }
}

/// Remove and return a live future writer by id, if present.
pub(crate) fn take_future_writer(id: u64) -> Option<FutureWriter<JsElem>> {
    FUTURE_WRITERS.with(|cell| cell.borrow_mut().remove(&id))
}

/// Whether a stream/future element type takes the `stream<u8>` typed-array fast
/// path. Only `u8`, directly or through aliases, qualifies.
pub(crate) fn elem_is_u8(mut ty: Option<Type>) -> bool {
    while let Some(Type::Alias(alias)) = ty {
        ty = Some(alias.ty());
    }
    matches!(ty, Some(Type::U8))
}

/// The `&'static StreamVtable<u8>` for a `stream<u8>` type `index`, built from the
/// wit-dylib `Stream` metadata on first use and cached. `lift` and `lower` are
/// `None`, since the `Vec<u8>` wit-bindgen reads and writes is already canonical.
pub(crate) fn u8_stream_vtable(ty: WitStream, index: u32) -> &'static StreamVtable<u8> {
    U8_STREAM_VTABLES.with(|cell| {
        *cell.borrow_mut().entry(index).or_insert_with(|| {
            Box::leak(Box::new(StreamVtable::<u8> {
                layout: Layout::from_size_align(ty.abi_payload_size(), ty.abi_payload_align())
                    .expect("valid stream<u8> payload layout"),
                lower: None,
                dealloc_lists: None,
                lift: None,
                // The wit-dylib metadata types the buffers as `c_void` pointers
                // and wit-bindgen's vtable as `u8` pointers, differing in type but
                // not layout. A plain `as` cast cannot change a fn-pointer
                // signature.
                start_write: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *const c_void, usize) -> u32,
                        unsafe extern "C" fn(u32, *const u8, usize) -> u32,
                    >(ty.write())
                },
                start_read: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *mut c_void, usize) -> u32,
                        unsafe extern "C" fn(u32, *mut u8, usize) -> u32,
                    >(ty.read())
                },
                cancel_write: ty.cancel_write(),
                cancel_read: ty.cancel_read(),
                drop_writable: ty.drop_writable(),
                drop_readable: ty.drop_readable(),
                new: ty.new(),
            }))
        })
    })
}

/// The `&'static StreamVtable<JsElem>` for a generic (non-`u8`) stream type
/// `index`, built from the wit-dylib `Stream` metadata on first use and cached.
///
/// `lift` and `lower` are [`elem_lift`] and [`elem_lower`], and `dealloc_lists` is
/// [`elem_dealloc`].
pub(crate) fn generic_stream_vtable(ty: WitStream, index: u32) -> &'static StreamVtable<JsElem> {
    GENERIC_STREAM_VTABLES.with(|cell| {
        *cell.borrow_mut().entry(index).or_insert_with(|| {
            Box::leak(Box::new(StreamVtable::<JsElem> {
                layout: Layout::from_size_align(ty.abi_payload_size(), ty.abi_payload_align())
                    .expect("valid stream payload layout"),
                lower: Some(elem_lower),
                dealloc_lists: Some(elem_dealloc),
                lift: Some(elem_lift),
                start_write: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *const c_void, usize) -> u32,
                        unsafe extern "C" fn(u32, *const u8, usize) -> u32,
                    >(ty.write())
                },
                start_read: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *mut c_void, usize) -> u32,
                        unsafe extern "C" fn(u32, *mut u8, usize) -> u32,
                    >(ty.read())
                },
                cancel_write: ty.cancel_write(),
                cancel_read: ty.cancel_read(),
                drop_writable: ty.drop_writable(),
                drop_readable: ty.drop_readable(),
                new: ty.new(),
            }))
        })
    })
}

/// The `&'static FutureVtable<JsElem>` for a future type `index`, built from the
/// wit-dylib `Future` metadata on first use and cached.
///
/// A future holds a single element, and there is no `u8` fast path.
pub(crate) fn future_vtable(ty: WitFuture, index: u32) -> &'static FutureVtable<JsElem> {
    FUTURE_VTABLES.with(|cell| {
        *cell.borrow_mut().entry(index).or_insert_with(|| {
            Box::leak(Box::new(FutureVtable::<JsElem> {
                layout: Layout::from_size_align(ty.abi_payload_size(), ty.abi_payload_align())
                    .expect("valid future payload layout"),
                lower: elem_lower,
                dealloc_lists: elem_dealloc,
                lift: elem_lift,
                start_write: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *const c_void) -> u32,
                        unsafe extern "C" fn(u32, *const u8) -> u32,
                    >(ty.write())
                },
                start_read: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *mut c_void) -> u32,
                        unsafe extern "C" fn(u32, *mut u8) -> u32,
                    >(ty.read())
                },
                cancel_write: ty.cancel_write(),
                cancel_read: ty.cancel_read(),
                drop_writable: ty.drop_writable(),
                drop_readable: ty.drop_readable(),
                new: ty.new(),
            }))
        })
    })
}
