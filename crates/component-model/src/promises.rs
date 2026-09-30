// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Component Model `future<T>` as `Promise` (wasm-only).
//!
//! A `future<T>` lifted into JS, as an export's argument or an import's result,
//! becomes a `Promise`, and its value is read whether or not JS uses it. A
//! `future<result<T, E>>` fulfills with the `ok` payload and rejects with an
//! error holding the `err` payload: an instance of `E`'s class if it has one
//! (see [`ErrorClass`](crate::ErrorClass)), and a `ComponentError` otherwise. Any
//! other `future<T>` fulfills with its value.
//!
//! Lowering a `future<T>` out of JS accepts a promise or any other value, which
//! is resolved as `Promise.resolve` resolves it, and writes the value once it
//! settles. A fulfillment is written as the `ok` payload of a
//! `future<result<T, E>>` and as the value of any other `future<T>`. A rejection
//! is written as the `err` payload of a `future<result<T, E>>` it converts to
//! (see [`crate::component_error::err_payload`]), and traps otherwise, as does a
//! value that does not lower to `T`. The write is a pending send of the event
//! loop the future was lowered on (see
//! [`core_runtime::event_loop::PendingSend`]), so the call that lowered it
//! finishes once the value is written, or once the loop has no other work left
//! that could settle the promise. A promise that settles later still writes its
//! value.

#![cfg(target_arch = "wasm32")]

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::{Context, Poll};

use core_runtime::{jsclass, jsmethods};
use js::class::create_instance_with;
use js::error::ExnThrown;
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Promise};
use wit_bindgen::rt::async_support::{self, FutureReader, FutureWriteError};

use crate::interpreter::{future_payload, trap, trap_message, wit_future};
use crate::stack::CallStack;
use crate::streams::{
    future_vtable, register_future_writer, spawn_transfer, take_future_writer,
    with_elem_type_future, ElemType, JsElem, SendDone,
};
use crate::value::TypeShape;

/// Lift the readable end `handle` of a `future<T>` with type index `index` into a
/// `Promise`, and start reading the value.
///
/// With an event loop active, the read starts right away and that loop owns it.
/// With none, the read starts from a microtask and is owned by the loop active
/// when the microtask runs, or by the next loop that polls if none is active then
/// either. An export's arguments are lifted before the call's loop is active, so
/// the call's own loop owns their reads.
pub fn future_from_handle(scope: &Scope<'_>, index: u32, handle: u32) -> Result<Value, ExnThrown> {
    let promise = Promise::new_pending(scope)?;
    if core_runtime::event_loop::with_active_event_loop(|_| ()).is_some() {
        spawn_read(&promise, index, handle);
    } else {
        let state = create_instance_with::<FutureReadImpl>(scope, |_| FutureReadImpl {
            index,
            handle,
            promise: Heap::from(promise),
        })?;
        let start = Function::new_callback(scope, c"", 0, start_read, state)?;
        js::jobs::queue_microtask(scope, &start)?;
    }
    Ok(promise.as_value())
}

/// A `future<T>`'s read waiting for the microtask that starts it.
#[jsclass(hidden)]
pub struct FutureRead {
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
    fn new() -> Self {
        // Internal type, created through `create_instance_with`. This constructor
        // only registers the prototype.
        FutureReadImpl::default()
    }
}

/// Microtask: start the read [`future_from_handle`] deferred.
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
            // An `err` payload leaves its error as the pending exception, which
            // rejects the promise.
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
pub struct FutureWrite {
    /// The `future<T>` type index.
    #[no_trace]
    index: u32,
    /// The id of the writer in the future-writer registry.
    #[no_trace]
    writer: u64,
    /// Set once the value is written, which completes the write's pending send.
    #[no_trace]
    done: SendDone,
}

#[jsmethods]
impl FutureWrite {
    fn new() -> Self {
        // Internal type, created through `create_instance_with`. This constructor
        // only registers the prototype.
        FutureWriteImpl::default()
    }
}

/// Lower `value` to the readable end of a `future<T>` with type index `index`,
/// writing the value `value` settles to once it does.
pub fn future_to_handle(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    index: u32,
) -> Result<u32, ExnThrown> {
    let promise = Promise::call_original_resolve(scope, value)?;
    let ty = wit_future(index as usize);
    let vtable = future_vtable(ty, index);
    // SAFETY: `vtable` is built from this future type's wit-dylib metadata. The
    // `default` closure never runs, since the writer stays in the registry until
    // it is written.
    let (writer, reader) =
        unsafe { async_support::future_new(|| JsElem::new(js::value::undefined()), vtable) };
    let rx = reader.take_handle();
    drop(reader);
    let writer = register_future_writer(writer);
    let done = SendDone::default();
    // An abandoned write completes the pending send, so the call can finish. It
    // leaves the writer registered, so the value is still written if the promise
    // settles later.
    done.register({
        let done = done.clone();
        move || done.set()
    });
    let state = create_instance_with::<FutureWriteImpl>(scope, |_| FutureWriteImpl {
        index,
        writer,
        done,
    })?;
    let on_fulfilled = Function::new_callback(scope, c"", 1, future_fulfilled, state)?;
    let on_rejected = Function::new_callback(scope, c"", 1, future_rejected, state)?;
    promise.add_reactions(scope, Some(*on_fulfilled), Some(*on_rejected))?;
    Ok(rx)
}

/// Fulfillment reaction: write the value.
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

/// Rejection reaction: write the `err` payload the reason converts to, or trap.
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
        let message = crate::readable::describe(scope, reason);
        trap(
            scope,
            &format!("a promise lowered to a future rejected: {message}"),
        );
    };
    let Some(payload) = crate::component_error::err_payload(scope, reason, err.as_deref()) else {
        let message = crate::readable::describe(scope, reason);
        trap(
            scope,
            &format!(
                "a promise lowered to a future<result> rejected with a value its error type \
                 cannot hold: {message}"
            ),
        );
    };
    write_result(scope, &state, payload, ok.is_some(), err.is_some(), true)
}

/// Write `payload` as the `ok` or `err` arm of a `future<result<T, E>>`.
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

/// Write `value` to the future and complete the write's pending send once it
/// is written. A `value` that does not lower to the future's type traps here,
/// rather than in the poll that lowers it. The poll lowers a copy of `value`
/// (see [`crate::validate::snapshot`]), so a later change to `value` has no
/// effect on what is written.
fn write(
    scope: &Scope<'_>,
    state: &FutureWrite<'_>,
    value: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let index = state.data().index;
    let value = match &future_payload(index).shape {
        Some(shape) => match crate::validate::snapshot(scope, value, shape) {
            Ok(copy) => copy,
            Err(mismatch) => trap_message(
                &mismatch.describe("the value a promise lowered to a future settled with"),
            ),
        },
        None => scope.root_value(value.get()),
    };
    // Created before the writer is taken, so a failure leaves the writer
    // registered and unwritten.
    let promise = Promise::new_pending(scope)?;
    let Some(writer) = take_future_writer(state.data().writer) else {
        return Ok(js::value::undefined());
    };
    let done = state.data().done.clone();
    let ty = wit_future(index as usize);
    let value = JsElem::new(value.get());
    let write = FinishingWrite {
        write: Some(Box::pin(writer.write(value))),
        ty,
    };
    let future = async move {
        // A reader that dropped first makes the write fail, and the value, lifted
        // back, drops here.
        let _ = with_elem_type_future(ElemType::Future(ty), write).await;
        done.set();
        PromiseOutcome::Resolve(Box::new(|_scope: &Scope<'_>| Ok(HandleValue::undefined())))
    };
    // The write's first poll lowers the value, so with an event loop active it
    // runs now, while the value is in the state `snapshot` checked.
    spawn_transfer(&promise, PromiseFuture::new(future));
    Ok(js::value::undefined())
}

/// A future's write that goes on as a task of its own if it is dropped before it
/// finishes, as when an event loop drops its pending futures. Dropping the
/// write itself would cancel it, and cancelling hands back a writer whose drop
/// writes its default value, which the future's type cannot hold. The task keeps the
/// current component task running until a reader takes the value.
struct FinishingWrite {
    write: Option<Pin<Box<async_support::FutureWrite<JsElem>>>>,
    ty: wit_dylib_ffi::Future,
}

impl Future for FinishingWrite {
    type Output = Result<(), FutureWriteError<JsElem>>;

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
