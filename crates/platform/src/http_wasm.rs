// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The parts of the wasm backends of [`http`](crate::http) that don't depend on the `wasi:http`
//! version: how an owned outgoing body is written to the host, and how its send ends.
//!
//! [`http_p2`](crate::http_p2) and [`http_p3`](crate::http_p3) implement [`HostBodyWriter`] for
//! their body streams.

use futures_lite::StreamExt;

use crate::http::{IncomingBody, OutgoingBody, Remaining, Request};

/// The write end of an outgoing body's host stream.
pub(crate) trait HostBodyWriter {
    /// Write all of `chunk`. Returns `false` if the host stopped reading.
    async fn send_chunk(&mut self, chunk: Vec<u8>) -> bool;

    /// Copy `body` to the host stream, trimmed by `remaining`. Only called by backends that copy
    /// host bodies through the guest.
    async fn send_host_body(
        &mut self,
        body: IncomingBody,
        remaining: &mut Remaining,
    ) -> BodySendOutcome;
}

/// How a body's send ended, reported through [`BodyDone`].
pub enum BodySendOutcome {
    /// Every chunk was handed to the host.
    Sent,
    /// The body ended before the `Content-Length` the response declared for it.
    Truncated,
    /// The `timeout` given to the body writer expired before the send finished.
    TimedOut,
    /// The send failed: the host dropped the stream's read end (the connection
    /// is gone), or a streamed chunk was itself an error.
    Failed(String),
}

/// Resolves once a body's writer task has ended, with how it ended, or with `None` if the task
/// was cancelled first, as at instance teardown.
pub struct BodyDone(pub(crate) futures_channel::oneshot::Receiver<BodySendOutcome>);

impl std::future::Future for BodyDone {
    type Output = Option<BodySendOutcome>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<BodySendOutcome>> {
        std::pin::Pin::new(&mut self.0).poll(cx).map(|r| r.ok())
    }
}

/// Signals a streamed body's writer that the event loop feeding it has run out of work, so no
/// further chunks will be produced. The writer then sends what the channel already holds and ends
/// the body, as [`finish_abandoned_body`] describes. Dropping it doesn't signal the writer.
pub struct AbandonBody(pub(crate) futures_channel::oneshot::Sender<()>);

impl AbandonBody {
    pub fn abandon(self) {
        let _ = self.0.send(());
    }
}

/// The failure of a write that the host stopped reading.
pub(crate) const HOST_STOPPED_READING: &str = "the host stopped reading the response body";

/// How an abandoned body's send is reported.
const ABANDONED_BODY: &str =
    "the event loop finished while the response body was still open, so the body can never \
     complete; ending it without the terminating chunk";

/// Write `body` to `writer`, and return how the send ended.
///
/// `declared_length` is enforced for streamed bodies, and for host bodies, which only backends
/// that copy them through the guest pass. `abandoned` is the receiving end of the body's
/// [`AbandonBody`].
pub(crate) async fn write_owned_body(
    writer: &mut impl HostBodyWriter,
    body: OutgoingBody,
    declared_length: Option<u64>,
    abandoned: futures_channel::oneshot::Receiver<()>,
) -> BodySendOutcome {
    let mut remaining = Remaining::new(declared_length);
    match body {
        OutgoingBody::Bytes(bytes) => {
            // Zero-copy when the bytes are uniquely owned and Vec-backed.
            if !writer.send_chunk(Vec::from(bytes)).await {
                return BodySendOutcome::Failed(HOST_STOPPED_READING.to_string());
            }
            BodySendOutcome::Sent
        }
        OutgoingBody::Stream(mut receiver) => {
            write_stream_body(writer, &mut receiver, abandoned, &mut remaining).await
        }
        OutgoingBody::Host(body) => writer.send_host_body(body, &mut remaining).await,
        OutgoingBody::Consumed => BodySendOutcome::Sent,
    }
}

/// Run `send`, or end it with [`BodySendOutcome::TimedOut`] once `timeout` has passed.
pub(crate) async fn bounded(
    timeout: Option<std::time::Duration>,
    send: impl std::future::Future<Output = BodySendOutcome>,
) -> BodySendOutcome {
    match timeout {
        None => send.await,
        Some(limit) => {
            let expired = async {
                crate::clock::sleep(limit).await;
                BodySendOutcome::TimedOut
            };
            futures_lite::future::or(send, expired).await
        }
    }
}

/// The outcome of finishing sending a body.
pub(crate) fn outcome_at_end(remaining: &Remaining) -> BodySendOutcome {
    if remaining.is_unfilled() {
        BodySendOutcome::Truncated
    } else {
        BodySendOutcome::Sent
    }
}

/// Feed a streamed body's chunks to the host as they arrive, until the stream ends, one of them
/// fails, the declared length was reached, or [`AbandonBody`] is signaled.
async fn write_stream_body(
    writer: &mut impl HostBodyWriter,
    receiver: &mut crate::http::OutgoingBodyReceiver,
    mut abandoned: futures_channel::oneshot::Receiver<()>,
    remaining: &mut Remaining,
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
                if !writer.send_chunk(chunk).await {
                    return BodySendOutcome::Failed(HOST_STOPPED_READING.to_string());
                }
                if let Some(e) = error {
                    return BodySendOutcome::Failed(e.to_string());
                }
            }
            Some(Some(Err(e))) => return BodySendOutcome::Failed(e.to_string()),
            Some(None) => return outcome_at_end(remaining),
            None => return finish_abandoned_body(writer, receiver, remaining).await,
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
    writer: &mut impl HostBodyWriter,
    receiver: &mut crate::http::OutgoingBodyReceiver,
    remaining: &mut Remaining,
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
                if !writer.send_chunk(chunk).await {
                    return BodySendOutcome::Failed(HOST_STOPPED_READING.to_string());
                }
            }
            Some(Err(e)) => return BodySendOutcome::Failed(e.to_string()),
        }
    }
    BodySendOutcome::Failed(ABANDONED_BODY.to_string())
}

/// The scheme, authority and path with query of an absolute request URL.
pub(crate) struct UrlParts {
    pub scheme: String,
    pub authority: Option<String>,
    pub path_with_query: String,
}

impl UrlParts {
    pub(crate) fn parse(url: &str) -> Result<Self, crate::http::Error> {
        let parsed =
            url::Url::parse(url).map_err(|e| crate::http::Error(format!("invalid URL: {e}")))?;
        let authority = match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => Some(format!("{host}:{port}")),
            (Some(host), None) => Some(host.to_string()),
            (None, _) => None,
        };
        let path_with_query = match parsed.query() {
            Some(query) => format!("{}?{}", parsed.path(), query),
            None => parsed.path().to_string(),
        };
        Ok(UrlParts {
            scheme: parsed.scheme().to_string(),
            authority,
            path_with_query,
        })
    }
}

/// The header entries to send for `request`, with values [`isomorphic_encode`]d.
///
/// An in-memory body has a known length: it is advertised with a `Content-Length` (as a
/// non-streaming client would) so the peer frames it correctly. A streamed body of unknown length
/// is sent without one.
///
/// [`isomorphic_encode`]: crate::http::isomorphic_encode
pub(crate) fn request_header_entries(request: &Request) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<(String, Vec<u8>)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), crate::http::isomorphic_encode(value)))
        .collect();
    if let OutgoingBody::Bytes(bytes) = &request.body {
        if !bytes.is_empty()
            && !entries
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        {
            entries.push((
                "Content-Length".to_string(),
                bytes.len().to_string().into_bytes(),
            ));
        }
    }
    entries
}

/// Define the conversions between this crate's HTTP types and the `wasi:http` types of one
/// version, whose shapes are the same in every version: `method` and `scheme` variants, `fields`,
/// and the outgoing `request` and `response` resources.
macro_rules! wasi_http_conversions {
    (
        method: $method:ty,
        scheme: $scheme:ty,
        fields: $fields:ty,
        request: $request:ty,
        response: $response:ty $(,)?
    ) => {
        /// Map a method name onto a `wasi:http` method, case-sensitively, so `patch` stays
        /// `patch`. Anything unrecognized is passed through as `Other`.
        fn method_of_string(method: &str) -> $method {
            type M = $method;
            match method {
                "GET" => M::Get,
                "HEAD" => M::Head,
                "POST" => M::Post,
                "PUT" => M::Put,
                "DELETE" => M::Delete,
                "CONNECT" => M::Connect,
                "OPTIONS" => M::Options,
                "TRACE" => M::Trace,
                "PATCH" => M::Patch,
                _ => M::Other(method.to_string()),
            }
        }

        fn string_of_method(method: $method) -> String {
            type M = $method;
            match method {
                M::Get => "GET".to_string(),
                M::Head => "HEAD".to_string(),
                M::Post => "POST".to_string(),
                M::Put => "PUT".to_string(),
                M::Delete => "DELETE".to_string(),
                M::Connect => "CONNECT".to_string(),
                M::Options => "OPTIONS".to_string(),
                M::Trace => "TRACE".to_string(),
                M::Patch => "PATCH".to_string(),
                M::Other(other) => other,
            }
        }

        /// The URL of an incoming request with `scheme`, `authority` and `path_with_query`. A
        /// missing one is `http`, `localhost` or `/`.
        fn incoming_request_url(
            scheme: Option<$scheme>,
            authority: Option<String>,
            path_with_query: Option<String>,
        ) -> String {
            type S = $scheme;
            let scheme = match scheme {
                Some(S::Https) => "https".to_string(),
                Some(S::Other(other)) => other,
                _ => "http".to_string(),
            };
            let authority = authority.as_deref().unwrap_or("localhost");
            let path = path_with_query.as_deref().unwrap_or("/");
            format!("{scheme}://{authority}{path}")
        }

        /// `wasi:http` fields with the headers of `request` (see
        /// [`request_header_entries`](crate::http_wasm::request_header_entries)). Fails naming
        /// the first header the host refuses.
        fn request_fields(request: &crate::http::Request) -> Result<$fields, crate::http::Error> {
            // Appended one at a time rather than with the all-or-nothing `Fields::from_list`, so
            // that the error names the header the host refuses.
            let fields = <$fields>::new();
            for (name, value) in &crate::http_wasm::request_header_entries(request) {
                fields.append(name, value).map_err(|e| {
                    crate::http::Error(format!(
                        "the host refused the request header `{name}`: {e:?}"
                    ))
                })?;
            }
            Ok(fields)
        }

        /// `wasi:http` fields with `headers`. A header the host refuses, such as `keep-alive`,
        /// is left out and logged.
        fn response_fields(headers: &http::HeaderMap) -> $fields {
            let fields = <$fields>::new();
            for (name, value) in headers {
                if let Err(e) = fields.append(name.as_str(), value.as_bytes()) {
                    eprintln!(
                        "serve: dropping the response header `{name}`, which the host refused: \
                         {e:?}"
                    );
                }
            }
            fields
        }

        /// Set the method, scheme, authority and path of `request` to `method` and those of
        /// `url`.
        fn set_request_target(
            request: &$request,
            method: &str,
            url: &crate::http_wasm::UrlParts,
        ) -> Result<(), crate::http::Error> {
            type S = $scheme;
            let scheme = match url.scheme.as_str() {
                "http" => S::Http,
                "https" => S::Https,
                _ => S::Other(url.scheme.clone()),
            };
            let invalid = |part: &str| crate::http::Error(format!("invalid request {part}"));
            request
                .set_method(&method_of_string(method))
                .map_err(|()| invalid("method"))?;
            request
                .set_scheme(Some(&scheme))
                .map_err(|()| invalid("scheme"))?;
            request
                .set_authority(url.authority.as_deref())
                .map_err(|()| invalid("authority"))?;
            request
                .set_path_with_query(Some(&url.path_with_query))
                .map_err(|()| invalid("path"))
        }

        /// Set the status of `response` to `status`. The host can refuse a status that HTTP
        /// allows, and a refused status becomes 500 rather than `Response`'s default status.
        fn set_status(response: &$response, status: u16) {
            if response.set_status_code(status).is_err() {
                eprintln!("the host refused status {status}; answering with a 500 instead");
                let _ = response.set_status_code(500);
            }
        }
    };
}
pub(crate) use wasi_http_conversions;
