// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TLS client sessions against an in-process rustls server whose certificate `fixtures/tls/ca.pem`
//! issued. The test CA is trusted through `NODE_EXTRA_CA_CERTS`.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{fixture, handshake, server};
use platform::tls::TlsClient;
use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn bytes_after_close_notify_are_discarded() {
    // SAFETY: this is the only test in this binary, so no other thread reads the environment.
    std::env::set_var("NODE_EXTRA_CA_CERTS", fixture("ca.pem"));
    let mut client = TlsClient::new("localhost").unwrap();
    let mut server = server();
    handshake(&mut client, &mut server);

    server.writer().write_all(b"hello").unwrap();
    server.send_close_notify();
    let mut incoming = Vec::new();
    while server.wants_write() {
        server.write_tls(&mut incoming).unwrap();
    }
    // `read_tls` reads at most 4096 bytes at a time, so these are left over after the alert.
    incoming.resize(incoming.len() + 8192, 0);

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut plaintext = Vec::new();
        let closed = client.receive(&incoming, &mut plaintext).unwrap();
        let again = client.receive(&[0; 16], &mut plaintext).unwrap();
        let _ = sender.send((closed, again, plaintext));
    });
    let (closed, again, plaintext) = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("`receive` returned");
    assert!(closed);
    assert!(!again);
    assert_eq!(plaintext, b"hello");
}
