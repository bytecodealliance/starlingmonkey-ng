// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/webcrypto/#subtlecrypto-interface>

use core_runtime::event_loop::{reject_without_event_loop, with_active_event_loop, Task, TaskId};
use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::{ConversionError, FromJSVal};
use js::error::{ExnThrown, ThrowException};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::JSTracer;
use js::prelude::HandleValue;
use js::promise::Promise as PromiseType;
use js::typedarray::BufferSource;
use js::{ArrayBuffer, Promise};
use web_globals::dom_exception::DOMExceptionError;

use crate::algorithm::{
    normalize_digest_algorithm, normalize_import_key_algorithm, normalize_signature_algorithm,
    AlgorithmIdentifier, ImportKeyAlgorithm, SignatureAlgorithm,
};
use crate::crypto_key::{
    create_crypto_key, CryptoKey, CryptoKeyImpl, KeyData, KeyFormat, KeyMaterial, KeyType,
    KeyUsage, NewKey,
};
use crate::jwk::JsonWebKey;
use crate::{ed25519, hmac};

/// <https://w3c.github.io/webcrypto/#subtlecrypto-interface>
#[webidl_interface]
pub struct SubtleCrypto {}

/// `(BufferSource or JsonWebKey)`, the _keyData_ of `importKey()`.
///
/// Only converted from JS, so this implements just that direction of the union rather than
/// using `#[webidl_union]`, which would also need a `JsonWebKey` to JS conversion.
pub enum BufferSourceOrJsonWebKey<'a> {
    Buffer(BufferSource<'a>),
    Jwk(Box<JsonWebKey>),
}

/// <https://webidl.spec.whatwg.org/#js-union>, for the union of a buffer source type and a
/// dictionary: an `ArrayBuffer` or `ArrayBufferView` converts to the `BufferSource`, and any
/// other value, `undefined` and `null` included, to the dictionary.
impl<'s, 'v> FromJSVal<'s, 'v> for BufferSourceOrJsonWebKey<'s> {
    type Config = ();

    fn from_jsval(
        scope: &'s Scope<'s>,
        val: HandleValue<'v>,
        _: (),
    ) -> Result<Self, ConversionError> {
        match BufferSource::from_jsval(scope, val, ()) {
            Ok(buffer) => Ok(Self::Buffer(buffer)),
            Err(ConversionError::Failure(_)) => {
                JsonWebKey::from_jsval(scope, val, ()).map(|jwk| Self::Jwk(Box::new(jwk)))
            }
            Err(error) => Err(error),
        }
    }
}

/// The outcome of an operation's steps that run in parallel, as Rust data: what to resolve the
/// promise with, or the error to reject it with. The task that settles the promise creates the
/// JS value from it.
type Outcome = Result<Resolution, DOMExceptionError>;

/// What to resolve an operation's promise with.
enum Resolution {
    /// A new `ArrayBuffer` containing the bytes.
    ArrayBuffer(Vec<u8>),
    Boolean(bool),
    /// A new `CryptoKey`.
    Key(Box<NewKey>),
}

#[webidl_methods]
impl SubtleCrypto<'_> {
    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-digest>
    #[method]
    fn digest<'r>(
        &self,
        scope: &'r Scope<'_>,
        algorithm: AlgorithmIdentifier<'_>,
        data: BufferSource<'_>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _algorithm_ be the algorithm parameter passed to the `digest()` method.
        // Step 2: Let _normalizedAlgorithm_ be the result of `normalizing an algorithm`, with
        //         _alg_ set to _algorithm_ and _op_ set to "digest".
        // Step 3: If an error occurred, return a Promise rejected with _normalizedAlgorithm_.
        //         (The `Err` returned here is turned into that rejected promise by
        //         `#[webidl_methods]`, as for any error from a promise-returning operation.)
        let hash = normalize_digest_algorithm(scope, algorithm)?;
        // Step 4: Let _data_ be the result of `getting a copy of the bytes held by` the _data_
        //         parameter passed to the `digest()` method.
        //         (The order is observable: a `name` getter run in step 2 can detach or modify
        //         _data_, so the bytes must be read after it.)
        // Step 5: Let _realm_ be the `relevant realm` of `this`.
        //         (The result below is created by a task of the current event loop, so in the
        //         current realm, which is `this`'s relevant realm as long as there is only one
        //         realm.)
        // Step 6: Let _promise_ be a new Promise.
        let promise = Promise::new_pending(scope)?;
        //         (Settling it takes a task, so without an active event loop it is rejected, as
        //         for `fetch` and the other operations that need one.)
        if reject_without_event_loop(scope, &promise, "`digest`")? {
            return Ok(promise);
        }
        // Step 7: Return _promise_ and perform the remaining steps `in parallel`.
        // Step 8: If the following steps or referenced procedures say to `throw an error`,
        //         `queue a global task` to reject _promise_ with the returned error, and then
        //         terminate the algorithm.
        //         (Nothing below can fail but allocation, which rejects _promise_ in the task.)
        // Step 9: Let _digest_ be the result of performing the digest operation specified by
        //         _normalizedAlgorithm_ using _algorithm_, with _data_ as _message_.
        //         (Nothing can run between reading _data_ and finishing the hash, so hashing
        //         _data_'s own bytes is the same as hashing a copy, without making one. Hashing
        //         now instead of after returning is not observable either.)
        let digest = data.with_bytes(|bytes| hash.digest(bytes));
        // Step 10: `Queue a global task` to perform the remaining steps.
        //          (The task can't run before the jobs that are already queued, so _promise_ is
        //          settled after them, as the spec requires.)
        let task = SettleTask {
            promise: Heap::from(promise),
            outcome: Ok(Resolution::ArrayBuffer(digest)),
        };
        with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)))
            .expect("checked to be active above");
        Ok(promise)
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-importKey>
    #[method]
    fn import_key<'r>(
        &self,
        scope: &'r Scope<'_>,
        format: KeyFormat,
        key_data: BufferSourceOrJsonWebKey<'_>,
        algorithm: AlgorithmIdentifier<'_>,
        extractable: bool,
        key_usages: Vec<KeyUsage>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _format_, _algorithm_, _extractable_ and _usages_, be the _format_,
        //         _algorithm_, _extractable_ and _keyUsages_ parameters passed to the
        //         `importKey()` method, respectively.
        // Step 2: Let _normalizedAlgorithm_ be the result of `normalizing an algorithm`, with
        //         _alg_ set to _algorithm_ and _op_ set to "importKey".
        // Step 3: If an error occurred, return a Promise rejected with _normalizedAlgorithm_.
        //         (As in `digest()`.)
        let normalized = normalize_import_key_algorithm(scope, algorithm)?;
        let key_data = match (format, key_data) {
            // Step 4: If _format_ is equal to the string "jwk":
            // Step 4.1: If the _keyData_ parameter passed to the `importKey()` method is not a
            //           `JsonWebKey` dictionary, throw a `TypeError`.
            // Step 4.2: Let _keyData_ be the _keyData_ parameter passed to the `importKey()`
            //           method.
            (KeyFormat::Jwk, BufferSourceOrJsonWebKey::Jwk(jwk)) => KeyData::Jwk(jwk),
            (KeyFormat::Jwk, _) => {
                return Err(js::error::throw_type_error(
                    scope,
                    c"The key data must be a JsonWebKey for the \"jwk\" format",
                ));
            }
            // Otherwise:
            // Step 4.1: If the _keyData_ parameter passed to the `importKey()` method is a
            //           `JsonWebKey` dictionary, throw a `TypeError`.
            (_, BufferSourceOrJsonWebKey::Jwk(_)) => {
                return Err(js::error::throw_type_error(
                    scope,
                    c"The key data must be a BufferSource for formats other than \"jwk\"",
                ));
            }
            // Step 4.2: Let _keyData_ be the result of `getting a copy of the bytes held by`
            //           the _keyData_ parameter passed to the `importKey()` method.
            (_, BufferSourceOrJsonWebKey::Buffer(buffer)) => {
                KeyData::Bytes(buffer.with_bytes(<[u8]>::to_vec))
            }
        };
        // Step 5: Let _realm_ be the `relevant realm` of `this`.
        //         (As in `digest()`.)
        // Step 6: Let _promise_ be a new Promise.
        //         (As in `digest()`, rejected without an active event loop.)
        let promise = Promise::new_pending(scope)?;
        if reject_without_event_loop(scope, &promise, "`importKey`")? {
            return Ok(promise);
        }
        // Step 7: Return _promise_ and perform the remaining steps `in parallel`.
        // Step 8: If the following steps or referenced procedures say to `throw an error`,
        //         `queue a global task` on the `crypto task source`, given _realm_'s global object,
        //         to reject _promise_ with the returned error; and then terminate the algorithm.
        //         (The steps return the error, which `SettleTask` rejects _promise_ with.)
        let outcome = import_key_in_parallel(normalized, format, key_data, extractable, key_usages);
        let task = SettleTask {
            promise: Heap::from(promise),
            outcome,
        };
        with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)))
            .expect("checked to be active above");
        Ok(promise)
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-sign>
    #[method]
    fn sign<'r>(
        &self,
        scope: &'r Scope<'_>,
        algorithm: AlgorithmIdentifier<'_>,
        key: CryptoKey<'_>,
        data: BufferSource<'_>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _algorithm_ and _key_ be the _algorithm_ and _key_ parameters passed to
        //         the `sign()` method, respectively.
        // Step 2: Let _normalizedAlgorithm_ be the result of `normalizing an algorithm`, with
        //         _alg_ set to _algorithm_ and _op_ set to "sign".
        // Step 3: If an error occurred, return a Promise rejected with _normalizedAlgorithm_.
        //         (As in `digest()`.)
        let normalized = normalize_signature_algorithm(scope, algorithm)?;
        // Step 4: Let _data_ be the result of `getting a copy of the bytes held by` the _data_
        //         parameter passed to the `sign()` method.
        //         (As in `digest()`, the bytes are read in place when signing.)
        // Step 5: Let _realm_ be the `relevant realm` of `this`.
        //         (As in `digest()`.)
        // Step 6: Let _promise_ be a new Promise.
        //         (As in `digest()`, rejected without an active event loop.)
        let promise = Promise::new_pending(scope)?;
        if reject_without_event_loop(scope, &promise, "`sign`")? {
            return Ok(promise);
        }
        // Step 7: Return _promise_ and perform the remaining steps `in parallel`.
        // Step 8: If the following steps or referenced procedures say to `throw an error`,
        //         `queue a global task` on the `crypto task source`, given _realm_'s global object,
        //         to reject _promise_ with the returned error; and then terminate the algorithm.
        //         (The steps return the error, which `SettleTask` rejects _promise_ with.)
        let outcome = sign_in_parallel(normalized, &key.data(), data);
        let task = SettleTask {
            promise: Heap::from(promise),
            outcome,
        };
        with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)))
            .expect("checked to be active above");
        Ok(promise)
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-verify>
    #[method]
    fn verify<'r>(
        &self,
        scope: &'r Scope<'_>,
        algorithm: AlgorithmIdentifier<'_>,
        key: CryptoKey<'_>,
        signature: BufferSource<'_>,
        data: BufferSource<'_>,
    ) -> Result<Promise<'r>, ExnThrown> {
        // Step 1: Let _algorithm_ and _key_ be the _algorithm_ and _key_ parameters passed to
        //         the `verify()` method, respectively.
        // Step 2: Let _normalizedAlgorithm_ be the result of `normalizing an algorithm`, with
        //         _alg_ set to _algorithm_ and _op_ set to "verify".
        // Step 3: If an error occurred, return a Promise rejected with _normalizedAlgorithm_.
        //         (As in `digest()`.)
        let normalized = normalize_signature_algorithm(scope, algorithm)?;
        // Step 4: Let _signature_ be the result of `getting a copy of the bytes held by` the
        //         _signature_ parameter passed to the `verify()` method.
        let signature = signature.with_bytes(<[u8]>::to_vec);
        // Step 5: Let _data_ be the result of `getting a copy of the bytes held by` the _data_
        //         parameter passed to the `verify()` method.
        //         (As in `digest()`, the bytes are read in place when verifying.)
        // Step 6: Let _realm_ be the `relevant realm` of `this`.
        //         (As in `digest()`.)
        // Step 7: Let _promise_ be a new Promise.
        //         (As in `digest()`, rejected without an active event loop.)
        let promise = Promise::new_pending(scope)?;
        if reject_without_event_loop(scope, &promise, "`verify`")? {
            return Ok(promise);
        }
        // Step 8: Return _promise_ and perform the remaining steps `in parallel`.
        // Step 9: If the following steps or referenced procedures say to `throw an error`,
        //         `queue a global task` on the `crypto task source`, given _realm_'s global object,
        //         to reject _promise_ with the returned error; and then terminate the algorithm.
        //         (The steps return the error, which `SettleTask` rejects _promise_ with.)
        let outcome = verify_in_parallel(normalized, &key.data(), &signature, data);
        let task = SettleTask {
            promise: Heap::from(promise),
            outcome,
        };
        with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)))
            .expect("checked to be active above");
        Ok(promise)
    }
}

/// The steps of `importKey()` that run in parallel, from step 9.
fn import_key_in_parallel(
    normalized: ImportKeyAlgorithm,
    format: KeyFormat,
    key_data: KeyData,
    extractable: bool,
    usages: Vec<KeyUsage>,
) -> Outcome {
    // Step 9: Let _result_ be the `CryptoKey` object that results from performing the import key
    //         operation specified by _normalizedAlgorithm_ using _keyData_, _algorithm_, _format_,
    //         _extractable_ and _usages_.
    let imported = match normalized {
        ImportKeyAlgorithm::Hmac { hash, length } => {
            hmac::import_key(hash, length, format, key_data, extractable, &usages)?
        }
        ImportKeyAlgorithm::Ed25519 => ed25519::import_key(format, key_data, extractable, &usages)?,
    };
    // Step 10: If the `[[type]]` internal slot of _result_ is "secret" or "private" and _usages_
    //          is empty, then throw a `SyntaxError`.
    if matches!(imported.key_type, KeyType::Secret | KeyType::Private) && usages.is_empty() {
        return Err(DOMExceptionError::new(
            "SyntaxError",
            "A secret or private key must have at least one usage",
        ));
    }
    // Step 11: Set the `[[extractable]]` internal slot of _result_ to _extractable_.
    // Step 12: Set the `[[usages]]` internal slot of _result_ to the `normalized value` of
    //          _usages_.
    // Step 13: `Queue a global task` on the `crypto task source`, given _realm_'s global object,
    //          to perform the remaining steps.
    //          (As in `digest()`, by the caller.)
    Ok(Resolution::Key(Box::new(NewKey {
        key_type: imported.key_type,
        extractable,
        usages: KeyUsage::normalize(&usages),
        material: imported.material,
    })))
}

/// The steps of `sign()` that run in parallel, from step 9.
fn sign_in_parallel(
    normalized: SignatureAlgorithm,
    key: &CryptoKeyImpl,
    data: BufferSource<'_>,
) -> Outcome {
    // Step 9: If the `name` member of _normalizedAlgorithm_ is not equal to the `name` attribute
    //         of the `[[algorithm]]` internal slot of _key_ then throw an `InvalidAccessError`.
    check_algorithm_name(normalized, key.material())?;
    // Step 10: If the `[[usages]]` internal slot of _key_ does not contain an entry that is
    //          "sign", then throw an `InvalidAccessError`.
    check_usage(key, KeyUsage::Sign)?;
    // Step 11: Let _signature_ be the result of performing the sign operation specified by
    //          _normalizedAlgorithm_ using _key_ and _algorithm_ and with _data_ as _message_.
    let signature = match (normalized, key.material()) {
        (SignatureAlgorithm::Hmac, KeyMaterial::Hmac { hash, key, .. }) => {
            data.with_bytes(|message| hmac::sign(*hash, key, message))
        }
        (SignatureAlgorithm::Ed25519, KeyMaterial::Ed25519(key)) => {
            data.with_bytes(|message| ed25519::sign(key, message))?
        }
        // (Step 9 checked that the key is for _normalizedAlgorithm_.)
        _ => unreachable!("the key's algorithm is the normalized algorithm"),
    };
    // Step 12: `Queue a global task` on the `crypto task source`, given _realm_'s global object,
    //          to perform the remaining steps.
    //          (As in `digest()`, by the caller.)
    Ok(Resolution::ArrayBuffer(signature))
}

/// The steps of `verify()` that run in parallel, from step 10.
fn verify_in_parallel(
    normalized: SignatureAlgorithm,
    key: &CryptoKeyImpl,
    signature: &[u8],
    data: BufferSource<'_>,
) -> Outcome {
    // Step 10: If the `name` member of _normalizedAlgorithm_ is not equal to the `name` attribute
    //          of the `[[algorithm]]` internal slot of _key_ then throw an `InvalidAccessError`.
    check_algorithm_name(normalized, key.material())?;
    // Step 11: If the `[[usages]]` internal slot of _key_ does not contain an entry that is
    //          "verify", then throw an `InvalidAccessError`.
    check_usage(key, KeyUsage::Verify)?;
    // Step 12: Let _result_ be the result of performing the verify operation specified by
    //          _normalizedAlgorithm_ using _key_, _algorithm_ and _signature_ and with _data_ as
    //          _message_.
    let result = match (normalized, key.material()) {
        (SignatureAlgorithm::Hmac, KeyMaterial::Hmac { hash, key, .. }) => {
            data.with_bytes(|message| hmac::verify(*hash, key, message, signature))
        }
        (SignatureAlgorithm::Ed25519, KeyMaterial::Ed25519(key)) => {
            data.with_bytes(|message| ed25519::verify(key, message, signature))?
        }
        // (Step 10 checked that the key is for _normalizedAlgorithm_.)
        _ => unreachable!("the key's algorithm is the normalized algorithm"),
    };
    // Step 13: `Queue a global task` on the `crypto task source`, given _realm_'s global object,
    //          to perform the remaining steps.
    //          (As in `digest()`, by the caller.)
    Ok(Resolution::Boolean(result))
}

/// The check, in the steps of `sign()` and `verify()`, that "the `name` member of
/// _normalizedAlgorithm_" is equal to "the `name` attribute of the `[[algorithm]]` internal slot
/// of _key_", throwing an `InvalidAccessError` if not.
fn check_algorithm_name(
    normalized: SignatureAlgorithm,
    key: &KeyMaterial,
) -> Result<(), DOMExceptionError> {
    if normalized.name() != key.algorithm_name() {
        return Err(DOMExceptionError::new(
            "InvalidAccessError",
            format!(
                "The key is for {}, not {}",
                key.algorithm_name(),
                normalized.name()
            ),
        ));
    }
    Ok(())
}

/// The check, in the steps of the operations that use a key, that "the `[[usages]]` internal
/// slot of _key_" contains _usage_, throwing an `InvalidAccessError` if not.
fn check_usage(key: &CryptoKeyImpl, usage: KeyUsage) -> Result<(), DOMExceptionError> {
    if !key.has_usage(usage) {
        return Err(DOMExceptionError::new(
            "InvalidAccessError",
            format!("The key's usages don't include \"{usage}\""),
        ));
    }
    Ok(())
}

/// The steps of the operations that run in a queued global task: creating the result and
/// settling the promise.
///
/// The `Heap` is kept alive by the event loop tracing live tasks via [`Task::trace`], so the task
/// itself lives unrooted in the task queue.
#[js::allow_unrooted_interior]
struct SettleTask {
    promise: Heap<PromiseType>,
    outcome: Outcome,
}

impl Task for SettleTask {
    fn kind(&self) -> &'static str {
        "subtle-crypto"
    }

    fn run(self: Box<Self>, scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        let promise = self.promise.take(scope);
        let settled = match self.outcome {
            // `digest()`'s step 11 (`sign()`'s step 13): Let _result_ be the result of `creating
            // an ArrayBuffer` in _realm_, containing _digest_ (_signature_).
            // Step 12 (14): Resolve _promise_ with _result_.
            Ok(Resolution::ArrayBuffer(bytes)) => ArrayBuffer::with_data(scope, &bytes)
                .and_then(|result| promise.resolve(scope, result)),
            // `verify()`'s step 14: Resolve _promise_ with _result_.
            Ok(Resolution::Boolean(result)) => promise.resolve(scope, result),
            // `importKey()`'s step 14: Let _result_ be the result of converting _result_ to an
            // ECMAScript Object in _realm_, as defined by [WebIDL].
            // Step 15: Resolve _promise_ with _result_.
            Ok(Resolution::Key(key)) => {
                create_crypto_key(scope, *key).and_then(|result| promise.resolve(scope, result))
            }
            // Step 8 (`verify()`'s step 9), for an error from the steps that ran in parallel.
            Err(error) => Err(error.throw(scope)),
        };
        match settled {
            Ok(()) => Ok(()),
            // Step 8, for the allocation failing or the error above: reject _promise_ with the
            // error.
            Err(ExnThrown) => promise.reject_with_pending(scope),
        }
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn trace(&self, trc: *mut JSTracer) {
        unsafe {
            self.promise.trace(trc);
        }
    }
}
