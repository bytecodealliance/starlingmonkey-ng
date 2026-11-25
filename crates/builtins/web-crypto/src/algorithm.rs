// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Algorithm identifiers and
//! <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>, for the
//! operations implemented so far: "digest", "importKey", "sign" and "verify". (Numbers like
//! "Step 2.5" count the items of the spec's nested list in order.)

use core_runtime::{webidl_dictionary, webidl_union};
use js::conversion::{EnforceRange, FromJSVal, ToJSVal};
use js::error::{throw_type_error, ExnThrown};
use js::gc::scope::Scope;
use js::Object;
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use web_globals::dom_exception::throw_dom_exception;

/// <https://w3c.github.io/webcrypto/#dfn-Algorithm>
#[webidl_dictionary]
pub struct Algorithm {
    pub name: String,
}

/// <https://w3c.github.io/webcrypto/#dfn-AlgorithmIdentifier>
///
/// `(object or DOMString)`. An object stands for an [`Algorithm`] dictionary.
#[webidl_union]
pub enum AlgorithmIdentifier<'a> {
    Object(Object<'a>),
    Name(String),
}

/// <https://w3c.github.io/webcrypto/#hmac-importparams>
#[webidl_dictionary(extends = Algorithm)]
pub struct HmacImportParams<'a> {
    pub parent: Algorithm,
    /// <https://w3c.github.io/webcrypto/#dfn-HmacImportParams-hash>
    ///
    /// A [`HashAlgorithmIdentifier`], normalized with _op_ "digest" by step 2.10.1.3.
    ///
    /// [`HashAlgorithmIdentifier`]: https://w3c.github.io/webcrypto/#dom-hashalgorithmidentifier
    pub hash: AlgorithmIdentifier<'a>,
    /// <https://w3c.github.io/webcrypto/#dfn-HmacImportParams-length>
    pub length: Option<EnforceRange<u32>>,
}

/// The hash functions `digest()` supports, which are also the ones HMAC can use.
///
/// MD5 isn't one of the spec's registered algorithms. It's supported, as an implementation-defined
/// algorithm the spec allows other applicable specifications to add, for compatibility with the
/// C++ StarlingMonkey, which supports it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgorithm {
    /// The `supportedAlgorithms` entries for "digest", looked up by case-insensitive match.
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "MD5" => Some(Self::Md5),
            "SHA-1" => Some(Self::Sha1),
            "SHA-256" => Some(Self::Sha256),
            "SHA-384" => Some(Self::Sha384),
            "SHA-512" => Some(Self::Sha512),
            _ => None,
        }
    }

    /// The registered name of the hash function.
    pub fn name(self) -> &'static str {
        match self {
            Self::Md5 => "MD5",
            Self::Sha1 => "SHA-1",
            Self::Sha256 => "SHA-256",
            Self::Sha384 => "SHA-384",
            Self::Sha512 => "SHA-512",
        }
    }

    /// The digest operation's result for each hash function: the message digest of _data_.
    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Md5 => Md5::digest(data).to_vec(),
            Self::Sha1 => Sha1::digest(data).to_vec(),
            Self::Sha256 => Sha256::digest(data).to_vec(),
            Self::Sha384 => Sha384::digest(data).to_vec(),
            Self::Sha512 => Sha512::digest(data).to_vec(),
        }
    }
}

/// A normalized algorithm for "sign" and "verify".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    /// HMAC, whose parameters for these operations are just an `Algorithm`.
    Hmac,
    /// Ed25519, whose parameters for these operations are just an `Algorithm`.
    Ed25519,
}

impl SignatureAlgorithm {
    /// The `supportedAlgorithms` entries for "sign" and "verify", looked up by case-insensitive
    /// match.
    fn from_name(name: &str) -> Option<Self> {
        [Self::Hmac, Self::Ed25519]
            .into_iter()
            .find(|algorithm| algorithm.name().eq_ignore_ascii_case(name))
    }

    /// The `name` of the normalized algorithm: the registered name it matched.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hmac => "HMAC",
            Self::Ed25519 => "Ed25519",
        }
    }
}

/// A normalized algorithm for "importKey".
pub enum ImportKeyAlgorithm {
    /// HMAC's `HmacImportParams`, with `hash` normalized.
    Hmac {
        hash: HashAlgorithm,
        length: Option<u32>,
    },
    /// Ed25519, whose parameters for "importKey" are just an `Algorithm`.
    Ed25519,
}

/// The `supportedAlgorithms` entries for "importKey", looked up by case-insensitive match. Each
/// has its own _desiredType_, which [`normalize_import_key_algorithm`] converts to.
#[derive(Clone, Copy)]
enum ImportKeyRegistered {
    Hmac,
    Ed25519,
}

impl ImportKeyRegistered {
    fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("HMAC") {
            Some(Self::Hmac)
        } else if name.eq_ignore_ascii_case("Ed25519") {
            Some(Self::Ed25519)
        } else {
            None
        }
    }
}

/// Steps 1-2.5 of <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>,
/// with _registeredAlgorithms_ looked up by `registered`. Returns the registered algorithm
/// _algName_ matched and, for an object _alg_, the object, which the caller converts to its
/// _desiredType_ from step 2.6. A string _alg_ is `None`: it stands for a new `Algorithm`
/// dictionary with only a `name`.
fn lookup_algorithm<'a, T>(
    scope: &Scope<'_>,
    identifier: AlgorithmIdentifier<'a>,
    registered: fn(&str) -> Option<T>,
) -> Result<(T, Option<Object<'a>>), ExnThrown> {
    // Step 1: If _alg_ is an instance of a DOMString: Return the result of running the
    //         `normalize an algorithm` algorithm, with _alg_ set to a new `Algorithm`
    //         dictionary whose `name` attribute is _alg_, and with _op_ set to _op_.
    //         (For that new dictionary, steps 2.1-2.4 just yield _alg_ as _algName_, so this
    //         skips straight to step 2.5.)
    let (alg_name, object) = match identifier {
        AlgorithmIdentifier::Name(name) => (name, None),
        // Step 2: If _alg_ is an object:
        AlgorithmIdentifier::Object(object) => {
            // Step 2.1: Let _registeredAlgorithms_ be the associative container stored at the
            //           _op_ key of `supportedAlgorithms`.
            //           (That is `registered`.)
            // Step 2.2: Let _initialAlg_ be the result of converting the ECMAScript object
            //           represented by _alg_ to the IDL dictionary type `Algorithm`, as
            //           defined by [WebIDL].
            // Step 2.3: If an error occurred, return the error and terminate this algorithm.
            //           (`name` is a required member, so a missing one is a `TypeError`.)
            // Step 2.4: Let _algName_ be the value of the `name` attribute of _initialAlg_.
            (
                Algorithm::from_jsval_throwing(scope, object.to_jsval_throwing(scope)?, ())?.name,
                Some(object),
            )
        }
    };
    // Step 2.5: If _registeredAlgorithms_ contains a key that is a `case-insensitive` string
    //           match for _algName_: set _algName_ to the value of the matching key and let
    //           _desiredType_ be the IDL dictionary type stored at _algName_ in
    //           _registeredAlgorithms_. Otherwise: return a new `NotSupportedError` and
    //           terminate this algorithm.
    let Some(algorithm) = registered(&alg_name) else {
        return Err(throw_dom_exception(
            scope,
            "NotSupportedError",
            &format!("Unrecognized algorithm name: {alg_name}"),
        ));
    };
    Ok((algorithm, object))
}

/// <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>, for an _op_
/// whose `supportedAlgorithms` are all of the plain `Algorithm` dictionary type, as they have no
/// members beyond `name`.
fn normalize_to_algorithm<T>(
    scope: &Scope<'_>,
    identifier: AlgorithmIdentifier<'_>,
    registered: fn(&str) -> Option<T>,
) -> Result<T, ExnThrown> {
    // Steps 1-2.5.
    let (algorithm, _) = lookup_algorithm(scope, identifier, registered)?;
    // Step 2.6: Let _normalizedAlgorithm_ be the result of converting the ECMAScript object
    //           represented by _alg_ to the IDL dictionary type _desiredType_, as defined
    //           by [WebIDL].
    // Step 2.7: Set the `name` attribute of _normalizedAlgorithm_ to _algName_.
    // Step 2.8: If an error occurred, return the error and terminate this algorithm.
    //           (_desiredType_ is `Algorithm` itself, already converted by steps 2.2-2.3. The
    //           returned `T` stands for the matched key.)
    // Step 2.9: Let _dictionaries_ be a list consisting of the IDL dictionary type _desiredType_
    //           and all of _desiredType_'s inherited dictionaries, in order from least to most
    //           derived.
    // Step 2.10: For each dictionary _dictionary_ in _dictionaries_:
    // Step 2.10.1: For each dictionary member _member_ declared on _dictionary_, in order:
    // Step 2.10.1.1: Let _key_ be the identifier of _member_.
    // Step 2.10.1.2: Let _idlValue_ be the value of the dictionary member with key name of _key_
    //                on _normalizedAlgorithm_.
    // Step 2.10.1.3: If _member_ is of the type `BufferSource` and is present: Set the
    //                dictionary member on _normalizedAlgorithm_ with key name _key_ to the result
    //                of `getting a copy of the bytes held by` _idlValue_, replacing the current
    //                value. If _member_ is of the type `HashAlgorithmIdentifier`: Set the
    //                dictionary member on _normalizedAlgorithm_ with key name _key_ to the result
    //                of `normalizing an algorithm`, with the _alg_ set to _idlValue_ and the _op_
    //                set to "digest". If _member_ is of the type `AlgorithmIdentifier`: Set the
    //                dictionary member on _normalizedAlgorithm_ with key name _key_ to the result
    //                of `normalizing an algorithm`, with the _alg_ set to _idlValue_ and the _op_
    //                set to the operation defined by the specification that defines the
    //                algorithm identified by _algName_.
    // Step 2.10.1.4: If an error occurred, return the error and terminate this algorithm.
    //                (`Algorithm` has only `name`, so there is nothing to do.)
    // Step 2.11: Return _normalizedAlgorithm_.
    Ok(algorithm)
}

/// <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>, with _op_
/// set to "digest", whose `supportedAlgorithms` are the four SHA functions and MD5. None of them has
/// members beyond `name`, so their _desiredType_ is the plain `Algorithm` dictionary.
pub fn normalize_digest_algorithm(
    scope: &Scope<'_>,
    identifier: AlgorithmIdentifier<'_>,
) -> Result<HashAlgorithm, ExnThrown> {
    normalize_to_algorithm(scope, identifier, HashAlgorithm::from_name)
}

/// <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>, with _op_
/// set to "sign" or "verify". Every algorithm registered for them so far takes no parameters
/// beyond `name`, so its _desiredType_ is the plain `Algorithm` dictionary.
pub fn normalize_signature_algorithm(
    scope: &Scope<'_>,
    identifier: AlgorithmIdentifier<'_>,
) -> Result<SignatureAlgorithm, ExnThrown> {
    normalize_to_algorithm(scope, identifier, SignatureAlgorithm::from_name)
}

/// <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>, with _op_
/// set to "importKey".
pub fn normalize_import_key_algorithm(
    scope: &Scope<'_>,
    identifier: AlgorithmIdentifier<'_>,
) -> Result<ImportKeyAlgorithm, ExnThrown> {
    // Steps 1-2.5.
    let (registered, object) = lookup_algorithm(scope, identifier, ImportKeyRegistered::from_name)?;
    // Step 2.6: Let _normalizedAlgorithm_ be the result of converting the ECMAScript object
    //           represented by _alg_ to the IDL dictionary type _desiredType_, as defined by
    //           [WebIDL].
    // Step 2.7: Set the `name` attribute of _normalizedAlgorithm_ to _algName_.
    // Step 2.8: If an error occurred, return the error and terminate this algorithm.
    // Step 2.9: Let _dictionaries_ be a list consisting of the IDL dictionary type _desiredType_
    //           and all of _desiredType_'s inherited dictionaries, in order from least to most
    //           derived.
    // Step 2.10: For each dictionary _dictionary_ in _dictionaries_:
    // Step 2.10.1: For each dictionary member _member_ declared on _dictionary_, in order:
    // Step 2.10.1.1: Let _key_ be the identifier of _member_.
    // Step 2.10.1.2: Let _idlValue_ be the value of the dictionary member with key name of _key_
    //                on _normalizedAlgorithm_.
    // Step 2.10.1.3: If _member_ is of the type `BufferSource` and is present: Set the dictionary
    //                member on _normalizedAlgorithm_ with key name _key_ to the result of
    //                `getting a copy of the bytes held by` _idlValue_, replacing the current
    //                value. If _member_ is of the type `HashAlgorithmIdentifier`: Set the
    //                dictionary member on _normalizedAlgorithm_ with key name _key_ to the result
    //                of `normalizing an algorithm`, with the _alg_ set to _idlValue_ and the _op_
    //                set to "digest". If _member_ is of the type `AlgorithmIdentifier`: Set the
    //                dictionary member on _normalizedAlgorithm_ with key name _key_ to the result
    //                of `normalizing an algorithm`, with the _alg_ set to _idlValue_ and the _op_
    //                set to the operation defined by the specification that defines the algorithm
    //                identified by _algName_.
    // Step 2.10.1.4: If an error occurred, return the error and terminate this algorithm.
    // Step 2.11: Return _normalizedAlgorithm_.
    //
    // Each arm below performs steps 2.6-2.11 for its _desiredType_. `ImportKeyAlgorithm`'s
    // variant stands for _algName_ (step 2.7), and the dictionaries' members are the only
    // thing steps 2.9-2.10 can act on.
    match registered {
        // _desiredType_ is `HmacImportParams`, whose dictionaries are `Algorithm` and then
        // `HmacImportParams`. Only `hash`, a `HashAlgorithmIdentifier`, needs anything in step
        // 2.10.1.3.
        ImportKeyRegistered::Hmac => {
            // A string _alg_ is a dictionary with only a `name`, so it lacks the required `hash`
            // member, a `TypeError` (step 2.8).
            let Some(object) = object else {
                return Err(throw_type_error(
                    scope,
                    c"HmacImportParams: missing required member 'hash'",
                ));
            };
            let params =
                HmacImportParams::from_jsval_throwing(scope, object.to_jsval_throwing(scope)?, ())?;
            let hash = normalize_digest_algorithm(scope, params.hash)?;
            Ok(ImportKeyAlgorithm::Hmac {
                hash,
                length: params.length.map(|EnforceRange(length)| length),
            })
        }
        // _desiredType_ is `Algorithm` itself, already converted by steps 2.2-2.3, and it has
        // only `name`, so steps 2.8-2.10 have nothing to do.
        ImportKeyRegistered::Ed25519 => Ok(ImportKeyAlgorithm::Ed25519),
    }
}
