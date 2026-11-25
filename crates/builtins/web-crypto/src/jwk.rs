// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/webcrypto/#JsonWebKey-dictionary>, and the checks of a JWK that the
//! import key steps of several algorithms share.
//!
//! Every member is declared, though the implemented algorithms only read those of every key
//! (`kty`, `use`, `key_ops`, `alg` and `ext`), of a symmetric key (`k`) and of an Ed25519 key
//! (`crv`, `x` and `d`): converting the dictionary gets and converts all of them, which script
//! can observe.

use core_runtime::webidl_dictionary;
use web_globals::base64::forgiving_base64_decode;
use web_globals::dom_exception::DOMExceptionError;

use crate::crypto_key::KeyUsage;

/// <https://w3c.github.io/webcrypto/#JsonWebKey-dictionary>
#[webidl_dictionary]
pub struct JsonWebKey {
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-kty>
    pub kty: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-use>
    #[webidl(name = "use")]
    pub use_: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-key_ops>
    // (The default name would be "keyOps".)
    #[webidl(name = "key_ops")]
    pub key_ops: Option<Vec<String>>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-alg>
    pub alg: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-ext>
    pub ext: Option<bool>,
    // The following fields are defined in Section 6 of JSON Web Algorithms.
    // (Those without an `allow(dead_code)` are read by an implemented algorithm. The rest are
    // still converted.)
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-crv>
    pub crv: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-x>
    pub x: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-y>
    #[allow(dead_code)]
    pub y: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-d>
    pub d: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-n>
    #[allow(dead_code)]
    pub n: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-e>
    #[allow(dead_code)]
    pub e: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-p>
    #[allow(dead_code)]
    pub p: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-q>
    #[allow(dead_code)]
    pub q: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-dp>
    #[allow(dead_code)]
    pub dp: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-dq>
    #[allow(dead_code)]
    pub dq: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-qi>
    #[allow(dead_code)]
    pub qi: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-oth>
    #[allow(dead_code)]
    pub oth: Option<Vec<RsaOtherPrimesInfo>>,
    /// <https://w3c.github.io/webcrypto/#dom-jsonwebkey-k>
    pub k: Option<String>,
}

/// <https://w3c.github.io/webcrypto/#dfn-RsaOtherPrimesInfo>
#[webidl_dictionary]
#[allow(dead_code)]
pub struct RsaOtherPrimesInfo {
    /// <https://w3c.github.io/webcrypto/#dom-rsaotherprimesinfo-r>
    pub r: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-rsaotherprimesinfo-d>
    pub d: Option<String>,
    /// <https://w3c.github.io/webcrypto/#dom-rsaotherprimesinfo-t>
    pub t: Option<String>,
}

pub fn data_error(message: impl Into<String>) -> DOMExceptionError {
    DOMExceptionError::new("DataError", message)
}

/// Decode _value_, the JWK member _member_, as base64url: the URL- and filename-safe alphabet
/// without padding, as [JWS] Section 2 defines it for JWKs. Anything else, padding included, is
/// a `DataError`.
///
/// [JWS]: https://www.rfc-editor.org/rfc/rfc7515#section-2
pub fn decode_base64url(member: &str, value: &str) -> Result<Vec<u8>, DOMExceptionError> {
    let invalid = || data_error(format!("The JWK's \"{member}\" member is not base64url"));
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid());
    }
    // Map the alphabet to the standard one. What's left is what forgiving-base64 decodes: no
    // padding, so only a length of 1 modulo 4 fails.
    let standard: String = value
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    forgiving_base64_decode(&standard).map_err(|_| invalid())
}

/// The checks of _jwk_'s `use`, `key_ops` and `ext` fields that the import key steps of every
/// algorithm make, given the _extractable_ and _usages_ passed to `importKey()`. _use_ is the
/// `use` the algorithm's keys have.
pub fn check_use_key_ops_ext(
    jwk: &JsonWebKey,
    use_: &str,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<(), DOMExceptionError> {
    // If _usages_ is non-empty and the `use` field of _jwk_ is present and is not _use_, then
    // throw a `DataError`.
    if !usages.is_empty() && jwk.use_.as_deref().is_some_and(|u| u != use_) {
        return Err(data_error(format!(
            "The JWK's \"use\" member must be \"{use_}\""
        )));
    }
    // If the `key_ops` field of _jwk_ is present, and is invalid according to the requirements
    // of JSON Web Key [JWK] or does not contain all of the specified _usages_ values, then
    // throw a `DataError`.
    // (Of [JWK]'s requirements, Section 4.3's "Duplicate key operation values MUST NOT be
    // present in the array" is the one a sequence of strings can break.)
    if let Some(key_ops) = &jwk.key_ops {
        let repeated = key_ops
            .iter()
            .enumerate()
            .any(|(i, op)| key_ops[..i].contains(op));
        if repeated {
            return Err(data_error(
                "The JWK's \"key_ops\" member contains a value more than once",
            ));
        }
        if usages
            .iter()
            .any(|usage| !key_ops.iter().any(|op| op == usage.as_str()))
        {
            return Err(data_error(
                "The JWK's \"key_ops\" member doesn't contain all of the requested usages",
            ));
        }
    }
    // If the `ext` field of _jwk_ is present and has the value false and _extractable_ is true,
    // then throw a `DataError`.
    if jwk.ext == Some(false) && extractable {
        return Err(data_error(
            "The JWK's \"ext\" member is false, but the key is to be extractable",
        ));
    }
    Ok(())
}
