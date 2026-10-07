// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:stream` and `node:stream/promises`. See <https://nodejs.org/api/stream.html>.
//!
//! The implementation follows Node.js v24's `lib/internal/streams` (MIT license, Copyright
//! Node.js contributors), function by function. A stream's state is the Rust data of
//! [`state::StreamState`], which its `_readableState` and `_writableState` views expose with
//! Node's names, so code that inspects them reads the same values. `process.nextTick` work runs
//! in the tick queue of `process.nextTick`.

pub(crate) mod adapters;
mod destroy;
mod duplex;
mod duplex_pair;
mod eos;
mod from;
mod legacy;
mod operators;
mod pipeline;
mod readable;
pub(crate) mod state;
pub(crate) mod util;
mod writable;

pub(crate) use destroy::stream_destroyed;
pub(crate) use duplex::{duplex as duplex_class, init_duplex, init_transform, transform};
pub(crate) use eos::{eos as end_of_stream, eos_native, EosCallback, EosOptions};
pub(crate) use legacy::stream as stream_class;
pub(crate) use readable::{init_new_readable, readable as readable_class, READABLE_PROPERTIES};
pub(crate) use writable::writable as writable_class;

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use crate::errors::{NodeError, Thrown};
use util::{is_node_stream, sides, Field};

/// Register the stream module's classes for `global`: its internal classes, and the stream
/// classes' constructors.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    state::register_classes(scope, global);
    AbortSignalLink::add_to_global(scope, global);
    destroy::ConstructState::add_to_global(scope, global);
    destroy::DestroyRequest::add_to_global(scope, global);
    duplex::FlushState::add_to_global(scope, global);
    duplex::TransformWrite::add_to_global(scope, global);
    duplex_pair::DuplexSide::add_to_global(scope, global);
    eos::EosState::add_to_global(scope, global);
    eos::FinishedState::add_to_global(scope, global);
    legacy::LegacyPipeState::add_to_global(scope, global);
    pipeline::DestroyerState::add_to_global(scope, global);
    pipeline::FinalStageState::add_to_global(scope, global);
    pipeline::PipeLinkState::add_to_global(scope, global);
    pipeline::PipelineState::add_to_global(scope, global);
    readable::DisposeState::add_to_global(scope, global);
    readable::IteratorState::add_to_global(scope, global);
    readable::IteratorStep::add_to_global(scope, global);
    readable::PipeState::add_to_global(scope, global);
    readable::WrapState::add_to_global(scope, global);
    writable::WritevCallbacks::add_to_global(scope, global);
    adapters::DestroyDone::add_to_global(scope, global);
    adapters::FromWebState::add_to_global(scope, global);
    adapters::ReadableToWebState::add_to_global(scope, global);
    adapters::WebWrite::add_to_global(scope, global);
    adapters::WritableToWebState::add_to_global(scope, global);
    from::FromClose::add_to_global(scope, global);
    operators::register_classes(scope, global);
    from::FromState::add_to_global(scope, global);
    legacy::stream::register(scope)?;
    readable::readable::register(scope)?;
    writable::writable::register(scope)?;
    duplex::duplex::register(scope)?;
    duplex::transform::register(scope)?;
    duplex::pass_through::register(scope)?;
    Ok(())
}

/// The writable state's `needDrain` of `stream`, or `false` for a stream without one.
pub(crate) fn writable_need_drain(
    scope: &Scope<'_>,
    stream: Object<'_>,
) -> Result<bool, ExnThrown> {
    match sides(scope, stream)?.writable {
        Some(side) => side.need_drain(scope),
        None => Ok(false),
    }
}

/// The readable state's `resumeScheduled` of `stream`, or `false` for a stream without one.
pub(crate) fn readable_resume_scheduled(
    scope: &Scope<'_>,
    stream: Object<'_>,
) -> Result<bool, ExnThrown> {
    Ok(match state::ReadableSide::try_of(scope, stream)? {
        Some(side) => side.has(state::R::RESUME_SCHEDULED),
        None => false,
    })
}

/// The readable state's `autoDestroy` of `stream`, or `None` for a stream without one.
pub(crate) fn readable_auto_destroy(
    scope: &Scope<'_>,
    stream: Object<'_>,
) -> Result<Option<bool>, ExnThrown> {
    match sides(scope, stream)?.readable {
        Some(side) => Ok(Some(side.truthy(scope, Field::AutoDestroy)?)),
        None => Ok(None),
    }
}

/// The writable state's `autoDestroy` of `stream`, or `None` for a stream without one.
pub(crate) fn writable_auto_destroy(
    scope: &Scope<'_>,
    stream: Object<'_>,
) -> Result<Option<bool>, ExnThrown> {
    match sides(scope, stream)?.writable {
        Some(side) => Ok(Some(side.truthy(scope, Field::AutoDestroy)?)),
        None => Ok(None),
    }
}

/// Set the writable state's `corked` count of `stream`, if it has a writable state.
pub(crate) fn set_writable_corked(
    scope: &Scope<'_>,
    stream: Object<'_>,
    corked: f64,
) -> Result<(), ExnThrown> {
    match sides(scope, stream)?.writable {
        Some(util::Side::Writable(side)) => side.get_mut().corked = corked,
        Some(util::Side::Foreign(state)) => state.set_property(scope, c"corked", corked)?,
        _ => {}
    }
    Ok(())
}

/// The link [`add_abort_signal`] makes between a stream and a signal: the abort listener, which
/// `finished` removes.
#[core_runtime::jsclass(hidden)]
pub(crate) struct AbortSignalLink {
    stream: Heap<js::object::Object>,
    signal: Heap<js::object::Object>,
    listener: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl AbortSignalLink {}

/// The abort listener of [`add_abort_signal`], whose payload is an [`AbortSignalLink`]: destroy
/// the stream with an `AbortError`.
fn on_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = cast_payload::<AbortSignalLink>(scope, payload);
    let (stream, signal) = {
        let data = link.data();
        (data.stream.get(scope), data.signal.get(scope))
    };
    let reason = signal.get_property(scope, c"reason")?;
    let error = crate::errors::abort_error(scope, Some(reason))?;
    js::Function::call_by_name(scope, stream.handle(), c"destroy", &[error])?;
    Ok(js::value::undefined())
}

/// The `finished` callback of [`add_abort_signal`], whose payload is an [`AbortSignalLink`]:
/// remove the abort listener.
fn remove_abort_listener(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let link = cast_payload::<AbortSignalLink>(scope, payload);
    let (signal, listener) = {
        let data = link.data();
        (data.signal.get(scope), data.listener.get(scope))
    };
    if let Some(listener) = listener {
        crate::events::listen(scope, signal, c"removeEventListener", c"abort", listener)?;
    }
    Ok(js::value::undefined())
}

/// The check of `addAbortSignal`'s `signal`, Node.js's own `validateAbortSignal` in
/// `add-abort-signal.js`: an object that isn't a function and has an `aborted` property. `null`
/// throws the `TypeError` of the `in` operator.
fn validate_signal(scope: &Scope<'_>, signal: HandleValue<'_>) -> Result<(), Thrown> {
    if signal.is_null() {
        let message = "Cannot use 'in' operator to search for 'aborted' in null";
        return Err(js::error::TypeError(message.into()).throw(scope).into());
    }
    let valid = match js::Object::from_value(scope, signal) {
        Ok(object) if !object.is_callable() => object.has_property(scope, c"aborted")?,
        _ => false,
    };
    if !valid {
        let expected = "an instance of AbortSignal";
        return Err(NodeError::invalid_arg_type(scope, "signal", expected, signal).into());
    }
    Ok(())
}

/// `stream.addAbortSignal(signal, stream)`: destroy `stream` with an `AbortError` once `signal`
/// aborts.
pub(crate) fn add_abort_signal<'s>(
    scope: &'s Scope<'_>,
    signal: HandleValue<'_>,
    stream: HandleValue<'s>,
) -> Result<HandleValue<'s>, Thrown> {
    validate_signal(scope, signal)?;
    if !is_node_stream(scope, stream)? {
        return Err(NodeError::invalid_arg_type(
            scope,
            "stream",
            "an instance of ReadableStream, WritableStream, or Stream",
            stream,
        )
        .into());
    }
    let Ok(signal_object) = js::Object::from_value(scope, signal) else {
        return Ok(stream);
    };
    let stream_object = js::Object::from_value(scope, stream).expect("streams are objects");
    let link =
        js::class::create_instance_with::<AbortSignalLinkImpl>(scope, |_| AbortSignalLinkImpl {
            stream: Heap::from(stream_object),
            signal: Heap::from(signal_object),
            listener: None,
        })?;
    if js::conversion::to_boolean(signal_object.get_property(scope, c"aborted")?) {
        on_abort(
            scope,
            CallbackArgs::from_values(&[]),
            link.to_jsval_throwing(scope)?,
        )?;
    } else {
        let listener = js::Function::new_callback(scope, c"", 0, on_abort, link)?;
        link.data_mut().listener = Some(Heap::from(*listener));
        crate::events::listen(
            scope,
            signal_object,
            c"addEventListener",
            c"abort",
            *listener,
        )?;
        let payload = link.to_jsval_throwing(scope)?;
        eos::eos_native(
            scope,
            stream_object,
            eos::EosOptions::default(),
            eos::EosCallback::Native(remove_abort_listener, payload),
        )?;
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
    #[function(length = 3)]
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
    #[function(length = 2)]
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
    #[function(length = 2)]
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
    #[function(length = 1)]
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
        let Ok(stream) = js::Object::from_value(scope, stream) else {
            return Ok(false);
        };
        Ok(
            js::conversion::to_boolean(stream.get_property(scope, c"readableDidRead")?)
                || js::conversion::to_boolean(stream.get_property(scope, c"readableAborted")?),
        )
    }

    /// <https://nodejs.org/api/stream.html#streamiserroredstream>
    pub fn is_errored(scope: &Scope<'_>, stream: HandleValue<'_>) -> Result<bool, ExnThrown> {
        let Ok(stream) = js::Object::from_value(scope, stream) else {
            return Ok(false);
        };
        for property in [c"readableErrored", c"writableErrored"] {
            let value = stream.get_property(scope, property)?;
            if !value.is_null_or_undefined() {
                return Ok(js::conversion::to_boolean(value));
            }
        }
        let sides = sides(scope, stream)?;
        for field in [Field::ErrorEmitted, Field::Errored] {
            for side in [sides.readable, sides.writable].into_iter().flatten() {
                let value = side.get(scope, field)?;
                if !value.is_null_or_undefined() {
                    return Ok(js::conversion::to_boolean(value));
                }
            }
        }
        Ok(false)
    }

    #[function(name = "_isArrayBufferView")]
    pub fn is_array_buffer_view(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        js::ArrayBufferView::from_value(scope, value).is_some()
    }

    #[function(name = "_isUint8Array")]
    pub fn is_uint8_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        js::Object::from_value(scope, value)
            .is_ok_and(|object| object.cast::<js::Uint8Array>().is_ok())
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
    #[function(length = 2)]
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
        namespace.get_property(scope, c"default")
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
