// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Base64 encoding and decoding globals: `btoa` and `atob`.
//!
//! These implement the WHATWG [Forgiving Base64] algorithms as global
//! functions, throwing `DOMException` with name `"InvalidCharacterError"`
//! on invalid input per the specification.
//!
//! [Forgiving Base64]: https://infra.spec.whatwg.org/#forgiving-base64

#[core_runtime::jsglobals]
pub mod base64_globals {
    use crate::dom_exception::DOMExceptionError;

    /// WHATWG `btoa(data)`: encode a byte string to base64.
    ///
    /// Coerces the argument to a string via `ToString`, then base64-encodes it.
    /// Throws `DOMException` with name `"InvalidCharacterError"` if the string
    /// contains any code point above U+00FF.
    pub fn btoa(data: String) -> Result<String, DOMExceptionError> {
        // <https://html.spec.whatwg.org/#dom-btoa>: The `btoa(data)` method must throw an
        // "``InvalidCharacterError``" ``DOMException`` if _data_ contains any character whose code
        // point is greater than U+00FF. Otherwise, the user agent must convert _data_ to a byte
        // sequence whose _n_th byte is the eight-bit representation of the _n_th code point of
        // _data_, and then must apply `forgiving-base64 encode` to that byte sequence and return
        // the result.
        super::btoa(&data).map_err(|msg| DOMExceptionError::new("InvalidCharacterError", msg))
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

/// The realm object marker of the `atob` function [`add_to_global`] installs.
struct AtobFunction;

/// The realm object marker of the `btoa` function [`add_to_global`] installs.
struct BtoaFunction;

/// Install `atob` and `btoa` on `global`.
pub fn add_to_global(scope: &js::gc::scope::Scope<'_>, global: js::Object<'_>) {
    base64_globals::add_to_global(scope, global);
    let installed = |name| {
        let value = global
            .get_property(scope, name)
            .expect("failed to read an installed base64 global");
        js::Object::from_value(scope, *value).expect("base64 globals are functions")
    };
    js::class::register_realm_object::<AtobFunction>(scope, installed(c"atob"));
    js::class::register_realm_object::<BtoaFunction>(scope, installed(c"btoa"));
}

/// The `atob` function [`add_to_global`] installed on the current global, even if the global's
/// `atob` property was reassigned since.
pub fn atob_function<'s>(scope: &'s js::gc::scope::Scope<'_>) -> Option<js::Object<'s>> {
    js::class::realm_object::<AtobFunction>(scope)
}

/// The `btoa` function [`add_to_global`] installed on the current global, even if the global's
/// `btoa` property was reassigned since.
pub fn btoa_function<'s>(scope: &'s js::gc::scope::Scope<'_>) -> Option<js::Object<'s>> {
    js::class::realm_object::<BtoaFunction>(scope)
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// `bytes` as base64: in the standard alphabet with `=` padding, or with `url`, in the URL-safe
/// alphabet without padding.
pub fn encode(bytes: &[u8], url: bool) -> String {
    const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let alphabet = if url { URL } else { STANDARD };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..=chunk.len() {
            out.push(alphabet[(n >> (18 - 6 * index)) as usize & 63] as char);
        }
        if !url {
            for _ in chunk.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// btoa implementation
// ---------------------------------------------------------------------------

/// WHATWG `btoa`: encode a byte string to base64.
///
/// The input string must contain only code points in the range U+0000..U+00FF
/// (i.e., it must be a "byte string"). If any code point exceeds U+00FF, an
/// `InvalidCharacterError` is thrown.
pub(crate) fn btoa(data: &str) -> Result<String, String> {
    // Validate that all characters are ≤ U+00FF (Latin-1 range).
    for ch in data.chars() {
        if ch as u32 > 0x00FF {
            return Err(
                "The string to be encoded contains characters outside of the Latin1 range."
                    .to_string(),
            );
        }
    }

    // Collect the Latin-1 byte values.
    let bytes: Vec<u8> = data.chars().map(|ch| ch as u8).collect();
    Ok(encode(&bytes, false))
}

// ---------------------------------------------------------------------------
// atob implementation
// ---------------------------------------------------------------------------

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

// Nothing platform-specific in these tests, so skip them on wasm32.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use crate::base64::{atob, btoa};

    // -- btoa tests --

    #[test]
    fn btoa_empty() {
        assert_eq!(btoa("").unwrap(), "");
    }

    #[test]
    fn btoa_basic() {
        assert_eq!(btoa("ab").unwrap(), "YWI=");
        assert_eq!(btoa("abc").unwrap(), "YWJj");
        assert_eq!(btoa("abcd").unwrap(), "YWJjZA==");
        assert_eq!(btoa("abcde").unwrap(), "YWJjZGU=");
    }

    #[test]
    fn btoa_latin1() {
        assert_eq!(btoa("\u{FF}\u{FF}\u{C0}").unwrap(), "///A");
    }

    #[test]
    fn btoa_null_bytes() {
        assert_eq!(btoa("\0a").unwrap(), "AGE=");
        assert_eq!(btoa("a\0b").unwrap(), "YQBi");
    }

    #[test]
    fn btoa_coerced_types() {
        // These test the JS-side coercion, but we can verify the Rust core:
        assert_eq!(btoa("undefined").unwrap(), "dW5kZWZpbmVk");
        assert_eq!(btoa("null").unwrap(), "bnVsbA==");
        assert_eq!(btoa("7").unwrap(), "Nw==");
        assert_eq!(btoa("true").unwrap(), "dHJ1ZQ==");
        assert_eq!(btoa("false").unwrap(), "ZmFsc2U=");
        assert_eq!(btoa("NaN").unwrap(), "TmFO");
    }

    #[test]
    fn btoa_rejects_non_latin1() {
        let result = btoa("עברית");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("outside of the Latin1 range"));
    }

    #[test]
    fn btoa_all_single_bytes() {
        // U+0000..U+00FF should all succeed
        for i in 0..=0xFF_u32 {
            let ch = char::from_u32(i).unwrap();
            let s = ch.to_string();
            assert!(btoa(&s).is_ok(), "btoa should accept U+{i:04X}");
        }
    }

    #[test]
    fn btoa_first_non_latin1() {
        // U+0100 should fail
        let ch = char::from_u32(0x100).unwrap();
        assert!(btoa(&ch.to_string()).is_err());
    }

    // -- atob tests --

    #[test]
    fn atob_empty() {
        assert_eq!(atob("").unwrap(), "");
    }

    #[test]
    fn atob_basic() {
        assert_eq!(atob("YWI=").unwrap(), "ab");
        assert_eq!(atob("YWJj").unwrap(), "abc");
        assert_eq!(atob("YWJjZA==").unwrap(), "abcd");
        assert_eq!(atob("YWJjZGU=").unwrap(), "abcde");
    }

    #[test]
    fn atob_no_padding() {
        // atob should work without padding too
        assert_eq!(atob("YWI").unwrap(), "ab");
        assert_eq!(atob("YWJjZA").unwrap(), "abcd");
    }

    #[test]
    fn atob_whitespace_stripping() {
        assert_eq!(atob(" Y W J j ").unwrap(), "abc");
        assert_eq!(atob("\tYWJj\n").unwrap(), "abc");
        assert_eq!(atob("\r\nYWJj\r\n").unwrap(), "abc");
    }

    #[test]
    fn atob_invalid_length() {
        // After stripping whitespace and removing padding, length % 4 == 1 is invalid
        let result = atob("A");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not correctly encoded"));
    }

    #[test]
    fn atob_invalid_chars() {
        let result = atob("====");
        assert!(result.is_err());
    }

    #[test]
    fn atob_roundtrip() {
        for s in &["", "a", "ab", "abc", "abcd", "hello world", "\0\x01\x7F"] {
            let encoded = btoa(s).unwrap();
            let decoded = atob(&encoded).unwrap();
            assert_eq!(*s, decoded, "roundtrip failed for {:?}", s);
        }
    }

    // -- Integration tests (require JS engine) --

    mod integration {
        use core_runtime::test_util::{eval_with_setup, throws_with_setup};

        fn eval(code: &str) -> String {
            eval_with_setup(
                || {
                    core_runtime::runtime::register_global_initializer(crate::add_to_global);
                },
                code,
            )
        }

        fn eval_throws(code: &str) -> bool {
            throws_with_setup(
                || {
                    core_runtime::runtime::register_global_initializer(crate::add_to_global);
                },
                code,
            )
        }

        #[test]
        fn js_btoa_basic() {
            assert_eq!(eval("btoa('abc')"), "YWJj");
            assert_eq!(eval("btoa('')"), "");
            assert_eq!(eval("btoa('abcde')"), "YWJjZGU=");
        }

        #[test]
        fn js_atob_basic() {
            assert_eq!(eval("atob('YWJj')"), "abc");
            assert_eq!(eval("atob('')"), "");
        }

        #[test]
        fn js_btoa_throws_on_non_latin1() {
            assert!(eval_throws("btoa('\\u0100')"));
        }

        #[test]
        fn js_btoa_throws_dom_exception() {
            // btoa must throw a DOMException with name "InvalidCharacterError"
            assert_eq!(
                eval(
                    "try { btoa('\\u0100'); 'no-throw' } catch(e) { \
                     (e instanceof DOMException) + ',' + e.name + ',' + e.code }"
                ),
                "true,InvalidCharacterError,5"
            );
        }

        #[test]
        fn js_atob_throws_on_invalid() {
            assert!(eval_throws("atob('====')"));
        }

        #[test]
        fn js_atob_throws_dom_exception() {
            // atob must throw a DOMException with name "InvalidCharacterError"
            assert_eq!(
                eval(
                    "try { atob('===='); 'no-throw' } catch(e) { \
                     (e instanceof DOMException) + ',' + e.name + ',' + e.code }"
                ),
                "true,InvalidCharacterError,5"
            );
        }

        #[test]
        fn js_roundtrip() {
            assert_eq!(eval("atob(btoa('hello'))"), "hello");
        }

        #[test]
        fn js_btoa_coercion() {
            // JS coerces arguments to string: btoa(undefined) encodes "undefined"
            assert_eq!(eval("btoa(undefined)"), "dW5kZWZpbmVk");
            assert_eq!(eval("btoa(null)"), "bnVsbA==");
            assert_eq!(eval("btoa(7)"), "Nw==");
            assert_eq!(eval("btoa(true)"), "dHJ1ZQ==");
        }
    }
}
