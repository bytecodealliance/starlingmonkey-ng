// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The WASIp3 backend of [`http`](crate::http): the `wasi:http@0.3` client for sending,
//! [`IncomingBody`] wrapping a `wasi:http` body stream, and the request and response conversions
//! of the `wasi:http/handler` export.

use wasip3::http::types::{
    ErrorCode, Fields, Method, Request as WasiRequest, Response as WasiResponse, Scheme,
};
use wasip3::{wit_future, wit_stream};

use crate::http::{BodyBytes, Error, OutgoingBody, Request, Response};
use crate::http_wasm::{AbandonBody, BodyDone, BodySendOutcome, HostBodyWriter, UrlParts};

/// The `wasi:http/types` interface of the `wasi:http` version the runtime links: the one its
/// `wasip3` bindings target.
pub const HOST_HTTP_TYPES: &str = "wasi:http/types@0.3.0";

/// The interface of the HTTP handler export, without a version.
pub const HOST_HTTP_HANDLER: &str = "wasi:http/handler";

/// The resource of [`HOST_HTTP_TYPES`] that [`HostRequest`] is.
pub const HOST_REQUEST_RESOURCE: &str = "request";

/// The resource of [`HOST_HTTP_TYPES`] that [`HostResponse`] is.
pub const HOST_RESPONSE_RESOURCE: &str = "response";

/// The request the `wasi:http/handler` export receives.
pub type HostRequest = WasiRequest;
/// The response the `wasi:http/handler` export returns.
pub type HostResponse = WasiResponse;
/// The error the `wasi:http/handler` export returns.
pub type HostErrorCode = ErrorCode;

type BodyStreamReader = wasip3::wit_bindgen::rt::async_support::StreamReader<u8>;
type BodyStreamWriter = wasip3::wit_bindgen::rt::async_support::StreamWriter<u8>;
type TrailersResult = Result<Option<Fields>, wasip3::http::types::ErrorCode>;
type TrailersWriter = wasip3::wit_bindgen::rt::async_support::FutureWriter<TrailersResult>;
type TrailersReader = wasip3::wit_bindgen::rt::async_support::FutureReader<TrailersResult>;

/// The pieces of an [`OutgoingBody`] handed to the host: the contents stream,
/// the trailers future, and, when we own the bytes, the writer half for
/// [`spawn_body_writer`] to feed.
struct BodyParts {
    contents: Option<BodyStreamReader>,
    /// The trailers future given to the host. For a piped host body this is
    /// the pipe source's trailers reader, so an abort on the incoming body
    /// propagates to the peer instead of looking like a clean end. Otherwise
    /// it is a fresh future that [`spawn_body_writer`] resolves.
    trailers: TrailersReader,
    writer: Option<(BodyStreamWriter, OutgoingBody)>,
}

/// Split `body` into the pieces the host needs. A host body is handed
/// straight through, trailers and all. A body-less request/response
/// gets no stream at all.
///
/// The host reads a host body's stream itself, so `body_timeout` doesn't apply to it. Bounding it
/// would require copying every chunk of a proxied body through the guest.
fn body_contents(body: OutgoingBody) -> (BodyParts, TrailersWriter) {
    let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
    let (contents, piped_trailers, writer) = match body {
        OutgoingBody::Host(response_body) => {
            (Some(response_body.stream), response_body.trailers, None)
        }
        OutgoingBody::Consumed => (None, None, None),
        OutgoingBody::Bytes(bytes) if bytes.is_empty() => (None, None, None),
        other => {
            let (body_tx, body_rx) = wit_stream::new();
            (Some(body_rx), None, Some((body_tx, other)))
        }
    };
    (
        BodyParts {
            contents,
            trailers: piped_trailers.unwrap_or(trailers_rx),
            writer,
        },
        trailers_tx,
    )
}

/// Feed the owned body (in-memory, or streamed through the channel) to its
/// wit stream from a spawned task, then write the trailers: `Ok(None)` on a
/// clean end, an `ErrorCode` otherwise. The trailers future is the only abort
/// signal a wit stream's consumer can observe.
///
/// `timeout` bounds the write, so a handler that never ends its stream doesn't hold the wit
/// stream open for the life of the instance. Running out aborts the body with error trailers.
///
/// The returned [`BodyDone`] resolves when the send is over, with its
/// [`BodySendOutcome`].
fn spawn_body_writer(
    writer_body: Option<(BodyStreamWriter, OutgoingBody)>,
    trailers_tx: TrailersWriter,
    timeout: Option<std::time::Duration>,
    declared_length: Option<u64>,
) -> (BodyDone, AbandonBody) {
    let (done_tx, done_rx) = futures_channel::oneshot::channel::<BodySendOutcome>();
    let (abandon_tx, abandon_rx) = futures_channel::oneshot::channel::<()>();
    crate::task::spawn(async move {
        let mut outcome = BodySendOutcome::Sent;
        if let Some((body_tx, body)) = writer_body {
            let mut writer = StreamBodyWriter(body_tx);
            outcome = crate::http_wasm::bounded(
                timeout,
                crate::http_wasm::write_owned_body(&mut writer, body, declared_length, abandon_rx),
            )
            .await;
            // Ends the stream either way, and before the trailers below go out: a consumer must
            // not see the abort reason while the body still looks open.
            drop(writer);
        }
        let trailers = match abort_message(&outcome) {
            None => Ok(None),
            Some(message) => Err(ErrorCode::InternalError(Some(message))),
        };
        // The outcome is sent before the trailers, so a host slow to take the trailers doesn't
        // delay the serve path.
        let _ = done_tx.send(outcome);
        let _ = trailers_tx.write(trailers).await;
    });
    (BodyDone(done_rx), AbandonBody(abandon_tx))
}

/// A body's wit stream, which the host reads host bodies from directly.
struct StreamBodyWriter(BodyStreamWriter);

impl HostBodyWriter for StreamBodyWriter {
    async fn send_chunk(&mut self, chunk: Vec<u8>) -> bool {
        // `write_all` returns the values left unwritten because the host dropped the read end.
        self.0.write_all(chunk).await.is_empty()
    }

    async fn send_host_body(
        &mut self,
        _body: IncomingBody,
        _remaining: &mut crate::http::Remaining,
    ) -> BodySendOutcome {
        unreachable!("`body_contents` hands host bodies to the host")
    }
}

crate::http_wasm::wasi_http_conversions!(
    method: Method,
    scheme: Scheme,
    fields: Fields,
    request: WasiRequest,
    response: WasiResponse,
);

/// The message the peer receives in the error trailers of a body whose send ended with
/// `outcome`, or `None` for a body sent whole.
fn abort_message(outcome: &BodySendOutcome) -> Option<String> {
    match outcome {
        BodySendOutcome::Sent => None,
        BodySendOutcome::Truncated => {
            Some("the response body ended before the `Content-Length` it declared".to_string())
        }
        BodySendOutcome::TimedOut => Some(
            "the response body was not fully sent within the response-body or end-to-end \
             timeout"
                .to_string(),
        ),
        BodySendOutcome::Failed(message) => Some(message.clone()),
    }
}

/// Send `request` with the `wasi:http` client, returning once the response status and headers
/// are available.
pub async fn send(request: Request) -> Result<Response, Error> {
    let url = UrlParts::parse(&request.url)?;
    let fields = request_fields(&request)?;
    let (body, trailers_tx) = body_contents(request.body);
    let (wasi_request, transmitted) = WasiRequest::new(fields, body.contents, body.trailers, None);
    set_request_target(&wasi_request, &request.method, &url)?;

    // Write the body we own (in-memory or channel-streamed) and the trailers once the
    // request exists, so the host is ready to read from the stream. A host-piped body has no
    // writer here: the host reads its stream directly.
    // No timeout: the serve timeouts bound responses, not an outgoing request's body.
    drop(spawn_body_writer(body.writer, trailers_tx, None, None));

    // wasmtime aborts the request's connection when this future is dropped, cutting off the
    // response body at whatever the host has already buffered. Awaiting it keeps the connection
    // open until the exchange is over, including for a response body handed on to the host.
    crate::task::spawn(async move {
        let _ = transmitted.await;
    });

    let response = wasip3::http::client::send(wasi_request)
        .await
        .map_err(|e| Error(format!("fetch failed: {e:?}")))?;

    let status = response.get_status_code();
    let headers = response
        .get_headers()
        .copy_all()
        .into_iter()
        .map(|(name, value)| (name, crate::http::isomorphic_decode(&value)))
        .collect();

    let (result_tx, result_rx) = wit_future::new(|| Ok(()));
    let (body_stream, trailers) = WasiResponse::consume_body(response, result_rx);
    // The result future isn't used, so we resolve it at once.
    crate::task::spawn(async move {
        let _ = result_tx.write(Ok(())).await;
    });

    Ok(Response {
        status,
        headers,
        body: IncomingBody {
            stream: body_stream,
            trailers: Some(trailers),
        },
    })
}

/// Read an incoming `wasi:http` request into its parts (method, full URL, headers, body).
///
/// Returns `Err` for a field that the host accepted but `http` cannot represent.
pub fn read_incoming_request(
    request: WasiRequest,
) -> Result<(String, String, http::HeaderMap, IncomingBody), ErrorCode> {
    let method = string_of_method(request.get_method());
    let url = incoming_request_url(
        request.get_scheme(),
        request.get_authority(),
        request.get_path_with_query(),
    );
    let headers = http::HeaderMap::try_from(request.get_headers())?;

    // Take the body's stream without reading it.
    let (result_tx, result_rx) = wit_future::new(|| Ok(()));
    let (body_stream, trailers) = WasiRequest::consume_body(request, result_rx);
    crate::task::spawn(async move {
        let _ = result_tx.write(Ok(())).await;
    });
    Ok((
        method,
        url,
        headers,
        IncomingBody {
            stream: body_stream,
            trailers: Some(trailers),
        },
    ))
}

/// Build a `wasi:http` response from parts.
///
/// The body is written from a spawned task: an in-memory body inline,
/// a host body handed straight through, a stream body forwarded chunk by chunk.
///
/// The returned [`BodyDone`] resolves once every chunk was handed to the host,
/// which is at once for a host body. `body_timeout` bounds the send (see
/// [`spawn_body_writer`]).
///
/// `declared_length` is used as the `Content-Length` header, and enforced for guest-produced
/// streams.
pub fn build_outgoing_response(
    status: u16,
    headers: http::HeaderMap,
    body: OutgoingBody,
    body_timeout: Option<std::time::Duration>,
    declared_length: Option<u64>,
) -> (WasiResponse, BodyDone, AbandonBody) {
    let (body, trailers_tx) = body_contents(body);
    let (response, _result) =
        WasiResponse::new(response_fields(&headers), body.contents, body.trailers);
    set_status(&response, status);

    let (body_done, abandon) =
        spawn_body_writer(body.writer, trailers_tx, body_timeout, declared_length);
    (response, body_done, abandon)
}

/// Bytes read lazily from a host stream: the body of an HTTP client response, an
/// incoming request body on the serve path, or any other `stream<u8>` (see
/// [`IncomingBody::from_stream`]).
pub struct IncomingBody {
    stream: BodyStreamReader,
    /// The body's trailers future, taken when the body ends.
    ///
    /// `wasi:http` communicates errors that abort the stream through the trailers
    /// future, so we store it here to consult once the body stream closed.
    trailers: Option<TrailersReader>,
}

impl IncomingBody {
    /// The bytes of `stream`, which has no trailers that could report an abort.
    pub fn from_stream(stream: BodyStreamReader) -> Self {
        IncomingBody {
            stream,
            trailers: None,
        }
    }

    /// The stream the bytes are read from. A stream can't hold an error, so if the body has
    /// trailers, a task awaits them and prints an abort they report to stderr.
    pub fn into_stream(self) -> BodyStreamReader {
        if let Some(trailers) = self.trailers {
            crate::task::spawn_background(async move {
                if let Err(code) = trailers.await {
                    eprintln!(
                        "a stream's source ended the stream early: the body was aborted: {code:?}"
                    );
                }
            });
        }
        self.stream
    }

    /// Read the entire body to a byte sequence.
    pub async fn read_all(self) -> Result<BodyBytes, Error> {
        // `collect` consumes the stream, so destructure first and consult the trailers directly
        // rather than through `end_of_body` (which needs `self`).
        let IncomingBody { stream, trailers } = self;
        let bytes = bytes::Bytes::from(stream.collect().await);
        if let Some(trailers) = trailers {
            trailers
                .await
                .map_err(|code| Error(format!("the body was aborted: {code:?}")))?;
        }
        Ok(bytes)
    }

    /// Read the next bytes into `buf`, at most as many as it has capacity for. Returns the bytes
    /// read, or `Ok(None)` at the end of the body. The bytes are never empty.
    pub async fn read_into(&mut self, mut buf: Vec<u8>) -> Result<Option<Vec<u8>>, Error> {
        use wasip3::wit_bindgen::rt::async_support::StreamResult;
        loop {
            let (status, read) = self.stream.read(buf).await;
            buf = read;
            match status {
                // The stream is still open after a zero-length read, so it is read again.
                // Awaiting the trailers here would hang, since they only resolve once the stream
                // is closed.
                StreamResult::Complete(0) => {}
                StreamResult::Complete(_) => return Ok(Some(buf)),
                // The body has ended. The trailers report whether it ended cleanly.
                StreamResult::Dropped if !buf.is_empty() => return Ok(Some(buf)),
                StreamResult::Dropped => return self.end_of_body().await.map(|()| None),
                // The read was cancelled at teardown. The stream was never reported closed, so
                // the trailers may never resolve and aren't awaited.
                StreamResult::Cancelled => return Ok(None),
            }
        }
    }

    /// Await the trailers future of a body whose stream has ended, and return an error if the
    /// peer aborted the body.
    ///
    /// Only valid once the stream has been reported closed. The future does not resolve before
    /// then.
    async fn end_of_body(&mut self) -> Result<(), Error> {
        let Some(trailers) = self.trailers.take() else {
            return Ok(());
        };
        match trailers.await {
            Ok(_) => Ok(()),
            Err(code) => Err(Error(format!("the body was aborted: {code:?}"))),
        }
    }
}
