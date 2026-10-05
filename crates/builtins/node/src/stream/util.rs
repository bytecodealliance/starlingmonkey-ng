// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The predicates of Node's `lib/internal/streams/utils.js`.
//!
//! Node's predicates read the `_readableState` and `_writableState` of any object, which includes
//! the streams of other implementations, such as the `readable-stream` package. They read the state
//! of this module's streams from Rust, and the state objects of other streams by their properties.

use std::ffi::CStr;

use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use super::state::{ReadableSide, WritableSide, R, W};
use crate::support::{has_method, is_true};

/// `Symbol.for('nodejs.stream.destroyed')`, which marks a destroyed stream.
pub(crate) static STREAM_DESTROYED: js::symbol::CachedSymbol =
    js::symbol::CachedSymbol::registered(c"nodejs.stream.destroyed");

/// A field of a readable or writable state, by its name in Node's `ReadableState` and
/// `WritableState`.
#[derive(Clone, Copy)]
pub(crate) enum Field {
    Destroyed,
    Errored,
    ErrorEmitted,
    Closed,
    Ended,
    EndEmitted,
    Finished,
    Length,
    AutoDestroy,
    EmitClose,
    NeedDrain,
    Pendingcb,
    /// The readable side's `readable`.
    Readable,
    /// The writable side's `writable`.
    Writable,
}

impl Field {
    fn name(self) -> &'static CStr {
        match self {
            Field::Destroyed => c"destroyed",
            Field::Errored => c"errored",
            Field::ErrorEmitted => c"errorEmitted",
            Field::Closed => c"closed",
            Field::Ended => c"ended",
            Field::EndEmitted => c"endEmitted",
            Field::Finished => c"finished",
            Field::Length => c"length",
            Field::AutoDestroy => c"autoDestroy",
            Field::EmitClose => c"emitClose",
            Field::NeedDrain => c"needDrain",
            Field::Pendingcb => c"pendingcb",
            Field::Readable => c"readable",
            Field::Writable => c"writable",
        }
    }
}

/// One side of a stream's state: the Rust state of one of this module's streams, or the state
/// object of another stream.
#[derive(Clone, Copy)]
pub(crate) enum Side<'s> {
    Readable(ReadableSide<'s>),
    Writable(WritableSide<'s>),
    Foreign(Object<'s>),
}

impl<'s> Side<'s> {
    /// The value of `field`, as the state object's property of that name has it.
    pub(crate) fn get<'r>(
        &self,
        scope: &'r Scope<'_>,
        field: Field,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        Ok(scope.root_value(match self {
            Side::Foreign(state) => return state.get_property(scope, field.name()),
            Side::Readable(side) => {
                let flag = |flag: R| js::value::from_bool(side.has(flag));
                match field {
                    Field::Destroyed => flag(R::DESTROYED),
                    Field::Errored => {
                        return Ok(side.errored(scope).unwrap_or(HandleValue::null()))
                    }
                    Field::ErrorEmitted => flag(R::ERROR_EMITTED),
                    Field::Closed => flag(R::CLOSED),
                    Field::Ended => flag(R::ENDED),
                    Field::EndEmitted => flag(R::END_EMITTED),
                    Field::Length => js::value::from_f64(side.length()),
                    Field::AutoDestroy => flag(R::AUTO_DESTROY),
                    Field::EmitClose => flag(R::EMIT_CLOSE),
                    Field::Readable => return Ok(side.get().readable.get(scope)),
                    Field::Finished | Field::NeedDrain | Field::Pendingcb | Field::Writable => {
                        js::value::undefined()
                    }
                }
            }
            Side::Writable(side) => {
                let flag = |flag: W| js::value::from_bool(side.has(flag));
                match field {
                    Field::Destroyed => flag(W::DESTROYED),
                    Field::Errored => {
                        return Ok(side.errored(scope).unwrap_or(HandleValue::null()))
                    }
                    Field::ErrorEmitted => flag(W::ERROR_EMITTED),
                    Field::Closed => flag(W::CLOSED),
                    Field::Ended => flag(W::ENDED),
                    Field::Finished => flag(W::FINISHED),
                    Field::Length => js::value::from_f64(side.length()),
                    Field::AutoDestroy => flag(W::AUTO_DESTROY),
                    Field::EmitClose => flag(W::EMIT_CLOSE),
                    Field::NeedDrain => flag(W::NEED_DRAIN),
                    Field::Pendingcb => js::value::from_f64(side.get().pendingcb),
                    Field::Writable => match side.get().writable {
                        Some(writable) => js::value::from_bool(writable),
                        None => js::value::undefined(),
                    },
                    Field::EndEmitted | Field::Readable => js::value::undefined(),
                }
            }
        }))
    }

    /// Whether `field` is truthy.
    pub(crate) fn truthy(&self, scope: &Scope<'_>, field: Field) -> Result<bool, ExnThrown> {
        Ok(js::conversion::to_boolean(self.get(scope, field)?))
    }

    /// `field` if it is a boolean.
    pub(crate) fn boolean(
        &self,
        scope: &Scope<'_>,
        field: Field,
    ) -> Result<Option<bool>, ExnThrown> {
        let value = self.get(scope, field)?;
        Ok(value.is_boolean().then(|| value.to_boolean()))
    }

    /// Whether `field` is the boolean `false`.
    fn is_false(&self, scope: &Scope<'_>, field: Field) -> Result<bool, ExnThrown> {
        Ok(self.boolean(scope, field)? == Some(false))
    }

    pub(crate) fn error_emitted(&self, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        self.truthy(scope, Field::ErrorEmitted)
    }

    pub(crate) fn need_drain(&self, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        self.truthy(scope, Field::NeedDrain)
    }
}

/// The readable and writable sides of a stream's state.
#[derive(Clone, Copy)]
pub(crate) struct Sides<'s> {
    pub(crate) readable: Option<Side<'s>>,
    pub(crate) writable: Option<Side<'s>>,
}

/// The sides of `stream`'s state: those of its Rust state, or else its `_readableState` and
/// `_writableState`, if they are objects.
pub(crate) fn sides<'s>(scope: &'s Scope<'_>, stream: Object<'s>) -> Result<Sides<'s>, ExnThrown> {
    if let Some(state) = super::state::try_state(scope, stream)? {
        return Ok(Sides {
            readable: ReadableSide::of_state(state).map(Side::Readable),
            writable: WritableSide::of_state(state).map(Side::Writable),
        });
    }
    let side = |name: &'static CStr| -> Result<Option<Side<'s>>, ExnThrown> {
        let state = stream.get_property(scope, name)?;
        Ok(Object::from_value(scope, state).ok().map(Side::Foreign))
    };
    Ok(Sides {
        readable: side(c"_readableState")?,
        writable: side(c"_writableState")?,
    })
}

/// Node's `isReadableNodeStream`.
pub(crate) fn is_readable_node_stream(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    strict: bool,
) -> Result<bool, ExnThrown> {
    let Ok(stream) = Object::from_value(scope, value) else {
        return Ok(false);
    };
    if !has_method(scope, stream, c"pipe")? || !has_method(scope, stream, c"on")? {
        return Ok(false);
    }
    if strict && (!has_method(scope, stream, c"pause")? || !has_method(scope, stream, c"resume")?) {
        return Ok(false);
    }
    let sides = sides(scope, stream)?;
    if sides.writable.is_some() {
        let Some(readable) = sides.readable else {
            return Ok(false);
        };
        if readable.is_false(scope, Field::Readable)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Node's `isWritableNodeStream`.
pub(crate) fn is_writable_node_stream(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    let Ok(stream) = Object::from_value(scope, value) else {
        return Ok(false);
    };
    if !has_method(scope, stream, c"write")? || !has_method(scope, stream, c"on")? {
        return Ok(false);
    }
    let sides = sides(scope, stream)?;
    if sides.readable.is_some() {
        if let Some(writable) = sides.writable {
            if writable.is_false(scope, Field::Writable)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Node's `isNodeStream`.
pub(crate) fn is_node_stream(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Ok(stream) = Object::from_value(scope, value) else {
        return Ok(false);
    };
    let sides = sides(scope, stream)?;
    if sides.readable.is_some() || sides.writable.is_some() {
        return Ok(true);
    }
    let on = has_method(scope, stream, c"on")?;
    Ok(on && (has_method(scope, stream, c"write")? || has_method(scope, stream, c"pipe")?))
}

/// Node's `isDestroyed`, with `None` for a value that isn't a stream.
pub(crate) fn is_destroyed(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    if !is_node_stream(scope, value)? {
        return Ok(None);
    }
    let stream = Object::from_value(scope, value).expect("streams are objects");
    if js::conversion::to_boolean(stream.get_property(scope, c"destroyed")?) {
        return Ok(Some(true));
    }
    let key = STREAM_DESTROYED.id(scope)?;
    if js::conversion::to_boolean(stream.get_property_by_id(scope, key)?) {
        return Ok(Some(true));
    }
    let sides = sides(scope, stream)?;
    match sides.writable.or(sides.readable) {
        Some(side) => Ok(Some(side.truthy(scope, Field::Destroyed)?)),
        None => Ok(Some(false)),
    }
}

/// Node's `isWritableEnded`.
pub(crate) fn is_writable_ended(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    if !is_writable_node_stream(scope, value)? {
        return Ok(None);
    }
    let stream = Object::from_value(scope, value).expect("streams are objects");
    if is_true(stream.get_property(scope, c"writableEnded")?) {
        return Ok(Some(true));
    }
    let Some(side) = sides(scope, stream)?.writable else {
        return Ok(None);
    };
    if side.truthy(scope, Field::Errored)? {
        return Ok(Some(false));
    }
    side.boolean(scope, Field::Ended)
}

/// Node's `isWritableFinished`.
pub(crate) fn is_writable_finished(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    strict: bool,
) -> Result<Option<bool>, ExnThrown> {
    if !is_writable_node_stream(scope, value)? {
        return Ok(None);
    }
    let stream = Object::from_value(scope, value).expect("streams are objects");
    if is_true(stream.get_property(scope, c"writableFinished")?) {
        return Ok(Some(true));
    }
    let Some(side) = sides(scope, stream)?.writable else {
        return Ok(None);
    };
    if side.truthy(scope, Field::Errored)? {
        return Ok(Some(false));
    }
    let Some(finished) = side.boolean(scope, Field::Finished)? else {
        return Ok(None);
    };
    if finished || strict || side.boolean(scope, Field::Ended)? != Some(true) {
        return Ok(Some(finished));
    }
    let length = side.get(scope, Field::Length)?;
    Ok(Some(length.is_number() && length.to_number() == 0.0))
}

/// Node's `isReadableFinished`.
pub(crate) fn is_readable_finished(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    strict: bool,
) -> Result<Option<bool>, ExnThrown> {
    if !is_readable_node_stream(scope, value, false)? {
        return Ok(None);
    }
    let stream = Object::from_value(scope, value).expect("streams are objects");
    let Some(side) = sides(scope, stream)?.readable else {
        return Ok(None);
    };
    if side.truthy(scope, Field::Errored)? {
        return Ok(Some(false));
    }
    let Some(end_emitted) = side.boolean(scope, Field::EndEmitted)? else {
        return Ok(None);
    };
    if end_emitted || strict || side.boolean(scope, Field::Ended)? != Some(true) {
        return Ok(Some(end_emitted));
    }
    let length = side.get(scope, Field::Length)?;
    Ok(Some(length.is_number() && length.to_number() == 0.0))
}

/// Node's `isReadable`.
pub(crate) fn is_readable(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    let Ok(stream) = Object::from_value(scope, value) else {
        return Ok(None);
    };
    let readable = stream.get_property(scope, c"readable")?;
    if !readable.is_boolean() {
        return Ok(None);
    }
    if is_destroyed(scope, value)? == Some(true) {
        return Ok(Some(false));
    }
    Ok(Some(
        is_readable_node_stream(scope, value, false)?
            && readable.to_boolean()
            && is_readable_finished(scope, value, true)? != Some(true),
    ))
}

/// Node's `isWritable`.
pub(crate) fn is_writable(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    let Ok(stream) = Object::from_value(scope, value) else {
        return Ok(None);
    };
    let writable = stream.get_property(scope, c"writable")?;
    if !writable.is_boolean() {
        return Ok(None);
    }
    if is_destroyed(scope, value)? == Some(true) {
        return Ok(Some(false));
    }
    Ok(Some(
        is_writable_node_stream(scope, value)?
            && writable.to_boolean()
            && is_writable_ended(scope, value)? != Some(true),
    ))
}

/// Node's `isFinished`, for both sides of a stream.
pub(crate) fn is_finished(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<bool>, ExnThrown> {
    if !is_node_stream(scope, value)? {
        return Ok(None);
    }
    if is_destroyed(scope, value)? == Some(true) {
        return Ok(Some(true));
    }
    if is_readable(scope, value)? == Some(true) || is_writable(scope, value)? == Some(true) {
        return Ok(Some(false));
    }
    Ok(Some(true))
}

/// Node's `isWritableErrored` or `isReadableErrored`: `stream[property]` if it is truthy, and
/// else the error of `side`, or `None`.
pub(crate) fn side_errored<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'_>,
    property: &'static CStr,
    side: Option<Side<'_>>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let direct = stream.get_property(scope, property)?;
    if js::conversion::to_boolean(direct) {
        return Ok(Some(direct));
    }
    let Some(side) = side else {
        return Ok(None);
    };
    let errored = side.get(scope, Field::Errored)?;
    Ok((!errored.is_null_or_undefined()).then_some(errored))
}

/// Node's `isClosed`.
pub(crate) fn is_closed(scope: &Scope<'_>, stream: Object<'_>) -> Result<Option<bool>, ExnThrown> {
    let closed = stream.get_property(scope, c"closed")?;
    if closed.is_boolean() {
        return Ok(Some(closed.to_boolean()));
    }
    let sides = sides(scope, stream)?;
    let side = |side: Option<Side<'_>>| -> Result<Option<bool>, ExnThrown> {
        match side {
            Some(side) => side.boolean(scope, Field::Closed),
            None => Ok(None),
        }
    };
    let (w, r) = (side(sides.writable)?, side(sides.readable)?);
    if w.is_some() || r.is_some() {
        return Ok(Some(w == Some(true) || r == Some(true)));
    }
    Ok(None)
}

/// Node's `willEmitClose`.
pub(crate) fn will_emit_close(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    let sides = sides(scope, stream)?;
    let Some(side) = sides.writable.or(sides.readable) else {
        return Ok(false);
    };
    Ok(side.truthy(scope, Field::AutoDestroy)?
        && side.truthy(scope, Field::EmitClose)?
        && side.boolean(scope, Field::Closed)? == Some(false))
}

/// Whether `value` has a `Symbol.asyncIterator` method, or a `Symbol.iterator` method if
/// `include_sync`.
pub(crate) fn is_iterable(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    include_sync: bool,
) -> Result<bool, ExnThrown> {
    if value.is_null_or_undefined() {
        return Ok(false);
    }
    let Ok(object) = Object::from_value(scope, value) else {
        return Ok(include_sync && value.is_string());
    };
    let codes = if include_sync {
        &[
            js::native::SymbolCode::asyncIterator,
            js::native::SymbolCode::iterator,
        ][..]
    } else {
        &[js::native::SymbolCode::asyncIterator][..]
    };
    for &code in codes {
        let key = js::symbol::get_well_known_key(scope, code);
        if js::function::is_callable(object.get_property_by_id(scope, scope.root_id(key))?) {
            return Ok(true);
        }
    }
    Ok(false)
}
