// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `Duplex`, `Transform` and `PassThrough`: Node's `lib/internal/streams/duplex.js`,
//! `transform.js` and `passthrough.js`.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use super::readable::accessor;
use super::state::{ReadableSide, WritableSide, R, W};
use crate::errors::{NodeError, Thrown};
use crate::support::this_object;

#[core_runtime::jsfunction_class(
    name = "Duplex",
    extends = "super::super::readable::readable",
    self_construct
)]
pub mod duplex {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamduplexoptions>
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_duplex(scope, this, options)
    }

    /// <https://nodejs.org/api/stream.html#streamduplexfromsrc>: a stub, which throws
    /// `ERR_METHOD_NOT_IMPLEMENTED` instead of inheriting `Readable.from`.
    #[static_method(length = 1)]
    pub fn from() -> Result<(), NodeError> {
        Err(NodeError::not_implemented("Duplex.from()"))
    }

    /// <https://nodejs.org/api/stream.html#streamduplexfromwebpair-options>
    #[static_method(length = 2)]
    pub fn from_web<'s>(
        scope: &'s Scope<'_>,
        pair: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        super::super::adapters::duplex_from_web(scope, pair, options)
    }

    /// <https://nodejs.org/api/stream.html#streamduplextowebstreamduplex-options>
    #[static_method(length = 2)]
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

/// Whether `value` is the boolean `false`.
fn is_false(value: HandleValue<'_>) -> bool {
    value.is_boolean() && !value.to_boolean()
}

/// The `Duplex` constructor's body.
pub(crate) fn init_duplex(
    scope: &Scope<'_>,
    stream: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    super::readable::init_readable(scope, stream, options, true)?;
    super::writable::init_writable(scope, stream, options, true)?;
    match options.and_then(|options| Object::from_value(scope, options).ok()) {
        Some(options) => {
            let allow = options.get_property(scope, c"allowHalfOpen")?;
            stream.set_property(scope, c"allowHalfOpen", !is_false(allow))?;
            if is_false(options.get_property(scope, c"readable")?) {
                let side = ReadableSide::of(scope, stream)?;
                side.get().readable.set(js::value::from_bool(false));
                side.set(R::ENDED | R::END_EMITTED, true);
            }
            if is_false(options.get_property(scope, c"writable")?) {
                let side = WritableSide::of(scope, stream)?;
                side.get_mut().writable = Some(false);
                side.set(W::ENDING | W::ENDED | W::FINISHED, true);
            }
            for (option, method) in [
                (c"read", c"_read"),
                (c"write", c"_write"),
                (c"writev", c"_writev"),
                (c"destroy", c"_destroy"),
                (c"final", c"_final"),
                (c"construct", c"_construct"),
            ] {
                let value = options.get_property(scope, option)?;
                if js::function::is_callable(value) {
                    stream.set_property(scope, method, value)?;
                }
            }
            let signal = options.get_property(scope, c"signal")?;
            if js::conversion::to_boolean(signal) {
                super::add_abort_signal(scope, signal, stream.to_jsval_throwing(scope)?)?;
            }
        }
        None => stream.set_property(scope, c"allowHalfOpen", true)?,
    }
    super::legacy::stream::init(scope, stream, options)?;
    super::readable::finish_construction(scope, stream)
}

/// The state of an accessor's `this`, if it has one.
fn this_state<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<Option<super::state::StreamState<'s>>, ExnThrown> {
    match Object::from_value(scope, args.this()) {
        Ok(this) => super::state::try_state(scope, this),
        Err(_) => Ok(None),
    }
}

/// `duplex.destroyed`: whether both sides are destroyed.
fn get_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(state) = this_state(scope, &args)? else {
        return Ok(js::value::from_bool(false));
    };
    let destroyed = state.data().duplex_destroyed();
    Ok(js::value::from_bool(destroyed))
}

/// `duplex.destroyed = value`: set both sides.
fn set_destroyed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = this_state(scope, &args)? {
        let destroyed = js::conversion::to_boolean(args.get(0));
        let mut data = state.data_mut();
        if data.readable.is_some() && data.writable.is_some() {
            if let Some(r) = data.readable.as_mut() {
                r.flags.set(R::DESTROYED, destroyed);
            }
            if let Some(w) = data.writable.as_mut() {
                w.flags.set(W::DESTROYED, destroyed);
            }
        }
    }
    Ok(js::value::undefined())
}

/// The `Duplex` class's [`duplex::setup`].
fn setup_duplex(scope: &Scope<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
    let writable_proto = super::writable::writable::prototype_object(scope)?;
    let accessors = super::writable::accessors();
    for key in writable_proto.own_keys(scope)? {
        let Some(name) = crate::support::rust_string(scope, key) else {
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
        if js::conversion::to_boolean(proto.get_property_by_id(scope, id)?) {
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
    proto.set_property(
        scope,
        c"destroy",
        writable_proto.get_property(scope, c"destroy")?,
    )?;
    accessor(
        scope,
        proto,
        c"destroyed",
        get_destroyed,
        Some(set_destroyed),
    )
}

/// The state of one `_write` of a `Transform`: the stream, the write callback, and the readable
/// length before the write.
#[core_runtime::jsclass(hidden)]
pub(crate) struct TransformWrite {
    stream: Heap<js::object::Object>,
    callback: Heap<Value>,
    #[no_trace]
    length: f64,
}

#[core_runtime::jsmethods]
impl TransformWrite {}

/// The callback `_transform` gets inside `_write`, whose payload is a [`TransformWrite`]: push the
/// transformed value, and call the write callback once there's room.
fn transform_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let write = cast_payload::<TransformWrite>(scope, payload);
    let (stream, callback, length) = {
        let data = write.data();
        (
            data.stream.get(scope),
            data.callback.get(scope),
            data.length,
        )
    };
    let error = args.get(0);
    if js::conversion::to_boolean(error) {
        js::Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        return Ok(js::value::undefined());
    }
    let value = args.get(1);
    if !value.is_null_or_undefined() {
        js::Function::call_by_name(scope, stream.handle(), c"push", &[value])?;
    }
    let r = ReadableSide::of(scope, stream)?;
    let w = WritableSide::of(scope, stream)?;
    if r.has(R::ENDED) {
        js::jobs::queue_tick(scope, callback, &[])?;
    } else if w.has(W::ENDED) || length == r.length() || r.length() < r.high_water_mark() {
        js::Function::call(
            scope,
            HandleValue::undefined(),
            callback,
            js::function::EmptyArgs,
        )?;
    } else {
        r.0.data_mut().transform_callback = Some(Heap::from(callback.get()));
    }
    Ok(js::value::undefined())
}

/// The state of a `Transform`'s flush: the stream and the callback of its `final`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct FlushState {
    stream: Heap<js::object::Object>,
    callback: Heap<Value>,
}

#[core_runtime::jsmethods]
impl FlushState {}

/// The callback `_flush` gets, whose payload is a [`FlushState`]: push the flushed value and end
/// the readable side.
fn flush_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let flush = cast_payload::<FlushState>(scope, payload);
    let (stream, callback) = {
        let data = flush.data();
        (data.stream.get(scope), data.callback.get(scope))
    };
    let error = args.get(0);
    if js::conversion::to_boolean(error) {
        if js::function::is_callable(callback) {
            js::Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        } else {
            js::Function::call_by_name(scope, stream.handle(), c"destroy", &[error])?;
        }
        return Ok(js::value::undefined());
    }
    let data = args.get(1);
    if !data.is_null_or_undefined() {
        js::Function::call_by_name(scope, stream.handle(), c"push", &[data])?;
    }
    js::Function::call_by_name(scope, stream.handle(), c"push", &[HandleValue::null()])?;
    if js::function::is_callable(callback) {
        js::Function::call(
            scope,
            HandleValue::undefined(),
            callback,
            js::function::EmptyArgs,
        )?;
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
    if js::function::is_callable(flush)
        && !js::conversion::to_boolean(stream.get_property(scope, c"destroyed")?)
    {
        let state = js::class::create_instance_with::<FlushStateImpl>(scope, |_| FlushStateImpl {
            stream: Heap::from(stream),
            callback: Heap::from(callback.get()),
        })?;
        let done = js::Function::new_callback(scope, c"", 2, flush_callback, state)?;
        js::Function::call_by_name(
            scope,
            stream.handle(),
            c"_flush",
            &[(*done).to_jsval_throwing(scope)?],
        )?;
    } else {
        js::Function::call_by_name(scope, stream.handle(), c"push", &[HandleValue::null()])?;
        if js::function::is_callable(callback) {
            js::Function::call(
                scope,
                HandleValue::undefined(),
                callback,
                js::function::EmptyArgs,
            )?;
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
    let ours = transform::prototype_object(scope)?.get_property(scope, c"_final")?;
    if !js::comparison::strictly_equal(scope, this.get_property(scope, c"_final")?, ours)? {
        transform_final(scope, this, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(name = "Transform", extends = "super::duplex", self_construct)]
pub mod transform {
    use super::*;

    /// <https://nodejs.org/api/stream.html#new-streamtransformoptions>
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_transform(scope, this, options)
    }

    #[method(name = "_final", length = 1)]
    pub fn final_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let callback = callback.unwrap_or(HandleValue::undefined());
        super::transform_final(scope, this, callback)
    }

    #[method(name = "_transform", length = 3)]
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
        let length = ReadableSide::of(scope, this)?.length();
        let write =
            js::class::create_instance_with::<TransformWriteImpl>(scope, |_| TransformWriteImpl {
                stream: Heap::from(this),
                callback: Heap::from(callback.get()),
                length,
            })?;
        let done = js::Function::new_callback(scope, c"", 2, super::transform_callback, write)?;
        js::Function::call_by_name(
            scope,
            this.handle(),
            c"_transform",
            &[chunk, encoding, (*done).to_jsval_throwing(scope)?],
        )?;
        Ok(())
    }

    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let Some(state) = super::super::state::try_state(scope, this)? else {
            return Ok(());
        };
        let callback = state
            .data_mut()
            .transform_callback
            .take()
            .map(|callback| callback.get(scope));
        if let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) {
            js::Function::call(
                scope,
                HandleValue::undefined(),
                callback,
                js::function::EmptyArgs,
            )?;
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
    if let Some(object) = options.and_then(|options| Object::from_value(scope, options).ok()) {
        let mark =
            super::state::high_water_mark(scope, object, c"readableHighWaterMark", true, false)?;
        let explicit = object.get_property(scope, c"highWaterMark")?;
        let readable_explicit = object.get_property(scope, c"readableHighWaterMark")?;
        if mark == 0.0
            && (!explicit.is_null_or_undefined() || !readable_explicit.is_null_or_undefined())
        {
            let copy = Object::new_plain(scope)?;
            copy.assign(scope, object)?;
            copy.set_property(scope, c"highWaterMark", js::value::null())?;
            copy.set_property(scope, c"readableHighWaterMark", 0)?;
            let writable_mark = object.get_property(scope, c"writableHighWaterMark")?;
            if js::conversion::to_boolean(writable_mark) {
                copy.set_property(scope, c"writableHighWaterMark", writable_mark)?;
            } else {
                copy.set_property(scope, c"writableHighWaterMark", 0)?;
            }
            options = Some(copy.to_jsval_throwing(scope)?);
        }
    }
    init_duplex(scope, stream, options)?;
    ReadableSide::of(scope, stream)?.set(R::SYNC, false);
    if let Some(object) = options.and_then(|options| Object::from_value(scope, options).ok()) {
        for (option, method) in [(c"transform", c"_transform"), (c"flush", c"_flush")] {
            let value = object.get_property(scope, option)?;
            if js::function::is_callable(value) {
                stream.set_property(scope, method, value)?;
            }
        }
    }
    let prefinish = js::class::get_or_init_shared_function(
        scope,
        transform_prefinish as *const () as usize,
        |scope| js::Function::new_callback(scope, c"prefinish", 0, transform_prefinish, ()),
    )?;
    crate::events::listen(scope, stream, c"on", c"prefinish", *prefinish)?;
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
    #[constructor(length = 1)]
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
        js::Function::call(
            scope,
            HandleValue::undefined(),
            callback,
            &[HandleValue::null(), chunk],
        )?;
        Ok(())
    }
}
