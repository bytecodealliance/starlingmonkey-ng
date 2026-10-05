// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The base `Stream` class and its legacy `pipe`: Node's `lib/internal/streams/legacy.js`.

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{cast_payload, Callback, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::readable::prepend_listener;
use crate::errors::Thrown;
use crate::events::{listen, listener_count};
use crate::support::has_method;

/// The state of a legacy `pipe`: the source, the destination, the listeners it adds, and whether
/// it ended or destroyed the destination.
#[core_runtime::jsclass(hidden)]
pub(crate) struct LegacyPipeState {
    source: Heap<js::object::Object>,
    dest: Heap<js::object::Object>,
    ondata: Heap<js::object::Object>,
    ondrain: Heap<js::object::Object>,
    onend: Heap<js::object::Object>,
    onclose: Heap<js::object::Object>,
    onerror: Heap<js::object::Object>,
    cleanup: Heap<js::object::Object>,
    #[no_trace]
    did_on_end: bool,
}

#[core_runtime::jsmethods]
impl LegacyPipeState {
    fn new(
        &self,
        scope: &Scope<'_>,
        source: Object<'_>,
        dest: Object<'_>,
    ) -> Result<(), ExnThrown> {
        let listener = |body: Callback| Function::new_callback(scope, c"", 1, body, self);
        let data = self.data();
        data.source.set(source);
        data.dest.set(dest);
        data.ondata.set(*listener(ondata)?);
        data.ondrain.set(*listener(ondrain)?);
        data.onend.set(*listener(onend)?);
        data.onclose.set(*listener(onclose)?);
        data.onerror.set(*listener(onerror)?);
        data.cleanup.set(*listener(cleanup)?);
        Ok(())
    }
}

fn ondata(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let pipe = cast_payload::<LegacyPipeState>(scope, payload);
    let (source, dest) = {
        let data = pipe.data();
        (data.source.get(scope), data.dest.get(scope))
    };
    if js::conversion::to_boolean(dest.get_property(scope, c"writable")?) {
        let written = Function::call_by_name(scope, dest.handle(), c"write", &[args.get(0)])?;
        if written.is_boolean() && !written.to_boolean() && has_method(scope, source, c"pause")? {
            Function::call_by_name(scope, source.handle(), c"pause", js::function::EmptyArgs)?;
        }
    }
    Ok(js::value::undefined())
}

fn ondrain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let source = cast_payload::<LegacyPipeState>(scope, payload)
        .data()
        .source
        .get(scope);
    if js::conversion::to_boolean(source.get_property(scope, c"readable")?)
        && has_method(scope, source, c"resume")?
    {
        Function::call_by_name(scope, source.handle(), c"resume", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

/// Mark the pipe ended, and return its destination unless it was already.
fn end_once<'s>(scope: &'s Scope<'_>, pipe: LegacyPipeState<'_>) -> Option<Object<'s>> {
    let mut data = pipe.data_mut();
    if data.did_on_end {
        return None;
    }
    data.did_on_end = true;
    Some(data.dest.get(scope))
}

fn onend(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let pipe = cast_payload::<LegacyPipeState>(scope, payload);
    if let Some(dest) = end_once(scope, pipe) {
        Function::call_by_name(scope, dest.handle(), c"end", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

fn onclose(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let pipe = cast_payload::<LegacyPipeState>(scope, payload);
    if let Some(dest) = end_once(scope, pipe) {
        if has_method(scope, dest, c"destroy")? {
            Function::call_by_name(scope, dest.handle(), c"destroy", js::function::EmptyArgs)?;
        }
    }
    Ok(js::value::undefined())
}

fn onerror(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).ok();
    let error = scope.root_value(args.get(0).get());
    cleanup_pipe(scope, cast_payload::<LegacyPipeState>(scope, payload))?;
    if let Some(this) = this {
        let event = js::class::get_or_init_atom(scope, c"error")?;
        if listener_count(scope, this, event)? == 0.0 {
            crate::events::emit(scope, this, c"error", &[error])?;
        }
    }
    Ok(js::value::undefined())
}

fn cleanup(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cleanup_pipe(scope, cast_payload::<LegacyPipeState>(scope, payload))?;
    Ok(js::value::undefined())
}

/// Remove the listeners the pipe added.
fn cleanup_pipe(scope: &Scope<'_>, pipe: LegacyPipeState<'_>) -> Result<(), ExnThrown> {
    let (source, dest, ondata, ondrain, onend, onclose, onerror, cleanup) = {
        let data = pipe.data();
        (
            data.source.get(scope),
            data.dest.get(scope),
            data.ondata.get(scope),
            data.ondrain.get(scope),
            data.onend.get(scope),
            data.onclose.get(scope),
            data.onerror.get(scope),
            data.cleanup.get(scope),
        )
    };
    for (emitter, event, listener) in [
        (source, c"data", ondata),
        (dest, c"drain", ondrain),
        (source, c"end", onend),
        (source, c"close", onclose),
        (source, c"error", onerror),
        (dest, c"error", onerror),
        (source, c"end", cleanup),
        (source, c"close", cleanup),
        (dest, c"close", cleanup),
    ] {
        listen(scope, emitter, c"removeListener", event, listener)?;
    }
    Ok(())
}

#[core_runtime::jsfunction_class(name = "Stream", extends = "crate::events::event_emitter")]
pub mod stream {
    use super::*;

    /// <https://nodejs.org/api/stream.html#stream>: an `EventEmitter`.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, options)
    }

    /// The legacy `pipe`: write `data` to `dest`, pausing on backpressure, and end or destroy
    /// `dest` once this ends or closes.
    #[method(length = 2)]
    pub fn pipe<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        dest: HandleValue<'s>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let dest_object = Object::from_value(scope, dest).map_err(|_| {
            js::error::TypeError(String::from("dest.on is not a function")).throw(scope)
        })?;
        let pipe = LegacyPipeState::new(scope, this, dest_object)?;
        let (ondata, ondrain, onend, onclose, onerror, cleanup) = {
            let data = pipe.data();
            (
                data.ondata.get(scope),
                data.ondrain.get(scope),
                data.onend.get(scope),
                data.onclose.get(scope),
                data.onerror.get(scope),
                data.cleanup.get(scope),
            )
        };
        listen(scope, this, c"on", c"data", ondata)?;
        listen(scope, dest_object, c"on", c"drain", ondrain)?;
        let end_option = options
            .and_then(|options| Object::from_value(scope, options).ok())
            .map(|options| options.get_property(scope, c"end"))
            .transpose()?;
        let keep_open = end_option.is_some_and(|end| end.is_boolean() && !end.to_boolean());
        if !js::conversion::to_boolean(dest_object.get_property(scope, c"_isStdio")?) && !keep_open
        {
            listen(scope, this, c"on", c"end", onend)?;
            listen(scope, this, c"on", c"close", onclose)?;
        }
        prepend_listener(scope, this, c"error", onerror)?;
        prepend_listener(scope, dest_object, c"error", onerror)?;
        listen(scope, this, c"on", c"end", cleanup)?;
        listen(scope, this, c"on", c"close", cleanup)?;
        listen(scope, dest_object, c"on", c"close", cleanup)?;
        crate::events::emit(
            scope,
            dest_object,
            c"pipe",
            &[this.to_jsval_throwing(scope)?],
        )?;
        Ok(dest)
    }
}
