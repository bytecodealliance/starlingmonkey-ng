// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Cryptographically secure random bytes: the operating system's source natively, and
//! `wasi:random/random` on wasm.

/// Fill `bytes` with cryptographically secure random bytes.
///
/// # Panics
///
/// Natively, if the operating system provides no random source.
#[cfg(not(target_arch = "wasm32"))]
pub fn fill(bytes: &mut [u8]) {
    getrandom::fill(bytes).expect("the operating system provides random bytes");
}

/// Fill `bytes` with cryptographically secure random bytes, requested in chunks of at most
/// 64 KiB.
#[cfg(target_arch = "wasm32")]
pub fn fill(bytes: &mut [u8]) {
    const CHUNK: usize = 64 * 1024;
    let mut filled = 0;
    while filled < bytes.len() {
        let remaining = (bytes.len() - filled).min(CHUNK) as u64;
        let chunk = wasip3::random::random::get_random_bytes(remaining);
        let count = chunk.len().min(bytes.len() - filled);
        bytes[filled..filled + count].copy_from_slice(&chunk[..count]);
        filled += count;
    }
}

/// A version 4 UUID from 16 random bytes, in the lowercase hyphenated form.
pub fn uuid_v4() -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; 16];
    fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut uuid = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            uuid.push('-');
        }
        uuid.push(char::from(DIGITS[usize::from(byte >> 4)]));
        uuid.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    uuid
}
