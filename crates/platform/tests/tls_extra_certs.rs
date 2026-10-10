// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A `NODE_EXTRA_CA_CERTS` file with a malformed certificate after the test CA's.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{fixture, handshake, server};
use platform::tls::TlsClient;

#[test]
fn certificates_before_a_malformed_one_are_trusted() {
    let mut pem = std::fs::read(fixture("ca.pem")).unwrap();
    pem.extend_from_slice(b"-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n");
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("malformed-extra-certs.pem");
    std::fs::write(&path, pem).unwrap();
    // SAFETY: this is the only test in this binary, so no other thread reads the environment.
    std::env::set_var("NODE_EXTRA_CA_CERTS", &path);
    let mut client = TlsClient::new("localhost").unwrap();
    handshake(&mut client, &mut server());
}
