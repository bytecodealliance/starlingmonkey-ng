// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! In-flight `fetch` abort.
//!
//! `fetch` registers an abort algorithm on the request's `AbortSignal`. When the
//! signal aborts while the request is in flight, the fetch promise is rejected
//! with the signal's abort reason. No-op when response delivery has completed.

use crate::body_mixin::BodyMixin;
use crate::incoming_body::HostBackedBodyOwner;
use crate::request::Request;
use crate::response::Response;
use core_runtime::jsclass;
use core_runtime::jsmethods;
use js::error::ExnThrown;
use js::function::cast_payload;
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{value, Function, Promise};
use web_globals::signals::{AbortSignal, AbortSignalImpl};

/// State for the abort algorithm: the fetch promise to reject when the signal
/// aborts, and (once delivered) the response whose body should be aborted.
///
/// The signal, and the callback registered on it, are kept so the algorithm can
/// deregister itself once a later abort could no longer do anything — see
/// [`AbortFetchState::detach`].
#[jsclass(hidden)]
pub struct AbortFetchState {
    promise: Heap<js::promise::Promise>,
    signal: Option<Heap<AbortSignalImpl>>,
    request: Option<Heap<crate::request::RequestImpl>>,
    response: Option<Heap<crate::response::ResponseImpl>>,
    callback: Option<Heap<js::function::Function>>,
}

#[jsmethods]
impl AbortFetchState {
    fn new(promise: Promise, request: Request, signal: AbortSignal) -> Self {
        Self {
            promise: Heap::from(promise),
            signal: Some(Heap::from(signal)),
            request: Some(Heap::from(request)),
            response: None,
            callback: None,
        }
    }
}

impl AbortFetchState<'_> {
    /// Record the delivered response, so a later abort can also abort its body. A
    /// response with nothing an abort could act on detaches the algorithm instead.
    /// One with a body detaches it once that body has been read (see
    /// `Response::consume`), since aborting mid-read must still error the stream.
    pub(crate) fn deliver(&self, scope: &Scope<'_>, response: &Response<'_>) {
        if response.body_is_abortable(scope) {
            debug_assert!(self.data().response.is_none());
            self.data_mut().response = Some(Heap::from(*response));
            response.data_mut().abort_state = Some(Heap::from(*self));
        } else {
            self.detach(scope);
        }
    }

    /// Deregister the abort algorithm from the signal and drop what it held.
    ///
    /// An `AbortSignal` outlives the fetches made with it, so an algorithm left
    /// registered would accumulate one entry per fetch, each rooting that fetch's
    /// promise, the `Response`, and the `Response`'s host body and connection.
    /// Detaching once the algorithm can no longer have an effect keeps a reused
    /// signal from growing without bound.
    pub(crate) fn detach(&self, scope: &Scope<'_>) {
        let signal = self.data_mut().signal.take_rooted(scope);
        let callback = self.data_mut().callback.take_rooted(scope);
        if let (Some(signal), Some(callback)) = (signal, callback) {
            web_globals::signals::algorithms::remove_abort_algorithm(&signal, &callback);
        }
        self.data_mut().request = None;
        self.data_mut().response = None;
    }
}

/// <https://fetch.spec.whatwg.org/#abort-fetch>
/// To abort a fetch() call with a promise, request, responseObject, and an error:
pub(crate) fn abort_fetch(
    scope: &Scope<'_>,
    promise: &Promise<'_>,
    request: &Request<'_>,
    response_object: Option<&Response<'_>>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    // Step 1: `Reject` _promise_ with _error_.
    // Note: This does nothing if _promise_ has already fulfilled.
    promise.reject(scope, error)?;

    // Step 2: If _request_’s `body` is non-null and is `readable`, then `cancel` _request_’s `body`
    //     with _error_.
    // A body has a stream to cancel here once one exists for it: an author's `ReadableStream` body
    // always, and a host-source body once content asked for it (they are materialized lazily, see
    // `BodyMixin::body_value`). What cancelling it means depends on what is behind it:
    //   - An author stream: its `cancel` steps run.
    //   - A proxy for a host stream, e.g. for `fetch(incomingRequest)`: cancel's the proxied
    //     stream, so the host body reached through one is dropped and its connection closed.
    //   - A host-source stream forwarded straight to the transport (`body: response.body`): a no-op
    //     here, but released instead by aborting the controller, which drops the transport future
    //     that now owns it. `on_abort` does that before calling this algorithm.
    if let Some(stream) = HostBackedBodyOwner::body_stream(request, scope) {
        if stream.is_readable() {
            // Rejects if the stream's `cancel()` throws. Nothing observes this promise, so it is
            // marked handled rather than reported as an unhandled rejection.
            let _ = stream
                .cancel_internal(scope, error)
                .set_any_is_handled(scope);
        }
    }

    // Cancelling a body `disturbs` it, so `bodyUsed` reports true afterwards. A body with
    // no materialized stream has only the flag standing in for that.
    request.set_source_disturbed();

    // Step 3: If _responseObject_ is null, then return.
    let Some(response_object) = response_object else {
        return Ok(());
    };

    // Step 4: Let _response_ be _responseObject_’s `response`.
    // Step 5: If _response_’s `body` is non-null and is `readable`, then `error` _response_’s
    //     `body` with _error_.
    // `abort_body` errors the `.body` stream and stops the host read that feeds it.
    response_object.abort_body(scope, error);
    Ok(())
}

/// <https://fetch.spec.whatwg.org/#dom-global-fetch> step 11.
///
/// Add an abort algorithm to `signal` that rejects `promise` with the abort reason
/// (and aborts the response body if one has been delivered). Returns the state so
/// `fetch` can record the response on it once delivered.
pub(crate) fn register_fetch_abort<'r>(
    scope: &'r Scope<'_>,
    signal: AbortSignal<'_>,
    promise: Promise<'_>,
    request: Request<'_>,
) -> Result<AbortFetchState<'r>, ExnThrown> {
    let state = AbortFetchState::new(scope, promise, request, signal)?;
    let callback = Function::new_callback(scope, c"", 1, on_abort, state)?;
    web_globals::signals::algorithms::add_abort_algorithm(&signal, &callback);
    state.data_mut().callback = Some(Heap::from(callback));

    // The algorithm is detached where the fetch promise settles: by `deliver`, or on each path
    // that rejects it. A reaction on the promise would mark it handled, which suppresses the
    // report of a rejection the caller never catches.
    Ok(state)
}

/// <https://fetch.spec.whatwg.org/#dom-global-fetch> steps 11.1 to 11.4.
///
/// The registered abort algorithm.
fn on_abort(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<AbortFetchState>(scope, payload);
    let promise = state.data().promise.get(scope);
    let reason = args.get(0);

    // Step 11.1: Set _locallyAborted_ to true.
    // (not applicable)
    // Step 11.2: `Assert`: _controller_ is non-null.
    // (implicit)
    // Step 11.3: `Abort` _controller_ with _requestObject_’s `signal`’s `abort reason`.
    // Implemented by dropping the transport future. That future owns the outgoing body, so
    // dropping it frees that body, too.
    js::promise::cancel_pending_future(promise);

    // Step 11.4: `Abort the `fetch()` call` with _p_, _request_, _responseObject_, and
    //     _requestObject_’s `signal`’s `abort reason`.
    let request = state.data().request.get(scope);
    let response = state.data().response.get(scope);
    if let Some(request) = request {
        abort_fetch(scope, &promise, &request, response.as_ref(), reason)?;
    }
    state.detach(scope);

    Ok(value::undefined())
}
