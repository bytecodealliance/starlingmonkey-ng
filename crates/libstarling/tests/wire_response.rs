// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! How `prepare_wire_response` reduces a dispatched response to what goes on the wire: which header
//! fields are dropped, when a status or a `HEAD` request rules out content, and which
//! `Content-Length` frames the body.

#![cfg(not(target_arch = "wasm32"))]

use libstarling::{prepare_wire_response, WireResponse};
use platform::http::OutgoingBody;

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

fn names(response: &WireResponse) -> Vec<String> {
    response
        .headers
        .iter()
        .map(|(name, _)| name.as_str().to_string())
        .collect()
}

#[test]
fn framing_headers_from_the_handler_are_dropped() {
    let response = prepare_wire_response(
        false,
        200,
        headers(&[
            ("Content-Length", "10000"),
            ("transfer-encoding", "chunked"),
            ("CONNECTION", "keep-alive"),
            ("x-keep", "me"),
        ]),
        OutgoingBody::Bytes(bytes::Bytes::from_static(b"short")),
    );
    assert_eq!(names(&response), ["x-keep"]);
}

#[test]
fn headers_that_could_split_the_response_are_dropped() {
    let response = prepare_wire_response(
        false,
        200,
        headers(&[
            ("x-cr", "a\rb"),
            ("x-lf", "a\nb"),
            ("x-nul", "a\0b"),
            ("x-na\rme", "fine"),
            ("x-keep", "me"),
        ]),
        OutgoingBody::Bytes(bytes::Bytes::new()),
    );
    assert_eq!(names(&response), ["x-keep"]);
}

/// Each code unit of a value becomes one byte. A code unit above U+00FF becomes `?`.
#[test]
fn header_values_are_isomorphic_encoded() {
    let response = prepare_wire_response(
        false,
        200,
        headers(&[("x-latin1", "caf\u{e9}"), ("x-wide", "a\u{2603}b")]),
        OutgoingBody::Bytes(bytes::Bytes::new()),
    );
    assert_eq!(response.headers["x-latin1"].as_bytes(), b"caf\xe9");
    assert_eq!(response.headers["x-wide"].as_bytes(), b"a?b");
}

/// The body a response sends, for a test that only ever builds in-memory ones.
fn content(response: &WireResponse) -> &[u8] {
    match &response.body {
        OutgoingBody::Bytes(bytes) => bytes,
        _ => panic!("the test built an in-memory body"),
    }
}

/// A `1xx`, `204` or `304` has no content and no declared length, whatever the body, the
/// handler's `Content-Length` and the method.
#[test]
fn a_bodiless_status_has_no_content_or_length() {
    for status in [103, 204, 304] {
        for head_request in [false, true] {
            for streamed in [false, true] {
                let body = if streamed {
                    platform::http::body_channel().1
                } else {
                    OutgoingBody::Bytes(bytes::Bytes::from_static(b"ignored"))
                };
                let response = prepare_wire_response(
                    head_request,
                    status,
                    headers(&[("x-keep", "me"), ("content-length", "7")]),
                    body,
                );
                let case = format!("{status} head={head_request} streamed={streamed}");
                assert!(content(&response).is_empty(), "{case}");
                assert_eq!(response.declared_length, None, "{case}");
                assert_eq!(names(&response), ["x-keep"], "{case}");
            }
        }
    }
}

#[test]
fn a_head_request_declares_the_length_it_would_have_sent() {
    let response = prepare_wire_response(
        true,
        200,
        Vec::new(),
        OutgoingBody::Bytes(bytes::Bytes::from_static(b"BODYBYTES")),
    );
    assert!(content(&response).is_empty());
    assert_eq!(response.declared_length, Some(9));
}

#[test]
fn a_head_request_declares_no_length_it_would_have_to_produce() {
    // A streamed body's length is only knowable by generating the content.
    let (_sender, streamed) = platform::http::body_channel();
    let response = prepare_wire_response(true, 200, Vec::new(), streamed);
    assert!(content(&response).is_empty());
    assert_eq!(response.declared_length, None);
}

/// A streamed body has no length until it is produced, so the handler's declaration is the
/// only thing that can frame it.
#[test]
fn a_streamed_body_takes_the_declared_length() {
    let (_sender, streamed) = platform::http::body_channel();
    let response =
        prepare_wire_response(false, 200, headers(&[("content-length", "42")]), streamed);
    assert_eq!(response.declared_length, Some(42));
    assert_eq!(names(&response), Vec::<String>::new());
}

/// Content already in memory is measured. A declaration that disagrees with it would frame the
/// response as something other than what it sends.
#[test]
fn an_in_memory_body_is_measured_not_declared() {
    let response = prepare_wire_response(
        false,
        200,
        headers(&[("content-length", "10000")]),
        OutgoingBody::Bytes(bytes::Bytes::from_static(b"short")),
    );
    assert_eq!(response.declared_length, Some(5));
    assert_eq!(content(&response), b"short");
}

/// Only an unambiguous `1*DIGIT` that fits a `u64` frames a response. Repeated fields arrive
/// comma-joined, which is not a number, and two separate fields are refused as well.
#[test]
fn a_declared_length_that_is_not_one_number_is_refused() {
    for value in [
        "",
        "x",
        "10000, 10000",
        " 5",
        "+5",
        "5.0",
        "0x5",
        "-1",
        "18446744073709551616",
    ] {
        let (_sender, streamed) = platform::http::body_channel();
        let response =
            prepare_wire_response(false, 200, headers(&[("content-length", value)]), streamed);
        assert_eq!(response.declared_length, None, "{value:?}");
    }
    let (_sender, streamed) = platform::http::body_channel();
    let response = prepare_wire_response(
        false,
        200,
        headers(&[("content-length", "5"), ("Content-Length", "5")]),
        streamed,
    );
    assert_eq!(response.declared_length, None, "two fields");
}

/// A consumed body doesn't send any bytes, so no length frames it.
#[test]
fn a_consumed_body_declares_no_length() {
    let response = prepare_wire_response(
        false,
        200,
        headers(&[("content-length", "42")]),
        OutgoingBody::Consumed,
    );
    assert_eq!(response.declared_length, None);
}

/// RFC 9110 §9.3.2: a `HEAD` may report the length its `GET` would have had. The handler's
/// declaration takes precedence over the length of an in-memory body.
#[test]
fn a_head_request_declares_the_length_it_was_given() {
    let response = prepare_wire_response(
        true,
        200,
        headers(&[("content-length", "12345")]),
        OutgoingBody::Bytes(bytes::Bytes::from_static(b"BODYBYTES")),
    );
    assert!(content(&response).is_empty());
    assert_eq!(response.declared_length, Some(12345));
}
