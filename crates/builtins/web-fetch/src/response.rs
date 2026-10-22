// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://fetch.spec.whatwg.org/>

use super::algorithms::{self, Body, BodySource};
use super::body_mixin::{BodyInit, BodyMixin, BodyStorage};
use super::headers::{Guard, HeaderList, Headers, HeadersImpl, HeadersInit};
use super::incoming_body::HostBackedBodyOwner;
use super::request::RequestRedirect;
use core_runtime::webidl_methods;
use core_runtime::{webidl_dictionary, webidl_interface};
use js::class::Ref;
use js::error::{throw_type_error, ExnThrown, RangeError};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Promise;
use url::Url;
use web_streams::readable::default_reader::{DefaultReader, DefaultReaderImpl};
use web_streams::readable::native_read::acquire_native_reader;
use web_streams::readable::ReadableStream;

js::webidl_enum! {
    /// WebIDL enum `ResponseType`
    pub enum ResponseType {
        Basic => "basic",
        Cors => "cors",
        Default => "default",
        Error => "error",
        Opaque => "opaque",
        Opaqueredirect => "opaqueredirect",
    }
}

/// <https://fetch.spec.whatwg.org/#concept-response>
///
/// The spec's *response* is an internal struct distinct from the `Response`
/// interface object, which wraps it.
#[derive(Clone)]
pub struct ResponseRecord {
    /// <https://fetch.spec.whatwg.org/#concept-response-type>
    pub response_type: ResponseType,
    /// <https://fetch.spec.whatwg.org/#concept-response-status>
    pub status: u16,
    /// <https://fetch.spec.whatwg.org/#concept-response-status-message>
    pub status_message: String,
    /// <https://fetch.spec.whatwg.org/#concept-response-url-list>
    /// May be empty (the response's URL is the last entry, or null if empty).
    pub url_list: Vec<Url>,
    /// <https://fetch.spec.whatwg.org/#concept-response-aborted>
    pub aborted: bool,
}

impl Default for ResponseRecord {
    fn default() -> Self {
        Self {
            response_type: ResponseType::Default,
            status: 200,
            status_message: String::new(),
            url_list: Vec::new(),
            aborted: false,
        }
    }
}

impl ResponseRecord {
    /// <https://fetch.spec.whatwg.org/#concept-response-url>
    /// The response's URL is the last URL in its URL list, or null if the list is empty.
    pub fn url(&self) -> Option<&Url> {
        self.url_list.last()
    }
}

/// <https://fetch.spec.whatwg.org/#ok-status>
/// An ok status is any status in the range 200 to 299, inclusive.
pub fn is_ok_status(status: u16) -> bool {
    (200..=299).contains(&status)
}

/// <https://fetch.spec.whatwg.org/#null-body-status>
/// A null body status is a status that is 101, 103, 204, 205, or 304.
pub fn is_null_body_status(status: u16) -> bool {
    matches!(status, 101 | 103 | 204 | 205 | 304)
}

/// <https://fetch.spec.whatwg.org/#redirect-status>
/// A redirect status is a status that is 301, 302, 303, 307, or 308.
pub fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// <https://fetch.spec.whatwg.org/#responseinit>
#[webidl_dictionary]
pub struct ResponseInit {
    #[webidl(default = 200)]
    pub status: u16,
    #[webidl(default = String::new())]
    pub status_text: String,
    pub headers: Option<HeadersInit>,
}

/// <https://fetch.spec.whatwg.org/#response-class>
#[webidl_interface]
pub struct Response {
    /// <https://fetch.spec.whatwg.org/#concept-response-response>
    #[no_trace]
    pub(crate) response: ResponseRecord,
    /// <https://fetch.spec.whatwg.org/#concept-response-body>, with its stream and host body.
    /// The host body is set only for an incoming response.
    pub(crate) body: BodyStorage,
    /// <https://fetch.spec.whatwg.org/#response-headers>
    pub(crate) headers: Heap<HeadersImpl>,
    /// The `fetch` abort algorithm's state, for a response delivered by `fetch` whose body was
    /// still abortable when the fetch settled. Set so consuming the body can detach the
    /// algorithm from the `AbortSignal`, which would otherwise keep it, and this response,
    /// alive for the life of the signal.
    pub(crate) abort_state: Option<Heap<crate::abort::AbortFetchStateImpl>>,
    /// The reader [`Response::reserve_body_for_sending`] acquired on `body_stream`, which the
    /// send path reads the body through.
    pub(crate) send_reader: Option<Heap<DefaultReaderImpl>>,
}

#[webidl_methods]
impl Response {
    /// Stop reporting the body's bytes to the GC. A compacting GC may have moved the object
    /// since `sync_body_accounting` ran, so the finalizer's address is used here.
    #[destructor]
    fn release_body_accounting(&mut self, object: *mut js::native::JSObject) {
        // SAFETY: `object` is the object being finalized, whose private data this is.
        unsafe { self.body.accounted.release(object) };
    }

    /// <https://fetch.spec.whatwg.org/#dom-response>
    #[constructor]
    fn new(
        &self,
        scope: &Scope<'_>,
        body: Option<Option<BodyInit<'_>>>,
        init: Option<ResponseInit>,
    ) -> Result<(), ExnThrown> {
        // Step 1: Set `this`’s `response` to a new `response`.
        self.data_mut().response = ResponseRecord::default();
        // Step 2: Set `this`’s `headers` to a `new` `Headers` object with `this`’s `relevant
        //     realm`, whose `header list` is `this`’s `response`’s `header list` and `guard` is
        //     "`response`".
        let headers = js::class::create_instance_with::<HeadersImpl>(scope, |_| HeadersImpl {
            header_list: HeaderList::new().into(),
            guard: Guard::Response,
        })?;
        self.data_mut().headers.set(headers);
        // Step 3: Let _bodyWithType_ be null.
        // Step 4: If _body_ is non-null, then set _bodyWithType_ to the result of `extracting`
        //     _body_.
        let body_with_type = match body {
            Some(Some(body_value)) => Some(algorithms::extract_body(
                scope,
                algorithms::BodyInitOrBytes::BodyInit(body_value),
                false,
            )?),
            _ => None,
        };
        // Step 5: Perform `initialize a response` given `this`, _init_, and _bodyWithType_.
        algorithms::initialize_a_response(scope, self, init, body_with_type)
    }

    pub fn from_record_headers_body(
        record: ResponseRecord,
        headers: Headers,
        body: Option<Body>,
        body_stream: Option<ReadableStream>,
    ) -> Self {
        Self {
            response: record,
            body: BodyStorage::new(body, body_stream),
            headers: Heap::from(headers),
            abort_state: None,
            send_reader: None,
        }
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-type>
    #[getter(name = "type")]
    pub fn get_type(&self) -> ResponseType {
        // Step 1: Return `this`’s `response`’s `type`.
        self.data().response.response_type
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-url>
    #[getter(name = "url")]
    pub fn url_string(&self) -> Ref<'_, str> {
        // Step 1: Return the empty string if `this`’s `response`’s `URL` is null; otherwise
        //     `this`’s `response`’s `URL`, `serialized` with `_exclude fragment_` set to true.
        Ref::map(self.data(), |data| match data.response.url() {
            None => "",
            Some(url) => &url[..url::Position::AfterQuery],
        })
    }

    /// The response's URL.
    pub fn url(&self) -> Option<Url> {
        self.data().response.url().cloned()
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-redirected>
    #[getter]
    pub fn redirected(&self) -> bool {
        // Step 1: Return true if `this`’s `response`’s `URL list`’s `size` is greater than 1;
        //     otherwise false.
        self.data().response.url_list.len() > 1
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-status>
    #[getter]
    pub fn status(&self) -> u16 {
        // Step 1: Return `this`’s `response`’s `status`.
        self.data().response.status
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-ok>
    #[getter]
    pub fn ok(&self) -> bool {
        // Step 1: Return true if `this`’s `response`’s `status` is an `ok status`; otherwise
        //     false.
        is_ok_status(self.data().response.status)
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-statustext>
    #[getter]
    pub fn status_text(&self) -> Ref<'_, str> {
        // Step 1: Return `this`’s `response`’s `status message`.
        Ref::map(self.data(), |data| data.response.status_message.as_str())
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-headers>
    #[getter]
    pub fn headers<'r>(&self, scope: &'r Scope<'_>) -> Headers<'r> {
        // Step 1: Return `this`’s `headers`.
        self.data().headers.get(scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-body>
    #[getter]
    pub fn body<'r>(&self, scope: &'r Scope<'_>) -> Result<Option<ReadableStream<'r>>, ExnThrown> {
        // WebIDL: ReadableStream
        BodyMixin::body(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-bodyused>
    #[getter]
    pub fn body_used(&self, scope: &Scope<'_>) -> bool {
        BodyMixin::body_used(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-clone>
    #[method(name = "clone")]
    fn js_clone<'r>(&self, scope: &'r Scope<'_>) -> Result<Response<'r>, ExnThrown> {
        // Step 1: If `this` is `unusable`, then `throw` a `TypeError`.
        self.throw_if_unusable(scope)?;
        // Step 2: Let _clonedResponse_ be the result of `cloning` `this`’s `response`.
        // (inlined)
        // [inlined clone a response] Step 1: If _response_ is a `filtered response`, then return a
        //     new identical `filtered response` whose `internal response` is a `clone` of
        //     _response_’s `internal response`.
        // The record clone below covers this.
        // [inlined clone a response] Step 2: Let _newResponse_ be a copy of _response_, except for
        //     its `body`.
        let new_response = self.data().response.clone();
        // [inlined clone a response] Step 3: If _response_’s `body` is non-null, then set
        //     _newResponse_’s `body` to the result of `cloning` _response_’s `body`.
        // `clone_body_onto`, which runs `clone a body` (`algorithms::clone_a_body_body`).
        // [inlined clone a response] Step 4: Return _newResponse_.
        let (cloned_body, cloned_stream) = crate::incoming_body::clone_body_onto(scope, self)?;
        // Step 3: Return the result of `creating` a `Response` object, given _clonedResponse_,
        //     `this`’s `headers`’s `guard`, and `this`’s `relevant realm`.
        let headers = self.headers(scope);
        let guard = headers.data().guard;
        algorithms::create_a_response_object(
            scope,
            new_response,
            headers.list(),
            guard,
            cloned_body,
            cloned_stream,
        )
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-arraybuffer>
    #[method]
    pub fn array_buffer<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::array_buffer(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-blob>
    #[method]
    pub fn blob<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::blob(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-bytes>
    #[method]
    pub fn bytes<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::bytes(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-formdata>
    #[method]
    pub fn form_data<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::form_data(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-json>
    #[method]
    pub fn json<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::json(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-text>
    #[method]
    pub fn text<'r>(&self, scope: &'r Scope<'_>) -> Result<Promise<'r>, ExnThrown> {
        BodyMixin::text(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-body-textstream>
    #[method]
    pub fn text_stream<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        // returns WebIDL: ReadableStream
        BodyMixin::text_stream(self, scope)
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-error>
    #[static_method]
    pub fn error<'r>(scope: &'r Scope<'_>) -> Result<Response<'r>, ExnThrown> {
        // Step 1: Return the result of `creating` a `Response` object, given a new `network
        //     error`, "`immutable`", and the `current realm`.
        let record = ResponseRecord {
            response_type: ResponseType::Error,
            status: 0,
            ..ResponseRecord::default()
        };
        algorithms::create_a_response_object(
            scope,
            record,
            HeaderList::new(),
            Guard::Immutable,
            None,
            None,
        )
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-redirect>
    #[static_method]
    pub fn redirect<'r>(
        scope: &'r Scope<'_>,
        url: String,
        status: Option<u16>,
    ) -> Result<Response<'r>, ExnThrown> {
        // Step 1: Let _parsedURL_ be the result of `parsing` _url_ with `current settings object`’s
        //     `API base URL`.
        let base = web_globals::worker_location::current_location_url();
        let parsed = Url::options()
            .base_url(base.as_ref())
            .parse(&url)
            // Step 2: If _parsedURL_ is failure, then `throw` a `TypeError`.
            .map_err(|_| throw_type_error(scope, c"Failed to parse redirect URL"))?;
        // Step 3: If _status_ is not a `redirect status`, then `throw` a `RangeError`.
        let status = status.unwrap_or(302);
        if !is_redirect_status(status) {
            return Err(RangeError(format!("{status} is not a redirect status")).throw(scope));
        }
        // Step 4: Let _responseObject_ be the result of `creating` a `Response` object, given a
        //     new `response`, "`immutable`", and the `current realm`.
        let response = algorithms::create_a_response_object(
            scope,
            ResponseRecord::default(),
            HeaderList::new(),
            Guard::Immutable,
            None,
            None,
        )?;
        // Step 5: Set _responseObject_’s `response`’s `status` to _status_.
        response.data_mut().response.status = status;
        // Step 6: Let _value_ be _parsedURL_, `serialized` and `isomorphic encoded`.
        let value = parsed.to_string();
        // Step 7: `Append` (`Location`, _value_) to _responseObject_’s `response`’s `header
        //     list`.
        let headers = response.data().headers.get(scope);
        algorithms::append_a_header(
            &mut headers.data_mut().header_list,
            "Location".to_string(),
            value,
        );
        // Step 8: Return _responseObject_.
        Ok(response)
    }

    /// <https://fetch.spec.whatwg.org/#dom-response-json>
    #[static_method(name = "json")]
    fn from_value_as_json<'r>(
        scope: &'r Scope<'_>,
        data: HandleValue<'_>,
        init: Option<ResponseInit>,
    ) -> Result<Response<'r>, ExnThrown> {
        // Step 1: Let _bytes_ the result of running `serialize a JavaScript value to JSON bytes` on
        //     _data_.
        let Some(json_string) = js::json::stringify(scope, data, None, None)? else {
            return Err(throw_type_error(
                scope,
                c"The data is not JSON-serializable",
            ));
        };
        // Step 2: Let _body_ be the result of `extracting` _bytes_.
        let (body, _, _) = algorithms::extract_body(
            scope,
            algorithms::BodyInitOrBytes::Bytes(bytes::Bytes::from(json_string.into_bytes())),
            false,
        )?;
        // Step 3: Let _responseObject_ be the result of `creating` a `Response` object, given a
        //     new `response`, "`response`", and the `current realm`.
        let response = algorithms::create_a_response_object(
            scope,
            ResponseRecord::default(),
            HeaderList::new(),
            Guard::Response,
            None,
            None,
        )?;
        // Step 4: Perform `initialize a response` given _responseObject_, _init_, and (_body_,
        //     "`application/json`").
        algorithms::initialize_a_response(
            scope,
            &response,
            init,
            Some((
                body,
                None,
                Some(std::borrow::Cow::Borrowed("application/json")),
            )),
        )?;
        // Step 5: Return _responseObject_.
        Ok(response)
    }
}

impl Response<'_> {
    /// Set this response's body to null, per
    /// [`main fetch`](https://fetch.spec.whatwg.org/#concept-main-fetch)'s
    /// HEAD/CONNECT/null-body-status step (Step 22).
    /// Drops the body record, its stream, and any host body so consume/`.body` see no body.
    pub(crate) fn clear_body(&self) {
        {
            let mut data = self.data_mut();
            data.body.record = None;
            data.body.stream = None;
            data.body.host = None;
            data.body.source = None;
        }
        self.sync_body_accounting();
    }

    /// Whether an abort can still act on this response's body: it has an unread host body, or a
    /// `.body` stream that is still readable.
    pub(crate) fn body_is_abortable(&self, scope: &Scope<'_>) -> bool {
        let data = self.data();
        data.body.host.is_some()
            || data
                .body
                .stream
                .get(scope)
                .is_some_and(|stream| stream.is_readable())
    }

    /// Abort this response's body: error its `.body` stream with `reason` (so pending reads
    /// reject) and stop the host read, cancelling an in-flight chunk read and dropping any unread
    /// host body, which closes the connection. Called by the `fetch` abort algorithm.
    pub(crate) fn abort_body(&self, scope: &Scope<'_>, reason: HandleValue<'_>) {
        // Drop an unread host body (the `.body`/consume path never ran): closes the connection.
        self.data_mut().body.host = None;
        // Stop an in-flight host read driving the `.body` stream.
        let source = self.data().body.source.get(scope);
        if let Some(source) = source {
            source.abort(scope);
        }
        // Error the `.body` stream so pending and future reads reject.
        let stream = self.data().body.stream.get(scope);
        if let Some(stream) = stream {
            stream.error(scope, reason);
        }
    }
}

/// Build a `Response` object for a `data:` URL, the "`data`" case of `scheme fetch`.
///
/// <https://fetch.spec.whatwg.org/#concept-scheme-fetch> step 3, "`data`":
/// runs the `data:` URL processor on `url`; on failure (a network error) returns
/// `None`, and the caller rejects the `fetch()` promise with a `TypeError`. On
/// success synthesizes a `basic`, 200 "`OK`" response whose only header is the
/// serialized MIME type and whose body is the decoded bytes.
pub(crate) fn response_from_data_url<'r>(
    scope: &'r Scope<'_>,
    url: &Url,
) -> Result<Option<Response<'r>>, ExnThrown> {
    // Step 3 "`data`".1: Let _dataURLStruct_ be the result of running the `data:` URL processor`
    //     on _request_’s `current URL`.
    // Step 3 "`data`".2: If _dataURLStruct_ is failure, then return a `network error`.
    let Some(data_url_struct) = algorithms::data_url_processor(url) else {
        return Ok(None);
    };
    // Step 3 "`data`".3: Let _mimeType_ be _dataURLStruct_’s `MIME type`, `serialized`.
    // `data_url::mime::Mime`'s `Display` is "serialize a MIME type".
    let mime_type = data_url_struct.mime_type.to_string();
    // Step 3 "`data`".4: Return a new `response` whose `status message` is `OK`, `header list` is
    //     « (`Content-Type`, _mimeType_) », and `body` is _dataURLStruct_’s `body` `as a body`.
    // The response is given the `basic` type, as `fetch` does for every response in this runtime
    // (the test suite expects it).
    let record = ResponseRecord {
        response_type: ResponseType::Basic,
        status: 200,
        status_message: "OK".to_string(),
        url_list: vec![url.clone()],
        aborted: false,
    };
    // [inlined] `as a body` is `get a byte sequence bytes as a body`: the `body` of the result of
    //     `safely extracting` the bytes — for a byte sequence, `extract a body` with the decoded
    //     bytes.
    let (body, _, _) = algorithms::extract_body(
        scope,
        algorithms::BodyInitOrBytes::Bytes(bytes::Bytes::from(data_url_struct.body)),
        false,
    )?;
    let response = algorithms::create_a_response_object(
        scope,
        record,
        vec![("Content-Type".to_string(), mime_type)],
        Guard::Immutable,
        Some(body),
        None,
    )?;
    Ok(Some(response))
}

/// Build a `Response` object from a transport response and the request's URL.
///
/// Used by `fetch`: the response is given the `basic` type and an `immutable`
/// headers guard and its header list populated from the transport headers.
pub(crate) fn response_from_platform<'r>(
    scope: &'r Scope<'_>,
    response: platform::http::Response,
    url_list: Vec<Url>,
    method: &str,
    redirect_mode: RequestRedirect,
    tainting: crate::transport::ResponseTainting,
) -> Result<Response<'r>, ExnThrown> {
    // [inlined `HTTP fetch`](https://fetch.spec.whatwg.org/#concept-http-fetch) Step 6.3
    //     "`manual`".2: Otherwise, set _response_ to an `opaque-redirect filtered response` whose
    //     `internal response` is _internalResponse_.
    // An [opaque-redirect filtered
    // response](https://fetch.spec.whatwg.org/#concept-filtered-response-opaque-redirect) has type
    // "opaqueredirect", status 0, empty status message and header list, null body, and the
    // request's URL list (so `redirected` is false and `url` is the original). The platform
    // response body is dropped here, closing the connection.
    // (Hiding the redirect target is a browser-security policy, gated by the request-restrictions
    // switch: without it, `redirect: "manual"` hands the caller the real redirect response —
    // status, `Location` header, body and all.)
    if core_runtime::config::web_security_restrictions()
        && redirect_mode == RequestRedirect::Manual
        && is_redirect_status(response.status)
    {
        let record = ResponseRecord {
            response_type: ResponseType::Opaqueredirect,
            status: 0,
            status_message: String::new(),
            url_list,
            aborted: false,
        };
        return algorithms::create_a_response_object(
            scope,
            record,
            HeaderList::new(),
            Guard::Immutable,
            None,
            None,
        );
    }
    // [inlined `main fetch`](https://fetch.spec.whatwg.org/#concept-main-fetch) Step 14.2: Set
    //     _response_ to the `filtered response` matching _request_'s `response tainting`:
    // "`opaque`" → an [opaque filtered
    // response](https://fetch.spec.whatwg.org/#concept-filtered-response-opaque): type
    // "`opaque`", `URL list` « », status 0, status message empty, header list « », body null.
    // The platform response body is dropped here, closing the connection.
    if tainting == crate::transport::ResponseTainting::Opaque {
        let record = ResponseRecord {
            response_type: ResponseType::Opaque,
            status: 0,
            status_message: String::new(),
            url_list: Vec::new(),
            aborted: false,
        };
        return algorithms::create_a_response_object(
            scope,
            record,
            HeaderList::new(),
            Guard::Immutable,
            None,
            None,
        );
    }

    // "`basic`" strips only forbidden response headers, which we deliberately expose, and "`cors`"
    // would narrow the headers to the CORS-safelisted set. Since we don't have a CORS model, there
    // is no `Access-Control-Expose-Headers` to honor, so both are returned unfiltered as "`basic`".
    let record = ResponseRecord {
        response_type: ResponseType::Basic,
        status: response.status,
        // Not available in the platform response, so we leave it as always empty, as browsers
        // do for HTTP/2,3.
        // See note at https://fetch.spec.whatwg.org/#concept-response-status-message
        status_message: String::new(),
        url_list,
        aborted: false,
    };
    let header_list: HeaderList = response.headers.into_iter().collect();
    // [inlined `main fetch`](https://fetch.spec.whatwg.org/#concept-main-fetch) Step 22: If
    //     _response_ is not a `network error` and either _request_’s `method` is `HEAD` or
    //     `CONNECT`, or _internalResponse_’s `status` is a `null body status`, set
    //     _internalResponse_’s `body` to null and disregard any enqueuing toward it (if any).
    let has_body = !(method.eq_ignore_ascii_case("HEAD")
        || method.eq_ignore_ascii_case("CONNECT")
        || is_null_body_status(response.status));
    let body = has_body.then(|| Body {
        source: BodySource::Null,
        length: None,
        source_disturbed: false,
    });
    let response_object = crate::algorithms::create_a_response_object(
        scope,
        record,
        header_list,
        Guard::Immutable,
        body,
        None,
    )?;
    if has_body {
        response_object.data_mut().body.host = Some(response.body);
    }
    Ok(response_object)
}

impl BodyMixin for Response<'_> {
    const UNUSABLE_MESSAGE: &'static std::ffi::CStr = c"Response body is unusable";
    const TEXT_STREAM_UNSUPPORTED: &'static std::ffi::CStr =
        c"Response.textStream() is not yet supported";

    /// The body is being read to completion, so an abort can no longer error it:
    /// drop the `fetch` abort algorithm that was keeping this response alive.
    fn on_body_consumed(&self, scope: &Scope<'_>) {
        let state = self.data_mut().abort_state.take_rooted(scope);
        if let Some(state) = state {
            state.detach(scope);
        }
    }
}

impl HostBackedBodyOwner for Response<'_> {
    fn with_body<R>(&self, f: impl FnOnce(&mut BodyStorage) -> R) -> R {
        f(&mut self.data_mut().body)
    }

    fn object(&self) -> js::Object<'_> {
        self.as_object()
    }

    fn take_send_reader<'r>(&self, scope: &'r Scope<'_>) -> Option<DefaultReader<'r>> {
        self.data_mut().send_reader.take_rooted(scope)
    }
}

impl Response<'_> {
    /// Returns a copy of the response's header list.
    pub fn headers_list(&self, scope: &Scope<'_>) -> Vec<(String, String)> {
        self.headers(scope).list()
    }

    /// The response's body as a [`platform::http::OutgoingBody`] for sending on the wire, on the
    /// serve path. An in-memory byte source is sent as-is; an unread host network body (a `fetch`
    /// response used directly as the reply) is handed straight through; a materialized body stream
    /// goes through [`crate::outgoing_body::outgoing_body_from_stream`], as an outgoing request's
    /// does, or is canceled without sending if `start_reading` is false (see
    /// [`crate::outgoing_body::consume_outgoing_body`]).
    pub fn take_send_body(
        &self,
        scope: &Scope<'_>,
        start_reading: bool,
    ) -> platform::http::OutgoingBody {
        crate::outgoing_body::consume_outgoing_body(scope, self, start_reading)
    }

    /// <https://fetch.spec.whatwg.org/#body-unusable>
    ///
    /// Whether the body is non-null and its stream already `disturbed` or `locked`.
    pub fn is_body_unusable(&self, scope: &Scope<'_>) -> bool {
        BodyMixin::is_unusable(self, scope)
    }

    /// Commit the body to being sent: mark it as read, so `bodyUsed` reports true, and lock a
    /// materialized `.body` stream with the reader [`Response::take_send_body`] later reads it
    /// through, so author code can no longer read from it.
    pub fn reserve_body_for_sending(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        BodyMixin::set_source_disturbed(self);
        let stream = self.data().body.stream.get(scope);
        if let Some(stream) = stream {
            if !stream.is_locked() {
                let reader = acquire_native_reader(scope, &stream)?;
                self.data_mut().send_reader.set(reader);
            }
        }
        Ok(())
    }
}
