// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! ECMAScript iteration protocol support.
//!
//! [`AsyncIteratorRecord`] is the spec's Iterator Record for async iteration:
//! [`get_async_iterator`] performs `GetIterator(obj, async)`, including the
//! `CreateAsyncFromSyncIterator` fallback that wraps a sync-only iterable in a
//! native async `next`. [`AsyncIteratorRecord::call_next`] is the raw
//! `IteratorNext` invocation and [`AsyncIteratorRecord::return_promise`] the
//! `return`-method teardown path. The free functions implement
//! `CreateIterResultObject` and the iterator-result accessors
//! (`IteratorComplete` / `IteratorValue`).
//!
//! The async-from-sync wrapper is allocation-lean: its per-iteration reaction
//! callbacks are cached on the record, and a primitive yielded value, which
//! can never be a thenable, skips `Promise.resolve` entirely by parking the
//! value on the record and chaining the wrap continuation on the shared
//! resolved trigger ([`Promise::shared_resolved_undefined`]).

use crate::class::get_or_init_shared_function;
use crate::conversion::{FromJSVal, ToJSVal};
use crate::error::ExnThrown;
use crate::function::{cast_payload, EmptyArgs};
use crate::gc::handle::Heap;
use crate::gc::scope::Scope;
use crate::macros::{jsclass, jsmethods};
use crate::native::Value;
use crate::prelude::{CallbackArgs, HandleValue};
use crate::value;
use crate::{Function, Object, Promise};

/// An async Iterator Record: the `[[Iterator]]` object and its cached
/// `[[NextMethod]]`, produced by [`get_async_iterator`]. For a sync-only
/// iterable the record wraps the sync iterator (`CreateAsyncFromSyncIterator`):
/// `[[NextMethod]]` is a native `next` that calls the sync iterator's `next`
/// and awaits the yielded value.
///
/// The record is a hidden JS class so it is GC-traced and can serve as a
/// native reaction callback's payload.
#[jsclass(hidden)]
pub struct AsyncIteratorRecord {
    /// The `[[Iterator]]` — the async (or wrapped sync) iterator object.
    iterator: Heap<crate::object::Object>,
    /// The `[[NextMethod]]` — for a sync iterable this is the native `afs_next`.
    next_method: Heap<crate::object::Object>,
    /// The sync iterator's own `next`, called by `afs_next`. `None` for a true
    /// async iterable — only the sync-wrapping path sets and uses it.
    sync_next: Option<Heap<crate::object::Object>>,
    /// `afs_next`'s rejection continuation (payload = this record), created on
    /// first use and reused for every subsequent iteration.
    afs_rejected_fn: Option<Heap<crate::function::Function>>,
    /// A primitive value yielded by the sync iterator, parked (with its `done`
    /// flag) for `afs_wrap_parked`, plus that continuation itself (payload =
    /// this record, created on first use). Single-occupancy as long as the
    /// consumer serializes its `next` calls — a new `[[NextMethod]]` call may
    /// only be made once the previous result promise has settled, which
    /// happens after the continuation has consumed the slot.
    pending_value: Heap<Value>,
    pending_done: bool,
    afs_wrap_fn: Option<Heap<crate::function::Function>>,
}

#[jsmethods]
impl AsyncIteratorRecord<'_> {
    /// `GetIterator(iterable, async)`: populate the iterator record, falling
    /// back to the sync iterator (wrapped via `afs_next`) when there is no
    /// `Symbol.asyncIterator` method. Use [`get_async_iterator`]. The global
    /// must have registered this hidden class with `add_to_global`.
    fn new(&self, scope: &Scope<'_>, iterable: HandleValue<'_>) -> Result<(), ExnThrown> {
        // The method lookups use the boxed value (so primitive iterables such as
        // strings work), but each iterator factory is called with the original
        // value as its `this`. `null` and `undefined` are not iterable.
        if iterable.is_null_or_undefined() {
            return Err(crate::error::throw_type_error(
                scope,
                c"value is not async iterable",
            ));
        }
        let obj = Object::from_value_coerce(scope, iterable)?;

        let async_key = scope.root_id(crate::symbol::get_well_known_key(
            scope,
            crate::native::SymbolCode::asyncIterator,
        ));
        let async_method = ensure_is_method(scope, obj.get_property_by_id(scope, async_key)?)?;

        match async_method {
            Some(method) => {
                let iter = Function::call(scope, iterable, method, EmptyArgs)?;
                let iter = Object::from_jsval_throwing(scope, iter, ())?;
                let next = iter.get_property(scope, c"next")?;
                let next = Object::from_jsval_throwing(scope, next, ())?;
                self.data_mut().iterator.set(iter);
                self.data_mut().next_method.set(next);
            }
            None => {
                let sync_key = scope.root_id(crate::symbol::get_well_known_key(
                    scope,
                    crate::native::SymbolCode::iterator,
                ));
                let sync_method =
                    ensure_is_method(scope, obj.get_property_by_id(scope, sync_key)?)?.ok_or_else(
                        || crate::error::throw_type_error(scope, c"value is not async iterable"),
                    )?;
                let sync_iter = Function::call(scope, iterable, sync_method, EmptyArgs)?;
                let sync_iter = Object::from_jsval_throwing(scope, sync_iter, ())?;
                let sync_next = sync_iter.get_property(scope, c"next")?;
                let sync_next = Object::from_jsval_throwing(scope, sync_next, ())?;
                self.data_mut().iterator.set(sync_iter);
                self.data_mut().sync_next = Some(Heap::from(sync_next));
                // `CreateAsyncFromSyncIterator`: the next method becomes a native
                // function that calls the sync `next` and awaits the yielded value.
                let afs = Function::new_callback(scope, c"", 1, afs_next, self)?;
                self.data_mut().next_method.set(*afs);
            }
        }
        Ok(())
    }
}

/// `GetIterator(iterable, async)` with the `CreateAsyncFromSyncIterator`
/// fallback: build an [`AsyncIteratorRecord`] for `iterable`, throwing a
/// `TypeError` when it is not (async or sync) iterable. The global must have
/// registered the hidden record class with `AsyncIteratorRecord::add_to_global`.
pub fn get_async_iterator<'s>(
    scope: &'s Scope<'_>,
    iterable: HandleValue<'_>,
) -> Result<AsyncIteratorRecord<'s>, ExnThrown> {
    AsyncIteratorRecord::new(scope, iterable)
}

impl<'s> AsyncIteratorRecord<'s> {
    /// `IteratorNext(iteratorRecord)`: invoke the cached `[[NextMethod]]` on
    /// the iterator with no arguments, returning the raw result value — for a
    /// well-behaved iterator, a promise of an iterator result. A thrown error
    /// is left pending as `Err`.
    pub fn call_next<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let iterator = self.data().iterator.get(scope);
        let next_method = self.data().next_method.get(scope);
        Function::call(scope, iterator, next_method, EmptyArgs)
    }

    fn return_promise_sync<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: HandleValue<'r>,
    ) -> Result<Promise<'r>, ExnThrown> {
        let iterator = self.data().iterator.get(scope);
        // Let _returnMethod_ be `GetMethod`(_iterator_, "return"). If abrupt,
        // return a promise rejected with its value.
        let return_method = iterator.get_property(scope, c"return")?;
        // If undefined, return a promise resolved with undefined.
        if return_method.is_null_or_undefined() {
            return Promise::new_resolved_with_value(scope, HandleValue::undefined());
        }
        if !crate::function::is_callable(return_method) {
            crate::error::throw_type_error(scope, c"iterator return is not callable");
            return Promise::new_rejected_with_pending_error(scope);
        }
        let return_result = Function::call(scope, iterator, return_method, &[reason])?;
        let return_promise = Promise::call_original_resolve(scope, return_result)?;
        // React with the result check; the continuation carries no state, so a
        // per-global shared function serves every call.
        let cb = get_or_init_shared_function(
            scope,
            return_result_check as *const () as usize,
            |scope| Function::new_callback(scope, c"", 1, return_result_check, ()),
        )?;
        return_promise.then(scope, Some(*cb), None)
    }

    /// The `return`-method teardown path (the async analogue of
    /// `IteratorClose`, as spelled out in e.g. `ReadableStream.from`'s cancel
    /// steps): look up `iterator.return`; when absent, resolve with undefined;
    /// otherwise call it with `reason` and react to the promise-wrapped result
    /// by checking it is an Object and resolving with undefined. A lookup or
    /// call error becomes a rejected promise. The returned promise is freshly
    /// derived and safe to expose.
    pub fn return_promise<'r>(
        &self,
        scope: &'r Scope<'_>,
        reason: HandleValue<'r>,
    ) -> Result<Promise<'r>, ExnThrown> {
        match self.return_promise_sync(scope, reason) {
            Ok(r) => Ok(r),
            Err(_) => Promise::new_rejected_with_pending_error(scope),
        }
    }
}

/// The fulfillment continuation of [`AsyncIteratorRecord::return_promise`]:
/// a non-object iterator result is a `TypeError`; otherwise resolve with
/// undefined.
fn return_result_check(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    iter_result_object(scope, args.get(0))?;
    Ok(value::undefined())
}

/// `GetMethod`-style check: undefined/null yields `None`; a non-callable
/// throws a `TypeError`; a callable is returned.
pub(crate) fn ensure_is_method<'r>(
    scope: &'r Scope<'_>,
    v: HandleValue<'r>,
) -> Result<Option<HandleValue<'r>>, ExnThrown> {
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    if !crate::function::is_callable(v) {
        return Err(crate::error::throw_type_error(
            scope,
            c"iterator method is not callable",
        ));
    }
    Ok(Some(v))
}

/// The async-from-sync `next`: call the sync iterator's `next`, then return a
/// promise resolving to `{ value: await result.value, done: result.done }`.
fn afs_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    match afs_next_sync(scope, payload) {
        Ok(r) => Ok(r.as_value()),
        Err(_) => Ok(Promise::new_rejected_with_pending_error(scope)?.as_value()),
    }
}

fn afs_next_sync<'s>(scope: &'s Scope<'_>, payload: HandleValue) -> Result<Promise<'s>, ExnThrown> {
    let record = cast_payload::<AsyncIteratorRecord>(scope, payload);
    let sync_iter = record.data().iterator.get(scope);
    let sync_next = record
        .data()
        .sync_next
        .as_ref()
        .expect("afs_next only wraps sync iterators")
        .get(scope);
    let result = Function::call(scope, sync_iter, sync_next, EmptyArgs)?;
    let result_obj = iter_result_object(scope, result)?;
    let done = iter_result_done(scope, &result_obj)?;
    let value = iter_result_value(scope, &result_obj)?;

    // `Let valueWrapper be PromiseResolve(%Promise%, value)`, split by value
    // type. An object must go through `Promise.resolve`: the `then` lookup
    // (thenable adoption) is author-observable, and a genuine promise passes
    // through unchanged. A primitive can never be a thenable, so its wrapper
    // would only be a fresh fulfilled promise whose reaction runs one microtask
    // later — park the value and `done` on the record and chain the wrap
    // continuation on the per-global resolved trigger instead: same timing,
    // nothing allocated. (No rejection continuation: the trigger never
    // rejects.) The slot is single-occupancy; see its field documentation.
    if !value.get().is_object() {
        record.data().pending_value.set(*value);
        record.data_mut().pending_done = done;
        if record.data().afs_wrap_fn.is_none() {
            let wrap = Function::new_callback(scope, c"", 1, afs_wrap_parked, payload)?;
            record.data_mut().afs_wrap_fn = Some(Heap::from(wrap));
        }
        let wrap = record
            .data()
            .afs_wrap_fn
            .as_ref()
            .expect("created above")
            .get(scope);
        let trigger = Promise::shared_resolved_undefined(scope)?;
        return trigger.then(scope, Some(*wrap), None);
    }
    let value_wrapper = Promise::call_original_resolve(scope, value)?;
    // The fulfillment continuation only closes over the `done` flag,
    // so a per-global shared function per flag value serves every iteration.
    let cb = if done {
        get_or_init_shared_function(
            scope,
            afs_value_fulfilled_done as *const () as usize,
            |scope| Function::new_callback(scope, c"", 1, afs_value_fulfilled_done, ()),
        )?
    } else {
        get_or_init_shared_function(
            scope,
            afs_value_fulfilled_not_done as *const () as usize,
            |scope| Function::new_callback(scope, c"", 1, afs_value_fulfilled_not_done, ()),
        )?
    };

    // `AsyncFromSyncIteratorContinuation` with `closeOnRejection == true`: unless the
    // iterator is already done, a rejection of the yielded value must close the
    // sync iterator (`IteratorClose` with the `throw` completion) before the
    // rejection propagates. When `done` is `true` there is nothing to close.
    // The continuation's payload — the record — never changes, so it is created
    // once and reused for every subsequent iteration.
    let on_rejected = if done {
        None
    } else {
        if record.data().afs_rejected_fn.is_none() {
            let rejected = Function::new_callback(scope, c"", 1, afs_value_rejected, payload)?;
            record.data_mut().afs_rejected_fn = Some(Heap::from(rejected));
        }
        Some(
            record
                .data()
                .afs_rejected_fn
                .as_ref()
                .expect("created above")
                .get(scope),
        )
    };
    value_wrapper.then(scope, Some(*cb), on_rejected.map(|f| *f))
}

/// The async-from-sync value rejection continuation: close the sync iterator,
/// then re-throw the original rejection (`IteratorClose` with a throw completion).
fn afs_value_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let record = cast_payload::<AsyncIteratorRecord>(scope, payload);
    let error = args.get(0);
    let sync_iter = record.data().iterator.get(scope);
    // `IteratorClose`: call the iterator's `return` (nothing happens if it is missing).
    // Its outcome is discarded — when the incoming completion is a throw, the
    // original error wins, so a throwing or absent `return` cannot mask it.
    if let Ok(return_method) = sync_iter.get_property(scope, c"return") {
        if crate::function::is_callable(return_method) {
            let _ = Function::call(scope, sync_iter, return_method, EmptyArgs);
        }
    }
    // Re-throw the original rejection.
    Err(crate::exception::set_pending(
        scope,
        error,
        crate::native::ExceptionStackBehavior::Capture,
    ))
}

/// The async-from-sync continuation for a parked primitive value: build the
/// iterator result from the record's `pending_value`/`pending_done` slot
/// (payload = the record). Runs one microtask after `afs_next`, mirroring the
/// timing of a `Promise.resolve(value).then(…)` reaction.
fn afs_wrap_parked(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let record = cast_payload::<AsyncIteratorRecord>(scope, payload);
    // Take the parked value: root it, then clear the slot so it is not kept
    // alive past this iteration.
    let value = record.data().pending_value.get(scope);
    record.data().pending_value.set(value::undefined());
    let done = record.data().pending_done;
    let result = create_iter_result(scope, value, done)?;
    Ok(result.as_value())
}

/// The async-from-sync value continuation for a `done` result: wrap the awaited
/// value into a done iterator-result object.
fn afs_value_fulfilled_done(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let result = create_iter_result(scope, args.get(0), true)?;
    Ok(result.as_value())
}

/// The async-from-sync value continuation for a not-`done` result: wrap the
/// awaited value into a not-done iterator-result object.
fn afs_value_fulfilled_not_done(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let result = create_iter_result(scope, args.get(0), false)?;
    Ok(result.as_value())
}

/// Build an ordinary `{ value, done }` iterator-result object (the spec's
/// `CreateIterResultObject`), returned as a value.
///
/// `value` and `done` are installed as own, enumerable, writable, configurable
/// data properties via `[[DefineOwnProperty]]` (`CreateDataPropertyOrThrow`), not
/// `[[Set]]`. The distinction is load-bearing: `[[Set]]` walks the prototype
/// chain, so a hostile accessor on `Object.prototype.value`/`done` would observe
/// the value and suppress the own property; the define path is immune.
pub fn create_iter_result<'r>(
    scope: &'r Scope<'_>,
    value: impl ToJSVal<'r>,
    done: bool,
) -> Result<Object<'r>, ExnThrown> {
    let attrs = crate::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;
    let value = value.to_jsval_throwing(scope)?;
    let obj = Object::new_plain(scope)?;
    let value_id = crate::class::get_or_init_property_id(scope, c"value")?;
    obj.define_value_by_id(scope, value_id, value, attrs)?;
    let done_id = crate::class::get_or_init_property_id(scope, c"done")?;
    obj.define_value_by_id(scope, done_id, done, attrs)?;
    Ok(obj)
}

/// A WebIDL default iterator object's kind.
///
/// <https://webidl.spec.whatwg.org/#default-iterator-object-kind>
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IterationKind {
    /// "`key`"
    Key,
    /// "`value`"
    Value,
    /// "`key+value`"
    #[default]
    KeyValue,
}

/// <https://webidl.spec.whatwg.org/#iterator-result>
/// The iterator result for a value pair (`key`, `value`) and a kind `kind`.
pub fn pair_iterator_result<'r>(
    scope: &'r Scope<'_>,
    key: impl ToJSVal<'r>,
    value: impl ToJSVal<'r>,
    kind: IterationKind,
) -> Result<Object<'r>, ExnThrown> {
    // Step 1: Let _result_ be a value determined by the value of _kind_:
    let result = match kind {
        // "`key`":
        // Let _idlKey_ be _pair_'s `key`.
        // Let _key_ be the result of `converting` _idlKey_ to a JavaScript value.
        // _result_ is _key_.
        IterationKind::Key => key.to_jsval_throwing(scope)?,
        // "`value`":
        // Let _idlValue_ be _pair_'s `value`.
        // Let _value_ be the result of `converting` _idlValue_ to a JavaScript value.
        // _result_ is _value_.
        IterationKind::Value => value.to_jsval_throwing(scope)?,
        // "`key+value`":
        // Let _idlKey_ be _pair_'s `key`.
        // Let _idlValue_ be _pair_'s `value`.
        // Let _key_ be the result of `converting` _idlKey_ to a JavaScript value.
        // Let _value_ be the result of `converting` _idlValue_ to a JavaScript value.
        // Let _array_ be ! ArrayCreate(2).
        // Perform ! CreateDataPropertyOrThrow(_array_, "`0`", _key_).
        // Perform ! CreateDataPropertyOrThrow(_array_, "`1`", _value_).
        // _result_ is _array_.
        IterationKind::KeyValue => {
            let key = key.to_jsval_throwing(scope)?;
            let value = value.to_jsval_throwing(scope)?;
            crate::Array::with_contents(scope, &[key, value])?.to_jsval_throwing(scope)?
        }
    };
    // Step 2: Return CreateIteratorResultObject(_result_, false).
    create_iter_result(scope, result, false)
}

/// <https://webidl.spec.whatwg.org/#js-iterable>
/// The `forEach` method steps of an interface with a pair iterator, from the step that converts
/// `callback` onwards. `target` is the `this` value, already checked to implement the interface.
///
/// `pair_at(i)` returns the `i`th entry of `target`'s current list of value pairs to iterate over,
/// converted to JS values, or `None` if the list has fewer entries. It is called again after every
/// callback invocation, since the callback can change the list.
pub fn for_each_pair<'r>(
    scope: &'r Scope<'_>,
    target: impl ToJSVal<'r>,
    callback: crate::Callable<'_>,
    this_arg: HandleValue<'_>,
    mut pair_at: impl FnMut(usize) -> Result<Option<(HandleValue<'r>, HandleValue<'r>)>, ExnThrown>,
) -> Result<(), ExnThrown> {
    // Step 4: Let _idlCallback_ be _callback_, `converted` to a ``Function``.
    //         (Done by the caller's argument conversion.)
    // Step 5: Let _idlObject_ be the IDL `interface type` value that represents a reference to
    //         _jsValue_.
    let idl_object = target.to_jsval_throwing(scope)?;
    // Step 6: Let _pairs_ be _idlObject_'s list of `value pairs to iterate over`.
    //         (`pair_at` reads the current list.)
    // Step 7: Let _i_ be 0.
    let mut i = 0;
    // Step 8: While _i_ < _pairs_'s `size`:
    // Step 8.1: Let _pair_ be _pairs_[_i_].
    while let Some((key, value)) = pair_at(i)? {
        // Step 8.2: `Invoke` _idlCallback_ with « _pair_'s `value`, _pair_'s `key`, _idlObject_ »
        //           and with _thisArg_ as the `callback this value`.
        Function::call(scope, this_arg, callback, &[value, key, idl_object])?;
        // Step 8.3: Set _pairs_ to _idlObject_'s current list of `value pairs to iterate over`.
        //           (It might have changed.)
        //           (The next `pair_at` call reads the current list.)
        // Step 8.4: Set _i_ to _i_ + 1.
        i += 1;
    }
    Ok(())
}

/// The iterator-result Object requirement shared by every result consumer: a
/// non-object result is a `TypeError`.
pub fn iter_result_object<'r>(
    scope: &'r Scope<'_>,
    result: HandleValue<'_>,
) -> Result<Object<'r>, ExnThrown> {
    Object::from_raw_value(scope, result.get())
        .map_err(|_| crate::error::throw_type_error(scope, c"iterator result is not an object"))
}

/// `IteratorComplete(iterResult)`: `Get(iterResult, "done")`, coerced to a
/// boolean.
pub fn iter_result_done(scope: &Scope<'_>, result: &Object<'_>) -> Result<bool, ExnThrown> {
    Ok(result.get_property(scope, c"done")?.get().to_boolean())
}

/// `IteratorValue(iterResult)`: `Get(iterResult, "value")`.
pub fn iter_result_value<'r>(
    scope: &'r Scope<'_>,
    result: &Object<'_>,
) -> Result<HandleValue<'r>, ExnThrown> {
    result.get_property(scope, c"value")
}
