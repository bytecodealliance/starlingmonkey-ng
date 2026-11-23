// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/webcrypto/#subtlecrypto-interface>

use core_runtime::event_loop::{reject_without_event_loop, with_active_event_loop, Task, TaskId};
use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::JSTracer;
use js::promise::Promise as PromiseType;
use js::typedarray::BufferSource;
use js::{ArrayBuffer, Promise};

use crate::algorithm::{normalize_digest_algorithm, AlgorithmIdentifier};

/// <https://w3c.github.io/webcrypto/#subtlecrypto-interface>
#[webidl_interface]
pub struct SubtleCrypto {}

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
        let task = DigestTask {
            promise: Heap::from(promise),
            digest,
        };
        with_active_event_loop(|event_loop| event_loop.queue_ready(Box::new(task)))
            .expect("checked to be active above");
        Ok(promise)
    }
}

/// The steps of `digest()` that run in a queued global task: creating the result and settling
/// the promise.
///
/// The `Heap` is kept alive by the event loop tracing live tasks via [`Task::trace`], so the task
/// itself lives unrooted in the task queue.
#[js::allow_unrooted_interior]
struct DigestTask {
    promise: Heap<PromiseType>,
    digest: Vec<u8>,
}

impl Task for DigestTask {
    fn kind(&self) -> &'static str {
        "digest"
    }

    fn run(self: Box<Self>, scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
        let promise = self.promise.take(scope);
        // Step 11: Let _result_ be the result of `creating an ArrayBuffer` in _realm_,
        //          containing _digest_.
        match ArrayBuffer::with_data(scope, &self.digest) {
            // Step 12: Resolve _promise_ with _result_.
            Ok(result) => promise.resolve(scope, result),
            // Step 8, for the allocation failing: reject _promise_ with the error.
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
