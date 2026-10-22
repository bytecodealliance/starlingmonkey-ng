// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The native backend of [`http`](crate::http): reqwest for the client, and a
//! channel-backed [`IncomingBody`] for bodies read off a served connection.

use futures_lite::StreamExt as _;

use crate::http::{
    send_on, BodyBytes, Error, OutgoingBody, Request, Response, BODY_CHANNEL_CAPACITY,
};

thread_local! {
    // The fetch layer follows redirects, so the client must not.
    //
    // TODO: a server that sends a body with a body-exempt response (HEAD, 1xx, 204, 304) leaves
    // those bytes unread on a pooled connection, and the next request on it fails to parse its
    // response. hyper retries idempotent methods but not a POST, which makes the
    // `response-null-body` WPT subtests for 204 and 304 flaky. reqwest has no API to evict such a
    // connection, so fixing this needs a pool built on hyper or a fix upstream.
    static CLIENT: reqwest::Client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build HTTP client");
}

/// Send `request` with reqwest, returning once the response status and headers are available.
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
        OutgoingBody::Stream(receiver) => {
            builder = builder.body(reqwest::Body::wrap_stream(receiver))
        }
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

/// The write end of a channel-backed [`IncomingBody`]. Sends await channel capacity, and
/// dropping it ends the body.
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
    /// Chunks delivered on a channel created by [`incoming_body_channel`]. A final `Err` aborts
    /// the body, and closing the channel ends it.
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

    /// Read the next chunk, or return `Ok(None)` at the end of the body. A chunk is read whole, so
    /// it can hold more bytes than `buf` has capacity for. It is returned in its own allocation if
    /// it is the only reference to it, and copied into an allocation of its size otherwise, so
    /// `buf` is unused. Empty chunks are skipped.
    pub async fn read_into(&mut self, _buf: Vec<u8>) -> Result<Option<Vec<u8>>, Error> {
        loop {
            let Some(chunk) = self.next_chunk().await? else {
                return Ok(None);
            };
            if !chunk.is_empty() {
                return Ok(Some(Vec::from(chunk)));
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
