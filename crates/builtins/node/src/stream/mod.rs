// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:stream` and `node:stream/promises`. See <https://nodejs.org/api/stream.html>.
//!
//! The implementation follows Node.js v24's `lib/internal/streams` (MIT license, Copyright
//! Node.js contributors), function by function. A stream's state is in `_readableState` and
//! `_writableState` objects with a property per field, under the names Node's states expose, so
//! code that inspects them reads the same values. `process.nextTick` work runs in the tick queue of
//! `process.nextTick`.

mod destroy;
mod duplex;
mod duplex_pair;
mod eos;
mod from;
mod legacy;
mod pipeline;
mod readable;
mod state;
mod util;
mod writable;

pub(crate) use duplex::{duplex as duplex_class, init_duplex, init_transform, transform};
pub(crate) use eos::eos as end_of_stream;
pub(crate) use legacy::stream as stream_class;
pub(crate) use readable::readable as readable_class;
pub(crate) use util::{
    as_object, call_method, closure, error_value, has_method, is_function, next_tick_call, string,
    take_exception, truthy, Captures, Props,
};
pub(crate) use writable::writable as writable_class;

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;

use crate::errors::{validate_abort_signal, NodeError, Thrown};
use util::{is_node_stream, listen};

/// Register the stream classes.
pub(crate) fn register_classes(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    legacy::stream::register(scope)?;
    readable::readable_state::register(scope)?;
    readable::readable::register(scope)?;
    writable::writable_state::register(scope)?;
    writable::writable::register(scope)?;
    duplex::duplex::register(scope)?;
    duplex::transform::register(scope)?;
    duplex::pass_through::register(scope)?;
    Ok(())
}

/// The abort listener of [`add_abort_signal`], whose captures are `[stream, signal]`: destroy the
/// stream with an `AbortError`.
fn on_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let stream = captures.obj(0)?;
    let reason = captures.obj(1)?.val(scope, c"reason")?;
    let error = crate::errors::abort_error(scope, Some(reason))?;
    util::call_method(scope, stream, c"destroy", &[error])?;
    Ok(js::value::undefined())
}

/// The `finished` callback of [`add_abort_signal`], whose captures are `[signal, listener]`.
fn remove_abort_listener(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    listen(
        scope,
        captures.obj(0)?,
        c"removeEventListener",
        "abort",
        captures.obj(1)?,
    )?;
    Ok(js::value::undefined())
}

/// `stream.addAbortSignal(signal, stream)`: destroy `stream` with an `AbortError` once `signal`
/// aborts.
pub(crate) fn add_abort_signal<'s>(
    scope: &'s Scope<'_>,
    signal: HandleValue<'_>,
    stream: HandleValue<'s>,
) -> Result<HandleValue<'s>, Thrown> {
    validate_abort_signal(scope, signal, "signal")?;
    if !is_node_stream(scope, stream)? {
        return Err(NodeError::invalid_arg_type(
            scope,
            "stream",
            "an instance of ReadableStream, WritableStream, or Stream",
            stream,
        )
        .into());
    }
    let Some(signal_object) = as_object(scope, signal) else {
        return Ok(stream);
    };
    let stream_object = as_object(scope, stream).expect("streams are objects");
    let listener = closure(
        scope,
        0,
        on_abort,
        &[
            stream_object.to_jsval_raw_throwing(scope)?,
            signal_object.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    if truthy(scope, signal_object.get_property(scope, c"aborted")?) {
        util::call(
            scope,
            HandleValue::undefined(),
            listener.to_jsval_throwing(scope)?,
            &[],
        )?;
    } else {
        listen(scope, signal_object, c"addEventListener", "abort", listener)?;
        let remove = closure(
            scope,
            1,
            remove_abort_listener,
            &[
                signal_object.to_jsval_raw_throwing(scope)?,
                listener.to_jsval_raw_throwing(scope)?,
            ],
        )?;
        eos::eos(scope, stream, None, remove.to_jsval_throwing(scope)?)?;
    }
    Ok(stream)
}

#[core_runtime::jsmodule(node_args, name = "node:stream", default = "Stream")]
pub mod module {
    use super::*;

    pub use super::duplex::duplex as Duplex;
    pub use super::duplex::pass_through as PassThrough;
    pub use super::duplex::transform as Transform;
    pub use super::legacy::stream as Stream;
    pub use super::readable::readable as Readable;
    pub use super::writable::writable as Writable;

    /// <https://nodejs.org/api/stream.html#streampipelinesource-transforms-destination-callback>
    pub fn pipeline<'s>(
        scope: &'s Scope<'_>,
        streams: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let streams = streams
            .iter()
            .map(|stream| scope.root_value(stream.get()))
            .collect();
        super::pipeline::pipeline(scope, streams)
    }

    /// <https://nodejs.org/api/stream.html#streamfinishedstream-options-callback>: returns the
    /// cleanup function.
    pub fn finished<'s>(
        scope: &'s Scope<'_>,
        stream: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let stream = stream.unwrap_or(HandleValue::undefined());
        let options = options.unwrap_or(HandleValue::undefined());
        match callback {
            Some(callback) => super::eos::eos(scope, stream, Some(options), callback),
            None => super::eos::eos(scope, stream, None, options),
        }
    }

    /// <https://nodejs.org/api/stream.html#streamaddabortsignalsignal-stream>
    pub fn add_abort_signal<'s>(
        scope: &'s Scope<'_>,
        signal: Option<HandleValue<'_>>,
        stream: Option<HandleValue<'s>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let signal = signal.unwrap_or(HandleValue::undefined());
        let stream = stream.unwrap_or(HandleValue::undefined());
        super::add_abort_signal(scope, signal, stream)
    }

    /// <https://nodejs.org/api/stream.html#streamdestroystream-error>
    pub fn destroy(
        scope: &Scope<'_>,
        stream: Option<HandleValue<'_>>,
        error: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let stream = stream.unwrap_or(HandleValue::undefined());
        let error = error.unwrap_or(HandleValue::undefined());
        super::destroy::destroyer(scope, stream, error)
    }

    /// <https://nodejs.org/api/stream.html#streamduplexpairoptions>
    pub fn duplex_pair<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'s>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        super::duplex_pair::duplex_pair(scope, options)
    }

    /// <https://nodejs.org/api/stream.html#streamgetdefaulthighwatermarkobjectmode>
    pub fn get_default_high_water_mark(object_mode: bool) -> f64 {
        super::state::default_high_water_mark(object_mode)
    }

    /// <https://nodejs.org/api/stream.html#streamsetdefaulthighwatermarkobjectmode-value>
    pub fn set_default_high_water_mark(
        scope: &Scope<'_>,
        object_mode: bool,
        value: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let value = super::state::validate_default(scope, value)?;
        super::state::set_default_high_water_mark(object_mode, value);
        Ok(())
    }

    /// <https://nodejs.org/api/stream.html#streamisdestroyedstream>
    pub fn is_destroyed(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
    ) -> Result<Option<bool>, ExnThrown> {
        util::is_destroyed(scope, stream)
    }

    /// <https://nodejs.org/api/stream.html#streamisreadablestream>
    pub fn is_readable(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
    ) -> Result<Option<bool>, ExnThrown> {
        util::is_readable(scope, stream)
    }

    /// <https://nodejs.org/api/stream.html#streamiswritablestream>
    pub fn is_writable(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
    ) -> Result<Option<bool>, ExnThrown> {
        util::is_writable(scope, stream)
    }

    /// <https://nodejs.org/api/stream.html#streamisdisturbedstream>
    pub fn is_disturbed(scope: &Scope<'_>, stream: HandleValue<'_>) -> Result<bool, ExnThrown> {
        let Some(stream) = as_object(scope, stream) else {
            return Ok(false);
        };
        Ok(
            truthy(scope, stream.get_property(scope, c"readableDidRead")?)
                || truthy(scope, stream.get_property(scope, c"readableAborted")?),
        )
    }

    /// <https://nodejs.org/api/stream.html#streamiserroredstream>
    pub fn is_errored(scope: &Scope<'_>, stream: HandleValue<'_>) -> Result<bool, ExnThrown> {
        let Some(stream) = as_object(scope, stream) else {
            return Ok(false);
        };
        for (property, state, field) in [
            (Some(c"readableErrored"), None, None),
            (Some(c"writableErrored"), None, None),
            (None, Some(c"_readableState"), Some(c"errorEmitted")),
            (None, Some(c"_writableState"), Some(c"errorEmitted")),
            (None, Some(c"_readableState"), Some(c"errored")),
            (None, Some(c"_writableState"), Some(c"errored")),
        ] {
            let value = match (property, state, field) {
                (Some(property), _, _) => stream.val(scope, property)?,
                (None, Some(state), Some(field)) => match stream.obj(scope, state)? {
                    Some(state) => state.val(scope, field)?,
                    None => continue,
                },
                _ => continue,
            };
            if !value.is_null() && !value.is_undefined() {
                return Ok(truthy(scope, value));
            }
        }
        Ok(false)
    }

    #[function(name = "_isArrayBufferView")]
    pub fn is_array_buffer_view(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        super::readable::is_array_buffer_view(scope, value)
    }

    #[function(name = "_isUint8Array")]
    pub fn is_uint8_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        as_object(scope, value).is_some_and(|object| object.cast::<js::Uint8Array>().is_ok())
    }

    #[function(name = "_uint8ArrayToBuffer")]
    pub fn uint8_array_to_buffer<'s>(
        scope: &'s Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        super::readable::view_to_buffer(scope, value)
    }
}

#[core_runtime::jsmodule(node_args, name = "node:stream/promises", default_namespace)]
pub mod promises {
    use super::*;

    /// <https://nodejs.org/api/stream.html#streampipelinesource-transforms-destination-options>
    pub fn pipeline<'s>(
        scope: &'s Scope<'_>,
        streams: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        let streams = streams
            .iter()
            .map(|stream| scope.root_value(stream.get()))
            .collect();
        super::pipeline::pipeline_promise(scope, streams)
    }

    /// <https://nodejs.org/api/stream.html#streamfinishedstream-options>: an invalid
    /// `options.cleanup` throws, and other invalid arguments reject the promise.
    pub fn finished<'s>(
        scope: &'s Scope<'_>,
        stream: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        super::eos::finished_promise(scope, stream, options)?.to_jsval_throwing(scope)
    }
}

/// Register `node:stream`, whose `promises` is `node:stream/promises`' `default` export.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let promises = || -> Result<HandleValue<'_>, ExnThrown> {
        let namespace = core_runtime::module::builtin_namespace(scope, "node:stream/promises")?
            .ok_or(ExnThrown)?;
        namespace.val(scope, c"default")
    };
    let Ok(promises) = promises() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(
        scope,
        name,
        &[("promises", promises)],
    )
}
