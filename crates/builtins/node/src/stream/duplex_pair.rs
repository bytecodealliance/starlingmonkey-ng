// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `stream.duplexPair()`: Node's `lib/internal/streams/duplexpair.js`. Each side is a `Duplex`
//! whose writes are pushed to the other side's readable half. A side holds the callback of the
//! other side's last write until it reads, which provides backpressure.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

/// The state of one side of a pair, Node's `DuplexSide`: the other side, the callback of the
/// other side's last write, and the state of the other side.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DuplexSide {
    other: Heap<js::object::Object>,
    callback: Option<Heap<Value>>,
    peer: Option<Heap<DuplexSideImpl>>,
}

#[core_runtime::jsmethods]
impl DuplexSide {}

/// The two connected sides, created with `options`.
pub(crate) fn duplex_pair<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'s>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let ctor = super::duplex::duplex::class_constructor(scope)?;
    let args: Vec<HandleValue<'_>> = options.into_iter().collect();
    let side0 = Function::construct(scope, ctor, &args)?;
    let side1 = Function::construct(scope, ctor, &args)?;
    let new_state = |other: Object<'_>| {
        js::class::create_instance_with::<DuplexSideImpl>(scope, |_| DuplexSideImpl {
            other: Heap::from(other),
            callback: None,
            peer: None,
        })
    };
    let state0 = new_state(side1)?;
    let state1 = new_state(side0)?;
    state0.data_mut().peer = Some(Heap::from(state1));
    state1.data_mut().peer = Some(Heap::from(state0));
    connect(scope, side0, state0)?;
    connect(scope, side1, state1)?;
    js::Array::with_contents(
        scope,
        &[
            side0.to_jsval_throwing(scope)?,
            side1.to_jsval_throwing(scope)?,
        ],
    )?
    .to_jsval_throwing(scope)
}

/// Define `side`'s `_read`, `_write`, `_final` and `_destroy` to read from and write to the other
/// side, with `state` as their payload.
fn connect(scope: &Scope<'_>, side: Object<'_>, state: DuplexSide<'_>) -> Result<(), ExnThrown> {
    for (name, nargs, body) in [
        (c"_read", 1, read as Callback),
        (c"_write", 3, write),
        (c"_final", 1, final_),
        (c"_destroy", 2, destroy),
    ] {
        let method = Function::new_callback(scope, name, nargs, body, state)?;
        side.define_property(scope, name, *method, 0)?;
    }
    Ok(())
}

/// `_read`: call the other side's pending write callback.
fn read(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<DuplexSide>(scope, payload);
    let callback = state
        .data_mut()
        .callback
        .take()
        .map(|callback| callback.get(scope));
    if let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) {
        Function::call(
            scope,
            HandleValue::undefined(),
            callback,
            js::function::EmptyArgs,
        )?;
    }
    Ok(js::value::undefined())
}

/// `_write(chunk, encoding, callback)`: push `chunk` to the other side and hold `callback` until
/// it reads. An empty chunk's callback is called in a tick.
fn write(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<DuplexSide>(scope, payload);
    let (other, peer) = {
        let data = state.data();
        (data.other.get(scope), data.peer.get(scope))
    };
    let chunk = args.get(0);
    let callback = args.get(2);
    let empty = if chunk.is_string() {
        js::JSString::from_value(scope, *chunk)?.is_empty()
    } else {
        match Object::from_value(scope, *chunk) {
            Ok(chunk) => {
                let length = chunk.get_property(scope, c"length")?;
                length.is_number() && length.to_number() == 0.0
            }
            Err(_) => false,
        }
    };
    if empty {
        js::jobs::queue_tick(scope, callback, &[])?;
    } else {
        Function::call_by_name(scope, other.handle(), c"push", &[chunk])?;
        if let Some(peer) = peer {
            peer.data_mut().callback = Some(Heap::from(callback.get()));
        }
    }
    Ok(js::value::undefined())
}

/// `_final(callback)`: end the other side's readable half, and call `callback` on its `end`.
fn final_(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let other = cast_payload::<DuplexSide>(scope, payload)
        .data()
        .other
        .get(scope);
    Function::call_by_name(
        scope,
        other.handle(),
        c"on",
        &[js::class::get_or_init_atom(scope, c"end")?, args.get(0)],
    )?;
    Function::call_by_name(scope, other.handle(), c"push", &[HandleValue::null()])?;
    Ok(js::value::undefined())
}

/// `_destroy(err, callback)`: in a tick, destroy the other side without the error if there is one,
/// or end its readable half otherwise. Then call `callback`.
fn destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let other = cast_payload::<DuplexSide>(scope, payload)
        .data()
        .other
        .get(scope);
    let error = args.get(0);
    if !js::conversion::to_boolean(other.get_property(scope, c"destroyed")?) {
        js::jobs::queue_native_tick(scope, close_other, other, &[error])?;
    }
    Function::call(scope, HandleValue::undefined(), args.get(1), &[error])?;
    Ok(js::value::undefined())
}

/// The tick `destroy` queues, whose payload is the other side and whose argument is the error.
fn close_other(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let other = js::function::cast_payload::<js::Object>(scope, payload);
    if js::conversion::to_boolean(other.get_property(scope, c"destroyed")?) {
        return Ok(js::value::undefined());
    }
    if js::conversion::to_boolean(args.get(0)) {
        Function::call_by_name(scope, other.handle(), c"destroy", js::function::EmptyArgs)?;
    } else {
        Function::call_by_name(scope, other.handle(), c"push", &[HandleValue::null()])?;
    }
    Ok(js::value::undefined())
}
