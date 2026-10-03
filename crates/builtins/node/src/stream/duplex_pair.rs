// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `stream.duplexPair()`: Node's `lib/internal/streams/duplexpair.js`. Each side is a `Duplex`
//! whose writes are pushed to the other side's readable half. A side holds the callback of its
//! last write in `_starlingCallback` until the other side reads, which provides backpressure.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::util::{
    call, call_method, closure, is_function, next_tick, string, this_object, truthy, Captures,
    Props,
};

/// The two connected sides, created with `options`.
pub(crate) fn duplex_pair<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'s>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let ctor = super::duplex::duplex::class_constructor(scope)?;
    let args: Vec<HandleValue<'_>> = options.into_iter().collect();
    let side0 = Function::construct(scope, ctor, &args)?;
    let side1 = Function::construct(scope, ctor, &args)?;
    connect(scope, side0, side1)?;
    connect(scope, side1, side0)?;
    js::Array::with_contents(
        scope,
        &[
            side0.to_jsval_throwing(scope)?,
            side1.to_jsval_throwing(scope)?,
        ],
    )?
    .to_jsval_throwing(scope)
}

/// Define `side`'s `_read`, `_write`, `_final` and `_destroy` to read from and write to `other`.
fn connect(scope: &Scope<'_>, side: Object<'_>, other: Object<'_>) -> Result<(), ExnThrown> {
    side.define_property(scope, c"_starlingCallback", js::value::null(), 0)?;
    let other = other.to_jsval_throwing(scope)?;
    for (name, nargs, body) in [
        (c"_read", 1, read as js::function::Callback),
        (c"_write", 3, write),
        (c"_final", 1, final_),
        (c"_destroy", 2, destroy),
    ] {
        let method = closure(scope, nargs, body, &[other])?;
        side.define_property(scope, name, method, 0)?;
    }
    Ok(())
}

/// `_read`: call the other side's pending write callback.
fn read(scope: &Scope<'_>, args: CallbackArgs<'_>, _: HandleValue) -> Result<Value, ExnThrown> {
    let this = this_object(scope, &args)?;
    let callback = this.val(scope, c"_starlingCallback")?;
    if is_function(scope, callback) {
        this.set_property(scope, c"_starlingCallback", js::value::null())?;
        call(scope, HandleValue::undefined(), callback, &[])?;
    }
    Ok(js::value::undefined())
}

/// `_write(chunk, encoding, callback)`, whose captures are `[other]`: push `chunk` to the other
/// side and hold `callback` until it reads. An empty chunk's callback is called in a tick.
fn write(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let other = Captures::of(scope, payload)?.obj(0)?;
    let chunk = args.get(0);
    let callback = args.get(2);
    let empty = if chunk.is_string() {
        js::JSString::from_value(scope, *chunk)?.is_empty()
    } else {
        Object::from_value(scope, *chunk).ok().is_some_and(|chunk| {
            chunk
                .num(scope, c"length")
                .is_ok_and(|length| length == 0.0)
        })
    };
    if empty {
        js::jobs::queue_tick(scope, callback, &[])?;
    } else {
        call_method(scope, other, c"push", &[chunk])?;
        other.set_property(scope, c"_starlingCallback", callback)?;
    }
    Ok(js::value::undefined())
}

/// `_final(callback)`, whose captures are `[other]`: end the other side's readable half, and call
/// `callback` on its `end`.
fn final_(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let other = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, other, c"on", &[string(scope, "end")?, args.get(0)])?;
    let null = scope.root_value(js::value::null());
    call_method(scope, other, c"push", &[null])?;
    Ok(js::value::undefined())
}

/// `_destroy(err, callback)`, whose captures are `[other]`: in a tick, destroy the other side
/// without the error if there is one, or end its readable half otherwise. Then call `callback`.
fn destroy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let other = Captures::of(scope, payload)?.obj(0)?;
    let error = args.get(0);
    if !other.flag(scope, c"destroyed")? {
        next_tick(
            scope,
            close_other,
            &[other.to_jsval_raw_throwing(scope)?, error.get()],
        )?;
    }
    call(scope, HandleValue::undefined(), args.get(1), &[error])?;
    Ok(js::value::undefined())
}

/// The tick `destroy` queues, whose captures are `[other, error]`.
fn close_other(
    scope: &Scope<'_>,
    _: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let other = captures.obj(0)?;
    if other.flag(scope, c"destroyed")? {
        return Ok(js::value::undefined());
    }
    if truthy(scope, captures.get(1)?) {
        call_method(scope, other, c"destroy", &[])?;
    } else {
        let null = scope.root_value(js::value::null());
        call_method(scope, other, c"push", &[null])?;
    }
    Ok(js::value::undefined())
}
