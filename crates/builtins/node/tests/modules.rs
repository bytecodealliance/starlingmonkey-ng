// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The Node.js builtin modules and globals, each tested by a script in `tests/js`.

// The scripts run in this process, which needs a native target.
#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{run_js, run_js_after};

#[test]
fn assert() {
    run_js("assert.mjs");
}

#[test]
fn async_hooks() {
    run_js("async_hooks.mjs");
}

#[test]
fn buffer() {
    run_js("buffer.mjs");
}

#[test]
fn commonjs() {
    run_js("commonjs.mjs");
}

#[test]
fn commonjs_entry() {
    run_js("commonjs-entry.cjs");
}

#[test]
fn crypto() {
    run_js("crypto.mjs");
}

#[test]
fn fs() {
    run_js("fs.mjs");
}

#[test]
fn diagnostics_channel() {
    run_js("diagnostics_channel.mjs");
}

#[test]
fn events() {
    run_js("events.mjs");
}

#[test]
fn io_modules() {
    run_js("io_modules.mjs");
}

#[test]
fn net() {
    run_js("net.mjs");
}

#[test]
fn os() {
    run_js("os.mjs");
}

#[test]
fn path() {
    run_js("path.mjs");
}

#[test]
fn process() {
    run_js("process.mjs");
}

#[test]
fn punycode() {
    run_js("punycode.mjs");
}

#[test]
fn querystring() {
    run_js("querystring.mjs");
}

#[test]
fn readline() {
    run_js("readline.mjs");
}

#[test]
fn stack_trace() {
    run_js("stack_trace.mjs");
}

#[test]
fn stream() {
    run_js("stream.mjs");
}

#[test]
fn string_decoder() {
    run_js("string_decoder.mjs");
}

#[test]
fn stubs() {
    run_js("stubs.mjs");
}

#[test]
fn timers() {
    run_js("timers.mjs");
}

#[test]
fn url() {
    run_js("url.mjs");
}

#[test]
fn util() {
    run_js("util.mjs");
}

#[test]
fn zlib() {
    run_js("zlib.mjs");
}

#[test]
fn http() {
    let port = start_http_server();
    let refused = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("binding a port")
        .port();
    run_js_after(
        "http.mjs",
        &format!("globalThis.HTTP_PORT = {port}; globalThis.REFUSED_PORT = {refused};"),
    );
}

/// Serve HTTP/1.1 on a local port for the rest of the process, and return the port. Each request
/// gets its own connection, which the response closes:
///
/// - The response to `/chunked` is the chunks `one`, `two` and `three`.
/// - `/redirect` redirects to `/echo`.
/// - The response to `/slow` comes after two seconds.
/// - The response to every other path is a JSON object of the request's method, path, headers, as
///   pairs, and body, with two `Set-Cookie` and two `X-Dup` headers.
fn start_http_server() -> u16 {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding a port");
    let port = listener.local_addr().expect("a local address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("cloning the stream"));
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .expect("reading the request line");
                let mut parts = request_line.split_whitespace();
                let (method, path) = (
                    parts.next().unwrap_or_default(),
                    parts.next().unwrap_or_default(),
                );
                let mut headers = Vec::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("reading a header");
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').expect("a header line");
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().expect("a length");
                    }
                    headers.push((name.to_string(), value.trim().to_string()));
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("reading the body");
                let quote =
                    |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
                let mut stream = stream;
                let response = match path {
                    "/chunked" => "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
                        3\r\none\r\n3\r\ntwo\r\n5\r\nthree\r\n0\r\n\r\n"
                        .to_string(),
                    "/redirect" => "HTTP/1.1 302 Found\r\nLocation: /echo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    _ => {
                        if path == "/slow" {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                        }
                        let pairs: Vec<String> = headers
                            .iter()
                            .map(|(name, value)| format!("[{},{}]", quote(name), quote(value)))
                            .collect();
                        let json = format!(
                            "{{\"method\":{},\"path\":{},\"headers\":[{}],\"body\":{}}}",
                            quote(method),
                            quote(path),
                            pairs.join(","),
                            quote(&String::from_utf8_lossy(&body)),
                        );
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\
                             X-Dup: 1\r\nX-Dup: 2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                            json.len()
                        )
                    }
                };
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    port
}
