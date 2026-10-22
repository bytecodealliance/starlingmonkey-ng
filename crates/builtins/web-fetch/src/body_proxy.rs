// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The spec's "create a proxy" for a body, used by the `Request` constructor's
//! step 41: the derived request gets its own `ReadableStream` whose chunks
//! are read from the original body's stream, and the original is locked when
//! the proxy is created.

use core_runtime::jsclass;
use core_runtime::jsmethods;
use js::error::ExnThrown;
use js::function::cast_payload;
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{HandleValue, OptionHeapExt};
use js::{Object, Promise};
use web_streams::readable::default_reader::DefaultReaderImpl;
use web_streams::readable::native_read::{
    acquire_native_reader, native_reader_cancel, native_reader_read, NativeReadSteps,
};
use web_streams::readable::readable_stream::{ReadableStream, ReadableStreamImpl};
use web_streams::readable::{DefaultReader, ReadableStreamDefaultController};
use web_streams::AlgorithmArg;

/// State for one body proxy: the internal reader on the original stream, the
/// proxy stream's controller (captured at pull time), and the in-flight pull's
/// promise (settled when the read delivers).
#[jsclass(hidden)]
pub struct BodyProxySource {
    reader: Heap<DefaultReaderImpl>,
    stream: Option<Heap<ReadableStreamImpl>>,
    current_pull: Option<Heap<js::promise::Promise>>,
}

#[jsmethods]
impl BodyProxySource {
    fn new(reader: DefaultReader) -> Self {
        Self {
            reader: Heap::from(reader),
            stream: None,
            current_pull: None,
        }
    }

    fn stream<'r>(&self, scope: &'r Scope) -> ReadableStream<'r> {
        self.data()
            .stream
            .get(scope)
            .expect("source is fully initialized")
    }

    fn controller<'r>(&self, scope: &'r Scope) -> ReadableStreamDefaultController<'r> {
        self.stream(scope)
            .default_controller(scope)
            .expect("Body proxies always have a default controller")
    }
}

/// Create a proxy stream for `source`: locks `source` to an internal reader and
/// returns a new stream that re-delivers its chunks one pull at a time.
pub(crate) fn proxy_body_stream<'r>(
    scope: &'r Scope<'_>,
    source: &ReadableStream<'_>,
) -> Result<ReadableStream<'r>, ExnThrown> {
    let reader = acquire_native_reader(scope, source)?;
    let state = BodyProxySource::new(scope, reader)?;
    let pull = AlgorithmArg::native(scope, proxy_pull, state)?;
    // Cancelling the proxy must cancel the stream it is draining, otherwise the original stays
    // locked to the internal reader forever, and the source isn't stopped & dropped.
    let cancel = AlgorithmArg::native(scope, proxy_cancel, state)?;
    // No native-source marker: the proxy must deliver through its own queue, so
    // the incoming→outgoing shortcut does not apply to it.
    let stream = ReadableStream::new_native(scope, HandleValue::undefined(), pull, cancel)?;
    state.data_mut().stream.set(stream);
    Ok(stream)
}

/// Underlying-source `cancel`: cancel the original stream with the same reason.
fn proxy_cancel(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<BodyProxySource>(scope, payload);
    let reader = state.data().reader.get(scope);
    let cancelled = native_reader_cancel(scope, reader, args.get(0));
    // Any pull still in flight will never be delivered now; settle it so the stream machinery
    // does not wait on it.
    settle_pull(scope, &state);
    Ok(cancelled.as_value())
}

/// The proxy's read-request steps: enqueue/close/error the proxy stream's
/// controller, then settle the in-flight pull promise.
const PROXY_STEPS: NativeReadSteps = NativeReadSteps {
    chunk: proxy_chunk_step,
    close: proxy_close_step,
    error: proxy_error_step,
};

/// Underlying-source `pull`: issue one internal read on the original stream;
/// the returned promise settles when the chunk has been enqueued (so the
/// stream machinery never overlaps pulls).
fn proxy_pull(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<BodyProxySource>(scope, payload);
    let promise = Promise::new_pending(scope)?;
    state.data_mut().current_pull = Some(Heap::from(promise));
    let reader = state.data().reader.get(scope);
    // Ensure we're getting the right controller.
    unsafe {
        debug_assert_eq!(state.controller(scope).as_raw(), args.get(0).to_object());
    }
    native_reader_read(scope, reader, PROXY_STEPS, state.as_object())?;
    Ok(promise.as_value())
}

/// Settle the in-flight pull promise (delivery finished), and forget it — this
/// pull is over, so retaining the settled promise would keep it (and everything
/// its reactions hold) alive until the next pull replaces it.
fn settle_pull(scope: &Scope<'_>, state: &BodyProxySource<'_>) {
    let pull = state.data().current_pull.get(scope);
    state.data_mut().current_pull = None;
    if let Some(promise) = pull {
        if promise.resolve(scope, HandleValue::undefined()).is_err() {
            // See `proxy_close_step`: a native callback must not return with an exception
            // pending, and there is no caller here to propagate one to.
            // TODO: use promise rejection reporting once available.
            js::exception::clear(scope);
        }
    }
}

fn proxy_chunk_step(
    scope: &Scope<'_>,
    payload: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<BodyProxySource>()
        .expect("payload is a BodyProxySource");
    let controller = state.controller(scope);
    // A proxy that has been cancelled — by `abort the fetch() call` cancelling the request
    // body — can no longer be enqueued to; the chunk has nowhere to go. See
    // `proxy_close_step` for why the pending exception must be cleared too.
    if controller.enqueue(scope, chunk).is_err() {
        js::exception::clear(scope);
    }
    settle_pull(scope, &state);
    Ok(())
}

fn proxy_close_step(scope: &Scope<'_>, payload: Object<'_>) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<BodyProxySource>()
        .expect("payload is a BodyProxySource");
    let controller = state.controller(scope);
    // The proxy can already be closed or cancelled by the time the proxied stream closes:
    // `abort the fetch() call` cancels the request body, and cancelling the proxy cancels the
    // proxied stream, whose close re-enters here through any read request still pending on it
    // (a host-backed body always has one — its chunk read is in flight). Closing again is
    // meaningless and fails, so ignore it, as the chunk step does. These are a read request's
    // `close steps`, which the streams machinery requires not to throw.
    if controller.close(scope).is_err() {
        // Clear the exception the failed close left pending: a native callback must not
        // return with one set, and there is no caller here to propagate it to.
        js::exception::clear(scope);
    }
    settle_pull(scope, &state);
    Ok(())
}

fn proxy_error_step(
    scope: &Scope<'_>,
    payload: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = payload
        .cast::<BodyProxySource>()
        .expect("payload is a BodyProxySource");
    let controller = state.controller(scope);
    // Erroring an already closed or cancelled proxy fails without any effect. The failure
    // is ignored for the same reason as in the close step: these are a read request's
    // `error steps`.
    if controller.error(scope, Some(error)).is_err() {
        js::exception::clear(scope);
    }
    settle_pull(scope, &state);
    Ok(())
}
