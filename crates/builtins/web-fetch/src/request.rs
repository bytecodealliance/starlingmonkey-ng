// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://fetch.spec.whatwg.org/>

use super::{
    algorithms::{self, Body, BodySource},
    body_mixin::BodyMixin,
    headers::{Guard, HeaderList, Headers, HeadersImpl, HeadersInit},
};
use crate::body_mixin::BodyInit;
use crate::body_mixin::BodyStorage;
use crate::incoming_body::HostBackedBodyOwner;
use core_runtime::config;
use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods, webidl_union};
use js::class::create_instance_with;
use js::class::Ref;
use js::error::{throw_type_error, ExnThrown};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Promise;
use url::Url;
use web_globals::signals::algorithms::create_dependent_abort_signal;
use web_globals::signals::{AbortSignal, AbortSignalImpl};
use web_streams::readable::readable_stream::ReadableStream;

js::webidl_enum! {
    pub enum RequestDestination {
        Empty => "",
        Audio => "audio",
        Audioworklet => "audioworklet",
        Document => "document",
        Embed => "embed",
        Font => "font",
        Frame => "frame",
        Iframe => "iframe",
        Image => "image",
        Json => "json",
        Manifest => "manifest",
        Object => "object",
        Paintworklet => "paintworklet",
        Report => "report",
        Script => "script",
        Sharedworker => "sharedworker",
        Style => "style",
        Text => "text",
        Track => "track",
        Video => "video",
        Worker => "worker",
        Xslt => "xslt",
    }
}

js::webidl_enum! {
    /// <https://w3c.github.io/webappsec-referrer-policy/#enumdef-referrerpolicy>
    pub enum ReferrerPolicy {
        Empty => "",
        NoReferrer => "no-referrer",
        NoReferrerWhenDowngrade => "no-referrer-when-downgrade",
        SameOrigin => "same-origin",
        Origin => "origin",
        StrictOrigin => "strict-origin",
        OriginWhenCrossOrigin => "origin-when-cross-origin",
        StrictOriginWhenCrossOrigin => "strict-origin-when-cross-origin",
        UnsafeUrl => "unsafe-url",
    }
}

js::webidl_enum! {
    pub enum RequestMode {
        Navigate => "navigate",
        SameOrigin => "same-origin",
        NoCors => "no-cors",
        Cors => "cors",
    }
}

js::webidl_enum! {
    pub enum RequestCredentials {
        Omit => "omit",
        SameOrigin => "same-origin",
        Include => "include",
    }
}

js::webidl_enum! {
    pub enum RequestCache {
        Default => "default",
        NoStore => "no-store",
        Reload => "reload",
        NoCache => "no-cache",
        ForceCache => "force-cache",
        OnlyIfCached => "only-if-cached",
    }
}

js::webidl_enum! {
    pub enum RequestRedirect {
        Follow => "follow",
        Error => "error",
        Manual => "manual",
    }
}

js::webidl_enum! {
    pub enum RequestDuplex {
        Half => "half",
    }
}

js::webidl_enum! {
    pub enum RequestPriority {
        High => "high",
        Low => "low",
        Auto => "auto",
    }
}

/// A request's referrer.
///
/// <https://fetch.spec.whatwg.org/#concept-request-referrer>
#[derive(Debug, Clone, Default)]
pub enum Referrer {
    /// "`no-referrer`"
    NoReferrer,
    /// "`client`"
    #[default]
    Client,
    /// A URL.
    Url(Url),
}

/// <https://fetch.spec.whatwg.org/#concept-request>
///
/// The spec's *request* is an internal struct distinct from the `Request`
/// interface object, which wraps it.
#[derive(Clone)]
pub struct RequestRecord {
    /// <https://fetch.spec.whatwg.org/#concept-request-method>
    pub method: String,
    /// <https://fetch.spec.whatwg.org/#concept-request-url-list>
    /// A non-empty list; the current URL is the last entry.
    pub url_list: Vec<Url>,
    /// <https://fetch.spec.whatwg.org/#concept-request-destination>
    pub destination: RequestDestination,
    /// <https://fetch.spec.whatwg.org/#concept-request-referrer>
    pub referrer: Referrer,
    /// <https://fetch.spec.whatwg.org/#concept-request-referrer-policy>
    pub referrer_policy: ReferrerPolicy,
    /// <https://fetch.spec.whatwg.org/#concept-request-mode>
    pub mode: RequestMode,
    /// <https://fetch.spec.whatwg.org/#concept-request-credentials-mode>
    pub credentials_mode: RequestCredentials,
    /// <https://fetch.spec.whatwg.org/#concept-request-cache-mode>
    pub cache_mode: RequestCache,
    /// <https://fetch.spec.whatwg.org/#concept-request-redirect-mode>
    pub redirect_mode: RequestRedirect,
    /// <https://fetch.spec.whatwg.org/#concept-request-integrity-metadata>
    pub integrity_metadata: String,
    /// <https://fetch.spec.whatwg.org/#request-keepalive-flag>
    pub keepalive: bool,
    /// <https://fetch.spec.whatwg.org/#request-priority>
    pub priority: RequestPriority,
    /// <https://fetch.spec.whatwg.org/#concept-request-reload-navigation-flag>
    pub reload_navigation: bool,
    /// <https://fetch.spec.whatwg.org/#concept-request-history-navigation-flag>
    pub history_navigation: bool,
    /// <https://fetch.spec.whatwg.org/#unsafe-request-flag>
    pub unsafe_request: bool,
    /// <https://fetch.spec.whatwg.org/#use-cors-preflight-flag>
    pub use_cors_preflight: bool,
}

impl Default for RequestRecord {
    fn default() -> Self {
        // The defaults of a "new request" per the spec's request struct.
        Self {
            method: "GET".to_string(),
            url_list: Vec::new(),
            destination: RequestDestination::Empty,
            referrer: Referrer::Client,
            referrer_policy: ReferrerPolicy::Empty,
            mode: RequestMode::NoCors,
            credentials_mode: RequestCredentials::SameOrigin,
            cache_mode: RequestCache::Default,
            redirect_mode: RequestRedirect::Follow,
            integrity_metadata: String::new(),
            keepalive: false,
            priority: RequestPriority::Auto,
            reload_navigation: false,
            history_navigation: false,
            unsafe_request: false,
            use_cors_preflight: false,
        }
    }
}

impl RequestRecord {
    /// <https://fetch.spec.whatwg.org/#concept-request-current-url>
    /// The current URL is the last URL in the URL list.
    pub fn current_url(&self) -> &Url {
        self.url_list.last().expect("request URL list is non-empty")
    }
}

/// Parse `input` as a URL, resolving relative URLs against `base` (the API base
/// URL). An empty string resolves to the base URL.
fn parse_url(input: &str, base: Option<&Url>) -> Option<Url> {
    Url::options().base_url(base).parse(input).ok()
}

/// Returns `init` if present, unless it has no present member.
fn request_init_if_not_empty(init: Option<RequestInit>) -> Option<RequestInit> {
    let init = init?;
    let not_empty = init.method.is_some()
        || init.headers.is_some()
        || init.body.is_some()
        || init.referrer.is_some()
        || init.referrer_policy.is_some()
        || init.mode.is_some()
        || init.credentials.is_some()
        || init.cache.is_some()
        || init.redirect.is_some()
        || init.integrity.is_some()
        || init.keepalive.is_some()
        || init.signal.is_some()
        || init.duplex.is_some()
        || init.priority.is_some()
        || init.window.is_some();
    not_empty.then_some(init)
}

/// <https://fetch.spec.whatwg.org/#request-class>
#[webidl_interface]
pub struct Request {
    /// <https://fetch.spec.whatwg.org/#concept-request-request>
    /// Note: The header list lives in `headers` and the body in `body`/`body_stream`.
    #[no_trace]
    request: RequestRecord,
    /// <https://fetch.spec.whatwg.org/#concept-request-body>, with its stream and host body.
    /// The host body is set only for a request built by [`Request::from_incoming`].
    body: BodyStorage,
    /// <https://fetch.spec.whatwg.org/#request-headers>
    headers: Heap<HeadersImpl>,
    /// <https://fetch.spec.whatwg.org/#request-signal>
    signal: Heap<AbortSignalImpl>,
}

/// <https://fetch.spec.whatwg.org/#requestinfo>
#[webidl_union]
pub enum RequestInfo<'s> {
    Request(Request<'s>),
    String(String),
}

/// Converts a value `fetch` receives as its input into a `Request`, before the
/// value is converted to a [`RequestInfo`]. Returns `None` for a value it can't
/// convert.
pub type RequestInputAdapter =
    for<'s> fn(&'s Scope<'_>, HandleValue<'_>) -> Result<Option<Request<'s>>, ExnThrown>;

js::instance_local! {
    /// The adapter [`register_request_input_adapter`] registered.
    static REQUEST_INPUT_ADAPTER: std::cell::Cell<Option<RequestInputAdapter>> =
        const { std::cell::Cell::new(None) };
}

/// Register `adapter` to convert the values `fetch` receives as its input,
/// replacing any adapter registered before.
pub fn register_request_input_adapter(adapter: RequestInputAdapter) {
    REQUEST_INPUT_ADAPTER.with(|cell| cell.set(Some(adapter)));
}

/// Convert `value` to a [`RequestInfo`].
///
/// If an adapter was registered with [`register_request_input_adapter`], that'll be tried first.
/// If that fails to convert, the normal `RequestInfo` conversion is used.
pub(crate) fn request_info<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<RequestInfo<'s>, ExnThrown> {
    use js::conversion::FromJSVal;
    if let Some(adapter) = REQUEST_INPUT_ADAPTER.with(std::cell::Cell::get) {
        if let Some(request) = adapter(scope, value)? {
            return Ok(RequestInfo::Request(request));
        }
    }
    RequestInfo::from_jsval_throwing(scope, value, ())
}

#[webidl_methods]
impl Request {
    /// Stop reporting the body's bytes to the GC. A compacting GC may have moved the object
    /// since `sync_body_accounting` ran, so the finalizer's address is used here.
    #[destructor]
    fn release_body_accounting(&mut self, object: *mut js::native::JSObject) {
        // SAFETY: `object` is the object being finalized, whose private data this is.
        unsafe { self.body.accounted.release(object) };
    }

    /// <https://fetch.spec.whatwg.org/#dom-request>
    #[constructor]
    fn new(
        &self,
        scope: &Scope<'_>,
        input: RequestInfo<'_>,
        init: Option<RequestInit>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Let _request_ be null.
        let mut request: RequestRecord;
        // Step 2: Let _fallbackMode_ be null.
        let fallback_mode: Option<RequestMode>;
        // Step 3: Let _baseURL_ be `this`’s `relevant settings object`’s `API base URL`.
        let base_url = web_globals::worker_location::current_location_url();
        // Step 4: Let _signal_ be null.
        let mut signal: Option<AbortSignal<'_>> = None;
        let init_header_list: HeaderList;
        let mut input_body: Option<Body> = None;
        let mut input_request: Option<Request<'_>> = None;
        match input {
            // Step 5: If _input_ is a string, then:
            RequestInfo::String(input) => {
                // Step 5.1: Let _parsedURL_ be the result of `parsing` _input_ with _baseURL_.
                let parsed = parse_url(&input, base_url.as_ref()).ok_or_else(|| {
                    // Step 5.2: If _parsedURL_ is failure, then `throw` a `TypeError`.
                    throw_type_error(scope, c"Failed to parse URL from Request input")
                })?;
                // Step 5.3: If _parsedURL_ `includes credentials`, then `throw` a `TypeError`.
                if config::enforce_fetch_restrictions()
                    && !input.is_empty()
                    && (!parsed.username().is_empty() || parsed.password().is_some())
                {
                    return Err(throw_type_error(
                        scope,
                        c"Request URL must not include credentials",
                    ));
                }
                // Step 5.4: Set _request_ to a new `request` whose `URL` is _parsedURL_.
                request = RequestRecord {
                    url_list: vec![parsed],
                    ..Default::default()
                };
                // Step 5.5: Set _fallbackMode_ to "`cors`".
                fallback_mode = Some(RequestMode::Cors);
                init_header_list = HeaderList::new();
            }
            // Step 6: Otherwise:
            RequestInfo::Request(input_req) => {
                // Step 6.1: `Assert`: _input_ is a `Request` object.
                // Step 6.2: Set _request_ to _input_’s `request`.
                request = input_req.data().request.clone();
                // Step 6.3: Set _signal_ to _input_’s `signal`.
                signal = Some(input_req.data().signal.get(scope));
                init_header_list = input_req
                    .data()
                    .headers
                    .get(scope)
                    .data()
                    .header_list
                    .clone();
                input_body = input_req.data().body.record.clone();
                input_request = Some(input_req);
                fallback_mode = None;
            }
        }

        // Step 7: Let _origin_ be `this`’s `relevant settings object`’s `origin`.
        // Step 8: Let _traversableForUserPrompts_ be "`client`".
        // Step 9: If _request_’s `traversable for user prompts` is an `environment settings object`
        //     and its `origin` is `same origin` with _origin_, then set _traversableForUserPrompts_
        //     to _request_’s `traversable for user prompts`.
        // (Not applicable: there is no origin or window model.)

        // Step 10: If _init_["`window`"] `exists` and is non-null, then `throw` a `TypeError`.
        // (There is no window in this runtime, so the rejection is a browser-model rule, gated by
        // the request-restrictions switch.)
        if config::enforce_fetch_restrictions() {
            if let Some(window) = init.as_ref().and_then(|i| i.window.as_ref()) {
                if !window.is_null_or_undefined() {
                    return Err(throw_type_error(scope, c"Request window must be null"));
                }
            }
        }
        // Step 11: If _init_["`window`"] `exists`, then set _traversableForUserPrompts_ to
        //     "`no-traversable`".
        // (Not applicable.)

        // Step 12: Set _request_ to a new `request` with the following properties:
        // (snip: the properties are copied from the input request or n/a, so the copy is implicit —
        // except for the `unsafe-request flag`, which step 12 sets unconditionally:)
        request.unsafe_request = true;

        let mut init = request_init_if_not_empty(init);
        // Step 13: If _init_ `is not empty`, then:
        if let Some(init) = &mut init {
            // Step 13.1: If _request_’s `mode` is "`navigate`", then set it to "`same-origin`".
            if request.mode == RequestMode::Navigate {
                request.mode = RequestMode::SameOrigin;
            }
            // Step 13.2: Unset _request_’s `reload-navigation flag`.
            request.reload_navigation = false;
            // Step 13.3: Unset _request_’s `history-navigation flag`.
            request.history_navigation = false;
            // Step 13.4: Set _request_’s `origin` to "`client`".
            // (Not applicable.)
            // Step 13.5: Set _request_’s `referrer` to "`client`".
            request.referrer = Referrer::Client;
            // Step 13.6: Set _request_’s `referrer policy` to the empty string.
            request.referrer_policy = ReferrerPolicy::Empty;
            // Step 13.7: Set _request_’s `URL` to _request_’s `current URL`.
            // Step 13.8: Set _request_’s `URL list` to « _request_’s `URL` ».
            let current = request.current_url().clone();
            request.url_list = vec![current];
            // Step 14: If _init_["`referrer`"] `exists`, then:
            // Step 14.1: Let _referrer_ be _init_["`referrer`"].
            if let Some(referrer) = &init.referrer {
                // Step 14.2: If _referrer_ is the empty string, then set _request_’s `referrer` to
                //     "`no-referrer`".
                if referrer.is_empty() {
                    request.referrer = Referrer::NoReferrer;
                } else {
                    // Step 14.3: Otherwise:
                    // Step 14.3.1: Let _parsedReferrer_ be the result of `parsing` _referrer_ with
                    //     _baseURL_.
                    let parsed = parse_url(referrer, base_url.as_ref()).ok_or_else(|| {
                        // Step 14.3.2: If _parsedReferrer_ is failure, then `throw` a
                        //     `TypeError`.
                        throw_type_error(scope, c"Failed to parse referrer URL")
                    })?;
                    // Step 14.3.3: If one of the following is true
                    //   - _parsedReferrer_’s `scheme` is "`about`" and `path` is the string
                    //     "`client`"
                    //   - _parsedReferrer_’s `origin` is not `same origin` with _origin_
                    //     then set _request_’s `referrer` to "`client`".
                    // (There is no origin model, so only the `about:client` case applies.)
                    if parsed.scheme() == "about" && parsed.path() == "client" {
                        request.referrer = Referrer::Client;
                    } else {
                        // Step 14.3.4: Otherwise, set _request_’s `referrer` to _parsedReferrer_.
                        request.referrer = Referrer::Url(parsed);
                    }
                }
            }
            // Step 15: If _init_["`referrerPolicy`"] `exists`, then set _request_’s `referrer
            //     policy` to it.
            if let Some(policy) = init.referrer_policy {
                request.referrer_policy = policy;
            }
        }
        // Step 16: Let _mode_ be _init_["`mode`"] if it `exists`, and _fallbackMode_ otherwise.
        let mode = init.as_ref().and_then(|i| i.mode).or(fallback_mode);
        // Step 17: If _mode_ is "`navigate`", then `throw` a `TypeError`.
        if config::enforce_fetch_restrictions() && mode == Some(RequestMode::Navigate) {
            return Err(throw_type_error(
                scope,
                c"Request mode must not be navigate",
            ));
        }
        // Step 18: If _mode_ is non-null, set _request_’s `mode` to _mode_.
        if let Some(mode) = mode {
            request.mode = mode;
        }
        if let Some(init) = &init {
            // Step 19: If _init_["`credentials`"] `exists`, then set _request_’s `credentials
            //     mode` to it.
            if let Some(credentials) = init.credentials {
                request.credentials_mode = credentials;
            }
            // Step 20: If _init_["`cache`"] `exists`, then set _request_’s `cache mode` to it.
            if let Some(cache) = init.cache {
                request.cache_mode = cache;
            }
        }
        // Step 21: If _request_’s `cache mode` is "`only-if-cached`" and _request_’s `mode` is
        //     _not_ "`same-origin`", then `throw` a `TypeError`.
        if config::enforce_fetch_restrictions()
            && request.cache_mode == RequestCache::OnlyIfCached
            && request.mode != RequestMode::SameOrigin
        {
            return Err(throw_type_error(
                scope,
                c"cache mode 'only-if-cached' can only be used with mode 'same-origin'",
            ));
        }
        if let Some(init) = &mut init {
            // Step 22: If _init_["`redirect`"] `exists`, then set _request_’s `redirect mode` to
            //     it.
            if let Some(redirect) = init.redirect {
                request.redirect_mode = redirect;
            }
            // Step 23: If _init_["`integrity`"] `exists`, then set _request_’s `integrity
            //     metadata` to it.
            if let Some(integrity) = &init.integrity {
                request.integrity_metadata = integrity.clone();
            }
            // Step 24: If _init_["`keepalive`"] `exists`, then set _request_’s `keepalive` to
            //     it.
            if let Some(keepalive) = init.keepalive {
                request.keepalive = keepalive;
            }
            // Step 25: If _init_["`method`"] `exists`, then:
            // Step 25.1: Let _method_ be _init_["`method`"].
            if let Some(method) = init.method.take() {
                // Step 25.2: If _method_ is not a `method` or _method_ is a `forbidden method`,
                //     then `throw` a `TypeError`.
                if !algorithms::is_method(&method)
                    || (config::enforce_fetch_restrictions()
                        && algorithms::is_forbidden_method(&method))
                {
                    return Err(throw_type_error(scope, c"Invalid request method"));
                }
                // Step 25.3: `Normalize` _method_.
                // Step 25.4: Set _request_’s `method` to _method_.
                request.method = algorithms::normalize_a_method(method);
            }
            // Step 26: If _init_["`signal`"] `exists`, then set _signal_ to it.
            if let Some(signal_init) = init.signal {
                signal = signal_init;
            }
            // Step 27: If _init_["`priority`"] `exists`, then:
            // Step 27.1: If _request_’s `internal priority` is not null, then update _request_’s
            //     `internal priority` in an `implementation-defined` manner.
            // (There is no internal priority.)
            // Step 27.2: Otherwise, set _request_’s `priority` to _init_["`priority`"].
            if let Some(priority) = init.priority {
                request.priority = priority;
            }
        }
        // Step 28: Set `this`’s `request` to _request_.
        self.data_mut().request = request;
        // Step 29: Let _signals_ be « _signal_ » if _signal_ is non-null; otherwise « ».
        // Step 30: Set `this`’s `signal` to the result of `creating a dependent abort signal` from
        //     _signals_, using `AbortSignal` and `this`’s `relevant realm`.
        let dependent = match &signal {
            Some(signal) => create_dependent_abort_signal(scope, std::slice::from_ref(signal))?,
            None => create_dependent_abort_signal(scope, &[])?,
        };
        self.data_mut().signal.set(dependent);
        // Step 31: Set `this`’s `headers` to a `new` `Headers` object with `this`’s `relevant
        //     realm`, whose `header list` is _request_’s `header list` and `guard` is "`request`".
        let headers = Headers::from_list(scope, init_header_list, Guard::Request)?;
        // Step 32: If `this`’s `request`’s `mode` is "`no-cors`", then:
        // (A browser-security policy, gated by the request-restrictions switch.)
        if config::enforce_fetch_restrictions() && self.data().request.mode == RequestMode::NoCors {
            // Step 32.1: If `this`’s `request`’s `method` is not a `CORS-safelisted method`, then
            //     `throw` a `TypeError`.
            if !algorithms::is_cors_safelisted_method(&self.data().request.method) {
                return Err(throw_type_error(
                    scope,
                    c"a no-cors request method must be GET, HEAD, or POST",
                ));
            }
            // Step 32.2: Set `this`’s `headers`’s `guard` to "`request-no-cors`".
            headers.data_mut().guard = Guard::RequestNoCors;
        }
        // Step 33: If _init_ `is not empty`, then: The headers are sanitized as they might contain
        //     headers that are not allowed by this mode. Otherwise, they were previously sanitized
        //     or are unmodified since they were set by a privileged API.
        if let Some(init) = &mut init {
            // Step 33.1: Let _headers_ be a copy of `this`’s `headers` and its associated `header
            //     list`.
            // Step 33.2: If _init_["`headers`"] `exists`, then set _headers_ to
            //     _init_["`headers`"].
            // Step 33.3: Empty `this`’s `headers`’s `header list`.
            // Step 33.4: If _headers_ is a `Headers` object, then `for each` _header_ of its
            //     `header list`, `append` _header_ to `this`’s `headers`.
            // Step 33.5: Otherwise, `fill` `this`’s `headers` with _headers_.
            // (Filling from _init_["`headers`"] is user input and goes through the fully
            // validating `fill`. Without it, steps 33.1/33.3/33.4 amount to re-filtering the
            // headers' own former list — already validated and normalized, so it needs only the
            // guard's policy filtering, which is skipped entirely when request restrictions are
            // off; see `refill_headers_from_own_list`.)
            match init.headers.take() {
                Some(init_headers) => {
                    headers.data_mut().header_list.clear();
                    algorithms::fill_headers(scope, &headers, init_headers)?;
                }
                None => {
                    algorithms::refill_headers_from_own_list(&headers);
                }
            }
        }
        self.data_mut().headers.set(headers);
        // Step 34: Let _inputBody_ be _input_’s `request`’s `body` if _input_ is a `Request`
        //     object; otherwise null.
        // (Captured as `input_body` in step 6.)
        // Step 35: If either _init_["`body`"] `exists` and is non-null or _inputBody_ is
        //     non-null, and _request_’s `method` is `GET` or `HEAD`, then `throw` a
        //     `TypeError`.
        let init_body_given = init.as_ref().is_some_and(|i| i.body.is_some());
        let method_forbids_body = {
            let method = &self.data().request.method;
            method == "GET" || method == "HEAD"
        };
        if (init_body_given || input_body.is_some()) && method_forbids_body {
            return Err(throw_type_error(
                scope,
                c"Request with GET/HEAD method cannot have a body",
            ));
        }
        // Step 36: Let _initBody_ be null.
        let mut init_body: Option<Body> = None;
        let mut init_stream: Option<ReadableStream<'_>> = None;
        // Step 37: If _init_["`body`"] `exists` and is non-null, then:
        if let Some(body_value) = init.as_mut().and_then(|i| i.body.take()) {
            // Step 37.1: Let _bodyWithType_ be the result of `extracting` _init_["`body`"], with
            //     `_keepalive_` set to _request_’s `keepalive`.
            let keepalive = self.data().request.keepalive;
            let (body, stream, content_type) = algorithms::extract_body(
                scope,
                algorithms::BodyInitOrBytes::BodyInit(body_value),
                keepalive,
            )?;
            // Step 37.2: Set _initBody_ to _bodyWithType_’s `body`.
            init_body = Some(body);
            init_stream = stream;
            // Step 37.3: Let _type_ be _bodyWithType_’s `type`.
            // Step 37.4: If _type_ is non-null and `this`’s `headers`’s `header list` `does not
            //     contain` `Content-Type`, then `append` (`Content-Type`, _type_) to `this`’s
            //     `headers`.
            if let Some(content_type) = content_type {
                let headers = self.data().headers.get(scope);
                if !algorithms::contains(&headers.data().header_list, "Content-Type") {
                    // The simple `push` here is equivalent to `append`ing, since the checks
                    // that operation does are guaranteed to succeed if we got here.
                    headers
                        .data_mut()
                        .header_list
                        .push(("Content-Type".to_string(), content_type.into_owned()));
                }
            }
        }
        {
            // Step 38: Let _inputOrInitBody_ be _initBody_ if it is non-null; otherwise
            //     _inputBody_.
            let input_or_init = init_body.as_ref().or(input_body.as_ref());
            // Step 39: If _inputOrInitBody_ is non-null and _inputOrInitBody_’s `source` is null,
            //     then:
            if let Some(body) = input_or_init {
                if matches!(body.source, BodySource::Null) {
                    // Step 39.1: If _initBody_ is non-null and _init_["`duplex`"] does not
                    //     `exist`, then throw a `TypeError`.
                    if init_body.is_some()
                        && init.as_ref().map(|i| i.duplex.is_none()).unwrap_or(true)
                    {
                        return Err(throw_type_error(
                            scope,
                            c"a streaming request body requires init.duplex to be set",
                        ));
                    }
                    // Step 39.2: If `this`’s `request`’s `mode` is neither "`same-origin`" nor
                    //     "`cors`", then throw a `TypeError`.
                    if config::enforce_fetch_restrictions() {
                        let mode = self.data().request.mode;
                        if mode != RequestMode::SameOrigin && mode != RequestMode::Cors {
                            return Err(throw_type_error(
                                scope,
                                c"a streaming request body requires mode 'same-origin' or 'cors'",
                            ));
                        }
                        // Step 39.3: Set `this`’s `request`’s `use-CORS-preflight flag`.
                        self.data_mut().request.use_cors_preflight = true;
                    }
                }
            }
        }
        // Step 40: Let _finalBody_ be _inputOrInitBody_.
        // Step 41: If _initBody_ is null and _inputBody_ is non-null, then:
        let (final_body, final_stream) = if init_body.is_some() {
            (init_body, init_stream)
        } else if input_body.is_some() {
            // A host-backed input (an incoming server request) has no materialized stream yet;
            // bring it into existence so there is something to proxy.
            if let Some(input_req) = input_request.as_ref() {
                crate::incoming_body::materialize_host_body(scope, input_req)?;
            }
            let input_stream = input_request
                .as_ref()
                .and_then(|req| req.data().body.stream.get(scope));
            // Step 41.1: If _inputBody_ is `unusable`, then `throw` a `TypeError`.
            let unusable = input_request
                .as_ref()
                .is_some_and(|req| req.is_unusable(scope));
            if unusable {
                return Err(throw_type_error(scope, c"input Request body is unusable"));
            }
            // Step 41.2: Set _finalBody_ to the result of `creating a proxy` for _inputBody_.
            // (An unmaterialized byte source is copied. Its proxy is a fresh byte
            // body, and the input's stream, when accessed later, is marked as consumed. An already
            // materialized input stream is proxied as specified.)
            let proxied_stream = match &input_stream {
                Some(stream) => Some(crate::body_proxy::proxy_body_stream(scope, stream)?),
                None => None,
            };
            (input_body, proxied_stream)
        } else {
            (None, None)
        };
        // Step 42: Set `this`’s `request`’s `body` to _finalBody_.
        self.data_mut().body.record = final_body;
        self.sync_body_accounting();
        if let Some(stream) = final_stream {
            self.data_mut().body.stream.set(stream);
        }
        // When the input is a Request with a body, constructing a new Request from it consumes
        // (disturbs) the input's body.
        // Done last, so a construction that threw above leaves the input untouched.
        if let Some(input_request) = input_request {
            // Emptied as well as flagged: the body record copied at step 6 shares the input's
            // bytes, so the input's own handle on them holds the buffer for a body it can no
            // longer read.
            let _ = input_request.take_byte_source();
            if let Some(body) = input_request.data_mut().body.record.as_mut() {
                body.source_disturbed = true;
            }
        }
        Ok(())
    }

    /// Build a `Request` for an incoming server request from its parsed parts.
    ///
    /// This is the "creating a `Request` object" of
    /// [Create Fetch Event and Dispatch](https://w3c.github.io/ServiceWorker/#create-fetch-event-and-dispatch)
    /// step 17.4.3, which supplies the `Headers` guard ("immutable", set below) and the `signal`,
    /// which is that step's _abortController_'s signal, which the caller owns so it can signal
    /// abort at step 17.4.20.
    pub fn from_incoming(
        scope: &Scope<'_>,
        method: &str,
        url: &str,
        headers: HeaderList,
        body: Option<platform::http::IncomingBody>,
        length: Option<u64>,
        signal: AbortSignal,
    ) -> Result<Self, ExnThrown> {
        let parsed = parse_url(url, None)
            .ok_or_else(|| throw_type_error(scope, c"invalid incoming request URL"))?;
        let record = RequestRecord {
            method: method.to_string(),
            url_list: vec![parsed],
            ..Default::default()
        };
        let body_record = body.is_some().then(|| Body {
            // The bytes live on the transport, not in memory, until the handler reads them.
            source: BodySource::Null,
            length,
            source_disturbed: false,
        });
        let headers = Headers::from_list(scope, headers, Guard::Immutable)?;
        Ok(Self {
            request: record,
            body: BodyStorage {
                host: body,
                ..BodyStorage::new(body_record, None)
            },
            headers: Heap::from(headers),
            signal: Heap::from(signal),
        })
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-method>
    #[getter]
    pub fn method(&self) -> Ref<'_, str> {
        // Step 1: Return `this`’s `request`’s `method`.
        Ref::map(self.data(), |data| data.request.method.as_str())
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-url>
    #[getter]
    pub fn url(&self) -> Ref<'_, str> {
        // Step 1: Return `this`’s `request`’s `URL`, `serialized`.
        Ref::map(self.data(), |data| data.request.current_url().as_str())
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-headers>
    #[getter]
    pub fn headers<'r>(&self, scope: &'r Scope<'_>) -> Headers<'r> {
        // Step 1: Return `this`’s `headers`.
        self.data().headers.get(scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-destination>
    #[getter]
    fn destination(&self) -> RequestDestination {
        self.data().request.destination
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-referrer>
    #[getter]
    fn referrer(&self) -> String {
        match &self.data().request.referrer {
            // Step 1: If `this`’s `request`’s `referrer` is "`no-referrer`", then return the empty
            //     string.
            Referrer::NoReferrer => String::new(),
            // Step 2: If `this`’s `request`’s `referrer` is "`client`", then return
            //     "`about:client`".
            Referrer::Client => "about:client".to_string(),
            // Step 3: Return `this`’s `request`’s `referrer`, `serialized`.
            Referrer::Url(url) => url.to_string(),
        }
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-referrerpolicy>
    #[getter]
    fn referrer_policy(&self) -> ReferrerPolicy {
        // Step 1: Return `this`’s `request`’s `referrer policy`.
        self.data().request.referrer_policy
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-mode>
    #[getter]
    fn mode(&self) -> RequestMode {
        // Step 1: Return `this`’s `request`’s `mode`.
        self.data().request.mode
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-credentials>
    #[getter]
    fn credentials(&self) -> RequestCredentials {
        // Step 1: Return `this`’s `request`’s `credentials mode`.
        self.data().request.credentials_mode
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-cache>
    #[getter]
    fn cache(&self) -> RequestCache {
        // Step 1: Return `this`’s `request`’s `cache mode`.
        self.data().request.cache_mode
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-redirect>
    #[getter]
    fn redirect(&self) -> RequestRedirect {
        // Step 1: Return `this`’s `request`’s `redirect mode`.
        self.data().request.redirect_mode
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-integrity>
    #[getter]
    fn integrity(&self) -> String {
        // Step 1: Return `this`’s `request`’s `integrity metadata`.
        self.data().request.integrity_metadata.clone()
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-keepalive>
    #[getter]
    fn keepalive(&self) -> bool {
        // Step 1: Return `this`’s `request`’s `keepalive`.
        self.data().request.keepalive
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-isreloadnavigation>
    #[getter]
    fn is_reload_navigation(&self) -> bool {
        // Step 1: Return true if `this`’s `request`’s `reload-navigation flag` is set; otherwise
        //     false.
        self.data().request.reload_navigation
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-ishistorynavigation>
    #[getter]
    fn is_history_navigation(&self) -> bool {
        // Step 1: Return true if `this`’s `request`’s `history-navigation flag` is set; otherwise
        //     false.
        self.data().request.history_navigation
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-signal>
    #[getter]
    pub fn signal<'r>(&self, scope: &'r Scope<'_>) -> AbortSignal<'r> {
        // WebIDL: AbortSignal
        // Step 1: Return `this`’s `signal`.
        self.data().signal.get(scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-duplex>
    #[getter]
    fn duplex(&self) -> RequestDuplex {
        // Step 1: Return "`half`".
        RequestDuplex::Half
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-body>
    #[getter]
    pub fn body<'r>(&self, scope: &'r Scope<'_>) -> Result<Option<ReadableStream<'r>>, ExnThrown> {
        // WebIDL: ReadableStream
        BodyMixin::body(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-bodyused>
    #[getter]
    fn body_used(&self, scope: &Scope<'_>) -> bool {
        BodyMixin::body_used(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-request-clone>
    #[method(name = "clone")]
    fn js_clone<'r>(&self, scope: &'r Scope<'_>) -> Result<Request<'r>, ExnThrown> {
        // Step 1: If `this` is `unusable`, then `throw` a `TypeError`.
        self.throw_if_unusable(scope)?;
        // Step 2: Let _clonedRequest_ be the result of `cloning` `this`’s `request`.
        // [inlined clone a request] Step 1: Let _newRequest_ be a copy of _request_, except for its
        //     `body` and `WebDriver id`.
        // The record clone below; the body lives outside the record.
        // [inlined clone a request] Step 2: Set _newRequest_’s `WebDriver id` to the result of
        //     `generating a random UUID`.
        // There is no WebDriver in this runtime.
        // [inlined clone a request] Step 3: If _request_’s `body` is non-null, set _newRequest_’s
        //     `body` to the result of `cloning` _request_’s `body`.
        // `clone_body_onto`, which runs `clone a body` (`algorithms::clone_a_body_body`).
        // [inlined clone a request] Step 4: Return _newRequest_.
        let cloned_record = self.data().request.clone();
        let (cloned_body, cloned_stream) = crate::incoming_body::clone_body_onto(scope, self)?;
        // Step 3: `Assert`: `this`’s `signal` is non-null.
        let this_signal = self.data().signal.get(scope);
        // Step 4: Let _clonedSignal_ be the result of `creating a dependent abort signal` from «
        //     `this`’s `signal` », using `AbortSignal` and `this`’s `relevant realm`.
        let cloned_signal =
            create_dependent_abort_signal(scope, std::slice::from_ref(&this_signal))?;
        // Step 5: Let _clonedRequestObject_ be the result of `creating` a `Request` object, given
        //     _clonedRequest_, `this`’s `headers`’s `guard`, _clonedSignal_ and `this`’s `relevant
        //     realm`.
        // [inlined create a Request object] Step 3: Set _requestObject_’s `headers` to a `new`
        //     `Headers` object with _realm_, whose `headers list` is _request_’s `headers list`
        //     and `guard` is _guard_.
        let (header_list, guard) = {
            let headers = self.data().headers.get(scope);
            let header_list = headers.data().header_list.clone();
            let guard = headers.data().guard;
            (header_list, guard)
        };
        let cloned_headers = Headers::from_list(scope, header_list, guard)?;
        // [inlined create a Request object] Steps 1, 2, 4: Let _requestObject_ be a `new`
        //     `Request` object with _realm_; set _requestObject_’s `request` to _request_; set
        //     _requestObject_’s `signal` to _signal_.
        let cloned_object = create_instance_with::<RequestImpl>(scope, |_| RequestImpl {
            request: cloned_record,
            // A cloned request reads through its teed stream; the host body stays with the
            // source (whose stream is the other tee branch).
            body: BodyStorage::new(cloned_body, cloned_stream),
            headers: Heap::from(cloned_headers),
            signal: Heap::from(cloned_signal),
        })?;
        cloned_object.sync_body_accounting();
        // [inlined create a Request object] Step 5: Return _requestObject_.
        // Step 6: Return _clonedRequestObject_.
        Ok(cloned_object)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-arraybuffer>
    #[method]
    fn array_buffer<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::array_buffer(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-blob>
    #[method]
    fn blob<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::blob(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-bytes>
    #[method]
    fn bytes<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::bytes(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-formdata>
    #[method]
    fn form_data<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::form_data(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-json>
    #[method]
    fn json<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::json(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-text>
    #[method]
    fn text<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::text(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-textstream>
    #[method]
    fn text_stream<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        // returns WebIDL: ReadableStream
        BodyMixin::text_stream(self, scope)
    }
}

impl Request<'_> {
    /// Build the [`platform::http::Request`] for sending this request.
    ///
    /// An in-memory byte source is sent as its bytes; an unread host body (an incoming server
    /// request's forwarded body) is handed straight through; a `ReadableStream` body goes through
    /// [`crate::outgoing_body::outgoing_body_from_stream`].
    pub(crate) fn platform_request(&self, scope: &Scope<'_>) -> platform::http::Request {
        let method = self.data().request.method.clone();
        let url = self.data().request.current_url().to_string();
        let mut has_accept = false;
        let mut has_content_length = false;

        let request_headers = self.data().headers.get(scope);
        let mut headers: Vec<(String, String)> = request_headers
            .data()
            .header_list
            .iter()
            .map(|(name, value)| {
                if name.eq_ignore_ascii_case("accept") {
                    has_accept = true;
                } else if name.eq_ignore_ascii_case("content-length") {
                    has_content_length = true;
                }
                (name.clone(), value.clone())
            })
            .collect();
        // [inlined fetch](https://fetch.spec.whatwg.org/#concept-fetch) Step 12: If _request_'s
        //     `header list` `does not contain` `Accept`, then:
        // [inlined fetch] Step 12.1: Let _value_ be `*/*`.
        // [inlined fetch] Step 12.4: `Append` (`Accept`, _value_) to _request_'s `header list`.
        if !has_accept {
            headers.push(("Accept".to_string(), "*/*".to_string()));
        }
        // [inlined `HTTP-network-or-cache fetch`](https://fetch.spec.whatwg.org/#concept-http-network-or-cache-fetch):
        // if the body is null and the method is `POST` or `PUT`,
        // the Content-Length is 0; if the body is non-null with a non-null source, it is the
        // body's length. A body whose source is null (a `ReadableStream`) has no known length and
        // gets none, so it is sent chunked.
        //
        // Read before the body is taken, which empties a byte source of its bytes.
        let content_length = match self.data().body.record.as_ref() {
            None => (method == "POST" || method == "PUT").then_some(0),
            Some(body) => match &body.source {
                BodySource::Bytes(bytes) => Some(bytes.len() as u64),
                BodySource::Null => None,
            },
        };
        let body = crate::outgoing_body::consume_outgoing_body(scope, self, true);
        if let Some(content_length) = content_length {
            if !has_content_length {
                headers.push(("Content-Length".to_string(), content_length.to_string()));
            }
        }
        platform::http::Request {
            method,
            url,
            headers,
            body,
        }
    }

    /// The request's current URL.
    pub fn current_url(&self) -> Url {
        self.data().request.current_url().clone()
    }

    /// The body of an incoming request as the host delivers it, for a consumer that reads it
    /// natively, while no `body` stream was created for it. Taking it marks the body disturbed.
    pub fn take_incoming_body(&self) -> Option<platform::http::IncomingBody> {
        let body = self.take_unread_host_body()?;
        BodyMixin::set_source_disturbed(self);
        Some(body)
    }

    /// The request's method (e.g. `"GET"`, `"HEAD"`).
    pub(crate) fn http_method(&self) -> String {
        self.data().request.method.clone()
    }

    /// The request's redirect mode.
    pub(crate) fn redirect_mode(&self) -> RequestRedirect {
        self.data().request.redirect_mode
    }

    /// The request's mode (the concept-record field, distinct from the JS `mode` getter).
    pub(crate) fn request_mode(&self) -> RequestMode {
        self.data().request.mode
    }
}

impl BodyMixin for Request<'_> {
    const UNUSABLE_MESSAGE: &'static std::ffi::CStr = c"Request body is unusable";
    const TEXT_STREAM_UNSUPPORTED: &'static std::ffi::CStr =
        c"Request.textStream() is not yet supported";
}

impl HostBackedBodyOwner for Request<'_> {
    fn with_body<R>(&self, f: impl FnOnce(&mut BodyStorage) -> R) -> R {
        f(&mut self.data_mut().body)
    }

    fn object(&self) -> js::Object<'_> {
        self.as_object()
    }
}

/// <https://fetch.spec.whatwg.org/#requestinit>
#[webidl_dictionary]
pub struct RequestInit<'a> {
    pub method: Option<String>,
    pub headers: Option<HeadersInit>,
    pub body: Option<BodyInit<'a>>,
    pub referrer: Option<String>,
    pub referrer_policy: Option<ReferrerPolicy>,
    pub mode: Option<RequestMode>,
    pub credentials: Option<RequestCredentials>,
    pub cache: Option<RequestCache>,
    pub redirect: Option<RequestRedirect>,
    pub integrity: Option<String>,
    pub keepalive: Option<bool>,
    pub signal: Option<Option<AbortSignal<'a>>>,
    pub duplex: Option<RequestDuplex>,
    pub priority: Option<RequestPriority>,
    pub window: Option<HandleValue<'a>>, // WebIDL: any
}
