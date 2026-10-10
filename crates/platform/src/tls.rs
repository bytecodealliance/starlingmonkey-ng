// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TLS client sessions over rustls, which the caller drives with the bytes it reads from and
//! writes to a connection, such as a [`Connection`](crate::net::Connection).
//!
//! A session verifies the server's certificate chain against the Mozilla root certificates of
//! `webpki-roots` and the certificates of the PEM file that the `NODE_EXTRA_CA_CERTS` environment
//! variable names, which is read when the first session is created. If the file can't be read or
//! a certificate in it can't be parsed, a warning is printed to stderr, and the certificates before
//! that one are used.
//!
//! Sessions are only supported natively, where aws-lc-rs provides the cryptography. On wasm,
//! creating a session fails: the pure-Rust rustls providers generate keys with `getrandom` 0.2,
//! whose WASI backend makes the component import WASIp2 interfaces.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, OnceLock};

#[cfg(not(target_arch = "wasm32"))]
use rustls::pki_types::pem::PemObject;
#[cfg(not(target_arch = "wasm32"))]
use rustls::pki_types::{CertificateDer, ServerName};

/// Why a TLS operation failed.
#[derive(Debug)]
pub struct TlsError(String);

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TlsError {}

#[cfg(not(target_arch = "wasm32"))]
impl From<rustls::Error> for TlsError {
    fn from(error: rustls::Error) -> Self {
        TlsError(error.to_string())
    }
}

/// The configuration every session uses, created with the first session.
#[cfg(not(target_arch = "wasm32"))]
fn config() -> Result<Arc<rustls::ClientConfig>, TlsError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            if let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS") {
                if let Err(error) = add_extra_certs(&mut roots, &path) {
                    eprintln!(
                        "Warning: Ignoring extra certs from `{}`, load failed: {error}",
                        path.to_string_lossy()
                    );
                }
            }
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider()))
                .with_safe_default_protocol_versions()
                .map_err(|error| error.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
        .map_err(TlsError)
}

/// Add the certificates of the PEM file at `path` to `roots`, in order, up to the first one that
/// can't be read, parsed or added, whose error is returned.
#[cfg(not(target_arch = "wasm32"))]
fn add_extra_certs(
    roots: &mut rustls::RootCertStore,
    path: &std::ffi::OsStr,
) -> Result<(), String> {
    let pem = std::fs::read(path).map_err(|error| error.to_string())?;
    for certificate in CertificateDer::pem_slice_iter(&pem) {
        let certificate = certificate.map_err(|error| error.to_string())?;
        roots.add(certificate).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The cryptography of the sessions, aws-lc-rs's.
#[cfg(not(target_arch = "wasm32"))]
fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

/// A TLS client session.
pub struct TlsClient {
    #[cfg(not(target_arch = "wasm32"))]
    connection: rustls::ClientConnection,
}

#[cfg(target_arch = "wasm32")]
impl TlsClient {
    /// Fail: sessions aren't supported on wasm.
    pub fn new(_server_name: &str) -> Result<Self, TlsError> {
        Err(TlsError("TLS isn't supported on WebAssembly".to_string()))
    }

    /// Nothing: no session exists on wasm.
    pub fn receive(
        &mut self,
        _ciphertext: &[u8],
        _plaintext: &mut Vec<u8>,
    ) -> Result<bool, TlsError> {
        Ok(false)
    }

    /// Nothing: no session exists on wasm.
    pub fn send(&mut self, _plaintext: &[u8]) -> Result<(), TlsError> {
        Ok(())
    }

    /// Nothing: no session exists on wasm.
    pub fn close(&mut self) {}

    /// Nothing: no session exists on wasm.
    pub fn take_outgoing(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl TlsClient {
    /// A session with the server `server_name`, a DNS name or an IP address, which starts with the
    /// client's handshake message in [`TlsClient::take_outgoing`].
    pub fn new(server_name: &str) -> Result<Self, TlsError> {
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|error| TlsError(format!("invalid server name `{server_name}`: {error}")))?;
        let mut connection = rustls::ClientConnection::new(config()?, name)?;
        connection.set_buffer_limit(None);
        Ok(TlsClient { connection })
    }

    /// Process `ciphertext`, the next bytes received from the server, and append the application
    /// data they decrypt to `plaintext`. Returns whether the server closed the session with a
    /// `close_notify` alert. Bytes after the alert are discarded. After an error,
    /// [`TlsClient::take_outgoing`] holds the alert for the error.
    pub fn receive(
        &mut self,
        mut ciphertext: &[u8],
        plaintext: &mut Vec<u8>,
    ) -> Result<bool, TlsError> {
        let mut closed = false;
        while !ciphertext.is_empty() {
            let read = self
                .connection
                .read_tls(&mut ciphertext)
                .map_err(|error| TlsError(error.to_string()))?;
            // `read_tls` reads nothing once the session received a `close_notify` alert.
            if read == 0 {
                break;
            }
            let state = self.connection.process_new_packets()?;
            let available = state.plaintext_bytes_to_read();
            plaintext
                .try_reserve(available)
                .map_err(|_| TlsError("out of memory".to_string()))?;
            let start = plaintext.len();
            plaintext.resize(start + available, 0);
            let mut filled = start;
            while filled < plaintext.len() {
                match std::io::Read::read(&mut self.connection.reader(), &mut plaintext[filled..]) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => filled += count,
                }
            }
            plaintext.truncate(filled);
            closed |= state.peer_has_closed();
        }
        Ok(closed)
    }

    /// Encrypt `plaintext` for the server. Before the handshake completes, it is kept until then.
    pub fn send(&mut self, plaintext: &[u8]) -> Result<(), TlsError> {
        std::io::Write::write_all(&mut self.connection.writer(), plaintext)
            .map_err(|error| TlsError(error.to_string()))
    }

    /// Queue the `close_notify` alert that ends the session.
    pub fn close(&mut self) {
        self.connection.send_close_notify();
    }

    /// The bytes to send to the server, which are removed from the session.
    pub fn take_outgoing(&mut self) -> Vec<u8> {
        let mut outgoing = Vec::new();
        while self.connection.wants_write() {
            if self.connection.write_tls(&mut outgoing).is_err() {
                break;
            }
        }
        outgoing
    }
}
