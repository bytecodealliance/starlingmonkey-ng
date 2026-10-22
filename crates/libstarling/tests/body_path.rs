// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Which route an outgoing body takes to the wire: handed over whole (the host shortcut) or pumped
//! through JS chunk by chunk.
//!
//! Both deliver the same bytes, so nothing a client can see distinguishes them — which is exactly
//! why losing the shortcut needs a test of its own. Without one, a change that started copying every
//! response body through JS would leave every other test green.
//!
//! These run in a test binary of their own because they read process-wide counters
//! (`web_fetch::outgoing_body::paths_taken`), which any other request in flight would perturb. The
//! counters belong to the process, and this file holds a single test, so the counts always belong
//! to the request that just ran.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{dechunk, request, start_chunked_upstream, start_echo_upstream, start_serve};
use web_fetch::outgoing_body::paths_taken;

/// Serve one request against `handler` and report `(response body, shortcuts, pumps)`.
fn serve_once(handler: &str, path: &str) -> (String, usize, usize) {
    let handle = start_serve(handler);
    let port = handle.port();
    paths_taken::reset();
    let body = request(port, "GET", path, "");
    let (shortcut, pumped) = paths_taken::counts();
    handle.stop();
    (body, shortcut, pumped)
}

#[test]
fn outgoing_bodies_take_the_route_they_should() {
    single_bodies_take_the_route_they_should();
    piping_many_bodies_into_one_transform_keeps_every_byte();
    a_queued_chunk_survives_a_pipe_that_is_still_running();
    a_pipe_that_keeps_the_transform_open_forfeits_the_shortcut();
    a_head_request_still_claims_a_proxied_body();
    a_head_request_cancels_the_body_it_will_not_send();
    a_byte_response_body_leaves_its_owner_holding_nothing();
    a_byte_request_body_leaves_no_copy_behind_on_the_request_it_came_from();
}

/// An in-memory response body is moved to the transport, not copied to it, so once it is on its way
/// the `Response` holds no reference to the buffer — which lives as long as the `Response` does.
///
/// Nothing on the wire distinguishes that from keeping one, so the counter is the only place it
/// shows.
fn a_byte_response_body_leaves_its_owner_holding_nothing() {
    let handler = "addEventListener('fetch', (e) => e.respondWith(new Response('payload')))";
    let handle = start_serve(handler);
    let port = handle.port();
    paths_taken::reset();

    let body = request(port, "GET", "/", "");
    let (sole, shared) = paths_taken::byte_counts();
    handle.stop();

    assert_eq!(body, "payload", "got: {body}");
    assert_eq!(
        (sole, shared),
        (1, 0),
        "the response must hand its bytes over rather than keep a reference alongside them"
    );
}

/// The same for a request body, where the reference to let go of is the one `new Request(input)`
/// leaves behind on the input: `fetch` builds its own `Request` from the one it is given, sharing
/// its bytes, and the input can no longer read them.
fn a_byte_request_body_leaves_no_copy_behind_on_the_request_it_came_from() {
    let upstream = start_echo_upstream();
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith(fetch(new Request('http://127.0.0.1:{upstream}/', {{
                method: 'POST',
                body: 'payload',
            }})));
        }})"#
    );
    let handle = start_serve(&handler);
    let port = handle.port();
    paths_taken::reset();

    let body = request(port, "GET", "/", "");
    let (sole, shared) = paths_taken::byte_counts();
    handle.stop();

    assert_eq!(
        dechunk(&body),
        "payload",
        "the body must still reach the upstream: {body}"
    );
    assert_eq!(
        (sole, shared),
        (1, 0),
        "the request the bytes were taken from must not go on holding them"
    );
}

/// A `HEAD` sends no body, and a stream it cannot hand over whole has to be let go of rather than
/// left sitting on the response: whatever the stream draws from — an upstream response feeding a
/// transform, a source of the handler's own — is held until it is, and cancelling is what releases
/// it. Cancelling and not reading, at that: `pull()` would produce content the response may not
/// carry.
///
/// Both shapes, since only the piped one has to travel the length of a pipe to reach its source.
fn a_head_request_cancels_the_body_it_will_not_send() {
    // Nothing may be pulled before the response is taken, so neither the source nor the transform
    // it feeds may want a chunk of its own accord: hence the zero high-water marks.
    let handler = r#"const seen = [];
    addEventListener('fetch', (event) => {
        const path = new URL(event.request.url).pathname;
        if (path === '/seen') {
            event.respondWith(new Response(seen.join(' ')));
            return;
        }
        const source = new ReadableStream({
            pull() { seen.push(path + ':pulled'); },
            cancel() { seen.push(path + ':canceled'); },
        }, { highWaterMark: 0 });
        event.respondWith(new Response(path === '/piped'
            ? source.pipeThrough(new TransformStream(undefined, { highWaterMark: 0 }))
            : source));
    })"#;
    let handle = start_serve(handler);
    let port = handle.port();
    paths_taken::reset();

    let direct = request(port, "HEAD", "/direct", "");
    let piped = request(port, "HEAD", "/piped", "");
    let (shortcut, pumped) = paths_taken::counts();
    let seen = request(port, "GET", "/seen", "");
    handle.stop();

    assert_eq!(
        (direct.as_str(), piped.as_str()),
        ("", ""),
        "a HEAD response carries no body"
    );
    assert_eq!(
        seen, "/direct:canceled /piped:canceled",
        "a HEAD must cancel the stream it will not send, and must not pull it"
    );
    assert_eq!(
        (shortcut, pumped),
        (0, 0),
        "a body that is cancelled unsent reaches the wire by neither route"
    );
}

/// A `HEAD` sends no body, but a proxied one must still be claimed off the response: dropping it
/// here is what closes the upstream. Left in place it lives on in the JS `Response` until the
/// collector gets to it, holding an upstream connection open for as long as that takes.
///
/// The counter is the only way to see it — the client is told nothing either way.
fn a_head_request_still_claims_a_proxied_body() {
    let upstream = start_chunked_upstream(&["one ", "two"]);
    let handler = format!(
        "addEventListener('fetch', (e) => e.respondWith(fetch('http://127.0.0.1:{upstream}/')))"
    );
    let handle = start_serve(&handler);
    let port = handle.port();
    paths_taken::reset();

    let response = request(port, "HEAD", "/", "");
    let (shortcut, pumped) = paths_taken::counts();
    handle.stop();

    assert_eq!(response, "", "a HEAD response carries no body: {response}");
    assert_eq!(
        (shortcut, pumped),
        (1, 0),
        "the proxied body must be taken and dropped, not left on the response"
    );

    // The same, with the host body materialized into a stream and carried across a transform. It
    // has still never been read, so it is still claimable whole — reaching the response as a stream
    // rather than as a body of its own must not cost it that.
    let upstream = start_chunked_upstream(&["one ", "two"]);
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                const upstream = await fetch('http://127.0.0.1:{upstream}/');
                return new Response(upstream.body.pipeThrough(new TransformStream()));
            }})());
        }})"#
    );
    let handle = start_serve(&handler);
    let port = handle.port();
    paths_taken::reset();

    let response = request(port, "HEAD", "/", "");
    let (shortcut, pumped) = paths_taken::counts();
    handle.stop();

    assert_eq!(response, "", "a HEAD response carries no body: {response}");
    assert_eq!(
        (shortcut, pumped),
        (1, 0),
        "a host body reached through a transform must be claimed and dropped too"
    );
}

/// A pipe that leaves the transform open (`preventClose`) must not let the body be claimed whole.
///
/// Claiming it hands the wire that one body and bypasses the transform, so whatever is written in
/// afterwards goes to a readable end that nothing reads: the second pipe stalls forever and its
/// bytes are dropped without a word. Worse, it made the choice of route observable: the same
/// handler delivered `AAA` when the body was claimed and `AAABBB` when it was pumped.
fn a_pipe_that_keeps_the_transform_open_forfeits_the_shortcut() {
    let first = start_chunked_upstream(&["AAA"]);
    let second = start_chunked_upstream(&["BBB"]);
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                const transform = new TransformStream();
                const a = await fetch('http://127.0.0.1:{first}/');
                // Started before the response below is handed back, so its native source would be
                // there to be claimed. preventClose, because more is going in after it.
                const firstDone = a.body.pipeTo(transform.writable, {{ preventClose: true }});
                const response = new Response(transform.readable);

                (async () => {{
                    // A pipe owns the writable until it finishes, so the second waits for the first.
                    await firstDone;
                    const b = await fetch('http://127.0.0.1:{second}/');
                    await b.body.pipeTo(transform.writable);
                }})();

                return response;
            }})());
        }})"#
    );
    let handle = start_serve(&handler);
    let port = handle.port();
    paths_taken::reset();

    let body = request(port, "GET", "/", "");
    let (shortcut, pumped) = paths_taken::counts();

    assert_eq!(
        dechunk(&body),
        "AAABBB",
        "the body piped in after the first must reach the client too: {body}"
    );
    assert_eq!(
        (shortcut, pumped),
        (0, 1),
        "a transform that is still open to more writes cannot be handed over as one body"
    );

    handle.stop();
}

/// The sharpest version of the mixture: a chunk already sitting in a transform's queue, and a pipe
/// from a host body into that same transform still running when the response is taken.
///
/// Piping into an identity transform carries the host body's native source onto the transform's
/// readable, so at the moment the response is taken there *is* a native source to claim — and the
/// host body behind it has not been read yet, because the pipe's first pull is deferred until
/// something actually wants a chunk. Claiming it would hand the wire that body alone and silently
/// drop the chunk queued ahead of it.
fn a_queued_chunk_survives_a_pipe_that_is_still_running() {
    let upstream = start_chunked_upstream(&["AB", "CD"]);
    let (body, shortcut, pumped) = serve_once(
        &format!(
            r#"addEventListener('fetch', (event) => {{
                event.respondWith((async () => {{
                    const encoder = new TextEncoder();
                    // The readable side needs room, or the write below waits for a reader that
                    // cannot exist yet — the only reader is the body pump, which does not start
                    // until this response is handed back.
                    const transform = new TransformStream(undefined, undefined,
                        {{ highWaterMark: 4 }});
                    const writer = transform.writable.getWriter();
                    // Queued in the transform before anything is piped in.
                    await writer.write(encoder.encode('<'));
                    writer.releaseLock();

                    const upstream = await fetch('http://127.0.0.1:{upstream}/');
                    // Deliberately not awaited: the pipe is still running when the response below
                    // is handed back and its body taken.
                    upstream.body.pipeTo(transform.writable);
                    return new Response(transform.readable);
                }})());
            }})"#
        ),
        "/",
    );

    assert_eq!(
        dechunk(&body),
        "<ABCD",
        "the chunk queued before the pipe must not be dropped in favour of the piped body: {body}"
    );
    assert_eq!(
        (shortcut, pumped),
        (0, 1),
        "a transform holding bytes of its own cannot be handed over as if it were just the piped \
         body"
    );
}

fn single_bodies_take_the_route_they_should() {
    // ---------------------------------------------------------------------------
    // Handing an upstream response straight back: the host body never becomes JS's problem.
    // ---------------------------------------------------------------------------
    let upstream = start_chunked_upstream(&["one ", "two"]);
    let (body, shortcut, pumped) = serve_once(
        &format!(
            "addEventListener('fetch', (e) => e.respondWith(fetch('http://127.0.0.1:{upstream}/')))"
        ),
        "/",
    );
    assert_eq!(dechunk(&body), "one two", "got: {body}");
    assert_eq!(
        (shortcut, pumped),
        (1, 0),
        "proxying a response whole must hand the host body to the wire, not pump it"
    );

    // Rebuilding the response around the same body keeps the shortcut: the body itself is
    // untouched, and an unread host-backed stream is still forwardable whole.
    let upstream = start_chunked_upstream(&["one ", "two"]);
    let (body, shortcut, pumped) = serve_once(
        &format!(
            r#"addEventListener('fetch', (event) => {{
                event.respondWith((async () => {{
                    const upstream = await fetch('http://127.0.0.1:{upstream}/');
                    return new Response(upstream.body, upstream);
                }})());
            }})"#
        ),
        "/",
    );
    assert_eq!(dechunk(&body), "one two", "got: {body}");
    assert_eq!(
        (shortcut, pumped),
        (1, 0),
        "an unread host body rebuilt into a new Response must still be forwarded whole"
    );

    // An identity transform is transparent to the shortcut: the native source is carried across it,
    // so piping through one does not force the body through JS.
    let upstream = start_chunked_upstream(&["one ", "two"]);
    let (body, shortcut, pumped) = serve_once(
        &format!(
            r#"addEventListener('fetch', (event) => {{
                event.respondWith((async () => {{
                    const upstream = await fetch('http://127.0.0.1:{upstream}/');
                    return new Response(upstream.body.pipeThrough(new TransformStream()));
                }})());
            }})"#
        ),
        "/",
    );
    assert_eq!(dechunk(&body), "one two", "got: {body}");
    assert_eq!(
        (shortcut, pumped),
        (1, 0),
        "an identity transform must not cost the shortcut"
    );

    // ---------------------------------------------------------------------------
    // And the cases that cannot shortcut, so the counters are shown to distinguish them rather than
    // always reporting a shortcut.
    // ---------------------------------------------------------------------------

    // A body the handler assembles itself has no host body behind it at all.
    let (body, shortcut, pumped) = serve_once(
        r#"addEventListener('fetch', (event) => {
            event.respondWith(new Response(new ReadableStream({
                start(c) { c.enqueue(new TextEncoder().encode('made up')); c.close(); }
            })));
        })"#,
        "/",
    );
    assert_eq!(dechunk(&body), "made up", "got: {body}");
    assert_eq!(
        (shortcut, pumped),
        (0, 1),
        "a JS-built body has nothing to shortcut and must be pumped"
    );

    // Reading an upstream body in JS and re-enqueuing it disqualifies the shortcut: bytes the
    // handler has already taken out cannot be handed over as if untouched.
    let upstream = start_chunked_upstream(&["ab", "cd"]);
    let (body, shortcut, pumped) = serve_once(
        &format!(
            r#"addEventListener('fetch', (event) => {{
                event.respondWith((async () => {{
                    const upstream = await fetch('http://127.0.0.1:{upstream}/');
                    const reader = upstream.body.getReader();
                    const out = new ReadableStream({{
                        async start(controller) {{
                            while (true) {{
                                const {{ done, value }} = await reader.read();
                                if (done) break;
                                controller.enqueue(value);
                            }}
                            controller.close();
                        }}
                    }});
                    return new Response(out);
                }})());
            }})"#
        ),
        "/",
    );
    assert_eq!(dechunk(&body), "abcd", "got: {body}");
    assert_eq!(
        (shortcut, pumped),
        (0, 1),
        "a body read out in JS and re-enqueued must be pumped"
    );
}

/// Several upstream bodies fed into one `TransformStream`, with the handler's own chunks written
/// between them. The response mixes host bytes from two upstreams with bytes JS made up, so it
/// cannot be handed to the wire whole, and every byte has to arrive in order. Piping into an
/// identity transform moves a native source to its readable side, so two of them arriving at the
/// same transform would overwrite each other.
///
/// `/piped` pipes each body in. Piping into a writable locks it, so the bodies go in one after
/// another with the transform held open in between, and the handler's own chunks are written around
/// them. `/read` reads each body with a reader and writes its chunks with a writer.
fn piping_many_bodies_into_one_transform_keeps_every_byte() {
    let upstream = start_chunked_upstream(&["AB", "CD"]);
    let handler = format!(
        r#"addEventListener('fetch', (event) => {{
            const route = new URL(event.request.url).pathname;
            event.respondWith((async () => {{
                const encoder = new TextEncoder();
                const transform = new TransformStream();
                const response = new Response(transform.readable);

                (async () => {{
                    let writer = transform.writable.getWriter();
                    await writer.write(encoder.encode('<'));

                    for (const round of [1, 2]) {{
                        const upstream = await fetch('http://127.0.0.1:{upstream}/?round=' + round);
                        if (route === '/piped') {{
                            writer.releaseLock();
                            // The pipe owns the writable until it finishes. Keep it open for the
                            // next.
                            await upstream.body.pipeTo(transform.writable, {{ preventClose: true }});
                            writer = transform.writable.getWriter();
                        }} else {{
                            const reader = upstream.body.getReader();
                            while (true) {{
                                const {{ done, value }} = await reader.read();
                                if (done) break;
                                await writer.write(value);
                            }}
                        }}
                        await writer.write(encoder.encode('|'));
                    }}

                    await writer.write(encoder.encode('>'));
                    await writer.close();
                }})();

                return response;
            }})());
        }})"#
    );
    let handle = start_serve(&handler);
    let port = handle.port();

    for route in ["/piped", "/read"] {
        paths_taken::reset();
        let body = request(port, "GET", route, "");
        let (shortcut, pumped) = paths_taken::counts();

        assert_eq!(
            dechunk(&body),
            "<ABCD|ABCD|>",
            "{route}: feeding two bodies into one transform must not lose either, nor the chunks \
             around them: {body}"
        );
        assert_eq!(
            (shortcut, pumped),
            (0, 1),
            "{route}: a transform fed by more than one body cannot be handed to the wire whole"
        );
    }

    handle.stop();
}
