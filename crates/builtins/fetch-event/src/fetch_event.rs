// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/ServiceWorker/#fetchevent-interface>

use super::extendable_event::{ExtendableEvent, ExtendableEventImpl, ExtendableEventInit};
use core_runtime::event_loop::{with_active_event_loop, InterestHandle};
use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::class::Ref;
use js::conversion::FromJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::cast_payload;
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::{HandleValue, OptionHeapExt};
use js::{value, Function, Promise, PromiseOf};
use std::net::SocketAddr;
use std::ops::Deref;
use web_fetch::request::{Request, RequestImpl};
use web_fetch::response::{Response, ResponseImpl};
use web_globals::dom_exception::{DOMException, DOMExceptionError};
use web_globals::events::algorithms::ScriptStackState;
use web_globals::events::EventTarget;

/// <https://w3c.github.io/ServiceWorker/#dictdef-fetcheventinit>
// Note: the missing fields are intentionally omitted. The only difference that makes is that
// validation will accept things the spec wouldn't, but that's really marginal: it'd reject
// e.g. a `resultingClientId` property where `toString()` throws, but not much else.
#[webidl_dictionary(extends = ExtendableEventInit)]
pub struct FetchEventInit<'a> {
    parent: ExtendableEventInit,
    pub request: Request<'a>, // WebIDL: Request
    #[webidl(default = String::new())]
    pub client_id: String,
}

#[webidl_interface(extends = ExtendableEvent)]
pub struct FetchEvent {
    parent: ExtendableEventImpl,
    /// <https://w3c.github.io/ServiceWorker/#fetchevent-potential-response>
    /// (a response), initially set to None, and the following associated flags that are initially
    /// unset:
    potential_response: Option<Heap<ResponseImpl>>,
    /// The header list `potential_response` had when `respondWith`'s promise settled.
    /// There's a window between that point and when the response is sent out in which guest code
    /// can run, and mutate the headers.
    potential_response_headers: Option<Vec<(String, String)>>,
    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-request>
    request: Heap<RequestImpl>, // WebIDL: Request
    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-clientid>
    client_id: String,
    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-handled>
    handled: Heap<js::promise::Promise>,
    /// The loop-interest held while the `respondWith` promise is pending. The
    /// handle targets the loop that was active at `respondWith` time, so the
    /// settle reaction releases the right loop even when it runs during
    /// another request's turn.
    #[no_trace]
    respond_interest: Option<InterestHandle>,
    /// <https://w3c.github.io/ServiceWorker/#fetchevent-respond-with-entered-flag>
    respond_with_entered: bool,
    /// <https://w3c.github.io/ServiceWorker/#fetchevent-wait-to-respond-flag>
    wait_to_respond: bool,
    /// <https://w3c.github.io/ServiceWorker/#fetchevent-respond-with-error-flag>
    respond_with_error: bool,
    /// The address of the client that sent the request, if the transport provides it.
    #[no_trace]
    client_address: Option<SocketAddr>,
    // Note: `preloadResponse`, `resultingClientId`, and `replacesClientId` omitted intentionally.
    // The first isn't supported anywhere, and all three don't really matter to our use cases.
}

#[webidl_methods]
impl FetchEvent {
    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-fetchevent>
    #[constructor]
    fn js_ctor(
        scope: &Scope<'_>,
        r#type: &str,
        event_init_dict: &FetchEventInit<'_>,
    ) -> Result<Self, ExnThrown> {
        // The spec-defined script constructor; the runtime creates its events via
        // `create_for_request` instead, and only those events are ever dispatched.
        // WPT still contains tests for the JS constructor though.
        let handled = Promise::new_pending(scope)?;
        Ok(FetchEventImpl {
            parent: ExtendableEventImpl::new(r#type, Some(event_init_dict.deref())),
            potential_response: None,
            potential_response_headers: None,
            request: Heap::from(event_init_dict.request),
            client_id: event_init_dict.client_id.clone(),
            handled: Heap::from(handled),
            respond_interest: None,
            respond_with_entered: false,
            wait_to_respond: false,
            respond_with_error: false,
            client_address: None,
        })
    }

    /// [`Create Fetch Event and Dispatch`]: https://w3c.github.io/ServiceWorker/#create-fetch-event-and-dispatch
    pub fn create_for_request(scope: &Scope, request: Request) -> Result<Self, ExnThrown> {
        // Step 17.4.11: Initialize _e_’s `handled` to _eventHandled_.
        // Step 17.2 creates _eventHandled_ before queueing the task; the event is the only thing
        // carrying it here, so it is created with the event.
        let handled = Promise::new_pending(scope)?;
        Ok(FetchEventImpl {
            // Step 17.4.4: Initialize _e_’s `type` attribute to `fetch`.
            // Step 17.4.5: Initialize _e_’s `cancelable` attribute to true.
            parent: ExtendableEventImpl::new("fetch", Some(&ExtendableEventInit::new(true))),
            potential_response: None,
            potential_response_headers: None,
            // Step 17.4.6: Initialize _e_’s `request` attribute to _requestObject_.
            request: Heap::from(request),
            // Step 17.4.7: Initialize _e_’s `preloadResponse` to _preloadResponse_.
            // (n/a)
            // Step 17.4.8: If _client_ is not null, initialize _e_’s `clientId` attribute to
            //     _client_’s `id`.
            // (n/a)
            client_id: String::new(),
            // Step 17.4.9: If _request_ is a `non-subresource request`, _request_’s `destination`
            //     is not `"report"`, and _reservedClient_ is not null, initialize _e_’s
            //     `resultingClientId` attribute to _reservedClient_’s `id`.
            // (n/a)
            // Step 17.4.10: If _request_ is a `navigation request`, initialize _e_’s
            //     `replacesClientId` attribute to _request_’s `replaces client id`.
            // (n/a)
            handled: Heap::from(handled),
            respond_interest: None,
            respond_with_entered: false,
            wait_to_respond: false,
            respond_with_error: false,
            client_address: None,
        }
        // The event is platform-created, so it is trusted; `respondWith` and
        // `waitUntil` refuse to run on an event that isn't.
        .trusted())
    }

    /// Whether the worker has any `fetch` listener — the spec's
    /// [`all fetch listeners are empty flag`], inverted.
    ///
    /// [`all fetch listeners are empty flag`]: https://w3c.github.io/ServiceWorker/#dfn-all-fetch-listeners-are-empty-flag
    pub fn has_listener(scope: &Scope) -> bool {
        scope
            .global()
            .cast::<EventTarget>()
            .expect("The global is an event target")
            .has_listener_for("fetch")
    }

    /// Dispatch an incoming request: create a `FetchEvent` for `request` and deliver it to the
    /// global's `fetch` listeners. The caller must drive the per-request event loop.
    ///
    /// Step 17.4.13 of the [`Create Fetch Event and Dispatch`] task, whose surrounding steps are
    /// inlined into the serve path (see [`crate`]).
    ///
    /// [`Create Fetch Event and Dispatch`]: https://w3c.github.io/ServiceWorker/#create-fetch-event-and-dispatch
    pub fn dispatch(
        scope: &'s Scope,
        request: Request,
        script_stack_state: ScriptStackState,
    ) -> Result<FetchEvent<'s>, ExnThrown> {
        Self::dispatch_from(scope, request, script_stack_state, None)
    }

    /// [`FetchEvent::dispatch`], with the address of the client that sent the request, if the
    /// transport provides it, which the event's [`client_address`](FetchEvent::client_address)
    /// returns.
    pub fn dispatch_from(
        scope: &'s Scope,
        request: Request,
        script_stack_state: ScriptStackState,
        client_address: Option<SocketAddr>,
    ) -> Result<FetchEvent<'s>, ExnThrown> {
        let event = FetchEvent::create_for_request(scope, request)?;
        event.data_mut().client_address = client_address;
        // Step 17.4.13: `Dispatch` _e_ at _activeWorker_’s `global object`.
        let target = scope
            .global()
            .cast::<EventTarget>()
            .expect("The global is an event target");
        target.dispatch_trusted(scope, **event, script_stack_state);
        // Step 17.4.14: Invoke `Update Service Worker Extended Events Set` with _activeWorker_ and
        //     _e_.
        // n/a lifetime tracking is handled using `InterestHandle` instead.
        Ok(event)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-request>
    #[getter]
    pub fn request<'r>(&self, scope: &'r Scope) -> Request<'r> {
        self.data().request.get(scope)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-clientid>
    #[getter]
    pub fn client_id(&self) -> Ref<'_, str> {
        Ref::map(self.data(), |data| data.client_id.as_str())
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-resultingclientid>
    #[getter]
    pub fn resulting_client_id(&self) -> String {
        String::new()
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-handled>
    #[getter]
    pub fn handled<'r>(&self, scope: &'r Scope) -> Promise<'r> {
        self.data().handled.get(scope)
    }

    /// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-respondwith>
    // `r` is a WebIDL `Promise<Response>`: the trampoline accepts any value, hands over a promise
    // resolved with it, and rejects that promise if what it settles with isn't a `Response`. That
    // rejection lands in step 9 below, which sets the `respond-with error flag` — the same flag
    // step 10.1 sets for a non-`Response` fulfillment, which is why step 10.1 is unreachable here
    // (see `on_respond_fulfilled`).
    #[method]
    pub fn respond_with(
        &self,
        scope: &Scope,
        r: PromiseOf<'_, Response<'_>>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Let _event_ be `this`.
        // Step 2: If _event_’s `dispatch flag` is unset, `throw` an "`InvalidStateError`"
        //     `DOMException`.
        if !self.is_dispatching() {
            return Err(DOMExceptionError::new(
                "InvalidStateError",
                "respondWith must be called during fetch event dispatch",
            )
            .throw(scope));
        }
        // Step 3: If _event_’s `respond-with entered flag` is set, `throw` an
        //     "`InvalidStateError`" `DOMException`.
        if self.data().respond_with_entered {
            return Err(DOMExceptionError::new(
                "InvalidStateError",
                "respondWith has already been called on this FetchEvent",
            )
            .throw(scope));
        }
        // Step 4: `Add lifetime promise` _r_ to _event_.
        //     Spec note: Note: `event.respondWith(r)` extends the lifetime of the event by default
        //     as if `event.waitUntil(r)` is called.
        // (Inlined, to fold the `waitUntil` reaction and the one required here into one promise
        // reaction.)
        // [inlined `add lifetime promise`] Steps 1–2.
        self.check_can_extend_lifetime(scope, "respondWith")?;
        // [inlined `add lifetime promise`] Step 5: Upon `fulfillment` or `rejection` of _promise_,
        //     `queue a microtask` to run these substeps:
        // The same reactions that run steps 9 and 10 below; step 5.1's decrement is
        // [`ExtendableEvent::note_lifetime_promise_settled`].
        // Step 9: `Upon rejection` of _r_:
        let on_fulfilled = Function::new_callback(scope, c"", 1, on_respond_fulfilled, *self)?;
        // Step 10: `Upon fulfillment` of _r_ with _response_:
        let on_rejected = Function::new_callback(scope, c"", 1, on_respond_rejected, *self)?;
        r.add_reactions_ignoring_unhandled_rejection(
            scope,
            Some(*on_fulfilled),
            Some(*on_rejected),
        )?;
        // [inlined `add lifetime promise`] Step 3: Add _promise_ to _event_’s `extend lifetime
        //     promises`.
        self.data_mut().respond_interest = Some(
            with_active_event_loop(|el| el.acquire_interest_handle())
                .expect("event loop is active"),
        );
        // [inlined `add lifetime promise`] Step 4: Increment _event_’s `pending promises count` by
        //     one.
        self.note_lifetime_promise_pending();
        // Step 5: Set _event_’s `stop propagation flag` and `stop immediate propagation flag`.
        self.stop_immediate_propagation();
        // Step 6: Set _event_’s `respond-with entered flag`.
        // Step 7: Set _event_’s `wait to respond flag`.
        {
            let mut data = self.data_mut();
            data.respond_with_entered = true;
            data.wait_to_respond = true;
        }
        // Step 8: Let _targetRealm_ be _event_’s `relevant Realm`.
        // (Only used by step 10.2.5's body copy, see `on_respond_fulfilled`.)
        Ok(())
    }
}

impl FetchEventImpl {
    /// Marks `self` as trusted and returns it.
    #[js::allow_unrooted]
    fn trusted(mut self) -> Self {
        self.set_trusted(true);
        self
    }
}

/// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-respondwith>.
/// Step 10: Upon fulfillment` of _r_ with _response_
fn on_respond_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let event = cast_payload::<FetchEvent>(scope, payload);
    // Step 10.1: If _response_ is not a `Response` object, then set the `respond-with error
    //     flag`.
    let response =
        Response::from_jsval(scope, args.get(0), ()).expect("WebIDL conversion ensures this");
    // Step 10.2: Else:
    // Step 10.2.2.1: If _response_ is `unusable`, then set the `respond-with error flag` and abort
    //     these steps.
    let unusable = response.is_body_unusable(scope);
    if !unusable {
        // Steps 10.2.2 to 10.2.5 are implemented at the platform layer instead of here, except
        // for the effect step 10.2.2's transform has on the response handed in, which is to take
        // its body, and step 10.2.5.1's getting a reader for the body's stream, which
        // `reserve_body_for_sending` acquires now so author code can no longer read from it.
        response.reserve_body_for_sending(scope)?;
    }
    {
        let mut data = event.data_mut();
        if unusable {
            data.respond_with_error = true;
        } else {
            // Step 10.2.1: Let _potentialResponse_ be a copy of _response_’s associated response,
            //              except for its body.
            // Note: We only copy the headers, since everything else is effectively unchangeable
            // from content.
            data.potential_response_headers = Some(response.headers_list(scope));
            // Step 10.2.6: Set _event_’s `potential response` to _potentialResponse_.
            data.potential_response.set(response);
        }
        // Step 10.3: Unset _event_’s `wait to respond flag`.
        data.wait_to_respond = false;
        // [inlined `add lifetime promise`] Step 5.2:
        drop(data.respond_interest.take());
    }
    // [inlined `add lifetime promise`] Step 5.1:
    event.note_lifetime_promise_settled();
    if unusable {
        eprintln!("fetch event: the Response passed to respondWith has an unusable body");
    }
    Ok(value::undefined())
}

/// <https://w3c.github.io/ServiceWorker/#dom-fetchevent-respondwith>.
/// Step 9: `Upon rejection` of _r_:
fn on_respond_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let event = cast_payload::<FetchEvent>(scope, payload);
    {
        let mut data = event.data_mut();
        // Step 9.1: Set _event_’s `respond-with error flag`.
        data.respond_with_error = true;
        // Step 9.2: Unset _event_’s `wait to respond flag`.
        data.wait_to_respond = false;
        // [inlined `add lifetime promise`] Step 5.2:
        drop(data.respond_interest.take());
    }
    // [inlined `add lifetime promise`] Step 5.1:
    event.note_lifetime_promise_settled();
    eprintln!(
        "fetch event: the promise passed to respondWith rejected with {}",
        describe_rejection(scope, args.get(0))
    );
    Ok(value::undefined())
}

/// A rejection reason as text for the warning above.
///
/// Stringifying runs author code (`toString`), which can itself throw; the diagnostic swallows
/// that rather than leaving an exception pending for whatever runs next in this reaction.
fn describe_rejection(scope: &Scope<'_>, reason: HandleValue<'_>) -> String {
    match String::from_jsval(scope, reason, ()) {
        Ok(text) => text,
        Err(_) => {
            js::exception::clear(scope);
            "a value that could not be converted to a string".to_string()
        }
    }
}

impl<'s> FetchEvent<'s> {
    /// Whether the `respond-with entered flag` is set: `Create Fetch Event and Dispatch` step
    /// 17.4.15, which decides at step 21 whether the event answered the request at all.
    pub fn respond_with_entered(&self) -> bool {
        self.data().respond_with_entered
    }

    /// Settle the `handled` promise: steps 21–23 of
    /// [Create Fetch Event and Dispatch](https://w3c.github.io/ServiceWorker/#create-fetch-event-and-dispatch),
    /// where the event's outcome becomes final. `responded` says whether a `Response` is on its way
    /// to the client.
    ///
    /// The spec settles `handled` three ways: resolve once a response is on its way (step 23),
    /// reject when the event failed (steps 21.1.1 and 22.1), and, when the handler never called
    /// `respondWith`, resolve (step 21.2), because that case returns null, which tells `Fetch`
    /// to go get the response from the network itself.
    ///
    /// There is no network to fall back to here, so that third case ends in a `500` like the
    /// failures do, and resolving would tell the handler its request was served. So this resolves
    /// only when `responded`, and rejects otherwise. Which failure it was only affects the
    /// rejection's message, which [`respond_with_error`](Self::respond_with_error_set) selects.
    pub fn settle_handled(&self, scope: &'s Scope<'_>, responded: bool) {
        let handled = self.data().handled.get(scope);
        if !handled.is_pending() {
            return;
        }
        let result = if responded {
            handled.resolve(scope, value::undefined())
        } else {
            let reason = if self.data().respond_with_error {
                "the promise passed to respondWith did not produce a Response"
            } else {
                "the fetch event was not responded to"
            };
            match DOMException::new(scope, reason.into(), "NetworkError".into()) {
                Ok(exception) => handled.reject(scope, exception),
                Err(_) => handled.reject_with_pending(scope),
            }
            .and_then(|()| {
                // Most handlers never look at `handled`, and a rejection nobody subscribed to is
                // reported as an unhandled rejection, which would put a spurious warning on the
                // stderr of every failed request.
                handled.set_settled_is_handled(scope)
            })
        };
        if result.is_err() {
            // Settling is bookkeeping on the way to the response. A failure here must not leave an
            // exception pending for the dispatch's next JSAPI call to trip over.
            js::exception::report_and_clear(scope, "settling FetchEvent.handled");
        }
    }

    /// Whether the `respond-with error flag` is set: `respondWith` was called and failed.
    pub fn respond_with_error_set(&self) -> bool {
        self.data().respond_with_error
    }

    /// The response provided by awaiting the value passed to `respondWith`.
    /// Note: the response's headers might've been changed by content in the meantime. Use
    /// [`Self::take_potential_response_headers`] to read them instead.
    pub fn potential_response(&self, scope: &'s Scope<'_>) -> Option<Response<'s>> {
        self.data().potential_response.get(scope)
    }

    /// A copy of response headers as a `Vec<String, String>`, taken right after the value passed
    /// to `respondWith` resolved, and before content had an opportunity to change the headers in
    /// the meantime.
    ///
    /// Can only be invoked once and transfers ownership of the list.
    pub fn take_potential_response_headers(&self) -> Option<Vec<(String, String)>> {
        self.data_mut().potential_response_headers.take()
    }

    /// The spec's "wait to respond" flag: true from `respondWith` being called until its promise
    /// settles.
    pub fn wait_to_respond_set(&self) -> bool {
        self.data().wait_to_respond
    }

    /// The address of the client that sent the request, if the transport provides it. Native
    /// serving provides it, and `wasi:http` doesn't.
    pub fn client_address(&self) -> Option<SocketAddr> {
        self.data().client_address
    }

    /// Drop the event loop interest for the `respondWith` promise.
    ///
    /// The promise itself might still settle, and its reactions run, but the event loop won't
    /// be kept alive if this was the only remaining interest.
    pub fn abandon_respond(&self) {
        drop(self.data_mut().respond_interest.take());
    }
}
