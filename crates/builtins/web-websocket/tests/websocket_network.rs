// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Native `WebSocket` tests against in-process loopback servers that tungstenite runs over
//! blocking sockets.

#![cfg(not(target_arch = "wasm32"))]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use core_runtime::event_loop::{run_to_completion, with_event_loop, EventLoop};
use core_runtime::runtime::{clear_global_initializers, Runtime};
use js::conversion::FromJSVal;
use tungstenite::protocol::frame::coding::{Data, OpCode};
use tungstenite::protocol::frame::Frame;
use tungstenite::Message;

/// Start a server that accepts one connection on `127.0.0.1`, runs `handle` with it, and return
/// the server's `ws:` URL.
fn start_server(handle: impl FnOnce(TcpStream) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .ok();
            handle(stream);
        }
    });
    format!("ws://127.0.0.1:{port}/path")
}

/// Start a server that echoes every message until the client closes the connection.
fn start_echo_server() -> String {
    start_server(|stream| {
        let mut socket = tungstenite::accept(stream).unwrap();
        while let Ok(message) = socket.read() {
            if message.is_text() || message.is_binary() {
                let _ = socket.send(message);
            }
        }
    })
}

/// Evaluate `code`, which sets `globalThis.__out`, drive the event loop to completion, and return
/// `__out`.
fn run_and_get_out(code: &str) -> String {
    clear_global_initializers();
    libstarling::register_builtins();
    let rt = Runtime::init(&core_runtime::config::RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let rawcx = unsafe { scope.cx_mut().raw_cx() };
    let el = EventLoop::new();
    with_event_loop(&el, |_| {
        js::compile::evaluate_with_filename(&scope, code, "<test>", 1).expect("eval failed");
    });
    js::jobs::run_jobs(&scope);
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio_rt.block_on(async { unsafe { run_to_completion(rawcx, &el, tokio::time::sleep).await } });
    let out =
        js::compile::evaluate_with_filename(&scope, "globalThis.__out", "<check>", 1).unwrap();
    String::from_jsval(&scope, out, ()).unwrap()
}

/// A script that records the events of a socket to `url` in `__out`, and runs `on_open` with the
/// socket `ws` once it opens.
fn recording_script(url: &str, on_open: &str) -> String {
    format!(
        r#"
        const log = [];
        globalThis.__out = "";
        const ws = new WebSocket("{url}");
        ws.binaryType = "arraybuffer";
        ws.onopen = () => {{ log.push("open"); {on_open} }};
        ws.onmessage = (e) => log.push(typeof e.data === "string"
            ? "text:" + e.data
            : "binary:" + new Uint8Array(e.data).join(","));
        ws.onerror = (e) => log.push("error:" + e.constructor.name);
        ws.onclose = (e) => {{
            log.push(`close:${{e.code}}:${{e.reason}}:${{e.wasClean}}:${{ws.readyState}}`);
            globalThis.__out = log.join("|");
        }};
        "#
    )
}

#[test]
fn echoes_text_and_binary_messages_and_closes_cleanly() {
    let url = start_echo_server();
    let out = run_and_get_out(&recording_script(
        &url,
        r#"ws.send("hello"); ws.send(new Uint8Array([1, 2, 3]));
           ws.send(new Blob(["blob"])); ws.send(new Uint16Array([1, 2]).subarray(1));
           ws.onmessage = ((onmessage, count) => (e) => {
               onmessage(e);
               if (++count === 4) ws.close(1000, "bye");
           })(ws.onmessage, 0);"#,
    ));
    assert_eq!(
        out,
        "open|text:hello|binary:1,2,3|binary:98,108,111,98|binary:2,0|close:1000:bye:true:3"
    );
}

#[test]
fn reports_a_server_initiated_close_and_assembles_fragments() {
    let url = start_server(|stream| {
        let mut socket = tungstenite::accept(stream).unwrap();
        let first = Frame::message("frag", OpCode::Data(Data::Text), false);
        let last = Frame::message("ment", OpCode::Data(Data::Continue), true);
        socket.send(Message::Frame(first)).unwrap();
        socket.send(Message::Frame(last)).unwrap();
        let big = vec![7u8; 200_000];
        socket.send(Message::Binary(big.into())).unwrap();
        socket
            .close(Some(tungstenite::protocol::CloseFrame {
                code: 4001.into(),
                reason: "done".into(),
            }))
            .unwrap();
        while socket.read().is_ok() {}
    });
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "";
        const log = [];
        const ws = new WebSocket("{url}");
        ws.binaryType = "arraybuffer";
        ws.onmessage = (e) => log.push(typeof e.data === "string"
            ? e.data : e.data.byteLength + ":" + new Uint8Array(e.data)[199999]);
        ws.onclose = (e) => {{
            log.push(`${{e.code}}:${{e.reason}}:${{e.wasClean}}`);
            globalThis.__out = log.join("|");
        }};
        "#
    ));
    assert_eq!(out, "fragment|200000:7|4001:done:true");
}

#[test]
fn fails_on_a_response_that_does_not_switch_protocols() {
    let url = start_server(|mut stream| {
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer);
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
    });
    let out = run_and_get_out(&recording_script(&url, ""));
    assert_eq!(out, "error:Event|close:1006::false:3");
}

#[test]
fn fails_on_invalid_utf8_in_a_text_message() {
    let url = start_server(|stream| {
        let mut socket = tungstenite::accept(stream).unwrap();
        let frame = Frame::message(vec![0xff, 0xfe], OpCode::Data(Data::Text), true);
        socket.send(Message::Frame(frame)).unwrap();
        while socket.read().is_ok() {}
    });
    let out = run_and_get_out(&recording_script(&url, ""));
    assert_eq!(out, "open|error:Event|close:1006::false:3");
}

#[test]
fn counts_buffered_data_until_it_is_sent() {
    let url = start_echo_server();
    let out = run_and_get_out(&recording_script(
        &url,
        r#"ws.send("12345"); ws.send(new ArrayBuffer(3));
           log.push("buffered:" + ws.bufferedAmount);
           setTimeout(() => { log.push("later:" + ws.bufferedAmount); ws.close(); }, 100);
           ws.onmessage = null;"#,
    ));
    assert_eq!(out, "open|buffered:8|later:0|close:1005::true:3");
}

#[test]
fn close_while_connecting_fails_the_connection() {
    let url = start_echo_server();
    let out = run_and_get_out(&format!(
        r#"
        {}
        ws.close();
        log.push("state:" + ws.readyState);
        "#,
        recording_script(&url, "")
    ));
    assert_eq!(out, "state:2|error:Event|close:1006::false:3");
}

/// With Node.js's options, the opening handshake sends the `headers` option's headers, except
/// those the handshake sets itself, whose own values are sent instead.
#[test]
#[allow(clippy::result_large_err)] // tungstenite's handshake callback returns its error response.
fn sends_further_headers_but_not_the_handshake_headers() {
    let (sender, received) = std::sync::mpsc::channel();
    let url = start_server(move |stream| {
        let record = |request: &tungstenite::handshake::server::Request, response| {
            let headers: Vec<String> = request
                .headers()
                .iter()
                .map(|(name, value)| format!("{name}={}", value.to_str().unwrap()))
                .collect();
            sender.send(headers).unwrap();
            Ok(response)
        };
        let mut socket = tungstenite::accept_hdr(stream, record).unwrap();
        while socket.read().is_ok() {}
    });
    web_websocket::websocket::set_node_compat(true);
    let out = run_and_get_out(&format!(
        r#"
        globalThis.__out = "";
        const ws = new WebSocket("{url}", {{ headers: {{
            Host: "example.com", Connection: "keep-alive", Upgrade: "h2c",
            "Sec-WebSocket-Version": "8", "Sec-WebSocket-Key": "abc", "X-Custom": "y",
        }} }});
        ws.onopen = () => {{ globalThis.__out = "open"; ws.close(); }};
        "#
    ));
    assert_eq!(out, "open");
    let headers = received.recv().unwrap();
    let host = url
        .trim_start_matches("ws://")
        .trim_end_matches("/path")
        .to_string();
    let value = |name: &str| -> Vec<&str> {
        headers
            .iter()
            .filter_map(|header| header.strip_prefix(&format!("{name}=")))
            .collect()
    };
    assert_eq!(value("host"), [host.as_str()]);
    assert_eq!(value("connection"), ["Upgrade"]);
    assert_eq!(value("upgrade"), ["websocket"]);
    assert_eq!(value("sec-websocket-version"), ["13"]);
    assert_eq!(value("sec-websocket-key").len(), 1);
    assert_ne!(value("sec-websocket-key"), ["abc"]);
    assert_eq!(value("x-custom"), ["y"]);
}
