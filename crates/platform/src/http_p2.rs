// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The WASIp2 backend of [`http`](crate::http): the `wasi:http@0.2` client for sending,
//! [`IncomingBody`] wrapping an incoming body's input stream, and the request and response
//! conversions of the `wasi:http/incoming-handler` export.
//!
//! `wasi:http@0.2` gives every body to the guest as a `wasi:io` stream, so the guest copies a
//! host body it passes on, chunk by chunk.

use wasip2::http::types::{
    ErrorCode, Fields, IncomingBody as WasiIncomingBody, IncomingRequest, IncomingResponse, Method,
    OutgoingBody as WasiOutgoingBody, OutgoingRequest, OutgoingResponse, Scheme,
};
use wasip2::io::streams::{InputStream, OutputStream, StreamError};

use crate::http::{BodyBytes, Error, OutgoingBody, Remaining, Request, Response};
use crate::http_wasm::{
    AbandonBody, BodyDone, BodySendOutcome, HostBodyWriter, UrlParts, HOST_STOPPED_READING,
};

/// The `wasi:http/types` interface of the `wasi:http` version the runtime links: the one its
/// `wasip2` bindings target.
pub const HOST_HTTP_TYPES: &str = "wasi:http/types@0.2.12";

/// The interface of the HTTP handler export, without a version.
pub const HOST_HTTP_HANDLER: &str = "wasi:http/incoming-handler";

/// The resource of [`HOST_HTTP_TYPES`] that [`HostRequest`] is.
pub const HOST_REQUEST_RESOURCE: &str = "incoming-request";

/// The resource of [`HOST_HTTP_TYPES`] that [`HostResponse`] is.
pub const HOST_RESPONSE_RESOURCE: &str = "outgoing-response";

/// The request the `wasi:http/incoming-handler` export receives.
pub type HostRequest = IncomingRequest;
/// The response the `wasi:http/incoming-handler` export sets its response outparam to.
pub type HostResponse = OutgoingResponse;
/// The error the `wasi:http/incoming-handler` export sets its response outparam to.
pub type HostErrorCode = ErrorCode;

/// The size of the reads from an incoming body's stream.
const READ_SIZE: u64 = 64 * 1024;

crate::http_wasm::wasi_http_conversions!(
    method: Method,
    scheme: Scheme,
    fields: Fields,
    request: OutgoingRequest,
    response: OutgoingResponse,
);

/// Send `request` with the `wasi:http` client, returning once the response status and headers
/// are available.
pub async fn send(request: Request) -> Result<Response, Error> {
    let url = UrlParts::parse(&request.url)?;
    let wasi_request = OutgoingRequest::new(request_fields(&request)?);
    set_request_target(&wasi_request, &request.method, &url)?;
    let wasi_body = wasi_request
        .body()
        .expect("a new request's body is taken only here");

    let future = wasip2::http::outgoing_handler::handle(wasi_request, None)
        .map_err(|e| Error(format!("fetch failed: {e:?}")))?;
    // No timeout: the serve timeouts bound responses, not an outgoing request's body.
    drop(spawn_body_writer(wasi_body, request.body, None, None));

    crate::task::wait_for(future.subscribe()).await;
    let response = match future.get() {
        Some(Ok(Ok(response))) => response,
        Some(Ok(Err(e))) => return Err(Error(format!("fetch failed: {e:?}"))),
        Some(Err(())) | None => {
            return Err(Error(
                "fetch failed: the host did not provide the response".to_string(),
            ))
        }
    };
    let status = response.status();
    let headers = response
        .headers()
        .entries()
        .into_iter()
        .map(|(name, value)| (name, crate::http::isomorphic_decode(&value)))
        .collect();
    let body = IncomingBody::from_parent(BodyParent::Response(response))
        .map_err(|message| Error(format!("fetch failed: {message}")))?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// Read an incoming `wasi:http` request into its parts (method, full URL, headers, body).
///
/// Returns `Err` for a field the host accepted that `http` cannot represent.
pub fn read_incoming_request(
    request: IncomingRequest,
) -> Result<(String, String, http::HeaderMap, IncomingBody), ErrorCode> {
    let method = string_of_method(request.method());
    let url = incoming_request_url(
        request.scheme(),
        request.authority(),
        request.path_with_query(),
    );
    let mut headers = http::HeaderMap::new();
    for (name, value) in request.headers().entries() {
        let unrepresentable =
            || ErrorCode::InternalError(Some(format!("the request header `{name}` is invalid")));
        let header_name =
            http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| unrepresentable())?;
        let header_value = http::HeaderValue::from_bytes(&value).map_err(|_| unrepresentable())?;
        headers.append(header_name, header_value);
    }
    let body = IncomingBody::from_parent(BodyParent::Request(request))
        .map_err(|message| ErrorCode::InternalError(Some(message)))?;
    Ok((method, url, headers, body))
}

/// Build a `wasi:http` response from parts.
///
/// The body is written from a task started with [`spawn`](crate::task::spawn), so the export call
/// finishes sending it before it returns. The response must be set as the export's response
/// before that task first runs, since the host doesn't read the body before then.
///
/// The returned [`BodyDone`] resolves once every chunk was handed to the host and the body was
/// finished. `body_timeout` bounds the whole send, including copying a host body and waiting for
/// the host to take the last chunk.
///
/// `declared_length` is used as the `Content-Length` header, and enforced for streamed and host
/// bodies.
pub fn build_outgoing_response(
    status: u16,
    headers: http::HeaderMap,
    body: OutgoingBody,
    body_timeout: Option<std::time::Duration>,
    declared_length: Option<u64>,
) -> (OutgoingResponse, BodyDone, AbandonBody) {
    let response = OutgoingResponse::new(response_fields(&headers));
    set_status(&response, status);
    let wasi_body = response
        .body()
        .expect("a new response's body is taken only here");
    let (body_done, abandon) = spawn_body_writer(wasi_body, body, body_timeout, declared_length);
    (response, body_done, abandon)
}

/// Write `contents` to `body` from a task started with [`spawn`](crate::task::spawn), then
/// finish `body`. A body whose send doesn't end in [`BodySendOutcome::Sent`] is dropped without
/// being finished, which makes the host abort it.
///
/// The returned [`BodyDone`] resolves when the send is over, with its [`BodySendOutcome`].
fn spawn_body_writer(
    body: WasiOutgoingBody,
    contents: OutgoingBody,
    timeout: Option<std::time::Duration>,
    declared_length: Option<u64>,
) -> (BodyDone, AbandonBody) {
    let (done_tx, done_rx) = futures_channel::oneshot::channel::<BodySendOutcome>();
    let (abandon_tx, abandon_rx) = futures_channel::oneshot::channel::<()>();
    crate::task::spawn(async move {
        let outcome = match BodyWriter::new(body) {
            None => BodySendOutcome::Failed("the host refused the body's stream".to_string()),
            Some(mut writer) => {
                let send = async move {
                    let outcome = crate::http_wasm::write_owned_body(
                        &mut writer,
                        contents,
                        declared_length,
                        abandon_rx,
                    )
                    .await;
                    match outcome {
                        BodySendOutcome::Sent => match writer.finish().await {
                            Ok(()) => BodySendOutcome::Sent,
                            Err(message) => BodySendOutcome::Failed(message),
                        },
                        outcome => outcome,
                    }
                };
                crate::http_wasm::bounded(timeout, send).await
            }
        };
        let _ = done_tx.send(outcome);
    });
    (BodyDone(done_rx), AbandonBody(abandon_tx))
}

/// An outgoing body and its output stream. Dropping it without [`BodyWriter::finish`] makes the
/// host abort the body.
struct BodyWriter {
    // Fields are dropped in declaration order, so the stream is dropped before the body it
    // belongs to, which the host requires.
    stream: OutputStream,
    body: WasiOutgoingBody,
    /// The trailers [`BodyWriter::finish`] sends.
    trailers: Option<Fields>,
}

impl BodyWriter {
    fn new(body: WasiOutgoingBody) -> Option<Self> {
        let stream = body.write().ok()?;
        Some(BodyWriter {
            stream,
            body,
            trailers: None,
        })
    }

    /// Wait until everything written was flushed, then finish the body with its trailers.
    async fn finish(self) -> Result<(), String> {
        let flushed = self.flush().await;
        let BodyWriter {
            stream,
            body,
            trailers,
        } = self;
        drop(stream);
        if !flushed {
            drop(body);
            return Err(HOST_STOPPED_READING.to_string());
        }
        WasiOutgoingBody::finish(body, trailers)
            .map_err(|e| format!("the host refused to finish the body: {e:?}"))
    }

    /// Wait until everything written was flushed. Returns `false` if the write failed.
    ///
    /// The host closes the stream once a body has the length its `Content-Length` declares, so a
    /// closed stream counts as flushed. [`WasiOutgoingBody::finish`] fails for a body that is
    /// shorter.
    async fn flush(&self) -> bool {
        match self.stream.flush() {
            Ok(()) => {}
            Err(StreamError::Closed) => return true,
            Err(StreamError::LastOperationFailed(_)) => return false,
        }
        // The flush is complete once the stream accepts writes again.
        loop {
            match self.stream.check_write() {
                Ok(0) => crate::task::wait_for(self.stream.subscribe()).await,
                Ok(_) | Err(StreamError::Closed) => return true,
                Err(StreamError::LastOperationFailed(_)) => return false,
            }
        }
    }
}

impl HostBodyWriter for BodyWriter {
    async fn send_chunk(&mut self, chunk: Vec<u8>) -> bool {
        let mut written = 0;
        while written < chunk.len() {
            match self.stream.check_write() {
                Ok(0) => crate::task::wait_for(self.stream.subscribe()).await,
                Ok(permitted) => {
                    let end = chunk
                        .len()
                        .min(written + usize::try_from(permitted).unwrap_or(usize::MAX));
                    if self.stream.write(&chunk[written..end]).is_err() {
                        return false;
                    }
                    written = end;
                }
                Err(_) => return false,
            }
        }
        true
    }

    async fn send_host_body(
        &mut self,
        mut body: IncomingBody,
        remaining: &mut Remaining,
    ) -> BodySendOutcome {
        loop {
            match body.next_part(READ_SIZE).await {
                Ok(BodyPart::Chunk(chunk)) => {
                    // Content past the declared length is dropped, and the body ends.
                    let Some(chunk) = remaining.take(chunk) else {
                        return BodySendOutcome::Sent;
                    };
                    if !self.send_chunk(chunk).await {
                        return BodySendOutcome::Failed(HOST_STOPPED_READING.to_string());
                    }
                }
                Ok(BodyPart::End(trailers)) => {
                    self.trailers = trailers;
                    return crate::http_wasm::outcome_at_end(remaining);
                }
                Err(e) => return BodySendOutcome::Failed(e.0),
            }
        }
    }
}

/// The resource an incoming body was consumed from, which the host requires to outlive it.
enum BodyParent {
    Request(IncomingRequest),
    Response(IncomingResponse),
}

/// What [`IncomingBody::next_part`] read.
enum BodyPart {
    Chunk(Vec<u8>),
    /// The body ended cleanly, with these trailers.
    End(Option<Fields>),
}

/// A body read lazily from the host's incoming `wasi:http` body stream: an HTTP client response,
/// or an incoming request body on the serve path.
pub struct IncomingBody {
    // Fields are dropped in declaration order, so each resource is dropped before the one it was
    // obtained from, which the host requires. `stream` and `body` are `None` once the body ended.
    stream: Option<InputStream>,
    body: Option<WasiIncomingBody>,
    _parent: BodyParent,
}

impl IncomingBody {
    /// Consume `parent`'s body. `Err` explains why the host refused it.
    fn from_parent(parent: BodyParent) -> Result<Self, String> {
        let body = match &parent {
            BodyParent::Request(request) => request.consume(),
            BodyParent::Response(response) => response.consume(),
        }
        .map_err(|()| "the host refused the body".to_string())?;
        let stream = body
            .stream()
            .map_err(|()| "the host refused the body's stream".to_string())?;
        Ok(IncomingBody {
            stream: Some(stream),
            body: Some(body),
            _parent: parent,
        })
    }

    /// Read the entire body to a byte sequence.
    pub async fn read_all(mut self) -> Result<BodyBytes, Error> {
        let mut bytes = Vec::new();
        while let BodyPart::Chunk(chunk) = self.next_part(READ_SIZE).await? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(BodyBytes::from(bytes))
    }

    /// Read the next bytes, at most as many as `buf` has capacity for. Returns the bytes read, or
    /// `Ok(None)` at the end of the body. The bytes are never empty, and are in an allocation
    /// made by the host rather than in `buf`.
    pub async fn read_into(&mut self, buf: Vec<u8>) -> Result<Option<Vec<u8>>, Error> {
        let max = u64::try_from(buf.capacity()).unwrap_or(u64::MAX);
        match self.next_part(max).await? {
            BodyPart::Chunk(chunk) => Ok(Some(chunk)),
            BodyPart::End(_) => Ok(None),
        }
    }

    /// Read the next chunk of at most `max` bytes, or the trailers at end of body. Returns an
    /// error if the peer aborted the body. Once the body has ended, every call returns
    /// `BodyPart::End(None)`.
    async fn next_part(&mut self, max: u64) -> Result<BodyPart, Error> {
        loop {
            let Some(stream) = &self.stream else {
                return Ok(BodyPart::End(None));
            };
            match stream.read(max) {
                Ok(chunk) if !chunk.is_empty() => return Ok(BodyPart::Chunk(chunk)),
                Ok(_) => crate::task::wait_for(stream.subscribe()).await,
                Err(StreamError::Closed) => return self.end_of_body().await.map(BodyPart::End),
                Err(StreamError::LastOperationFailed(e)) => {
                    let message = match wasip2::http::types::http_error_code(&e) {
                        Some(code) => format!("the body was aborted: {code:?}"),
                        None => format!("the body was aborted: {}", e.to_debug_string()),
                    };
                    self.stream = None;
                    self.body = None;
                    return Err(Error(message));
                }
            }
        }
    }

    /// Finish a body whose stream reported its end, and return its trailers, or an error if the
    /// peer aborted the body.
    async fn end_of_body(&mut self) -> Result<Option<Fields>, Error> {
        self.stream = None;
        let Some(body) = self.body.take() else {
            return Ok(None);
        };
        let trailers = WasiIncomingBody::finish(body);
        crate::task::wait_for(trailers.subscribe()).await;
        match trailers.get() {
            Some(Ok(Ok(trailers))) => Ok(trailers),
            Some(Ok(Err(code))) => Err(Error(format!("the body was aborted: {code:?}"))),
            Some(Err(())) | None => Err(Error(
                "the host did not provide the body's trailers".to_string(),
            )),
        }
    }
}
