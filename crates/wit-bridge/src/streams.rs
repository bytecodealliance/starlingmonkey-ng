// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The vtables and element conversions used to read and write `stream` and
//! `future` values.
//!
//! [`crate::readable`] represents a `stream` as a `ReadableStream`, and
//! [`crate::promises`] represents a `future` as a `Promise`. Both read and
//! write with wit-bindgen's `StreamReader`, `StreamWriter`, `FutureReader` and
//! `FutureWriter`, using the vtables defined here.
//!
//! # Vtables
//!
//! wit-bindgen's readers and writers are generic over the element type, and use
//! a `&'static StreamVtable<T>` or `FutureVtable<T>`. The vtable of each stream
//! and future type is built from the type's wit-dylib metadata on first use,
//! leaked, and cached by type index.
//!
//! A `stream<u8>` uses `Vec<u8>` buffers, whose layout is the canonical ABI's,
//! so its vtable has no `lift` and `lower`. Every other type uses `Elem` as
//! its element type, and converts elements with `elem_lift` and `elem_lower`,
//! which call the metadata's `lift` and `lower` functions. The elements of a
//! `stream` or `future` without a payload type are lifted as `undefined`, and
//! lowered to nothing.

#![cfg(target_arch = "wasm32")]

use std::alloc::Layout;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use wit_bindgen::rt::async_support::{FutureVtable, StreamVtable};
use wit_dylib_ffi::{Future as WitFuture, Stream as WitStream, Type};

use crate::init;
use crate::interpreter::CallCx;
use crate::resources::{imported_wrapper, ImportedResource};
use crate::stack::CallStack;
use crate::value::TypeShapeValue;

/// The element type of the vtables of all types except `stream<u8>`. Its JS
/// values are rooted, because wit-bindgen's buffers aren't traced.
pub(crate) enum Elem {
    /// A lifted element, or an element of a `stream` or `future` without a
    /// payload type.
    Js(RootedHeap<Value>),
    /// An element to be written, converted by
    /// [`crate::validate::snapshot_payload`].
    Shaped(TypeShapeValue),
}

impl Elem {
    /// Create an element holding `value`, which is rooted immediately.
    #[js::allow_unrooted]
    pub(crate) fn new(value: Value) -> Elem {
        Elem::Js(RootedHeap::from(value))
    }

    /// Create an element to be written from `payload`, as returned by
    /// [`crate::validate::snapshot_payload`]. `None` becomes `undefined`.
    pub(crate) fn payload(payload: Option<TypeShapeValue>) -> Elem {
        match payload {
            Some(payload) => Elem::Shaped(payload),
            None => Elem::new(js::value::undefined()),
        }
    }

    /// Return the element's value.
    ///
    /// # Safety
    ///
    /// The returned [`Value`] is not separately rooted, so the caller must
    /// consume or root it before the next allocation can trigger a GC.
    ///
    /// # Panics
    ///
    /// Panics for an [`Elem::Shaped`].
    pub(crate) unsafe fn get(&self) -> Value {
        match self {
            // SAFETY: read for immediate consumption. The value stays rooted, so
            // no GC can invalidate it before use.
            Elem::Js(value) => unsafe { value.as_raw() },
            Elem::Shaped(_) => panic!("Elem::get called for a snapshot"),
        }
    }

    /// Return the snapshot of an [`Elem::Shaped`], and `None` for an
    /// [`Elem::Js`].
    pub(crate) fn shaped(&self) -> Option<&TypeShapeValue> {
        match self {
            Elem::Js(_) => None,
            Elem::Shaped(shaped) => Some(shaped),
        }
    }
}

/// Drop the wrappers in [`LOWERED_ELEM_ORIGINALS`]. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    LOWERED_ELEM_ORIGINALS.with(|cell| cell.borrow_mut().clear());
}

js::instance_local! {
    /// The wrapper of each lowered element that is an imported-resource wrapper,
    /// keyed by the address of the lowered value. If the element isn't sent and
    /// is lifted again, its handle is moved back to this wrapper. An entry is
    /// removed by [`elem_dealloc`] once the element is sent, or by [`elem_lift`]
    /// when the element is lifted again.
    static LOWERED_ELEM_ORIGINALS: RefCell<HashMap<*mut u8, RootedHeap<js::object::Object>>> =
        RefCell::new(HashMap::new());
}

/// Return `value` if it is an imported-resource wrapper that holds a handle.
/// Objects implementing exported resources aren't wrappers, and lifting such an
/// object's handle again returns the same object (see `resources::push_own`).
fn live_wrapper<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<Object<'s>> {
    let wrapper = imported_wrapper(scope, value)?;
    let live = wrapper.data().handle.is_some();
    live.then_some(*wrapper)
}

/// Move the handle from `fresh`, a wrapper created by lifting an element again,
/// to `original`, the wrapper that was lowered. `fresh` is left without a
/// handle.
fn hand_handle_back(original: &Object<'_>, fresh: &Object<'_>) {
    if let (Ok(original), Ok(fresh)) = (
        original.cast::<ImportedResource>(),
        fresh.cast::<ImportedResource>(),
    ) {
        crate::resources::move_handle(fresh, original);
    }
}

js::instance_local! {
    /// The types of the elements being converted, read by [`elem_lift`] and
    /// [`elem_lower`], which are plain functions and can't capture them. The
    /// last entry is the active type. Each nested conversion pushes an entry.
    static ELEM_TYPE: RefCell<Vec<ElemType>> = const { RefCell::new(Vec::new()) };
}

/// The wit-dylib metadata of a stream or future type.
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

    /// Whether this is a `stream<T>` or `future<T>` with a `T`.
    fn has_payload(&self) -> bool {
        match self {
            ElemType::Stream(s) => s.ty().is_some(),
            ElemType::Future(f) => f.ty().is_some(),
        }
    }

    /// Lift one element from the canonical ABI buffer at `ptr` onto `cx`'s stack.
    /// An element without a payload lifts to `undefined`.
    ///
    /// # Safety
    ///
    /// `ptr` must point at one initialized canonical-ABI payload of this type.
    unsafe fn lift(&self, cx: &mut CallCx<'_>, ptr: *mut u8) {
        match self {
            _ if !self.has_payload() => cx.stack().push_value(js::value::undefined()),
            ElemType::Stream(s) => unsafe { s.lift(cx, ptr) },
            ElemType::Future(f) => unsafe { f.lift(cx, ptr) },
        }
    }

    /// Lower one element off `cx`'s stack into the canonical ABI buffer at `ptr`.
    /// An element without a payload is popped and discarded.
    ///
    /// # Safety
    ///
    /// `ptr` must point at writable space for one canonical-ABI payload.
    unsafe fn lower(&self, cx: &mut CallCx<'_>, ptr: *mut u8) {
        match self {
            _ if !self.has_payload() => {
                cx.stack().pop_value();
            }
            ElemType::Stream(s) => unsafe { s.lower(cx, ptr) },
            ElemType::Future(f) => unsafe { f.lower(cx, ptr) },
        }
    }
}

/// Run `f` with `ty` as the active element type. The previous type is restored
/// afterwards.
fn with_elem_type<R>(ty: ElemType, f: impl FnOnce() -> R) -> R {
    ELEM_TYPE.with(|cell| cell.borrow_mut().push(ty));
    let r = f();
    ELEM_TYPE.with(|cell| {
        cell.borrow_mut().pop().expect("element type was pushed");
    });
    r
}

/// A future that sets the active element type during each poll of the wrapped
/// future, and while the wrapped future is dropped.
///
/// wit-bindgen calls the vtable's `lift` and `lower` during the poll that
/// completes a transfer, so the type must be set during each poll. Setting it
/// per poll also lets transfers of different element types interleave.
/// Dropping an unfinished read or write cancels it, which lifts the elements
/// that weren't sent.
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

/// Return the active element type, or `None` outside of [`with_elem_type`].
/// Every read and write runs in a [`WithElemType`], including the drop that
/// cancels it. A conversion outside of it only happens when wit-bindgen writes
/// the default value of a `FutureWriter` that is dropped unwritten, which
/// `promises::FutureWrite` never does.
fn active_elem_type() -> Option<ElemType> {
    ELEM_TYPE.with(|cell| cell.borrow().last().copied())
}

js::instance_local! {
    /// The stack used to lower each element, keyed by the address of the
    /// lowered value. A lowered string points into the strings stored on the
    /// stack, so the stack must outlive the lowered value.
    ///
    /// Each entry is removed by exactly one of [`elem_dealloc`], for an element
    /// that was sent, and [`elem_lift`], for an element that wasn't sent and is
    /// lifted again.
    static LOWERED_ELEM_STACKS: RefCell<HashMap<*mut u8, CallStack>> =
        RefCell::new(HashMap::new());
}

/// The vtable's `lower` function: lower `value` into the buffer at `dst` with
/// the metadata's `lower` function. The stack used for lowering is stored in
/// [`LOWERED_ELEM_STACKS`] if it holds data that the lowered value can point
/// to.
///
/// # Safety
///
/// `dst` must point at writable space for one canonical-ABI payload of the
/// active element type, and a [`with_elem_type`] span must be active.
unsafe fn elem_lower(value: Elem, dst: *mut u8) {
    let Some(ty) = active_elem_type() else {
        // No active type (see `active_elem_type`). The value is dropped and `dst`
        // left as wit-bindgen allocated it.
        drop(value);
        return;
    };
    // If the element isn't sent, lifting it again takes over the strings and
    // lists of this stack.
    let mut stack = CallStack::new();
    stack.set_adoptable();
    match value {
        // SAFETY: `value.as_raw()` is read for immediate consumption by
        // `push_value`, which roots it on the stack. `value` is dropped at the
        // end of this arm, before the lower below can allocate.
        Elem::Js(value) => stack.push_value(unsafe { value.as_raw() }),
        Elem::Shaped(shaped) => stack.push_shaped(shaped),
    }
    // Lowering an `own` handle of an imported resource removes the handle from
    // its wrapper. The wrapper is kept, so that the handle can be moved back if
    // the element isn't sent and is lifted again. Only an element that is itself
    // a wrapper is kept. A wrapper nested in a record, tuple, option or list is
    // replaced by a new wrapper when the element is lifted again.
    if ty.owns_resources() && stack.last_shaped().is_none() {
        init::with_scope(|scope| {
            if let Some(original) = live_wrapper(scope, scope.root_value(stack.last())) {
                LOWERED_ELEM_ORIGINALS
                    .with(|cell| cell.borrow_mut().insert(dst, RootedHeap::new(original)));
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
    // A stack holding strings or allocations that the lowered value can point
    // to is kept until `dealloc_lists` is called for `dst`.
    if stack.holds_lowered_data() {
        LOWERED_ELEM_STACKS.with(|cell| cell.borrow_mut().insert(dst, stack));
    }
}

/// The vtable's `dealloc_lists` function, called for each element that was
/// sent. Drops the stack and the wrapper stored by [`elem_lower`] for `dst`.
///
/// # Safety
///
/// `dst` may be any address. An address without stored entries is ignored.
unsafe fn elem_dealloc(dst: *mut u8) {
    drop(LOWERED_ELEM_STACKS.with(|cell| cell.borrow_mut().remove(&dst)));
    drop(LOWERED_ELEM_ORIGINALS.with(|cell| cell.borrow_mut().remove(&dst)));
}

/// The vtable's `lift` function: lift the element in the buffer at `src` with
/// the metadata's `lift` function.
///
/// # Safety
///
/// `src` must point at one initialized canonical-ABI payload of the active
/// element type, and a [`with_elem_type`] span must be active.
unsafe fn elem_lift(src: *mut u8) -> Elem {
    let Some(ty) = active_elem_type() else {
        // No active type (see `active_elem_type`).
        return Elem::new(js::value::undefined());
    };
    let mut stack = CallStack::new();
    {
        let mut cx = CallCx::Borrowed(&mut stack);
        // SAFETY: `src` is a valid initialized payload per the precondition, and
        // the active element type matches the buffer `src` points into. The
        // metadata `lift` pushes one JS value onto the stack.
        unsafe { ty.lift(&mut cx, src) };
    }
    // `Elem::new` roots the popped value before anything can allocate.
    let mut elem = Elem::new(stack.pop_value());
    // If `elem_lower` stored a stack for `src`, the element wasn't sent and is
    // being lifted again, so `dealloc_lists` won't be called for it. The lift
    // took ownership of the strings and lists, so the stack releases them
    // without freeing them.
    if let Some(mut stack) = LOWERED_ELEM_STACKS.with(|cell| cell.borrow_mut().remove(&src)) {
        stack.release_adopted();
    }
    // Lifting an unsent imported resource creates a new wrapper. The handle is
    // moved back to the wrapper that was lowered, and the element is replaced
    // by that wrapper.
    let original = LOWERED_ELEM_ORIGINALS.with(|cell| cell.borrow_mut().remove(&src));
    if let Some(original) = original {
        init::with_scope(|scope| {
            let original = original.get(scope);
            // SAFETY: read for immediate consumption while `elem` stays rooted.
            if let Ok(fresh) = Object::from_raw_value(scope, unsafe { elem.get() }) {
                hand_handle_back(&original, &fresh);
            }
            elem = Elem::new(original.as_value());
        });
    }
    elem
}

/// Whether a [`PendingSend`](core_runtime::event_loop::PendingSend) for a
/// stream pump or a future write is done, and the waker of the task waiting
/// for it.
#[derive(Clone, Default)]
pub(crate) struct SendDone(std::rc::Rc<RefCell<(bool, Option<std::task::Waker>)>>);

impl SendDone {
    /// Mark the send as done, and wake the task waiting for it.
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

    /// Register a pending send on the active event loop that completes once
    /// this is set, and calls `abandon` once the loop has no other work left.
    /// Returns `false` without registering a send if no event loop is active.
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
    /// The vtable of each `stream<u8>` type, by type index. Each vtable is built
    /// and leaked on first use, so at most one vtable per type is leaked.
    static U8_STREAM_VTABLES: RefCell<HashMap<u32, &'static StreamVtable<u8>>> =
        RefCell::new(HashMap::new());

    /// The vtable of each other stream type, by type index.
    static GENERIC_STREAM_VTABLES: RefCell<HashMap<u32, &'static StreamVtable<Elem>>> =
        RefCell::new(HashMap::new());

    /// The vtable of each future type, by type index.
    static FUTURE_VTABLES: RefCell<HashMap<u32, &'static FutureVtable<Elem>>> =
        RefCell::new(HashMap::new());
}

/// Spawn `future`, which transfers the values of a stream or future, to settle
/// `promise`. If an event loop is active, `future` is polled once immediately
/// (see [`js::Promise::spawn_polled`]), so the transfer lowers its values right
/// away. Otherwise, the first poll happens on the next event loop.
pub(crate) fn spawn_transfer(promise: &js::Promise<'_>, future: js::promise::PromiseFuture) {
    if core_runtime::event_loop::has_active_event_loop() {
        promise.spawn_polled(future);
    } else {
        promise.spawn(future);
    }
}

/// Whether `ty` is `u8`, directly or through aliases.
pub(crate) fn elem_is_u8(mut ty: Option<Type>) -> bool {
    while let Some(Type::Alias(alias)) = ty {
        ty = Some(alias.ty());
    }
    matches!(ty, Some(Type::U8))
}

/// Return the vtable of the `stream<u8>` type `ty`, whose index is `index`.
/// `lift` and `lower` are `None`, because the canonical ABI layout of a `u8`
/// buffer is that of a `Vec<u8>`.
pub(crate) fn u8_stream_vtable(ty: WitStream, index: u32) -> &'static StreamVtable<u8> {
    U8_STREAM_VTABLES.with(|cell| {
        *cell.borrow_mut().entry(index).or_insert_with(|| {
            Box::leak(Box::new(StreamVtable::<u8> {
                layout: Layout::from_size_align(ty.abi_payload_size(), ty.abi_payload_align())
                    .expect("valid stream<u8> payload layout"),
                lower: None,
                dealloc_lists: None,
                lift: None,
                // SAFETY: the two signatures differ only in the pointee type of
                // the buffer pointer, `c_void` in the wit-dylib metadata and `u8`
                // in wit-bindgen's vtable, which has the same ABI. A plain `as`
                // cast cannot change a fn-pointer signature.
                start_write: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *const c_void, usize) -> u32,
                        unsafe extern "C" fn(u32, *const u8, usize) -> u32,
                    >(ty.write())
                },
                // SAFETY: as for `start_write`.
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

/// Return the vtable of the stream type `ty`, whose index is `index` and whose
/// elements aren't `u8`.
pub(crate) fn generic_stream_vtable(ty: WitStream, index: u32) -> &'static StreamVtable<Elem> {
    GENERIC_STREAM_VTABLES.with(|cell| {
        *cell.borrow_mut().entry(index).or_insert_with(|| {
            Box::leak(Box::new(StreamVtable::<Elem> {
                layout: Layout::from_size_align(ty.abi_payload_size(), ty.abi_payload_align())
                    .expect("valid stream payload layout"),
                lower: Some(elem_lower),
                dealloc_lists: Some(elem_dealloc),
                lift: Some(elem_lift),
                // SAFETY: as for the `stream<u8>` vtable's `start_write`.
                start_write: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *const c_void, usize) -> u32,
                        unsafe extern "C" fn(u32, *const u8, usize) -> u32,
                    >(ty.write())
                },
                // SAFETY: as for `start_write`.
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

/// Return the vtable of the future type `ty`, whose index is `index`.
pub(crate) fn future_vtable(ty: WitFuture, index: u32) -> &'static FutureVtable<Elem> {
    FUTURE_VTABLES.with(|cell| {
        *cell.borrow_mut().entry(index).or_insert_with(|| {
            Box::leak(Box::new(FutureVtable::<Elem> {
                layout: Layout::from_size_align(ty.abi_payload_size(), ty.abi_payload_align())
                    .expect("valid future payload layout"),
                lower: elem_lower,
                dealloc_lists: elem_dealloc,
                lift: elem_lift,
                // SAFETY: as for the `stream<u8>` vtable's `start_write`.
                start_write: unsafe {
                    std::mem::transmute::<
                        unsafe extern "C" fn(u32, *const c_void) -> u32,
                        unsafe extern "C" fn(u32, *const u8) -> u32,
                    >(ty.write())
                },
                // SAFETY: as for `start_write`.
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
