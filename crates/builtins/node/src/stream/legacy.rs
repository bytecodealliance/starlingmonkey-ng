// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The base `Stream` class and its legacy `pipe`: Node's `lib/internal/streams/legacy.js`.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use super::readable::prepend_listener;
use super::util::{
    as_object, call_method, closure, emit, has_method, listen, listener_count, string, truthy,
    type_error, Captures,
};
use crate::errors::Thrown;

/// The captures of the listeners legacy `pipe` adds: `[source, dest, ondata, ondrain, onend,
/// onclose, onerror, cleanup, didOnEnd]`.
mod captures {
    pub(super) const SOURCE: u32 = 0;
    pub(super) const DEST: u32 = 1;
    pub(super) const ONDATA: u32 = 2;
    pub(super) const ONDRAIN: u32 = 3;
    pub(super) const ONEND: u32 = 4;
    pub(super) const ONCLOSE: u32 = 5;
    pub(super) const ONERROR: u32 = 6;
    pub(super) const CLEANUP: u32 = 7;
    pub(super) const DID_ON_END: u32 = 8;
    pub(super) const COUNT: usize = 9;
}

/// The legacy `pipe` array of a listener's captures `[pipe]`.
fn pipe_array<'s>(
    scope: &'s Scope<'_>,
    payload: HandleValue<'_>,
) -> Result<Captures<'s>, ExnThrown> {
    Captures::of(scope, Captures::of(scope, payload)?.get(0)?)
}

fn ondata(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let pipe = pipe_array(scope, payload)?;
    let source = pipe.obj(captures::SOURCE)?;
    let dest = pipe.obj(captures::DEST)?;
    if truthy(scope, dest.get_property(scope, c"writable")?) {
        let written = call_method(scope, dest, c"write", &[args.get(0)])?;
        if written.is_boolean() && !written.to_boolean() && has_method(scope, source, c"pause")? {
            call_method(scope, source, c"pause", &[])?;
        }
    }
    Ok(js::value::undefined())
}

fn ondrain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let source = pipe_array(scope, payload)?.obj(captures::SOURCE)?;
    if truthy(scope, source.get_property(scope, c"readable")?)
        && has_method(scope, source, c"resume")?
    {
        call_method(scope, source, c"resume", &[])?;
    }
    Ok(js::value::undefined())
}

fn onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let pipe = pipe_array(scope, payload)?;
    if !pipe.flag(captures::DID_ON_END)? {
        pipe.set(captures::DID_ON_END, true)?;
        call_method(scope, pipe.obj(captures::DEST)?, c"end", &[])?;
    }
    Ok(js::value::undefined())
}

fn onclose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let pipe = pipe_array(scope, payload)?;
    if !pipe.flag(captures::DID_ON_END)? {
        pipe.set(captures::DID_ON_END, true)?;
        let dest = pipe.obj(captures::DEST)?;
        if has_method(scope, dest, c"destroy")? {
            call_method(scope, dest, c"destroy", &[])?;
        }
    }
    Ok(js::value::undefined())
}

fn onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = as_object(scope, scope.root_value(args.this()));
    let error = scope.root_value(args.get(0).get());
    cleanup(scope, args, payload)?;
    if let Some(this) = this {
        let event = string(scope, "error")?;
        if listener_count(scope, this, event)? == 0.0 {
            emit(scope, this, "error", &[error])?;
        }
    }
    Ok(js::value::undefined())
}

fn cleanup(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    use captures::*;
    let pipe = pipe_array(scope, payload)?;
    let source = pipe.obj(SOURCE)?;
    let dest = pipe.obj(DEST)?;
    for (emitter, event, index) in [
        (source, "data", ONDATA),
        (dest, "drain", ONDRAIN),
        (source, "end", ONEND),
        (source, "close", ONCLOSE),
        (source, "error", ONERROR),
        (dest, "error", ONERROR),
        (source, "end", CLEANUP),
        (source, "close", CLEANUP),
        (dest, "close", CLEANUP),
    ] {
        listen(scope, emitter, c"removeListener", event, pipe.obj(index)?)?;
    }
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(name = "Stream", extends = "crate::events::event_emitter")]
pub mod stream {
    use super::*;

    /// <https://nodejs.org/api/stream.html#stream>: an `EventEmitter`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, options)
    }

    /// The legacy `pipe`: write `data` to `dest`, pausing on backpressure, and end or destroy
    /// `dest` once this ends or closes.
    pub fn pipe<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        dest: HandleValue<'s>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        use captures::*;
        let pipe = js::Array::with_contents(scope, &[js::value::undefined(); COUNT])?;
        let pipe_value = scope.root_value((*pipe).to_jsval_raw_throwing(scope)?);
        let state = Captures::of(scope, (*pipe).to_jsval_throwing(scope)?)?;
        state.set(SOURCE, this)?;
        state.set(DEST, dest)?;
        state.set(DID_ON_END, false)?;
        for (index, body) in [
            (ONDATA, ondata as Callback),
            (ONDRAIN, ondrain),
            (ONEND, onend),
            (ONCLOSE, onclose),
            (ONERROR, onerror),
            (CLEANUP, cleanup),
        ] {
            state.set(index, closure(scope, 1, body, &[pipe_value.get()])?)?;
        }
        listen(scope, this, c"on", "data", state.obj(ONDATA)?)?;
        let dest_object =
            as_object(scope, dest).ok_or_else(|| type_error(scope, "dest.on is not a function"))?;
        listen(scope, dest_object, c"on", "drain", state.obj(ONDRAIN)?)?;
        let end_option = options
            .and_then(|options| as_object(scope, options))
            .map(|options| options.get_property(scope, c"end"))
            .transpose()?;
        let keep_open = end_option.is_some_and(|end| end.is_boolean() && !end.to_boolean());
        if !truthy(scope, dest_object.get_property(scope, c"_isStdio")?) && !keep_open {
            listen(scope, this, c"on", "end", state.obj(ONEND)?)?;
            listen(scope, this, c"on", "close", state.obj(ONCLOSE)?)?;
        }
        prepend_listener(scope, this, "error", state.obj(ONERROR)?)?;
        prepend_listener(scope, dest_object, "error", state.obj(ONERROR)?)?;
        listen(scope, this, c"on", "end", state.obj(CLEANUP)?)?;
        listen(scope, this, c"on", "close", state.obj(CLEANUP)?)?;
        listen(scope, dest_object, c"on", "close", state.obj(CLEANUP)?)?;
        emit(
            scope,
            dest_object,
            "pipe",
            &[this.to_jsval_throwing(scope)?],
        )?;
        Ok(dest)
    }
}
