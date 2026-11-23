// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Cryptographically secure random bytes.
//!
//! Natively they come from the operating system, through the `getrandom` crate. On WASI they come
//! from `wasi:random/random`'s `get-random-bytes`, which fills a buffer in one call. `getrandom`
//! can't be used there: it calls `get-random-u64` once per eight bytes.

use std::fmt;

/// Why [`fill`] couldn't produce random bytes.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Fill `buf` with cryptographically secure random bytes.
#[cfg(not(target_arch = "wasm32"))]
pub fn fill(buf: &mut [u8]) -> Result<(), Error> {
    getrandom::fill(buf).map_err(|e| Error(e.to_string()))
}

/// Fill `buf` with cryptographically secure random bytes.
#[cfg(target_arch = "wasm32")]
pub fn fill(buf: &mut [u8]) -> Result<(), Error> {
    let mut filled = 0;
    while filled < buf.len() {
        // The p2 host returns exactly the length asked for. The p3 one may return fewer bytes.
        let bytes = host_random_bytes((buf.len() - filled) as u64);
        if bytes.is_empty() {
            return Err(Error("the host returned no random bytes".into()));
        }
        // A well-behaved host never returns more than asked for, but don't trust it to.
        let count = bytes.len().min(buf.len() - filled);
        buf[filled..filled + count].copy_from_slice(&bytes[..count]);
        filled += count;
    }
    Ok(())
}

#[cfg(target_env = "p2")]
fn host_random_bytes(len: u64) -> Vec<u8> {
    wasip2::random::random::get_random_bytes(len)
}

#[cfg(target_env = "p3")]
fn host_random_bytes(max_len: u64) -> Vec<u8> {
    wasip3::random::random::get_random_bytes(max_len)
}
