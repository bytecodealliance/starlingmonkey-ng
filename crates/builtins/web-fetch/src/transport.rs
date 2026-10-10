// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Redirect following for `fetch`, layered over the platform transport so native
//! and wasm behave identically (the platform clients do not follow redirects).
//!
//! This inlines `HTTP fetch` Step 6.3 — the redirect-mode switch
//! (<https://fetch.spec.whatwg.org/#concept-http-fetch>) — and, in `follow` mode,
//! the applicable steps of HTTP-redirect fetch
//! (<https://fetch.spec.whatwg.org/#http-redirect-fetch>): it resolves `Location`,
//! applies the method/body changes for 301/302/303, and re-sends, up to the
//! redirect limit; `error` mode fails on a redirect; `manual` mode returns the
//! redirect response as-is. It also inlines `main fetch`'s origin/mode switch
//! (see [`apply_main_fetch_switch`]), run per hop as the spec's recursion
//! through `main fetch` would. CORS itself and referrer taint are out of scope.

use platform::http::{Error, OutgoingBody, Request, Response};
use url::Url;

use crate::request::{RequestMode, RequestRedirect};
use crate::response::is_redirect_status;

/// <https://fetch.spec.whatwg.org/#concept-http-redirect-fetch> — the redirect
/// count limit.
const MAX_REDIRECTS: usize = 20;

/// `Response tainting`
/// (<https://fetch.spec.whatwg.org/#concept-request-response-tainting>), as far
/// as we model it: it records whether the request (or any redirect
/// hop) left the requester's origin, and in which mode. "opaque" selects the
/// `opaque filtered response` when the response object is built; "cors" is
/// tracked so a redirect back to the original origin does not revert to
/// "basic", but, with no CORS model, carries no filtering of its own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseTainting {
    Basic,
    Cors,
    Opaque,
}

/// [inlined `main fetch`](https://fetch.spec.whatwg.org/#concept-main-fetch)
/// Step 12: the origin/mode switch, deciding whether a hop may proceed and how
/// it taints the response. The spec runs it once per `main fetch`, and redirects
/// recurse through `main fetch`, so `send_following_redirects` re-runs it for
/// the initial URL and for every redirect target.
///
/// The request's origin is its client's. For us: the worker location's. Without
/// one there is nothing to compare against, and in permissive server mode the
/// browser security policies are off wholesale, so in both cases every hop
/// proceeds as "basic".
fn apply_main_fetch_switch(
    url: &Url,
    request_origin: Option<&url::Origin>,
    mode: RequestMode,
    redirect_mode: RequestRedirect,
    tainting: &mut ResponseTainting,
) -> Result<(), Error> {
    if !core_runtime::config::web_security_restrictions() {
        return Ok(());
    }
    let Some(request_origin) = request_origin else {
        return Ok(());
    };
    // [inlined] Step 12, first arm: _request_'s `current URL`'s `origin` is `same origin` with
    //     _request_'s `origin`, and _request_'s `response tainting` is "`basic`" / _request_'s
    //     `current URL`'s `scheme` is "`data`" / _request_'s `mode` is "`navigate`" or
    //     "`websocket`" → set _request_'s `response tainting` to "`basic`" (and run `scheme
    //     fetch`).
    // `data:` URLs never reach the transport, "`navigate`" is rewritten to "`same-origin`" by
    // the `Request` constructor, and there are no WebSockets, so the arm reduces to the origin
    // check.
    if *tainting == ResponseTainting::Basic && url.origin() == *request_origin {
        return Ok(());
    }
    match mode {
        // [inlined] Step 12, second arm: _request_'s `mode` is "`same-origin`" → return a
        //     `network error`.
        RequestMode::SameOrigin | RequestMode::Navigate => Err(Error(format!(
            "cannot fetch {url} in \"same-origin\" mode: not same origin with the requester"
        ))),
        // [inlined] Step 12, third arm: _request_'s `mode` is "`no-cors`":
        RequestMode::NoCors => {
            // [inlined] Step 12, third arm, 1: If _request_'s `redirect mode` is not
            //     "`follow`", then return a `network error`.
            if redirect_mode != RequestRedirect::Follow {
                return Err(Error(format!(
                    "cannot fetch {url} in \"no-cors\" mode with a \"{}\" redirect mode",
                    redirect_mode.as_str()
                )));
            }
            // [inlined] Step 12, third arm, 2: Set _request_'s `response tainting` to
            //     "`opaque`".
            *tainting = ResponseTainting::Opaque;
            // [inlined] Step 12, third arm, 3: Return the result of running `scheme fetch`.
            Ok(())
        }
        // [inlined] Step 12, fourth arm: _request_'s `current URL`'s `scheme` is not an
        //     `HTTP(S) scheme` → return a `network error`.
        // Guaranteed here: only HTTP(S) URLs reach the transport, and Step 6 of the redirect
        // loop rejects non-HTTP(S) `Location`s.
        // [inlined] Step 12, fifth arm: set _request_'s `response tainting` to "`cors`" (and
        //     run `HTTP fetch` with the CORS machinery). There is no CORS machinery in this
        //     runtime — no `Origin` header is sent and no `Access-Control-*` response checks
        //     are made — so the request proceeds as a plain one; the tainting still records
        //     that it crossed origins.
        RequestMode::Cors => {
            *tainting = ResponseTainting::Cors;
            Ok(())
        }
    }
}

/// Send `request`, following redirects per `redirect_mode`. Returns the final
/// response, the URL list (its last entry is the response URL; more than one
/// entry means the response is redirected), and the response tainting the hops
/// accumulated.
pub(crate) async fn send_following_redirects(
    mut request: Request,
    redirect_mode: RequestRedirect,
    mode: RequestMode,
    initial_url: Url,
    request_origin: Option<url::Origin>,
) -> Result<(Response, Vec<Url>, ResponseTainting), Error> {
    let mut tainting = ResponseTainting::Basic;
    apply_main_fetch_switch(
        &initial_url,
        request_origin.as_ref(),
        mode,
        redirect_mode,
        &mut tainting,
    )?;
    let mut url_list = vec![initial_url];
    loop {
        // Build this attempt's request, taking the body: a byte body is kept for replay on a
        // body-preserving redirect (its clone is a refcount bump, not a copy); a stream body is
        // moved (and cannot be replayed).
        let body_is_stream = matches!(
            request.body,
            OutgoingBody::Stream(_) | OutgoingBody::Host(_)
        );
        let send_body = match &mut request.body {
            OutgoingBody::Bytes(bytes) => OutgoingBody::Bytes(bytes.clone()),
            // A streamed or host-piped body is consumed by the send and cannot be replayed.
            OutgoingBody::Stream(_) | OutgoingBody::Host(_) => {
                std::mem::replace(&mut request.body, OutgoingBody::Consumed)
            }
            OutgoingBody::Consumed => OutgoingBody::Consumed,
        };
        let attempt = Request {
            method: request.method.clone(),
            url: request.url.clone(),
            headers: request.headers.clone(),
            body: send_body,
        };
        // [`HTTP-network-or-cache fetch`](https://fetch.spec.whatwg.org/#concept-http-network-or-cache-fetch)
        // / [`HTTP-network fetch`](https://fetch.spec.whatwg.org/#concept-http-network-fetch),
        // collapsed into the platform transport (no HTTP cache and no connection pool of our own).
        let response = platform::http::send(attempt).await?;

        // [inlined `HTTP fetch`](https://fetch.spec.whatwg.org/#concept-http-fetch) Step 6: If
        //     _internalResponse_'s `status` is a `redirect status`:
        // Steps 6.1–6.2 — navigation timing and the HTTP/2 `RST_STREAM` hint — do not apply: no
        // navigations, and the connection belongs to the platform transport.
        // [inlined `HTTP fetch`] Step 6.3: Switch on _request_'s `redirect mode`:
        // (inlined)
        // A non-redirect status is the final response.
        if !is_redirect_status(response.status) {
            return Ok((response, url_list, tainting));
        }
        match redirect_mode {
            // [inlined] Step 6.3 "`manual`".2: Otherwise, set _response_ to an `opaque-redirect
            //     filtered response` whose `internal response` is _internalResponse_.
            // ("`manual`".1 applies only to "`navigate`" mode, which does not exist here.)
            // `response_from_platform` builds that filtered response, and drops this body.
            RequestRedirect::Manual => return Ok((response, url_list, tainting)),
            // [inlined] Step 6.3 "`error`".1: Set _response_ to a `network error`.
            RequestRedirect::Error => {
                return Err(Error(
                    "redirect encountered with redirect mode \"error\"".to_string(),
                ));
            }
            // [inlined] Step 6.3 "`follow`".2: Set _response_ to the result of running
            //     [`HTTP-redirect fetch`](https://fetch.spec.whatwg.org/#concept-http-redirect-fetch)
            //     given _fetchParams_ and _response_.
            // Inlined as the rest of this loop body; the `[inlined] Step N` comments below are its
            // steps. ("`follow`".1 is the WebDriver BiDi response completed steps; no WebDriver.
            // Steps 1–2 of `HTTP-redirect fetch` unwrap a `filtered response`; nothing is wrapped
            // at this layer, so _internalResponse_ is the response itself.)
            RequestRedirect::Follow => {}
        }

        // [inlined] Step 3: Let _locationURL_ be _internalResponse_'s `location URL` given
        //     _request_'s `current URL`'s `fragment`.
        // (inlined)
        // [inlined location URL](https://fetch.spec.whatwg.org/#concept-response-location-url)
        // Step 1: If _response_'s `status` is not a `redirect status`, then return null.
        // Established above: a non-redirect status already returned as the final response.
        // [inlined location URL] Step 2: Let _location_ be the result of `extracting header list
        //     values` given `Location` and _response_'s `header list`.
        // The `ABNF` for `Location` allows a single `header`, so a second one (identical or not)
        // is an error.
        let mut locations = response
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("location"))
            .map(|(_, value)| value.clone());
        let location = locations.next();
        let multiple_locations = locations.next().is_some();
        drop(locations);
        // [inlined] Step 4: If _locationURL_ is null, then return _response_.
        let Some(location) = location else {
            return Ok((response, url_list, tainting));
        };
        // [inlined] Step 5: If _locationURL_ is failure, then return a `network error`.
        // Failure is multiple `Location` headers ([inlined location URL] Step 2 above) or a
        // `Location` that does not parse (Step 3 below).
        if multiple_locations {
            return Err(Error(
                "multiple Location headers in redirect response".to_string(),
            ));
        }
        // [inlined location URL] Step 3: If _location_ is a `header value`, then set _location_ to
        //     the result of `parsing` _location_ with _response_'s `URL`.
        let current_url = url_list.last().expect("url_list is never empty");
        let mut location_url = current_url
            .join(&percent_encode_location_bytes(&location))
            .map_err(|e| Error(format!("invalid redirect Location: {e}")))?;
        // [inlined location URL] Step 4: If _location_ is a `URL` whose `fragment` is null, then
        //     set _location_'s `fragment` to _requestFragment_ (the current URL's fragment).
        // Observably inert in this runtime — `response.url` serializes with `exclude fragment` —
        // but it keeps the URL list faithful across hops.
        if location_url.fragment().is_none() {
            location_url.set_fragment(current_url.fragment());
        }
        // [inlined location URL] Step 5: Return _location_.

        // [inlined] Step 6: If _locationURL_'s `scheme` is not an `HTTP(S) scheme`, then return a
        //     `network error`.
        if !matches!(location_url.scheme(), "http" | "https") {
            return Err(Error(format!(
                "redirect to non-HTTP(S) scheme \"{}:\"",
                location_url.scheme()
            )));
        }

        // [inlined] Step 7: If _request_'s `redirect count` is 20, then return a `network error`.
        // [inlined] Step 8: Increase _request_'s `redirect count` by 1.
        // (The count is carried as the URL list's length past the initial entry.)
        if url_list.len() > MAX_REDIRECTS {
            return Err(Error("too many redirects".to_string()));
        }

        // [inlined] Step 9: If _request_'s `mode` is "`cors`", _locationURL_ `includes
        //     credentials`, and _request_'s `origin` is not `same origin` with _locationURL_'s
        //     `origin`, then return a `network error`.
        // [inlined] Step 10: If _request_'s `response tainting` is "`cors`" and _locationURL_
        //     `includes credentials`, then return a `network error`. This catches a cross-origin
        //     resource redirecting to a same-origin URL.
        //
        // There is no origin or response-tainting model in this runtime — the requester has no
        // origin — so the same-origin conditions cannot be evaluated. A credentialed Location on
        // a cors-mode request is rejected outright: the strictest reading of steps 9–10, and what
        // browsers observe for `fetch()` (always cors-mode here) of cross-origin resources.
        let includes_credentials =
            !location_url.username().is_empty() || location_url.password().is_some();
        if mode == RequestMode::Cors && includes_credentials {
            return Err(Error(
                "redirect Location must not include credentials".to_string(),
            ));
        }

        // [inlined] Step 11: If _internalResponse_'s `status` is not 303, _request_'s `body` is
        //     non-null, and _request_'s `body`'s `source` is null, then return a `network error`.
        let status = response.status;
        if status != 303 && body_is_stream {
            return Err(Error(
                "cannot follow redirect: the request body is a stream and cannot be replayed"
                    .to_string(),
            ));
        }

        // [inlined] Step 12: If one of the following is true
        //   - _internalResponse_'s `status` is 301 or 302 and _request_'s `method` is `POST`
        //   - _internalResponse_'s `status` is 303 and _request_'s `method` is not `GET` or
        //     `HEAD`
        //     then:
        // [inlined] Step 12.1: Set _request_'s `method` to `GET` and _request_'s `body` to null.
        // [inlined] Step 12.2: `For each` _headerName_ of `request-body-header name`, `delete`
        //     _headerName_ from _request_'s `header list`.
        let is_post = request.method.eq_ignore_ascii_case("POST");
        let is_get_or_head = request.method.eq_ignore_ascii_case("GET")
            || request.method.eq_ignore_ascii_case("HEAD");
        if ((status == 301 || status == 302) && is_post) || (status == 303 && !is_get_or_head) {
            request.method = "GET".to_string();
            request.body = OutgoingBody::Bytes(bytes::Bytes::new());
            request
                .headers
                .retain(|(name, _)| !is_request_body_header(name));
        }

        // [inlined] Step 13: If _request_'s `current URL`'s `origin` is not `same origin` with
        //     _locationURL_'s `origin`, then `for each` _headerName_ of `CORS non-wildcard
        //     request-header name`, `delete` _headerName_ from _request_'s `header list`. I.e., the
        //     moment another origin is seen after the initial request, the `Authorization` header
        //     is removed.
        if current_url.origin() != location_url.origin() {
            // The spec's `CORS non-wildcard request-header name` list is just
            // `Authorization`, because in a browser `Cookie`/`Proxy-Authorization`
            // are forbidden request headers a script cannot set. This runtime's
            // server mode lets a handler set them (forbidden-header enforcement
            // off), so also strip them on a cross-origin redirect — defense in
            // depth against leaking credentials to another origin. This only ever
            // strips *more* than the spec, never fewer.
            request.headers.retain(|(name, _)| {
                !name.eq_ignore_ascii_case("authorization")
                    && !name.eq_ignore_ascii_case("cookie")
                    && !name.eq_ignore_ascii_case("proxy-authorization")
            });
        }

        // [inlined] Step 14: If _request_'s `body` is non-null, then set _request_'s `body` to the
        //     `body` of the result of `safely extracting` _request_'s `body`'s `source`.
        // (Byte bodies are re-cloned at the top of the loop; a stream body cannot reach here — step
        // 11 errored, or step 12 dropped it.)
        // [inlined] Steps 15–17: `timing info`'s `redirect end time` / `post-redirect start time` /
        //     `redirect start time`.
        // No fetch timing info is kept.
        // [inlined] Step 18: `Append` _locationURL_ to _request_'s `URL list`.
        request.url = location_url.to_string();
        url_list.push(location_url);
        // [inlined] Step 19: Invoke `set _request_'s referrer policy on redirect`. No referrer.
        // [inlined] Steps 20–21: _recursive_ is true unless the `redirect mode` is "`manual`",
        //     which returned above, so it is always true here.
        // [inlined] Step 22: Return the result of running `main fetch` given _fetchParams_ and
        //     _recursive_.
        // The recursion through `main fetch` is what re-runs its Step 12 origin/mode switch
        // against the new URL, enforcing "same-origin" mode across hops and tainting a
        // response that has crossed origins, so this loop does the same before re-sending.
        apply_main_fetch_switch(
            url_list.last().expect("url_list is never empty"),
            request_origin.as_ref(),
            mode,
            redirect_mode,
            &mut tainting,
        )?;
        // Drop the redirect response (and its body), closing that connection, then follow.
        drop(response);
    }
}

/// Percent-encode the non-ASCII bytes of a `Location` header value, so the URL
/// parser resolves it as the byte sequence it is.
///
/// A header value is bytes, and the platform layer hands it over isomorphic-
/// decoded, one code unit per byte. Passing that straight to the URL parser
/// would have it treat those code units as *text* and percent-encode each one's
/// UTF-8 encoding — so a `Location` of the three bytes `E2 98 83` would resolve
/// to `%C3%A2%C2%98%C2%83` rather than `%E2%98%83`, a different URL. Encoding
/// them here as the bytes they are avoids that.
///
/// ASCII is left untouched, so an already-escaped `%e2` keeps its spelling and
/// a literal `%` stays literal — the URL parser does not re-encode either.
///
/// <https://github.com/whatwg/fetch/issues/883> — the behaviour
/// `redirect-location-escape.tentative.any.js` covers, not yet in the spec.
fn percent_encode_location_bytes(location: &str) -> std::borrow::Cow<'_, str> {
    use std::fmt::Write;

    if location.is_ascii() {
        return std::borrow::Cow::Borrowed(location);
    }
    let mut encoded = String::with_capacity(location.len());
    for ch in location.chars() {
        match u8::try_from(ch as u32) {
            // The value came from an isomorphic decode, so every code unit is one byte.
            Ok(byte) if !ch.is_ascii() => {
                write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
            }
            _ => encoded.push(ch),
        }
    }
    std::borrow::Cow::Owned(encoded)
}

/// A `request-body header name`: removed when a redirect drops the request body.
/// <https://fetch.spec.whatwg.org/#request-body-header-name>
///
/// The spec's list is `Content-Encoding`, `Content-Language`, `Content-Location`
/// and `Content-Type`. `Content-Length` is added to it: the body is being
/// dropped here, so a length describing it would misframe the new request. In a
/// browser this cannot arise, because `Content-Length` is a forbidden request
/// header a script cannot set; this runtime lets a server-side embedder set it.
fn is_request_body_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-encoding")
        || name.eq_ignore_ascii_case("content-language")
        || name.eq_ignore_ascii_case("content-location")
        || name.eq_ignore_ascii_case("content-type")
        || name.eq_ignore_ascii_case("content-length")
}
