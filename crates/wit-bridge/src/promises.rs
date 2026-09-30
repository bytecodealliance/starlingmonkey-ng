// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `future` values, which are promises in JavaScript.
//!
//! A lifted `future` is a `Promise`, and its value is read whether or not
//! JavaScript uses the promise:
//!
//! - The promise for a `future<result<T, E>>` fulfills with the `ok` value, or
//!   rejects with an error holding the `err` value. The error is an instance of
//!   `E`'s class if it has one (see [`ErrorClass`](crate::ErrorClass)), and a
//!   `ComponentError` otherwise.
//! - The promise for any other `future<T>` fulfills with the value.
//! - The promise for a `future` without a payload type fulfills with
//!   `undefined`.
//!
//! A lowered `future` can be given as a promise or as any other value, which is
//! resolved with the same rules as `Promise.resolve`. The value is written once
//! the promise settles:
//!
//! - A fulfillment value is written as the `ok` value of a
//!   `future<result<T, E>>`, and as the value of any other `future<T>`.
//! - A rejection reason is written as the `err` value of a
//!   `future<result<T, E>>` if it can be converted into one (see
//!   `component_error::err_payload`).
//!
//! A trap occurs if a rejection reason can't be written, if a fulfillment value
//! can't be lowered to `T`, or if a `future` without a payload type is settled
//! with a value other than `undefined`.
//!
//! The write is a pending send of the event loop on which the future was
//! lowered (see [`core_runtime::event_loop::PendingSend`]). The call that
//! lowered the future completes once the value is written, or once the loop has
//! no other work that could settle the promise. A promise that settles later
//! still writes its value. If the promise is garbage-collected without
//! settling, the future is never written, and a warning is reported.

#![cfg(target_arch = "wasm32")]

use std::future::{Future, IntoFuture};
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::task::{Context, Poll};

use core_runtime::{jsclass, jsmethods};
use js::error::ExnThrown;
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Promise};
use wit_bindgen::rt::async_support::{self, FutureReader, FutureWriteError, FutureWriter};
use wit_dylib_ffi::Type;

use crate::interpreter::{future_payload, trap, trap_message, wit_future, wit_type_name};
use crate::stack::CallStack;
use crate::streams::{
    future_vtable, spawn_transfer, with_elem_type_future, Elem, ElemType, SendDone,
};
use crate::value::TypeShape;

/// Lift the readable end `handle` of a `future<T>` with type index `index` into a
/// `Promise`, and start reading the value.
///
/// If an event loop is active, the read starts immediately and runs on that
/// loop. Otherwise, the read starts in a microtask, and runs on the loop that
/// is active when the microtask runs, or on the next loop that polls. This is
/// the case for the arguments of an export, which are lifted before the call's
/// event loop is active.
pub fn future_from_handle(scope: &Scope<'_>, index: u32, handle: u32) -> Result<Value, ExnThrown> {
    let promise = Promise::new_pending(scope)?;
    if core_runtime::event_loop::has_active_event_loop() {
        spawn_read(&promise, index, handle);
    } else {
        let state = FutureRead::new(scope, index, handle, promise)?;
        let start = Function::new_callback(scope, c"", 0, start_read, state)?;
        js::jobs::queue_microtask(scope, &start)?;
    }
    Ok(promise.as_value())
}

/// The state of a `future` read that starts in a microtask.
#[jsclass(hidden)]
pub(crate) struct FutureRead {
    /// The `future<T>` type index.
    #[no_trace]
    index: u32,
    /// The readable end of the future.
    #[no_trace]
    handle: u32,
    /// The promise the read settles.
    promise: Heap<js::promise::Promise>,
}

#[jsmethods]
impl FutureRead {
    /// The read of the readable end `handle` of a `future<T>` with type index
    /// `index`, which settles `promise`.
    fn new(index: u32, handle: u32, promise: Promise) -> Self {
        FutureReadImpl {
            index,
            handle,
            promise: Heap::from(promise),
        }
    }
}

/// The microtask that starts a read deferred by [`future_from_handle`].
fn start_read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FutureRead>(scope, payload);
    let promise: Promise<'_> = state.data().promise.get(scope);
    spawn_read(&promise, state.data().index, state.data().handle);
    Ok(js::value::undefined())
}

/// Read the value of the `future<T>` with type index `index` from its readable
/// end `handle`, and settle `promise` with it.
fn spawn_read(promise: &Promise<'_>, index: u32, handle: u32) {
    let ty = wit_future(index as usize);
    let vtable = future_vtable(ty, index);
    let future = async move {
        // SAFETY: `handle` is the live readable handle the host handed in, and
        // `vtable` is this future type's.
        let reader = unsafe { FutureReader::new(handle, vtable) };
        // The `lift` runs inside a poll of the read future.
        let value = with_elem_type_future(ElemType::Future(ty), reader.into_future()).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let mut stack = CallStack::new();
            // SAFETY: read for immediate consumption by `push_value`, which roots
            // it. `value` is dropped right after.
            stack.push_value(unsafe { value.get() });
            drop(value);
            // For the `err` case, the error is left pending, which rejects the
            // promise.
            let payload = future_payload(index);
            crate::imports::handle_import_result(
                scope,
                &mut stack,
                payload.shape.as_ref(),
                payload.err_class.as_ref(),
            )
            .map(|value| scope.root_value(value))
        }))
    };
    promise.spawn(PromiseFuture::new(future));
}

/// The state of a lowered `future<T>` waiting for its promise to settle.
#[jsclass(hidden)]
pub(crate) struct FutureWrite {
    /// The `future<T>` type index.
    #[no_trace]
    index: u32,
    /// The future's writable end, until the value is written.
    ///
    /// Dropping an unwritten writer writes wit-bindgen's default value, whose
    /// `elem_lower` runs outside a `with_elem_type` span and leaves the lowered
    /// value uninitialized. No value is valid for every type, so a writer whose
    /// promise never settles is never dropped.
    #[no_trace]
    writer: Option<ManuallyDrop<FutureWriter<Elem>>>,
    /// Set once the value is written, which completes the write's pending send.
    #[no_trace]
    done: SendDone,
}

#[jsmethods]
impl FutureWrite {
    /// The write of `writer`, the writable end of a `future<T>` with type index
    /// `index`, which sets `done` once the value is written.
    fn new(index: u32, writer: ManuallyDrop<FutureWriter<Elem>>, done: SendDone) -> Self {
        FutureWriteImpl {
            index,
            writer: Some(writer),
            done,
        }
    }

    /// Queue a warning if the writer is unwritten when this state is finalized.
    /// Only the promise's reactions reference this state, so the promise was
    /// collected without settling.
    #[destructor]
    fn warn_if_writer_leaks(&mut self) {
        if self.writer.is_some() {
            let ty = wit_type_name(Type::Future(wit_future(self.index as usize)));
            core_runtime::diagnostics::report_later(format!(
                "Warning: a promise lowered to a {ty} was garbage-collected without settling, so \
                 the future's writable end leaks for the instance's lifetime"
            ));
        }
    }
}

/// Lower `value` to a `future` of the type with index `index`, and return the
/// readable end. The value is written once `value`, resolved as a promise,
/// settles.
pub fn future_to_handle(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    index: u32,
) -> Result<u32, ExnThrown> {
    let promise = Promise::call_original_resolve(scope, value)?;
    let ty = wit_future(index as usize);
    let vtable = future_vtable(ty, index);
    // SAFETY: `vtable` is built from this future type's wit-dylib metadata. The
    // `default` closure never runs, since an unwritten writer is never dropped
    // (see `FutureWrite::writer`).
    let (writer, reader) =
        unsafe { async_support::future_new(|| Elem::new(js::value::undefined()), vtable) };
    let rx = reader.take_handle();
    drop(reader);
    let done = SendDone::default();
    // An abandoned write completes the pending send, so the call can finish. It
    // leaves the writer in place, so the value is still written if the promise
    // settles later.
    done.register({
        let done = done.clone();
        move || done.set()
    });
    let state = FutureWrite::new(scope, index, ManuallyDrop::new(writer), done)?;
    let on_fulfilled = Function::new_callback(scope, c"", 1, future_fulfilled, state)?;
    let on_rejected = Function::new_callback(scope, c"", 1, future_rejected, state)?;
    promise.add_reactions(scope, Some(*on_fulfilled), Some(*on_rejected))?;
    Ok(rx)
}

/// The fulfillment reaction, which writes the value.
fn future_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FutureWrite>(scope, payload);
    let value = args.get(0);
    let index = state.data().index;
    match &future_payload(index).shape {
        Some(TypeShape::Result(ok, err)) => {
            write_result(scope, &state, value, ok.is_some(), err.is_some(), false)
        }
        _ => write(scope, &state, value),
    }
}

/// The rejection reaction, which writes the reason as the `err` value, or
/// traps if that isn't possible.
fn future_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<FutureWrite>(scope, payload);
    let reason = args.get(0);
    let index = state.data().index;
    let payload = future_payload(index);
    let Some(TypeShape::Result(ok, err)) = &payload.shape else {
        let message = crate::exports::capture_value(scope, reason).to_string();
        trap(
            scope,
            &format!("a promise lowered to a future rejected: {message}"),
        );
    };
    let payload = match crate::component_error::err_payload(scope, reason, err.as_deref()) {
        Ok(payload) => payload,
        Err(why) => {
            let message = crate::exports::capture_value(scope, reason).to_string();
            trap(
                scope,
                &format!(
                    "a promise lowered to a future<result> rejected with a value that can't be \
                     converted into its error type ({why}): {message}"
                ),
            );
        }
    };
    write_result(scope, &state, payload, ok.is_some(), err.is_some(), true)
}

/// Write `payload` as the `ok` or `err` case of a `future<result<T, E>>`.
fn write_result(
    scope: &Scope<'_>,
    state: &FutureWrite<'_>,
    payload: HandleValue<'_>,
    ok_has_payload: bool,
    err_has_payload: bool,
    is_err: bool,
) -> Result<Value, ExnThrown> {
    let mut stack = CallStack::new();
    let has_payload = if is_err {
        err_has_payload
    } else {
        ok_has_payload
    };
    if has_payload {
        stack.push_value(payload.get());
    }
    crate::value::push_result(&mut stack, scope, ok_has_payload, err_has_payload, is_err)?;
    let wrapper = scope.root_value(stack.pop_value());
    write(scope, state, wrapper)
}

/// Write `value` to the future, and complete the write's pending send once it
/// is written. Traps if `value` can't be lowered to the future's type. The
/// snapshot of `value` is lowered (see [`crate::validate::snapshot_payload`]),
/// so later changes to `value` don't affect what is written.
fn write(
    scope: &Scope<'_>,
    state: &FutureWrite<'_>,
    value: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let index = state.data().index;
    let payload = future_payload(index);
    let snapshot = match crate::validate::snapshot_payload(scope, value, payload.shape.as_ref()) {
        Ok(snapshot) => snapshot,
        Err(mismatch) => {
            trap_message(&mismatch.describe("the value a promise lowered to a future settled with"))
        }
    };
    // Created before the writer is taken, so a failure leaves the writer in
    // place and unwritten.
    let promise = Promise::new_pending(scope)?;
    let writer = state.data_mut().writer.take();
    let Some(writer) = writer else {
        return Ok(js::value::undefined());
    };
    let writer = ManuallyDrop::into_inner(writer);
    let done = state.data().done.clone();
    let ty = wit_future(index as usize);
    let write = FinishingWrite {
        write: Some(Box::pin(writer.write(Elem::payload(snapshot)))),
        ty,
    };
    let future = async move {
        // If the reader was dropped, the write fails, and the value is dropped
        // here.
        let _ = with_elem_type_future(ElemType::Future(ty), write).await;
        done.set();
        PromiseOutcome::Resolve(Box::new(|_scope: &Scope<'_>| Ok(HandleValue::undefined())))
    };
    // The write's first poll lowers the value. If an event loop is active, the
    // poll happens now, while the value is as checked by `snapshot_payload`.
    spawn_transfer(&promise, PromiseFuture::new(future));
    Ok(js::value::undefined())
}

/// A future write that continues in a separate task if it is dropped before it
/// finishes, such as when an event loop drops its pending futures. Dropping the
/// write itself would cancel it, and dropping the writer returned by the
/// cancellation would write its default value. The task keeps the current
/// component task running until the value is read.
struct FinishingWrite {
    write: Option<Pin<Box<async_support::FutureWrite<Elem>>>>,
    ty: wit_dylib_ffi::Future,
}

impl Future for FinishingWrite {
    type Output = Result<(), FutureWriteError<Elem>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let write = self.write.as_mut().expect("polled after it finished");
        let written = std::task::ready!(write.as_mut().poll(cx));
        self.write = None;
        Poll::Ready(written)
    }
}

impl Drop for FinishingWrite {
    fn drop(&mut self) {
        if let Some(write) = self.write.take() {
            async_support::spawn_local(with_elem_type_future(
                ElemType::Future(self.ty),
                async move {
                    let _ = write.await;
                },
            ));
        }
    }
}
