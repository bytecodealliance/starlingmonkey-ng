// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end test of native serve mode: a script registers a `fetch` handler,
//! the runtime serves HTTP requests, and each is dispatched as a `fetch` event
//! whose `respondWith` reply is sent back. Covers the request body, an async
//! (promise) response, and concurrent requests on isolated per-request loops.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{
    dechunk, raw_request, read_one_response, read_until_eof, request, request_within,
    start_chunked_upstream, start_echo_upstream, start_endless_upstream, start_serve,
    start_serve_config, start_serve_with, start_silent_upstream, start_upstream, PATIENCE,
};
use libstarling::config::RuntimeConfig;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// How long [`eventually`] keeps asking. Long enough for a loaded machine to reach the response, and
/// finite so that work which never lands fails its test.
const SETTLE_PATIENCE: Duration = Duration::from_secs(20);

/// Ask `path` until its response satisfies `settled`, and return the last one.
///
/// Work a handler leaves behind runs while the request's loop is drained after the response: a
/// `waitUntil` promise, a signal's reactions, a stream's cancel. Asking until the outcome is there
/// takes as long as the work does, rather than a fixed span chosen for the slowest machine.
///
/// Only for assertions that wait for something to happen. A request drives the loop it is served
/// on, so an assertion that something does not happen waits a fixed span instead.
fn eventually(port: u16, path: &str, settled: impl Fn(&str) -> bool) -> String {
    let deadline = std::time::Instant::now() + SETTLE_PATIENCE;
    loop {
        let answer = request(port, "GET", path, "");
        if settled(&answer) || std::time::Instant::now() >= deadline {
            return answer;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serve_dispatches_fetch_events() {
    // The handler echoes method + path + request body, responding via a promise (request.text()).
    // It reads the body off a clone and reports the signal state: `clone()` and `signal` must work
    // on an incoming request (both are backed by the signal `from_incoming` installs).
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        const clone = event.request.clone();
        event.respondWith(
            clone.text().then((body) =>
                new Response(event.request.method + ' ' + url.pathname + ' body=' + body
                    + ' aborted=' + event.request.signal.aborted)));
    })"#;
    let handle = start_serve(handler, 18137);

    // A GET (no body) and a POST (with a body) — exercises Request::from_incoming + request.text().
    assert_eq!(
        request(18137, "GET", "/foo", ""),
        "GET /foo body= aborted=false"
    );
    assert_eq!(
        request(18137, "POST", "/submit", "hello world"),
        "POST /submit body=hello world aborted=false"
    );

    // Two concurrent requests: each runs on its own event loop and must get exactly its own
    // response (no cross-request contamination).
    let a = std::thread::spawn(|| request(18137, "GET", "/a", ""));
    let b = std::thread::spawn(|| request(18137, "GET", "/b", ""));
    assert_eq!(a.join().unwrap(), "GET /a body= aborted=false");
    assert_eq!(b.join().unwrap(), "GET /b body= aborted=false");

    handle.stop();
}

/// An incoming body forwarded straight back out as an *outgoing request's* body, teed so the
/// handler reads it at the same time. Both halves have to see the whole upload: the bytes are
/// travelling from one connection to another while a second reader pulls on the same stream.
#[test]
fn serve_forwards_an_incoming_body_upstream_while_reading_it() {
    let upstream = start_echo_upstream();
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                const [forward, inspect] = event.request.body.tee();
                const sent = fetch('http://127.0.0.1:{upstream}/', {{
                    method: 'POST',
                    body: forward,
                    duplex: 'half',
                }});
                let seen = '';
                const reader = inspect.getReader();
                const decoder = new TextDecoder();
                while (true) {{
                    const {{ done, value }} = await reader.read();
                    if (done) break;
                    seen += decoder.decode(value, {{ stream: true }});
                }}
                const echoed = await (await sent).text();
                return new Response('upstream=' + echoed + ' local=' + seen);
            }})());
        }})"#
    );
    let handle = start_serve(&handler, 18209);

    assert_eq!(
        request(18209, "POST", "/", "payload bytes"),
        "upstream=payload bytes local=payload bytes"
    );

    handle.stop();
}

/// Forwarding an upstream response's body as this response's body — `new Response(upstream.body,
/// upstream)` — rather than handing back the upstream `Response` whole. The body travels as a
/// stream the handler has taken apart and rebuilt, and every chunk still has to arrive, in order.
#[test]
fn serve_forwards_an_upstream_body_as_a_stream() {
    let upstream = start_chunked_upstream(&["one ", "two ", "three"]);
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                const upstream = await fetch('http://127.0.0.1:{upstream}/');
                return new Response(upstream.body, upstream);
            }})());
        }})"#
    );
    let handle = start_serve(&handler, 18197);

    let body = request(18197, "GET", "/", "");
    assert_eq!(dechunk(&body), "one two three", "got: {body}");

    handle.stop();
}

/// A handler may respond with a stream it goes on to fill once `respondWith` has been called. That
/// is the shape every incremental response takes, where the work producing the body outlives the
/// call that promised it. The chunks written afterwards still have to reach the client.
#[test]
fn serve_streams_a_body_written_after_respond_with() {
    let handler = r#"addEventListener('fetch', async (event) => {
        const encoder = new TextEncoder();
        const body = new TransformStream({
            transform(chunk, controller) { controller.enqueue(encoder.encode(chunk)); }
        });
        const writer = body.writable.getWriter();
        // The response is committed here, with nothing in it yet.
        event.respondWith(new Response(body.readable));
        await writer.write('hello ');
        await writer.write('world');
        await writer.close();
    })"#;
    let handle = start_serve(handler, 18199);

    let body = request(18199, "GET", "/", "");
    assert_eq!(dechunk(&body), "hello world", "got: {body}");

    handle.stop();
}

/// Several upstream bodies concatenated into one response: the handler holds more than one stream
/// open at a time and drains them in order, so a chunk of the second must not appear before the
/// first has ended.
#[test]
fn serve_concatenates_several_upstream_bodies() {
    let upstream = start_chunked_upstream(&["ab", "cd"]);
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                const bodies = [];
                for (let i = 0; i < 3; i++) {{
                    bodies.push((await fetch('http://127.0.0.1:{upstream}/?n=' + i)).body);
                }}
                const out = new ReadableStream({{
                    async start(controller) {{
                        for (const body of bodies) {{
                            const reader = body.getReader();
                            while (true) {{
                                const {{ done, value }} = await reader.read();
                                if (done) break;
                                controller.enqueue(value);
                            }}
                        }}
                        controller.close();
                    }}
                }});
                return new Response(out);
            }})());
        }})"#
    );
    let handle = start_serve(&handler, 18201);

    let body = request(18201, "GET", "/", "");
    assert_eq!(dechunk(&body), "abcdabcdabcd", "got: {body}");

    handle.stop();
}

#[test]
fn serve_proxies_via_fetch() {
    // The handler proxies each request to an upstream via `fetch` and replies with the result. This
    // exercises the future-attribution fix: respondWith resolves from a `fetch` future, whose
    // reaction must release the request loop's interest *on that loop*. Two concurrent proxies must
    // each get the upstream body, with no cross-request contamination.
    let upstream = start_upstream("UPSTREAM-BODY");
    let handler = format!(
        "addEventListener('fetch', (e) => e.respondWith(fetch('http://127.0.0.1:{upstream}/')))"
    );
    let handle = start_serve(&handler, 18139);

    assert!(request(18139, "GET", "/", "").contains("UPSTREAM-BODY"));

    let a = std::thread::spawn(|| request(18139, "GET", "/a", ""));
    let b = std::thread::spawn(|| request(18139, "GET", "/b", ""));
    assert!(a.join().unwrap().contains("UPSTREAM-BODY"));
    assert!(b.join().unwrap().contains("UPSTREAM-BODY"));

    handle.stop();
}

#[test]
fn serve_streams_a_readable_stream_response_body() {
    // respondWith(new Response(stream)) with a chunk already queued: taking the send body must
    // run on the request's loop with its reaction jobs drained (previously this tripped the
    // pending-microtask assert under debugmozjs, and hung after the headers in release).
    let handler = r#"addEventListener('fetch', (event) => {
        const stream = new ReadableStream({
            start(c) {
                c.enqueue(new TextEncoder().encode('streamed '));
                c.enqueue(new TextEncoder().encode('body'));
                c.close();
            }
        });
        event.respondWith(new Response(stream));
    })"#;
    let handle = start_serve(handler, 18143);
    let body = request(18143, "GET", "/", "");
    // The body arrives chunk-framed; de-chunk and assert the exact payload, so a
    // chunk-framing corruption that merely preserved the substrings would fail.
    assert_eq!(dechunk(&body), "streamed body", "got: {body}");
    handle.stop();
}

#[test]
fn serve_enforces_request_limits() {
    let handler = r#"addEventListener('fetch', (e) => e.respondWith(new Response('ok')))"#;
    let handle = start_serve(handler, 18141);

    // A Content-Length beyond the body cap is rejected up front — the server must not
    // allocate it (previously `vec![0; content_length]` was attacker-sized).
    let response = raw_request(
        18141,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999999\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413"), "got: {response}");

    // An oversized request head (one huge header) is rejected, not buffered without bound. The
    // cap is approximate — a single read may overshoot it — so this is well past the default
    // 64 KiB rather than just over it.
    let mut huge = Vec::from(&b"GET / HTTP/1.1\r\nHost: x\r\nX-Big: "[..]);
    huge.extend(std::iter::repeat_n(b'a', 1024 * 1024));
    huge.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    let response = raw_request(18141, &huge);
    assert!(response.starts_with("HTTP/1.1 431"), "got: {response}");

    // A malformed Content-Length is a 400, not silently treated as zero.
    let response = raw_request(
        18141,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: banana\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");

    // The server survives all of the above and still serves.
    assert_eq!(request(18141, "GET", "/", ""), "ok");

    handle.stop();
}

/// Request smuggling turns on two parties framing one message differently. Every way of stating a
/// body length ambiguously has to be refused outright rather than resolved to whichever
/// interpretation happens to be ours (RFC 9112 §6.3).
#[test]
fn serve_rejects_ambiguous_body_framing() {
    let handler = r#"addEventListener('fetch', (e) => e.respondWith(new Response('ok')))"#;
    let handle = start_serve(handler, 18179);

    // Two Content-Length headers: a front-end framing by the first and this server framing by the
    // second disagree about where the request ends, so the remainder is read as a second request.
    let response = raw_request(
        18179,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nContent-Length: 41\r\n\
          Connection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");

    // The same ambiguity inside one field.
    let response = raw_request(
        18179,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0, 41\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");

    // Transfer-Encoding with Content-Length: the classic pair, each party picking a different one.
    // The message is framed by the Transfer-Encoding and the Content-Length is dropped, so what
    // the other party would have read as a second request is never served (RFC 9112 §6.3).
    let mut stream = TcpStream::connect(("127.0.0.1", 18179)).unwrap();
    stream
        .write_all(
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 44\r\nTransfer-Encoding: chunked\r\n\
              \r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: x\r\nFoo: ",
        )
        .unwrap();
    let mut carry = Vec::new();
    let first = read_one_response(&mut stream, &mut carry, PATIENCE).expect("the framed request");
    assert!(first.starts_with("HTTP/1.1 200"), "got: {first}");
    assert!(
        read_one_response(&mut stream, &mut carry, Duration::from_secs(2)).is_none(),
        "the smuggled request must not be served"
    );

    // A Transfer-Encoding whose final coding is not chunked delimits nothing at all, so there is no
    // framing to fall back on.
    let response = raw_request(
        18179,
        b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked, gzip\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");

    // Repeated Transfer-Encoding fields are one list: `gzip` then `chunked` still ends in chunked,
    // so this one *is* framed and must be served rather than caught by the check above.
    let response = raw_request(
        18179,
        b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\
          Connection: close\r\n\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");

    assert_eq!(request(18179, "GET", "/", ""), "ok");

    handle.stop();
}

#[test]
fn serve_maps_invalid_status_to_500() {
    // A handler returning `Response.error()` — a network-error response with
    // status 0 — must not serialize the protocol-illegal status line
    // `HTTP/1.1 0`; the status is surfaced as a 500.
    let handler = r#"addEventListener('fetch', (e) => e.respondWith(Response.error()))"#;
    let handle = start_serve(handler, 18151);

    let response = raw_request(
        18151,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 500"), "got: {response}");

    handle.stop();
}

#[test]
fn serve_decodes_a_chunked_request_body() {
    // A POST with Transfer-Encoding: chunked (no Content-Length) must have its
    // body decoded and delivered to the handler, not silently dropped.
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then((body) => new Response('got:' + body)));
    })"#;
    let handle = start_serve(handler, 18153);

    // "hello world" as two chunks: "hello " (0x6) then "world" (0x5).
    let chunked = b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n";
    let response = raw_request(18153, chunked);
    assert!(response.contains("got:hello world"), "got: {response}");

    // Transfer-Encoding together with Content-Length is a smuggling vector: the message is framed
    // by the Transfer-Encoding, so the body is the empty chunked one and not the five bytes the
    // Content-Length claims. `serve_rejects_ambiguous_body_framing` covers what that leaves on the
    // connection.
    let smuggle = b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\nConnection: close\r\n\r\n0\r\n\r\n";
    let response = raw_request(18153, smuggle);
    assert!(response.contains("got:"), "got: {response}");
    assert!(!response.contains("got:hello"), "got: {response}");

    handle.stop();
}

/// A client that disconnects mid-body must not leave the streamed-body pump
/// running (or parked forever on the body channel): the pull counter settles,
/// the request's in-flight slot frees up, and the server keeps serving.
#[test]
fn client_disconnect_stops_a_streamed_body_pump() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/pulls') {
            event.respondWith(new Response(String(globalThis.pulls ?? 0)));
            return;
        }
        event.respondWith(new Response(new ReadableStream({
            pull(controller) {
                globalThis.pulls = (globalThis.pulls ?? 0) + 1;
                controller.enqueue(new Uint8Array(1024));
            }
        })));
    })"#;
    let handle = start_serve(handler, 18145);

    // Open the endless stream, read a little, and drop the connection.
    {
        let mut stream = TcpStream::connect(("127.0.0.1", 18145)).unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut buf = [0u8; 4096];
        let mut read = 0;
        while read < 16 * 1024 {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            read += n;
        }
        assert!(read > 0, "the stream should have produced body bytes");
    }

    // The pump must come to rest: the pull counter settles instead of growing
    // while nobody consumes the body.
    std::thread::sleep(Duration::from_millis(400));
    let first: u64 = request(18145, "GET", "/pulls", "").parse().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let second: u64 = request(18145, "GET", "/pulls", "").parse().unwrap();
    assert_eq!(
        first, second,
        "the body pump must stop after the client disconnects"
    );

    handle.stop();
}

/// Losing the client aborts the request's signal, and nothing more. The abort tells a handler that
/// the fetch it was serving is over, so it can drop work it was doing *for that client* — an
/// upstream it passed the signal to, say. It must not touch the event's lifetime work:
/// `waitUntil` exists for exactly the things that outlive the response, and telemetry a client
/// never waited for is still telemetry that has to be sent.
///
/// The lifetime promise here deliberately ignores the signal, which is what separates this from
/// the abort test further down: that one watches the signal *from* a `waitUntil`, so it would
/// still pass if the only work that survived a disconnect were work waiting on the abort itself.
#[test]
fn a_client_disconnect_does_not_cancel_wait_until_work() {
    let handler = r#"globalThis.state = {};
    addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/check') {
            event.respondWith(new Response(JSON.stringify(globalThis.state)));
            return;
        }
        event.request.signal.addEventListener('abort', () => {
            globalThis.state.aborted = true;
        });
        // Stands in for telemetry: nothing to do with the client, and deliberately not wired to
        // the request's signal.
        event.waitUntil(new Promise((resolve) => setTimeout(() => {
            globalThis.state.telemetry = 'sent';
            resolve();
        }, 300)));
        // A body slow enough that the client can vanish part-way through it.
        const encoder = new TextEncoder();
        let n = 0;
        event.respondWith(new Response(new ReadableStream({
            async pull(controller) {
                await new Promise((r) => setTimeout(r, 50));
                controller.enqueue(encoder.encode('chunk' + (n++) + '\n'));
            }
        })));
    })"#;
    let handle = start_serve(handler, 18215);

    // Take the head and the first of the body, then vanish: the next write fails and the request's
    // signal is aborted.
    {
        let mut stream = TcpStream::connect(("127.0.0.1", 18215)).unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut buf = [0u8; 64];
        let read = stream.read(&mut buf).unwrap();
        assert!(read > 0, "the response should have started arriving");
    }

    // The 300ms lifetime promise settles after the disconnect, and the abort lands before it.
    let state = eventually(18215, "/check", |state| {
        state.contains("\"telemetry\":\"sent\"")
    });
    assert!(
        state.contains("\"aborted\":true"),
        "losing the client must abort the request's signal: {state}"
    );
    assert!(
        state.contains("\"telemetry\":\"sent\""),
        "waitUntil work must still run after the client is gone: {state}"
    );

    handle.stop();
}

/// The response goes out as soon as the respondWith promise settles — per
/// Handle Fetch, waitUntil extends the event's lifetime, not the response —
/// and the waitUntil work still completes afterwards.
#[test]
fn wait_until_does_not_delay_the_response() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/check') {
            event.respondWith(new Response(String(globalThis.waitUntilDone ?? false)));
            return;
        }
        event.respondWith(new Response('hi'));
        event.waitUntil(new Promise((resolve) => setTimeout(() => {
            globalThis.waitUntilDone = true;
            resolve();
        }, 800)));
    })"#;
    let handle = start_serve(handler, 18147);

    let started = std::time::Instant::now();
    assert_eq!(request(18147, "GET", "/", ""), "hi");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(700),
        "the response must not wait for waitUntil work (took {elapsed:?})"
    );

    // The waitUntil work still runs to completion after the response.
    assert_eq!(eventually(18147, "/check", |seen| seen == "true"), "true");

    handle.stop();
}

/// The request body reaches the handler as it arrives, not once the whole
/// request has been received. The client sends the first chunk, waits for the
/// handler to echo it back, and only then sends the second — a server that
/// buffered the body before dispatching would deadlock here, since it would be
/// waiting for a chunk the client will not send until it sees a response.
#[test]
fn serve_streams_an_incoming_request_body() {
    let handler = r#"addEventListener('fetch', (event) => {
        const reader = event.request.body.getReader();
        event.respondWith(new Response(new ReadableStream({
            async pull(controller) {
                const { done, value } = await reader.read();
                if (done) {
                    controller.close();
                    return;
                }
                controller.enqueue(value);
            }
        })));
    })"#;
    let handle = start_serve(handler, 18155);

    let mut stream = TcpStream::connect(("127.0.0.1", 18155)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(
            b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        )
        .unwrap();

    /// Read from `stream` until `body_so_far` contains `expected` past the response head.
    fn read_until(stream: &mut TcpStream, raw: &mut Vec<u8>, expected: &str) -> String {
        for _ in 0..200 {
            let body = String::from_utf8_lossy(raw)
                .split_once("\r\n\r\n")
                .map(|(_, body)| dechunk(body))
                .unwrap_or_default();
            if body.contains(expected) {
                return body;
            }
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap();
            assert_ne!(n, 0, "connection closed while waiting for {expected:?}");
            raw.extend_from_slice(&buf[..n]);
        }
        panic!("never received {expected:?}");
    }

    let mut raw = Vec::new();
    // First chunk: the echo must come back before the rest of the request is sent.
    stream.write_all(b"5\r\nfirst\r\n").unwrap();
    read_until(&mut stream, &mut raw, "first");

    // Only now send the second chunk and the terminator.
    stream.write_all(b"6\r\nsecond\r\n0\r\n\r\n").unwrap();
    let body = read_until(&mut stream, &mut raw, "second");
    assert_eq!(body, "firstsecond", "got: {body}");

    handle.stop();
}

/// A client that closes mid-body must not present the truncation to the handler
/// as a complete body: the request's stream errors, so `text()` rejects.
#[test]
fn serve_rejects_a_truncated_request_body() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then(
            (body) => new Response('complete:' + body),
            () => new Response('truncated'),
        ));
    })"#;
    let handle = start_serve(handler, 18157);

    let mut stream = TcpStream::connect(("127.0.0.1", 18157)).unwrap();
    // Declare ten bytes, send four, then close the sending side.
    stream
        .write_all(
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\nConnection: close\r\n\r\nfour",
        )
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.contains("truncated"), "got: {response}");

    handle.stop();
}

/// A trailer section longer than the head's own field-count budget ends the read with the
/// terminating empty line still unread, so what arrived is not the whole message. Reporting it as a
/// complete body would hand the handler a request that broke the server's limits — the head rejects
/// the same overrun outright.
#[test]
fn serve_rejects_an_over_budget_trailer_section() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then(
            (body) => new Response('complete:' + body),
            () => new Response('rejected'),
        ));
    })"#;
    let handle = start_serve(handler, 18291);

    let mut request = Vec::from(
        &b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
           5\r\nhello\r\n0\r\n"[..],
    );
    for i in 0..200 {
        request.extend_from_slice(format!("X-Trailer-{i}: v\r\n").as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    let response = raw_request(18291, &request);
    assert!(response.contains("rejected"), "got: {response}");

    // A trailer section within the budget still ends the body cleanly.
    let response = raw_request(
        18291,
        b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
          5\r\nhello\r\n0\r\nX-Trailer: v\r\n\r\n",
    );
    assert!(response.contains("complete:hello"), "got: {response}");

    handle.stop();
}

/// `respondWith` step 10.2.1 copies the response when the promise settles, so the client gets the
/// response as the handler passed it, not whatever the handler edited it into afterwards. The
/// `handled` reaction is a window where author code runs after the settle and before the head
/// reaches the wire.
#[test]
fn a_response_edited_after_it_was_answered_with_goes_out_as_it_was() {
    let handler = r#"addEventListener('fetch', (e) => {
        const response = new Response('body', { headers: { 'x-answered': 'yes' } });
        e.handled.then(() => {
            response.headers.set('x-late', 'yes');
            response.headers.delete('x-answered');
        });
        e.respondWith(response);
    })"#;
    let handle = start_serve(handler, 18295);

    let response = raw_request(
        18295,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let head = response.to_lowercase();
    assert!(head.contains("x-answered: yes"), "got: {response}");
    assert!(!head.contains("x-late"), "got: {response}");

    handle.stop();
}

/// RFC 9112 §3.2: more than one `Host` field is a 400. A front-end that routes by the first and a
/// server that reads the last disagree about who the request was addressed to — the routing twin of
/// the `Content-Length` ambiguity next door.
#[test]
fn serve_rejects_a_second_host_header() {
    let handler = r#"addEventListener('fetch', (e) => e.respondWith(new Response(new URL(e.request.url).host)))"#;
    let handle = start_serve(handler, 18293);

    let response = raw_request(
        18293,
        b"GET / HTTP/1.1\r\nHost: first.example\r\nHost: second.example\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "got: {response}");

    // One Host still routes as before.
    let response = raw_request(
        18293,
        b"GET / HTTP/1.1\r\nHost: only.example\r\nConnection: close\r\n\r\n",
    );
    assert!(response.contains("only.example"), "got: {response}");

    handle.stop();
}

/// An incoming request body used directly as the response body is handed to the
/// wire without being pumped through JS (the incoming→outgoing shortcut), which
/// applies because the incoming body is a host body rather than bytes.
#[test]
fn serve_pipes_an_incoming_request_body_to_the_response() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response(event.request.body));
    })"#;
    let handle = start_serve(handler, 18159);

    let mut stream = TcpStream::connect(("127.0.0.1", 18159)).unwrap();
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| dechunk(body))
        .unwrap_or_default();
    assert_eq!(body, "hello world", "got: {response}");

    handle.stop();
}

/// A chunked body's length is not known until it ends, so the size cap can only
/// be applied as it grows: the body errors mid-stream (the handler's read
/// rejects) rather than being refused with a 413 before dispatch, as a declared
/// over-cap `Content-Length` still is.
#[test]
fn serve_caps_a_chunked_request_body_mid_stream() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then(
            (body) => new Response('complete:' + body.length),
            () => new Response('too large'),
        ));
    })"#;
    const CAP: usize = 64 * 1024;
    let handle = serve_with_limits(handler, 18161, "--max-request-body-bytes 64KiB");

    // Whole, well-formed chunked bodies, terminating chunk included, so the upload never stalls
    // and only the cap can reject one. A stalled upload rejects the handler's read too, and would
    // pass an assertion on the rejection alone.
    let over = upload_chunked(18161, CAP + 1);
    assert!(over.contains("too large"), "got: {over}");

    // One byte less is delivered whole, so the cap rejected the body above rather than its
    // framing.
    let under = upload_chunked(18161, CAP);
    assert!(under.contains(&format!("complete:{CAP}")), "got: {under}");

    handle.stop();
}

/// `POST` a `size`-byte chunked body in a single chunk and return the whole response.
///
/// The server may reject the body and close mid-upload, so a write that fails is part of the
/// outcome rather than an error.
fn upload_chunked(port: u16, size: usize) -> String {
    let mut raw = Vec::from(
        &b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"[..],
    );
    raw.extend_from_slice(format!("{size:x}\r\n").as_bytes());
    raw.extend(std::iter::repeat_n(b'a', size));
    raw.extend_from_slice(b"\r\n0\r\n\r\n");

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let _ = stream.write_all(&raw);
    // Both outcomes response and close at once, so a wait anywhere near this long has already
    // failed. Shorter than `PATIENCE` so that reporting it does not cost the suite that long.
    let response = read_until_eof(&mut stream, Duration::from_secs(10)).unwrap_or_default();
    String::from_utf8_lossy(&response).into_owned()
}

/// A chunk-size line that never terminates must not make the server buffer it
/// without bound: it is cut off and the body errors, rather than growing a buffer
/// for as long as the client keeps sending.
#[test]
fn serve_caps_a_chunked_framing_line() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then(
            (body) => new Response('complete:' + body.length),
            () => new Response('bad framing'),
        ));
    })"#;
    let handle = start_serve(handler, 18165);

    let mut stream = TcpStream::connect(("127.0.0.1", 18165)).unwrap();
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
        .unwrap();
    // A chunk-size line of endless extensions, with no terminating newline.
    let mut line = Vec::from(&b"1;"[..]);
    line.extend(std::iter::repeat_n(b'a', 16 * 1024));
    stream.write_all(&line).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(response.contains("bad framing"), "got: {response}");

    handle.stop();
}

/// Cloning an incoming request materializes its host body into a stream and tees
/// it, so both halves read the same bytes — the path a handler takes to read an
/// upload twice.
#[test]
fn serve_clones_an_incoming_request_body() {
    let handler = r#"addEventListener('fetch', (event) => {
        const copy = event.request.clone();
        event.respondWith(Promise.all([event.request.text(), copy.text()])
            .then(([a, b]) => new Response(a + '|' + b)));
    })"#;
    let handle = start_serve(handler, 18163);

    assert_eq!(
        request(18163, "POST", "/", "payload"),
        "payload|payload",
        "both halves of the tee must see the body"
    );

    handle.stop();
}

/// Under `--serve-isolated` the content script is re-evaluated per request, so waiting for it to
/// finish evaluating is per-request work too. Two requests, because the point is that every one of
/// them waits, not just the first.
#[test]
fn an_isolated_request_waits_for_the_scripts_top_level_await() {
    let dir = std::env::temp_dir().join(format!(
        "starling-serve-isolated-tla-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("entry.mjs"),
        r#"await new Promise((resolve) => setTimeout(resolve, 50));
        addEventListener('fetch', (event) => {
            const url = new URL(event.request.url);
            event.respondWith(new Response('async-handler' + url.pathname));
        });
        "#,
    )
    .unwrap();

    let handle = start_serve_config(
        RuntimeConfig {
            script_path: dir.join("entry.mjs").to_string_lossy().into_owned(),
            serve: Some(18167),
            serve_isolated: true,
            ..Default::default()
        },
        18167,
    );

    assert_eq!(request(18167, "GET", "/a", ""), "async-handler/a");
    assert_eq!(request(18167, "GET", "/b", ""), "async-handler/b");

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `FetchEvent.handled` is one promise for the event's lifetime — a WebIDL promise attribute
/// returns the same object every time — and it reports how the request ended: resolved once a
/// `Response` is on its way, rejected when the handler failed to produce one.
#[test]
fn fetch_event_handled_reports_the_outcome() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/check') {
            // Taken rather than read, so each case below reads its own outcome instead of
            // whichever one the case before it left behind.
            event.respondWith(new Response(globalThis.state ?? 'never settled'));
            globalThis.state = undefined;
            return;
        }
        const identical = event.handled === event.handled;
        event.handled.then(
            () => { globalThis.state = 'resolved,same=' + identical; },
            (e) => { globalThis.state = 'rejected:' + e.name + ',same=' + identical; });
        if (url.pathname === '/responds') {
            event.respondWith(new Response('ok'));
        } else if (url.pathname === '/rejects') {
            event.respondWith(Promise.reject(new Error('no response for you')));
        } else if (url.pathname === '/invalid') {
            event.respondWith(Promise.resolve('a string is not a Response'));
        }
        // /silent falls through without calling respondWith at all.
    })"#;
    let handle = start_serve(handler, 18181);

    // `handled` settles as the request's outcome becomes final, and its reactions run while the
    // loop is drained afterwards, so ask until they have recorded something.
    let recorded = || eventually(18181, "/check", |state| state != "never settled");
    let settled = |path: &str| {
        request(18181, "GET", path, "");
        recorded()
    };

    assert_eq!(request(18181, "GET", "/responds", ""), "ok");
    assert_eq!(recorded(), "resolved,same=true");

    // A respondWith whose promise rejects is a network error, and `handled` reports it as one.
    assert_eq!(settled("/rejects"), "rejected:NetworkError,same=true");

    // A promise that settles with something that is not a `Response` is the same outcome, reached
    // through the `respond-with error flag` rather than a rejection.
    assert_eq!(settled("/invalid"), "rejected:NetworkError,same=true");

    // So is never responding at all. A browser resolves this one, having fallen back to the
    // network; there is no network here, and the client got a 500.
    assert_eq!(settled("/silent"), "rejected:NetworkError,same=true");

    handle.stop();
}

/// `waitUntil` may only extend an event that is still `active`: during dispatch, or while an
/// earlier lifetime promise is still pending. Once the event is over there is nothing left to
/// extend, since the request has been served, so it throws rather than silently doing nothing.
#[test]
fn wait_until_throws_once_the_event_is_over() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/check') {
            event.respondWith(new Response(globalThis.late ?? 'never ran'));
            return;
        }
        // During dispatch the event is active, so this is allowed.
        let during = 'allowed';
        try { event.waitUntil(Promise.resolve()); } catch (e) { during = e.name; }
        // A body left open for a moment keeps the loop running past the response without
        // extending the event — only waitUntil does that, and the one above has settled — which
        // is what gives the timer below something to run in.
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(new TextEncoder().encode('during=' + during));
                setTimeout(() => {
                    try {
                        event.waitUntil(Promise.resolve());
                        globalThis.late = 'allowed';
                    } catch (e) {
                        globalThis.late = e.name;
                    }
                    c.close();
                }, 20);
            }
        })));
    })"#;
    let handle = start_serve(handler, 18183);

    assert_eq!(dechunk(&request(18183, "GET", "/", "")), "during=allowed");
    // The timer runs while the request's loop is drained after its response.
    assert_eq!(
        eventually(18183, "/check", |late| late != "never ran"),
        "InvalidStateError"
    );

    handle.stop();
}

/// The other half of `active`: a pending lifetime promise keeps the event extendable even after
/// dispatch has finished, so `waitUntil` from a later task is allowed as long as an earlier one is
/// still outstanding. This is what distinguishes a real pending-promise count from one that is
/// never incremented — without the count, the case above would still throw for the wrong reason.
#[test]
fn wait_until_is_allowed_while_an_earlier_one_is_pending() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/check') {
            event.respondWith(new Response(globalThis.second ?? 'never ran'));
            return;
        }
        let releaseFirst;
        event.waitUntil(new Promise((resolve) => { releaseFirst = resolve; }));
        event.respondWith(new Response('ok'));
        setTimeout(() => {
            // Dispatch is long over, but the first waitUntil promise is still pending, so the
            // event is still active and this must be allowed.
            try {
                event.waitUntil(Promise.resolve());
                globalThis.second = 'allowed';
            } catch (e) {
                globalThis.second = e.name;
            }
            releaseFirst();
        }, 20);
    })"#;
    let handle = start_serve(handler, 18185);

    assert_eq!(request(18185, "GET", "/", ""), "ok");
    assert_eq!(
        eventually(18185, "/check", |second| second != "never ran"),
        "allowed"
    );

    handle.stop();
}

/// A module's top-level `await` need not be settled by the time the server starts: a script may
/// leave one waiting on something only a request can provide. Startup has to stop waiting for it
/// (or the server would never accept, and the request that would settle it could never arrive), and
/// the rest of the module still has to run once it does settle.
#[test]
fn a_top_level_await_settled_by_a_request_resumes_the_script() {
    let dir = std::env::temp_dir().join(format!("starling-serve-tla-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("entry.mjs"),
        r#"let release;
        const gate = new Promise((resolve) => { release = resolve; });
        addEventListener('fetch', (event) => {
            if (new URL(event.request.url).pathname === '/check') {
                event.respondWith(new Response(globalThis.resumed ? 'resumed' : 'still waiting'));
                return;
            }
            release();
            event.respondWith(new Response('released'));
        });
        await gate;
        globalThis.resumed = true;
        "#,
    )
    .unwrap();

    let handle = start_serve_config(
        RuntimeConfig {
            script_path: dir.join("entry.mjs").to_string_lossy().into_owned(),
            serve: Some(18207),
            ..Default::default()
        },
        18207,
    );

    // The server accepts even though the module's evaluation is still suspended.
    assert_eq!(request(18207, "GET", "/", ""), "released");
    // And once the request settles it, the script picks up where it left off.
    assert_eq!(
        eventually(18207, "/check", |state| state == "resumed"),
        "resumed"
    );

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the wire's headers look like from JS: a field that arrived in mixed case is found by any
/// casing, the incoming set is immutable (it describes what was received, which nothing can change
/// after the fact), and headers put on the response come back out with `set-cookie` kept as
/// separate fields while everything else combines.
#[test]
fn serve_exposes_incoming_headers_and_keeps_them_immutable() {
    let handler = r#"addEventListener('fetch', (event) => {
        const incoming = event.request.headers;
        let immutable = 'mutable';
        try {
            incoming.delete('user-agent');
        } catch (e) {
            immutable = e.constructor.name;
        }
        const response = new Response('ok', {
            headers: [
                ['x-seen', incoming.get('EXAMPLE-HEADER') ?? 'missing'],
                ['x-agent', incoming.get('User-Agent') ?? 'missing'],
                ['x-immutable', immutable],
            ],
        });
        response.headers.append('set-cookie', 'A');
        response.headers.append('set-cookie', 'B');
        response.headers.append('another', 'A');
        response.headers.append('another', 'B');
        event.respondWith(response);
    })"#;
    let handle = start_serve(handler, 18203);

    let response = raw_request(
        18203,
        b"GET / HTTP/1.1\r\nHost: x\r\neXample-hEader: Header Value\r\nUser-Agent: test-agent\r\n\
          Connection: close\r\n\r\n",
    );

    // Read back through the response, so casing on the wire is asserted too.
    assert!(
        response.contains("x-seen: Header Value"),
        "a mixed-case field must be found by any casing: {response}"
    );
    assert!(response.contains("x-agent: test-agent"), "got: {response}");
    assert!(
        response.contains("x-immutable: TypeError"),
        "an incoming request's headers must not be mutable: {response}"
    );
    // `set-cookie` must stay one field per cookie: combining two cookies into one field changes
    // what they mean. Other repeats may go out either as separate fields or as one comma list —
    // RFC 9110 §5.2 makes those equivalent, and this server sends them separately — but both
    // values have to survive.
    assert_eq!(
        response.matches("set-cookie: ").count(),
        2,
        "each cookie needs a field of its own: {response}"
    );
    assert!(response.contains("set-cookie: A"), "got: {response}");
    assert!(response.contains("set-cookie: B"), "got: {response}");
    assert!(
        response.contains("another: A, B")
            || (response.contains("another: A") && response.contains("another: B")),
        "both values of a repeated header must reach the client: {response}"
    );

    handle.stop();
}

/// A handler that throws instead of responding gets a 500 rather than a hung
/// connection or an empty 200 — the exception is reported and the request ends.
#[test]
fn a_throwing_handler_is_answered_with_500() {
    let handler = r#"addEventListener('fetch', () => {
        throw new Error('handler blew up');
    });
    addEventListener('fetch', (event) => {
        // A later listener still runs, and its response is the one that counts.
        if (new URL(event.request.url).pathname === '/ok') {
            event.respondWith(new Response('recovered'));
        }
    })"#;
    let handle = start_serve(handler, 18205);

    let response = raw_request(
        18205,
        b"GET /boom HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 500"), "got: {response}");

    // The throw does not take the dispatch down: a listener after it still gets the event.
    assert_eq!(request(18205, "GET", "/ok", ""), "recovered");

    handle.stop();
}

/// A handler can hand back a `ReadableStream` it then never feeds or closes. Nothing can complete
/// that body — the stream's pending read is not work the event loop can run, so the loop finishes
/// with the body still open — and waiting on it would hang the connection for good and leak its
/// in-flight slot. The server has to end the request instead, and without the terminating chunk, so
/// the client sees a truncated response rather than a complete-looking empty one.
#[test]
fn an_abandoned_response_body_does_not_hang_the_connection() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/ok') {
            event.respondWith(new Response('still serving'));
            return;
        }
        event.respondWith(new Response(new ReadableStream({
            start(_c) { /* never enqueues, never closes */ }
        })));
    })"#;
    let handle = start_serve(handler, 18193);

    // The connection has to close on its own; `request` reads to EOF, so it would hang otherwise.
    let started = std::time::Instant::now();
    let response = raw_request(
        18193,
        b"GET /abandoned HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "an abandoned body must not hold the connection open (took {elapsed:?})"
    );
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
    assert!(
        !response.ends_with("0\r\n\r\n"),
        "a body that never completed must not be terminated as if it had: {response:?}"
    );

    // And the slot is back: the server still serves.
    assert_eq!(request(18193, "GET", "/ok", ""), "still serving");

    handle.stop();
}

/// The same, on a server that handles one request at a time — where a leaked slot does not merely
/// cost a connection, it wedges the server for good.
#[test]
fn an_abandoned_response_body_does_not_wedge_an_isolated_server() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/ok') {
            event.respondWith(new Response('still serving'));
            return;
        }
        event.respondWith(new Response(new ReadableStream({ start(_c) {} })));
    })"#;
    let handle = start_serve_with(handler, 18195, true);

    raw_request(
        18195,
        b"GET /abandoned HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(request(18195, "GET", "/ok", ""), "still serving");

    handle.stop();
}

/// A streamed response's head must reach the client when it is ready, not when the first chunk is.
/// A handler that streams may take a while to produce anything — an upstream that thinks first, an
/// event stream that is idle — and the status and headers are what the client acts on meanwhile.
#[test]
fn a_streamed_response_sends_its_head_before_the_first_chunk() {
    let handler = r#"addEventListener('fetch', (event) => {
        const stream = new ReadableStream({
            start(c) {
                setTimeout(() => {
                    c.enqueue(new TextEncoder().encode('late'));
                    c.close();
                }, 700);
            }
        });
        event.respondWith(new Response(stream, { headers: { 'x-marker': 'early' } }));
    })"#;
    let handle = start_serve(handler, 18187);

    let mut stream = TcpStream::connect(("127.0.0.1", 18187)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let started = std::time::Instant::now();

    // Read just enough to have seen the head, and time how long that took.
    let mut buf = [0u8; 256];
    let read = stream.read(&mut buf).unwrap();
    let elapsed = started.elapsed();
    let head = String::from_utf8_lossy(&buf[..read]).to_string();

    assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    assert!(head.contains("x-marker: early"), "got: {head}");
    assert!(
        elapsed < Duration::from_millis(500),
        "the head must not wait for the first chunk (took {elapsed:?})"
    );

    handle.stop();
}

/// A script that leaves a repeating timer behind — a background refresh, the ordinary reason to
/// call `setInterval` at top level — still has to reach the accept loop. Waiting for its startup
/// loop to *complete* never returns, so the server would bind and then serve nothing.
#[test]
fn a_repeating_timer_does_not_prevent_serving() {
    let handler = r#"setInterval(() => { globalThis.ticks = (globalThis.ticks ?? 0) + 1; }, 20);
    addEventListener('fetch', (event) => {
        event.respondWith(new Response('served'));
    });"#;
    // `start_serve` fails the test outright if the server never accepts.
    let handle = start_serve(handler, 18171);

    assert_eq!(request(18171, "GET", "/", ""), "served");

    handle.stop();
}

/// A `setInterval` in a shared-global script is background work the script expects to keep running
/// — a cache refresh, a metrics flush — so the server has to keep driving its loop between
/// requests, not just get past it at startup.
#[test]
fn a_repeating_timer_keeps_running_between_requests() {
    let handler = r#"globalThis.ticks = 0;
    setInterval(() => { globalThis.ticks += 1; }, 20);
    addEventListener('fetch', (event) => {
        event.respondWith(new Response('ticks=' + globalThis.ticks));
    });"#;
    let handle = start_serve(handler, 18189);

    // Nothing has driven the interval yet beyond startup.
    let first: u32 = request(18189, "GET", "/", "")
        .strip_prefix("ticks=")
        .and_then(|n| n.parse().ok())
        .expect("a tick count");

    // Idle time with no requests at all: the interval has to fire on its own.
    std::thread::sleep(Duration::from_millis(500));
    let second: u32 = request(18189, "GET", "/", "")
        .strip_prefix("ticks=")
        .and_then(|n| n.parse().ok())
        .expect("a tick count");

    assert!(
        second > first,
        "the interval must fire while the server idles (went {first} -> {second})"
    );

    handle.stop();
}

/// The number after `ticks=` in the response to a request for `path` on `port`.
fn ticks_at(port: u16, path: &str) -> u32 {
    request(port, "GET", path, "")
        .strip_prefix("ticks=")
        .and_then(|n| n.parse().ok())
        .expect("a tick count")
}

/// A request handler that clears a `setInterval` the script's top level started stops it, although
/// the interval runs on the script's event loop rather than the request's.
#[test]
fn a_request_clears_a_top_level_interval() {
    let handler = r#"globalThis.ticks = 0;
    const interval = setInterval(() => { globalThis.ticks += 1; }, 20);
    addEventListener('fetch', (event) => {
        if (event.request.url.endsWith('/clear')) clearInterval(interval);
        event.respondWith(new Response('ticks=' + globalThis.ticks));
    });"#;
    let port = 18538;
    let handle = start_serve(handler, port);

    let cleared = ticks_at(port, "/clear");
    std::thread::sleep(Duration::from_millis(300));
    let later = ticks_at(port, "/");
    assert_eq!(cleared, later, "the cleared interval kept firing");

    handle.stop();
}

/// A zero-delay `setInterval` is always ready to run again, so driving the script's loop to
/// completion would pin the thread and the server would never accept anything. It has to keep
/// serving regardless.
#[test]
fn a_zero_delay_repeating_timer_does_not_starve_the_server() {
    let handler = r#"globalThis.ticks = 0;
    setInterval(() => { globalThis.ticks += 1; }, 0);
    addEventListener('fetch', (event) => {
        event.respondWith(new Response('ticks=' + globalThis.ticks));
    });"#;
    let handle = start_serve(handler, 18191);

    // Three requests in a row: each has to be served while the timer keeps re-arming.
    for _ in 0..3 {
        let body = request(18191, "GET", "/", "");
        assert!(body.starts_with("ticks="), "got: {body}");
    }

    handle.stop();
}

/// The same for an isolated server, where the script — and so its `setInterval` — is re-evaluated
/// per request: the timer must not hang each request's own startup either.
#[test]
fn a_repeating_timer_does_not_prevent_serving_isolated_requests() {
    let handler = r#"setInterval(() => {}, 20);
    addEventListener('fetch', (event) => {
        event.respondWith(new Response('served'));
    });"#;
    let handle = start_serve_with(handler, 18173, true);

    assert_eq!(request(18173, "GET", "/", ""), "served");
    assert_eq!(request(18173, "GET", "/", ""), "served");

    handle.stop();
}

/// A streamed response body must not be held open by `waitUntil` work either. The body's pump runs
/// on the request's loop, so the write has to drive that loop — but only for as long as the body
/// needs it, or the client is kept waiting on an open connection for lifetime work it has no
/// interest in (every response is `Connection: close`, so a client reading to EOF waits exactly
/// that long).
#[test]
fn wait_until_does_not_delay_a_streamed_response() {
    let handler = r#"addEventListener('fetch', (event) => {
        const stream = new ReadableStream({
            start(c) {
                c.enqueue(new TextEncoder().encode('streamed body'));
                c.close();
            }
        });
        event.respondWith(new Response(stream));
        event.waitUntil(new Promise((resolve) => setTimeout(resolve, 1500)));
    })"#;
    let handle = start_serve(handler, 18175);

    let started = std::time::Instant::now();
    let body = request(18175, "GET", "/", "");
    let elapsed = started.elapsed();
    assert_eq!(dechunk(&body), "streamed body", "got: {body}");
    assert!(
        elapsed < Duration::from_millis(1000),
        "a streamed response must not wait for waitUntil work (took {elapsed:?})"
    );

    handle.stop();
}

/// Isolation has to reach through `import`, not just the entry script. Module objects are cached
/// per path, and the cached one belongs to the global it was evaluated in — so without clearing
/// that cache the second request links against the first request's modules: their top-level code
/// never runs again in the new global (the `fetch` handler it registers is simply absent, responding
/// 500), and the state they hold outlives the request that made it.
#[test]
fn isolated_requests_re_evaluate_imported_modules() {
    let dir = std::env::temp_dir().join(format!("starling-serve-modules-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("entry.mjs"), "import './handler.mjs';\n").unwrap();
    std::fs::write(
        dir.join("handler.mjs"),
        r#"let count = 0;
        addEventListener('fetch', (event) => {
            count += 1;
            event.respondWith(new Response('count=' + count));
        });
        "#,
    )
    .unwrap();

    let handle = start_serve_config(
        RuntimeConfig {
            script_path: dir.join("entry.mjs").to_string_lossy().into_owned(),
            serve: Some(18177),
            serve_isolated: true,
            ..Default::default()
        },
        18177,
    );

    // `count=1` twice: the handler is registered afresh for the second request (a cached module
    // would leave it unregistered), and the module's own state starts over with it.
    assert_eq!(request(18177, "GET", "/", ""), "count=1");
    assert_eq!(request(18177, "GET", "/", ""), "count=1");

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each isolated request gets a global of its own: what one request leaves on `globalThis` is not
/// there for the next, and a top-level `const` does not collide with the one before it.
#[test]
fn isolated_requests_do_not_share_global_state() {
    let handler = r#"const perRequest = 'fresh';
    addEventListener('fetch', (event) => {
        const seen = globalThis.leaked === undefined ? 'none' : globalThis.leaked;
        globalThis.leaked = 'from-a-previous-request';
        event.respondWith(new Response(perRequest + ',' + seen));
    });"#;
    let handle = start_serve_with(handler, 18169, true);

    // A shared global would serve `fresh,from-a-previous-request` the second time, and would not
    // reach the second request at all, since re-evaluating `const perRequest` would throw.
    assert_eq!(request(18169, "GET", "/", ""), "fresh,none");
    assert_eq!(request(18169, "GET", "/", ""), "fresh,none");

    handle.stop();
}

/// The request's `AbortSignal` fires when the connection is lost mid-response: `Create Fetch Event
/// and Dispatch` step 17.4.20, whose "terminated" here is the response write failing.
///
/// The handler streams forever, so the client can hang up while the write is still going; a second
/// request reads back what the first one's `waitUntil` observed.
#[test]
fn a_lost_connection_aborts_the_request_signal() {
    let handler = r#"let observed = 'no-request-yet';
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/observed') {
            event.respondWith(new Response(observed));
            return;
        }
        observed = 'not-aborted';
        // Poll rather than listen, and give up well inside the lifetime timeout, so a signal that
        // never fires fails the assertion instead of hanging the test for two minutes.
        event.waitUntil(new Promise((resolve) => {
            let tries = 0;
            const check = () => {
                if (event.request.signal.aborted) {
                    observed = 'aborted:' + event.request.signal.reason.name;
                    resolve();
                } else if (++tries > 300) {
                    resolve();
                } else {
                    setTimeout(check, 10);
                }
            };
            setTimeout(check, 10);
        }));
        // A body with no end: whatever the client reads, there is always another write to fail.
        const chunk = new Uint8Array(64 * 1024);
        event.respondWith(new Response(new ReadableStream({
            pull(controller) { controller.enqueue(chunk); },
        })));
    });"#;
    let handle = start_serve(handler, 18211);

    {
        let mut stream = TcpStream::connect(("127.0.0.1", 18211)).unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        // Read enough to know the response is flowing, then hang up on it.
        let mut buf = [0u8; 1024];
        assert!(
            stream.read(&mut buf).unwrap() > 0,
            "the response should start flowing"
        );
    }

    // The abort lands while the first request's lifetime work is still being drained, so give that
    // a moment before asking.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(request(18211, "GET", "/observed", ""), "aborted:AbortError");

    handle.stop();
}

/// Startup waits for the content script to finish evaluating, and for nothing else. A timer the
/// script leaves behind is not part of that, however long it runs for — waiting on one would keep
/// the server from responding for exactly as long as the script felt like sleeping.
#[test]
fn a_timer_left_at_top_level_does_not_delay_serving() {
    let handler = r#"setTimeout(() => {}, 60000);
    addEventListener('fetch', (event) => event.respondWith(new Response('ready')));"#;
    let handle = start_serve(handler, 18241);

    let started = std::time::Instant::now();
    assert_eq!(
        request_within(18241, "/", Duration::from_secs(10)).as_deref(),
        Some("ready"),
        "the server should serve without waiting the timer out"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );

    handle.stop();
}

/// The other half: the script's *own* completion is waited for, including a top-level `await` that
/// is itself waiting on a timer. Nothing is registered until that await resolves, so a server that
/// dispatched before it would response the first request with a 500.
#[test]
fn startup_waits_for_a_top_level_await_on_a_timer() {
    let dir = std::env::temp_dir().join(format!("starling-serve-await-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("entry.mjs"),
        r#"await new Promise((resolve) => setTimeout(resolve, 400));
        addEventListener('fetch', (event) => event.respondWith(new Response('after-await')));
        "#,
    )
    .unwrap();

    let handle = start_serve_config(
        RuntimeConfig {
            script_path: dir.join("entry.mjs").to_string_lossy().into_owned(),
            serve: Some(18243),
            ..Default::default()
        },
        18243,
    );

    assert_eq!(request(18243, "GET", "/", ""), "after-await");

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Only `waitUntil` extends a fetch event's lifetime. A timer the handler leaves behind is not
/// lifetime work, so the request ends without it and the callback never runs — the reason
/// `waitUntil` exists, and why a timer is no way to keep work alive past a response.
#[test]
fn a_bare_timer_does_not_extend_the_event_lifetime() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/check') {
            event.respondWith(new Response(globalThis.ran ?? 'never ran'));
            return;
        }
        setTimeout(() => { globalThis.ran = 'ran'; }, 300);
        event.respondWith(new Response('answered'));
    })"#;
    let handle = start_serve(handler, 18237);

    assert_eq!(request(18237, "GET", "/", ""), "answered");
    // Well past the timer's delay: it went with the request's loop rather than firing.
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(request(18237, "GET", "/check", ""), "never ran");

    handle.stop();
}

/// The same for a `fetch` the handler starts and never awaits: in flight is not the same as
/// extending the event, so the request does not wait for it. Isolated mode serves one request at
/// a time, which is what makes the slot being held (or not) observable.
#[test]
fn an_unawaited_fetch_does_not_extend_the_event_lifetime() {
    let upstream = start_silent_upstream();
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            fetch('http://127.0.0.1:{upstream}/');
            event.respondWith(new Response('answered'));
        }})"#
    );
    let handle = start_serve_with(&handler, 18239, true);

    assert_eq!(request(18239, "GET", "/", ""), "answered");
    // The upstream never responds, so a request that waited for it would hold isolated mode's
    // single slot and leave this one queued behind it for good.
    assert_eq!(
        request_within(18239, "/", Duration::from_secs(5)).as_deref(),
        Some("answered"),
        "the second request should not queue behind the first's abandoned fetch"
    );

    handle.stop();
}

/// An interval-fed stream — the shape of server-sent events, a ticker, any periodic push — runs
/// until the handler closes it, repeating timers and all, and arrives whole.
#[test]
fn an_interval_driven_stream_is_served_to_completion() {
    let handler = r#"addEventListener('fetch', (event) => {
        let ticks = 0;
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                const id = setInterval(() => {
                    c.enqueue(new TextEncoder().encode('tick ' + ++ticks + ' '));
                    if (ticks === 5) {
                        clearInterval(id);
                        c.close();
                    }
                }, 50);
            },
        })));
    })"#;
    let handle = start_serve(handler, 18229);

    let body = request(18229, "GET", "/", "");
    assert_eq!(dechunk(&body), "tick 1 tick 2 tick 3 tick 4 tick 5 ");

    handle.stop();
}

/// `--response-body-timeout` truncates a body the handler never finishes: the connection closes
/// *without* the last-chunk terminator, so the client sees the truncation (RFC 9112 §7.1) instead
/// of waiting forever on a response that looks like it is still coming.
#[test]
fn a_response_body_timeout_truncates_a_body_that_never_ends() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response(new ReadableStream({
            start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
        })));
    })"#;
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18217),
            response_body_timeout: Some(1),
            ..Default::default()
        },
        18217,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", 18217)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let response = read_until_eof(&mut stream, Duration::from_secs(8))
        .expect("the timeout should close the connection, not leave it streaming");
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.contains("tick "),
        "the body should have started flowing before the cut, got: {response}"
    );
    assert!(
        !response.ends_with("0\r\n\r\n"),
        "a cut body must not carry the complete-body terminator, got: {response}"
    );

    handle.stop();
}

/// Truncating the body ends the request's fetch (step 17.4.20 again), which reaches the handler
/// through `request.signal`. Its `waitUntil` work still gets its own full window afterwards,
/// rather than what the truncated body left of one.
#[test]
fn a_response_body_timeout_aborts_the_request_signal() {
    let handler = r#"let observed = 'no-request-yet';
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/observed') {
            event.respondWith(new Response(observed));
            return;
        }
        observed = 'not-aborted';
        event.waitUntil(new Promise((resolve) => {
            let tries = 0;
            const check = () => {
                if (event.request.signal.aborted) {
                    observed = 'aborted:' + event.request.signal.reason.name;
                    resolve();
                } else if (++tries > 300) {
                    resolve();
                } else {
                    setTimeout(check, 10);
                }
            };
            setTimeout(check, 10);
        }));
        event.respondWith(new Response(new ReadableStream({
            start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
        })));
    });"#;
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18219),
            response_body_timeout: Some(1),
            ..Default::default()
        },
        18219,
    );

    {
        let mut stream = TcpStream::connect(("127.0.0.1", 18219)).unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        read_until_eof(&mut stream, Duration::from_secs(8))
            .expect("the timeout should close the connection");
    }

    // The signal fires while the first request's lifetime work is still being drained, so
    // truncating the body left that window open for the `waitUntil` promise to observe it in.
    assert_eq!(
        eventually(18219, "/observed", |seen| seen.starts_with("aborted:")),
        "aborted:TimeoutError"
    );

    handle.stop();
}

/// The phases are independent windows: `waitUntil` work gets its own full `--waituntil-timeout`
/// counted from the response being over, not whatever the body left of it. Here the body spends
/// its entire 1s window and is then truncated, and lifetime work that runs for 2s afterwards still
/// finishes. That is longer than the body's window and well inside its own.
#[test]
fn wait_until_gets_its_own_window_after_the_body_is_truncated() {
    let handler = r#"let observed = 'no-request-yet';
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/observed') {
            event.respondWith(new Response(observed));
            return;
        }
        observed = 'lifetime-work-did-not-finish';
        event.waitUntil(new Promise((resolve) => setTimeout(() => {
            observed = 'lifetime-work-finished';
            resolve();
        }, 2000)));
        event.respondWith(new Response(new ReadableStream({
            start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
        })));
    });"#;
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18233),
            response_body_timeout: Some(1),
            // The bound only has to clear the 2s of lifetime work, with room for a loaded
            // machine. We're testing that truncating the body left the window intact.
            waituntil_timeout: Some(20),
            ..Default::default()
        },
        18233,
    );

    {
        let mut stream = TcpStream::connect(("127.0.0.1", 18233)).unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        read_until_eof(&mut stream, Duration::from_secs(8))
            .expect("the body timeout should close the connection");
    }

    // The 2s of lifetime work runs inside its own window and finishes there.
    assert_eq!(
        eventually(18233, "/observed", |seen| seen == "lifetime-work-finished"),
        "lifetime-work-finished"
    );

    handle.stop();
}

/// A client too slow to accept the response is bounded the same way as a handler too slow to
/// produce it: `--response-body-timeout` is wall clock over the send, backpressure included, so a
/// peer that stops reading cannot hold the request's slot forever.
#[test]
fn a_response_body_timeout_truncates_a_response_the_client_will_not_read() {
    const BODY_BYTES: usize = 16 * 1024 * 1024;
    let handler = format!(
        "addEventListener('fetch', (event) => \
             event.respondWith(new Response(new Uint8Array({BODY_BYTES}))));"
    );
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler),
            legacy_script: true,
            serve: Some(18221),
            response_body_timeout: Some(1),
            ..Default::default()
        },
        18221,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", 18221)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    // Read nothing while the timeout runs out, so the socket buffers fill and the server's write
    // parks. Only then start reading: what was buffered arrives, and the connection then ends.
    std::thread::sleep(Duration::from_secs(3));
    let response = read_until_eof(&mut stream, Duration::from_secs(8))
        .expect("the timeout should close the connection under an unread response");
    assert!(
        response.len() < BODY_BYTES,
        "the cut must land well short of the full body, got {} bytes",
        response.len()
    );

    handle.stop();
}

/// A proxied upstream body that never ends is bounded too. `respondWith(fetch(…))` hands the
/// upstream response straight through, so its body reaches the wire as a *host* body — a different
/// send path from a JS `ReadableStream`, and one where no JS runs per chunk at all.
#[test]
fn a_response_body_timeout_truncates_an_endless_proxied_body() {
    let upstream = start_endless_upstream(Duration::from_millis(50));
    let handler = format!(
        "addEventListener('fetch', (event) => \
             event.respondWith(fetch('http://127.0.0.1:{upstream}/')));"
    );
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler),
            legacy_script: true,
            serve: Some(18235),
            response_body_timeout: Some(1),
            ..Default::default()
        },
        18235,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", 18235)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let response = read_until_eof(&mut stream, Duration::from_secs(8))
        .expect("the timeout should close the connection on a proxied body that never ends");
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.contains("tick "),
        "the proxied body should have started flowing, got: {response}"
    );
    assert!(
        !response.ends_with("0\r\n\r\n"),
        "a cut body must not carry the complete-body terminator, got: {response}"
    );

    handle.stop();
}

/// `--end-to-end-timeout` reaches the phases the per-phase flags were not set for. With no
/// `--response-body-timeout` at all, it truncates a body that never ends.
#[test]
fn an_end_to_end_timeout_truncates_a_streaming_body() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response(new ReadableStream({
            start(c) { setInterval(() => c.enqueue(new TextEncoder().encode('tick ')), 50); },
        })));
    })"#;
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18223),
            end_to_end_timeout: Some(1),
            ..Default::default()
        },
        18223,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", 18223)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let response = read_until_eof(&mut stream, Duration::from_secs(8))
        .expect("the deadline should close the connection, not leave it streaming");
    assert!(
        !String::from_utf8_lossy(&response).ends_with("0\r\n\r\n"),
        "a cut body must not carry the complete-body terminator"
    );

    handle.stop();
}

/// With no `--dispatch-timeout`, `--end-to-end-timeout` is the bound on a `respondWith` that never
/// settles: the deadline covers every phase, whether or not its own flag was given.
#[test]
fn an_end_to_end_timeout_answers_a_never_settling_dispatch() {
    let handler = "addEventListener('fetch', (e) => e.respondWith(new Promise(() => {})));";
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18225),
            end_to_end_timeout: Some(1),
            ..Default::default()
        },
        18225,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", 18225)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let response = read_until_eof(&mut stream, Duration::from_secs(8))
        .expect("the deadline should answer the request, not hang it");
    assert!(
        String::from_utf8_lossy(&response).contains("500"),
        "an abandoned dispatch answers 500"
    );

    handle.stop();
}

/// The deadline also ends a request's leftover lifetime work: a never-settling `waitUntil`
/// keeps the request's loop alive, and whatever repeating timers it carries tick on — until
/// `--end-to-end-timeout` drops the loop, work and all.
#[test]
fn an_end_to_end_timeout_stops_leftover_lifetime_work() {
    let handler = r#"addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/ticks') {
            event.respondWith(new Response(String(globalThis.ticks)));
            return;
        }
        globalThis.ticks = 0;
        setInterval(() => { globalThis.ticks += 1; }, 20);
        event.waitUntil(new Promise(() => {}));
        event.respondWith(new Response('started'));
    });"#;
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18227),
            end_to_end_timeout: Some(1),
            ..Default::default()
        },
        18227,
    );

    assert_eq!(request(18227, "GET", "/", ""), "started");
    // Well past the deadline, the interval must have stopped: two samples with a gap between
    // them read the same count.
    std::thread::sleep(Duration::from_millis(2000));
    let first = request(18227, "GET", "/ticks", "");
    std::thread::sleep(Duration::from_millis(600));
    let second = request(18227, "GET", "/ticks", "");
    // A frozen counter only means the deadline stopped the work if the work was running in the
    // first place: an interval that never ticked at all would read the same twice too.
    assert!(
        first.parse::<u32>().is_ok_and(|ticks| ticks > 0),
        "the interval should have ticked before the deadline, read: {first}"
    );
    assert_eq!(
        first, second,
        "the request's loop should have been dropped at the deadline"
    );

    handle.stop();
}

/// In isolated mode the content script runs per request, so its startup is request work too:
/// `--end-to-end-timeout` bounds a startup that dawdles, sending a 500 rather than making every
/// request wait it out.
#[test]
fn an_end_to_end_timeout_bounds_isolated_startup() {
    let handler = r#"setTimeout(() => {
        addEventListener('fetch', (e) => e.respondWith(new Response('late')));
    }, 5000);"#;
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18231),
            serve_isolated: true,
            end_to_end_timeout: Some(1),
            ..Default::default()
        },
        18231,
    );

    let started = std::time::Instant::now();
    assert_eq!(request(18231, "GET", "/", ""), "Internal Server Error");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the deadline should cut startup short, not wait for it (took {:?})",
        started.elapsed()
    );

    handle.stop();
}

/// A script that registers no `fetch` listener can never serve anything, so the server reports
/// that and exits rather than binding a port that responds to every request with a 500.
#[test]
fn a_script_with_no_fetch_listener_refuses_to_serve() {
    libstarling::register_builtins();
    let config = RuntimeConfig {
        eval_script: Some("globalThis.ready = true;".to_string()),
        legacy_script: true,
        serve: Some(18245),
        ..Default::default()
    };
    let error =
        libstarling::serve_native::serve_with_shutdown(config, 18245, async {}).unwrap_err();
    assert!(
        error.contains("fetch"),
        "the error should name what is missing, got: {error}"
    );
}

/// A top-level `await` that rejects after the script registered its listener fails startup with
/// the rejection, rather than serving from a script that did not finish.
#[test]
fn a_late_top_level_rejection_refuses_to_serve() {
    libstarling::register_builtins();
    let config = RuntimeConfig {
        eval_script: Some(
            "addEventListener('fetch', (event) => event.respondWith(new Response('served')));\n\
             await new Promise((resolve) => setTimeout(resolve, 10));\n\
             throw new Error('late configuration failure');"
                .to_string(),
        ),
        serve: Some(18473),
        ..Default::default()
    };
    let error =
        libstarling::serve_native::serve_with_shutdown(config, 18473, async {}).unwrap_err();
    assert!(error.contains("late configuration failure"), "got: {error}");
}

/// With `--serve-isolated`, a top-level `await` that rejects after the script registered its
/// listener fails the request whose evaluation it belongs to, which gets a 500.
#[test]
fn an_isolated_late_top_level_rejection_fails_the_request() {
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(
                "addEventListener('fetch', (event) => event.respondWith(new Response('served')));\n\
                 await new Promise((resolve) => setTimeout(resolve, 10));\n\
                 throw new Error('late configuration failure');"
                    .to_string(),
            ),
            serve: Some(18475),
            serve_isolated: true,
            ..Default::default()
        },
        18475,
    );

    assert_eq!(request(18475, "GET", "/", ""), "Internal Server Error");
    assert_eq!(request(18475, "GET", "/", ""), "Internal Server Error");

    handle.stop();
}

/// A phase timeout longer than `--end-to-end-timeout` is a contradiction the server refuses to
/// start with, rather than quietly serving with a window the deadline never lets a phase use.
#[test]
fn a_contradictory_timeout_config_refuses_to_serve() {
    libstarling::register_builtins();
    let config = RuntimeConfig {
        eval_script: Some("addEventListener('fetch', () => {});".to_string()),
        legacy_script: true,
        serve: Some(18341),
        end_to_end_timeout: Some(5),
        waituntil_timeout: Some(6),
        ..Default::default()
    };
    let error =
        libstarling::serve_native::serve_with_shutdown(config, 18341, async {}).unwrap_err();
    assert!(
        error.contains("--waituntil-timeout") && error.contains("--end-to-end-timeout"),
        "got: {error}"
    );
}

/// `--dispatch-timeout` bounds a `respondWith` that never settles. Without one — the default — the
/// request would wait for as long as the handler takes, which is why the timeout is opt-in.
#[test]
fn a_dispatch_timeout_answers_a_never_settling_respond_with() {
    let handler = "addEventListener('fetch', (e) => e.respondWith(new Promise(() => {})));";
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18213),
            dispatch_timeout: Some(1),
            waituntil_timeout: Some(1),
            ..Default::default()
        },
        18213,
    );

    let start = std::time::Instant::now();
    assert_eq!(request(18213, "GET", "/", ""), "Internal Server Error");
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "the timeout should have answered the request, not the test harness"
    );

    handle.stop();
}

/// Answering a timed-out request has to release its slot too, or one such request costs a slot for
/// the whole `--waituntil-timeout` window — and with none configured, which is the default, costs
/// it permanently. `--serve-isolated` gives the server a single slot, so the follow-up request only
/// gets served if the first one let go.
#[test]
fn a_dispatch_timeout_frees_the_slot_it_held() {
    let handler = r#"addEventListener('fetch', (e) => {
        if (new URL(e.request.url).pathname === '/ok') { e.respondWith(new Response('ok')); return; }
        e.respondWith(new Promise(() => {}));
    });"#;
    let handle = start_serve_config(
        RuntimeConfig {
            eval_script: Some(handler.to_string()),
            legacy_script: true,
            serve: Some(18289),
            serve_isolated: true,
            dispatch_timeout: Some(1),
            ..Default::default()
        },
        18289,
    );

    assert_eq!(request(18289, "GET", "/hang", ""), "Internal Server Error");
    assert_eq!(
        request_within(18289, "/ok", Duration::from_secs(10)).as_deref(),
        Some("ok"),
        "the timed-out request never released the server's only in-flight slot"
    );

    handle.stop();
}

/// `Response.redirect` on the way out: the status and the `Location` header both have to survive
/// serialization. WPT `resources/redirect-worker.js` (`sw=gen`) covers the same ground.
#[test]
fn serve_delivers_a_redirect_response() {
    let handler =
        "addEventListener('fetch', (e) => e.respondWith(Response.redirect('http://example.com/there', 302)));";
    let handle = start_serve(handler, 18247);

    let response = raw_request(
        18247,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 302 "),
        "expected a 302 status line, got: {response}"
    );
    assert!(
        response
            .to_lowercase()
            .contains("location: http://example.com/there"),
        "the Location header must reach the wire: {response}"
    );

    handle.stop();
}

/// Every body source a `Response` can be built from, and the `Content-Type` each implies —
/// WPT `resources/fetch-event-respond-with-custom-response-worker.js`. `Blob` and `FormData` are
/// not implemented here, so the list is what this runtime actually has.
///
/// One server, one request per source: what is under test is the body-to-wire conversion, and a
/// source that silently produced an empty body would otherwise go unnoticed.
#[test]
fn serve_delivers_every_body_source() {
    let handler = r#"
        const sources = {
            '/string': () => new Response('plain text'),
            '/buffer': () => new Response(new TextEncoder().encode('from a view').buffer),
            '/view': () => new Response(new TextEncoder().encode('from a view')),
            '/search-params': () => new Response(new URLSearchParams({ a: '1', b: '2' })),
            '/null': () => new Response(null),
        };
        addEventListener('fetch', (event) => {
            const make = sources[new URL(event.request.url).pathname];
            event.respondWith(make ? make() : new Response('no such source', { status: 404 }));
        });
    "#;
    let handle = start_serve(handler, 18249);

    for (path, expected_body, expected_type) in [
        ("/string", "plain text", "text/plain;charset=UTF-8"),
        ("/buffer", "from a view", ""),
        ("/view", "from a view", ""),
        (
            "/search-params",
            "a=1&b=2",
            "application/x-www-form-urlencoded;charset=UTF-8",
        ),
        ("/null", "", ""),
    ] {
        let response = raw_request(
            18249,
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("a complete response");
        assert_eq!(body, expected_body, "body for {path}, full: {response}");
        if expected_type.is_empty() {
            assert!(
                !head.to_lowercase().contains("content-type:"),
                "a body with no type must not get one for {path}: {head}"
            );
        } else {
            assert!(
                head.to_lowercase()
                    .contains(&format!("content-type: {}", expected_type.to_lowercase())),
                "content-type for {path}: {head}"
            );
        }
    }

    handle.stop();
}

/// A `waitUntil` promise that rejects must not disturb the response: the two are separate
/// lifetimes, and the client has already been served. WPT covers the rejection path through the
/// install/activate events; here the response is what has to survive it.
#[test]
fn a_rejected_wait_until_does_not_break_the_response() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.waitUntil(Promise.reject(new Error('waitUntil failed')));
        event.respondWith(new Response('answered anyway'));
    })"#;
    let handle = start_serve(handler, 18251);

    assert_eq!(request(18251, "GET", "/", ""), "answered anyway");

    handle.stop();
}

/// A zero-length chunk in a response stream is not a terminator: it carries no bytes and the
/// chunks around it still have to arrive, in order. WPT
/// `resources/fetch-event-respond-with-readable-stream-chunk-worker.js` enqueues an empty chunk
/// first for exactly this reason.
#[test]
fn serve_streams_an_empty_chunk_without_ending_the_body() {
    let handler = r#"addEventListener('fetch', (event) => {
        const encoder = new TextEncoder();
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(new Uint8Array(0));
                c.enqueue(encoder.encode('chunk one '));
                c.enqueue(new Uint8Array(0));
                c.enqueue(encoder.encode('chunk two'));
                c.close();
            },
        })));
    })"#;
    let handle = start_serve(handler, 18253);

    let body = request(18253, "GET", "/", "");
    assert_eq!(
        dechunk(&body),
        "chunk one chunk two",
        "an empty chunk must neither truncate the body nor corrupt framing: {body}"
    );

    handle.stop();
}

/// A handler's own `Content-Length` does not decide the framing of content already in memory: the
/// server sends the length the body actually has. A streamed body has no length until it is
/// produced, so there the declaration does frame it — see
/// [`serve_frames_a_streamed_body_by_its_declared_content_length`].
///
/// This is a deliberate divergence from WPT `xhr-content-length.https.window.js`, which asserts a
/// browser hands the page whatever the service worker wrote — an over-long, bogus or duplicated
/// `Content-Length` included. There the response goes to a page; here it goes on the wire, where a
/// declared length that contradicts the body is a framing error: `Content-Length: 10000` on five
/// bytes leaves the client waiting for 9995 that never come.
#[test]
fn serve_frames_a_response_by_its_body_not_the_handlers_content_length() {
    let handler = r#"
        const cases = {
            // Longer than the body.
            '/larger': () => new Response('short', { headers: { 'content-length': '10000' } }),
            // Not a number at all.
            '/bogus': () => new Response('short', { headers: { 'content-length': 'test' } }),
            // Two of them, which combine into one comma-joined value.
            '/duplicate': () => {
                const headers = new Headers();
                headers.append('content-length', '10000');
                headers.append('content-length', '10000');
                return new Response('short', { headers });
            },
        };
        addEventListener('fetch', (event) => {
            const make = cases[new URL(event.request.url).pathname];
            event.respondWith(make());
        });
    "#;
    let handle = start_serve(handler, 18255);

    for path in ["/larger", "/bogus", "/duplicate"] {
        let response = raw_request(
            18255,
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("a complete response");
        assert_eq!(body, "short", "body for {path}: {response}");
        assert!(
            head.to_lowercase().contains("content-length: 5"),
            "the wire length must describe the body, not the handler's header, for {path}: {head}"
        );
        assert!(
            !head.contains("10000") && !head.to_lowercase().contains("content-length: test"),
            "the handler's own length must not reach the wire for {path}: {head}"
        );
    }

    handle.stop();
}

/// A streamed body's length is not knowable before it is produced, so a handler that declares one
/// frames its own response by it. The server then holds the body to that length: content past it
/// never goes out, and a body that ends under it leaves the response truncated and the connection
/// unusable, which is what a client needs to see rather than a short body that looks complete.
///
/// A declaration that is not a single `1*DIGIT` is refused, and the response falls back to chunked.
#[test]
fn serve_frames_a_streamed_body_by_its_declared_content_length() {
    let handler = r#"
        function stream(parts) {
            return new ReadableStream({
                start(c) {
                    for (const part of parts) c.enqueue(new TextEncoder().encode(part));
                    c.close();
                },
            });
        }
        const cases = {
            '/exact': () => new Response(stream(['abc', 'de']), {
                headers: { 'content-length': '5' },
            }),
            // More content than it declared: the excess never reaches the wire.
            '/longer': () => new Response(stream(['abcde', 'XXXXX']), {
                headers: { 'content-length': '5' },
            }),
            // Less: the response is truncated and the connection cannot carry another request.
            '/shorter': () => new Response(stream(['ab']), {
                headers: { 'content-length': '5' },
            }),
            // Not a single number, so nothing frames the response but chunked.
            '/bogus': () => new Response(stream(['abcde']), {
                headers: { 'content-length': 'test' },
            }),
            '/undeclared': () => new Response(stream(['abcde'])),
        };
        addEventListener('fetch', (event) => {
            const make = cases[new URL(event.request.url).pathname];
            event.respondWith(make());
        });
    "#;
    let handle = start_serve(handler, 18257);

    for (path, body) in [("/exact", "abcde"), ("/longer", "abcde")] {
        let response = raw_request(
            18257,
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        let (head, sent) = response
            .split_once("\r\n\r\n")
            .expect("a complete response");
        assert!(
            head.to_lowercase().contains("content-length: 5"),
            "the declared length must frame {path}: {head}"
        );
        assert!(
            !head.to_lowercase().contains("transfer-encoding"),
            "a declared length leaves nothing for chunked to frame, for {path}: {head}"
        );
        assert_eq!(sent, body, "body for {path}: {response}");
    }

    // The head is framed by the declared length, and the body stops short of it, so the client
    // reads a truncated message rather than a complete-looking one.
    let response = raw_request(
        18257,
        b"GET /shorter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (head, sent) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");
    assert!(
        head.to_lowercase().contains("content-length: 5"),
        "got: {head}"
    );
    assert!(
        sent.len() < 5,
        "the body must stop short of the length it declared: {response:?}"
    );

    for path in ["/bogus", "/undeclared"] {
        let response = raw_request(
            18257,
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        let (head, sent) = response
            .split_once("\r\n\r\n")
            .expect("a complete response");
        assert!(
            head.to_lowercase().contains("transfer-encoding: chunked"),
            "with no length to frame by, {path} is chunked: {head}"
        );
        assert!(!head.contains("test"), "got: {head}");
        assert_eq!(dechunk(sent), "abcde", "body for {path}: {response}");
    }

    handle.stop();
}

/// A response proxied straight from a `fetch` carries the upstream's `Content-Length` through,
/// rather than being re-framed as chunked: the upstream already measured the body being forwarded.
#[test]
fn serve_forwards_an_upstreams_content_length() {
    let upstream = start_echo_upstream();
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith(fetch('http://127.0.0.1:{upstream}/', {{
                method: 'POST',
                body: 'payload',
            }}));
        }})"#
    );
    let handle = start_serve(&handler, 18259);

    let response = raw_request(
        18259,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");
    assert!(
        head.to_lowercase().contains("content-length: 7"),
        "the upstream's length must frame the proxied response: {head}"
    );
    assert!(
        !head.to_lowercase().contains("transfer-encoding"),
        "got: {head}"
    );
    assert_eq!(body, "payload", "got: {response}");

    handle.stop();
}

/// A header value carrying a null byte is refused where it is built, not on the way out. WPT
/// `resources/invalid-header-worker.js` expects the *response* to become a network error, which is
/// how a browser reports it; here `Headers` rejects the value outright, so the handler never gets a
/// `Response` to respond with. Either way the bytes never reach the wire, which is the point.
#[test]
fn a_response_header_value_with_a_null_byte_is_refused() {
    let handler = r#"addEventListener('fetch', (event) => {
        let outcome;
        try {
            new Response('x', { headers: { 'x-test': 'b\0r' } });
            outcome = 'constructed';
        } catch (e) {
            outcome = e.name;
        }
        event.respondWith(new Response(outcome));
    })"#;
    let handle = start_serve(handler, 18343);

    assert_eq!(request(18343, "GET", "/", ""), "TypeError");

    handle.stop();
}

/// A response body that errors after sending bytes: the client gets what was produced, and then the
/// connection ends *without* the terminating zero-length chunk, so a truncated body cannot be
/// mistaken for a complete one. WPT `resources/fetch-error-worker.js` asserts the read rejects
/// rather than hanging; on the wire the missing terminator is what carries that.
#[test]
fn a_response_stream_that_errors_mid_flight_leaves_the_body_unterminated() {
    // The error comes from a timer, and a generous one. A body cannot outlive its request's event
    // loop going idle, so the stream holds the loop open until it errors rather than waiting for
    // the client. The delay only has to outlast the pump reaching the wire, which a GC zeal build
    // makes much slower.
    let handler = r#"addEventListener('fetch', (event) => {
        const encoder = new TextEncoder();
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(encoder.encode('first-chunk-'));
                setTimeout(() => c.error(new Error('stream broke')), 1000);
            },
        })));
    })"#;
    let handle = start_serve(handler, 18339);

    let response = raw_request(
        18339,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (_, body) = response.split_once("\r\n\r\n").expect("a complete head");
    assert!(
        body.contains("first-chunk-"),
        "the bytes produced before the error must still arrive: {response}"
    );
    assert!(
        !body.ends_with("0\r\n\r\n"),
        "an errored body must not be terminated as if it were complete: {response}"
    );

    handle.stop();
}

/// Chunks queued faster than the transport writes them are written together, in batches of up to
/// 64 KiB: 100 distinct 1 KiB chunks enqueued at once arrive intact and in order.
#[test]
fn serve_streams_many_queued_chunks_in_order() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                for (let i = 0; i < 100; i++) {
                    c.enqueue(new TextEncoder().encode(String(i).padStart(3, '0').repeat(341) + '|'));
                }
                c.close();
            },
        })));
    })"#;
    let handle = start_serve(handler, 18471);
    let body = request(18471, "GET", "/", "");
    let expected: String = (0..100)
        .map(|i| format!("{:03}", i).repeat(341) + "|")
        .collect();
    assert_eq!(dechunk(&body), expected);
    handle.stop();
}

/// Chunks queued ahead of an invalid chunk are written before the body errors, and the body then
/// ends unterminated.
#[test]
fn queued_chunks_before_an_invalid_chunk_are_written_before_the_error() {
    let handler = r#"addEventListener('fetch', (event) => {
        const encoder = new TextEncoder();
        event.respondWith(new Response(new ReadableStream({
            start(c) {
                c.enqueue(encoder.encode('first-'));
                c.enqueue(encoder.encode('second-'));
                c.enqueue('a string, not a Uint8Array');
                c.close();
            },
        })));
    })"#;
    let handle = start_serve(handler, 18472);
    let response = raw_request(
        18472,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (_, body) = response.split_once("\r\n\r\n").expect("a complete head");
    assert!(body.contains("first-second-"), "got: {response}");
    assert!(!body.ends_with("0\r\n\r\n"), "got: {response}");
    handle.stop();
}

/// The same, for a stream that enqueues something that is not a `Uint8Array`: the body errors
/// before any byte is written, so the head is already committed and the connection ends
/// unterminated. WPT
/// `resources/fetch-event-respond-with-response-body-with-invalid-chunk-worker.js`.
#[test]
fn a_response_stream_with_an_invalid_chunk_leaves_the_body_unterminated() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response(new ReadableStream({
            start(c) { c.enqueue('a string, not a Uint8Array'); c.close(); },
        })));
    })"#;
    let handle = start_serve(handler, 18261);

    let response = raw_request(
        18261,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (head, body) = response.split_once("\r\n\r\n").expect("a complete head");
    assert!(
        head.starts_with("HTTP/1.1 200 "),
        "the head goes out before the first chunk is pulled, so it is still a 200: {response}"
    );
    assert!(
        !body.ends_with("0\r\n\r\n"),
        "a body that never produced a valid chunk must not be terminated: {response}"
    );

    handle.stop();
}

/// An unusual status reaches the client untouched, and the request behind it is dispatched exactly
/// once — nothing retries it. WPT `fetch-event.https.html` pins this with a 421 (`Misdirected
/// Request`), the status a client is most tempted to retry elsewhere, and asserts the origin saw
/// `'Request was sent 1 times.'`.
///
/// The count is read back on a second request, which is why this server keeps one global.
#[test]
fn serve_delivers_an_unusual_status_without_retrying_it() {
    let handler = r#"
        globalThis.dispatches = 0;
        addEventListener('fetch', (event) => {
            const path = new URL(event.request.url).pathname;
            if (path === '/count') {
                event.respondWith(new Response('dispatched ' + globalThis.dispatches + ' times'));
                return;
            }
            globalThis.dispatches++;
            event.respondWith(new Response('misdirected', { status: 421 }));
        });
    "#;
    let handle = start_serve(handler, 18263);

    let response = raw_request(
        18263,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 421 "),
        "a 421 must reach the client as a 421: {response}"
    );
    assert!(
        response.ends_with("misdirected"),
        "the body must survive too: {response}"
    );

    assert_eq!(
        request(18263, "GET", "/count", ""),
        "dispatched 1 times",
        "a 421 must not be retried behind the handler's back"
    );

    handle.stop();
}

/// A urlencoded POST as a handler sees it: method, `Content-Type` and the body all have to arrive
/// intact. WPT `fetch-event.https.html` (`?form-post`) drives this from an HTML form; on the wire
/// it is just a POST, which is what makes it portable here.
#[test]
fn serve_exposes_a_urlencoded_post_to_the_handler() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(event.request.text().then((body) => new Response(
            event.request.method + ':' + event.request.headers.get('content-type') + ':' + body,
        )));
    })"#;
    let handle = start_serve(handler, 18265);

    let body = "testName1=testValue1&testName2=testValue2";
    let raw = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let response = raw_request(18265, raw.as_bytes());
    let (_, got) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");

    assert_eq!(
        got,
        format!("POST:application/x-www-form-urlencoded:{body}"),
        "full response: {response}"
    );

    handle.stop();
}

/// A handler may `fetch()` its own server: the second request is dispatched while the first is
/// still waiting on it, so a server that could only be inside one dispatch at a time would
/// deadlock here. The existing proxy tests all reach a *separate* upstream, which never exercises
/// that.
#[test]
fn a_handler_can_fetch_its_own_server() {
    // The handler addresses itself, so the port is bound once and interpolated: a renumbering that
    // updated only `start_serve` would otherwise leave this fetching a dead — or worse, another
    // test's — port, and fail as something other than the deadlock this is watching for.
    let port = 18267;
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            const path = new URL(event.request.url).pathname;
            if (path === '/inner') {{
                event.respondWith(new Response('from the inner handler'));
                return;
            }}
            event.respondWith(
                fetch('http://127.0.0.1:{port}/inner')
                    .then((r) => r.text())
                    .then((text) => new Response('outer saw: ' + text)),
            );
        }})"#
    );
    let handle = start_serve(&handler, port);

    assert_eq!(
        request_within(port, "/outer", Duration::from_secs(10)).as_deref(),
        Some("outer saw: from the inner handler"),
        "a handler fetching its own server must not deadlock"
    );

    handle.stop();
}

/// The shape of the `Request` a wire request turns into. None of this is asserted anywhere else, so
/// a change to any of it — deliberate or not — currently goes unnoticed.
///
/// These are the server-side responses, and several differ from what a browser would report for the
/// navigation this resembles: WPT `request-end-to-end.https.html` expects `mode:'navigate'`,
/// `credentials:'include'` and `redirect:'manual'` for a top-level document request. There is no
/// navigation here and no client, so the values are the plain `Request` defaults instead. The point
/// of pinning them is that the difference should be a decision, not a drift.
#[test]
fn serve_exposes_the_incoming_request_shape() {
    let handler = r#"addEventListener('fetch', (event) => {
        const request = event.request;
        event.respondWith(new Response([
            'method=' + request.method,
            'url=' + request.url,
            'destination=' + JSON.stringify(request.destination),
            'mode=' + request.mode,
            'credentials=' + request.credentials,
            'redirect=' + request.redirect,
            'referrer=' + request.referrer,
            'referrerPolicy=' + JSON.stringify(request.referrerPolicy),
            'cache=' + request.cache,
            'integrity=' + JSON.stringify(request.integrity),
            'keepalive=' + request.keepalive,
            'isReloadNavigation=' + request.isReloadNavigation,
            'isHistoryNavigation=' + request.isHistoryNavigation,
            'bodyUsed=' + request.bodyUsed,
            'hasSignal=' + (request.signal !== undefined && request.signal !== null),
            'clientId=' + JSON.stringify(event.clientId),
            'resultingClientId=' + JSON.stringify(event.resultingClientId),
            'isTrusted=' + event.isTrusted,
            'cancelable=' + event.cancelable,
            'type=' + event.type,
        ].join('\n')));
    })"#;
    let handle = start_serve(handler, 18269);

    // The URL is rebuilt from the request target and the `Host` header — this helper sends
    // `Host: localhost`, with no port, and that is exactly what comes back.
    assert_eq!(
        request(18269, "GET", "/shape?q=1", ""),
        [
            "method=GET",
            "url=http://localhost/shape?q=1",
            "destination=\"\"",
            "mode=no-cors",
            "credentials=same-origin",
            "redirect=follow",
            "referrer=about:client",
            "referrerPolicy=\"\"",
            "cache=default",
            "integrity=\"\"",
            "keepalive=false",
            "isReloadNavigation=false",
            "isHistoryNavigation=false",
            "bodyUsed=false",
            "hasSignal=true",
            // No client originates a server-side request, so both ids stay empty.
            "clientId=\"\"",
            "resultingClientId=\"\"",
            "isTrusted=true",
            "cancelable=true",
            "type=fetch",
        ]
        .join("\n")
    );

    handle.stop();
}

/// A response to `HEAD` carries the headers but no body (RFC 9110 §9.3.2). On a connection the
/// client keeps open, leftover bytes are read as the start of the next response.
#[test]
fn serve_answers_a_head_request_without_a_body() {
    let handler =
        "addEventListener('fetch', (e) => e.respondWith(new Response('BODYBYTES', { status: 201 })));";
    let handle = start_serve(handler, 18271);

    let response = raw_request(
        18271,
        b"HEAD / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (head, body) = response.split_once("\r\n\r\n").expect("a complete head");

    assert!(
        head.starts_with("HTTP/1.1 201 "),
        "the head is still the handler's: {response}"
    );
    assert_eq!(
        body, "",
        "a HEAD response must carry no body, but got: {response}"
    );

    handle.stop();
}

/// Answering a `HEAD` must not produce the content it then refuses to send: a streamed body's
/// `pull()` is the handler generating exactly that. It still declares the length of a body already
/// in memory, which costs nothing to measure.
#[test]
fn a_head_request_does_not_run_the_body_it_will_not_send() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/pulls') {
            event.respondWith(new Response(String(globalThis.pulls ?? 0)));
            return;
        }
        event.respondWith(new Response(new ReadableStream({
            pull(controller) {
                globalThis.pulls = (globalThis.pulls ?? 0) + 1;
                controller.enqueue(new TextEncoder().encode('chunk'));
            },
        })));
    })"#;
    let handle = start_serve(handler, 18297);

    let response = raw_request(
        18297,
        b"HEAD / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "got: {response}");
    // A stream's length is not knowable without producing it, so none is declared.
    assert!(
        !response.to_lowercase().contains("content-length"),
        "got: {response}"
    );

    // The stream is constructed either way, which pulls once to fill its queue; what must not
    // happen is the send pump reading from it on top of that.
    assert_eq!(
        request(18297, "GET", "/pulls", ""),
        "1",
        "the handler's pull() ran for a body the HEAD response never carried"
    );

    handle.stop();
}

/// A `204 No Content` is framed by its status: no `Content-Length`, no `Transfer-Encoding` (RFC
/// 9110 §8.6, RFC 9112 §6.2). The same holds for `304`.
#[test]
fn a_bodiless_status_has_no_framing_headers() {
    let handler = r#"addEventListener('fetch', (event) => {
        const status = Number(new URL(event.request.url).pathname.slice(1));
        event.respondWith(new Response(null, { status }));
    })"#;
    let handle = start_serve(handler, 18273);

    for status in [204, 304] {
        let response = raw_request(
            18273,
            format!("GET /{status} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        let head = response
            .split_once("\r\n\r\n")
            .map_or(&*response, |(h, _)| h);
        let head = head.to_lowercase();

        assert!(
            !head.contains("content-length"),
            "a {status} must not be framed with Content-Length: {response}"
        );
        assert!(
            !head.contains("transfer-encoding"),
            "a {status} must not be framed with Transfer-Encoding: {response}"
        );
    }

    handle.stop();
}

/// `respondWith(response)` consumes the response's body, so `bodyUsed` reads true afterwards —
/// WPT `fetch-event.https.html` (`?used-check`), which reads the flag back on a later request.
#[test]
fn respond_with_marks_the_response_body_used() {
    let handler = r#"
        globalThis.stored = null;
        addEventListener('fetch', (event) => {
            if (new URL(event.request.url).pathname === '/check') {
                event.respondWith(new Response('bodyUsed: ' + globalThis.stored.bodyUsed));
                return;
            }
            globalThis.stored = new Response('payload');
            event.respondWith(globalThis.stored);
        });
    "#;
    let handle = start_serve(handler, 18275);

    assert_eq!(request(18275, "GET", "/", ""), "payload");
    assert_eq!(request(18275, "GET", "/check", ""), "bodyUsed: true");

    handle.stop();
}

/// Sending a response hands its bytes to the transport rather than copying them, so the `Response`
/// is left holding none of the buffer. What content sees must not move with them: the body still
/// reads as one that was read — a consumed stream that cannot be read again — rather than as a body
/// that was never there.
#[test]
fn a_sent_byte_body_reads_as_consumed_rather_than_absent() {
    let handler = r#"
        globalThis.stored = null;
        addEventListener('fetch', (event) => {
            if (new URL(event.request.url).pathname === '/check') {
                event.respondWith((async () => {
                    const body = globalThis.stored.body;
                    let reread;
                    try {
                        await globalThis.stored.text();
                        reread = 'ok';
                    } catch (e) {
                        reread = e.name;
                    }
                    return new Response('body=' + (body === null ? 'null' : 'stream')
                        + ' locked=' + (body !== null && body.locked)
                        + ' reread=' + reread);
                })());
                return;
            }
            globalThis.stored = new Response('payload');
            event.respondWith(globalThis.stored);
        });
    "#;
    let handle = start_serve(handler, 18325);

    assert_eq!(request(18325, "GET", "/", ""), "payload");
    assert_eq!(
        request(18325, "GET", "/check", ""),
        "body=stream locked=true reread=TypeError"
    );

    handle.stop();
}

/// `new Request(input)` takes the input's body over rather than copying it, so the input is left
/// holding no bytes — while still reading as a body that was read, exactly as after a `text()`.
///
/// The taking is the point: the copy that reaches the new request must be the same bytes, so
/// reading them there has to still produce the payload.
#[test]
fn constructing_a_request_from_another_takes_its_bytes_over() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith((async () => {
            const input = new Request('https://example.com/', {
                method: 'POST',
                body: 'payload',
            });
            const taken = new Request(input);
            let reread;
            try {
                await input.text();
                reread = 'ok';
            } catch (e) {
                reread = e.name;
            }
            return new Response('used=' + input.bodyUsed
                + ' body=' + (input.body === null ? 'null' : 'stream')
                + ' reread=' + reread
                + ' taken=' + await taken.text());
        })());
    })"#;
    let handle = start_serve(handler, 18327);

    assert_eq!(
        request(18327, "GET", "/", ""),
        "used=true body=stream reread=TypeError taken=payload"
    );

    handle.stop();
}

/// Responding with a `Response` whose body was already consumed is a network error — WPT
/// `resources/fetch-event-network-error-worker.js` (`?used-body`). It has to be refused before the
/// head goes out: once the status line is committed there is no way left to report the failure.
#[test]
fn responding_with_a_used_stream_backed_response_is_a_network_error() {
    let handler = r#"
        globalThis.stored = null;
        addEventListener('fetch', (event) => {
            if (new URL(event.request.url).pathname === '/reuse') {
                event.respondWith(globalThis.stored);
                return;
            }
            globalThis.stored = new Response(new ReadableStream({
                start(c) { c.enqueue(new TextEncoder().encode('payload')); c.close(); },
            }));
            event.respondWith(globalThis.stored);
        });
    "#;
    let handle = start_serve(handler, 18277);

    assert_eq!(dechunk(&request(18277, "GET", "/", "")), "payload");

    let response = raw_request(
        18277,
        b"GET /reuse HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 500 "),
        "a used body must be refused before the head is committed, but got: {response}"
    );

    handle.stop();
}

/// A header value is a byte sequence, and the `ByteString` a script supplies maps each code unit
/// onto one byte: `'ßÀ¿'` (U+00DF U+00C0 U+00BF) is the three bytes `DF C0 BF`. Sending the UTF-8
/// encoding instead would be a different header value — six bytes — which a peer matching on it
/// would not match, and which comes back as six code units if echoed.
///
/// WPT `resources/iso-latin1-header-worker.js` asserts the load succeeds; on the wire what matters
/// is the bytes, so this reads them directly rather than through a `String`.
#[test]
fn a_response_header_value_is_sent_as_bytes_not_utf8() {
    let handler =
        "addEventListener('fetch', (e) => e.respondWith(new Response('x', { headers: { 'x-test': '\u{df}\u{c0}\u{bf}' } })));";
    let handle = start_serve(handler, 18279);

    let mut stream = TcpStream::connect(("127.0.0.1", 18279)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let raw = read_until_eof(&mut stream, Duration::from_secs(10)).expect("a complete response");

    let needle = b"x-test: \xdf\xc0\xbf\r\n";
    assert!(
        raw.windows(needle.len()).any(|window| window == needle),
        "the header must be the three bytes DF C0 BF, got: {:?}",
        String::from_utf8_lossy(&raw)
    );

    handle.stop();
}

/// The `respondWith` outcomes WPT `resources/fetch-event-network-error-worker.js` pins, each
/// against the control that must still succeed. All of them end as a network error here — a `500`,
/// since there is no network to fall back to.
///
/// `/used-body` is the shape that ties the two halves together: `await r.text()` resumes in a
/// microtask, so it only reaches `respondWith` at all because a microtask is still inside the
/// dispatch, and the body it then hands over has nothing left to send.
#[test]
fn respond_with_rejects_the_responses_it_cannot_send() {
    let handler = r#"addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        switch (path) {
            // A promise that resolves to null is not a Response.
            case '/resolve-null':
                event.respondWith(Promise.resolve(null));
                return;
            // A rejection carrying no reason at all still has to be handled.
            case '/reject-bare':
                event.respondWith(Promise.reject());
                return;
            // The body was read before it was handed over.
            case '/used-body':
                event.respondWith((async () => {
                    const response = new Response('payload');
                    await response.text();
                    return response;
                })());
                return;
            // The control: same shape, body left alone.
            case '/unused-body':
                event.respondWith((async () => new Response('payload'))());
                return;
            default:
                event.respondWith(new Response('unknown'));
        }
    })"#;
    let handle = start_serve(handler, 18281);

    for path in ["/resolve-null", "/reject-bare", "/used-body"] {
        let response = raw_request(
            18281,
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        assert!(
            response.starts_with("HTTP/1.1 500 "),
            "{path} must be a network error, got: {response}"
        );
    }

    assert_eq!(
        request(18281, "GET", "/unused-body", ""),
        "payload",
        "an untouched body must still be sendable"
    );

    handle.stop();
}

/// `preventDefault()` and `respondWith()` are independent: cancelling the event without responding
/// is a network error, but a *later* listener may still respond — `respondWith` is what stops
/// propagation, and a listener that only cancels does not.
///
/// WPT `resources/fetch-event-network-error-worker.js`, cases `?prevent-default` and
/// `?prevent-default-and-respond-with`.
#[test]
fn prevent_default_does_not_stop_a_later_listener_responding() {
    let handler = r#"
        addEventListener('fetch', (event) => { event.preventDefault(); });
        addEventListener('fetch', (event) => {
            if (new URL(event.request.url).pathname === '/respond') {
                event.respondWith(new Response('answered by the second listener'));
            }
        });
    "#;
    let handle = start_serve(handler, 18283);

    assert_eq!(
        request(18283, "GET", "/respond", ""),
        "answered by the second listener"
    );

    let cancelled = raw_request(
        18283,
        b"GET /cancel-only HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(
        cancelled.starts_with("HTTP/1.1 500 "),
        "cancelling without responding is a network error, got: {cancelled}"
    );

    handle.stop();
}

/// A client that goes away cancels the response body's stream, so the underlying source's
/// `cancel()` runs and a handler can stop filling a body nobody is reading. WPT
/// `resources/fetch-event-respond-with-readable-stream-worker.js` (`observe-cancel`) reads the
/// outcome back on a later request, as this does.
///
/// Distinct from `client_disconnect_stops_a_streamed_body_pump`, which asserts the pump comes to
/// rest; this asserts JS is told why.
#[test]
fn a_client_disconnect_cancels_the_response_stream_in_the_handler() {
    let handler = r#"addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/query') {
            event.respondWith(new Response(globalThis.cancelled ?? 'not cancelled'));
            return;
        }
        event.respondWith(new Response(new ReadableStream({
            pull(controller) { controller.enqueue(new Uint8Array(1024)); },
            cancel(reason) { globalThis.cancelled = 'cancelled'; },
        })));
    })"#;
    let handle = start_serve(handler, 18285);

    {
        let mut stream = TcpStream::connect(("127.0.0.1", 18285)).unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut buf = [0u8; 4096];
        let mut read = 0;
        while read < 8 * 1024 {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            read += n;
        }
        assert!(read > 0, "the stream should have produced body bytes");
    }

    // The cancel reaches JS while the abandoned request's loop drains.
    assert_eq!(
        eventually(18285, "/query", |seen| seen == "cancelled"),
        "cancelled",
        "the handler's stream must be cancelled when the client stops reading"
    );

    handle.stop();
}

/// The boundary of `active` — pending promises count above zero *or* the dispatch flag set — as
/// WPT `resources/extendable-event-async-waituntil.js` maps it out.
///
/// `/immediate` settles its lifetime promise straight away, so the later `waitUntil` calls land in
/// the dispatch's own microtask checkpoint and are allowed however deep. `/task` settles it from a
/// timer, leaving only the pending count: `add lifetime promise` step 5 queues a *microtask* to
/// decrement it, so a reaction on the promise itself still counts as active and one microtask
/// later does not.
#[test]
fn wait_until_tracks_the_end_of_the_events_active_window() {
    let handler = r#"
        globalThis.log = [];
        const attempt = (event, label) => {
            try { event.waitUntil(Promise.resolve()); log.push(label + ':ok'); }
            catch (err) { log.push(label + ':' + err.name); }
        };
        addEventListener('fetch', (event) => {
            const path = new URL(event.request.url).pathname;
            if (path === '/log') {
                event.respondWith(new Response(log.join(',')));
                return;
            }
            const settled = path === '/immediate'
                ? Promise.resolve()
                : new Promise((resolve) => setTimeout(resolve, 1));
            event.waitUntil(settled);
            // Directly in the reaction on the lifetime promise...
            settled.then(() => attempt(event, path.slice(1) + '-sync'));
            // ...and one microtask further out.
            settled.then(() => Promise.resolve())
                .then(() => attempt(event, path.slice(1) + '-extra'));
            event.respondWith(new Response('answered'));
        });
    "#;
    let handle = start_serve(handler, 18287);

    assert_eq!(request(18287, "GET", "/immediate", ""), "answered");
    assert_eq!(request(18287, "GET", "/task", ""), "answered");
    // The lifetime work runs after each response. The log is complete once the last of it is in.
    const LOG: &str =
        "immediate-sync:ok,immediate-extra:ok,task-sync:ok,task-extra:InvalidStateError";
    assert_eq!(
        eventually(18287, "/log", |log| log == LOG),
        LOG,
        "the dispatch flag covers the whole checkpoint; after it, the pending count expires one \
         microtask after the promise settles"
    );

    handle.stop();
}

/// Requests on one connection are served in sequence without closing it. The shared global's
/// counter proves both hit the same server; the second response proves the connection survived
/// the first, which therefore must not have promised `Connection: close`.
#[test]
fn serve_reuses_a_connection_for_sequential_requests() {
    let handler = r#"globalThis.n = 0;
        addEventListener('fetch', (event) => {
            globalThis.n += 1;
            const url = new URL(event.request.url);
            event.respondWith(new Response(url.pathname + ' #' + globalThis.n));
        })"#;
    let handle = start_serve(handler, 18301);

    let mut stream = TcpStream::connect(("127.0.0.1", 18301)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(b"GET /one HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let first = read_one_response(&mut stream, &mut carry, PATIENCE).expect("a first response");
    assert!(first.contains("/one #1"), "got: {first}");
    assert!(
        !first.to_lowercase().contains("connection: close"),
        "a reusable response must not promise a close; got: {first}"
    );

    stream
        .write_all(b"GET /two HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let second = read_one_response(&mut stream, &mut carry, PATIENCE)
        .expect("a second response on the same connection");
    assert!(second.contains("/two #2"), "got: {second}");

    handle.stop();
}

/// A request that asks for `Connection: close` gets it: the response repeats the header, and EOF
/// follows.
#[test]
fn serve_honors_a_requests_connection_close() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response('bye'));
    })"#;
    let handle = start_serve(handler, 18303);

    let response = raw_request(
        18303,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.to_lowercase().contains("connection: close"),
        "got: {response}"
    );
    assert!(response.ends_with("bye"), "got: {response}");

    handle.stop();
}

/// A body the handler never reads is consumed off the wire before the next request's head is
/// parsed, for both framings — otherwise its bytes would be read as that head.
#[test]
fn serve_drains_an_unread_body_and_reuses_the_connection() {
    let handler = r#"globalThis.n = 0;
        addEventListener('fetch', (event) => {
            globalThis.n += 1;
            event.respondWith(new Response('ok #' + globalThis.n));
        })"#;
    let handle = start_serve(handler, 18305);

    let mut stream = TcpStream::connect(("127.0.0.1", 18305)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 11\r\n\r\nhello world")
        .unwrap();
    let first = read_one_response(&mut stream, &mut carry, PATIENCE).expect("a first response");
    assert!(first.contains("ok #1"), "got: {first}");

    stream
        .write_all(b"GET /b HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let second = read_one_response(&mut stream, &mut carry, PATIENCE)
        .expect("a response after the unread sized body was drained");
    assert!(second.contains("ok #2"), "got: {second}");

    stream
        .write_all(
            b"POST /c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
              6\r\nchunky\r\n0\r\n\r\n",
        )
        .unwrap();
    let third = read_one_response(&mut stream, &mut carry, PATIENCE).expect("a third response");
    assert!(third.contains("ok #3"), "got: {third}");

    stream
        .write_all(b"GET /d HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let fourth = read_one_response(&mut stream, &mut carry, PATIENCE)
        .expect("a response after the unread chunked body was drained");
    assert!(fourth.contains("ok #4"), "got: {fourth}");

    handle.stop();
}

/// Draining an unread body for reuse is only worth so much: past the drain budget the server
/// closes the connection instead of reading megabytes no reader asked for. The close surfaces to
/// client as a failed body write or as EOF where the next response would have been — never as a
/// served next request.
#[test]
fn serve_closes_rather_than_drain_an_oversized_unread_body() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response('ignored your body'));
    })"#;
    // A drain budget far below the upload, and a short read timeout so the wait for a handler
    // that never reads gives up promptly. The upload has to outgrow all passive buffering (the
    // incoming-body channel, the body pump's held frame, and hyper's read-ahead, together over
    // 1 MiB), so that bytes remain for the drain to give up on.
    const UPLOAD: usize = 4 * 1024 * 1024;
    let handle = serve_with_limits(
        handler,
        18307,
        "--max-body-drain-bytes 64KiB --request-read-timeout 1",
    );

    let mut stream = TcpStream::connect(("127.0.0.1", 18307)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(
            format!("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: {UPLOAD}\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let first = read_one_response(&mut stream, &mut carry, PATIENCE).expect("the response");
    assert!(first.contains("ignored your body"), "got: {first}");

    // Feed the declared body. The server gives up draining partway and closes, so these writes
    // may start failing at any point; that failure is itself the expected outcome.
    let chunk = [b'a'; 32 * 1024];
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut wrote_all = true;
    for _ in 0..(UPLOAD / chunk.len()) {
        if stream.write_all(&chunk).is_err() {
            wrote_all = false;
            break;
        }
    }
    if wrote_all {
        let _ = stream.write_all(b"GET /after HTTP/1.1\r\nHost: x\r\n\r\n");
    }
    assert!(
        read_one_response(&mut stream, &mut carry, Duration::from_secs(5)).is_none(),
        "the connection must close rather than serve another request past an oversized drain"
    );

    handle.stop();
}

/// Chunked framing is HTTP/1.1: a 1.0 client gets a streamed body as raw bytes delimited by the
/// close. HTTP/1.0 does not persist by default, so the close needs no announcing.
#[test]
fn serve_answers_http_1_0_with_a_close_delimited_body() {
    let handler = r#"addEventListener('fetch', (event) => {
        const stream = new ReadableStream({
            start(controller) {
                controller.enqueue(new TextEncoder().encode('raw body'));
                controller.close();
            },
        });
        event.respondWith(new Response(stream));
    })"#;
    let handle = start_serve(handler, 18309);

    let response = raw_request(18309, b"GET / HTTP/1.0\r\nHost: x\r\n\r\n");
    let lower = response.to_lowercase();
    assert!(!lower.contains("connection: keep-alive"), "got: {response}");
    assert!(!lower.contains("transfer-encoding"), "got: {response}");
    assert!(response.ends_with("raw body"), "got: {response}");

    handle.stop();
}

/// Two requests written back to back before the first response: the second's bytes sit buffered
/// behind the first's head and must be served next, not discarded with the connection.
#[test]
fn serve_answers_pipelined_requests_in_order() {
    let handler = r#"globalThis.n = 0;
        addEventListener('fetch', (event) => {
            globalThis.n += 1;
            const url = new URL(event.request.url);
            event.respondWith(new Response(url.pathname + ' #' + globalThis.n));
        })"#;
    let handle = start_serve(handler, 18311);

    let mut stream = TcpStream::connect(("127.0.0.1", 18311)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let first = read_one_response(&mut stream, &mut carry, PATIENCE).expect("a first response");
    assert!(first.contains("/a #1"), "got: {first}");
    let second = read_one_response(&mut stream, &mut carry, PATIENCE)
        .expect("the pipelined second response");
    assert!(second.contains("/b #2"), "got: {second}");

    handle.stop();
}

/// waitUntil work belongs to the request that registered it, not to the connection: the next
/// request on the same connection is served while that work is still pending.
#[test]
fn serve_does_not_hold_the_next_request_behind_waituntil() {
    let handler = r#"addEventListener('fetch', (event) => {
        const url = new URL(event.request.url);
        if (url.pathname === '/lingering') {
            event.waitUntil(new Promise((resolve) => setTimeout(resolve, 3000)));
        }
        event.respondWith(new Response(url.pathname));
    })"#;
    let handle = start_serve(handler, 18313);

    let mut stream = TcpStream::connect(("127.0.0.1", 18313)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(b"GET /lingering HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    read_one_response(&mut stream, &mut carry, PATIENCE).expect("the lingering response");

    let asked = std::time::Instant::now();
    stream
        .write_all(b"GET /prompt HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let second = read_one_response(&mut stream, &mut carry, PATIENCE)
        .expect("a second response while waitUntil work is still pending");
    assert!(second.contains("/prompt"), "got: {second}");
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "the next request must not wait out the previous request's waitUntil work"
    );

    handle.stop();
}

/// A network-error 500 (here: a handler that never calls respondWith) leaves the connection's
/// framing intact, so it stays usable.
#[test]
fn serve_reuses_the_connection_after_a_network_error_response() {
    let handler = r#"addEventListener('fetch', () => {})"#;
    let handle = start_serve(handler, 18315);

    let mut stream = TcpStream::connect(("127.0.0.1", 18315)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let first = read_one_response(&mut stream, &mut carry, PATIENCE).expect("a first response");
    assert!(first.starts_with("HTTP/1.1 500"), "got: {first}");

    stream
        .write_all(b"GET /b HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let second = read_one_response(&mut stream, &mut carry, PATIENCE)
        .expect("a second response on the same connection after a 500");
    assert!(second.starts_with("HTTP/1.1 500"), "got: {second}");

    handle.stop();
}

/// Start a server whose limit flags come from `flags`, parsed as the CLI parses them, so these
/// tests exercise the real flag surface rather than a struct literal beside it.
fn serve_with_limits(handler: &str, port: u16, flags: &str) -> common::ServeHandle {
    let mut config = RuntimeConfig::from_arg_string(flags).expect("the limit flags parse");
    config.eval_script = Some(handler.to_string());
    config.legacy_script = true;
    config.serve = Some(port);
    start_serve_config(config, port)
}

/// The handler these limit tests use. None of them is about what it returns.
const OK_HANDLER: &str = r#"addEventListener('fetch', (e) => e.respondWith(new Response('ok')))"#;

#[test]
fn a_configured_head_limit_bounds_the_request_head() {
    let handle = serve_with_limits(OK_HANDLER, 18317, "--max-connection-buffer-size 8KiB");

    // Well under the default 64 KiB, so only the configured limit can reject this.
    let mut over = Vec::from(&b"GET / HTTP/1.1\r\nHost: x\r\nX-Big: "[..]);
    over.extend(std::iter::repeat_n(b'a', 32 * 1024));
    over.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    let response = raw_request(18317, &over);
    assert!(response.starts_with("HTTP/1.1 431"), "got: {response}");

    // A head within the limit is still served.
    assert_eq!(request(18317, "GET", "/", ""), "ok");

    handle.stop();
}

#[test]
fn a_configured_header_count_bounds_the_number_of_fields() {
    let handle = serve_with_limits(OK_HANDLER, 18319, "--max-request-headers 8");

    let mut over = Vec::from(&b"GET / HTTP/1.1\r\nHost: x\r\n"[..]);
    for n in 0..20 {
        over.extend_from_slice(format!("X-Field-{n}: v\r\n").as_bytes());
    }
    over.extend_from_slice(b"Connection: close\r\n\r\n");
    let response = raw_request(18319, &over);
    assert!(response.starts_with("HTTP/1.1 431"), "got: {response}");

    assert_eq!(request(18319, "GET", "/", ""), "ok");

    handle.stop();
}

#[test]
fn a_configured_body_limit_refuses_a_larger_declared_upload() {
    let handle = serve_with_limits(OK_HANDLER, 18321, "--max-request-body-bytes 64");

    let response = raw_request(
        18321,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413"), "got: {response}");

    // An upload within the limit is still served.
    assert_eq!(request(18321, "POST", "/", "small"), "ok");

    handle.stop();
}

/// The drain budget sets whether a connection whose request body went unread can be reused: the
/// same upload against the same handler is reused under a budget that covers it and closed under
/// one that does not.
///
/// The upload has to outgrow all passive buffering, or the whole body can be absorbed without
/// any of it reaching the drain, leaving the connection reusable under any budget: the
/// incoming-body channel buffers 9 chunks of up to 32 KiB, and the body pump and hyper's
/// read-ahead hold one chunk each of up to `--max-connection-buffer-size` (~408 KiB by default).
#[test]
fn the_drain_budget_sets_whether_an_unread_body_closes_the_connection() {
    // A short read timeout so the wait for the consumer that never reads gives up promptly.
    let generous = serve_with_limits(
        OK_HANDLER,
        18329,
        "--max-body-drain-bytes 8MiB --request-read-timeout 1",
    );
    assert!(
        reuses_after_unread_upload(18329),
        "a budget that covers the unread body must leave the connection usable"
    );
    generous.stop();

    let none = serve_with_limits(
        OK_HANDLER,
        18331,
        "--max-body-drain-bytes 0 --request-read-timeout 1",
    );
    assert!(
        !reuses_after_unread_upload(18331),
        "a zero budget must close rather than read a body nobody wanted"
    );
    none.stop();
}

/// Upload four megabytes the handler never reads, then report whether the connection went on to
/// serve another request.
fn reuses_after_unread_upload(port: u16) -> bool {
    const UPLOAD: usize = 4 * 1024 * 1024;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(
            format!("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: {UPLOAD}\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    // The server may stop reading and close partway, which fails these writes — that is the
    // outcome under test, not an error.
    let chunk = [b'a'; 32 * 1024];
    for _ in 0..(UPLOAD / chunk.len()) {
        if stream.write_all(&chunk).is_err() {
            return false;
        }
    }
    if read_one_response(&mut stream, &mut carry, PATIENCE).is_none() {
        return false;
    }
    if stream
        .write_all(b"GET /again HTTP/1.1\r\nHost: x\r\n\r\n")
        .is_err()
    {
        return false;
    }
    read_one_response(&mut stream, &mut carry, Duration::from_secs(10)).is_some()
}

#[test]
fn a_configured_keepalive_timeout_closes_an_idle_connection() {
    let handle = serve_with_limits(OK_HANDLER, 18333, "--keepalive-timeout 1");

    let mut stream = TcpStream::connect(("127.0.0.1", 18333)).unwrap();
    let mut carry = Vec::new();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    read_one_response(&mut stream, &mut carry, PATIENCE).expect("the first response");

    // Then say nothing. The connection must close on its own, and quietly: an idle client is
    // done with it, not late with a request.
    let closed = read_until_eof(&mut stream, Duration::from_secs(10)).expect("an idle close");
    assert!(
        closed.is_empty(),
        "an idle close sends nothing, not a status; got: {}",
        String::from_utf8_lossy(&closed)
    );

    handle.stop();
}

#[test]
fn a_configured_read_timeout_closes_a_stalled_head() {
    let handle = serve_with_limits(OK_HANDLER, 18335, "--request-read-timeout 1");

    let mut stream = TcpStream::connect(("127.0.0.1", 18335)).unwrap();
    // A head that never terminates: the client has started a request and then stalled, so the
    // connection is closed rather than held open for a request that may never arrive.
    stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").unwrap();
    let closed = read_until_eof(&mut stream, Duration::from_secs(10))
        .expect("the server to give up on the stalled head");
    assert!(
        closed.is_empty(),
        "a stalled head is closed on, not answered; got: {}",
        String::from_utf8_lossy(&closed)
    );

    handle.stop();
}

/// The connection cap bounds how many are served at once, not how many are accepted overall: a
/// second connection waits in the backlog and is served once the first is done.
#[test]
fn a_configured_connection_limit_serves_one_connection_at_a_time() {
    let handler = r#"globalThis.log = [];
        addEventListener('fetch', (event) => {
            const path = new URL(event.request.url).pathname;
            if (path === '/slow') {
                event.respondWith(new Promise((resolve) => setTimeout(() => {
                    log.push('slow-done');
                    resolve(new Response('slow'));
                }, 1000)));
                return;
            }
            event.respondWith(new Response(log.join(',') || 'nothing'));
        })"#;
    let handle = serve_with_limits(handler, 18337, "--max-connections 1");

    let mut slow = TcpStream::connect(("127.0.0.1", 18337)).unwrap();
    slow.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    // Let the slow request take the only slot before the second connection asks for one.
    std::thread::sleep(Duration::from_millis(300));

    let mut waiting = TcpStream::connect(("127.0.0.1", 18337)).unwrap();
    waiting
        .write_all(b"GET /observed HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();

    let slow_response = read_until_eof(&mut slow, PATIENCE).expect("the slow response");
    assert!(
        String::from_utf8_lossy(&slow_response).ends_with("slow"),
        "got: {}",
        String::from_utf8_lossy(&slow_response)
    );
    let observed = read_until_eof(&mut waiting, PATIENCE).expect("the waiting response");
    let observed = String::from_utf8_lossy(&observed);
    assert!(
        observed.ends_with("slow-done"),
        "the second connection must not be served until the only slot frees up; got: {observed}"
    );

    handle.stop();
}

/// The slot frees when the connection closes, not when its requests' work ends: `waitUntil` work
/// keeps running on the serve loop after the socket is gone, and with a single slot a new
/// connection is served while that work is still pending.
#[test]
fn wait_until_work_does_not_hold_a_connection_slot() {
    let handler = r#"addEventListener('fetch', (event) => {
        event.respondWith(new Response('ok'));
        event.waitUntil(new Promise((resolve) => setTimeout(resolve, 5000)));
    })"#;
    let handle = serve_with_limits(handler, 18345, "--max-connections 1");

    let mut first = TcpStream::connect(("127.0.0.1", 18345)).unwrap();
    let mut carry = Vec::new();
    first
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    read_one_response(&mut first, &mut carry, PATIENCE).expect("the first response");
    drop(first);

    let started = std::time::Instant::now();
    assert_eq!(request(18345, "GET", "/", ""), "ok");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "a new connection must not wait for the closed connection's waitUntil work (took {elapsed:?})"
    );

    handle.stop();
}

/// With `--node-compat`, a served request can't block on host I/O, such as a `require` with a
/// computed specifier that reads a file, or `fs.readFileSync`, unless `--allow-blocking-host-io`
/// allows it. A `require` with a literal specifier loads the source read while the top level ran.
/// The content script's top level can block, and `fs.promises` doesn't block natively.
#[test]
fn served_requests_cannot_block_on_host_io_by_default() {
    let dir = std::env::temp_dir().join(format!("starling-serve-blocking-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("early.cjs"), "module.exports = 'early';").unwrap();
    std::fs::write(dir.join("late.cjs"), "module.exports = 'late';").unwrap();
    std::fs::write(dir.join("computed.cjs"), "module.exports = 'computed';").unwrap();
    std::fs::write(
        dir.join("entry.cjs"),
        r#"const early = require('./early.cjs');
        const fs = require('fs');
        const attempt = (load) => {
            try {
                return load();
            } catch (error) {
                return error.code;
            }
        };
        addEventListener('fetch', (event) => {
            const late = attempt(() => require('./late.cjs'));
            const computed = attempt(() => require('./' + 'computed.cjs'));
            const sync = attempt(() => fs.readFileSync(__dirname + '/late.cjs', 'utf8').slice(17, 23));
            event.respondWith(
                fs.promises.readFile(__dirname + '/late.cjs', 'utf8').then(
                    (text) => new Response([early, late, computed, sync, text.slice(17, 23)].join(' ')),
                ),
            );
        });
        "#,
    )
    .unwrap();

    for (port, allow, expected) in [
        (
            18476,
            false,
            "early late ERR_BLOCKING_NOT_ALLOWED ERR_BLOCKING_NOT_ALLOWED 'late'",
        ),
        (18477, true, "early late computed 'late' 'late'"),
    ] {
        let handle = start_serve_config(
            RuntimeConfig {
                script_path: dir.join("entry.cjs").to_string_lossy().into_owned(),
                serve: Some(port),
                node_compat: true,
                allow_blocking_host_io: allow,
                ..Default::default()
            },
            port,
        );
        assert_eq!(request(port, "GET", "/", ""), expected, "allow: {allow}");
        handle.stop();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `--node-compat`, a `require` with a string literal that first runs while serving a request
/// loads the module and JSON file it names, which were read while the top level ran, and a
/// `require` of a module nothing names that way is refused.
#[test]
fn node_requires_named_by_literals_load_while_serving() {
    let dir = std::env::temp_dir().join(format!("starling-serve-node-lazy-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(
        dir.join("lib/lazy.js"),
        "module.exports = () => require('./data.json').value + require('./nested');",
    )
    .unwrap();
    std::fs::write(dir.join("lib/data.json"), r#"{ "value": "json" }"#).unwrap();
    std::fs::write(dir.join("lib/nested.js"), "module.exports = '+nested';").unwrap();
    std::fs::write(dir.join("lib/unnamed.js"), "module.exports = 'unnamed';").unwrap();
    std::fs::write(
        dir.join("server.cjs"),
        r#"const http = require('http');
        http.createServer((req, res) => {
            if (req.url === '/lazy') {
                res.end(require('./lib/lazy')());
                return;
            }
            const name = './lib/' + 'unnamed';
            try {
                res.end(require(name));
            } catch (error) {
                res.end(error.code);
            }
        }).listen(0);
        "#,
    )
    .unwrap();
    let port = 18481;
    let handle = start_serve_config(
        RuntimeConfig {
            script_path: dir.join("server.cjs").to_string_lossy().into_owned(),
            serve: Some(port),
            node_compat: true,
            ..Default::default()
        },
        port,
    );

    let lazy = raw_request(
        port,
        b"GET /lazy HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(lazy.starts_with("HTTP/1.1 200"), "{lazy}");
    assert!(lazy.ends_with("json+nested"), "{lazy}");

    let unnamed = raw_request(
        port,
        b"GET /unnamed HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(unnamed.ends_with("ERR_BLOCKING_NOT_ALLOWED"), "{unnamed}");

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `--node-compat`, a served request with an `Upgrade` header gets a 501 when the server has
/// `upgrade` listeners, which have no socket to take over, and goes to the `request` listener
/// otherwise.
#[test]
fn node_upgrade_requests_get_501_with_upgrade_listeners() {
    let dir = std::env::temp_dir().join(format!(
        "starling-serve-node-upgrade-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.cjs"),
        r#"const http = require('http');
        const server = http.createServer((req, res) => res.end('request'));
        server.on('upgrade', () => { throw new Error('unreachable'); });
        server.listen(0);
        "#,
    )
    .unwrap();
    let port = 18482;
    let handle = start_serve_config(
        RuntimeConfig {
            script_path: dir.join("server.cjs").to_string_lossy().into_owned(),
            serve: Some(port),
            node_compat: true,
            ..Default::default()
        },
        port,
    );

    let upgrade = raw_request(
        port,
        b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nConnection: close\r\n\r\n",
    );
    assert!(upgrade.starts_with("HTTP/1.1 501"), "{upgrade}");
    let plain = raw_request(
        port,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(plain.ends_with("request"), "{plain}");

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `--node-compat`, an `http.createServer` server responds to the served requests: the
/// request reads as an `IncomingMessage` with Node's header joining and the client's address on
/// its socket, and the `ServerResponse` sends its status, headers, and the body written whole or in
/// pieces.
#[test]
fn node_http_servers_respond_to_served_requests() {
    let dir = std::env::temp_dir().join(format!("starling-serve-node-http-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.cjs"),
        r#"const http = require('http');
        const server = http.createServer((req, res) => {
            if (req.url.startsWith('/echo')) {
                let body = '';
                req.setEncoding('utf8');
                req.on('data', (chunk) => (body += chunk));
                req.on('end', () => {
                    res.setHeader('Content-Type', 'application/json');
                    res.end(JSON.stringify({
                        method: req.method,
                        url: req.url,
                        cookie: req.headers.cookie,
                        dup: req.headers['x-dup'],
                        body,
                        complete: req.complete,
                        remote: [req.socket.remoteAddress, req.socket.remoteFamily, typeof req.socket.remotePort],
                    }));
                });
            } else if (req.url === '/stream') {
                res.writeHead(201, { 'X-A': '1' });
                res.write('a');
                setTimeout(() => {
                    res.write('b');
                    res.end('c');
                }, 10);
            } else {
                res.statusCode = 404;
                res.setHeader('Set-Cookie', ['x=1', 'y=2']);
                res.end();
            }
        });
        server.listen(0);
        "#,
    )
    .unwrap();
    let port = 18480;
    let handle = start_serve_config(
        RuntimeConfig {
            script_path: dir.join("server.cjs").to_string_lossy().into_owned(),
            serve: Some(port),
            node_compat: true,
            ..Default::default()
        },
        port,
    );

    let echo = raw_request(
        port,
        b"POST /echo?q=1 HTTP/1.1\r\nHost: x\r\nCookie: a=1\r\nCookie: b=2\r\nX-Dup: 1\r\nX-Dup: 2\r\n\
          Content-Length: 2\r\nConnection: close\r\n\r\nhi",
    );
    assert!(echo.starts_with("HTTP/1.1 200"), "{echo}");
    assert!(
        echo.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{echo}"
    );
    assert!(
        echo.ends_with(r#"{"method":"POST","url":"/echo?q=1","cookie":"a=1; b=2","dup":"1, 2","body":"hi","complete":true,"remote":["127.0.0.1","IPv4","number"]}"#),
        "{echo}"
    );

    let stream = raw_request(
        port,
        b"GET /stream HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(stream.starts_with("HTTP/1.1 201"), "{stream}");
    assert!(stream.to_ascii_lowercase().contains("x-a: 1"), "{stream}");
    let body = &stream[stream.find("\r\n\r\n").unwrap() + 4..];
    assert_eq!(dechunk(body), "abc", "{stream}");

    let missing = raw_request(
        port,
        b"GET /missing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    assert_eq!(
        missing.to_ascii_lowercase().matches("set-cookie:").count(),
        2,
        "{missing}"
    );

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
