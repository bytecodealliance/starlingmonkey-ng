// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Global functions and constants from <https://fetch.spec.whatwg.org/>

use core_runtime::jsglobals;

#[jsglobals]
pub mod globals {
    use crate::request::{request_info, Request, RequestInit};
    use crate::response::response_from_platform;
    use js::conversion::ToJSVal;
    use js::error::ExnThrown;
    use js::error::{throw_type_error, ThrowException, TypeError};
    use js::gc::handle::RootedHeap;
    use js::gc::scope::Scope;
    use js::prelude::HandleValue;
    use js::promise::{PromiseFuture, PromiseOutcome};
    use js::Promise;

    /// <https://fetch.spec.whatwg.org/#dom-global-fetch>
    ///
    /// Note: This skips a lot of machinery relevant to browsers, and in particular their security
    /// model, including CORS, cache, and redirect taint.
    ///
    /// `input` is converted to a `RequestInfo` here rather than by the binding, so the adapter
    /// `register_request_input_adapter` registers can convert other values to a `Request` first.
    /// A failed conversion rejects the returned promise.
    pub fn fetch<'r>(
        scope: &'r Scope<'_>,
        input: HandleValue<'_>,
        init: Option<RequestInit<'_>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _p_ be `a new promise`.
        let p = Promise::new_pending(scope)?;
        // Note: only an active event loop drives the request, so without one _p_ is rejected
        // with a `TypeError` explaining why.
        if core_runtime::event_loop::reject_without_event_loop(scope, &p, "`fetch`")? {
            return Ok(p);
        }
        let input = match request_info(scope, input) {
            Ok(input) => input,
            Err(_) => {
                p.reject_with_pending(scope)?;
                return Ok(p);
            }
        };

        // Step 2: Let _requestObject_ be the result of invoking the initial value of `Request` as
        //     constructor with _input_ and _init_ as arguments. If this throws an exception,
        //     `reject` _p_ with it and return _p_.
        let request_object = match Request::new(scope, input, init) {
            Ok(object) => object,
            Err(_) => {
                p.reject_with_pending(scope)?;
                return Ok(p);
            }
        };

        // Step 3: Let _request_ be _requestObject_’s `request`.
        // All `request` state is stored on the object, so just rename it to align with the spec.
        let request = request_object;

        // Step 4: If _requestObject_’s `signal` is `aborted`, then:
        let signal = request.signal(scope);
        if signal.aborted() {
            // Step 4.1: `Abort the `fetch()` call` with _p_, _request_, null, and _requestObject_’s
            //     `signal`’s `abort reason`.
            let reason = signal.reason(scope);
            crate::abort::abort_fetch(scope, &p, &request, None, reason)?;
            // Step 4.2: Return _p_.
            return Ok(p);
        }
        let abort_state = crate::abort::register_fetch_abort(scope, signal, p, request)?;

        // Step 5: Let _globalObject_ be _request_’s `client`’s `global object`.
        // Step 6: If _globalObject_ is a `ServiceWorkerGlobalScope` object, then set _request_’s
        //     `service-workers mode` to "`none`".
        // Step 7: Let _responseObject_ be null.
        // Step 8: Let _relevantRealm_ be `this`’s `relevant realm`.
        // (not applicable)

        // Step 9: Let _locallyAborted_ be false.
        // Step 10: Let _controller_ be null.
        // (not applicable)

        // Step 11: `Add the following abort steps` to _requestObject_’s `signal`:
        // Step 11.1: Set _locallyAborted_ to true.
        // Step 11.2: `Assert`: _controller_ is non-null.
        // Step 11.3: `Abort` _controller_ with _requestObject_’s `signal`’s `abort reason`.
        // Step 11.4: `Abort the `fetch()` call` with _p_, _request_, _responseObject_, and
        //     _requestObject_’s `signal`’s `abort reason`.
        // Step 12: Set _controller_ to the result of calling `fetch` given _request_ and
        //     `_processResponse_` given _response_ being these steps:
        // Step 12.1: If _locallyAborted_ is true, then abort these steps.
        // Step 12.2: If _response_’s `aborted flag` is set, then:
        // Step 12.2.1: Let _deserializedError_ be the result of `deserialize a serialized abort
        //     reason` given _controller_’s `serialized abort reason` and _relevantRealm_.
        // Step 12.2.2: `Abort the `fetch()` call` with _p_, _request_, _responseObject_, and
        //     _deserializedError_.
        // Step 12.2.3: Abort these steps.
        // Step 12.3: If _response_ is a `network error`, then `reject` _p_ with a `TypeError` and
        //     abort these steps.
        // Step 12.4: Set _responseObject_ to the result of `creating` a `Response` object, given
        //     _response_, "`immutable`", and _relevantRealm_.
        // Step 12.5: `Resolve` _p_ with _responseObject_.
        //
        // _processResponse_ is applied below to whichever outcome `fetch` produced. The
        // _locallyAborted_ / `serialized abort reason` bookkeeping is not needed: an abort before
        // this point returned at Step 4, and rejecting an already-settled promise is a no-op.
        match crate::algorithms::fetch(scope, &request)? {
            // _processResponse_: Set _responseObject_ to the result of `creating` a `Response`
            // object, given _response_, "`immutable`", and _relevantRealm_ — done by `scheme
            // fetch`'s "`data`" arm — then `Resolve` _p_ with _responseObject_.
            crate::algorithms::FetchOutcome::Response(response_object) => {
                abort_state.deliver(scope, &response_object);
                p.resolve(scope, response_object)?;
                return Ok(p);
            }
            // _processResponse_: If _response_ is a `network error`, then `reject` _p_ with a
            // `TypeError` and abort these steps. Throw one to set the pending exception, then
            // take it as the rejection value.
            crate::algorithms::FetchOutcome::NetworkError => {
                abort_state.detach(scope);
                throw_type_error(scope, c"Failed to fetch");
                p.reject_with_pending(scope)?;
                return Ok(p);
            }
            // The response is produced by the host transport, so _processResponse_ runs when the
            // future below completes.
            crate::algorithms::FetchOutcome::Network => {}
        }
        // `main fetch` handed off to [`HTTP fetch`](https://fetch.spec.whatwg.org/#concept-http-fetch)
        // and below, which are `transport::send_following_redirects`. On completion,
        // _processResponse_: `response_from_platform` creates the `Response` object with an
        // "`immutable`" headers guard and _p_ resolves with it; a transport error is a `network
        // error`, rejecting _p_ with a `TypeError`.
        let platform_request = request.platform_request(scope);
        let url = request.current_url();
        let redirect_mode = request.redirect();
        let mode = request.mode();
        // The request's `origin` is "`client`", i.e. the requester's own origin, which is the
        // worker location's origin when one is set. It feeds `main fetch`'s origin/mode switch in
        // `send_following_redirects`.
        let request_origin =
            web_globals::worker_location::current_location_url().map(|url| url.origin());
        let method = platform_request.method.clone();
        // Root the abort state across the host fetch, so the resolve callback can
        // deliver the response to it, or detach it if the response fails.
        let abort_state = RootedHeap::new(abort_state);
        let future = async move {
            match crate::transport::send_following_redirects(
                platform_request,
                redirect_mode,
                mode,
                url,
                request_origin,
            )
            .await
            {
                Ok((response, url_list, tainting)) => {
                    PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
                        let response = response_from_platform(
                            scope,
                            response,
                            url_list,
                            &method,
                            redirect_mode,
                            tainting,
                        );
                        match &response {
                            Ok(response) => abort_state.get(scope).deliver(scope, response),
                            Err(_) => {
                                // Detaching runs with no exception pending.
                                let error = js::exception::take_pending(scope);
                                abort_state.get(scope).detach(scope);
                                if let Some(error) = error {
                                    js::exception::set_pending(
                                        scope,
                                        error,
                                        js::native::ExceptionStackBehavior::DoNotCapture,
                                    );
                                }
                            }
                        }
                        response?.to_jsval_throwing(scope)
                    }))
                }
                Err(error) => PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
                    abort_state.get(scope).detach(scope);
                    Err(TypeError(format!("Failed to fetch: {error}")).throw(scope))
                })),
            }
        };
        p.spawn(PromiseFuture::new(future));

        // Step 13: Return _p_.
        Ok(p)
    }
}
