// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:async_hooks`. See <https://nodejs.org/api/async_context.html> and
//! <https://nodejs.org/api/async_hooks.html>.
//!
//! `AsyncLocalStorage` and `AsyncResource` keep their stores in the async context
//! ([`js::async_context`]) as Node's `AsyncContextFrame`s do: the context is a `Map` from each
//! `AsyncLocalStorage` to its store, and setting a store replaces the context with a copy that
//! holds it. Promise reactions, timers and other event-loop tasks run with the context that was
//! current when they were created.
//!
//! `executionAsyncId()` returns 1 and `triggerAsyncId()` returns 0, Node's values at the top level
//! of a script. `AsyncResource` IDs count up from 2. `createHook` returns a hook whose callbacks
//! are never called.

use js::async_context::Snapshot;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};

/// The `Map` the current async context is, if there is one.
pub(crate) fn current_frame<'s>(scope: &'s Scope<'_>) -> Option<js::Map<'s>> {
    js::async_context::current(scope).and_then(|context| context.cast::<js::Map>().ok())
}

/// The body of the callback [`derived_frame`] copies entries with, whose payload is the new frame.
fn copy_entry(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let frame = Object::from_value(scope, payload)
        .map_err(|e| e.throw(scope))?
        .cast::<js::Map>()
        .map_err(|_| js::error::TypeError("not a Map".into()).throw(scope))?;
    frame.insert(scope, args.get(1), args.get(0))?;
    Ok(js::value::undefined())
}

/// A copy of the current async context's frame in which `storage` maps to `store`, or doesn't
/// appear if `store` is `None`.
pub(crate) fn derived_frame<'s>(
    scope: &'s Scope<'_>,
    storage: Object<'_>,
    store: Option<HandleValue<'_>>,
) -> Result<js::Map<'s>, ExnThrown> {
    let frame = js::Map::new(scope)?;
    if let Some(current) = current_frame(scope) {
        let copy = Function::new_callback(scope, c"", 2, copy_entry, *frame)?;
        let copy = (*copy).to_jsval_throwing(scope)?;
        current.for_each(scope, copy, scope.root_value(js::value::undefined()))?;
    }
    let key = storage.to_jsval_throwing(scope)?;
    match store {
        Some(store) => frame.insert(scope, key, store)?,
        None => {
            frame.delete(scope, key)?;
        }
    }
    Ok(frame)
}

/// Call `callback` with `this` and `args` while `context` is current, then restore the context
/// that was current before, whether or not `callback` throws.
pub(crate) fn call_in(
    scope: &Scope<'_>,
    context: Snapshot,
    callback: HandleValue<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let previous = context.enter();
    let result = Function::call(scope, this, callback, args);
    previous.enter();
    result.map(|value| value.get())
}

/// A snapshot of `frame`, a frame object or `undefined`.
fn snapshot_of(scope: &Scope<'_>, frame: HandleValue<'_>) -> Snapshot {
    Snapshot::of(Object::from_value(scope, frame).ok())
}

/// The body of a function [`bind_to_current`] returns, whose payload is `[frame, fn, thisArg]`:
/// call `fn` with the frame as the async context, and with `thisArg`, or the call's `this` if
/// `thisArg` is `undefined`.
fn call_bound(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let frame = state.get_element(scope, 0)?;
    let callback = state.get_element(scope, 1)?;
    let mut this = state.get_element(scope, 2)?;
    if this.is_undefined() {
        this = scope.root_value(args.this());
    }
    let call_args: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    call_in(scope, snapshot_of(scope, frame), callback, this, &call_args)
}

/// A function calling `callback` with the current async context, and with `this_arg`, or the
/// call's `this` if `this_arg` is `undefined`. Its `length` is `callback`'s.
fn bind_to_current<'s>(
    scope: &'s Scope<'_>,
    callback: Object<'_>,
    this_arg: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let frame = match js::async_context::current(scope) {
        Some(frame) => frame.to_jsval_throwing(scope)?,
        None => scope.root_value(js::value::undefined()),
    };
    let state = js::Array::with_contents(
        scope,
        &[
            frame.get(),
            callback.to_jsval_raw_throwing(scope)?,
            this_arg.get(),
        ],
    )?;
    let length = callback.get_property(scope, c"length")?;
    let length = if length.is_number() {
        length.to_number() as u32
    } else {
        0
    };
    let name = if this_arg.is_undefined() {
        c"bound"
    } else {
        c"bound runInAsyncScope"
    };
    let bound = Function::new_callback(scope, name, length, call_bound, *state)?;
    Ok(*bound)
}

/// The key of the realm's `WeakMap` from each `AsyncLocalStorage` and `AsyncResource` to the
/// object holding its state.
struct States;

/// The object holding the state of `this`, an `AsyncLocalStorage` or `AsyncResource`, created if
/// `create` is set. Without `create`, a `TypeError` is thrown if `this` has no state.
fn state_of<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    create: bool,
) -> Result<Object<'s>, ExnThrown> {
    let states = match js::class::realm_object::<States>(scope) {
        Some(states) => states
            .cast::<js::WeakMap>()
            .map_err(|_| js::error::TypeError("not a WeakMap".into()).throw(scope))?,
        None => {
            let states = js::WeakMap::new(scope)?;
            js::class::register_realm_object::<States>(scope, *states);
            states
        }
    };
    let key = scope.root_value(this.to_jsval_raw_throwing(scope)?);
    if create {
        let state = Object::new_plain(scope)?;
        states.insert(scope, key, state.to_jsval_throwing(scope)?)?;
        return Ok(state);
    }
    Object::from_value(scope, states.lookup(scope, key)?)
        .map_err(|_| js::error::TypeError("Illegal invocation".into()).throw(scope))
}

/// The body of the function `AsyncLocalStorage.snapshot()` returns, whose payload is the frame
/// current when it was created: call `args[0]` with the remaining arguments and that frame as the
/// async context.
fn call_in_snapshot(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let call_args: Vec<HandleValue<'_>> = (1..args.len()).map(|index| args.get(index)).collect();
    let undefined = scope.root_value(js::value::undefined());
    call_in(
        scope,
        snapshot_of(scope, payload),
        args.get(0),
        undefined,
        &call_args,
    )
}

#[core_runtime::jsfunction_class(name = "AsyncLocalStorage", class_syntax)]
pub mod async_local_storage {
    use super::*;

    /// <https://nodejs.org/api/async_context.html#new-asynclocalstorageoptions>
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let options = options.and_then(|options| Object::from_value(scope, options).ok());
        let (default_value, name) = match options {
            Some(options) => (
                options.get_property(scope, c"defaultValue")?,
                options.get_property(scope, c"name")?,
            ),
            None => (
                scope.root_value(js::value::undefined()),
                scope.root_value(js::value::undefined()),
            ),
        };
        let name = if name.is_undefined() {
            js::JSString::from_str(scope, "")?.to_jsval_throwing(scope)?
        } else {
            name
        };
        let state = state_of(scope, this, true)?;
        state.set_property(scope, c"defaultValue", default_value)?;
        state.set_property(scope, c"name", name)
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstoragerunstore-callback-args>
    pub fn run<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        store: HandleValue<'_>,
        callback: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let frame = derived_frame(scope, this, Some(store))?;
        let undefined = scope.root_value(js::value::undefined());
        Ok(scope.root_value(call_in(
            scope,
            Snapshot::of(Some(*frame)),
            callback,
            undefined,
            &args,
        )?))
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstorageexitcallback-args>
    pub fn exit<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let undefined = scope.root_value(js::value::undefined());
        let frame = derived_frame(scope, this, Some(undefined))?;
        Ok(scope.root_value(call_in(
            scope,
            Snapshot::of(Some(*frame)),
            callback,
            undefined,
            &args,
        )?))
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstoragegetstore>: the store, or the
    /// `defaultValue` option if no store was set.
    pub fn get_store<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let key = scope.root_value(this.to_jsval_raw_throwing(scope)?);
        match current_frame(scope) {
            Some(frame) if frame.has(scope, key)? => frame.lookup(scope, key),
            _ => state_of(scope, this, false)?.get_property(scope, c"defaultValue"),
        }
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstorageenterwithstore>: the store
    /// stays set for the rest of the current task or job, and for the work it creates.
    pub fn enter_with(
        scope: &Scope<'_>,
        this: Object<'_>,
        store: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let frame = derived_frame(scope, this, Some(store))?;
        Snapshot::of(Some(*frame)).enter();
        Ok(())
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstoragedisable>: removes the store
    /// from the current context, including from the work that captured it.
    pub fn disable(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if let Some(frame) = current_frame(scope) {
            frame.delete(scope, this.to_jsval_throwing(scope)?)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/async_context.html#static-method-asynclocalstoragebindfn>
    #[static_method]
    pub fn bind<'s>(scope: &'s Scope<'_>, callback: HandleValue<'_>) -> Result<Object<'s>, Thrown> {
        let callback = validate_function(scope, callback, "fn")?;
        let undefined = scope.root_value(js::value::undefined());
        Ok(bind_to_current(scope, callback, undefined)?)
    }

    /// <https://nodejs.org/api/async_context.html#static-method-asynclocalstoragesnapshot>
    #[static_method]
    pub fn snapshot<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        let frame = match js::async_context::current(scope) {
            Some(frame) => frame.to_jsval_throwing(scope)?,
            None => scope.root_value(js::value::undefined()),
        };
        Ok(*Function::new_callback(
            scope,
            c"runInAsyncScope",
            1,
            call_in_snapshot,
            frame,
        )?)
    }

    /// Define the `name` accessor.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"get name", 0, storage_name, ())?;
        proto.define_accessor(scope, c"name", Some(*getter), None, 0)
    }
}

/// `asyncLocalStorage.name`: the `name` option, or the empty string.
fn storage_name(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    Ok(state_of(scope, this, false)?
        .get_property(scope, c"name")?
        .get())
}

/// The key the per-realm counter of async resource IDs is kept under.
static ASYNC_ID_KEY: u8 = 0;

#[core_runtime::jsfunction_class(name = "AsyncResource", class_syntax)]
pub mod async_resource {
    use super::*;

    /// <https://nodejs.org/api/async_context.html#new-asyncresourcetype-options>: the
    /// `requireManualDestroy` option has no effect.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        r#type: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if !r#type.is_string() {
            return Err(
                NodeError::invalid_arg_type(scope, "type", "of type string", r#type).into(),
            );
        }
        let trigger = match options {
            Some(options) if options.is_number() => options,
            Some(options) if !options.is_undefined() => {
                let options = Object::from_value(scope, options).map_err(|e| e.throw(scope))?;
                options.get_property(scope, c"triggerAsyncId")?
            }
            _ => scope.root_value(js::value::undefined()),
        };
        let trigger = if trigger.is_undefined() {
            1.0
        } else {
            const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;
            let number = trigger.is_number().then(|| trigger.to_number());
            match number {
                Some(number)
                    if number.fract() == 0.0 && (-1.0..=MAX_SAFE_INTEGER).contains(&number) =>
                {
                    number
                }
                _ => {
                    let string = scope.global().get_property(scope, c"String")?;
                    let undefined = scope.root_value(js::value::undefined());
                    let shown = Function::call(scope, undefined, string, &[trigger])?;
                    let shown = js::JSString::from_value(scope, shown)?.to_utf8(scope)?;
                    return Err(NodeError::new(
                        ErrorKind::RangeError,
                        "ERR_INVALID_ASYNC_ID",
                        format!("Invalid triggerAsyncId value: {shown}"),
                    )
                    .into());
                }
            }
        };
        let frame = match js::async_context::current(scope) {
            Some(frame) => frame.to_jsval_throwing(scope)?,
            None => scope.root_value(js::value::undefined()),
        };
        // IDs start at 2, after `executionAsyncId()`.
        let id = js::class::next_global_counter(scope, &ASYNC_ID_KEY as *const u8 as usize) + 2;
        let state = state_of(scope, this, true)?;
        state.set_property(scope, c"frame", frame)?;
        state.set_property(scope, c"asyncId", id as f64)?;
        state.set_property(scope, c"triggerAsyncId", trigger)?;
        Ok(())
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourceruninasyncscopefn-thisarg-args>
    pub fn run_in_async_scope<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let frame = state_of(scope, this, false)?.get_property(scope, c"frame")?;
        let this_arg = this_arg.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        Ok(scope.root_value(call_in(
            scope,
            snapshot_of(scope, frame),
            callback,
            this_arg,
            &args,
        )?))
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourceemitdestroy>
    pub fn emit_destroy<'s>(this: Object<'s>) -> Object<'s> {
        this
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourceasyncid>
    pub fn async_id(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, ExnThrown> {
        let id = state_of(scope, this, false)?.get_property(scope, c"asyncId")?;
        Ok(f64::from_jsval(scope, id, ()).unwrap_or(0.0))
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourcetriggerasyncid>
    pub fn trigger_async_id(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, ExnThrown> {
        let id = state_of(scope, this, false)?.get_property(scope, c"triggerAsyncId")?;
        Ok(f64::from_jsval(scope, id, ()).unwrap_or(0.0))
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourcebindfn-thisarg>
    pub fn bind<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
        this_arg: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let callback = validate_function(scope, callback, "fn")?;
        let frame = state_of(scope, this, false)?.get_property(scope, c"frame")?;
        let this_arg = this_arg.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        let previous = snapshot_of(scope, frame).enter();
        let bound = bind_to_current(scope, callback, this_arg);
        previous.enter();
        let bound = bound?;
        bound.define_property(scope, c"asyncResource", this, 0)?;
        Ok(bound)
    }

    /// <https://nodejs.org/api/async_context.html#static-method-asyncresourcebindfn-type-thisarg>
    #[static_method(name = "bind")]
    pub fn bind_static<'s>(
        scope: &'s Scope<'_>,
        callback: HandleValue<'_>,
        r#type: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let callback = validate_function(scope, callback, "fn")?;
        let ctor = class_constructor(scope)?;
        let r#type = match r#type.filter(|value| value.is_string()) {
            Some(r#type) => r#type,
            None => {
                js::JSString::from_str(scope, "bound-anonymous-fn")?.to_jsval_throwing(scope)?
            }
        };
        let resource = Function::construct(scope, ctor, &[r#type])?;
        let resource = Object::from_value(scope, resource)?;
        bind(
            scope,
            resource,
            callback.to_jsval_throwing(scope)?,
            this_arg,
        )
    }
}

/// The `enable()` and `disable()` of a hook `createHook` returns: return `this`.
fn return_this(
    _scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(args.this())
}

/// The key of the realm's `executionAsyncResource()` object.
struct ExecutionAsyncResource;

#[core_runtime::jsmodule(node_args, name = "node:async_hooks", default_namespace)]
pub mod module {
    use super::*;

    pub use super::async_local_storage as AsyncLocalStorage;
    pub use super::async_resource as AsyncResource;

    /// <https://nodejs.org/api/async_hooks.html#async_hooksexecutionasyncid>: always 1.
    pub fn execution_async_id() -> f64 {
        1.0
    }

    /// <https://nodejs.org/api/async_hooks.html#async_hookstriggerasyncid>: always 0.
    pub fn trigger_async_id() -> f64 {
        0.0
    }

    /// <https://nodejs.org/api/async_hooks.html#async_hookscreatehookcallbacks>: a hook whose
    /// `enable()` and `disable()` return it, and whose callbacks are never called. Each callback
    /// must be a function or `undefined`.
    pub fn create_hook<'s>(
        scope: &'s Scope<'_>,
        callbacks: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let callbacks = Object::from_value(scope, callbacks).map_err(|e| e.throw(scope))?;
        for (name, qualified) in [
            (c"init", "hook.init"),
            (c"before", "hook.before"),
            (c"after", "hook.after"),
            (c"destroy", "hook.destroy"),
            (c"promiseResolve", "hook.promiseResolve"),
        ] {
            let callback = callbacks.get_property(scope, name)?;
            if !callback.is_undefined()
                && !Object::from_value(scope, callback).is_ok_and(|callback| callback.is_callable())
            {
                return Err(NodeError::new(
                    ErrorKind::TypeError,
                    "ERR_ASYNC_CALLBACK",
                    format!("{qualified} must be a function"),
                )
                .into());
            }
        }
        let hook = Object::new_plain(scope)?;
        for name in [c"enable", c"disable"] {
            let method = Function::new_callback(scope, name, 0, return_this, ())?;
            hook.set_property(scope, name, *method)?;
        }
        Ok(hook)
    }

    /// <https://nodejs.org/api/async_hooks.html#async_hooksexecutionasyncresource>: an empty
    /// object, the same one on every call in a realm.
    pub fn execution_async_resource<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        if let Some(resource) = js::class::realm_object::<ExecutionAsyncResource>(scope) {
            return Ok(resource);
        }
        let resource = Object::new_plain(scope)?;
        js::class::register_realm_object::<ExecutionAsyncResource>(scope, resource);
        Ok(resource)
    }
}
