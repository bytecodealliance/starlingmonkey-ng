// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A TLS server for the tests, with the certificates in `fixtures/tls`.

use platform::tls::TlsClient;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;

pub fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

pub fn server() -> rustls::ServerConnection {
    let chain = vec![CertificateDer::from_pem_file(fixture("server.pem")).unwrap()];
    let key = PrivateKeyDer::from_pem_file(fixture("server.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    rustls::ServerConnection::new(Arc::new(config)).unwrap()
}

/// Run the handshake between `client` and `server` to completion.
pub fn handshake(client: &mut TlsClient, server: &mut rustls::ServerConnection) {
    let mut plaintext = Vec::new();
    for _ in 0..10 {
        let outgoing = client.take_outgoing();
        server.read_tls(&mut outgoing.as_slice()).unwrap();
        server.process_new_packets().unwrap();
        let mut incoming = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut incoming).unwrap();
        }
        client.receive(&incoming, &mut plaintext).unwrap();
        if !server.is_handshaking() && outgoing.is_empty() && incoming.is_empty() {
            return;
        }
    }
    panic!("the handshake didn't complete");
}
