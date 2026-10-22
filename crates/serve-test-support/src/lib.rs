// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Shared plumbing for the serve-mode integration tests: speaking HTTP/1.1 to a server, the
//! upstream servers a handler forwards to, and the `wasmtime serve` harness in [`wasm_serve`].
//!
//! This crate has no dependencies, so the wasm end-to-end suite in `tests/` builds without
//! building the engine.

#![cfg(not(target_arch = "wasm32"))]

pub mod wasm_serve;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Like [`request`], but gives up after `patience` and returns `None` — for the cases where a
/// server that fails the test would otherwise leave it hanging rather than failing it.
pub fn request_within(port: u16, path: &str, patience: Duration) -> Option<String> {
    full_request_within(port, "GET", path, "", patience).map(body_of)
}

/// The part of `response` after the header terminator. See [`split_response`].
fn body_of(response: String) -> String {
    split_response(&response).1.to_string()
}

/// The head and the body of `response`, split at the header terminator. A response with no header
/// terminator is malformed rather than empty. It has no head, and the whole response is its body,
/// so an assertion on the body cannot pass against nothing at all.
pub(crate) fn split_response(response: &str) -> (Option<&str>, &str) {
    match response.split_once("\r\n\r\n") {
        Some((head, body)) => (Some(head), body),
        None => (None, response),
    }
}

/// The `Content-Length` value in `head`, which must already be lowercase.
fn content_length(head: &str) -> Option<usize> {
    head.split("content-length:")
        .nth(1)
        .and_then(|rest| rest.split("\r\n").next())
        .and_then(|value| value.trim().parse().ok())
}

/// The size on a chunk-size line, with any chunk extension after `;` ignored.
fn chunk_size(size_line: &str) -> Option<usize> {
    usize::from_str_radix(size_line.split(';').next()?.trim(), 16).ok()
}

/// Like [`request_within`], but for any method and body, and returning the whole response — status
/// line and headers included — for the assertions that are about the framing rather than the body.
pub fn full_request_within(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    patience: Duration,
) -> Option<String> {
    let response = raw_response_within(port, method, path, body, patience)?;
    Some(String::from_utf8_lossy(&response).into_owned())
}

/// Like [`full_request_within`], but the bytes as they arrived — for the assertions that are about
/// exactly what went on the wire, which UTF-8 replacement would erase.
pub fn raw_response_within(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    patience: Duration,
) -> Option<Vec<u8>> {
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    send_within(port, req.as_bytes(), patience)
}

/// Send `raw` on a new connection and read the response to EOF within `patience`.
fn send_within(port: u16, raw: &[u8], patience: Duration) -> Option<Vec<u8>> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // A refused request is responded to and its connection closed while this is still writing, so
    // a failed write is part of the outcome rather than an error.
    let _ = stream.write_all(raw);
    read_until_eof(&mut stream, patience)
}

/// How long the unbounded-looking helpers below will actually wait. Generous enough that no
/// passing test comes near it, and finite so that a server which stops responding fails its
/// test instead of hanging the run.
pub const PATIENCE: Duration = Duration::from_secs(30);

/// How long [`eventually`] polls before returning its last result. Finite so that work which never
/// completes fails its test.
pub const SETTLE_PATIENCE: Duration = Duration::from_secs(20);

/// Call `poll` every 20 ms until its result satisfies `settled` or [`SETTLE_PATIENCE`] runs out,
/// and return the last result.
///
/// Work a handler leaves behind runs while the request's loop is drained after the response: a
/// `waitUntil` promise, a signal's reactions, a stream's cancel. Polling until the outcome is there
/// takes as long as the work does, rather than a fixed span chosen for the slowest machine.
///
/// Only for assertions that wait for something to happen. A request drives the loop it is served
/// on, so an assertion that something does not happen waits a fixed span instead.
pub fn eventually(mut poll: impl FnMut() -> String, settled: impl Fn(&str) -> bool) -> String {
    let deadline = std::time::Instant::now() + SETTLE_PATIENCE;
    loop {
        let result = poll();
        if settled(&result) || std::time::Instant::now() >= deadline {
            return result;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Make one HTTP/1.1 request and return the response body (after the headers).
pub fn request(port: u16, method: &str, path: &str, body: &str) -> String {
    body_of(
        full_request_within(port, method, path, body, PATIENCE)
            .expect("the server answered within the patience"),
    )
}

/// A listener on a port the system picks on `127.0.0.1`, and that port.
pub fn bind_local() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// A `200` response with `body`, framed by its length, closing the connection.
fn ok_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    )
}

/// A minimal upstream HTTP server (multi-connection) that always replies with `body`.
pub fn start_upstream(body: &'static str) -> u16 {
    let (listener, port) = bind_local();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(ok_response(body).as_bytes());
        }
    });
    port
}

/// An upstream that responds with a chunked body, pausing between chunks, so a handler forwarding it
/// has to actually stream rather than receive the whole thing at once.
pub fn start_chunked_upstream(chunks: &'static [&'static str]) -> u16 {
    start_chunked(chunks, true)
}

/// An upstream whose chunked response is cut off: it sends `chunks` and then closes the connection
/// without the terminating chunk, so a handler forwarding it is forwarding a truncation.
pub fn start_truncated_upstream(chunks: &'static [&'static str]) -> u16 {
    start_chunked(chunks, false)
}

/// An upstream that sends `chunks` as a chunked body, 20 ms apart, and the terminating chunk if
/// `terminate` is set.
fn start_chunked(chunks: &'static [&'static str], terminate: bool) -> u16 {
    let (listener, port) = bind_local();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            );
            for chunk in chunks {
                let _ = stream.write_all(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes());
                let _ = stream.flush();
                std::thread::sleep(Duration::from_millis(20));
            }
            if terminate {
                let _ = stream.write_all(b"0\r\n\r\n");
            }
        }
    });
    port
}

/// An upstream that waits `delay` before responding with `body`, so a `fetch` to it is still in
/// flight while other work — another request, a previous one's drain — runs alongside it.
pub fn start_slow_upstream(delay: Duration, body: &'static str) -> u16 {
    let (listener, port) = bind_local();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                std::thread::sleep(delay);
                let _ = stream.write_all(ok_response(body).as_bytes());
            });
        }
    });
    port
}

/// An upstream whose chunked response never ends: a chunk every `every`, no terminator ever. A
/// handler proxying it sends a *host* body rather than a JS stream, which the serve timeouts have
/// to bound just the same.
pub fn start_endless_upstream(every: Duration) -> u16 {
    let (listener, port) = bind_local();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                if stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    )
                    .is_err()
                {
                    return;
                }
                // Until the peer goes away — which is what the timeout under test brings about.
                loop {
                    if stream.write_all(b"5\r\ntick \r\n").is_err() || stream.flush().is_err() {
                        return;
                    }
                    std::thread::sleep(every);
                }
            });
        }
    });
    port
}

/// An upstream that reports each request as it arrives and holds it until released.
///
/// By the time an arrival is reported, the guest has issued the request and is blocked on the
/// response with nothing left on its stack. A serving host hands an instance a further request
/// only in that state. A marker the guest writes does not establish it, since the guest writes one
/// while it is still running.
pub struct HeldUpstream {
    port: u16,
    arrivals: std::sync::mpsc::Receiver<()>,
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HeldUpstream {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for the next request to arrive, and report whether one did within `patience`.
    pub fn await_arrival(&self, patience: Duration) -> bool {
        self.arrivals.recv_timeout(patience).is_ok()
    }

    /// Answer everything held, and everything that arrives from here on. A request the scenario
    /// did not gate, such as one a second instance made, must not be left waiting on a gate that
    /// is never opened again.
    pub fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Start a [`HeldUpstream`] that responds with `body` once released.
pub fn start_held_upstream(body: &'static str) -> HeldUpstream {
    let (listener, port) = bind_local();
    let (arrived, arrivals) = std::sync::mpsc::channel();
    let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = released.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let arrived = arrived.clone();
            let flag = flag.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = arrived.send(());
                while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = stream.write_all(ok_response(body).as_bytes());
            });
        }
    });
    HeldUpstream {
        port,
        arrivals,
        released,
    }
}

/// A proxy that holds each request until released, then forwards it to the port passed to
/// [`ForwardingGate::release`] and copies the response back.
///
/// For the cases where the guest is the client. A handler fetching its own server sends that
/// request while it is still running, and a test cannot delay it. Forwarding it from here sets
/// when it arrives, so it can arrive once the instance that sent it is idle.
pub struct ForwardingGate {
    port: u16,
    arrivals: std::sync::mpsc::Receiver<()>,
    /// The port to forward to, or `0` while requests are held.
    target: std::sync::Arc<std::sync::atomic::AtomicU16>,
}

impl ForwardingGate {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for the next request to arrive, and report whether one did within `patience`.
    pub fn await_arrival(&self, patience: Duration) -> bool {
        self.arrivals.recv_timeout(patience).is_ok()
    }

    /// Forward everything held, and everything that arrives from here on, to `target`.
    pub fn release(&self, target: u16) {
        self.target
            .store(target, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Start a [`ForwardingGate`].
pub fn start_forwarding_gate() -> ForwardingGate {
    let (listener, port) = bind_local();
    let (arrived, arrivals) = std::sync::mpsc::channel();
    let target = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
    let released = target.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut from) = stream else { continue };
            let arrived = arrived.clone();
            let released = released.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match from.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => head.extend_from_slice(&buf[..read]),
                    }
                }
                let _ = arrived.send(());
                let target = loop {
                    match released.load(std::sync::atomic::Ordering::SeqCst) {
                        0 => std::thread::sleep(Duration::from_millis(5)),
                        target => break target,
                    }
                };
                // Forwarded with `Connection: close`, so the response ends at EOF. The rest of the
                // request is passed on as it arrived.
                let text = String::from_utf8_lossy(&head).into_owned();
                let mut forwarded: Vec<&str> = text
                    .lines()
                    .filter(|line| !line.to_lowercase().starts_with("connection:"))
                    .collect();
                while forwarded.last().is_some_and(|line| line.is_empty()) {
                    forwarded.pop();
                }
                let request = format!("{}\r\nConnection: close\r\n\r\n", forwarded.join("\r\n"));
                let Ok(mut to) = TcpStream::connect(("127.0.0.1", target)) else {
                    return;
                };
                if to.write_all(request.as_bytes()).is_err() {
                    return;
                }
                let answer = read_until_eof(&mut to, PATIENCE).unwrap_or_default();
                let _ = from.write_all(&answer);
            });
        }
    });
    ForwardingGate {
        port,
        arrivals,
        target,
    }
}

/// An upstream that accepts connections and never responds, holding them open until the process
/// ends. A `fetch` to it stays in flight for as long as anything is willing to wait for it.
pub fn start_silent_upstream() -> u16 {
    let (listener, port) = bind_local();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            // Held rather than dropped: closing would fail the fetch, which is the opposite of
            // what this upstream is for.
            held.push(stream);
        }
    });
    port
}

/// An upstream that reads a request body, chunked or length-delimited, and responds with it, so a
/// handler forwarding a body upstream can be checked on what actually arrived there.
pub fn start_echo_upstream() -> u16 {
    let (listener, port) = bind_local();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut raw = Vec::new();
            let mut buf = [0u8; 1024];
            while let Ok(read) = stream.read(&mut buf) {
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..read]);
                let text = String::from_utf8_lossy(&raw).to_string();
                let Some((head, body)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let lower = head.to_lowercase();
                let chunked = lower.contains("transfer-encoding: chunked");
                let complete = if chunked {
                    body.ends_with("0\r\n\r\n")
                } else {
                    content_length(&lower).is_some_and(|length| body.len() >= length)
                };
                if !complete {
                    continue;
                }
                let payload = if chunked {
                    dechunk(body)
                } else {
                    body.to_string()
                };
                let _ = stream.write_all(ok_response(&payload).as_bytes());
                break;
            }
        }
    });
    port
}

/// Decode an HTTP/1.1 message body into its payload, chunked or not.
///
/// A body with no chunk-size line to read is already its payload, which is how a length-framed
/// response arrives. Every payload these tests send is free of `CRLF`, so a length-framed one is
/// never mistaken for a chunked one.
pub fn dechunk(body: &str) -> String {
    let is_chunked = body
        .split_once("\r\n")
        .is_some_and(|(size_line, _)| chunk_size(size_line).is_some());
    if !is_chunked {
        return body.to_string();
    }
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = chunk_size(size_line).unwrap_or(0);
        if size == 0 {
            break;
        }
        let (chunk, tail) = after.split_at(size.min(after.len()));
        out.push_str(chunk);
        rest = tail.strip_prefix("\r\n").unwrap_or(tail);
    }
    out
}

/// Read `stream` to EOF within `patience`: `Some(bytes)` once EOF arrives, `None` if the budget
/// runs out first — the connection was left open. Total rather than per-read, since a peer that
/// streams forever keeps every individual read prompt.
pub fn read_until_eof(stream: &mut TcpStream, patience: Duration) -> Option<Vec<u8>> {
    let deadline = std::time::Instant::now() + patience;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        // A socket whose peer has already closed refuses the option outright on macOS (EINVAL),
        // and needs no bound: the read below cannot wait for data that can no longer arrive.
        let _ = stream.set_read_timeout(Some(remaining));
        match stream.read(&mut buf) {
            Ok(0) => return Some(bytes),
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return None
            }
            // A reset is how the peer closing surfaces once it has unread data for us; the
            // connection is just as gone as at a clean EOF.
            Err(_) => return Some(bytes),
        }
    }
}

/// Like [`raw_request`], but gives up after `patience` and returns `None` — for a server a failing
/// test would otherwise leave the suite hanging on.
pub fn raw_request_within(port: u16, raw: &[u8], patience: Duration) -> Option<String> {
    let response = send_within(port, raw, patience)?;
    Some(String::from_utf8_lossy(&response).into_owned())
}

/// Send `raw` bytes and return the full response (status line included).
pub fn raw_request(port: u16, raw: &[u8]) -> String {
    raw_request_within(port, raw, PATIENCE).unwrap_or_default()
}

/// Read exactly one HTTP response off a connection that may stay open — framed by its
/// `Content-Length` or chunked terminator rather than by EOF, which a kept-alive connection
/// never reaches. `carry` holds bytes read past the response's end (a pipelined successor's
/// head, say) between calls on the same connection. `None` if the connection closes or
/// `patience` runs out before one complete response has arrived.
pub fn read_one_response(
    stream: &mut TcpStream,
    carry: &mut Vec<u8>,
    patience: Duration,
) -> Option<String> {
    let deadline = std::time::Instant::now() + patience;
    let mut buf = [0u8; 64 * 1024];
    loop {
        if let Some(end) = response_end(carry) {
            let response: Vec<u8> = carry.drain(..end).collect();
            return Some(String::from_utf8_lossy(&response).into_owned());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let _ = stream.set_read_timeout(Some(remaining));
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => carry.extend_from_slice(&buf[..n]),
            Err(_) => return None,
        }
    }
}

/// Where the first complete response in `bytes` ends, or `None` if more must arrive first.
fn response_end(bytes: &[u8]) -> Option<usize> {
    let head_end = find(bytes, 0, b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&bytes[..head_end]).to_lowercase();
    if head.contains("transfer-encoding: chunked") {
        return chunked_end(bytes, head_end);
    }
    let length = content_length(&head).unwrap_or(0);
    let end = head_end + length;
    (bytes.len() >= end).then_some(end)
}

/// Walk a chunked body's framing from `pos` to just past its terminator, or `None` if it is
/// still incomplete. Assumes no trailer fields, which these servers never send.
fn chunked_end(bytes: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let line_end = find(bytes, pos, b"\r\n")?;
        let size = chunk_size(std::str::from_utf8(&bytes[pos..line_end]).ok()?)?;
        if size == 0 {
            // The zero-size line, then the final empty line.
            let end = line_end + 4;
            return (bytes.len() >= end).then_some(end);
        }
        // The size line, the data, and the data's trailing CRLF.
        pos = line_end + 2 + size + 2;
        if pos > bytes.len() {
            return None;
        }
    }
}

/// The first occurrence of `needle` at or after `from`.
fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}
