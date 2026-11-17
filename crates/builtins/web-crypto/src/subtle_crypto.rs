// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/webcrypto/#subtlecrypto-interface>

use core_runtime::{webidl_interface, webidl_methods, webidl_union};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::{ArrayBuffer, ArrayBufferView, Promise};

use crate::algorithm::{normalize_digest_algorithm, AlgorithmIdentifier};

/// <https://w3c.github.io/webcrypto/#subtlecrypto-interface>
#[webidl_interface]
pub struct SubtleCrypto {}

/// <https://webidl.spec.whatwg.org/#BufferSource>
#[webidl_union]
pub enum BufferSource<'a> {
    View(ArrayBufferView<'a>),
    Buffer(ArrayBuffer<'a>),
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
        //         _data_, so the copy must come after it.)
        let data = match data {
            BufferSource::View(view) => view.copy_bytes(scope)?,
            BufferSource::Buffer(buffer) => buffer.copy_bytes(scope)?,
        };
        // Step 5: Let _realm_ be the `relevant realm` of `this`.
        //         (The result below is created in the current realm, which is `this`'s relevant
        //         realm as long as there is only one realm.)
        // Step 6: Let _promise_ be a new Promise.
        // Step 7: Return _promise_ and perform the remaining steps `in parallel`.
        // Step 8: If the following steps or referenced procedures say to `throw an error`,
        //         `queue a global task` to reject _promise_ with the returned error, and then
        //         terminate the algorithm.
        //         (Nothing below can fail but allocation.)
        // Step 9: Let _digest_ be the result of performing the digest operation specified by
        //         _normalizedAlgorithm_ using _algorithm_, with _data_ as _message_.
        // Step 10: `Queue a global task` to perform the remaining steps.
        // Step 11: Let _result_ be the result of `creating an ArrayBuffer` in _realm_,
        //          containing _digest_.
        // Step 12: Resolve _promise_ with _result_.
        // (SHA over an in-memory buffer is cheap next to scheduling it, so steps 7-10 run inline
        // and the returned promise is already resolved.)
        let result = ArrayBuffer::with_data(scope, &hash.digest(&data))?;
        Promise::new_resolved_with_value(scope, result)
    }
}
