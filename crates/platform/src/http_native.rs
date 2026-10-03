// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The native backend of [`http`](crate::http): reqwest for the client, and a
//! channel-backed [`IncomingBody`] for bodies read off a served connection.

use futures_lite::StreamExt as _;

use crate::http::{
    send_on, BodyBytes, Error, OutgoingBody, Request, Response, BODY_CHANNEL_CAPACITY,
};

thread_local! {
    // Redirects are followed by the fetch layer (so native and wasm behave identically and
    // per spec), so the client itself must not follow them.
    //
    // TODO(keep-alive robustness): idle connection pooling is left enabled (reqwest's
    // default) for performance, but it has a known robustness gap against non-compliant
    // servers. hyper applies RFC 7230 §3.3.3 strictly: a body-exempt response (HEAD, 1xx,
    // 204, 304) has a zero-length body regardless of any `Content-Length`. A server that
    // nevertheless *sends* a body with such a status, such as WPT `status.py`,
    // leaves those bytes unread on the socket; hyper treats the message as
    // complete and returns the connection to the pool. The next request to reuse that
    // connection reads the stale bytes as a status line and fails with a parse error
    // (`hyper::Error(Parse(Version))`). hyper auto-retries idempotent methods on a fresh
    // connection, but a POST is not retried (it may already have been processed), so it
    // surfaces as a spurious network error. Symptom: the `response-null-body` WPT subtests
    // for 204/304 fail intermittently (see their FLAKY expectations).
    //
    // This cannot be fixed through reqwest: it exposes no connection-eviction or
    // idle-flush API, and dropping a body-exempt `Response` unread does not close its
    // connection (hyper considers a length-0 body already complete and pools it anyway).
    // Instead, fixing this would require building our own pool directly on hyper.
    // See wasmtime PR #12635 for the related "ignore a connection error that arrives after
    // the response" technique.
    // Ideally this is fixed upstream in reqwest/hyper (detect leftover bytes after a
    // body-exempt response and refuse to pool that connection) rather than by hand-rolling
    // a client here.
    static CLIENT: reqwest::Client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build HTTP client");
}

pub async fn send(request: Request) -> Result<Response, Error> {
    let client = CLIENT.with(|c| c.clone());
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .map_err(|e| Error(format!("invalid method: {e}")))?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), crate::http::isomorphic_encode(value));
    }
    match request.body {
        OutgoingBody::Bytes(bytes) if !bytes.is_empty() => builder = builder.body(bytes),
        OutgoingBody::Bytes(_) | OutgoingBody::Consumed => {}
        // Stream the body chunks (read from the channel) as they arrive.
        OutgoingBody::Stream(receiver) => {
            builder = builder.body(reqwest::Body::wrap_stream(receiver))
        }
        // Incoming→outgoing shortcut: stream a host body straight through.
        OutgoingBody::Host(host_body) => {
            builder = match host_body.0 {
                NativeBody::Transport(response) => {
                    builder.body(reqwest::Body::wrap_stream(response.bytes_stream()))
                }
                NativeBody::Channel(receiver) => builder.body(reqwest::Body::wrap_stream(receiver)),
            }
        }
    }
    let response = builder.send().await.map_err(|e| transport_error(&e))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                crate::http::isomorphic_decode(value.as_bytes()),
            )
        })
        .collect();
    Ok(Response {
        status,
        headers,
        body: IncomingBody(NativeBody::Transport(response)),
    })
}

/// The write end of a channel-backed [`IncomingBody`]: the native serve loop's
/// connection reader sends the request-body chunks it decodes here, and the
/// handler's `request.body` reads them out. The send awaits channel capacity, so
/// a client uploading faster than the handler consumes is paced by TCP
/// backpressure rather than buffered whole. Dropping it ends the body.
///
/// Native-only: on wasm an incoming body is already a `wasi:http` stream, which
/// [`IncomingBody`] wraps directly.
pub struct IncomingBodySender(futures_channel::mpsc::Sender<Result<BodyBytes, Error>>);

impl IncomingBodySender {
    /// Append a body chunk, awaiting channel capacity. Returns `false` if the
    /// receiver was dropped, meaning nothing will read the chunk.
    pub async fn send_chunk(&mut self, chunk: BodyBytes) -> bool {
        send_on(&mut self.0, Ok(chunk)).await
    }

    /// Abort the body with an error, awaiting capacity. The reader surfaces it as
    /// a failed [`IncomingBody::next_chunk`], so a truncated or over-long upload
    /// errors the consumer's stream instead of looking like a clean end of body.
    pub async fn send_error(&mut self, message: String) -> bool {
        send_on(&mut self.0, Err(Error(message))).await
    }
}

/// Create a channel-backed incoming body and its write end.
pub fn incoming_body_channel() -> (IncomingBodySender, IncomingBody) {
    let (sender, receiver) = futures_channel::mpsc::channel(BODY_CHANNEL_CAPACITY);
    (
        IncomingBodySender(sender),
        IncomingBody(NativeBody::Channel(receiver)),
    )
}

/// A body stream from the host: an HTTP client response, or an incoming
/// request body read off a served connection.
pub struct IncomingBody(NativeBody);

enum NativeBody {
    /// A response from the HTTP client, read straight off its connection.
    Transport(reqwest::Response),
    /// Chunks delivered on a channel by whoever owns the connection — the serve
    /// loop reading an incoming request body. A final `Err` aborts the body; the
    /// channel closing ends it. Created via [`incoming_body_channel`].
    Channel(futures_channel::mpsc::Receiver<Result<BodyBytes, Error>>),
}

impl IncomingBody {
    /// Read the entire body to a byte sequence.
    pub async fn read_all(self) -> Result<BodyBytes, Error> {
        match self.0 {
            NativeBody::Transport(response) => {
                response.bytes().await.map_err(|e| transport_error(&e))
            }
            NativeBody::Channel(mut receiver) => {
                let mut buffer = bytes::BytesMut::new();
                while let Some(chunk) = receiver.next().await {
                    buffer.extend_from_slice(&chunk?);
                }
                Ok(buffer.freeze())
            }
        }
    }

    /// Read the next chunk, or `Ok(None)` at end of body.
    pub async fn next_chunk(&mut self) -> Result<Option<BodyBytes>, Error> {
        match &mut self.0 {
            NativeBody::Transport(response) => {
                response.chunk().await.map_err(|e| transport_error(&e))
            }
            NativeBody::Channel(receiver) => receiver.next().await.transpose(),
        }
    }
}

/// The transport error for `error`: its message, followed by the messages of its sources.
fn transport_error(error: &reqwest::Error) -> Error {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    Error(message)
}
