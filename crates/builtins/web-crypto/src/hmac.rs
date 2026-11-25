// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The operations of HMAC.
//! <https://w3c.github.io/webcrypto/#hmac-operations>

use ::hmac::{EagerHash, Hmac, KeyInit, Mac};
use md5::Md5;
use sha1::Sha1;
use sha2::{Sha256, Sha384, Sha512};
use web_globals::dom_exception::DOMExceptionError;

use crate::algorithm::HashAlgorithm;
use crate::crypto_key::{ImportedKey, KeyData, KeyFormat, KeyMaterial, KeyType, KeyUsage};
use crate::jwk::{check_use_key_ops_ext, data_error, decode_base64url};

/// <https://w3c.github.io/webcrypto/#hmac-operations-import-key>, given the `hash` and
/// `length` members of _normalizedAlgorithm_, an `HmacImportParams`.
pub fn import_key(
    normalized_hash: HashAlgorithm,
    normalized_length: Option<u32>,
    format: KeyFormat,
    key_data: KeyData,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<ImportedKey, DOMExceptionError> {
    // Step 1: If the `length` member of _normalizedAlgorithm_ is present and is zero, then
    //         throw a `DataError`.
    if normalized_length == Some(0) {
        return Err(data_error("The key length must not be zero"));
    }
    // Step 2: Let _keyData_ be the key data to be imported.
    // Step 3: If _usages_ contains an entry which is not "sign" or "verify", then throw a
    //         `SyntaxError`.
    if usages
        .iter()
        .any(|usage| !matches!(usage, KeyUsage::Sign | KeyUsage::Verify))
    {
        return Err(DOMExceptionError::new(
            "SyntaxError",
            "An HMAC key can only be used to \"sign\" and \"verify\"",
        ));
    }
    // Step 4: Let _hash_ be a new `KeyAlgorithm`.
    // Step 5: (Each case below is introduced by its "If _format_ is" line.)
    let (data, hash) = match (format, key_data) {
        // If _format_ is "raw":
        (KeyFormat::Raw, KeyData::Bytes(data)) => {
            // Step 5.1: Let _data_ be _keyData_.
            // Step 5.2: Set _hash_ to equal the `hash` member of _normalizedAlgorithm_.
            (data, normalized_hash)
        }
        // If _format_ is "jwk":
        (KeyFormat::Jwk, key_data) => {
            // Step 5.1: If _keyData_ is a `JsonWebKey` dictionary: Let _jwk_ equal _keyData_.
            //           Otherwise: Throw a `DataError`.
            let KeyData::Jwk(jwk) = key_data else {
                return Err(data_error("The key data must be a JsonWebKey"));
            };
            // Step 5.2: If the `kty` field of _jwk_ is not "oct", then throw a `DataError`.
            if jwk.kty.as_deref() != Some("oct") {
                return Err(data_error("The JWK's \"kty\" member must be \"oct\""));
            }
            // Step 5.3: If _jwk_ does not meet the requirements of Section 6.4 of JSON Web
            //           Algorithms [JWA], then throw a `DataError`.
            //           (Section 6.4.1: a `k` member, required, holding the key as base64url.
            //           Decoding it here validates both.)
            let Some(k) = &jwk.k else {
                return Err(data_error("The JWK has no \"k\" member"));
            };
            let decoded_k = decode_base64url("k", k)?;
            // Step 5.4: Let _data_ be the byte sequence obtained by decoding the `k` field of
            //           _jwk_.
            let data = decoded_k;
            // Step 5.5: Set the _hash_ to equal the `hash` member of _normalizedAlgorithm_.
            let hash = normalized_hash;
            // Step 5.6: If the `name` attribute of _hash_ is "SHA-1": If the `alg` field of
            //           _jwk_ is present and is not "HS1", then throw a `DataError`.
            //           If the `name` attribute of _hash_ is "SHA-256": If the `alg` field of
            //           _jwk_ is present and is not "HS256", then throw a `DataError`.
            //           If the `name` attribute of _hash_ is "SHA-384": If the `alg` field of
            //           _jwk_ is present and is not "HS384", then throw a `DataError`.
            //           If the `name` attribute of _hash_ is "SHA-512": If the `alg` field of
            //           _jwk_ is present and is not "HS512", then throw a `DataError`.
            //           Otherwise, if the `name` attribute of _hash_ is defined in another
            //           applicable specification: Perform any key import steps defined by other
            //           applicable specifications, passing _format_, _jwk_ and _hash_ and
            //           obtaining _hash_.
            //           (The only other hash function supported is MD5, which no specification
            //           defines JWK import steps or an `alg` value for. So, as in the C++
            //           StarlingMonkey, an HMAC-MD5 key can't be imported as a JWK.)
            let expected_alg = match hash {
                HashAlgorithm::Md5 => {
                    return Err(data_error(
                        "An HMAC key using MD5 can't be imported as a JWK",
                    ));
                }
                HashAlgorithm::Sha1 => "HS1",
                HashAlgorithm::Sha256 => "HS256",
                HashAlgorithm::Sha384 => "HS384",
                HashAlgorithm::Sha512 => "HS512",
            };
            if jwk.alg.as_deref().is_some_and(|alg| alg != expected_alg) {
                return Err(data_error(format!(
                    "The JWK's \"alg\" member must be \"{expected_alg}\" for {}",
                    hash.name()
                )));
            }
            // Step 5.7: If _usages_ is non-empty and the `use` field of _jwk_ is present and
            //           is not "sig", then throw a `DataError`.
            // Step 5.8: If the `key_ops` field of _jwk_ is present, and is invalid according to
            //           the requirements of JSON Web Key [JWK] or does not contain all of the
            //           specified _usages_ values, then throw a `DataError`.
            // Step 5.9: If the `ext` field of _jwk_ is present and has the value false and
            //           _extractable_ is true, then throw a `DataError`.
            check_use_key_ops_ext(&jwk, "sig", extractable, usages)?;
            (data, hash)
        }
        // (`importKey()` makes _keyData_ a `JsonWebKey` exactly for "jwk", so "raw" always has
        // bytes.)
        (KeyFormat::Raw, KeyData::Jwk(_)) => unreachable!("\"raw\" key data is bytes"),
        // Otherwise: throw a `NotSupportedError`.
        (KeyFormat::Spki | KeyFormat::Pkcs8, _) => {
            return Err(DOMExceptionError::new(
                "NotSupportedError",
                format!("HMAC keys can't be imported in the \"{format}\" format"),
            ));
        }
    };
    // Step 6: Let _length_ be the length in bits of _data_.
    let data_length = data.len() as u64 * 8;
    // Step 7: If _length_ is zero then throw a `DataError`.
    if data_length == 0 {
        return Err(data_error("The key data must not be empty"));
    }
    // Step 8: If the `length` member of _normalizedAlgorithm_ is present:
    //           If the `length` member of _normalizedAlgorithm_ is greater than _length_: throw
    //           a `DataError`.
    //           If the `length` member of _normalizedAlgorithm_, is less than or equal to
    //           _length_ minus eight: throw a `DataError`.
    //           Otherwise: Set _length_ equal to the `length` member of _normalizedAlgorithm_.
    let length = match normalized_length.map(u64::from) {
        Some(length) if length > data_length => {
            return Err(data_error("The key length is longer than the key data"));
        }
        Some(length) if length <= data_length - 8 => {
            return Err(data_error(
                "The key length is shorter than the key data by a byte or more",
            ));
        }
        Some(length) => length,
        None => data_length,
    };
    // Step 9: Let _key_ be a new `CryptoKey` object representing an HMAC key with the first
    //         _length_ bits of _data_.
    //         (_length_ is within 8 bits of _data_'s length, so only the unused low bits of
    //         the last byte, if any, are dropped. They are set to zero.)
    let mut key = data;
    let unused_bits = (data_length - length) as u32;
    if let Some(last) = key.last_mut() {
        *last &= 0xffu8.checked_shl(unused_bits).unwrap_or(0);
    }
    // Step 10: Set the `[[type]]` internal slot of _key_ to "secret".
    // Step 11: Let _algorithm_ be a new `HmacKeyAlgorithm`.
    // Step 12: Set the `name` attribute of _algorithm_ to "HMAC".
    // Step 13: Set the `length` attribute of _algorithm_ to _length_.
    // Step 14: Set the `hash` attribute of _algorithm_ to _hash_.
    // Step 15: Set the `[[algorithm]]` internal slot of _key_ to _algorithm_.
    // Step 16: Return _key_.
    Ok(ImportedKey {
        key_type: KeyType::Secret,
        material: KeyMaterial::Hmac { hash, length, key },
    })
}

fn new_mac<D: EagerHash>(key: &[u8], message: &[u8]) -> Hmac<D> {
    let mut mac = <Hmac<D> as KeyInit>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(message);
    mac
}

/// <https://w3c.github.io/webcrypto/#hmac-operations-sign>, for the key with _hash_ and _key_
/// as the `hash` of its `[[algorithm]]` and its `[[handle]]`.
pub fn sign(hash: HashAlgorithm, key: &[u8], message: &[u8]) -> Vec<u8> {
    // Step 1: Let _mac_ be the result of performing the HMAC function described in Section 2 of
    //         [NIST-SP800-224] using the key represented by the `[[handle]]` internal slot of
    //         _key_, the hash function identified by the `hash` attribute of the `[[algorithm]]`
    //         internal slot of _key_ and _message_ as the message M.
    // Step 2: Return _mac_.
    fn mac<D: EagerHash>(key: &[u8], message: &[u8]) -> Vec<u8> {
        new_mac::<D>(key, message).finalize().into_bytes().to_vec()
    }
    match hash {
        HashAlgorithm::Md5 => mac::<Md5>(key, message),
        HashAlgorithm::Sha1 => mac::<Sha1>(key, message),
        HashAlgorithm::Sha256 => mac::<Sha256>(key, message),
        HashAlgorithm::Sha384 => mac::<Sha384>(key, message),
        HashAlgorithm::Sha512 => mac::<Sha512>(key, message),
    }
}

/// <https://w3c.github.io/webcrypto/#hmac-operations-verify>, for the key with _hash_ and _key_
/// as the `hash` of its `[[algorithm]]` and its `[[handle]]`.
pub fn verify(hash: HashAlgorithm, key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    // Step 1: Let _mac_ be the result of performing the HMAC function described in Section 2 of
    //         [NIST-SP800-224] using the key represented by the `[[handle]]` internal slot of
    //         _key_, the hash function identified by the `hash` attribute of the `[[algorithm]]`
    //         internal slot of _key_ and _message_ as the message M.
    // Step 2: Return true if _mac_ is equal to _signature_ and false otherwise. This comparison
    //         must be performed in constant-time.
    //         (`verify_slice` compares in constant time.)
    fn matches<D: EagerHash>(key: &[u8], message: &[u8], signature: &[u8]) -> bool {
        new_mac::<D>(key, message).verify_slice(signature).is_ok()
    }
    match hash {
        HashAlgorithm::Md5 => matches::<Md5>(key, message, signature),
        HashAlgorithm::Sha1 => matches::<Sha1>(key, message, signature),
        HashAlgorithm::Sha256 => matches::<Sha256>(key, message, signature),
        HashAlgorithm::Sha384 => matches::<Sha384>(key, message, signature),
        HashAlgorithm::Sha512 => matches::<Sha512>(key, message, signature),
    }
}
