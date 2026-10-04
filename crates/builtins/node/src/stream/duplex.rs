// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Duplex`, `Transform` and `PassThrough`: Node's `lib/internal/streams/duplex.js`,
//! `transform.js` and `passthrough.js`.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use super::readable::{accessor, readable_state};
use super::util::{
    as_object, call, call_method, closure, is_function, next_tick_call, required_obj, this_object,
    truthy, Captures, Props,
};
use super::writable::writable_state;
use crate::errors::{NodeError, Thrown};

#[core_runtime::jsfunction_class(
    name = "Duplex",
    extends = "super::super::readable::readable",
    self_construct
)]
pub mod duplex {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamduplexoptions>
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_duplex(scope, this, options)
    }

    /// <https://nodejs.org/api/stream.html#streamduplexfromsrc>: a stub, which throws
    /// `ERR_METHOD_NOT_IMPLEMENTED` instead of inheriting `Readable.from`.
    #[static_method]
    pub fn from() -> Result<(), NodeError> {
        Err(NodeError::not_implemented("Duplex.from()"))
    }

    /// <https://nodejs.org/api/stream.html#streamduplexfromwebpair-options>
    #[static_method]
    pub fn from_web<'s>(
        scope: &'s Scope<'_>,
        pair: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::duplex_from_web(scope, pair, options)
    }

    /// <https://nodejs.org/api/stream.html#streamduplextowebstreamduplex-options>
    #[static_method]
    pub fn to_web<'s>(
        scope: &'s Scope<'_>,
        duplex: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::duplex_to_web(scope, duplex, options)
    }

    /// Copy the methods and accessors of `Writable.prototype` that `Duplex.prototype` doesn't
    /// have, and define `destroyed` over both sides.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        super::setup_duplex(scope, proto)
    }
}

/// The `Duplex` constructor's body.
pub(crate) fn init_duplex(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    super::readable::init_readable(scope, stream, options, true)?;
    super::writable::init_writable(scope, stream, options, true)?;
    match options.and_then(|options| as_object(scope, options)) {
        Some(options) => {
            let allow = options.get_property(scope, c"allowHalfOpen")?;
            stream.set_flag(
                scope,
                c"allowHalfOpen",
                !(allow.is_boolean() && !allow.to_boolean()),
            )?;
            let readable = options.get_property(scope, c"readable")?;
            if readable.is_boolean() && !readable.to_boolean() {
                let state = readable_state(scope, stream)?;
                state.set_flag(scope, c"readable", false)?;
                state.set_flag(scope, c"ended", true)?;
                state.set_flag(scope, c"endEmitted", true)?;
            }
            let writable = options.get_property(scope, c"writable")?;
            if writable.is_boolean() && !writable.to_boolean() {
                let state = writable_state(scope, stream)?;
                state.set_flag(scope, c"writable", false)?;
                state.set_flag(scope, c"ending", true)?;
                state.set_flag(scope, c"ended", true)?;
                state.set_flag(scope, c"finished", true)?;
            }
            let signal = options.val(scope, c"signal")?;
            if truthy(scope, signal) {
                super::add_abort_signal(scope, signal, stream.to_jsval_throwing(scope)?)?;
            }
        }
        None => stream.set_flag(scope, c"allowHalfOpen", true)?,
    }
    super::legacy::stream::init(scope, stream, options)?;
    super::readable::finish_construction(scope, stream)
}

/// `duplex.destroyed`: whether both sides are destroyed.
fn get_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(this) = as_object(scope, scope.root_value(args.this())) else {
        return Ok(js::value::from_bool(false));
    };
    let (readable, writable) = super::util::states(scope, this)?;
    match (readable, writable) {
        (Some(r), Some(w)) => Ok(js::value::from_bool(
            r.flag(scope, c"destroyed")? && w.flag(scope, c"destroyed")?,
        )),
        _ => Ok(js::value::from_bool(false)),
    }
}

/// `duplex.destroyed = value`: set both sides.
fn set_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(this) = as_object(scope, scope.root_value(args.this())) {
        let (readable, writable) = super::util::states(scope, this)?;
        if let (Some(r), Some(w)) = (readable, writable) {
            r.set_property(scope, c"destroyed", args.get(0))?;
            w.set_property(scope, c"destroyed", args.get(0))?;
        }
    }
    Ok(js::value::undefined())
}

/// The `Duplex` class's [`duplex::setup`].
fn setup_duplex(scope: &Scope<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
    let writable = super::writable::writable::class_constructor(scope)?;
    let writable_proto = required_obj(scope, writable, c"prototype")?;
    let accessors = super::writable::accessors();
    for key in writable_proto.own_keys(scope)? {
        let Some(name) = super::util::rust_string(scope, key) else {
            continue;
        };
        if name == "constructor"
            || accessors
                .iter()
                .any(|(accessor, ..)| accessor.to_bytes() == name.as_bytes())
        {
            continue;
        }
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        if truthy(scope, proto.get_property_by_id(scope, id)?) {
            continue;
        }
        let value = writable_proto.get_property_by_id(scope, id)?;
        proto.set_property_by_id(scope, id, value)?;
    }
    for (name, getter, setter) in accessors {
        if [
            c"writable",
            c"writableHighWaterMark",
            c"writableObjectMode",
            c"writableBuffer",
            c"writableLength",
            c"writableFinished",
            c"writableCorked",
            c"writableEnded",
            c"writableNeedDrain",
        ]
        .contains(&name)
        {
            accessor(scope, proto, name, getter, setter)?;
        }
    }
    proto.set_property(scope, c"destroy", writable_proto.val(scope, c"destroy")?)?;
    accessor(
        scope,
        proto,
        c"destroyed",
        get_destroyed,
        Some(set_destroyed),
    )
}

/// The key of a `Transform`'s pending write callback: the registered symbol
/// `nodejs.stream.kCallback`.
fn transform_callback_key<'s>(scope: &'s Scope<'_>) -> Result<js::native::HandleId<'s>, ExnThrown> {
    let symbol = crate::symbol(scope, "nodejs.stream.kCallback", true)?;
    Ok(scope.root_id(js::id::value_to_id(scope, symbol)?))
}

/// The callback `_transform` gets inside `_write`, whose captures are `[stream, callback,
/// length]`: push the transformed value, and call the write callback once there's room.
fn transform_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    let callback = captures.get(1)?;
    let length = captures.get(2)?.to_number();
    let error = args.get(0);
    if truthy(scope, error) {
        call(scope, HandleValue::undefined(), callback, &[error])?;
        return Ok(js::value::undefined());
    }
    let value = args.get(1);
    if !value.is_null() && !value.is_undefined() {
        call_method(scope, stream, c"push", &[value])?;
    }
    let r = readable_state(scope, stream)?;
    let w = writable_state(scope, stream)?;
    if r.flag(scope, c"ended")? {
        next_tick_call(scope, callback, &[])?;
    } else if w.flag(scope, c"ended")?
        || length == r.num(scope, c"length")?
        || r.num(scope, c"length")? < r.num(scope, c"highWaterMark")?
    {
        call(scope, HandleValue::undefined(), callback, &[])?;
    } else {
        stream.set_property_by_id(scope, transform_callback_key(scope)?, callback)?;
    }
    Ok(js::value::undefined())
}

/// The callback `_flush` gets, whose captures are `[stream, callback]`: push the flushed value and
/// end the readable side.
fn flush_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    let callback = captures.get(1)?;
    let error = args.get(0);
    if truthy(scope, error) {
        if is_function(scope, callback) {
            call(scope, HandleValue::undefined(), callback, &[error])?;
        } else {
            call_method(scope, stream, c"destroy", &[error])?;
        }
        return Ok(js::value::undefined());
    }
    let data = args.get(1);
    if !data.is_null() && !data.is_undefined() {
        call_method(scope, stream, c"push", &[data])?;
    }
    call_method(scope, stream, c"push", &[HandleValue::null()])?;
    if is_function(scope, callback) {
        call(scope, HandleValue::undefined(), callback, &[])?;
    }
    Ok(js::value::undefined())
}

/// Node's `final` for `Transform`: flush, then end the readable side.
fn transform_final(
    scope: &Scope<'_>,
    stream: Object<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let flush = stream.get_property(scope, c"_flush")?;
    if is_function(scope, flush) && !truthy(scope, stream.get_property(scope, c"destroyed")?) {
        let done = closure(
            scope,
            2,
            flush_callback,
            &[stream.to_jsval_raw_throwing(scope)?, callback.get()],
        )?;
        call_method(scope, stream, c"_flush", &[done.to_jsval_throwing(scope)?])?;
    } else {
        call_method(scope, stream, c"push", &[HandleValue::null()])?;
        if is_function(scope, callback) {
            call(scope, HandleValue::undefined(), callback, &[])?;
        }
    }
    Ok(())
}

/// The `prefinish` listener of a `Transform`: run its `final` if `_final` was replaced.
fn transform_prefinish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = this_object(scope, &args)?;
    let ours = required_obj(scope, transform::class_constructor(scope)?, c"prototype")?
        .val(scope, c"_final")?;
    if !super::util::same(scope, this.val(scope, c"_final")?, ours)? {
        transform_final(scope, this, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(name = "Transform", extends = "super::duplex", self_construct)]
pub mod transform {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamtransformoptions>
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_transform(scope, this, options)
    }

    #[method(name = "_final")]
    pub fn final_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let callback = callback.unwrap_or(HandleValue::undefined());
        super::transform_final(scope, this, callback)
    }

    #[method(name = "_transform")]
    pub fn transform_impl() -> Result<(), NodeError> {
        Err(NodeError::not_implemented("_transform()"))
    }

    #[method(name = "_write")]
    pub fn write_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let length = readable_state(scope, this)?.num(scope, c"length")?;
        let done = closure(
            scope,
            2,
            super::transform_callback,
            &[
                this.to_jsval_raw_throwing(scope)?,
                callback.get(),
                js::value::from_f64(length),
            ],
        )?;
        call_method(
            scope,
            this,
            c"_transform",
            &[chunk, encoding, done.to_jsval_throwing(scope)?],
        )?;
        Ok(())
    }

    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let key = super::transform_callback_key(scope)?;
        let callback = this.get_property_by_id(scope, key)?;
        if is_function(scope, callback) {
            this.set_property_by_id(scope, key, HandleValue::null())?;
            call(scope, HandleValue::undefined(), callback, &[])?;
        }
        Ok(())
    }
}

/// The `Transform` constructor's body.
pub(crate) fn init_transform(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let mut options = options.map(|options| scope.root_value(options.get()));
    if let Some(object) = options.and_then(|options| as_object(scope, options)) {
        let mark =
            super::state::high_water_mark(scope, object, c"readableHighWaterMark", true, false)?;
        let explicit = object.val(scope, c"highWaterMark")?;
        let readable_explicit = object.val(scope, c"readableHighWaterMark")?;
        let set = |value: HandleValue<'_>| !value.is_null() && !value.is_undefined();
        if mark == 0.0 && (set(explicit) || set(readable_explicit)) {
            let copy = Object::new_plain(scope)?;
            copy.assign(scope, object)?;
            copy.set_property(scope, c"highWaterMark", js::value::null())?;
            copy.set_property(scope, c"readableHighWaterMark", 0)?;
            let writable_mark = object.val(scope, c"writableHighWaterMark")?;
            if truthy(scope, writable_mark) {
                copy.set_property(scope, c"writableHighWaterMark", writable_mark)?;
            } else {
                copy.set_property(scope, c"writableHighWaterMark", 0)?;
            }
            options = Some(copy.to_jsval_throwing(scope)?);
        }
    }
    init_duplex(scope, stream, options)?;
    readable_state(scope, stream)?.set_flag(scope, c"sync", false)?;
    stream.set_property_by_id(scope, transform_callback_key(scope)?, HandleValue::null())?;
    if let Some(object) = options.and_then(|options| as_object(scope, options)) {
        for (option, method) in [(c"transform", c"_transform"), (c"flush", c"_flush")] {
            let value = object.val(scope, option)?;
            if is_function(scope, value) {
                stream.set_property(scope, method, value)?;
            }
        }
    }
    let prefinish = js::class::get_or_init_shared_function(
        scope,
        transform_prefinish as *const () as usize,
        |scope| js::Function::new_callback(scope, c"prefinish", 0, transform_prefinish, ()),
    )?;
    super::util::listen(scope, stream, c"on", "prefinish", *prefinish)?;
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "PassThrough",
    extends = "super::transform",
    self_construct
)]
pub mod pass_through {
    use super::*;

    /// <https://nodejs.org/api/stream.html#class-streampassthrough>
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_transform(scope, this, options)
    }

    #[method(name = "_transform")]
    pub fn transform_impl(
        scope: &Scope<'_>,
        chunk: HandleValue<'_>,
        _encoding: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        call(
            scope,
            HandleValue::undefined(),
            callback,
            &[HandleValue::null(), chunk],
        )?;
        Ok(())
    }
}
