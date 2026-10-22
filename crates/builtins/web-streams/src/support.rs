// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Non-spec plumbing shared by the stream algorithms: the algorithms stored on
//! controllers and how they are invoked, attaching native promise reactions, and
//! building iterator-result objects.

use js::error::ExnThrown;
use js::function::{Callback, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::prelude::ToJSVal;
use js::Promise;
use js::{Callable, Function};

/// A stream algorithm, as the `SetUp*` and `Create*` operations take it.
#[derive(Clone, Copy)]
pub enum AlgorithmArg<'s> {
    /// No algorithm. A promise-returning one resolves with undefined, any other
    /// returns undefined.
    None,
    /// A JS callable, invoked with the controller's algorithm receiver as `this`.
    Js(HandleValue<'s>),
    /// A native callback, invoked directly with its payload.
    Native(Callback, HandleValue<'s>),
}

impl<'s> AlgorithmArg<'s> {
    /// A native algorithm running `callback` with `payload`.
    pub fn native(
        scope: &'s Scope<'_>,
        callback: Callback,
        payload: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        Ok(Self::Native(callback, payload.to_jsval_throwing(scope)?))
    }

    /// The algorithm for a WebIDL callback dictionary member: none when the
    /// member is absent, the callable otherwise.
    pub(crate) fn from_member(scope: &'s Scope<'_>, member: Option<&Callable<'_>>) -> Self {
        match member {
            Some(callable) => Self::Js(scope.root_value(callable.as_value())),
            None => Self::None,
        }
    }
}

/// A stream algorithm stored on a controller. See [`AlgorithmArg`].
#[js::must_root]
#[derive(core_runtime::Traceable, Default)]
pub(crate) enum Algorithm {
    #[default]
    None,
    Js(Heap<Value>),
    Native {
        #[no_trace]
        callback: Callback,
        payload: Heap<Value>,
    },
}

impl Algorithm {
    pub(crate) fn get<'s>(&self, scope: &'s Scope<'_>) -> AlgorithmArg<'s> {
        match self {
            Self::None => AlgorithmArg::None,
            Self::Js(callable) => AlgorithmArg::Js(callable.get(scope)),
            Self::Native { callback, payload } => {
                AlgorithmArg::Native(*callback, payload.get(scope))
            }
        }
    }
}

impl From<AlgorithmArg<'_>> for Algorithm {
    fn from(algorithm: AlgorithmArg<'_>) -> Self {
        match algorithm {
            AlgorithmArg::None => Self::None,
            AlgorithmArg::Js(callable) => Self::Js(Heap::from(callable.get())),
            AlgorithmArg::Native(callback, payload) => Self::Native {
                callback,
                payload: Heap::from(payload.get()),
            },
        }
    }
}

/// Call a native algorithm's `callback` with `payload` and `args`.
fn call_native<'r>(
    scope: &'r Scope<'_>,
    callback: Callback,
    payload: HandleValue<'_>,
    args: &[impl ToJSVal<'r>],
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut values: smallvec::SmallVec<[HandleValue<'r>; 2]> =
        smallvec::SmallVec::with_capacity(args.len());
    for arg in args {
        values.push(arg.to_jsval_throwing(scope)?);
    }
    Ok(scope.root_value(callback(
        scope,
        CallbackArgs::from_values(&values),
        payload,
    )?))
}

/// Invoke a stored promise-returning stream algorithm (a controller's pull or
/// cancel algorithm) following WebIDL "invoke a Promise-returning operation":
///
/// - an absent algorithm returns a promise resolved with undefined;
/// - a synchronous throw becomes a rejected promise;
/// - otherwise the call result is coerced to a promise.
///
/// `receiver` is the `this` value of a JS algorithm: the underlying source,
/// sink or transformer.
pub(crate) fn invoke_promise_algorithm<'r>(
    scope: &'r Scope<'_>,
    algorithm: AlgorithmArg<'r>,
    receiver: HandleValue<'r>,
    args: &[impl ToJSVal<'r>],
) -> Promise<'r> {
    let result = match algorithm {
        AlgorithmArg::None => {
            // This branch runs once per pull for a source without `pull` and
            // once per chunk for a sink without `write`, and its promise is only
            // ever reacted to internally, so the per-global reused instance
            // serves without allocating.
            return Promise::shared_resolved_undefined(scope)
                .expect("failed to get shared resolved promise");
        }
        // A native algorithm's result is already the algorithm's promise, which
        // the controller reacts to directly. It is coerced as-is, without an extra
        // adoption tick, matching the spec's `uponPromise`.
        AlgorithmArg::Native(callback, payload) => {
            call_native(scope, callback, payload, args).map(|result| {
                Promise::call_original_resolve(scope, result).expect("Promise.resolve failed")
            })
        }
        // A WebIDL operation invocation (a user source/sink/transformer method,
        // called with the dictionary as `this`). WebIDL's `Promise<T>` conversion
        // of the result is `NewPromiseCapability` + `Resolve` ("a promise
        // resolved with"), which creates a new promise rather than returning a
        // promise result as-is. When the method returns a promise (e.g. an async
        // transformer method), adopting it adds a microtask tick, which is
        // observable in cancel and erroring orderings.
        AlgorithmArg::Js(callable) => {
            Function::call(scope, receiver, callable, args).map(|result| {
                Promise::new_resolved_with_value(scope, result)
                    .expect("failed to create resolved promise")
            })
        }
    };
    result.unwrap_or_else(|_| {
        Promise::new_rejected_with_pending_error(scope).expect("failed to create rejected promise")
    })
}

/// Invoke a non-promise-returning stream algorithm (a controller's start
/// algorithm) with `this` = `receiver` for a JS algorithm. Returns the raw call
/// result. A synchronous throw propagates as `Err`, and no algorithm yields
/// `undefined`.
pub(crate) fn invoke_algorithm<'r>(
    scope: &'r Scope<'_>,
    algorithm: AlgorithmArg<'r>,
    receiver: HandleValue<'r>,
    args: &[impl ToJSVal<'r>],
) -> Result<HandleValue<'r>, ExnThrown> {
    match algorithm {
        AlgorithmArg::None => Ok(HandleValue::undefined()),
        AlgorithmArg::Native(callback, payload) => call_native(scope, callback, payload, args),
        AlgorithmArg::Js(callable) => Function::call(scope, receiver, callable, args),
    }
}

/// Attach native fulfillment/rejection reactions to `promise`, each called with a
/// single payload value (typically the controller or reader the reaction
/// operates on). Implements the spec's "upon fulfillment / upon rejection of
/// promise" and "react to promise".
///
/// The dependent promise is never exposed to author code, so it is excluded from
/// unhandled-rejection tracking. Otherwise a fulfillment-only reaction on a
/// promise that later rejects would report a spurious unhandled rejection.
/// Attaching the reactions also marks `promise` handled.
pub(crate) fn react<'r, P: ToJSVal<'r>>(
    scope: &'r Scope<'_>,
    promise: &Promise<'_>,
    on_fulfilled: Option<(Callback, P)>,
    on_rejected: Option<(Callback, P)>,
) -> Result<(), ExnThrown> {
    let fulfilled = match on_fulfilled {
        Some((cb, payload)) => Some(Function::new_callback(scope, c"", 1, cb, payload)?),
        None => None,
    };
    let rejected = match on_rejected {
        Some((cb, payload)) => Some(Function::new_callback(scope, c"", 1, cb, payload)?),
        None => None,
    };
    promise.add_reactions_ignoring_unhandled_rejection(
        scope,
        fulfilled.map(|f| *f),
        rejected.map(|f| *f),
    )
}

/// Report and clear the exception a failed operation left pending, for callers
/// that have no caller of their own to propagate it to.
pub(crate) fn report_failure<T>(scope: &Scope<'_>, result: Result<T, ExnThrown>, context: &str) {
    if result.is_err() {
        js::exception::report_and_clear(scope, context);
    }
}
