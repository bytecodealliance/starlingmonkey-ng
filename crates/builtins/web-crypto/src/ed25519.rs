// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The operations of Ed25519.
//! <https://w3c.github.io/webcrypto/#ed25519-operations>

use ed25519_dalek::pkcs8::spki::der::asn1::OctetStringRef;
use ed25519_dalek::pkcs8::spki::der::Decode;
use ed25519_dalek::pkcs8::spki::SubjectPublicKeyInfoRef;
use ed25519_dalek::pkcs8::{PrivateKeyInfoRef, ALGORITHM_OID};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use web_globals::dom_exception::DOMExceptionError;

use crate::crypto_key::{ImportedKey, KeyData, KeyFormat, KeyMaterial, KeyType, KeyUsage};
use crate::jwk::{check_use_key_ops_ext, data_error, decode_base64url, JsonWebKey};

/// The `[[handle]]` of an Ed25519 key.
pub enum Ed25519Key {
    /// A public key: its 32-byte encoding, as imported. That need not be a valid point, as no
    /// import format checks it: verifying with such a key returns false.
    Public([u8; 32]),
    /// A private key, with the public key derived from it.
    Private(Box<SigningKey>),
}

impl Ed25519Key {
    /// The key's `[[type]]`.
    fn key_type(&self) -> KeyType {
        match self {
            Self::Public(_) => KeyType::Public,
            Self::Private(_) => KeyType::Private,
        }
    }
}

fn syntax_error(message: &str) -> DOMExceptionError {
    DOMExceptionError::new("SyntaxError", message)
}

/// Whether _usages_ contains only _usage_.
fn only(usages: &[KeyUsage], usage: KeyUsage) -> bool {
    usages.iter().all(|&u| u == usage)
}

/// The 32 bytes of an Ed25519 key, or a `DataError` if _bytes_ is any other length.
fn key_bytes(bytes: &[u8]) -> Result<[u8; 32], DOMExceptionError> {
    bytes
        .try_into()
        .map_err(|_| data_error("An Ed25519 key must be 32 bytes"))
}

/// <https://w3c.github.io/webcrypto/#ed25519-operations-import-key>
pub fn import_key(
    format: KeyFormat,
    key_data: KeyData,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<ImportedKey, DOMExceptionError> {
    // Step 1: Let _keyData_ be the key data to be imported.
    // Step 2: (Each case below is introduced by its "If _format_ is" line.)
    let key = match (format, key_data) {
        // If _format_ is "spki":
        (KeyFormat::Spki, KeyData::Bytes(key_data)) => {
            // Step 2.1: If _usages_ contains an entry which is not "verify" then throw a
            //           `SyntaxError`.
            if !only(usages, KeyUsage::Verify) {
                return Err(syntax_error(
                    "An Ed25519 public key can only be used to \"verify\"",
                ));
            }
            // Step 2.2: Let _spki_ be the result of running the `parse a subjectPublicKeyInfo`
            //           algorithm over _keyData_.
            // Step 2.3: If an error occurred while parsing, then throw a `DataError`.
            //           (`from_der` fails if any bytes are left over, as _exactData_ is true.)
            let spki = SubjectPublicKeyInfoRef::from_der(&key_data)
                .map_err(|_| data_error("The key data is not a valid SubjectPublicKeyInfo"))?;
            // Step 2.4: If the algorithm object identifier field of the algorithm
            //           AlgorithmIdentifier field of _spki_ is not equal to the `id-Ed25519`
            //           object identifier defined in [RFC8410], then throw a `DataError`.
            if spki.algorithm.oid != ALGORITHM_OID {
                return Err(data_error("The key data is not an Ed25519 public key"));
            }
            // Step 2.5: If the parameters field of the algorithm AlgorithmIdentifier field of
            //           _spki_ is present, then throw a `DataError`.
            if spki.algorithm.parameters.is_some() {
                return Err(data_error(
                    "The key data's AlgorithmIdentifier must not have parameters",
                ));
            }
            // Step 2.6: Let _publicKey_ be the Ed25519 public key identified by the
            //           subjectPublicKey field of _spki_.
            //           (Its 32 bytes, with no unused bits. The spec doesn't say what to do with
            //           a subjectPublicKey of any other length; it identifies no Ed25519 public
            //           key, so this throws a `DataError`, like the "raw" format's length check.)
            let public_key = spki
                .subject_public_key
                .as_bytes()
                .ok_or_else(|| data_error("An Ed25519 key must be 32 bytes"))
                .and_then(key_bytes)?;
            // Step 2.7: Let _key_ be a new `CryptoKey` that represents _publicKey_.
            // Step 2.8: Set the `[[type]]` internal slot of _key_ to "public"
            // Step 2.9: Let _algorithm_ be a new `KeyAlgorithm`.
            // Step 2.10: Set the `name` attribute of _algorithm_ to "Ed25519".
            // Step 2.11: Set the `[[algorithm]]` internal slot of _key_ to _algorithm_.
            //            (`KeyMaterial::Ed25519` stands for that `KeyAlgorithm`.)
            Ed25519Key::Public(public_key)
        }
        // If _format_ is "pkcs8":
        (KeyFormat::Pkcs8, KeyData::Bytes(key_data)) => {
            // Step 2.1: If _usages_ contains an entry which is not "sign" then throw a
            //           `SyntaxError`.
            if !only(usages, KeyUsage::Sign) {
                return Err(syntax_error(
                    "An Ed25519 private key can only be used to \"sign\"",
                ));
            }
            // Step 2.2: Let _privateKeyInfo_ be the result of running the `parse a
            //           privateKeyInfo` algorithm over _keyData_.
            // Step 2.3: If an error occurs while parsing, then throw a `DataError`.
            //           (`from_der` fails if any bytes are left over, as _exactData_ is true.
            //           It also accepts a version 2 OneAsymmetricKey [RFC5958], whose optional
            //           publicKey field is ignored rather than checked against the private key:
            //           the spec names the version 1 structure of [RFC5208] and says nothing about
            //           a public key in it.)
            let private_key_info = PrivateKeyInfoRef::from_der(&key_data)
                .map_err(|_| data_error("The key data is not a valid PrivateKeyInfo"))?;
            // Step 2.4: If the algorithm object identifier field of the privateKeyAlgorithm
            //           PrivateKeyAlgorithm field of _privateKeyInfo_ is not equal to the
            //           `id-Ed25519` object identifier defined in [RFC8410], then throw a
            //           `DataError`.
            if private_key_info.algorithm.oid != ALGORITHM_OID {
                return Err(data_error("The key data is not an Ed25519 private key"));
            }
            // Step 2.5: If the parameters field of the privateKeyAlgorithm
            //           PrivateKeyAlgorithmIdentifier field of _privateKeyInfo_ is present,
            //           then throw a `DataError`.
            if private_key_info.algorithm.parameters.is_some() {
                return Err(data_error(
                    "The key data's AlgorithmIdentifier must not have parameters",
                ));
            }
            // Step 2.6: Let _curvePrivateKey_ be the result of performing the `parse an ASN.1
            //           structure` algorithm, with _data_ as the privateKey field of
            //           _privateKeyInfo_, _structure_ as the ASN.1 CurvePrivateKey structure
            //           specified in Section 7 of [RFC8410], and _exactData_ set to true.
            //           (CurvePrivateKey is an OCTET STRING.)
            // Step 2.7: If an error occurred while parsing, then throw a `DataError`.
            let curve_private_key = <&OctetStringRef>::from_der(
                private_key_info.private_key.as_bytes(),
            )
            .map_err(|_| data_error("The key data's privateKey is not a CurvePrivateKey"))?;
            // Step 2.8: Let _key_ be a new `CryptoKey` that represents the Ed25519 private key
            //           identified by _curvePrivateKey_.
            let secret_key = key_bytes(curve_private_key.as_bytes())?;
            // Step 2.9: Set the `[[type]]` internal slot of _key_ to "private"
            // Step 2.10: Let _algorithm_ be a new `KeyAlgorithm`.
            // Step 2.11: Set the `name` attribute of _algorithm_ to "Ed25519".
            // Step 2.12: Set the `[[algorithm]]` internal slot of _key_ to _algorithm_.
            //            (`KeyMaterial::Ed25519` stands for that `KeyAlgorithm`.)
            Ed25519Key::Private(Box::new(SigningKey::from_bytes(&secret_key)))
        }
        // If _format_ is "jwk":
        (KeyFormat::Jwk, key_data) => {
            // Step 2.1: If _keyData_ is a `JsonWebKey` dictionary: Let _jwk_ equal _keyData_.
            //           Otherwise: Throw a `DataError`.
            let KeyData::Jwk(jwk) = key_data else {
                return Err(data_error("The key data must be a JsonWebKey"));
            };
            import_jwk(&jwk, extractable, usages)?
        }
        // If _format_ is "raw":
        (KeyFormat::Raw, KeyData::Bytes(key_data)) => {
            // Step 2.1: If _usages_ contains an entry which is not "verify" then throw a
            //           `SyntaxError`.
            if !only(usages, KeyUsage::Verify) {
                return Err(syntax_error(
                    "An Ed25519 public key can only be used to \"verify\"",
                ));
            }
            // Step 2.2: If the length in bits of _keyData_ is not 256 then throw a `DataError`.
            let public_key = key_bytes(&key_data)?;
            // Step 2.3: Let _algorithm_ be a new `KeyAlgorithm` object.
            // Step 2.4: Set the `name` attribute of _algorithm_ to "Ed25519".
            // Step 2.5: Let _key_ be a new `CryptoKey` representing the key data provided in
            //           _keyData_.
            // Step 2.6: Set the `[[type]]` internal slot of _key_ to "public"
            // Step 2.7: Set the `[[algorithm]]` internal slot of _key_ to _algorithm_.
            //           (`KeyMaterial::Ed25519` stands for that `KeyAlgorithm`.)
            Ed25519Key::Public(public_key)
        }
        // (`importKey()` makes _keyData_ a `JsonWebKey` exactly for "jwk", so the other
        // formats always have bytes.)
        (KeyFormat::Spki | KeyFormat::Pkcs8 | KeyFormat::Raw, KeyData::Jwk(_)) => {
            unreachable!("only \"jwk\" key data is a JsonWebKey")
        }
    };
    // (No "Otherwise: throw a `NotSupportedError`" case: Ed25519 supports every `KeyFormat`.)
    // Step 3: Return _key_.
    Ok(ImportedKey {
        key_type: key.key_type(),
        material: KeyMaterial::Ed25519(key),
    })
}

/// The "jwk" case of <https://w3c.github.io/webcrypto/#ed25519-operations-import-key>, from step
/// 2.2, given _jwk_.
fn import_jwk(
    jwk: &JsonWebKey,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<Ed25519Key, DOMExceptionError> {
    // Step 2.2: If the `d` field is present and _usages_ contains a value which is not "sign",
    //           or, if the `d` field is not present and _usages_ contains a value which is not
    //           "verify" then throw a `SyntaxError`.
    if jwk.d.is_some() && !only(usages, KeyUsage::Sign) {
        return Err(syntax_error(
            "An Ed25519 private key can only be used to \"sign\"",
        ));
    }
    if jwk.d.is_none() && !only(usages, KeyUsage::Verify) {
        return Err(syntax_error(
            "An Ed25519 public key can only be used to \"verify\"",
        ));
    }
    // Step 2.3: If the `kty` field of _jwk_ is not "OKP", then throw a `DataError`.
    if jwk.kty.as_deref() != Some("OKP") {
        return Err(data_error("The JWK's \"kty\" member must be \"OKP\""));
    }
    // Step 2.4: If the `crv` field of _jwk_ is not "Ed25519", then throw a `DataError`.
    if jwk.crv.as_deref() != Some("Ed25519") {
        return Err(data_error("The JWK's \"crv\" member must be \"Ed25519\""));
    }
    // Step 2.5: If the `alg` field of _jwk_ is present and is not "Ed25519" or "EdDSA", then
    //           throw a `DataError`.
    if jwk
        .alg
        .as_deref()
        .is_some_and(|alg| alg != "Ed25519" && alg != "EdDSA")
    {
        return Err(data_error(
            "The JWK's \"alg\" member must be \"Ed25519\" or \"EdDSA\"",
        ));
    }
    // Step 2.6: If _usages_ is non-empty and the `use` field of _jwk_ is present and is not
    //           "sig", then throw a `DataError`.
    // Step 2.7: If the `key_ops` field of _jwk_ is present, and is invalid according to the
    //           requirements of JSON Web Key [JWK], or it does not contain all of the specified
    //           _usages_ values, then throw a `DataError`.
    // Step 2.8: If the `ext` field of _jwk_ is present and has the value false and
    //           _extractable_ is true, then throw a `DataError`.
    check_use_key_ops_ext(jwk, "sig", extractable, usages)?;
    // (Section 2 of [RFC8037]: the `x` member, required for both formats, is the public key,
    // and the `d` member the private key, each base64url-encoded. These are the requirements
    // steps 2.9.1 check, made before the branch so both share them. A missing `x` is a
    // `DataError`, as WPT's okp_importKey_failures tests expect.)
    let Some(x) = &jwk.x else {
        return Err(data_error("The JWK has no \"x\" member"));
    };
    let public_key = key_bytes(&decode_base64url("x", x)?)?;
    // Step 2.9: If the `d` field is present:
    let key = if let Some(d) = &jwk.d {
        // Step 2.9.1: If _jwk_ does not meet the requirements of the JWK private key format
        //             described in Section 2 of [RFC8037], then throw a `DataError`.
        //             (Besides the above, `x` must be the public key of `d`. RFC 8037 doesn't
        //             say so outright, but a key whose halves don't match isn't a valid key pair,
        //             and WPT's okp_importKey_failures tests expect a `DataError` for it.)
        let signing_key = SigningKey::from_bytes(&key_bytes(&decode_base64url("d", d)?)?);
        if signing_key.verifying_key().to_bytes() != public_key {
            return Err(data_error(
                "The JWK's \"x\" member is not the public key of its \"d\" member",
            ));
        }
        // Step 2.9.2: Let _key_ be a new `CryptoKey` object that represents the Ed25519 private
        //             key identified by interpreting _jwk_ according to Section 2 of
        //             [RFC8037].
        // Step 2.9.3: Set the `[[type]]` internal slot of _Key_ to "private".
        Ed25519Key::Private(Box::new(signing_key))
    } else {
        // Otherwise:
        // Step 2.9.1: If _jwk_ does not meet the requirements of the JWK public key format
        //             described in Section 2 of [RFC8037], then throw a `DataError`.
        //             (Checked above.)
        // Step 2.9.2: Let _key_ be a new `CryptoKey` object that represents the Ed25519 public
        //             key identified by interpreting _jwk_ according to Section 2 of [RFC8037].
        // Step 2.9.3: Set the `[[type]]` internal slot of _Key_ to "public".
        Ed25519Key::Public(public_key)
    };
    // Step 2.10: Let _algorithm_ be a new instance of a `KeyAlgorithm` object.
    // Step 2.11: Set the `name` attribute of _algorithm_ to "Ed25519".
    // Step 2.12: Set the `[[algorithm]]` internal slot of _key_ to _algorithm_.
    //            (`KeyMaterial::Ed25519` stands for that `KeyAlgorithm`.)
    Ok(key)
}

/// <https://w3c.github.io/webcrypto/#ed25519-operations-sign>, for the key with _key_ as its
/// `[[handle]]`.
pub fn sign(key: &Ed25519Key, message: &[u8]) -> Result<Vec<u8>, DOMExceptionError> {
    // Step 1: If the `[[type]]` internal slot of _key_ is not "private", then throw an
    //         `InvalidAccessError`.
    let Ed25519Key::Private(signing_key) = key else {
        return Err(DOMExceptionError::new(
            "InvalidAccessError",
            "Signing needs an Ed25519 private key",
        ));
    };
    // Step 2: Let _result_ be the result of performing the Ed25519 signing process, as
    //         specified in [RFC8032], Section 5.1.6, with _message_ as _M_, using the Ed25519
    //         private key associated with _key_.
    // Step 3: Return _result_.
    Ok(signing_key.sign(message).to_bytes().to_vec())
}

/// <https://w3c.github.io/webcrypto/#ed25519-operations-verify>, for the key with _key_ as its
/// `[[handle]]`.
pub fn verify(
    key: &Ed25519Key,
    message: &[u8],
    signature: &[u8],
) -> Result<bool, DOMExceptionError> {
    // Step 1: If the `[[type]]` internal slot of _key_ is not "public", then throw an
    //         `InvalidAccessError`.
    let Ed25519Key::Public(public_key) = key else {
        return Err(DOMExceptionError::new(
            "InvalidAccessError",
            "Verifying needs an Ed25519 public key",
        ));
    };
    // Step 2: If the key data of _key_ represents an invalid point or a small-order element on
    //         the Elliptic Curve of Ed25519, return false.
    //         (`from_bytes` rejects an invalid point, and `verify_strict` a small-order one.)
    let Ok(verifying_key) = VerifyingKey::from_bytes(public_key) else {
        return Ok(false);
    };
    // Step 3: If the point R, encoded in the first half of _signature_, represents an invalid
    //         point or a small-order element on the Elliptic Curve of Ed25519, return false.
    //         (Checked by `verify_strict`. A signature that isn't 64 bytes isn't valid.)
    let Ok(signature) = Signature::from_slice(signature) else {
        return Ok(false);
    };
    // Step 4: Perform the Ed25519 verification steps, as specified in [RFC8032], Section 5.1.7,
    //         using the cofactorless (unbatched) equation, `[S]B = R + [k]A'`, on the
    //         _signature_, with _message_ as _M_, using the Ed25519 public key associated with
    //         _key_.
    //         (`verify_strict` uses the cofactorless equation.)
    // Step 5: Let _result_ be a boolean with the value true if the signature is valid and the
    //         value false otherwise.
    // Step 6: Return _result_.
    Ok(verifying_key.verify_strict(message, &signature).is_ok())
}
