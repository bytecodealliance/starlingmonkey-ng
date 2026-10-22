// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end native `fetch()` test against an in-process loopback HTTP server.
//!
//! Exercises the whole networking path: the `fetch` global builds a `Request`,
//! the platform transport (reqwest) sends it over a real TCP socket, the
//! async-promise event-loop driver settles the promise, and the `Response` is
//! consumed with `text()`.

#![cfg(not(target_arch = "wasm32"))]

use core_runtime::event_loop::{run_to_completion, with_event_loop, EventLoop};
use core_runtime::runtime::{clear_global_initializers, Runtime};
use js::conversion::FromJSVal;
use js::gc::scope::Scope;
use std::io::{Read, Write};
use std::net::TcpListener;

/// Start a server on `127.0.0.1` that accepts one connection per entry of `replies`, in order,
/// replies with that entry's bytes, and return its URL.
fn start_server(replies: Vec<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for reply in replies {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&reply);
                let _ = stream.flush();
            }
        }
    });
    format!("http://127.0.0.1:{port}/")
}

/// A `200` reply with `body`.
fn ok(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A `302` reply to `location`.
fn found(location: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// Start a server that streams chunked data with a small delay between chunks,
/// until the client disconnects, so a test can read some then abort mid-stream.
fn start_streaming_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let headers =
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n";
            if stream.write_all(headers.as_bytes()).is_err() {
                return;
            }
            // Stream 16-byte chunks until the client disconnects (write fails after abort).
            loop {
                if stream.write_all(b"10\r\nxxxxxxxxxxxxxxxx\r\n").is_err()
                    || stream.flush().is_err()
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    });
    format!("http://127.0.0.1:{port}/")
}

/// Start a one-shot server that streams `count` distinct, sequence-numbered
/// chunks (`[000][001]…`) via `Transfer-Encoding: chunked` with a small delay
/// between them, then closes. The delay lets a pipe pull the source before a
/// second `fetch` consumes it, forcing the request-body *pump* path (not the
/// incoming→outgoing shortcut); the sequence numbers make a dropped chunk
/// visible as a gap. Returns the URL and the expected concatenated body.
fn start_multichunk_server(count: usize) -> (String, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let chunks: Vec<String> = (0..count).map(|i| format!("[{i:03}]")).collect();
    let expected = chunks.concat();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let headers =
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n";
            if stream.write_all(headers.as_bytes()).is_err() {
                return;
            }
            for chunk in &chunks {
                let framed = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
                if stream.write_all(framed.as_bytes()).is_err() || stream.flush().is_err() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
            let _ = stream.write_all(b"0\r\n\r\n");
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}/"), expected)
}

/// Decode an HTTP/1.1 `Transfer-Encoding: chunked` body.
fn decode_chunked(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&rest[..pos])
            .ok()
            .and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        let data = pos + 2;
        if data + size > rest.len() {
            break;
        }
        out.extend_from_slice(&rest[data..data + size]);
        rest = &rest[data + size + 2..];
    }
    out
}

/// Start a server that reads the request body (decoding `chunked`) and echoes it
/// back as the response body. Used to verify a streamed request body arrives.
fn start_echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Note: long timeout because GC_ZEAL can make things real slow.
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        // The chunked terminator marks the end of the request.
                        if buf.windows(5).any(|w| w == b"0\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
                Some(p) => decode_chunked(&buf[p + 4..]),
                None => Vec::new(),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(&body),
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}/")
}

/// Extract the `Authorization` header value out of a raw HTTP request, or "none".
fn authorization_of(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_else(|| "none".to_string())
}

/// Start a server that echoes the request's `Authorization` header as `auth:<value-or-none>`, and
/// return its URL. With `redirect_first`, it first redirects to `/target` on the same origin.
fn start_auth_echo_server(redirect_first: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if redirect_first {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&found("/target"));
            }
        }
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let _ = stream.write_all(&ok(&format!("auth:{}", authorization_of(&buf[..n]))));
        }
    });
    format!("http://127.0.0.1:{port}/")
}

/// Evaluate `code`, drive the event loop to completion, and return the result of `inspect`, which
/// runs while the runtime is still alive.
fn run<T>(code: &str, inspect: impl FnOnce(&Scope<'_>) -> T) -> T {
    clear_global_initializers();
    libstarling::register_builtins();
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let rawcx = unsafe { scope.cx_mut().raw_cx() };
    let el = EventLoop::new();

    with_event_loop(&el, |_| {
        js::compile::evaluate_with_filename(&scope, code, "<test>", 1).expect("eval failed");
    });
    // Drain microtasks queued during top-level evaluation (e.g. a synchronously settled promise's
    // reactions), as the runtime does before stepping the event loop.
    js::jobs::run_jobs(&scope);

    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio_rt.block_on(async { unsafe { run_to_completion(rawcx, &el, tokio::time::sleep).await } });
    inspect(&scope)
}

/// Evaluate `code` (which sets `globalThis.__out`), drive the event loop to completion, and
/// return `__out`.
fn run_and_get_out(code: &str) -> String {
    run(code, read_out)
}

/// Returns `String(globalThis.__out)`.
fn read_out(scope: &Scope<'_>) -> String {
    let out = js::compile::evaluate_with_filename(scope, "globalThis.__out", "<check>", 1).unwrap();
    String::from_jsval(scope, out, ()).unwrap()
}

#[test]
fn fetch_streams_a_readable_stream_request_body() {
    // A ReadableStream request body is streamed to the server (chunked), which echoes it back.
    let url = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        const encoder = new TextEncoder();
        const body = new ReadableStream({{
            start(controller) {{
                controller.enqueue(encoder.encode("hello "));
                controller.enqueue(encoder.encode("streamed "));
                controller.enqueue(encoder.encode("body"));
                controller.close();
            }}
        }});
        fetch("{url}", {{ method: "POST", body, duplex: "half" }})
            .then(r => r.text())
            .then(text => {{ globalThis.__out = text; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "hello streamed body");
}

#[test]
fn fetch_pipes_a_response_body_through_an_identity_transform() {
    // The complex incoming→outgoing shortcut: a response body piped through an identity
    // TransformStream into another request is still handed straight through (the native source is
    // propagated across the identity transform), so the second server echoes it unchanged.
    let source = start_server(vec![ok("piped through identity transform")]);
    let echo = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{source}")
            .then(response => fetch("{echo}", {{
                method: "POST",
                body: response.body.pipeThrough(new TransformStream()),
                duplex: "half",
            }}))
            .then(r => r.text())
            .then(text => {{ globalThis.__out = text; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "piped through identity transform");
}

#[test]
fn a_pipe_whose_body_is_taken_before_it_reads_still_finishes() {
    // `fetch()` takes the host body before the pipe's first read, so the pipe's read finds the
    // body gone and must close the stream rather than wait for demand that never comes.
    let source = start_server(vec![ok("taken before the pipe reads")]);
    let echo = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{source}")
            .then(async (response) => {{
                const transform = new TransformStream();
                const piped = response.body.pipeTo(transform.writable);
                const echoed = await fetch("{echo}", {{
                    method: "POST",
                    body: transform.readable,
                    duplex: "half",
                }});
                const text = await echoed.text();
                await piped;
                globalThis.__out = text;
            }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "taken before the pipe reads");
}

#[test]
fn fetch_pumps_a_piped_multichunk_body_without_dropping_chunks() {
    // The request-body *pump* path (as opposed to the incoming→outgoing shortcut): a multi-chunk
    // host response body is piped through a TransformStream carrying an explicit pass-through
    // `transform`, which disqualifies it as an identity transform. The native source is therefore
    // not propagated across it, so the second `fetch` cannot shortcut and must pump the body
    // chunk by chunk. Every sequence-numbered chunk must arrive at the echo server in order.
    let (source, expected) = start_multichunk_server(40);
    let echo = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        (async () => {{
            try {{
                const response = await fetch("{source}");
                const piped = response.body.pipeThrough(new TransformStream({{
                    transform(chunk, controller) {{ controller.enqueue(chunk); }},
                }}));
                const echoed = await fetch("{echo}", {{ method: "POST", body: piped, duplex: "half" }});
                globalThis.__out = await echoed.text();
            }} catch (e) {{ globalThis.__out = "error:" + e; }}
        }})();
        "#
    ));
    assert_eq!(out, expected);
}

#[test]
fn a_delayed_piped_identity_transform_still_delivers_the_whole_body() {
    // A pipe left running well ahead of the `fetch` that consumes it must still deliver every
    // byte in order. The pipe's first pull parks rather than reading the host body.
    let (source, expected) = start_multichunk_server(40);
    let echo = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        (async () => {{
            try {{
                const response = await fetch("{source}");
                const piped = response.body.pipeThrough(new TransformStream());
                await new Promise(r => setTimeout(r, 25));
                const echoed = await fetch("{echo}", {{ method: "POST", body: piped, duplex: "half" }});
                globalThis.__out = await echoed.text();
            }} catch (e) {{ globalThis.__out = "error:" + e; }}
        }})();
        "#
    ));
    assert_eq!(out, expected);
}

#[test]
fn a_parked_body_read_resumes_when_the_transform_is_read() {
    // A pull parked behind an identity transform must be a delay, not a cancellation: with no
    // `fetch` to claim the body, reading the transform's readable end has to wake the parked pull
    // and stream the whole body through. If the resume were missed, this would produce an empty
    // body or hang rather than the 40 chunks.
    let (source, expected) = start_multichunk_server(40);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        (async () => {{
            try {{
                const response = await fetch("{source}");
                const piped = response.body.pipeThrough(new TransformStream());
                // Let the pipe's first pull park before anything reads `piped`.
                await new Promise(r => setTimeout(r, 25));
                globalThis.__out = await new Response(piped).text();
            }} catch (e) {{ globalThis.__out = "error:" + e; }}
        }})();
        "#
    ));
    assert_eq!(out, expected);
}

#[test]
fn fetch_applies_a_non_identity_transform_to_a_piped_body() {
    // A non-identity transform must NOT be shortcut: its transform runs, so the second server sees
    // the transformed (uppercased) body.
    let source = start_server(vec![ok("transform me")]);
    let echo = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        const upper = new TransformStream({{
            transform(chunk, controller) {{
                controller.enqueue(chunk.map(b => (b >= 97 && b <= 122) ? b - 32 : b));
            }}
        }});
        fetch("{source}")
            .then(response => fetch("{echo}", {{
                method: "POST",
                body: response.body.pipeThrough(upper),
                duplex: "half",
            }}))
            .then(r => r.text())
            .then(text => {{ globalThis.__out = text; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "TRANSFORM ME");
}

#[test]
fn fetch_pre_aborted_signal_rejects_with_abort_error() {
    // A signal already aborted before `fetch` rejects without connecting (step 4).
    let out = run_and_get_out(
        r#"
        globalThis.__out = "pending";
        const controller = new AbortController();
        controller.abort();
        fetch("http://127.0.0.1:1/", { signal: controller.signal })
            .then(() => { globalThis.__out = "resolved"; })
            .catch(e => { globalThis.__out = e.name; });
        "#,
    );
    assert_eq!(out, "AbortError");
}

#[test]
fn fetch_abort_during_body_read_rejects_and_closes() {
    // Aborting mid-stream errors the body stream (the pending read rejects with AbortError) and
    // cancels the host read (closing the connection, so the event loop completes).
    let url = start_streaming_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        (async () => {{
            const controller = new AbortController();
            const response = await fetch("{url}", {{ signal: controller.signal }});
            const reader = response.body.getReader();
            await reader.read();
            controller.abort();
            try {{
                await reader.read();
                globalThis.__out = "resolved";
            }} catch (e) {{
                globalThis.__out = e.name;
            }}
        }})().catch(e => {{ globalThis.__out = "outer:" + e.name; }});
        "#
    ));
    assert_eq!(out, "AbortError");
}

#[test]
fn aborting_a_parked_body_read_rejects_rather_than_hanging() {
    // Aborting while a pull sits parked behind an identity transform must still tear the body
    // down. The parked pull is settled without closing the stream, which is only safe because
    // `abort_body` errors the stream in the same turn: were it left readable, the next read would
    // park again on a transform nothing will ever drive, and never settle — surfacing here as the
    // untouched "pending" rather than a rejection.
    let url = start_streaming_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        (async () => {{
            const controller = new AbortController();
            const response = await fetch("{url}", {{ signal: controller.signal }});
            const piped = response.body.pipeThrough(new TransformStream());
            // Let the pipe's first pull park before aborting.
            await new Promise(r => setTimeout(r, 25));
            controller.abort();
            try {{
                await new Response(piped).text();
                globalThis.__out = "resolved";
            }} catch (e) {{
                globalThis.__out = e.name;
            }}
        }})().catch(e => {{ globalThis.__out = "outer:" + e.name; }});
        "#
    ));
    assert_eq!(out, "AbortError");
}

#[test]
fn transform_start_chunks_are_not_dropped_by_the_shortcut() {
    // A transformer with only a start callback is not an identity transform: its
    // enqueued prefix must reach the destination rather than the shortcut handing the host
    // body straight through.
    let source = start_server(vec![ok("rest-of-body")]);
    let echo = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        const ts = new TransformStream({{ start(c) {{ c.enqueue(new TextEncoder().encode("prefix ")); }} }});
        fetch("{source}")
            .then(r => {{
                r.body.pipeTo(ts.writable);
                return fetch("{echo}", {{ method: "POST", body: ts.readable, duplex: "half" }});
            }})
            .then(r => r.text())
            .then(text => {{ globalThis.__out = text; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "prefix rest-of-body");
}

// ── HTTP-redirect fetch: the security steps ──

#[test]
fn redirect_strips_authorization_cross_origin() {
    // HTTP-redirect fetch step 13: the moment another origin is seen, the Authorization header is
    // removed. Two loopback ports are two origins.
    let target = start_auth_echo_server(false);
    let origin = start_server(vec![found(&target)]);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{origin}", {{ headers: {{ Authorization: "Bearer s3cret" }} }})
            .then(r => r.text())
            .then(text => {{ globalThis.__out = text; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "auth:none");
}

#[test]
fn redirect_keeps_authorization_same_origin() {
    // A same-origin redirect keeps the Authorization header.
    let url = start_auth_echo_server(true);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{url}", {{ headers: {{ Authorization: "Bearer s3cret" }} }})
            .then(r => r.text())
            .then(text => {{ globalThis.__out = text; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "auth:Bearer s3cret");
}

#[test]
fn redirect_with_credentialed_location_is_a_network_error() {
    // HTTP-redirect fetch steps 9–10: a Location that includes credentials is a network error for
    // a cors-mode request (every fetch() here). The redirect target is a *reachable* server that
    // returns 200, so the oracle discriminates: with the check, the fetch rejects before following
    // (rejected:TypeError); without it, the fetch would follow and resolve (resolved:200).
    let upstream = start_server(vec![ok("creds-followed")]);
    let credentialed = upstream.replace("http://", "http://user:pass@");
    let origin = start_server(vec![found(&credentialed)]);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{origin}")
            .then(r => {{ globalThis.__out = "resolved:" + r.status; }})
            .catch(e => {{ globalThis.__out = "rejected:" + e.constructor.name; }});
        "#
    ));
    assert_eq!(out, "rejected:TypeError");
}

#[test]
fn a_transport_error_message_includes_its_causes() {
    // Nothing listens on port 1, so connecting to it is refused.
    let out = run_and_get_out(
        r#"
        globalThis.__out = "pending";
        fetch("http://127.0.0.1:1/")
            .then(r => { globalThis.__out = "resolved:" + r.status; })
            .catch(e => { globalThis.__out = e.constructor.name + ":" + e.message; });
        "#,
    );
    assert!(
        out.starts_with("TypeError:Failed to fetch: ") && out.contains("refused"),
        "{out}"
    );
}

#[test]
fn a_network_body_is_a_byte_stream() {
    // A body read into views of 4 bytes arrives in chunks larger than the view. A view of 64 bytes
    // holds the whole body.
    let url = start_server(vec![ok("seventeen bytes!!"), ok("seventeen bytes!!")]);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        async function readInto(viewLength) {{
            const response = await fetch("{url}");
            const reader = response.body.getReader({{ mode: "byob" }});
            const dec = new TextDecoder();
            const reads = [];
            for (;;) {{
                const {{ value, done }} = await reader.read(new Uint8Array(viewLength));
                if (done) break;
                reads.push(dec.decode(value));
            }}
            return reads.join("|");
        }}
        (async () => {{
            const small = await readInto(4);
            const large = await readInto(64);
            globalThis.__out = small + " / " + large;
        }})().catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "seve|ntee|n by|tes!|! / seventeen bytes!!");
}

#[test]
fn response_body_unaffected_by_patched_controller_enqueue() {
    // A network response body is delivered through the controller abstract
    // operation, not the author-visible controller prototypes' methods. Patching
    // their `enqueue` to drop chunks must not affect the body's bytes.
    let url = start_server(vec![ok("untampered body bytes")]);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        ReadableStreamDefaultController.prototype.enqueue = function () {{ /* drop */ }};
        ReadableByteStreamController.prototype.enqueue = function () {{ /* drop */ }};
        fetch("{url}")
            .then(async (response) => {{
                const reader = response.body.getReader();
                const dec = new TextDecoder();
                let text = "";
                for (;;) {{
                    const {{ value, done }} = await reader.read();
                    if (done) break;
                    text += dec.decode(value);
                }}
                globalThis.__out = text;
            }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "untampered body bytes");
}

#[test]
fn response_body_transmitted_via_shortcut_marks_it_used() {
    // Using a response body directly as another request's body hands it straight
    // to the transport (the incoming→outgoing shortcut). The donor stream must be
    // left locked and disturbed, as if the pump had read it: bodyUsed is true and
    // the stream is locked.
    let source = start_server(vec![ok("shortcut body bytes")]);
    let echo = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{source}")
            .then(async (resp) => {{
                const echoResp = await fetch("{echo}", {{
                    method: "POST", body: resp.body, duplex: "half",
                }});
                const echoed = await echoResp.text();
                globalThis.__out =
                    `echoed=${{echoed}}|used=${{resp.bodyUsed}}|locked=${{resp.body.locked}}`;
            }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "echoed=shortcut body bytes|used=true|locked=true");
}

#[test]
fn status_text_is_empty_because_the_reason_phrase_is_not_recoverable() {
    // Neither transport surfaces the reason-phrase the peer sent, and it cannot be guessed from
    // the status code — a server may send any phrase, as this one does. Reporting the registered
    // phrase for the code would invent a value the peer never sent, so `statusText` stays empty.
    // Locks in the behaviour `fetch/api/basic/status.h2.any.js` depends on.
    let url = start_server(vec![
        b"HTTP/1.1 404 Totally Fine\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".to_vec(),
    ]);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{url}")
            .then(r => {{ globalThis.__out = `${{r.status}}|${{r.statusText}}|${{r.ok}}`; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "404||false");
}

#[test]
fn cancelling_a_derived_request_body_cancels_the_original() {
    // `new Request(input, ...)` gives the derived request a proxy stream over the input's body
    // (the spec's identity-transform pipe). Cancelling the proxy must cancel the original, as a
    // pipe would — otherwise the input stays locked to the proxy's internal reader forever and
    // its source keeps producing bytes nobody reads.
    let out = run_and_get_out(
        r#"
        globalThis.__out = "pending";
        (async () => {
            let cancelledWith = "never";
            const source = new ReadableStream({
                pull(c) { c.enqueue(new Uint8Array([1])); },
                cancel(reason) { cancelledWith = String(reason); },
            });
            const original = new Request("https://example.com/", {
                method: "POST", body: source, duplex: "half",
            });
            const derived = new Request(original);
            await derived.body.cancel("done here");
            globalThis.__out = `cancelled=${cancelledWith}|derivedLocked=${derived.body.locked}`;
        })().catch(e => { globalThis.__out = "error:" + e; });
        "#,
    );
    assert_eq!(out, "cancelled=done here|derivedLocked=false");
}

#[test]
fn a_reused_abort_signal_does_not_accumulate_fetch_abort_algorithms() {
    // `fetch` registers an abort algorithm on the request's signal. An `AbortSignal` outlives the
    // fetches made with it, so an algorithm that is never removed accumulates one entry per fetch
    // — each rooting that fetch's promise and Response, and through the Response its host body and
    // connection. Reading each body must detach it again.
    let url = start_server(vec![ok("body"); 3]);
    let code = format!(
        r#"
        globalThis.__out = "pending";
        globalThis.__controller = new AbortController();
        (async () => {{
            for (let i = 0; i < 3; i++) {{
                const r = await fetch("{url}", {{ signal: globalThis.__controller.signal }});
                await r.text();
            }}
            globalThis.__out = "done";
        }})().catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    );
    let (out, count) = run(&code, |scope| {
        // The signal is still reachable from the global, so anything it holds is still alive.
        let signal_value = js::compile::evaluate_with_filename(
            scope,
            "globalThis.__controller.signal",
            "<sig>",
            1,
        )
        .unwrap();
        let signal = js::Object::from_raw_value(scope, signal_value.get())
            .unwrap()
            .cast::<web_globals::signals::AbortSignal>()
            .unwrap();
        let count = web_globals::signals::algorithms::abort_algorithm_count_including_dependents(
            scope, &signal,
        );
        (read_out(scope), count)
    });
    assert_eq!(out, "done");
    assert_eq!(
        count, 0,
        "3 fetches left {count} abort algorithms registered on the reused signal's dependents"
    );
}

#[test]
fn static_json_ignores_a_patched_json_stringify() {
    // `serialize a JavaScript value to JSON bytes` calls %JSON.stringify%, the intrinsic.
    // Author code that patches the global must not be able to rewrite — or even observe —
    // the body `Response.json()` builds.
    let out = run_and_get_out(
        r#"
        globalThis.__out = "pending";
        (async () => {
            let observed = false;
            JSON.stringify = () => { observed = true; return '"hijacked"'; };
            const text = await Response.json({ a: 1 }).text();
            let threw = "no";
            try { Response.json(Symbol("nope")); } catch (e) { threw = e.constructor.name; }
            globalThis.__out = `${text}|observed=${observed}|threw=${threw}`;
        })().catch(e => { globalThis.__out = "error:" + e; });
        "#,
    );
    assert_eq!(out, r#"{"a":1}|observed=false|threw=TypeError"#);
}

#[test]
fn static_json_still_serializes_null_and_rejects_undefined() {
    // A real null serializes to the string "null", while values for which
    // `JSON.stringify` returns undefined must be rejected. The two must not be conflated.
    let out = run_and_get_out(
        r#"
        globalThis.__out = "pending";
        (async () => {
            const nullBody = await Response.json(null).text();
            let undef = "no";
            try { Response.json(undefined); } catch (e) { undef = e.constructor.name; }
            let fn = "no";
            try { Response.json(function () {}); } catch (e) { fn = e.constructor.name; }
            // An object whose toJSON returns undefined is also not serializable.
            let viaToJson = "no";
            try { Response.json({ toJSON() { return undefined; } }); }
            catch (e) { viaToJson = e.constructor.name; }
            // toJSON must run exactly once.
            let calls = 0;
            const once = await Response.json({ toJSON() { calls++; return { ok: 1 }; } }).text();
            globalThis.__out = `${nullBody}|${undef}|${fn}|${viaToJson}|${once}|calls=${calls}`;
        })().catch(e => { globalThis.__out = "error:" + e; });
        "#,
    );
    assert_eq!(
        out,
        r#"null|TypeError|TypeError|TypeError|{"ok":1}|calls=1"#
    );
}

/// The `Content-Length` header of a request made by running `code`, or `None`.
fn content_length_of_fetch(code: &str) -> Option<String> {
    let (url, rx) = start_byte_capture_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    );
    let out = run_and_get_out(&code.replace("URL", &url));
    assert_eq!(out, "ok", "the fetch itself failed");
    let head = rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
    let head = String::from_utf8_lossy(&head);
    let found: Vec<String> = head
        .lines()
        .filter(|line| {
            line.split(':')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("content-length"))
        })
        .map(|line| line.split_once(':').unwrap().1.trim().to_string())
        .collect();
    assert!(
        found.len() <= 1,
        "duplicate Content-Length headers: {found:?}"
    );
    found.into_iter().next()
}

#[test]
fn content_length_follows_http_network_or_cache_fetch() {
    // `HTTP-network-or-cache fetch`: a null body with a POST/PUT method sends `Content-Length: 0`,
    // a body with a known length sends that length, and a body with no known length (a stream)
    // sends none — it is chunked instead.
    const FETCH: &str = r#"
        globalThis.__out = "pending";
        fetch("URL", INIT).then(r => r.text())
            .then(t => { globalThis.__out = t; })
            .catch(e => { globalThis.__out = "error:" + e; });
    "#;

    // A bodyless POST is not a bodyless GET: only POST/PUT get the explicit zero.
    assert_eq!(
        content_length_of_fetch(&FETCH.replace("INIT", r#"{ method: "POST" }"#)),
        Some("0".to_string()),
    );
    assert_eq!(
        content_length_of_fetch(&FETCH.replace("INIT", r#"{ method: "PUT" }"#)),
        Some("0".to_string()),
    );
    assert_eq!(content_length_of_fetch(&FETCH.replace("INIT", "{}")), None);

    // A body with a source has its length.
    assert_eq!(
        content_length_of_fetch(&FETCH.replace("INIT", r#"{ method: "POST", body: "hello" }"#)),
        Some("5".to_string()),
    );
    // An explicitly empty body is a body of length 0.
    assert_eq!(
        content_length_of_fetch(&FETCH.replace("INIT", r#"{ method: "POST", body: "" }"#)),
        Some("0".to_string()),
    );

    // A stream body has no known length, so it is sent chunked with no Content-Length.
    assert_eq!(
        content_length_of_fetch(&FETCH.replace(
            "INIT",
            r#"{
                method: "POST",
                duplex: "half",
                body: new ReadableStream({
                    start(c) { c.enqueue(new TextEncoder().encode("hi")); c.close(); },
                }),
            }"#,
        )),
        None,
    );
}

#[test]
fn headers_for_each_observes_mutations_made_by_its_callback() {
    // WebIDL's `forEach` re-reads the pair list each turn, so headers the callback adds are
    // visited and ones it deletes are skipped. Iterating a snapshot taken up front would visit
    // the deleted header and miss the added one.
    let out = run_and_get_out(
        r#"
        globalThis.__out = "pending";
        const h = new Headers({ a: "1", b: "2", c: "3" });
        const seen = [];
        h.forEach((value, name) => {
            seen.push(name);
            if (name === "a") {
                h.delete("b");     // never visited
                h.set("d", "4");   // visited, sorts after c
            }
        });
        let nonCallable = "no";
        try { new Headers().forEach({}); } catch (e) { nonCallable = e.constructor.name; }
        globalThis.__out = `${seen.join(",")}|${nonCallable}`;
        "#,
    );
    assert_eq!(out, "a,c,d|TypeError");
}

/// Start a one-shot server that captures the raw request bytes and replies with
/// `reply` verbatim, so a test can assert on the exact bytes in both directions.
fn start_byte_capture_server(reply: &'static [u8]) -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Note: long timeout because GC_ZEAL can make things real slow.
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            while let Ok(n) = stream.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(buf);
            let _ = stream.write_all(reply);
        }
    });
    (format!("http://127.0.0.1:{port}/"), rx)
}

#[test]
fn header_values_are_isomorphic_encoded_on_the_wire() {
    // A Fetch header value is a byte sequence, and the WebIDL `ByteString` it comes from maps each
    // code unit one-to-one onto a byte. So `"bæte"` (U+00E6) must go out as the single byte 0xE6 —
    // not as its UTF-8 encoding 0xC3 0xA6, which is what treating the value as text produces.
    let (url, rx) = start_byte_capture_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    );
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{url}", {{ headers: {{ "X-Test": "bæte" }} }})
            .then(r => r.text())
            .then(t => {{ globalThis.__out = t; }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "ok", "the fetch itself failed");

    let sent = rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
    let line = sent
        .split(|&b| b == b'\n')
        .find(|line| line.to_ascii_lowercase().starts_with(b"x-test"))
        .expect("the X-Test header was sent")
        .to_vec();
    assert!(
        line.windows(4).any(|w| w == [b'b', 0xE6, b't', b'e']),
        "expected the isomorphic-encoded byte 0xE6, got {line:02x?}",
    );
}

#[test]
fn header_values_are_isomorphic_decoded_from_the_wire() {
    // The mirror of the above: a response header byte of 0xE6 is the code unit U+00E6, so
    // `headers.get` must yield "bæte", not drop the header or decode it to U+FFFD.
    let (url, _rx) = start_byte_capture_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Echo: b\xe6te\r\nConnection: close\r\n\r\nok",
    );
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "pending";
        fetch("{url}")
            .then(r => {{
                const value = r.headers.get("x-echo");
                globalThis.__out = Array.from(value)
                    .map(c => c.codePointAt(0).toString(16)).join(",");
            }})
            .catch(e => {{ globalThis.__out = "error:" + e; }});
        "#
    ));
    assert_eq!(out, "62,e6,74,65");
}
