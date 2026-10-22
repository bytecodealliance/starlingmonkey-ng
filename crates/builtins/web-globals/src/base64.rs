// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Base64 encoding and decoding globals: `btoa` and `atob`.
//!
//! These implement the WHATWG [Forgiving Base64] algorithms as global
//! functions, throwing `DOMException` with name `"InvalidCharacterError"`
//! on invalid input per the specification.
//!
//! [Forgiving Base64]: https://infra.spec.whatwg.org/#forgiving-base64

use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;

use crate::dom_exception::DOMExceptionError;

#[core_runtime::jsglobals]
pub mod base64_globals {
    use super::BtoaError;
    use crate::dom_exception::DOMExceptionError;

    /// WHATWG `btoa(data)`: encode a byte string to base64.
    ///
    /// Coerces the argument to a string via `ToString`, then base64-encodes it.
    /// Throws `DOMException` with name `"InvalidCharacterError"` if the string
    /// contains any code point above U+00FF, and reports out of memory if the
    /// encoded string can't be allocated.
    pub fn btoa(data: String) -> Result<String, BtoaError> {
        // <https://html.spec.whatwg.org/#dom-btoa>: The `btoa(data)` method must throw an
        // "``InvalidCharacterError``" ``DOMException`` if _data_ contains any character whose code
        // point is greater than U+00FF. Otherwise, the user agent must convert _data_ to a byte
        // sequence whose _n_th byte is the eight-bit representation of the _n_th code point of
        // _data_, and then must apply `forgiving-base64 encode` to that byte sequence and return
        // the result.
        super::btoa(&data)
    }

    /// WHATWG `atob(data)`: forgiving-base64 decode.
    ///
    /// Coerces the argument to a string via `ToString`, then forgiving-base64
    /// decodes it. Throws `DOMException` with name `"InvalidCharacterError"` if
    /// the input is not valid base64.
    pub fn atob(data: String) -> Result<String, DOMExceptionError> {
        // <https://html.spec.whatwg.org/#dom-atob>
        // Step 1: Let _decodedData_ be the result of running `forgiving-base64 decode` on _data_.
        // Step 2: If _decodedData_ is failure, then throw an "``InvalidCharacterError``"
        //         ``DOMException``.
        // Step 3: Return _decodedData_.
        super::atob(&data).map_err(|msg| DOMExceptionError::new("InvalidCharacterError", msg))
    }
}

/// Why [`btoa`] failed.
#[derive(Debug, PartialEq)]
pub enum BtoaError {
    /// The input contains a code point above U+00FF.
    InvalidCharacter,
    /// Allocating the encoded text failed.
    OutOfMemory,
}

impl ThrowException for BtoaError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        match self {
            BtoaError::InvalidCharacter => DOMExceptionError::new(
                "InvalidCharacterError",
                "The string to be encoded contains characters outside of the Latin1 range.",
            )
            .throw(scope),
            BtoaError::OutOfMemory => js::error::OutOfMemory.throw(scope),
        }
    }
}

/// WHATWG `btoa`: encode a byte string to base64.
///
/// The input string must contain only code points in the range U+0000..U+00FF
/// (i.e., it must be a "byte string"). If any code point exceeds U+00FF, an
/// `InvalidCharacterError` is thrown.
pub(crate) fn btoa(data: &str) -> Result<String, BtoaError> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    // Validate that all characters are ≤ U+00FF (Latin-1 range).
    let mut length = 0usize;
    for ch in data.chars() {
        if ch as u32 > 0x00FF {
            return Err(BtoaError::InvalidCharacter);
        }
        length += 1;
    }

    // Encode the Latin-1 byte values with `=` padding.
    let mut output = String::new();
    let encoded_length = length
        .div_ceil(3)
        .checked_mul(4)
        .ok_or(BtoaError::OutOfMemory)?;
    output
        .try_reserve_exact(encoded_length)
        .map_err(|_| BtoaError::OutOfMemory)?;
    let mut bytes = data.chars().map(|ch| ch as u8);
    while let Some(b0) = bytes.next() {
        let b1 = bytes.next();
        let b2 = bytes.next();
        let triple =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));

        output.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        output.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        output.push(match b1 {
            Some(_) => ALPHABET[((triple >> 6) & 0x3F) as usize] as char,
            None => '=',
        });
        output.push(match b2 {
            Some(_) => ALPHABET[(triple & 0x3F) as usize] as char,
            None => '=',
        });
    }
    Ok(output)
}

/// WHATWG `atob`: forgiving-base64 decode.
///
/// Forgiving-base64 decodes `data` and returns the decoded byte string as a
/// Latin-1 string (each byte maps directly to a code point in U+0000..U+00FF).
pub(crate) fn atob(data: &str) -> Result<String, String> {
    let bytes = forgiving_base64_decode(data)?;
    // Convert bytes to a Latin-1 string. In UTF-8 encoding, some non-ASCII characters
    // have multi-byte representantation, so we need to loop over the vec instead of
    // using `String::from_utf8_unchecked`.
    Ok(bytes.into_iter().map(|b| b as char).collect())
}

/// WHATWG [forgiving-base64 decode]: decode `data` to its byte sequence.
///
/// Delegates to the `data-url` crate's implementation of the algorithm. Its
/// input is bytes rather than code points, but the outcome is the same: any
/// non-ASCII code point is outside the base64 alphabet and its UTF-8 bytes are
/// too, so both fail on the same inputs.
///
/// [forgiving-base64 decode]: https://infra.spec.whatwg.org/#forgiving-base64-decode
pub fn forgiving_base64_decode(data: &str) -> Result<Vec<u8>, String> {
    data_url::forgiving_base64::decode_to_vec(data.as_bytes())
        .map_err(|_| "The string to be decoded is not correctly encoded.".to_string())
}
