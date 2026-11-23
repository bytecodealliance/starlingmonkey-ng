// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Algorithm identifiers and
//! <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>, for the
//! "digest" operation, the only one implemented so far. (Numbers like "Step 2.5" count the
//! items of the spec's nested list in order.)

use core_runtime::{webidl_dictionary, webidl_union};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;
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

/// The hash functions `digest()` supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgorithm {
    /// The `supportedAlgorithms` entries for "digest", looked up by case-insensitive match.
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "SHA-1" => Some(Self::Sha1),
            "SHA-256" => Some(Self::Sha256),
            "SHA-384" => Some(Self::Sha384),
            "SHA-512" => Some(Self::Sha512),
            _ => None,
        }
    }

    /// The digest operation's result for each hash function: the message digest of _data_.
    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => Sha1::digest(data).to_vec(),
            Self::Sha256 => Sha256::digest(data).to_vec(),
            Self::Sha384 => Sha384::digest(data).to_vec(),
            Self::Sha512 => Sha512::digest(data).to_vec(),
        }
    }
}

/// <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>, with _op_
/// fixed to "digest", whose `supportedAlgorithms` are the four SHA functions. None of them has
/// members beyond `name`, so their _desiredType_ is the plain `Algorithm` dictionary.
pub fn normalize_digest_algorithm(
    scope: &Scope<'_>,
    identifier: AlgorithmIdentifier<'_>,
) -> Result<HashAlgorithm, ExnThrown> {
    // Step 1: If _alg_ is an instance of a DOMString: Return the result of running the
    //         `normalize an algorithm` algorithm, with _alg_ set to a new `Algorithm`
    //         dictionary whose `name` attribute is _alg_, and with _op_ set to _op_.
    //         (For that new dictionary, steps 2.1-2.4 just yield _alg_ as _algName_, so this
    //         skips straight to step 2.5.)
    let alg_name = match identifier {
        AlgorithmIdentifier::Name(name) => name,
        // Step 2: If _alg_ is an object:
        AlgorithmIdentifier::Object(object) => {
            // Step 2.1: Let _registeredAlgorithms_ be the associative container stored at the
            //           _op_ key of `supportedAlgorithms`.
            //           (That is `HashAlgorithm::from_name`.)
            // Step 2.2: Let _initialAlg_ be the result of converting the ECMAScript object
            //           represented by _alg_ to the IDL dictionary type `Algorithm`.
            // Step 2.3: If an error occurred, return the error and terminate this algorithm.
            //           (`name` is a required member, so a missing one is a `TypeError`.)
            // Step 2.4: Let _algName_ be the value of the `name` attribute of _initialAlg_.
            Algorithm::from_jsval_throwing(scope, object.to_jsval_throwing(scope)?, ())?.name
        }
    };
    // Step 2.5: If _registeredAlgorithms_ contains a key that is a `case-insensitive` string
    //           match for _algName_: set _algName_ to the value of the matching key and let
    //           _desiredType_ be the IDL dictionary type stored at _algName_ in
    //           _registeredAlgorithms_. Otherwise: return a new `NotSupportedError` and
    //           terminate this algorithm.
    let Some(hash) = HashAlgorithm::from_name(&alg_name) else {
        return Err(throw_dom_exception(
            scope,
            "NotSupportedError",
            &format!("Unrecognized algorithm name: {alg_name}"),
        ));
    };
    // Step 2.6: Let _normalizedAlgorithm_ be the result of converting the ECMAScript object
    //           represented by _alg_ to the IDL dictionary type _desiredType_.
    // Step 2.7: Set the `name` attribute of _normalizedAlgorithm_ to _algName_.
    // Step 2.8: If an error occurred, return the error and terminate this algorithm.
    //           (_desiredType_ is `Algorithm` itself, already converted by steps 2.2-2.3. The
    //           returned `HashAlgorithm` stands for the matched key.)
    // Step 2.9: Let _dictionaries_ be a list consisting of the IDL dictionary type _desiredType_
    //           and all of _desiredType_'s inherited dictionaries, in order from least to most
    //           derived.
    // Step 2.10: For each dictionary _dictionary_ in _dictionaries_, for each dictionary member
    //            _member_ declared on _dictionary_, in order: if _member_ is of the type
    //            `BufferSource` and is present, replace its value with the result of `getting a
    //            copy of the bytes held by` it; if it is of the type `HashAlgorithmIdentifier`,
    //            replace its value with the result of `normalizing an algorithm` with _op_ set to
    //            "digest"; if it is of the type `AlgorithmIdentifier`, replace its value with the
    //            result of `normalizing an algorithm` with _op_ set to the operation defined by
    //            the specification that defines the algorithm identified by _algName_.
    //            (`Algorithm` has only `name`, so there is nothing to do.)
    // Step 2.11: If an error occurred, return the error and terminate this algorithm.
    // Step 2.12: Return _normalizedAlgorithm_.
    Ok(hash)
}
