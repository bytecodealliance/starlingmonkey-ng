// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The state of a stream: Node's `ReadableState` and `WritableState`, and the high water marks of
//! `lib/internal/streams/state.js`.
//!
//! A stream's state is the private data of a [`StreamState`] instance: the stream itself, or the
//! handle a `Readable.call(obj)` gives `obj`. It holds a [`ReadablePart`] for a readable stream, a
//! [`WritablePart`] for a writable stream, and both for a duplex stream. The `_readableState` and
//! `_writableState` properties a stream gets are [`ReadableState`] and [`WritableState`] views of
//! it, whose prototype accessors read and write the fields with Node's coercions.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::CStr;

use bitflags::bitflags;
use js::class::{Ref, RefMut};
use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{NodeError, Thrown};
use crate::support::as_array;

js::instance_local! {
    /// The default high water mark of byte streams, which `setDefaultHighWaterMark` changes.
    static DEFAULT_BYTES: Cell<f64> = const { Cell::new(if cfg!(windows) { 16384.0 } else { 65536.0 }) };

    /// The default high water mark of object-mode streams.
    static DEFAULT_OBJECTS: Cell<f64> = const { Cell::new(16.0) };
}

/// The default high water mark for object-mode streams if `object_mode`, and byte streams
/// otherwise.
pub(crate) fn default_high_water_mark(object_mode: bool) -> f64 {
    if object_mode {
        DEFAULT_OBJECTS.with(Cell::get)
    } else {
        DEFAULT_BYTES.with(Cell::get)
    }
}

/// Set the default high water mark for object-mode streams if `object_mode`, and byte streams
/// otherwise.
pub(crate) fn set_default_high_water_mark(object_mode: bool, value: f64) {
    if object_mode {
        DEFAULT_OBJECTS.with(|cell| cell.set(value));
    } else {
        DEFAULT_BYTES.with(|cell| cell.set(value));
    }
}

/// Node's `getHighWaterMark`: `options.highWaterMark`, else `options[duplex_key]` for a duplex
/// stream, else the default for `object_mode`. A value that isn't a non-negative integer is
/// `ERR_INVALID_ARG_VALUE` naming `options[duplex_key]` for a duplex stream and
/// `options.highWaterMark` otherwise.
pub(crate) fn high_water_mark(
    scope: &Scope<'_>,
    options: Object<'_>,
    duplex_key: &'static CStr,
    is_duplex: bool,
    object_mode: bool,
) -> Result<f64, Thrown> {
    let mut value = options.get_property(scope, c"highWaterMark")?;
    if value.is_null_or_undefined() && is_duplex {
        value = options.get_property(scope, duplex_key)?;
    }
    if value.is_null_or_undefined() {
        return Ok(default_high_water_mark(object_mode));
    }
    let number = value.is_number().then(|| value.to_number());
    match number {
        Some(number) if number >= 0.0 && number.fract() == 0.0 && number.is_finite() => Ok(number),
        _ => {
            let name = if is_duplex {
                format!("options.{}", duplex_key.to_str().unwrap_or_default())
            } else {
                "options.highWaterMark".to_string()
            };
            Err(NodeError::invalid_arg_value(scope, &name, value, "is invalid").into())
        }
    }
}

/// The high water mark passed to `setDefaultHighWaterMark`, validated as Node's
/// `validateInteger(value, 'value', 0)` does.
pub(crate) fn validate_default(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, Thrown> {
    crate::errors::validate_integer(scope, value, "value", 0.0, crate::errors::MAX_SAFE_INTEGER)
}

bitflags! {
    /// The boolean fields of a [`ReadablePart`], Node's `ReadableState` bits.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct R: u32 {
        const OBJECT_MODE = 1 << 0;
        const ENDED = 1 << 1;
        const END_EMITTED = 1 << 2;
        const READING = 1 << 3;
        const CONSTRUCTED = 1 << 4;
        const SYNC = 1 << 5;
        const NEED_READABLE = 1 << 6;
        const EMITTED_READABLE = 1 << 7;
        const READABLE_LISTENING = 1 << 8;
        const RESUME_SCHEDULED = 1 << 9;
        const ERROR_EMITTED = 1 << 10;
        const EMIT_CLOSE = 1 << 11;
        const AUTO_DESTROY = 1 << 12;
        const DESTROYED = 1 << 13;
        const CLOSED = 1 << 14;
        const CLOSE_EMITTED = 1 << 15;
        const MULTI_AWAIT_DRAIN = 1 << 16;
        const READING_MORE = 1 << 17;
        const DATA_EMITTED = 1 << 18;
        /// Node's `kDataListening`, which has no accessor.
        const DATA_LISTENING = 1 << 19;
        /// Whether `Readable.toWeb` added its no-op `error` listener, which Node.js records in
        /// the `kErrorSentinelAttached` property.
        const ERROR_SENTINEL_ATTACHED = 1 << 20;
    }
}

bitflags! {
    /// The boolean fields of a [`WritablePart`], Node's `WritableState` bits.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct W: u32 {
        const OBJECT_MODE = 1 << 0;
        const FINAL_CALLED = 1 << 1;
        const NEED_DRAIN = 1 << 2;
        const ENDING = 1 << 3;
        const ENDED = 1 << 4;
        const FINISHED = 1 << 5;
        const DESTROYED = 1 << 6;
        const DECODE_STRINGS = 1 << 7;
        const WRITING = 1 << 8;
        const SYNC = 1 << 9;
        const BUFFER_PROCESSING = 1 << 10;
        const CONSTRUCTED = 1 << 11;
        const PREFINISHED = 1 << 12;
        const ERROR_EMITTED = 1 << 13;
        const EMIT_CLOSE = 1 << 14;
        const AUTO_DESTROY = 1 << 15;
        const CLOSED = 1 << 16;
        const CLOSE_EMITTED = 1 << 17;
        const ALL_BUFFERS = 1 << 18;
        const ALL_NOOP = 1 << 19;
        /// Node's `kExpectWriteCb`, which has no accessor.
        const EXPECT_WRITE_CB = 1 << 20;
        /// Node's `kAfterWritePending`, which has no accessor.
        const AFTER_WRITE_PENDING = 1 << 21;
    }
}

/// The readable part of a stream's state: Node's `ReadableState`.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct ReadablePart {
    #[no_trace]
    pub(crate) flags: R,
    /// `flowing`: `None` until the stream starts or stops flowing.
    #[no_trace]
    pub(crate) flowing: Option<bool>,
    /// Node's `kHasPaused` and `kPaused`, which `isPaused()` reads.
    #[no_trace]
    pub(crate) paused: Option<bool>,
    #[no_trace]
    pub(crate) high_water_mark: f64,
    #[no_trace]
    pub(crate) length: f64,
    /// The index of the first chunk in `buffer` that wasn't read yet.
    #[no_trace]
    pub(crate) buffer_index: f64,
    /// The buffered chunks, Node's `buffer` array, with `null` below `buffer_index`.
    pub(crate) buffer: Vec<Heap<Value>>,
    /// The destinations of `pipe`, Node's `pipes` array.
    pub(crate) pipes: Vec<Heap<Value>>,
    /// The writers a pipe waits for `drain` from.
    pub(crate) await_drain_writers: AwaitDrainWriters,
    /// The error, which is only ever a truthy value.
    pub(crate) errored: Option<Heap<Value>>,
    /// The default encoding, or `None` for `utf8`.
    pub(crate) default_encoding: Option<Heap<Value>>,
    pub(crate) decoder: Option<Heap<js::object::Object>>,
    pub(crate) encoding: Option<Heap<Value>>,
    /// `readable`, which `Duplex` sets to `false` for a stream that isn't readable.
    pub(crate) readable: Heap<Value>,
}

impl ReadablePart {
    /// `readable`, as `Readable.prototype.readable` computes it.
    pub(crate) fn is_readable(&self) -> bool {
        // SAFETY: only the tag and the boolean payload of the value are read.
        let disabled =
            unsafe { self.readable.as_raw().is_boolean() && !self.readable.as_raw().to_boolean() };
        !disabled
            && !self
                .flags
                .intersects(R::DESTROYED | R::ERROR_EMITTED | R::END_EMITTED)
    }
}

/// Node's `awaitDrainWriters`.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) enum AwaitDrainWriters {
    /// `null`, a writer, or another value assigned to `awaitDrainWriters`.
    Value(Heap<Value>),
    /// A `Set` of writers, in insertion order, which `pipe` creates once a stream has a second
    /// destination.
    Set(Vec<Heap<Value>>),
}

/// The rooted contents of [`AwaitDrainWriters`].
pub(crate) enum DrainWriters<'s> {
    Value(HandleValue<'s>),
    Set(Vec<HandleValue<'s>>),
}

/// A write the writable part buffers: Node's `{ chunk, encoding, callback }`, with `undefined` as
/// the callback of a write without one.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct BufferedWrite {
    pub(crate) chunk: Heap<Value>,
    pub(crate) encoding: Heap<Value>,
    pub(crate) callback: Heap<Value>,
}

/// The writable part of a stream's state: Node's `WritableState`.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(crate) struct WritablePart {
    #[no_trace]
    pub(crate) flags: W,
    /// `writable`: `None` until it is set.
    #[no_trace]
    pub(crate) writable: Option<bool>,
    #[no_trace]
    pub(crate) high_water_mark: f64,
    #[no_trace]
    pub(crate) length: f64,
    #[no_trace]
    pub(crate) corked: f64,
    #[no_trace]
    pub(crate) writelen: f64,
    #[no_trace]
    pub(crate) pendingcb: f64,
    /// The error, which is only ever a truthy value.
    pub(crate) errored: Option<Heap<Value>>,
    /// The default encoding, or `None` for `utf8`.
    pub(crate) default_encoding: Option<Heap<Value>>,
    /// The callback of the write in progress, or `None` for Node's `nop`.
    pub(crate) writecb: Option<Heap<Value>>,
    /// The callback `_write` gets, created once per state.
    pub(crate) onwrite: Heap<js::object::Object>,
    /// Node's `afterWriteTickInfo`.
    pub(crate) after_write_tick_info: Option<Heap<AfterWriteTickInfoImpl>>,
    /// The buffered writes.
    pub(crate) buffered: VecDeque<BufferedWrite>,
    /// The callbacks `end()` got before the stream finished.
    pub(crate) on_finished: Vec<Heap<Value>>,
}

impl WritablePart {
    /// `writable`, as `Writable.prototype.writable` computes it.
    pub(crate) fn is_writable(&self) -> bool {
        self.writable != Some(false)
            && !self.flags.intersects(W::ENDING | W::ENDED | W::DESTROYED)
            && self.errored.is_none()
    }
}

/// The state of a stream.
#[core_runtime::jsclass(hidden)]
pub(crate) struct StreamState {
    pub(crate) readable: Option<ReadablePart>,
    pub(crate) writable: Option<WritablePart>,
    /// A `Transform`'s pending write callback, Node's `kCallback`.
    pub(crate) transform_callback: Option<Heap<Value>>,
    /// The `_writableState` view, which `afterWriteTickInfo` reports as its `state`.
    pub(crate) writable_view: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl StreamState {}

impl StreamStateImpl {
    /// Whether the stream has both sides and both are destroyed, as `Duplex.prototype.destroyed`
    /// computes it.
    pub(crate) fn duplex_destroyed(&self) -> bool {
        match (&self.readable, &self.writable) {
            (Some(r), Some(w)) => r.flags.contains(R::DESTROYED) && w.flags.contains(W::DESTROYED),
            _ => false,
        }
    }
}

/// Node's `afterWriteTickInfo`: the write callback that a tick calls `count` times for writes
/// that completed synchronously.
#[core_runtime::jsclass(hidden)]
pub(crate) struct AfterWriteTickInfo {
    pub(crate) stream: Heap<js::object::Object>,
    pub(crate) callback: Heap<Value>,
    #[no_trace]
    pub(crate) count: f64,
}

#[core_runtime::jsmethods]
impl AfterWriteTickInfo {}

/// The state of `stream`, or `None` for an object without one.
pub(crate) fn try_state<'s>(
    scope: &'s Scope<'_>,
    stream: Object<'s>,
) -> Result<Option<StreamState<'s>>, ExnThrown> {
    super::readable::readable::try_state_of(scope, stream)
}

/// The readable part of a stream's state.
#[derive(Clone, Copy)]
pub(crate) struct ReadableSide<'s>(pub(crate) StreamState<'s>);

impl<'s> ReadableSide<'s> {
    /// The readable part of the state of `stream`, or `None` if it has none.
    pub(crate) fn try_of(
        scope: &'s Scope<'_>,
        stream: Object<'s>,
    ) -> Result<Option<Self>, ExnThrown> {
        Ok(try_state(scope, stream)?
            .filter(|state| state.data().readable.is_some())
            .map(ReadableSide))
    }

    /// The readable part of the state of `stream`, or `ERR_INVALID_THIS` if it has none.
    pub(crate) fn of(scope: &'s Scope<'_>, stream: Object<'s>) -> Result<Self, ExnThrown> {
        Self::try_of(scope, stream)?.ok_or_else(|| crate::errors::invalid_this(scope, "Readable"))
    }

    /// The readable part of `state`, if it has one.
    pub(crate) fn of_state(state: StreamState<'s>) -> Option<Self> {
        state
            .data()
            .readable
            .is_some()
            .then_some(ReadableSide(state))
    }

    pub(crate) fn get(&self) -> Ref<'_, ReadablePart> {
        Ref::map(self.0.data(), |data| {
            data.readable
                .as_ref()
                .expect("a readable side has a readable part")
        })
    }

    pub(crate) fn get_mut(&self) -> RefMut<'_, ReadablePart> {
        RefMut::map(self.0.data_mut(), |data| {
            data.readable
                .as_mut()
                .expect("a readable side has a readable part")
        })
    }

    /// Whether all of `flags` are set.
    pub(crate) fn has(&self, flags: R) -> bool {
        self.get().flags.contains(flags)
    }

    /// Whether any of `flags` is set.
    pub(crate) fn any(&self, flags: R) -> bool {
        self.get().flags.intersects(flags)
    }

    /// Set or clear `flags`.
    pub(crate) fn set(&self, flags: R, on: bool) {
        self.get_mut().flags.set(flags, on);
    }

    pub(crate) fn length(&self) -> f64 {
        self.get().length
    }

    pub(crate) fn high_water_mark(&self) -> f64 {
        self.get().high_water_mark
    }

    pub(crate) fn is_errored(&self) -> bool {
        self.get().errored.is_some()
    }

    /// `errored`, or `None` for `null`.
    pub(crate) fn errored<'r>(&self, scope: &'r Scope<'_>) -> Option<HandleValue<'r>> {
        self.get().errored.as_ref().map(|error| error.get(scope))
    }

    /// Set `errored` as Node's setter does: to a truthy `error`, and to `null` otherwise.
    pub(crate) fn set_errored(&self, error: HandleValue<'_>) {
        let mut part = self.get_mut();
        if js::conversion::to_boolean(error) {
            part.errored = Some(Heap::from(error.get()));
        } else {
            part.errored = None;
        }
    }

    /// Whether `readable` was set to `false`.
    pub(crate) fn disabled(&self) -> bool {
        let part = self.get();
        // SAFETY: only the tag and the boolean payload of the value are read.
        unsafe { part.readable.as_raw().is_boolean() && !part.readable.as_raw().to_boolean() }
    }

    /// The buffered chunk at `index`, or `undefined` past the end of the buffer.
    pub(crate) fn chunk<'r>(&self, scope: &'r Scope<'_>, index: usize) -> HandleValue<'r> {
        match self.get().buffer.get(index) {
            Some(chunk) => chunk.get(scope),
            None => HandleValue::undefined(),
        }
    }

    /// Replace the buffered chunk at `index` with `chunk`, if the buffer reaches `index`.
    pub(crate) fn set_chunk(&self, index: usize, chunk: HandleValue<'_>) {
        if let Some(slot) = self.get().buffer.get(index) {
            slot.set(chunk.get());
        }
    }

    /// The number of entries in the buffer, including the `null`s below `buffer_index`.
    pub(crate) fn buffer_len(&self) -> usize {
        self.get().buffer.len()
    }

    /// The buffer as a new array, as Node's `buffer` array holds it.
    pub(crate) fn buffer_array<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<js::Array<'r>, ExnThrown> {
        js::Array::with_contents(scope, &rooted(scope, &self.get().buffer))
    }

    /// The destinations of `pipe`.
    pub(crate) fn pipes<'r>(&self, scope: &'r Scope<'_>) -> Vec<HandleValue<'r>> {
        rooted(scope, &self.get().pipes)
    }

    /// The writers a pipe waits for `drain` from.
    pub(crate) fn await_drain_writers<'r>(&self, scope: &'r Scope<'_>) -> DrainWriters<'r> {
        match &self.get().await_drain_writers {
            AwaitDrainWriters::Value(value) => DrainWriters::Value(value.get(scope)),
            AwaitDrainWriters::Set(writers) => DrainWriters::Set(rooted(scope, writers)),
        }
    }

    /// Set the writers a pipe waits for `drain` from.
    pub(crate) fn set_await_drain_writers(&self, writers: DrainWriters<'_>) {
        self.get_mut().await_drain_writers = match writers {
            DrainWriters::Value(value) => AwaitDrainWriters::Value(Heap::from(value.get())),
            DrainWriters::Set(writers) => AwaitDrainWriters::Set(
                writers
                    .iter()
                    .map(|writer| Heap::from(writer.get()))
                    .collect(),
            ),
        };
    }

    /// `awaitDrainWriters` as a JS value: `null`, a writer, or a new `Set` of writers.
    pub(crate) fn await_drain_writers_value<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        match self.await_drain_writers(scope) {
            DrainWriters::Value(value) => Ok(value),
            DrainWriters::Set(writers) => {
                let set = js::Set::new(scope)?;
                for writer in writers {
                    set.add(scope, writer)?;
                }
                set.to_jsval_throwing(scope)
            }
        }
    }

    pub(crate) fn decoder<'r>(&self, scope: &'r Scope<'_>) -> Option<Object<'r>> {
        self.get().decoder.get(scope)
    }

    /// `encoding`, or `None` for `null`.
    pub(crate) fn encoding<'r>(&self, scope: &'r Scope<'_>) -> Option<HandleValue<'r>> {
        self.get()
            .encoding
            .as_ref()
            .map(|encoding| encoding.get(scope))
    }

    /// `defaultEncoding`.
    pub(crate) fn default_encoding<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let encoding = self
            .get()
            .default_encoding
            .as_ref()
            .map(|encoding| encoding.get(scope));
        match encoding {
            Some(encoding) => Ok(encoding),
            None => js::class::get_or_init_atom(scope, c"utf8"),
        }
    }
}

/// The values of `heaps`, rooted.
fn rooted<'r>(scope: &'r Scope<'_>, heaps: &[Heap<Value>]) -> Vec<HandleValue<'r>> {
    heaps.iter().map(|value| value.get(scope)).collect()
}

/// The writable part of a stream's state.
#[derive(Clone, Copy)]
pub(crate) struct WritableSide<'s>(pub(crate) StreamState<'s>);

impl<'s> WritableSide<'s> {
    /// The writable part of the state of `stream`, or `None` if it has none.
    pub(crate) fn try_of(
        scope: &'s Scope<'_>,
        stream: Object<'s>,
    ) -> Result<Option<Self>, ExnThrown> {
        Ok(try_state(scope, stream)?
            .filter(|state| state.data().writable.is_some())
            .map(WritableSide))
    }

    /// The writable part of the state of `stream`, or `ERR_INVALID_THIS` if it has none.
    pub(crate) fn of(scope: &'s Scope<'_>, stream: Object<'s>) -> Result<Self, ExnThrown> {
        Self::try_of(scope, stream)?.ok_or_else(|| crate::errors::invalid_this(scope, "Writable"))
    }

    /// The writable part of `state`, if it has one.
    pub(crate) fn of_state(state: StreamState<'s>) -> Option<Self> {
        state
            .data()
            .writable
            .is_some()
            .then_some(WritableSide(state))
    }

    pub(crate) fn get(&self) -> Ref<'_, WritablePart> {
        Ref::map(self.0.data(), |data| {
            data.writable
                .as_ref()
                .expect("a writable side has a writable part")
        })
    }

    pub(crate) fn get_mut(&self) -> RefMut<'_, WritablePart> {
        RefMut::map(self.0.data_mut(), |data| {
            data.writable
                .as_mut()
                .expect("a writable side has a writable part")
        })
    }

    /// Whether all of `flags` are set.
    pub(crate) fn has(&self, flags: W) -> bool {
        self.get().flags.contains(flags)
    }

    /// Whether any of `flags` is set.
    pub(crate) fn any(&self, flags: W) -> bool {
        self.get().flags.intersects(flags)
    }

    /// Set or clear `flags`.
    pub(crate) fn set(&self, flags: W, on: bool) {
        self.get_mut().flags.set(flags, on);
    }

    pub(crate) fn length(&self) -> f64 {
        self.get().length
    }

    pub(crate) fn is_errored(&self) -> bool {
        self.get().errored.is_some()
    }

    /// `errored`, or `None` for `null`.
    pub(crate) fn errored<'r>(&self, scope: &'r Scope<'_>) -> Option<HandleValue<'r>> {
        self.get().errored.as_ref().map(|error| error.get(scope))
    }

    /// Set `errored` as Node's setter does: to a truthy `error`, and to `null` otherwise.
    pub(crate) fn set_errored(&self, error: HandleValue<'_>) {
        let mut part = self.get_mut();
        if js::conversion::to_boolean(error) {
            part.errored = Some(Heap::from(error.get()));
        } else {
            part.errored = None;
        }
    }

    /// Whether `writable` was set to `false`.
    pub(crate) fn disabled(&self) -> bool {
        self.get().writable == Some(false)
    }

    /// Whether the stream is corked: Node's `kCorked`.
    pub(crate) fn corked(&self) -> bool {
        self.get().corked > 0.0
    }

    /// Whether writes are buffered: Node's `kBuffered`.
    pub(crate) fn has_buffered(&self) -> bool {
        !self.get().buffered.is_empty()
    }

    /// `defaultEncoding`.
    pub(crate) fn default_encoding<'r>(
        &self,
        scope: &'r Scope<'_>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let encoding = self
            .get()
            .default_encoding
            .as_ref()
            .map(|encoding| encoding.get(scope));
        match encoding {
            Some(encoding) => Ok(encoding),
            None => js::class::get_or_init_atom(scope, c"utf8"),
        }
    }

    pub(crate) fn add_pending(&self, delta: f64) {
        self.get_mut().pendingcb += delta;
    }
}

/// The value to store as the `defaultEncoding` field for `value`: `None` for `utf8`.
pub(crate) fn default_encoding_field<'v>(
    scope: &Scope<'_>,
    value: HandleValue<'v>,
) -> Result<Option<HandleValue<'v>>, ExnThrown> {
    if value.is_string() {
        let name = js::JSString::from_value(scope, *value)?;
        if name.equals_ascii(scope, c"utf8")? || name.equals_ascii(scope, c"utf-8")? {
            return Ok(None);
        }
    }
    Ok(Some(value))
}

/// The `_readableState` view of a stream's state, `Readable.ReadableState`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ReadableState {
    /// The object whose private data is the state.
    holder: Heap<StreamStateImpl>,
}

#[core_runtime::jsmethods]
impl ReadableState {
    /// `new ReadableState(options, stream, isDuplex)`: a state of its own, which no stream holds.
    #[constructor(length = 3)]
    fn new(
        &self,
        scope: &Scope<'_>,
        options: Option<HandleValue<'_>>,
        _stream: Option<HandleValue<'_>>,
        is_duplex: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let holder = js::class::create_instance_with::<StreamStateImpl>(scope, |_| {
            StreamStateImpl::default()
        })?;
        let is_duplex = is_duplex.is_some_and(js::conversion::to_boolean);
        super::readable::init_readable_part(scope, holder, options, is_duplex)?;
        self.data().holder.set(holder);
        Ok(())
    }
}

/// The `_writableState` view of a stream's state, `Writable.WritableState`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct WritableState {
    /// The object whose private data is the state.
    holder: Heap<StreamStateImpl>,
}

#[core_runtime::jsmethods]
impl WritableState {
    /// `new WritableState(options, stream, isDuplex)`: a state of its own, whose `onwrite` writes
    /// to `stream`.
    #[constructor(length = 3)]
    fn new(
        &self,
        scope: &Scope<'_>,
        options: Option<HandleValue<'_>>,
        stream: Option<HandleValue<'_>>,
        is_duplex: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let holder = js::class::create_instance_with::<StreamStateImpl>(scope, |_| {
            StreamStateImpl::default()
        })?;
        let is_duplex = is_duplex.is_some_and(js::conversion::to_boolean);
        let stream = stream
            .and_then(|stream| Object::from_value(scope, *stream).ok())
            .unwrap_or(*holder);
        super::writable::init_writable_part(scope, holder, stream, options, is_duplex)?;
        self.data().holder.set(holder);
        Ok(())
    }

    /// `state.getBuffer()`: the buffered writes.
    #[method]
    fn get_buffer<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Array<'r>, ExnThrown> {
        let state = self.data().holder.get(scope);
        match WritableSide::of_state(state) {
            Some(side) => buffered_array(scope, side),
            None => js::Array::new(scope, 0),
        }
    }
}

/// The `_readableState` view of `holder`'s state.
pub(crate) fn new_readable_view<'s>(
    scope: &'s Scope<'_>,
    holder: StreamState<'_>,
) -> Result<ReadableState<'s>, ExnThrown> {
    js::class::create_instance_with::<ReadableStateImpl>(scope, |_| ReadableStateImpl {
        holder: Heap::from(holder),
    })
}

/// The `_writableState` view of `holder`'s state.
pub(crate) fn new_writable_view<'s>(
    scope: &'s Scope<'_>,
    holder: StreamState<'_>,
) -> Result<WritableState<'s>, ExnThrown> {
    js::class::create_instance_with::<WritableStateImpl>(scope, |_| WritableStateImpl {
        holder: Heap::from(holder),
    })
}

/// The buffered writes of `side` as Node's `[{ chunk, encoding, callback }]`, with Node's `nop` as
/// the callback of a write without one.
pub(crate) fn buffered_array<'s>(
    scope: &'s Scope<'_>,
    side: WritableSide<'_>,
) -> Result<js::Array<'s>, ExnThrown> {
    let array = js::Array::new(scope, 0)?;
    let count = side.get().buffered.len();
    for index in 0..count {
        let (chunk, encoding, callback) = {
            let part = side.get();
            let Some(entry) = part.buffered.get(index) else {
                break;
            };
            (
                entry.chunk.get(scope),
                entry.encoding.get(scope),
                entry.callback.get(scope),
            )
        };
        let entry = Object::new_plain(scope)?;
        entry.set_property(scope, c"chunk", chunk)?;
        entry.set_property(scope, c"encoding", encoding)?;
        if callback.is_undefined() {
            entry.set_property(scope, c"callback", crate::support::nop(scope)?)?;
        } else {
            entry.set_property(scope, c"callback", callback)?;
        }
        array.push(scope, entry)?;
    }
    Ok(array)
}

/// The readable part a `ReadableState` accessor's `this` views.
fn readable_view<'s>(scope: &'s Scope<'_>, args: &CallbackArgs<'_>) -> Option<ReadableSide<'s>> {
    let view = Object::from_value(scope, args.this())
        .ok()?
        .cast::<ReadableState>()
        .ok()?;
    let holder = view.data().holder.get(scope);
    ReadableSide::of_state(holder)
}

/// The writable part a `WritableState` accessor's `this` views.
fn writable_view<'s>(scope: &'s Scope<'_>, args: &CallbackArgs<'_>) -> Option<WritableSide<'s>> {
    let view = Object::from_value(scope, args.this())
        .ok()?
        .cast::<WritableState>()
        .ok()?;
    let holder = view.data().holder.get(scope);
    WritableSide::of_state(holder)
}

/// The `ReadableState` accessors for flags, Node's `makeBitMapDescriptor`s.
const READABLE_FLAGS: &[(&CStr, R)] = &[
    (c"objectMode", R::OBJECT_MODE),
    (c"ended", R::ENDED),
    (c"endEmitted", R::END_EMITTED),
    (c"reading", R::READING),
    (c"constructed", R::CONSTRUCTED),
    (c"sync", R::SYNC),
    (c"needReadable", R::NEED_READABLE),
    (c"emittedReadable", R::EMITTED_READABLE),
    (c"readableListening", R::READABLE_LISTENING),
    (c"resumeScheduled", R::RESUME_SCHEDULED),
    (c"errorEmitted", R::ERROR_EMITTED),
    (c"emitClose", R::EMIT_CLOSE),
    (c"autoDestroy", R::AUTO_DESTROY),
    (c"destroyed", R::DESTROYED),
    (c"closed", R::CLOSED),
    (c"closeEmitted", R::CLOSE_EMITTED),
    (c"multiAwaitDrain", R::MULTI_AWAIT_DRAIN),
    (c"readingMore", R::READING_MORE),
    (c"dataEmitted", R::DATA_EMITTED),
];

/// The `WritableState` accessors for flags, Node's `makeBitMapDescriptor`s.
const WRITABLE_FLAGS: &[(&CStr, W)] = &[
    (c"objectMode", W::OBJECT_MODE),
    (c"finalCalled", W::FINAL_CALLED),
    (c"needDrain", W::NEED_DRAIN),
    (c"ending", W::ENDING),
    (c"ended", W::ENDED),
    (c"finished", W::FINISHED),
    (c"destroyed", W::DESTROYED),
    (c"decodeStrings", W::DECODE_STRINGS),
    (c"writing", W::WRITING),
    (c"sync", W::SYNC),
    (c"bufferProcessing", W::BUFFER_PROCESSING),
    (c"constructed", W::CONSTRUCTED),
    (c"prefinished", W::PREFINISHED),
    (c"errorEmitted", W::ERROR_EMITTED),
    (c"emitClose", W::EMIT_CLOSE),
    (c"autoDestroy", W::AUTO_DESTROY),
    (c"closed", W::CLOSED),
    (c"closeEmitted", W::CLOSE_EMITTED),
    (c"allBuffers", W::ALL_BUFFERS),
    (c"allNoop", W::ALL_NOOP),
];

/// The flag accessor payload of `bits`.
fn bits_payload(bits: u32) -> Value {
    js::value::from_i32(bits as i32)
}

/// The flags of a flag accessor's payload.
fn payload_bits(payload: HandleValue<'_>) -> u32 {
    payload.to_int32() as u32
}

fn readable_flag_get(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = readable_view(scope, &args) else {
        return Ok(js::value::undefined());
    };
    let flags = R::from_bits_retain(payload_bits(payload));
    Ok(js::value::from_bool(side.has(flags)))
}

fn readable_flag_set(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(side) = readable_view(scope, &args) {
        let flags = R::from_bits_retain(payload_bits(payload));
        side.set(flags, js::conversion::to_boolean(args.get(0)));
    }
    Ok(js::value::undefined())
}

fn writable_flag_get(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = writable_view(scope, &args) else {
        return Ok(js::value::undefined());
    };
    let flags = W::from_bits_retain(payload_bits(payload));
    Ok(js::value::from_bool(side.has(flags)))
}

fn writable_flag_set(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(side) = writable_view(scope, &args) {
        let flags = W::from_bits_retain(payload_bits(payload));
        side.set(flags, js::conversion::to_boolean(args.get(0)));
    }
    Ok(js::value::undefined())
}

/// The `ReadableState` accessors for the fields that aren't flags.
#[derive(Clone, Copy)]
enum ReadableField {
    HighWaterMark,
    Buffer,
    BufferIndex,
    Length,
    Pipes,
    AwaitDrainWriters,
    Errored,
    DefaultEncoding,
    Decoder,
    Encoding,
    Flowing,
    PipesCount,
    Paused,
    Readable,
}

const READABLE_FIELDS: &[(&CStr, ReadableField, bool)] = &[
    (c"highWaterMark", ReadableField::HighWaterMark, true),
    (c"buffer", ReadableField::Buffer, true),
    (c"bufferIndex", ReadableField::BufferIndex, true),
    (c"length", ReadableField::Length, true),
    (c"pipes", ReadableField::Pipes, true),
    (c"awaitDrainWriters", ReadableField::AwaitDrainWriters, true),
    (c"errored", ReadableField::Errored, true),
    (c"defaultEncoding", ReadableField::DefaultEncoding, true),
    (c"decoder", ReadableField::Decoder, true),
    (c"encoding", ReadableField::Encoding, true),
    (c"flowing", ReadableField::Flowing, true),
    (c"pipesCount", ReadableField::PipesCount, false),
    (c"paused", ReadableField::Paused, true),
    (c"readable", ReadableField::Readable, true),
];

impl ReadableField {
    fn from_payload(payload: HandleValue<'_>) -> Self {
        let index = payload.to_int32() as usize;
        READABLE_FIELDS[index].1
    }
}

/// `ToNumber(value)`.
fn to_number(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, ExnThrown> {
    use js::conversion::FromJSVal;
    f64::from_jsval(scope, value, ()).map_err(|_| ExnThrown)
}

/// `value`, or `null` for `None`.
fn or_null(value: Option<HandleValue<'_>>) -> Value {
    value.map_or(js::value::null(), |value| value.get())
}

/// `value`, if it is truthy, as Node's setters store it.
fn truthy(value: HandleValue<'_>) -> Option<HandleValue<'_>> {
    js::conversion::to_boolean(value).then_some(value)
}

fn readable_field_get(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = readable_view(scope, &args) else {
        return Ok(js::value::undefined());
    };
    Ok(match ReadableField::from_payload(payload) {
        ReadableField::HighWaterMark => js::value::from_f64(side.get().high_water_mark),
        ReadableField::Buffer => side.buffer_array(scope)?.as_value(),
        ReadableField::BufferIndex => js::value::from_f64(side.get().buffer_index),
        ReadableField::Length => js::value::from_f64(side.get().length),
        ReadableField::Pipes => js::Array::with_contents(scope, &side.pipes(scope))?.as_value(),
        ReadableField::AwaitDrainWriters => side.await_drain_writers_value(scope)?.get(),
        ReadableField::Errored => or_null(side.errored(scope)),
        ReadableField::DefaultEncoding => side.default_encoding(scope)?.get(),
        ReadableField::Decoder => side
            .decoder(scope)
            .map_or(js::value::null(), |d| d.as_value()),
        ReadableField::Encoding => or_null(side.encoding(scope)),
        ReadableField::Flowing => match side.get().flowing {
            Some(flowing) => js::value::from_bool(flowing),
            None => js::value::null(),
        },
        ReadableField::PipesCount => js::value::from_f64(side.get().pipes.len() as f64),
        ReadableField::Paused => js::value::from_bool(side.get().paused == Some(true)),
        ReadableField::Readable => side.get().readable.get(scope).get(),
    })
}

fn readable_field_set(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = readable_view(scope, &args) else {
        return Ok(js::value::undefined());
    };
    let value = args.get(0);
    match ReadableField::from_payload(payload) {
        ReadableField::HighWaterMark => side.get_mut().high_water_mark = to_number(scope, value)?,
        ReadableField::Buffer => {
            if let Some(array) = as_array(scope, value) {
                let chunks = array.to_vec(scope)?;
                side.get_mut().buffer =
                    chunks.iter().map(|chunk| Heap::from(chunk.get())).collect();
            }
        }
        ReadableField::BufferIndex => side.get_mut().buffer_index = to_number(scope, value)?,
        ReadableField::Length => side.get_mut().length = to_number(scope, value)?,
        ReadableField::Pipes => {
            if let Some(array) = as_array(scope, value) {
                let pipes = array.to_vec(scope)?;
                side.get_mut().pipes = pipes.iter().map(|pipe| Heap::from(pipe.get())).collect();
            }
        }
        ReadableField::AwaitDrainWriters => {
            let set = Object::from_value(scope, *value)
                .ok()
                .and_then(|object| object.cast::<js::Set>().ok());
            let writers = match set {
                Some(set) => DrainWriters::Set(set.values_vec(scope)?),
                None => DrainWriters::Value(value),
            };
            side.set_await_drain_writers(writers);
        }
        ReadableField::Errored => side.set_errored(value),
        ReadableField::DefaultEncoding => {
            side.get_mut().default_encoding =
                default_encoding_field(scope, value)?.map(|value| Heap::from(value.get()));
        }
        ReadableField::Decoder => {
            side.get_mut().decoder = Object::from_value(scope, *value).ok().map(Heap::from);
        }
        ReadableField::Encoding => {
            side.get_mut().encoding = truthy(value).map(|value| Heap::from(value.get()))
        }
        ReadableField::Flowing => {
            side.get_mut().flowing =
                (!value.is_null_or_undefined()).then(|| js::conversion::to_boolean(value));
        }
        ReadableField::PipesCount => {}
        ReadableField::Paused => side.get_mut().paused = Some(js::conversion::to_boolean(value)),
        ReadableField::Readable => side.get().readable.set(value.get()),
    }
    Ok(js::value::undefined())
}

/// The `WritableState` accessors for the fields that aren't flags.
#[derive(Clone, Copy)]
enum WritableField {
    HighWaterMark,
    Length,
    Corked,
    Onwrite,
    Writelen,
    BufferedIndex,
    Pendingcb,
    Errored,
    Writable,
    DefaultEncoding,
    Writecb,
    AfterWriteTickInfo,
    Buffered,
    BufferedRequestCount,
}

const WRITABLE_FIELDS: &[(&CStr, WritableField, bool)] = &[
    (c"highWaterMark", WritableField::HighWaterMark, true),
    (c"length", WritableField::Length, true),
    (c"corked", WritableField::Corked, true),
    (c"onwrite", WritableField::Onwrite, true),
    (c"writelen", WritableField::Writelen, true),
    (c"bufferedIndex", WritableField::BufferedIndex, true),
    (c"pendingcb", WritableField::Pendingcb, true),
    (c"errored", WritableField::Errored, true),
    (c"writable", WritableField::Writable, true),
    (c"defaultEncoding", WritableField::DefaultEncoding, true),
    (c"writecb", WritableField::Writecb, true),
    (
        c"afterWriteTickInfo",
        WritableField::AfterWriteTickInfo,
        true,
    ),
    (c"buffered", WritableField::Buffered, false),
    (
        c"bufferedRequestCount",
        WritableField::BufferedRequestCount,
        false,
    ),
];

impl WritableField {
    fn from_payload(payload: HandleValue<'_>) -> Self {
        let index = payload.to_int32() as usize;
        WRITABLE_FIELDS[index].1
    }
}

/// `afterWriteTickInfo` as Node's object: `{ count, cb, stream, state }`, or `null`.
fn after_write_tick_info_object(
    scope: &Scope<'_>,
    side: WritableSide<'_>,
) -> Result<Value, ExnThrown> {
    let Some(info) = side.get().after_write_tick_info.get(scope) else {
        return Ok(js::value::null());
    };
    let (stream, callback, count) = {
        let data = info.data();
        (data.stream.get(scope), data.callback.get(scope), data.count)
    };
    let view = side.0.data().writable_view.get(scope);
    let object = Object::new_plain(scope)?;
    object.set_property(scope, c"count", count)?;
    object.set_property(scope, c"cb", callback)?;
    object.set_property(scope, c"stream", stream)?;
    match view {
        Some(view) => object.set_property(scope, c"state", view)?,
        None => object.set_property(scope, c"state", HandleValue::undefined())?,
    }
    object.to_jsval_raw_throwing(scope)
}

fn writable_field_get(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = writable_view(scope, &args) else {
        return Ok(js::value::undefined());
    };
    Ok(match WritableField::from_payload(payload) {
        WritableField::HighWaterMark => js::value::from_f64(side.get().high_water_mark),
        WritableField::Length => js::value::from_f64(side.get().length),
        WritableField::Corked => js::value::from_f64(side.get().corked),
        WritableField::Onwrite => side.get().onwrite.get(scope).as_value(),
        WritableField::Writelen => js::value::from_f64(side.get().writelen),
        WritableField::BufferedIndex => js::value::from_i32(0),
        WritableField::Pendingcb => js::value::from_f64(side.get().pendingcb),
        WritableField::Errored => or_null(side.errored(scope)),
        WritableField::Writable => match side.get().writable {
            Some(writable) => js::value::from_bool(writable),
            None => js::value::undefined(),
        },
        WritableField::DefaultEncoding => side.default_encoding(scope)?.get(),
        WritableField::Writecb => {
            let callback = side
                .get()
                .writecb
                .as_ref()
                .map(|callback| callback.get(scope));
            match callback {
                Some(callback) => callback.get(),
                None => crate::support::nop(scope)?.as_value(),
            }
        }
        WritableField::AfterWriteTickInfo => after_write_tick_info_object(scope, side)?,
        WritableField::Buffered => buffered_array(scope, side)?.as_value(),
        WritableField::BufferedRequestCount => {
            js::value::from_f64(side.get().buffered.len() as f64)
        }
    })
}

fn writable_field_set(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(side) = writable_view(scope, &args) else {
        return Ok(js::value::undefined());
    };
    let value = args.get(0);
    match WritableField::from_payload(payload) {
        WritableField::HighWaterMark => side.get_mut().high_water_mark = to_number(scope, value)?,
        WritableField::Length => side.get_mut().length = to_number(scope, value)?,
        WritableField::Corked => side.get_mut().corked = to_number(scope, value)?,
        WritableField::Onwrite => {
            if let Ok(function) = Object::from_value(scope, *value) {
                side.get().onwrite.set(function);
            }
        }
        WritableField::Writelen => side.get_mut().writelen = to_number(scope, value)?,
        WritableField::BufferedIndex => {}
        WritableField::Pendingcb => side.get_mut().pendingcb = to_number(scope, value)?,
        WritableField::Errored => side.set_errored(value),
        WritableField::Writable => {
            side.get_mut().writable =
                (!value.is_null_or_undefined()).then(|| js::conversion::to_boolean(value));
        }
        WritableField::DefaultEncoding => {
            side.get_mut().default_encoding =
                default_encoding_field(scope, value)?.map(|value| Heap::from(value.get()));
        }
        WritableField::Writecb => {
            side.get_mut().writecb = truthy(value).map(|value| Heap::from(value.get()))
        }
        WritableField::AfterWriteTickInfo => {
            if !js::conversion::to_boolean(value) {
                side.get_mut().after_write_tick_info = None;
            }
        }
        WritableField::Buffered | WritableField::BufferedRequestCount => {}
    }
    Ok(js::value::undefined())
}

/// Define a non-enumerable accessor on `proto` whose getter and setter get `payload`.
fn define_accessor(
    scope: &Scope<'_>,
    proto: Object<'_>,
    name: &'static CStr,
    getter: js::function::Callback,
    setter: Option<js::function::Callback>,
    payload: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let get = Function::new_callback(scope, name, 0, getter, payload)?;
    let set = setter
        .map(|setter| Function::new_callback(scope, name, 1, setter, payload))
        .transpose()?;
    proto.define_accessor(scope, name, Some(*get), set.map(|set| *set), 0)
}

/// Define the accessors of the `ReadableState` and `WritableState` prototypes.
fn define_view_accessors(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let readable = js::class::get_prototype_object_for::<ReadableStateImpl>(scope)
        .expect("ReadableState is registered");
    for (name, flags) in READABLE_FLAGS {
        let payload = scope.root_value(bits_payload(flags.bits()));
        define_accessor(
            scope,
            readable,
            name,
            readable_flag_get,
            Some(readable_flag_set),
            payload,
        )?;
    }
    for (index, (name, _, settable)) in READABLE_FIELDS.iter().enumerate() {
        let setter = settable.then_some(readable_field_set as js::function::Callback);
        let payload = scope.root_value(js::value::from_i32(index as i32));
        define_accessor(scope, readable, name, readable_field_get, setter, payload)?;
    }
    let writable = js::class::get_prototype_object_for::<WritableStateImpl>(scope)
        .expect("WritableState is registered");
    for (name, flags) in WRITABLE_FLAGS {
        let payload = scope.root_value(bits_payload(flags.bits()));
        define_accessor(
            scope,
            writable,
            name,
            writable_flag_get,
            Some(writable_flag_set),
            payload,
        )?;
    }
    for (index, (name, _, settable)) in WRITABLE_FIELDS.iter().enumerate() {
        let setter = settable.then_some(writable_field_set as js::function::Callback);
        let payload = scope.root_value(js::value::from_i32(index as i32));
        define_accessor(scope, writable, name, writable_field_get, setter, payload)?;
    }
    Ok(())
}

/// Register the classes of the state for `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) {
    StreamState::add_to_global(scope, global);
    AfterWriteTickInfo::add_to_global(scope, global);
    ReadableState::add_to_global(scope, global);
    WritableState::add_to_global(scope, global);
    define_view_accessors(scope).expect("defining the state accessors succeeds");
}
