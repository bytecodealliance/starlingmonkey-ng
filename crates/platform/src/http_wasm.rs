// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The wasm backend of [`http`](crate::http): the WASIp3 `wasi:http` client for
//! sending, and [`IncomingBody`] wrapping a `wasi:http` body stream.

use futures_lite::StreamExt;
use wasip3::http::types::{
    ErrorCode, Fields, Method, Request as WasiRequest, Response as WasiResponse, Scheme,
};
use wasip3::{wit_bindgen, wit_future, wit_stream};

use crate::http::{BodyBytes, Error, OutgoingBody, Request, Response};

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
    /// propagates to the peer instead of looking like a clean end; otherwise
    /// it is a fresh future [`spawn_body_writer`] resolves.
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

/// How a body's send ended, reported through [`BodyDone`].
pub enum BodySendOutcome {
    /// Every chunk was handed to the host.
    Sent,
    /// The body ended before the `Content-Length` the response declared for it.
    Truncated,
    /// The `timeout` given to [`spawn_body_writer`] expired before the send finished.
    TimedOut,
    /// The send failed: the host dropped the stream's read end (the connection
    /// is gone), or a streamed chunk was itself an error.
    Failed(String),
}

/// Resolves once a body's writer task has ended, with how it ended, or with `None` if the task
/// was cancelled first, as at instance teardown.
pub struct BodyDone(futures_channel::oneshot::Receiver<BodySendOutcome>);

impl std::future::Future for BodyDone {
    type Output = Option<BodySendOutcome>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<BodySendOutcome>> {
        std::pin::Pin::new(&mut self.0).poll(cx).map(|r| r.ok())
    }
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
    wit_bindgen::spawn_local(async move {
        let mut outcome = BodySendOutcome::Sent;
        if let Some((mut body_tx, body)) = writer_body {
            let mut remaining = crate::http::Remaining::new(declared_length);
            let write = async {
                match body {
                    OutgoingBody::Bytes(bytes) => {
                        // Zero-copy when the bytes are uniquely owned and Vec-backed.
                        if !body_tx.write_all(Vec::from(bytes)).await.is_empty() {
                            return BodySendOutcome::Failed(HOST_STOPPED_READING.to_string());
                        }
                        BodySendOutcome::Sent
                    }
                    OutgoingBody::Stream(mut receiver) => {
                        write_stream_body(&mut body_tx, &mut receiver, abandon_rx, &mut remaining)
                            .await
                    }
                    OutgoingBody::Host(_) | OutgoingBody::Consumed => BodySendOutcome::Sent,
                }
            };
            outcome = match timeout {
                None => write.await,
                Some(limit) => {
                    let expired = async {
                        crate::clock::sleep(limit).await;
                        BodySendOutcome::TimedOut
                    };
                    futures_lite::future::or(write, expired).await
                }
            };
            // Ends the stream either way, and before the trailers below go out: a consumer must
            // not see the abort reason while the body still looks open. `write` only borrows the
            // writer, so a lost race hands it back here.
            drop(body_tx);
        }
        let trailers = match &outcome {
            BodySendOutcome::Sent => Ok(None),
            BodySendOutcome::Truncated => Err(ErrorCode::InternalError(Some(
                "the response body ended before the `Content-Length` it declared".to_string(),
            ))),
            BodySendOutcome::TimedOut => Err(ErrorCode::InternalError(Some(
                "the response body was not fully sent within the response-body or \
                 end-to-end timeout"
                    .to_string(),
            ))),
            BodySendOutcome::Failed(message) => Err(wasip3::http::types::ErrorCode::InternalError(
                Some(message.clone()),
            )),
        };
        // The outcome is sent before the trailers, so a host slow to take the trailers doesn't
        // delay the serve path.
        let _ = done_tx.send(outcome);
        let _ = trailers_tx.write(trailers).await;
    });
    (BodyDone(done_rx), AbandonBody(abandon_tx))
}

/// The failure of a `write_all` that returned unwritten values because the host dropped the read
/// end.
const HOST_STOPPED_READING: &str = "the host stopped reading the response body";

/// How an abandoned body's send is reported.
const ABANDONED_BODY: &str =
    "the event loop finished while the response body was still open, so the body can never \
     complete; ending it without the terminating chunk";

/// The outcome of finishing sending a body.
fn outcome_at_end(remaining: &crate::http::Remaining) -> BodySendOutcome {
    if remaining.is_unfilled() {
        BodySendOutcome::Truncated
    } else {
        BodySendOutcome::Sent
    }
}

/// Feed a streamed body's chunks to the host as they arrive, until the stream ends, one of them
/// fails, the declared length was reached, or [`AbandonBody`] is signaled.
async fn write_stream_body(
    body_tx: &mut BodyStreamWriter,
    receiver: &mut crate::http::OutgoingBodyReceiver,
    mut abandoned: futures_channel::oneshot::Receiver<()>,
    remaining: &mut crate::http::Remaining,
) -> BodySendOutcome {
    loop {
        let chunk = futures_lite::future::or(async { Some(receiver.next().await) }, async {
            // A dropped sender is no signal: the caller simply had no use for one.
            if std::pin::Pin::new(&mut abandoned).await.is_err() {
                std::future::pending::<()>().await;
            }
            None
        })
        .await;
        match chunk {
            Some(Some(Ok(mut chunk))) => {
                let error = crate::http::coalesce_queued_chunks(receiver, &mut chunk);
                // Content past the declared length is dropped, and the body ends.
                let Some(chunk) = remaining.take(chunk) else {
                    return BodySendOutcome::Sent;
                };
                if !body_tx.write_all(chunk).await.is_empty() {
                    return BodySendOutcome::Failed(HOST_STOPPED_READING.to_string());
                }
                if let Some(e) = error {
                    return BodySendOutcome::Failed(e.to_string());
                }
            }
            Some(Some(Err(e))) => return BodySendOutcome::Failed(e.to_string()),
            Some(None) => return outcome_at_end(remaining),
            None => return finish_abandoned_body(body_tx, receiver, remaining).await,
        }
    }
}

/// Finish a body whose pump can no longer run, sending only the chunks the channel already
/// holds.
///
/// If they end the body, the send succeeded. Otherwise the send is reported failed, which ends
/// the stream as an abort, so the client sees the truncation rather than a complete-looking
/// response (RFC 9112 §7.1).
async fn finish_abandoned_body(
    body_tx: &mut BodyStreamWriter,
    receiver: &mut crate::http::OutgoingBodyReceiver,
    remaining: &mut crate::http::Remaining,
) -> BodySendOutcome {
    // `poll_once` takes only what is ready. A pending read means the channel is still open with
    // nothing in it, which is the abandoned-body case.
    while let Some(ready) = futures_lite::future::poll_once(receiver.next()).await {
        match ready {
            None => return outcome_at_end(remaining),
            Some(Ok(chunk)) => {
                let Some(chunk) = remaining.take(chunk) else {
                    return BodySendOutcome::Sent;
                };
                if !body_tx.write_all(chunk).await.is_empty() {
                    return BodySendOutcome::Failed(HOST_STOPPED_READING.to_string());
                }
            }
            Some(Err(e)) => return BodySendOutcome::Failed(e.to_string()),
        }
    }
    BodySendOutcome::Failed(ABANDONED_BODY.to_string())
}

/// Signals a streamed body's writer that the event loop feeding it has run out of work, so no
/// further chunks will be produced. The writer then sends what the channel already holds and ends
/// the body, as [`finish_abandoned_body`] describes. Dropping it doesn't signal the writer.
pub struct AbandonBody(futures_channel::oneshot::Sender<()>);

impl AbandonBody {
    pub fn abandon(self) {
        let _ = self.0.send(());
    }
}

/// Map a method name onto a `wasi:http` method, case-sensitively, so `patch` stays `patch`.
/// Anything unrecognized is passed through as `Other`.
fn method_of_string(method: &str) -> Method {
    match method {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "CONNECT" => Method::Connect,
        "OPTIONS" => Method::Options,
        "TRACE" => Method::Trace,
        "PATCH" => Method::Patch,
        _ => Method::Other(method.to_string()),
    }
}

fn string_of_method(method: Method) -> String {
    match method {
        Method::Get => "GET".to_string(),
        Method::Head => "HEAD".to_string(),
        Method::Post => "POST".to_string(),
        Method::Put => "PUT".to_string(),
        Method::Delete => "DELETE".to_string(),
        Method::Connect => "CONNECT".to_string(),
        Method::Options => "OPTIONS".to_string(),
        Method::Trace => "TRACE".to_string(),
        Method::Patch => "PATCH".to_string(),
        Method::Other(other) => other,
    }
}

/// Send `request` with the `wasi:http` client, returning once the response status and headers
/// are available.
pub async fn send(request: Request) -> Result<Response, Error> {
    let parsed = url::Url::parse(&request.url).map_err(|e| Error(format!("invalid URL: {e}")))?;
    let scheme = match parsed.scheme() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        other => Scheme::Other(other.to_string()),
    };
    let authority = match (parsed.host_str(), parsed.port()) {
        (Some(host), Some(port)) => Some(format!("{host}:{port}")),
        (Some(host), None) => Some(host.to_string()),
        (None, _) => None,
    };
    let path_with_query = match parsed.query() {
        Some(query) => format!("{}?{}", parsed.path(), query),
        None => parsed.path().to_string(),
    };

    let mut header_entries: Vec<(String, Vec<u8>)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), crate::http::isomorphic_encode(value)))
        .collect();
    // An in-memory body has a known length: advertise it with Content-Length (as a
    // non-streaming client would) so the peer frames it correctly. A streamed body of unknown
    // length is sent without one.
    if let OutgoingBody::Bytes(bytes) = &request.body {
        if !bytes.is_empty()
            && !header_entries
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        {
            header_entries.push((
                "Content-Length".to_string(),
                bytes.len().to_string().into_bytes(),
            ));
        }
    }
    // Appended one at a time rather than with the all-or-nothing `Fields::from_list`, so a header
    // the host refuses fails the request with an error naming it.
    let fields = Fields::new();
    for (name, value) in &header_entries {
        fields.append(name, value).map_err(|e| {
            Error(format!(
                "the host refused the request header `{name}`: {e:?}"
            ))
        })?;
    }

    let (body, trailers_tx) = body_contents(request.body);
    let (wasi_request, transmitted) = WasiRequest::new(fields, body.contents, body.trailers, None);

    wasi_request
        .set_method(&method_of_string(&request.method))
        .map_err(|()| Error("invalid request method".to_string()))?;
    wasi_request
        .set_scheme(Some(&scheme))
        .map_err(|()| Error("invalid request scheme".to_string()))?;
    wasi_request
        .set_authority(authority.as_deref())
        .map_err(|()| Error("invalid request authority".to_string()))?;
    wasi_request
        .set_path_with_query(Some(&path_with_query))
        .map_err(|()| Error("invalid request path".to_string()))?;

    // Write the body we own (in-memory or channel-streamed) and the trailers once the
    // request exists, so the host is ready to read from the stream. A host-piped body has no
    // writer here: the host reads its stream directly.
    // No timeout: the serve timeouts bound responses, not an outgoing request's body.
    drop(spawn_body_writer(body.writer, trailers_tx, None, None));

    // wasmtime aborts the request's connection when this future is dropped, cutting off the
    // response body at whatever the host has already buffered. Awaiting it keeps the connection
    // open until the exchange is over, including for a response body handed on to the host.
    wit_bindgen::spawn_local(async move {
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
    // The result future is unused; resolve it so it is not left dangling.
    wit_bindgen::spawn_local(async move {
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
    let scheme = match request.get_scheme() {
        Some(Scheme::Https) => "https".to_string(),
        Some(Scheme::Other(other)) => other,
        _ => "http".to_string(),
    };
    let authority = request
        .get_authority()
        .unwrap_or_else(|| "localhost".to_string());
    let path = request
        .get_path_with_query()
        .unwrap_or_else(|| "/".to_string());
    let url = format!("{scheme}://{authority}{path}");
    let headers = http::HeaderMap::try_from(request.get_headers())?;

    // Take the body's stream without reading it.
    let (result_tx, result_rx) = wit_future::new(|| Ok(()));
    let (body_stream, trailers) = WasiRequest::consume_body(request, result_rx);
    wit_bindgen::spawn_local(async move {
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
    let fields = Fields::new();
    for (name, value) in &headers {
        // Appended one at a time rather than converted with the all-or-nothing
        // `Fields::from_list`, so a header the host refuses, such as `keep-alive`, is dropped
        // without emptying the response's headers.
        if let Err(e) = fields.append(name.as_str(), value.as_bytes()) {
            eprintln!(
                "serve: dropping the response header `{name}`, which the host refused: {e:?}"
            );
        }
    }

    let (body, trailers_tx) = body_contents(body);
    let (response, _result) = WasiResponse::new(fields, body.contents, body.trailers);
    // The host can refuse a status that HTTP allows. A refused status becomes 500 rather than
    // `Response`'s default status.
    if response.set_status_code(status).is_err() {
        eprintln!("the host refused status {status}; answering with a 500 instead");
        let _ = response.set_status_code(500);
    }

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
            wit_bindgen::spawn_local(async move {
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
