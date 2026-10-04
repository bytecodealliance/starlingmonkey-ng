// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `MockTracker` and `MockFunctionContext`: <https://nodejs.org/api/test.html#class-mocktracker>.
//!
//! A mock function is a `Proxy` of the function it mocks, whose `mock` property is its
//! `MockFunctionContext`. Calls and constructions go to the context's implementation, and are
//! recorded in its `calls`. Mock timers and module mocks are not implemented.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::{length, objects_array, push};
use crate::errors::{validate_function, validate_plain_object, NodeError, Thrown};
use crate::stream::{as_object, call_method, is_function, is_str, Props};

/// The key of the realm's `WeakMap` from each `MockTracker` and `MockFunctionContext` to the
/// object holding its state.
struct States;

/// The object holding the state of `this`, a `MockTracker` or `MockFunctionContext`, created if
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

/// The realm's global `name`, as an object.
fn global<'s>(
    scope: &'s Scope<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Object<'s>, ExnThrown> {
    let value = scope.global().get_property(scope, name)?;
    as_object(scope, value).ok_or_else(|| js::error::TypeError("not an object".into()).throw(scope))
}

/// A new `MockTracker`.
pub(super) fn new_tracker<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let ctor = mock_tracker::class_constructor(scope)?;
    Function::construct(scope, ctor, &[] as &[Value])
}

/// `tracker.reset()`: restore the mocks `tracker` created, and forget them.
pub(super) fn reset_tracker(scope: &Scope<'_>, tracker: Object<'_>) -> Result<(), ExnThrown> {
    restore_all(scope, tracker)?;
    state_of(scope, tracker, false)?.set_property(scope, c"mocks", *js::Array::new(scope, 0)?)
}

/// `tracker.restoreAll()`: restore the mocks `tracker` created.
fn restore_all(scope: &Scope<'_>, tracker: Object<'_>) -> Result<(), ExnThrown> {
    let mocks = objects_array(scope, state_of(scope, tracker, false)?, c"mocks")?;
    for index in 0..length(scope, mocks)? {
        if let Some(ctx) = as_object(scope, mocks.get_element(scope, index)?) {
            restore(scope, ctx)?;
        }
    }
    Ok(())
}

/// `ctx.restore()`: define the mocked property of a method mock with its original descriptor, or
/// make a function mock call the original function.
fn restore(scope: &Scope<'_>, ctx: Object<'_>) -> Result<(), ExnThrown> {
    let state = state_of(scope, ctx, false)?;
    let Some(restore) = state.obj(scope, c"restore")? else {
        return Ok(());
    };
    let method_name = restore.get_property(scope, c"methodName")?;
    if method_name.is_string() || method_name.is_symbol() {
        let object = restore.get_property(scope, c"object")?;
        let descriptor = restore.get_property(scope, c"descriptor")?;
        call_method(
            scope,
            global(scope, c"Object")?,
            c"defineProperty",
            &[object, method_name, descriptor],
        )?;
    } else {
        state.set_property(
            scope,
            c"implementation",
            restore.get_property(scope, c"original")?,
        )?;
    }
    Ok(())
}

/// The empty function a mock of no function mocks: a function the realm's `Function` constructor
/// creates, named `original` as the parameter it is the default of in Node.js.
fn empty_function<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let function = Function::construct(scope, global(scope, c"Function")?, &[] as &[Value])?;
    function.define_property(
        scope,
        c"name",
        "original",
        js::class_spec::JSPROP_READONLY as u32,
    )?;
    function.to_jsval_throwing(scope)
}

/// `ERR_INVALID_ARG_TYPE` unless `times` is `Infinity` or an integer of at least 1.
fn validate_times(scope: &Scope<'_>, times: HandleValue<'_>, name: &str) -> Result<(), Thrown> {
    if times.is_number() && times.to_number() == f64::INFINITY {
        return Ok(());
    }
    crate::errors::validate_integer(scope, times, name, 1.0, 9007199254740991.0)?;
    Ok(())
}

/// The `times` option of `options`, or `Infinity` if it is `undefined`.
fn times_option<'s>(
    scope: &'s Scope<'_>,
    options: Object<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let value = options.get_property(scope, c"times")?;
    Ok(if value.is_undefined() {
        scope.root_value(js::value::from_f64(f64::INFINITY))
    } else {
        value
    })
}

/// A new `MockFunctionContext` calling `implementation` at most `times` times before it is
/// restored with `restore`.
fn new_context<'s>(
    scope: &'s Scope<'_>,
    implementation: HandleValue<'_>,
    restore: Object<'_>,
    times: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let ctor = mock_function_context::class_constructor(scope)?;
    let ctx = Function::construct(scope, ctor, &[] as &[Value])?;
    let state = state_of(scope, ctx, false)?;
    state.set_property(scope, c"implementation", implementation)?;
    state.set_property(scope, c"restore", restore)?;
    state.set_property(scope, c"times", times)?;
    Ok(ctx)
}

/// A mock of `target` whose calls `ctx` handles, which `tracker` restores.
fn setup_mock<'s>(
    scope: &'s Scope<'_>,
    tracker: Object<'_>,
    ctx: Object<'_>,
    target: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let handler = Object::new_plain(scope)?;
    let apply = Function::new_callback(scope, c"apply", 3, mock_apply, ctx)?;
    let construct = Function::new_callback(scope, c"construct", 3, mock_construct, ctx)?;
    let get = Function::new_callback(scope, c"get", 3, mock_get, ctx)?;
    handler.set_property(scope, c"apply", *apply)?;
    handler.set_property(scope, c"construct", *construct)?;
    handler.set_property(scope, c"get", *get)?;
    let proxy = Function::construct(
        scope,
        global(scope, c"Proxy")?,
        &[
            scope.root_value(target.get()),
            handler.to_jsval_throwing(scope)?,
        ],
    )?;
    push(
        scope,
        objects_array(scope, state_of(scope, tracker, false)?, c"mocks")?,
        ctx,
    )?;
    Ok(proxy)
}

/// The implementation the next call of the mock of `ctx` calls: the one `mockImplementationOnce`
/// set for it, or the context's implementation. The mock is restored before its last call.
fn next_implementation<'s>(
    scope: &'s Scope<'_>,
    ctx: Object<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let state = state_of(scope, ctx, false)?;
    let next = length(scope, objects_array(scope, state, c"calls")?)?;
    let key = scope.root_value(js::value::from_f64(f64::from(next)));
    let once = state
        .obj(scope, c"once")?
        .and_then(|once| once.cast::<js::Map>().ok());
    let mut implementation = state.get_property(scope, c"implementation")?;
    if let Some(once) = once {
        let replacement = once.lookup(scope, key)?;
        if !replacement.is_undefined() {
            implementation = replacement;
        }
        once.delete(scope, key)?;
    }
    if f64::from(next) + 1.0 == state.num(scope, c"times")? {
        restore(scope, ctx)?;
    }
    Ok(implementation)
}

/// Record a call of the mock of `ctx`.
#[allow(clippy::too_many_arguments)]
fn track_call(
    scope: &Scope<'_>,
    ctx: Object<'_>,
    arguments: HandleValue<'_>,
    error: HandleValue<'_>,
    result: HandleValue<'_>,
    target: HandleValue<'_>,
    this: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let call = Object::new_plain(scope)?;
    call.set_prototype(scope, js::native::HandleObject::null())?;
    call.set_property(scope, c"arguments", arguments)?;
    call.set_property(scope, c"error", error)?;
    call.set_property(scope, c"result", result)?;
    let stack = Function::construct(scope, global(scope, c"Error")?, &[] as &[Value])?;
    call.set_property(scope, c"stack", stack)?;
    call.set_property(scope, c"target", target)?;
    call.set_property(scope, c"this", this)?;
    push(
        scope,
        objects_array(scope, state_of(scope, ctx, false)?, c"calls")?,
        call,
    )
}

/// The elements of the array `list`.
fn list_elements<'s>(
    scope: &'s Scope<'_>,
    list: HandleValue<'_>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let Some(list) = as_object(scope, list) else {
        return Ok(Vec::new());
    };
    (0..length(scope, list)?)
        .map(|index| list.get_element(scope, index))
        .collect()
}

/// Throw `error` again.
fn rethrow(scope: &Scope<'_>, error: HandleValue<'_>) -> ExnThrown {
    js::exception::set_pending(
        scope,
        error,
        js::native::ExceptionStackBehavior::DoNotCapture,
    );
    ExnThrown
}

/// The `apply` trap of a mock, whose payload is its context: call the next implementation and
/// record the call.
fn mock_apply(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let ctx = as_object(scope, payload).ok_or(ExnThrown)?;
    let this = args.get(1);
    let list = args.get(2);
    let implementation = next_implementation(scope, ctx)?;
    let call_args = list_elements(scope, list)?;
    let undefined = HandleValue::undefined();
    match Function::call(scope, this, implementation, &call_args) {
        Ok(result) => {
            let result = scope.root_value(result.get());
            track_call(scope, ctx, list, undefined, result, undefined, this)?;
            Ok(result.get())
        }
        Err(ExnThrown) => {
            let error = crate::stream::take_exception(scope);
            track_call(scope, ctx, list, error, undefined, undefined, this)?;
            Err(rethrow(scope, error))
        }
    }
}

/// The `construct` trap of a mock, whose payload is its context: construct the next
/// implementation and record the construction.
fn mock_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let ctx = as_object(scope, payload).ok_or(ExnThrown)?;
    let target = args.get(0);
    let list = args.get(1);
    let new_target = as_object(scope, args.get(2)).ok_or(ExnThrown)?;
    let implementation = next_implementation(scope, ctx)?;
    let call_args = list_elements(scope, list)?;
    let undefined = HandleValue::undefined();
    match Function::construct_with_new_target(
        scope,
        implementation,
        new_target.handle(),
        &call_args,
    ) {
        Ok(result) => {
            let result = result.to_jsval_throwing(scope)?;
            track_call(scope, ctx, list, undefined, result, target, result)?;
            Ok(result.get())
        }
        Err(ExnThrown) => {
            let error = crate::stream::take_exception(scope);
            track_call(scope, ctx, list, error, undefined, target, undefined)?;
            Err(rethrow(scope, error))
        }
    }
}

/// The `get` trap of a mock, whose payload is its context: the context for `mock`, and the mocked
/// function's property otherwise.
fn mock_get(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let property = args.get(1);
    if is_str(scope, property, c"mock")? {
        return Ok(payload.get());
    }
    let reflect = global(scope, c"Reflect")?;
    let result = call_method(
        scope,
        reflect,
        c"get",
        &[args.get(0), property, args.get(2)],
    )?;
    Ok(result.get())
}

/// `Object.getOwnPropertyDescriptor` of `name` on `object` or the first object on its prototype
/// chain that has it, or `undefined`.
fn find_descriptor<'s>(
    scope: &'s Scope<'_>,
    object: HandleValue<'_>,
    name: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let object_ctor = global(scope, c"Object")?;
    let mut host = scope.root_value(object.get());
    while !host.is_null() && !host.is_undefined() {
        let descriptor = call_method(
            scope,
            object_ctor,
            c"getOwnPropertyDescriptor",
            &[host, name],
        )?;
        if !descriptor.is_undefined() {
            return Ok(descriptor);
        }
        host = call_method(scope, object_ctor, c"getPrototypeOf", &[host])?;
    }
    Ok(scope.root_value(js::value::undefined()))
}

/// Whether the option `name` of `options` is `true`, or `ERR_INVALID_ARG_TYPE` if it is neither
/// `undefined` nor a boolean.
fn boolean_option(
    scope: &Scope<'_>,
    options: Object<'_>,
    name: &'static std::ffi::CStr,
    label: &str,
) -> Result<bool, Thrown> {
    let value = options.get_property(scope, name)?;
    if value.is_undefined() {
        return Ok(false);
    }
    if !value.is_boolean() {
        return Err(NodeError::invalid_arg_type(scope, label, "of type boolean", value).into());
    }
    Ok(value.to_boolean())
}

/// `mock.method(object, methodName, implementation, options)`, with the options object `options`
/// and the `getter` or `setter` option given by `accessor`, if any.
fn mock_method<'s>(
    scope: &'s Scope<'_>,
    tracker: Object<'_>,
    object: HandleValue<'_>,
    method_name: HandleValue<'_>,
    implementation: Option<HandleValue<'_>>,
    options: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    if !method_name.is_string() && !method_name.is_symbol() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "methodName",
            "of type string or symbol",
            method_name,
        )
        .into());
    }
    if !is_function(scope, object) {
        validate_plain_object(scope, object, "object", false)?;
    }
    let mut implementation = implementation.filter(|value| !value.is_undefined());
    let mut options = options.filter(|value| !value.is_undefined());
    if let Some(value) = implementation {
        if value.is_object() && !is_function(scope, value) {
            options = Some(value);
            implementation = None;
        }
    }
    if let Some(value) = implementation {
        validate_function(scope, value, "implementation")?;
    }
    let options = match options {
        Some(options) => validate_plain_object(scope, options, "options", false)?,
        None => Object::new_plain(scope)?,
    };
    let getter = boolean_option(scope, options, c"getter", "options.getter")?;
    let setter = boolean_option(scope, options, c"setter", "options.setter")?;
    let times = times_option(scope, options)?;
    validate_times(scope, times, "options.times")?;
    if getter && setter {
        let value = scope.root_value(js::value::from_bool(setter));
        return Err(NodeError::invalid_arg_value(
            scope,
            "options.setter",
            value,
            "cannot be used with 'options.getter'",
        )
        .into());
    }
    let descriptor = find_descriptor(scope, object, method_name)?;
    let original = match as_object(scope, descriptor) {
        Some(descriptor) => descriptor.get_property(
            scope,
            if getter {
                c"get"
            } else if setter {
                c"set"
            } else {
                c"value"
            },
        )?,
        None => scope.root_value(js::value::undefined()),
    };
    if !is_function(scope, original) {
        return Err(NodeError::invalid_arg_value(
            scope,
            "methodName",
            original,
            "must be a method",
        )
        .into());
    }
    let restore_info = Object::new_plain(scope)?;
    restore_info.set_property(scope, c"descriptor", descriptor)?;
    restore_info.set_property(scope, c"object", object)?;
    restore_info.set_property(scope, c"methodName", method_name)?;
    let ctx = new_context(
        scope,
        implementation.unwrap_or(original),
        restore_info,
        times,
    )?;
    let mock = setup_mock(scope, tracker, ctx, original)?;
    let descriptor = as_object(scope, descriptor).ok_or(ExnThrown)?;
    let mock_descriptor = Object::new_plain(scope)?;
    mock_descriptor.set_property(
        scope,
        c"configurable",
        descriptor.get_property(scope, c"configurable")?,
    )?;
    mock_descriptor.set_property(
        scope,
        c"enumerable",
        descriptor.get_property(scope, c"enumerable")?,
    )?;
    if getter {
        mock_descriptor.set_property(scope, c"get", mock)?;
        mock_descriptor.set_property(scope, c"set", descriptor.get_property(scope, c"set")?)?;
    } else if setter {
        mock_descriptor.set_property(scope, c"get", descriptor.get_property(scope, c"get")?)?;
        mock_descriptor.set_property(scope, c"set", mock)?;
    } else {
        mock_descriptor.set_property(
            scope,
            c"writable",
            descriptor.get_property(scope, c"writable")?,
        )?;
        mock_descriptor.set_property(scope, c"value", mock)?;
    }
    call_method(
        scope,
        global(scope, c"Object")?,
        c"defineProperty",
        &[
            object,
            method_name,
            mock_descriptor.to_jsval_throwing(scope)?,
        ],
    )?;
    Ok(mock)
}

/// `mock.getter()` and `mock.setter()`: `mock.method()` with the option `accessor` set to `true`.
fn mock_accessor<'s>(
    scope: &'s Scope<'_>,
    tracker: Object<'_>,
    args: [HandleValue<'_>; 4],
    accessor: &'static std::ffi::CStr,
) -> Result<Object<'s>, Thrown> {
    let [object, method_name, mut implementation, mut options] = args;
    let label = format!("options.{}", accessor.to_str().unwrap_or_default());
    if implementation.is_object() && !is_function(scope, implementation) {
        options = implementation;
        implementation = HandleValue::undefined();
    } else if !options.is_undefined() {
        validate_plain_object(scope, options, "options", false)?;
    }
    let copy = Object::new_plain(scope)?;
    if let Some(options) = as_object(scope, options) {
        let assign = global(scope, c"Object")?;
        call_method(
            scope,
            assign,
            c"assign",
            &[
                copy.to_jsval_throwing(scope)?,
                options.to_jsval_throwing(scope)?,
            ],
        )?;
    }
    let value = copy.get_property(scope, accessor)?;
    if value.is_boolean() && !value.to_boolean() {
        return Err(NodeError::invalid_arg_value(scope, &label, value, "cannot be false").into());
    }
    copy.set_property_by_name(
        scope,
        accessor,
        if value.is_undefined() {
            scope.root_value(js::value::from_bool(true))
        } else {
            value
        },
    )?;
    let copy = copy.to_jsval_throwing(scope)?;
    mock_method(
        scope,
        tracker,
        object,
        method_name,
        Some(implementation),
        Some(copy),
    )
}

#[core_runtime::jsfunction_class(name = "MockTracker", class_syntax)]
pub mod mock_tracker {
    use super::*;

    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this, true)?;
        state.set_property(scope, c"mocks", *js::Array::new(scope, 0)?)
    }

    /// <https://nodejs.org/api/test.html#mockfnoriginal-implementation-options>
    #[method(name = "fn")]
    pub fn mock_fn<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        original: Option<HandleValue<'_>>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let mut original = original.filter(|value| !value.is_undefined());
        let mut implementation = implementation.filter(|value| !value.is_undefined());
        let mut options = options.filter(|value| !value.is_undefined());
        if let Some(value) =
            original.filter(|value| value.is_object() && !is_function(scope, *value))
        {
            options = Some(value);
            original = None;
            implementation = None;
        } else if let Some(value) =
            implementation.filter(|value| value.is_object() && !is_function(scope, *value))
        {
            options = Some(value);
            implementation = None;
        }
        let original = match original {
            Some(original) => original,
            None => empty_function(scope)?,
        };
        let implementation = implementation.unwrap_or(original);
        validate_function(scope, original, "original")?;
        validate_function(scope, implementation, "implementation")?;
        let options = match options {
            Some(options) => validate_plain_object(scope, options, "options", false)?,
            None => Object::new_plain(scope)?,
        };
        let times = times_option(scope, options)?;
        validate_times(scope, times, "options.times")?;
        let restore_info = Object::new_plain(scope)?;
        restore_info.set_property(scope, c"original", original)?;
        let ctx = new_context(scope, implementation, restore_info, times)?;
        Ok(setup_mock(scope, this, ctx, original)?)
    }

    /// <https://nodejs.org/api/test.html#mockmethodobject-methodname-implementation-options>
    pub fn method<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        object: HandleValue<'_>,
        method_name: HandleValue<'_>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        mock_method(scope, this, object, method_name, implementation, options)
    }

    /// <https://nodejs.org/api/test.html#mockgetterobject-methodname-implementation-options>
    pub fn getter<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        object: HandleValue<'_>,
        method_name: HandleValue<'_>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        let args = [
            object,
            method_name,
            implementation.unwrap_or(undefined),
            options.unwrap_or(undefined),
        ];
        mock_accessor(scope, this, args, c"getter")
    }

    /// <https://nodejs.org/api/test.html#mocksetterobject-methodname-implementation-options>
    pub fn setter<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        object: HandleValue<'_>,
        method_name: HandleValue<'_>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        let args = [
            object,
            method_name,
            implementation.unwrap_or(undefined),
            options.unwrap_or(undefined),
        ];
        mock_accessor(scope, this, args, c"setter")
    }

    /// <https://nodejs.org/api/test.html#mockreset>
    pub fn reset(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        reset_tracker(scope, this)
    }

    /// <https://nodejs.org/api/test.html#mockrestoreall>
    pub fn restore_all(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::restore_all(scope, this)
    }

    /// Define `timers`, which throws `ERR_METHOD_NOT_IMPLEMENTED`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"get timers", 0, timers_not_implemented, ())?;
        proto.define_accessor(scope, c"timers", Some(*getter), None, 0)
    }
}

/// The `timers` getter of `MockTracker.prototype`.
fn timers_not_implemented(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Err(NodeError::not_implemented("mock.timers").throw(scope))
}

#[core_runtime::jsfunction_class(name = "MockFunctionContext", class_syntax)]
pub mod mock_function_context {
    use super::*;

    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this, true)?;
        state.set_property(scope, c"calls", *js::Array::new(scope, 0)?)?;
        state.set_property(scope, c"once", *js::Map::new(scope)?)
    }

    /// <https://nodejs.org/api/test.html#ctxcallcount>
    pub fn call_count(scope: &Scope<'_>, this: Object<'_>) -> Result<f64, ExnThrown> {
        let calls = objects_array(scope, state_of(scope, this, false)?, c"calls")?;
        Ok(f64::from(length(scope, calls)?))
    }

    /// <https://nodejs.org/api/test.html#ctxmockimplementationimplementation>
    pub fn mock_implementation(
        scope: &Scope<'_>,
        this: Object<'_>,
        implementation: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        validate_function(scope, implementation, "implementation")?;
        state_of(scope, this, false)?.set_property(scope, c"implementation", implementation)?;
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#ctxmockimplementationonceimplementation-oncall>
    pub fn mock_implementation_once(
        scope: &Scope<'_>,
        this: Object<'_>,
        implementation: HandleValue<'_>,
        on_call: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        validate_function(scope, implementation, "implementation")?;
        let state = state_of(scope, this, false)?;
        let next = f64::from(length(scope, objects_array(scope, state, c"calls")?)?);
        let call = match on_call.filter(|value| !value.is_undefined() && !value.is_null()) {
            Some(value) => {
                crate::errors::validate_integer(scope, value, "onCall", next, 9007199254740991.0)?
            }
            None => next,
        };
        let once = state
            .obj(scope, c"once")?
            .and_then(|once| once.cast::<js::Map>().ok())
            .ok_or(ExnThrown)?;
        once.insert(
            scope,
            scope.root_value(js::value::from_f64(call)),
            implementation,
        )?;
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#ctxrestore>
    pub fn restore(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::restore(scope, this)
    }

    /// <https://nodejs.org/api/test.html#ctxresetcalls>
    pub fn reset_calls(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        state_of(scope, this, false)?.set_property(scope, c"calls", *js::Array::new(scope, 0)?)
    }

    /// Define `calls`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"get calls", 0, calls_getter, ())?;
        proto.define_accessor(scope, c"calls", Some(*getter), None, 0)
    }
}

/// `ctx.calls`: a copy of the recorded calls.
fn calls_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let calls = objects_array(scope, state_of(scope, this, false)?, c"calls")?;
    Ok(call_method(scope, calls, c"slice", &[] as &[HandleValue])?.get())
}
