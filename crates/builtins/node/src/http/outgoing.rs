// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `OutgoingMessage` and its subclass `ServerResponse`, which responds to a served request with a
//! web `Response`, or writes HTTP/1.1 to a socket (see [`super::wire`]).
//!
//! A message's state is an [`OutgoingState`]: its headers, the header block it stored, its
//! socket, the writes it queues until it has one, and how it sends its body, its [`Transport`].
//! The headers keep the values they were given, so `getHeader` returns the same values.

use std::collections::VecDeque;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyInit, PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};
use web_streams::readable::default_controller::ReadableStreamDefaultControllerImpl;
use web_streams::readable::ReadableStreamDefaultController;

use super::connection::ConnectionStateImpl;
use super::{has_invalid_header_char, status_text, validate_name, validate_value};
use crate::errors::{validate_string, ErrorKind, NodeError, Thrown};
use crate::support::{array_elements, chunk_bytes, next_tick_call, Canceller};

/// The number of bytes a response body stream queues before `write` returns `false`.
pub(super) const HIGH_WATER_MARK: f64 = 16384.0;

/// A header of an outgoing message: its lowercase name, its name as set, and its value as given.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(super) struct Header {
    #[no_trace]
    pub(super) lower: String,
    #[no_trace]
    pub(super) name: String,
    pub(super) value: Heap<Value>,
}

/// Bytes a message queues until it has a socket it owns, and their callback: Node's `outputData`.
#[js::must_root]
#[derive(core_runtime::Traceable)]
pub(super) struct Queued {
    #[no_trace]
    pub(super) data: Vec<u8>,
    pub(super) callback: Option<Heap<Value>>,
}

/// The state of a response to a served request.
#[js::must_root]
#[derive(core_runtime::Traceable, Default)]
pub(crate) struct Served {
    /// The promise `respondWith` got, until a web `Response` settles it.
    pub(super) respond: Option<Heap<js::promise::Promise>>,
    /// The controller of the body stream, while the body is streamed.
    body: Option<Heap<ReadableStreamDefaultControllerImpl>>,
    /// The bytes enqueued on the body stream since its reader last asked for more.
    #[no_trace]
    queued: f64,
    /// The interest the streamed body holds in its request's event loop.
    #[no_trace]
    interest: Option<core_runtime::event_loop::InterestHandle>,
}

impl Served {
    /// The state of a served response whose web `Response` resolves `respond`.
    pub(super) fn new(respond: js::Promise<'_>) -> Self {
        Served {
            respond: Some(Heap::from(respond)),
            ..Default::default()
        }
    }
}

/// The state of a client request.
#[js::must_root]
#[derive(core_runtime::Traceable, Default)]
pub(crate) struct ClientPart {
    /// The URL the request is sent to, or the empty string if its host can't be in a URL.
    #[no_trace]
    pub(super) url: String,
    /// The body written so far.
    #[no_trace]
    pub(super) body: Vec<u8>,
    /// The ID of the timer `setTimeout` started.
    #[no_trace]
    pub(super) timer: Option<u64>,
    /// The cancellation of the send in progress.
    #[no_trace]
    pub(super) cancel: Option<Canceller>,
    /// The name of the connection, which keys the agent's free sockets.
    #[no_trace]
    pub(super) socket_name: String,
    pub(super) agent: Option<Heap<js::object::Object>>,
    /// The value of `path`.
    pub(super) path: Option<Heap<Value>>,
    /// The response, once it arrived.
    pub(super) response: Option<Heap<js::object::Object>>,
}

/// How a message sends its body.
#[js::must_root]
#[derive(core_runtime::Traceable, Default)]
pub(crate) enum Transport {
    /// As HTTP/1.1 written to its socket.
    #[default]
    Wire,
    /// As the web `Response` to a served request.
    Served(Served),
    /// As the body of a request `platform::http` sends.
    Client(ClientPart),
}

/// The state of an `OutgoingMessage`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct OutgoingState {
    /// The headers set, in the order they were first set: Node's `kOutHeaders`, `None` until a
    /// header is set.
    pub(super) headers: Option<Vec<Header>>,
    /// The headers `writeHead` was given without a header set before, which it sends without
    /// setting.
    pub(super) head_headers: Vec<Header>,
    /// The stored header block, until it is sent.
    #[no_trace]
    pub(super) header: Option<String>,
    /// Whether the header block was stored, which `headersSent` returns.
    #[no_trace]
    pub(super) header_stored: bool,
    /// The number of `cork` calls not undone.
    #[no_trace]
    pub(super) corked: f64,
    /// The error the message was destroyed with.
    errored: Option<Heap<Value>>,
    /// The body bytes written, which `strictContentLength` checks.
    #[no_trace]
    pub(super) bytes_written: f64,
    #[no_trace]
    pub(super) high_water_mark: f64,
    /// Whether writing a body the status or method doesn't allow throws.
    #[no_trace]
    pub(super) reject_body_writes: bool,
    /// Whether `drain` is emitted once the message can be written to again.
    #[no_trace]
    pub(super) need_drain: bool,
    /// Whether `end` was called, which `finished` and `writableEnded` return.
    #[no_trace]
    pub(super) ended: bool,
    /// Whether `finish` was emitted.
    #[no_trace]
    pub(super) finish_emitted: bool,
    /// Whether `close` was emitted, which `_closed` and `closed` return.
    #[no_trace]
    pub(super) closed: bool,
    /// Whether the message was destroyed, which `destroyed` returns.
    #[no_trace]
    pub(super) destroyed: bool,
    /// The socket, which `socket` and `connection` return.
    pub(super) socket: Option<Heap<js::object::Object>>,
    /// The `close` listener `assignSocket` added to the socket.
    close_listener: Option<Heap<js::object::Object>>,
    /// The bytes waiting for the socket.
    pub(super) output: VecDeque<Queued>,
    #[no_trace]
    pub(super) output_size: f64,
    /// The request a response responds to.
    pub(super) request: Option<Heap<js::object::Object>>,
    /// The connection of a response a listening server writes.
    pub(super) connection: Option<Heap<ConnectionStateImpl>>,
    pub(super) transport: Transport,
}

#[core_runtime::jsmethods]
impl OutgoingState {}

/// The state of `message`, or `ERR_INVALID_THIS`.
pub(super) fn state_of<'s>(
    scope: &'s Scope<'_>,
    message: Object<'s>,
) -> Result<OutgoingState<'s>, ExnThrown> {
    outgoing_message_class::state_of(scope, message)
}

/// The state of `message`, if it is an `OutgoingMessage`.
pub(super) fn try_state_of<'s>(
    scope: &'s Scope<'_>,
    message: Object<'s>,
) -> Result<Option<OutgoingState<'s>>, ExnThrown> {
    outgoing_message_class::try_state_of(scope, message)
}

/// `ERR_HTTP_HEADERS_SENT` for `action`, such as "set".
fn headers_sent(action: &str) -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_HTTP_HEADERS_SENT",
        format!("Cannot {action} headers after they are sent to the client"),
    )
}

/// The values of the header value `value`: its elements if it is an array, and itself otherwise.
pub(super) fn values<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'s>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    Ok(array_elements(scope, value)?.unwrap_or_else(|| vec![value]))
}

/// `value` as an array, if it is one.
pub(super) fn as_array<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<js::Array<'s>>, ExnThrown> {
    if !js::Array::is_array_value(scope, value)? {
        return Ok(None);
    }
    Ok(Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<js::Array>().ok()))
}

/// The pairs of a headers argument: the `[name, value]` pairs of an array of pairs, the names and
/// values of a flat array of names and values, or the enumerable own properties of an object.
/// `flat_only` reads an array as a flat array even if its first element is an array.
pub(super) fn header_args<'s>(
    scope: &'s Scope<'_>,
    headers: HandleValue<'s>,
    flat_only: bool,
) -> Result<Vec<(HandleValue<'s>, HandleValue<'s>)>, Thrown> {
    if let Some(items) = array_elements(scope, headers)? {
        let pairs_given = !flat_only
            && match items.first() {
                Some(first) => js::Array::is_array_value(scope, *first)?,
                None => false,
            };
        if pairs_given {
            let mut pairs = Vec::with_capacity(items.len());
            for item in &items {
                let pair =
                    Object::from_value(scope, **item).map_err(|e| Thrown::from(e.throw(scope)))?;
                pairs.push((pair.get_element(scope, 0)?, pair.get_element(scope, 1)?));
            }
            return Ok(pairs);
        }
        if items.len() % 2 != 0 {
            return Err(
                NodeError::invalid_arg_value(scope, "headers", headers, "is invalid").into(),
            );
        }
        return Ok(items.chunks(2).map(|pair| (pair[0], pair[1])).collect());
    }
    let Ok(object) = Object::from_value(scope, *headers) else {
        return Ok(Vec::new());
    };
    let mut pairs = Vec::new();
    for key in object.own_enumerable_keys(scope)? {
        pairs.push((key, crate::http::get_by_key(scope, object, key)?));
    }
    Ok(pairs)
}

/// The headers of the message of `state` as (name, value) pairs, where a value can be an array:
/// the headers set in the order they were set, then the headers `writeHead` was given without any
/// header set before.
pub(super) fn header_entries<'s>(
    scope: &'s Scope<'_>,
    state: &OutgoingState<'_>,
) -> Vec<(String, HandleValue<'s>)> {
    let data = state.data();
    data.headers
        .iter()
        .flatten()
        .chain(data.head_headers.iter())
        .map(|header| (header.name.clone(), header.value.get(scope)))
        .collect()
}

/// The headers of the message of `state` as (name, value) pairs, with array values expanded and
/// every value converted to a string.
pub(super) fn header_pairs(
    scope: &Scope<'_>,
    state: &OutgoingState<'_>,
) -> Result<Vec<(String, String)>, ExnThrown> {
    let mut pairs = Vec::new();
    for (name, value) in header_entries(scope, state) {
        for value in values(scope, value)? {
            pairs.push((name.clone(), String::from_jsval_throwing(scope, value, ())?));
        }
    }
    Ok(pairs)
}

/// The index of the header set as `lower` in `headers`.
fn find(headers: &[Header], lower: &str) -> Option<usize> {
    headers.iter().position(|header| header.lower == lower)
}

/// The value of the header `name` of the message of `state`, if it is set.
pub(super) fn header_value<'s>(
    scope: &'s Scope<'_>,
    state: &OutgoingState<'_>,
    name: &str,
) -> Option<HandleValue<'s>> {
    let lower = name.to_ascii_lowercase();
    let data = state.data();
    let headers = data.headers.as_ref()?;
    find(headers, &lower).map(|index| headers[index].value.get(scope))
}

/// Set the header `name`, a valid token, to `value` on the message of `state`.
fn store_header_value(state: &OutgoingState<'_>, name: String, value: HandleValue<'_>) {
    let lower = name.to_ascii_lowercase();
    let mut data = state.data_mut();
    let headers = data.headers.get_or_insert_with(Vec::new);
    match find(headers, &lower) {
        Some(index) => {
            headers[index].name = name;
            headers[index].value.set(value.get());
        }
        None => headers.push(Header {
            lower,
            name,
            value: Heap::from(value.get()),
        }),
    }
}

/// Remove the header `name`, a validated string, from the message `message`. Removing
/// `Connection`, `Content-Length` or `Transfer-Encoding` keeps the header block from adding it,
/// and removing `Date` clears `sendDate`.
fn remove_named(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    name: &str,
) -> Result<(), Thrown> {
    if state.data().header_stored {
        return Err(headers_sent("remove").into());
    }
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "connection" => message.set_property(scope, c"_removedConnection", true)?,
        "content-length" => message.set_property(scope, c"_removedContLen", true)?,
        "transfer-encoding" => message.set_property(scope, c"_removedTE", true)?,
        "date" => message.set_property(scope, c"sendDate", false)?,
        _ => {}
    }
    let mut data = state.data_mut();
    if let Some(headers) = data.headers.as_mut() {
        if let Some(index) = find(headers, &lower) {
            headers.remove(index);
        }
    }
    Ok(())
}

/// Set the headers of `message` from `headers`, as `writeHead` does once a header was set: from
/// an object, a flat array of names and values, or an array of `[name, value]` pairs. A flat
/// array replaces the headers it names, and keeps its own duplicates.
fn apply_headers(
    scope: &Scope<'_>,
    message: Object<'_>,
    headers: HandleValue<'_>,
) -> Result<(), Thrown> {
    if as_array(scope, headers)?.is_some() {
        let pairs = header_args(scope, headers, true)?;
        for (name, _) in &pairs {
            outgoing_message_class::remove_header(scope, message, *name)?;
        }
        for (name, value) in pairs {
            if js::conversion::to_boolean(name) {
                outgoing_message_class::append_header(scope, message, name, value)?;
            }
        }
        return Ok(());
    }
    for (name, value) in header_args(scope, headers, true)? {
        if js::conversion::to_boolean(name) {
            outgoing_message_class::set_header(scope, message, name, value)?;
        }
    }
    Ok(())
}

/// Store `headers` as the headers `writeHead` was given without any header set before, after
/// checking each name and value as `setHeader` does: from an object, a flat array of names and
/// values, or an array of pairs.
fn store_head_headers(
    scope: &Scope<'_>,
    state: &OutgoingState<'_>,
    headers: HandleValue<'_>,
) -> Result<(), Thrown> {
    let mut entries = Vec::new();
    for (name, value) in header_args(scope, headers, false)? {
        let name = validate_name(scope, name)?;
        validate_value(scope, &name, value)?;
        entries.push((name, value));
    }
    state.data_mut().head_headers = entries
        .into_iter()
        .map(|(name, value)| Header {
            lower: name.to_ascii_lowercase(),
            name,
            value: Heap::from(value.get()),
        })
        .collect();
    Ok(())
}

/// Store the header block of `message` through its `_implicitHeader`, unless it was stored.
pub(super) fn implicit_header(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
) -> Result<(), ExnThrown> {
    if !state.data().header_stored {
        Function::call_by_name(
            scope,
            message.handle(),
            c"_implicitHeader",
            js::function::EmptyArgs,
        )?;
    }
    Ok(())
}

/// The tick callback that reports a failed write, whose payload is the message: call the
/// callback with the error, and emit it as `error` unless the message was destroyed.
fn report_write_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = js::function::cast_payload::<js::Object>(scope, payload);
    let error = args.get(0);
    let callback = args.get(1);
    if js::function::is_callable(callback) {
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
    }
    let destroyed = state_of(scope, message)?.data().destroyed;
    if !destroyed {
        crate::events::emit(scope, message, c"error", &[error])?;
    }
    Ok(js::value::undefined())
}

/// Report `error` to `callback` in the tick queue, and as an `error` event of `message` unless it
/// was destroyed, as Node.js's `onError` does.
pub(super) fn on_error(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    error: NodeError,
    callback: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let error = error.to_value(scope)?;
    let callback = callback.unwrap_or(HandleValue::undefined());
    if state.data().destroyed {
        return next_tick_call(scope, callback, &[error]);
    }
    js::jobs::queue_native_tick(scope, report_write_error, message, &[error, callback])
}

/// `ERR_STREAM_WRITE_AFTER_END`.
pub(super) fn write_after_end() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_STREAM_WRITE_AFTER_END",
        "write after end",
    )
}

/// The tick callback whose payload is a message: mark it destroyed and emit `close` on it, unless
/// it already emitted `close`.
pub(super) fn emit_close_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    emit_close(
        scope,
        js::function::cast_payload::<js::Object>(scope, payload),
    )?;
    Ok(js::value::undefined())
}

/// Mark `message` destroyed and emit `close` on it, unless it already emitted `close`.
pub(super) fn emit_close(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    let state = state_of(scope, message)?;
    {
        let mut data = state.data_mut();
        if data.closed {
            return Ok(());
        }
        data.closed = true;
        data.destroyed = true;
    }
    message.set_property(scope, c"_closed", true)?;
    message.set_property(scope, c"destroyed", true)?;
    crate::events::emit(scope, message, c"close", &[])?;
    Ok(())
}

/// The tick callback that finishes a served response or a client request, whose payload is the
/// message: emit `finish`, then, for the response to a served request, emit `close` and read the
/// rest of the request if nothing reads it.
fn finish_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = js::function::cast_payload::<js::Object>(scope, payload);
    let state = state_of(scope, message)?;
    let (served, request) = {
        let mut data = state.data_mut();
        data.finish_emitted = true;
        (
            matches!(data.transport, Transport::Served(_)),
            data.request.get(scope),
        )
    };
    crate::events::emit(scope, message, c"finish", &[])?;
    if let Some(request) = request.filter(|_| served) {
        emit_close(scope, message)?;
        if !js::conversion::to_boolean(request.get_property(scope, c"_consuming")?) {
            Function::call_by_name(scope, request.handle(), c"_dump", js::function::EmptyArgs)?;
        }
    }
    Ok(js::value::undefined())
}

/// Write `bytes` of a message that doesn't write to a socket, as the last chunk if `last`, and
/// return whether more can be written without waiting for `drain`.
fn send(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    bytes: Vec<u8>,
    last: bool,
) -> Result<bool, ExnThrown> {
    let client = matches!(state.data().transport, Transport::Client(_));
    if client {
        return super::client::send(scope, message, state, bytes, last);
    }
    send_served(scope, message, state, bytes, last)
}

/// The own properties the `OutgoingMessage` constructor creates after those of `EventEmitter`, in
/// Node's order.
pub(crate) static OUTGOING_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"outputData"),
    TemplateKey::Name(c"outputSize"),
    TemplateKey::Name(c"writable"),
    TemplateKey::Name(c"destroyed"),
    TemplateKey::Name(c"_last"),
    TemplateKey::Name(c"chunkedEncoding"),
    TemplateKey::Name(c"shouldKeepAlive"),
    TemplateKey::Name(c"maxRequestsOnConnectionReached"),
    TemplateKey::Name(c"_defaultKeepAlive"),
    TemplateKey::Name(c"useChunkedEncodingByDefault"),
    TemplateKey::Name(c"sendDate"),
    TemplateKey::Name(c"_removedConnection"),
    TemplateKey::Name(c"_removedContLen"),
    TemplateKey::Name(c"_removedTE"),
    TemplateKey::Name(c"strictContentLength"),
    TemplateKey::Name(c"_contentLength"),
    TemplateKey::Name(c"_hasBody"),
    TemplateKey::Name(c"_trailer"),
    TemplateKey::Name(c"finished"),
    TemplateKey::Name(c"_headerSent"),
    TemplateKey::Name(c"_closed"),
    TemplateKey::Name(c"_header"),
    TemplateKey::Name(c"_keepAliveTimeout"),
    TemplateKey::Name(c"_onPendingData"),
]);

/// The values of the properties of an `OutgoingMessage` that differ between the kinds of
/// messages. The default holds the values `new OutgoingMessage()` gives them.
pub(super) struct OutgoingFields<'a> {
    pub should_keep_alive: bool,
    pub use_chunked_encoding_by_default: bool,
    pub send_date: bool,
    pub has_body: bool,
    /// `_keepAliveTimeout`, or `0` for `None`.
    pub keep_alive_timeout: Option<HandleValue<'a>>,
    /// `_onPendingData`, or a function that does nothing for `None`.
    pub on_pending_data: Option<Object<'a>>,
}

impl Default for OutgoingFields<'_> {
    fn default() -> Self {
        OutgoingFields {
            should_keep_alive: true,
            use_chunked_encoding_by_default: true,
            send_date: false,
            has_body: true,
            keep_alive_timeout: None,
            on_pending_data: None,
        }
    }
}

/// Initialize `message` as an emitter, and create the own properties of an `OutgoingMessage` with
/// `fields`, as `init` creates properties. With [`PropertyInit::Define`], `message` is an
/// instance this crate created that has no own properties yet.
fn init_outgoing(
    scope: &Scope<'_>,
    message: Object<'_>,
    fields: &OutgoingFields<'_>,
    init: PropertyInit,
) -> Result<(), Thrown> {
    match init {
        PropertyInit::Define => crate::events::init_new_emitter(scope, message)?,
        PropertyInit::Assign => crate::stream::stream_class::init(scope, message, None)?,
    }
    // SAFETY: the caller guarantees the requirements of `Define`, and no JavaScript runs before
    // the writer finishes.
    let mut properties = unsafe { message.property_writer(scope, &OUTGOING_PROPERTIES, init) }?;
    properties.write(scope, c"outputData", *js::Array::new(scope, 0)?)?;
    properties.write(scope, c"outputSize", 0)?;
    properties.write(scope, c"writable", true)?;
    properties.write(scope, c"destroyed", false)?;
    properties.write(scope, c"_last", false)?;
    properties.write(scope, c"chunkedEncoding", false)?;
    properties.write(scope, c"shouldKeepAlive", fields.should_keep_alive)?;
    properties.write(scope, c"maxRequestsOnConnectionReached", false)?;
    properties.write(scope, c"_defaultKeepAlive", true)?;
    properties.write(
        scope,
        c"useChunkedEncodingByDefault",
        fields.use_chunked_encoding_by_default,
    )?;
    properties.write(scope, c"sendDate", fields.send_date)?;
    properties.write(scope, c"_removedConnection", false)?;
    properties.write(scope, c"_removedContLen", false)?;
    properties.write(scope, c"_removedTE", false)?;
    properties.write(scope, c"strictContentLength", false)?;
    properties.write(scope, c"_contentLength", HandleValue::null())?;
    properties.write(scope, c"_hasBody", fields.has_body)?;
    properties.write(scope, c"_trailer", js::class::get_or_init_atom(scope, c"")?)?;
    properties.write(scope, c"finished", false)?;
    properties.write(scope, c"_headerSent", false)?;
    properties.write(scope, c"_closed", false)?;
    properties.write(scope, c"_header", HandleValue::null())?;
    match fields.keep_alive_timeout {
        Some(timeout) => properties.write(scope, c"_keepAliveTimeout", timeout)?,
        None => properties.write(scope, c"_keepAliveTimeout", 0)?,
    }
    match fields.on_pending_data {
        Some(function) => properties.write(scope, c"_onPendingData", function)?,
        None => properties.write(
            scope,
            c"_onPendingData",
            crate::support::noop_function(scope)?,
        )?,
    }
    properties.finish(scope)?;
    Ok(())
}

/// Store `options.highWaterMark`, the high water mark without a socket, and
/// `options.rejectNonStandardBodyWrites` in the state of `message`. A `highWaterMark` that isn't
/// a number gives the default high water mark.
fn store_options(
    scope: &Scope<'_>,
    message: Object<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let options = options.and_then(|options| Object::from_value(scope, *options).ok());
    let option = |name| -> Result<HandleValue<'_>, ExnThrown> {
        match options {
            Some(options) => options.get_property(scope, name),
            None => Ok(HandleValue::undefined()),
        }
    };
    let high_water_mark = option(c"highWaterMark")?;
    let high_water_mark = if high_water_mark.is_number() {
        high_water_mark.to_number()
    } else {
        crate::stream::state::default_high_water_mark(false)
    };
    let reject = js::conversion::to_boolean(option(c"rejectNonStandardBodyWrites")?);
    let state = state_of(scope, message)?;
    let mut data = state.data_mut();
    data.high_water_mark = high_water_mark;
    data.reject_body_writes = reject;
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "OutgoingMessage",
    extends = "crate::stream::stream_class",
    state = "OutgoingState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod outgoing_message_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpoutgoingmessage>: `options.highWaterMark` is the
    /// high water mark without a socket, and `options.rejectNonStandardBodyWrites` makes writing
    /// a body the method or status doesn't allow throw.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        init_outgoing(
            scope,
            this,
            &OutgoingFields::default(),
            PropertyInit::Assign,
        )?;
        store_options(scope, this, options)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagesetheadername-value>
    pub fn set_header<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        value: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().header_stored {
            return Err(headers_sent("set").into());
        }
        let name = validate_name(scope, name)?;
        validate_value(scope, &name, value)?;
        store_header_value(&state, name, value);
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagesetheadersheaders>: set each header of
    /// `headers`, a `Headers` or a `Map`, with `setHeader`. The `set-cookie` values are set
    /// together.
    pub fn set_headers<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        headers: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        if state_of(scope, this)?.data().header_stored {
            return Err(headers_sent("set").into());
        }
        let map_like = match Object::from_value(scope, *headers) {
            Ok(object) if !js::Array::is_array(scope, object.handle())? => {
                js::function::is_callable(object.get_property(scope, c"keys")?)
                    && js::function::is_callable(object.get_property(scope, c"get")?)
            }
            _ => false,
        };
        if !map_like {
            return Err(NodeError::invalid_arg_type(
                scope,
                "headers",
                "an instance of Headers or Map",
                headers,
            )
            .into());
        }
        let mut cookies = Vec::new();
        let items: Vec<HandleValue<'_>> = Vec::from_jsval(scope, headers, ())?;
        let mut entries = Vec::with_capacity(items.len());
        for item in items {
            let entry = Object::from_value(scope, *item)?;
            entries.push((entry.get_element(scope, 0)?, entry.get_element(scope, 1)?));
        }
        for (name, value) in entries {
            if name.is_string() && String::from_jsval(scope, name, ())? == "set-cookie" {
                cookies.extend(values(scope, value)?);
                continue;
            }
            Function::call_by_name(scope, this.handle(), c"setHeader", &[name, value])?;
        }
        if !cookies.is_empty() {
            let cookies = js::Array::with_contents(scope, &cookies)?;
            Function::call_by_name(
                scope,
                this.handle(),
                c"setHeader",
                &[
                    js::class::get_or_init_atom(scope, c"set-cookie")?,
                    cookies.to_jsval_throwing(scope)?,
                ],
            )?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageappendheadername-value>
    pub fn append_header<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        value: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().header_stored {
            return Err(headers_sent("append").into());
        }
        let text = validate_name(scope, name)?;
        validate_value(scope, &text, value)?;
        let Some(previous) = header_value(scope, &state, &text) else {
            store_header_value(&state, text, value);
            return Ok(this);
        };
        let existing = match as_array(scope, previous)? {
            Some(array) => array,
            None => {
                let array = js::Array::with_contents(scope, &[previous])?;
                let list = array.to_jsval_throwing(scope)?;
                let lower = text.to_ascii_lowercase();
                let mut data = state.data_mut();
                if let Some(headers) = data.headers.as_mut() {
                    if let Some(index) = find(headers, &lower) {
                        headers[index].value.set(list.get());
                    }
                }
                array
            }
        };
        for item in values(scope, value)? {
            existing.push(scope, item)?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetheadername>
    pub fn get_header<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let name = validate_string(scope, name, "name")?;
        let state = state_of(scope, this)?;
        Ok(header_value(scope, &state, &name).unwrap_or(HandleValue::undefined()))
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetheaders>
    pub fn get_headers<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
    ) -> Result<Object<'s>, ExnThrown> {
        let result = Object::new_with_null_prototype(scope)?;
        let state = state_of(scope, this)?;
        let entries: Vec<(String, HandleValue<'_>)> = {
            let data = state.data();
            data.headers
                .iter()
                .flatten()
                .map(|header| (header.lower.clone(), header.value.get(scope)))
                .collect()
        };
        for (lower, value) in entries {
            let key = lower.as_str().to_jsval_throwing(scope)?;
            crate::http::set_by_key(scope, result, key, value)?;
        }
        Ok(result)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetheadernames>
    pub fn get_header_names<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
    ) -> Result<js::Array<'s>, ExnThrown> {
        let names: Vec<String> = {
            let state = state_of(scope, this)?;
            let data = state.data();
            data.headers
                .iter()
                .flatten()
                .map(|header| header.lower.clone())
                .collect()
        };
        let names = names
            .iter()
            .map(|name| name.as_str().to_jsval_throwing(scope))
            .collect::<Result<Vec<_>, _>>()?;
        js::Array::with_contents(scope, &names)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetrawheadernames>
    pub fn get_raw_header_names<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
    ) -> Result<js::Array<'s>, ExnThrown> {
        let names: Vec<String> = {
            let state = state_of(scope, this)?;
            let data = state.data();
            data.headers
                .iter()
                .flatten()
                .map(|header| header.name.clone())
                .collect()
        };
        let names = names
            .iter()
            .map(|name| name.as_str().to_jsval_throwing(scope))
            .collect::<Result<Vec<_>, _>>()?;
        js::Array::with_contents(scope, &names)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagehasheadername>
    pub fn has_header(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let name = validate_string(scope, name, "name")?;
        let state = state_of(scope, this)?;
        Ok(header_value(scope, &state, &name).is_some())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageremoveheadername>
    pub fn remove_header(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let name = validate_string(scope, name, "name")?;
        let state = state_of(scope, this)?;
        remove_named(scope, this, &state, &name)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageflushheaders>
    pub fn flush_headers(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        if matches!(state.data().transport, Transport::Wire) {
            return super::super::wire::flush_headers(scope, this, &state);
        }
        implicit_header(scope, this, &state)?;
        this.set_property(scope, c"_headerSent", true)?;
        send(scope, this, &state, Vec::new(), false)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagewritechunk-encoding-callback>: `false`
    /// once the body stream of a served response queues more than 16 KiB, or the socket of a
    /// response that writes to one does, and `drain` follows once it has room again.
    #[method(length = 3)]
    pub fn write(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        let (encoding, callback) = match encoding {
            Some(encoding) if js::function::is_callable(encoding) => (None, Some(encoding)),
            _ => (encoding, callback),
        };
        let callback = callback.filter(|callback| js::function::is_callable(*callback));
        let state = state_of(scope, this)?;
        if matches!(state.data().transport, Transport::Wire) {
            let room = super::super::wire::write(
                scope, this, &state, chunk, encoding, callback, false, None,
            )?;
            if !room {
                state.data_mut().need_drain = true;
            }
            return Ok(room);
        }
        if state.data().ended {
            on_error(scope, this, &state, write_after_end(), callback)?;
            return Ok(false);
        }
        let (bytes, _) = chunk_bytes(scope, chunk, encoding)?.ok_or_else(|| {
            NodeError::invalid_arg_type(
                scope,
                "chunk",
                "of type string or an instance of Buffer or Uint8Array",
                chunk,
            )
        })?;
        implicit_header(scope, this, &state)?;
        this.set_property(scope, c"_headerSent", true)?;
        let room = send(scope, this, &state, bytes, false)?;
        if let Some(callback) = callback {
            next_tick_call(scope, callback, &[])?;
        }
        Ok(room)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageendchunk-encoding-callback>
    #[method(length = 3)]
    pub fn end<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        chunk: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let (chunk, encoding, callback) = match (chunk, encoding) {
            (Some(chunk), _) if js::function::is_callable(chunk) => (None, None, Some(chunk)),
            (chunk, Some(encoding)) if js::function::is_callable(encoding) => {
                (chunk, None, Some(encoding))
            }
            (chunk, encoding) => (chunk, encoding, callback),
        };
        let callback = callback.filter(|callback| js::function::is_callable(*callback));
        let chunk = chunk.filter(|chunk| js::conversion::to_boolean(*chunk));
        let state = state_of(scope, this)?;
        if matches!(state.data().transport, Transport::Wire) {
            super::super::wire::end(scope, this, &state, chunk, encoding, callback)?;
            return Ok(this);
        }
        let (ended, finished) = {
            let data = state.data();
            (data.ended, data.finish_emitted)
        };
        if ended {
            if chunk.is_some() {
                on_error(scope, this, &state, write_after_end(), callback)?;
            } else if let Some(callback) = callback {
                if finished {
                    next_tick_call(scope, callback, &[])?;
                } else {
                    crate::events::listen(
                        scope,
                        this,
                        c"once",
                        c"finish",
                        Object::from_value(scope, *callback)?,
                    )?;
                }
            }
            return Ok(this);
        }
        let bytes = match chunk {
            Some(chunk) => {
                chunk_bytes(scope, chunk, encoding)?
                    .ok_or_else(|| {
                        NodeError::invalid_arg_type(
                            scope,
                            "chunk",
                            "of type string or an instance of Buffer or Uint8Array",
                            chunk,
                        )
                    })?
                    .0
            }
            None => Vec::new(),
        };
        implicit_header(scope, this, &state)?;
        if let Some(callback) = callback {
            crate::events::listen(
                scope,
                this,
                c"once",
                c"finish",
                Object::from_value(scope, *callback)?,
            )?;
        }
        state.data_mut().ended = true;
        this.set_property(scope, c"_headerSent", true)?;
        this.set_property(scope, c"finished", true)?;
        send(scope, this, &state, bytes, true)?;
        js::jobs::queue_native_tick(scope, finish_tick, this, &[])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagesettimeoutmsecs-callback>: sets the
    /// timeout of the socket, once a response that writes to a socket has one. `callback`
    /// becomes a `timeout` listener. The timeout has no effect for a served request's response.
    #[method(length = 2)]
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        msecs: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let msecs = msecs.unwrap_or(HandleValue::undefined());
        if let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) {
            crate::events::listen(
                scope,
                this,
                c"on",
                c"timeout",
                Object::from_value(scope, *callback).map_err(|e| e.throw(scope))?,
            )?;
        }
        let state = state_of(scope, this)?;
        let (socket, wire) = {
            let data = state.data();
            (
                data.socket.get(scope),
                matches!(data.transport, Transport::Wire),
            )
        };
        match socket {
            Some(socket) => {
                Function::call_by_name(scope, socket.handle(), c"setTimeout", &[msecs])?;
            }
            None if wire => {
                let set = Function::new_callback(scope, c"", 1, set_socket_timeout, msecs)?;
                crate::events::listen(scope, this, c"once", c"socket", *set)?;
            }
            None => {}
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageaddtrailersheaders>: the trailers a
    /// chunked message sends after its last chunk, from an object or an array of pairs. An
    /// array value is a line per element.
    pub fn add_trailers(
        scope: &Scope<'_>,
        this: Object<'_>,
        headers: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let mut trailer = String::new();
        let headers = Object::from_value_coerce(scope, headers)?;
        let pairs = if js::Array::is_array(scope, headers.handle())? {
            let mut pairs = Vec::new();
            let array = headers.cast::<js::Array>().map_err(|_| ExnThrown)?;
            for item in array.to_vec(scope)? {
                let pair =
                    Object::from_value(scope, *item).map_err(|e| Thrown::from(e.throw(scope)))?;
                pairs.push((pair.get_element(scope, 0)?, pair.get_element(scope, 1)?));
            }
            pairs
        } else {
            let mut pairs = Vec::new();
            for key in headers.own_enumerable_keys(scope)? {
                pairs.push((key, crate::http::get_by_key(scope, headers, key)?));
            }
            pairs
        };
        for (field, value) in pairs {
            super::super::module::validate_header_name(scope, field, Some("Trailer name".into()))?;
            let field = String::from_jsval(scope, field, ())?;
            let lines = match array_elements(scope, value)? {
                Some(items) if items.len() > 1 => items,
                Some(_) => {
                    let array = Object::from_value(scope, *value).map_err(|e| e.throw(scope))?;
                    let joined = Function::call_by_name(
                        scope,
                        array.handle(),
                        c"join",
                        &[js::class::get_or_init_atom(scope, c"; ")?],
                    )?;
                    vec![joined]
                }
                None => vec![value],
            };
            for value in lines {
                let value = String::from_jsval(scope, value, ())?;
                if has_invalid_header_char(&value) {
                    return Err(NodeError::new(
                        ErrorKind::TypeError,
                        "ERR_INVALID_CHAR",
                        format!("Invalid character in trailer content [\"{field}\"]"),
                    )
                    .into());
                }
                trailer.push_str(&format!("{field}: {value}\r\n"));
            }
        }
        this.set_property(scope, c"_trailer", trailer.as_str())?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagedestroyerror>: a response that writes
    /// to a socket destroys the socket with `error`, or emits `close` in the tick queue without
    /// one. A served request's response errors its body stream, or rejects its response, and
    /// emits `close`. A client request cancels its send.
    #[method(length = 1)]
    pub fn destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        error: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        let error = error.unwrap_or(HandleValue::undefined());
        {
            let mut data = state.data_mut();
            if data.destroyed {
                return Ok(this);
            }
            data.destroyed = true;
            data.errored = Some(Heap::from(error.get()));
        }
        this.set_property(scope, c"destroyed", true)?;
        let (wire, socket) = {
            let data = state.data();
            (
                matches!(data.transport, Transport::Wire),
                data.socket.get(scope),
            )
        };
        if wire {
            match socket {
                Some(socket) => {
                    Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
                }
                None => js::jobs::queue_native_tick(scope, emit_close_tick, this, &[])?,
            }
            return Ok(this);
        }
        let client = matches!(state.data().transport, Transport::Client(_));
        if client {
            super::super::client::destroy(scope, this, &state, error)?;
        } else {
            destroy_served(scope, this, &state, error)?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagecork>: count the call in
    /// `writableCorked`, and cork the socket, if the message has one.
    pub fn cork(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        let socket = {
            let mut data = state.data_mut();
            data.corked += 1.0;
            data.socket.get(scope)
        };
        if let Some(socket) = socket {
            if crate::support::has_method(scope, socket, c"cork")? {
                Function::call_by_name(scope, socket.handle(), c"cork", js::function::EmptyArgs)?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageuncork>: undo a `cork` call, and uncork
    /// the socket, if the message has one.
    pub fn uncork(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        let socket = {
            let mut data = state.data_mut();
            data.corked -= 1.0;
            data.socket.get(scope)
        };
        if let Some(socket) = socket {
            if crate::support::has_method(scope, socket, c"uncork")? {
                Function::call_by_name(scope, socket.handle(), c"uncork", js::function::EmptyArgs)?;
            }
        }
        Ok(())
    }

    /// Store the header block before the first write, which a subclass implements. This one
    /// throws `ERR_METHOD_NOT_IMPLEMENTED`, as Node.js's does.
    #[method(name = "_implicitHeader")]
    pub fn implicit_header_impl() -> Result<(), NodeError> {
        Err(NodeError::not_implemented("_implicitHeader()"))
    }

    /// Define the accessors over the message's state.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        for (name, setter) in [(c"socket", true), (c"connection", true)] {
            let getter = Function::new_callback(scope, name, 0, get_socket, ())?;
            let set = Function::new_callback(scope, name, 1, set_socket, ())?;
            proto.define_accessor(scope, name, Some(*getter), setter.then_some(*set), 0)?;
        }
        for (index, name) in OUTGOING_GETTERS.iter().enumerate() {
            let flags = if *name == c"headersSent" {
                js::class_spec::JSPROP_ENUMERATE as u32
            } else {
                0
            };
            let getter = Function::new_callback(scope, name, 0, outgoing_getter, index as f64)?;
            proto.define_accessor(scope, name, Some(*getter), None, flags)?;
        }
        Ok(())
    }
}

/// The names of the getters [`outgoing_getter`] implements, in the order of their payloads.
const OUTGOING_GETTERS: [&std::ffi::CStr; 10] = [
    c"headersSent",
    c"writableEnded",
    c"writableFinished",
    c"writableNeedDrain",
    c"closed",
    c"errored",
    c"writableLength",
    c"writableHighWaterMark",
    c"writableCorked",
    c"writableObjectMode",
];

/// The `writableLength` of `socket`.
pub(super) fn socket_writable_length(
    scope: &Scope<'_>,
    socket: Object<'_>,
) -> Result<f64, ExnThrown> {
    if let Some(side) = crate::stream::state::WritableSide::try_of(scope, socket)? {
        return Ok(side.length());
    }
    let length = socket.get_property(scope, c"writableLength")?;
    Ok(if length.is_number() {
        length.to_number()
    } else {
        0.0
    })
}

/// The getter of the `OutgoingMessage` property whose index in [`OUTGOING_GETTERS`] is the
/// payload: whether the header block was stored, whether `end` was called, whether the message
/// finished and its socket wrote everything, whether `drain` is due, whether the message emitted
/// `close`, the error it was destroyed with, the bytes it and its socket haven't written, the
/// high water mark of its socket or its own, the number of `cork` calls not undone, and `false`.
fn outgoing_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let state = state_of(scope, this)?;
    let index = payload.to_number() as usize;
    let socket = state.data().socket.get(scope);
    let socket_length = |scope: &Scope<'_>| -> Result<f64, ExnThrown> {
        match socket {
            Some(socket) => socket_writable_length(scope, socket),
            None => Ok(0.0),
        }
    };
    Ok(match index {
        0 => js::value::from_bool(state.data().header_stored),
        1 => js::value::from_bool(state.data().ended),
        2 => {
            let (ended, output) = {
                let data = state.data();
                (data.ended, data.output_size)
            };
            js::value::from_bool(ended && output == 0.0 && socket_length(scope)? == 0.0)
        }
        3 => js::value::from_bool(state.data().need_drain),
        4 => js::value::from_bool(state.data().closed),
        5 => match state.data().errored.as_ref() {
            Some(error) => error.get(scope).get(),
            None => js::value::null(),
        },
        6 => js::value::from_f64(state.data().output_size + socket_length(scope)?),
        7 => match socket {
            Some(socket) => {
                let mark = socket.get_property(scope, c"writableHighWaterMark")?;
                if mark.is_number() {
                    mark.get()
                } else {
                    js::value::from_f64(state.data().high_water_mark)
                }
            }
            None => js::value::from_f64(state.data().high_water_mark),
        },
        8 => js::value::from_f64(state.data().corked),
        _ => js::value::from_bool(false),
    })
}

/// The getter of `socket` and `connection`.
fn get_socket(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let socket = match try_state_of(scope, this)? {
        Some(state) => state.data().socket.get(scope),
        None => None,
    };
    Ok(socket.map_or(js::value::null(), |socket| socket.as_value()))
}

/// The setter of `socket` and `connection`: corks the new socket as many times as the message is
/// corked, as Node.js's does.
fn set_socket(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let Some(state) = try_state_of(scope, this)? else {
        return Ok(js::value::undefined());
    };
    let socket = Object::from_value(scope, args.get(0)).ok();
    let corked = {
        let mut data = state.data_mut();
        data.socket = socket.map(Heap::from);
        data.corked
    };
    if let Some(socket) = socket {
        for _ in 0..corked as u32 {
            Function::call_by_name(scope, socket.handle(), c"cork", js::function::EmptyArgs)?;
        }
    }
    Ok(js::value::undefined())
}

/// The `socket` listener of `setTimeout`, whose payload is the timeout: set the timeout of the
/// socket.
fn set_socket_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    Function::call_by_name(scope, socket.handle(), c"setTimeout", &[payload])?;
    Ok(js::value::undefined())
}

/// Whether the response of `state` has no body: a `HEAD` request's, or one with status 1xx, 204
/// or 304.
fn without_body(
    scope: &Scope<'_>,
    status: u16,
    state: &OutgoingState<'_>,
) -> Result<bool, ExnThrown> {
    if (100..200).contains(&status) || status == 204 || status == 304 {
        return Ok(true);
    }
    let Some(request) = state.data().request.get(scope) else {
        return Ok(false);
    };
    let method = request.get_property(scope, c"method")?;
    crate::support::is_str(scope, method, c"HEAD")
}

/// The status of `response`, from its `statusCode` truncated to an integer, and its reason
/// phrase: its `statusMessage`, or the phrase of Node.js's table.
fn status_of(scope: &Scope<'_>, response: Object<'_>) -> Result<(u16, String), ExnThrown> {
    let code = response.get_property(scope, c"statusCode")?;
    let code = super::to_int32(f64::from_jsval_throwing(scope, code, ())?);
    let code = u16::try_from(code).unwrap_or(0);
    let message = response.get_property(scope, c"statusMessage")?;
    let message = match crate::support::rust_string(scope, message) {
        Some(message) => message,
        None => status_text(code).unwrap_or("").to_string(),
    };
    Ok((code, message))
}

/// Settle the response promise of the served response of `state` with a web `Response` with
/// `status`, `message` and its headers, and with `body` as its body. The headers go to the host
/// as they are set, and the host replaces the ones that frame the response.
fn respond(
    scope: &Scope<'_>,
    state: &OutgoingState<'_>,
    status: u16,
    message: String,
    body: Option<Result<Vec<u8>, web_streams::readable::ReadableStream<'_>>>,
) -> Result<(), ExnThrown> {
    let promise = match &mut state.data_mut().transport {
        Transport::Served(served) => served.respond.take_rooted(scope),
        _ => None,
    };
    let Some(promise) = promise else {
        return Ok(());
    };
    let headers = header_pairs(scope, state)?;
    let record = web_fetch::response::ResponseRecord {
        status,
        status_message: message,
        ..Default::default()
    };
    let (body, stream) = match body {
        None => (None, None),
        Some(Ok(bytes)) => {
            let length = bytes.len() as u64;
            let body = web_fetch::algorithms::Body {
                source: web_fetch::algorithms::BodySource::Bytes(bytes::Bytes::from(bytes)),
                length: Some(length),
                source_disturbed: false,
            };
            (Some(body), None)
        }
        Some(Err(stream)) => (Some(web_fetch::algorithms::Body::default()), Some(stream)),
    };
    let web_response = web_fetch::algorithms::create_a_response_object(
        scope,
        record,
        headers,
        web_fetch::headers::Guard::Immutable,
        body,
        stream,
    )?;
    promise.resolve(scope, web_response.to_jsval_throwing(scope)?)
}

/// Write `bytes` of the served response `response`: as its whole body if `last` and nothing was
/// sent before, and otherwise to a stream of the chunks, starting with these. Returns whether
/// the stream has room for more.
fn send_served(
    scope: &Scope<'_>,
    response: Object<'_>,
    state: &OutgoingState<'_>,
    bytes: Vec<u8>,
    last: bool,
) -> Result<bool, ExnThrown> {
    let (controller, pending) = match &state.data().transport {
        Transport::Served(served) => (served.body.get(scope), served.respond.is_some()),
        _ => (None, false),
    };
    if let Some(controller) = controller {
        return enqueue(scope, state, controller, bytes, last);
    }
    if !pending {
        // Already responded to without a stream.
        return Ok(true);
    }
    let (status, message) = status_of(scope, response)?;
    if without_body(scope, status, state)? {
        respond(scope, state, status, message, None)?;
        return Ok(true);
    }
    if last {
        respond(scope, state, status, message, Some(Ok(bytes)))?;
        return Ok(true);
    }
    let pull = web_streams::AlgorithmArg::native(scope, pull_body, response)?;
    let cancel = web_streams::AlgorithmArg::native(scope, cancel_body, response)?;
    let stream = web_streams::readable::ReadableStream::new_native(
        scope,
        HandleValue::undefined(),
        pull,
        cancel,
    )?;
    let controller = stream
        .default_controller(scope)
        .expect("a native stream has a default controller");
    let interest =
        core_runtime::event_loop::with_active_event_loop(|el| el.acquire_interest_handle());
    if let Transport::Served(served) = &mut state.data_mut().transport {
        served.body = Some(Heap::from(controller));
        served.interest = interest;
    }
    respond(scope, state, status, message, Some(Err(stream)))?;
    enqueue(scope, state, controller, bytes, false)
}

/// Enqueue `bytes` on the body stream `controller` of a served response, and close the stream if
/// `last`. Returns whether the stream has room for more.
fn enqueue(
    scope: &Scope<'_>,
    state: &OutgoingState<'_>,
    controller: ReadableStreamDefaultController<'_>,
    bytes: Vec<u8>,
    last: bool,
) -> Result<bool, ExnThrown> {
    let queued = if bytes.is_empty() {
        match &state.data().transport {
            Transport::Served(served) => served.queued,
            _ => 0.0,
        }
    } else {
        let length = bytes.len() as f64;
        let chunk = js::Uint8Array::with_data(scope, &bytes)?;
        controller.enqueue(scope, scope.root_value(chunk.as_value()))?;
        match &mut state.data_mut().transport {
            Transport::Served(served) => {
                served.queued += length;
                served.queued
            }
            _ => 0.0,
        }
    };
    if last {
        controller.close(scope)?;
        release_body(state);
        return Ok(true);
    }
    let room = queued < HIGH_WATER_MARK;
    if !room {
        state.data_mut().need_drain = true;
    }
    Ok(room)
}

/// Release the interest the body stream of a served response holds in its event loop, and forget
/// the stream.
fn release_body(state: &OutgoingState<'_>) {
    if let Transport::Served(served) = &mut state.data_mut().transport {
        served.body = None;
        served.interest = None;
    }
}

/// The `pull` algorithm of the body stream of a served response, whose payload is the response:
/// the reader took everything enqueued, so emit `drain` if a write is waiting for it.
fn pull_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let response = js::function::cast_payload::<js::Object>(scope, payload);
    let state = state_of(scope, response)?;
    let drain = {
        let mut data = state.data_mut();
        if let Transport::Served(served) = &mut data.transport {
            served.queued = 0.0;
        }
        std::mem::take(&mut data.need_drain)
    };
    if drain {
        crate::events::emit(scope, response, c"drain", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `cancel` algorithm of the body stream of a served response, whose payload is the
/// response: the client disconnected before the body was written, so emit `close` on the
/// response and destroy its request with an `ECONNRESET` error.
fn cancel_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let response = js::function::cast_payload::<js::Object>(scope, payload);
    let state = state_of(scope, response)?;
    release_body(&state);
    let (closed, request) = {
        let data = state.data();
        (data.closed, data.request.get(scope))
    };
    if closed {
        return Ok(js::value::undefined());
    }
    emit_close(scope, response)?;
    if let Some(request) = request {
        let error = NodeError::new(ErrorKind::Error, "ECONNRESET", "aborted").to_value(scope)?;
        Function::call_by_name(scope, request.handle(), c"destroy", &[error])?;
    }
    Ok(js::value::undefined())
}

/// Error the body stream of the served response `response`, or reject its response, and emit
/// `close`.
fn destroy_served(
    scope: &Scope<'_>,
    response: Object<'_>,
    state: &OutgoingState<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let (controller, promise) = match &mut state.data_mut().transport {
        Transport::Served(served) => (served.body.get(scope), served.respond.take_rooted(scope)),
        _ => (None, None),
    };
    if let Some(controller) = controller {
        release_body(state);
        controller.error(scope, Some(error))?;
    } else if let Some(promise) = promise {
        let error = if error.is_undefined() {
            NodeError::new(
                ErrorKind::Error,
                "ERR_STREAM_DESTROYED",
                "The response was destroyed",
            )
            .to_value(scope)?
        } else {
            error
        };
        promise.reject(scope, error)?;
    }
    emit_close(scope, response)
}

/// Reject the response promise of the served response `response` with `error`, if it is still
/// pending, so the request doesn't wait for a response forever.
pub(super) fn reject_served(
    scope: &Scope<'_>,
    response: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = state_of(scope, response)?;
    let promise = match &mut state.data_mut().transport {
        Transport::Served(served) => served.respond.take_rooted(scope),
        _ => None,
    };
    if let Some(promise) = promise {
        promise.reject(scope, error)?;
    }
    Ok(())
}

/// The `close` listener a response adds to the socket it is assigned, whose payload is the
/// response: emit `close` on the response.
fn on_socket_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    emit_close(
        scope,
        js::function::cast_payload::<js::Object>(scope, payload),
    )?;
    Ok(js::value::undefined())
}

/// Make `socket` the socket of the response `response`, which writes to it, as Node.js's
/// `assignSocket` does: `socket._httpMessage` becomes the response, and `close` on the socket
/// emits `close` on the response.
pub(super) fn assign_socket(
    scope: &Scope<'_>,
    response: Object<'_>,
    socket: Object<'_>,
) -> Result<(), ExnThrown> {
    let state = state_of(scope, response)?;
    socket.set_property(scope, c"_httpMessage", response)?;
    let listener = Function::new_callback(
        scope,
        c"onServerResponseClose",
        0,
        on_socket_close,
        response,
    )?;
    crate::events::listen(scope, socket, c"on", c"close", *listener)?;
    let connection = {
        let mut data = state.data_mut();
        data.socket = Some(Heap::from(socket));
        data.close_listener = Some(Heap::from(*listener));
        data.connection.get(scope)
    };
    if let Some(connection) = connection {
        connection.data_mut().current = Some(Heap::from(response));
    }
    crate::events::emit(
        scope,
        response,
        c"socket",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    super::wire::flush(scope, response, &state)
}

/// The own properties the `ServerResponse` constructor creates after those of `OutgoingMessage`,
/// and `_maxRequestsPerSocket`, which a listening server's responses get, in Node's order.
pub(crate) static RESPONSE_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"req"),
    TemplateKey::Name(c"_sent100"),
    TemplateKey::Name(c"_expect_continue"),
    TemplateKey::Name(c"_maxRequestsPerSocket"),
]);

/// The values a listening server gives the properties of a response to a request it parsed.
pub(super) struct ConnectionFields<'a> {
    pub keep_alive: bool,
    pub keep_alive_timeout: HandleValue<'a>,
    pub max_requests_per_socket: HandleValue<'a>,
    pub on_pending_data: Option<Object<'a>>,
}

/// A new `ServerResponse` to `req`, as `new ServerResponse(req, options)` creates it with the
/// module's own constructor, and the assignments of a listening server's `connection` fields that
/// follow it. `high_water_mark` and `reject_body_writes` are the options' `highWaterMark` and
/// `rejectNonStandardBodyWrites`.
pub(super) fn new_response<'s>(
    scope: &'s Scope<'_>,
    req: Object<'_>,
    connection: Option<ConnectionFields<'_>>,
    high_water_mark: f64,
    reject_body_writes: bool,
) -> Result<Object<'s>, ExnThrown> {
    let response = server_response_class::new_instance(scope)?;
    init_response(scope, response, Some(req), connection, PropertyInit::Define)
        .map_err(|thrown| thrown.throw(scope))?;
    let state = state_of(scope, response)?;
    let mut data = state.data_mut();
    data.high_water_mark = high_water_mark;
    data.reject_body_writes = reject_body_writes;
    drop(data);
    Ok(response)
}

/// Initialize `response` as the `ServerResponse` constructor does for `req`, with the fields of
/// `connection`, creating its properties as `init` creates them. With [`PropertyInit::Define`],
/// `response` is an instance this crate created that has no own properties yet. The response to
/// a `HEAD` request has no body, and the response to an HTTP/1.0 request closes the connection,
/// unless `connection` keeps it alive, and uses chunked encoding only if the request's `TE`
/// header names it.
fn init_response(
    scope: &Scope<'_>,
    response: Object<'_>,
    req: Option<Object<'_>>,
    connection: Option<ConnectionFields<'_>>,
    init: PropertyInit,
) -> Result<(), Thrown> {
    let mut fields = OutgoingFields {
        send_date: true,
        ..OutgoingFields::default()
    };
    if let Some(req) = req {
        let method = req.get_property(scope, c"method")?;
        if crate::support::is_str(scope, method, c"HEAD")? {
            fields.has_body = false;
        }
        let major = req.get_property(scope, c"httpVersionMajor")?;
        let minor = req.get_property(scope, c"httpVersionMinor")?;
        if (major.is_number() && major.to_number() < 1.0)
            || (minor.is_number() && minor.to_number() < 1.0)
        {
            let te = super::incoming::header_text(scope, req, "te")?.unwrap_or_default();
            fields.use_chunked_encoding_by_default = super::wire::has_word(&te, "chunked");
            fields.should_keep_alive = false;
        }
    }
    if let Some(connection) = &connection {
        fields.should_keep_alive = connection.keep_alive;
        fields.keep_alive_timeout = Some(connection.keep_alive_timeout);
        fields.on_pending_data = connection.on_pending_data;
    }
    init_outgoing(scope, response, &fields, init)?;
    // SAFETY: the caller guarantees the requirements of `Define`, and no JavaScript runs before
    // the writer finishes.
    let mut properties = unsafe { response.property_writer(scope, &RESPONSE_PROPERTIES, init) }?;
    match req {
        Some(req) => {
            state_of(scope, response)?.data_mut().request = Some(Heap::from(req));
            properties.write(scope, c"req", req)?;
        }
        None => properties.write(scope, c"req", HandleValue::undefined())?,
    }
    properties.write(scope, c"_sent100", false)?;
    properties.write(scope, c"_expect_continue", false)?;
    if let Some(connection) = connection {
        properties.write(
            scope,
            c"_maxRequestsPerSocket",
            connection.max_requests_per_socket,
        )?;
    }
    properties.finish(scope)?;
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "ServerResponse",
    extends = "super::outgoing_message_class"
)]
pub mod server_response_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpserverresponse>: the response to `req`. It
    /// writes HTTP/1.1 to the socket `assignSocket` gives it, unless it responds to a served
    /// request. The response to a `HEAD` request has no body, and the response to an HTTP/1.0
    /// request closes the connection and uses chunked encoding only if the request's `TE` header
    /// names it.
    #[constructor(length = 2)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        req: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let req = req.and_then(|req| Object::from_value(scope, *req).ok());
        init_response(scope, this, req, None, PropertyInit::Assign)?;
        store_options(scope, this, options)
    }

    /// Make `socket` the socket the response writes to, emit `socket`, and write what the response
    /// queued. Throws `ERR_HTTP_SOCKET_ASSIGNED` if the socket has a response.
    pub fn assign_socket(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let socket =
            Object::from_value(scope, *socket).map_err(|e| Thrown::from(e.throw(scope)))?;
        if js::conversion::to_boolean(socket.get_property(scope, c"_httpMessage")?) {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_HTTP_SOCKET_ASSIGNED",
                "ServerResponse has an already assigned socket",
            )
            .into());
        }
        Ok(super::assign_socket(scope, this, socket)?)
    }

    /// Stop writing to `socket`.
    pub fn detach_socket(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        let (listener, connection) = {
            let mut data = state.data_mut();
            data.socket = None;
            (
                data.close_listener.take_rooted(scope),
                data.connection.get(scope),
            )
        };
        if let Some(connection) = connection {
            let mut data = connection.data_mut();
            if data
                .current
                .get(scope)
                .is_some_and(|current| current.handle().get() == this.handle().get())
            {
                data.current = None;
            }
        }
        if let Ok(socket) = Object::from_value(scope, *socket) {
            if let Some(listener) = listener {
                crate::events::listen(scope, socket, c"removeListener", c"close", listener)?;
            }
            socket.set_property(scope, c"_httpMessage", js::value::null())?;
        }
        Ok(())
    }

    /// Write what the response queued to its socket, as Node.js's `_flush` does.
    #[method(name = "_flush")]
    pub fn flush_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        super::super::wire::flush(scope, this, &state)
    }

    /// Emit `prefinish`, as Node.js's `_finish` does once all data was handed to the socket.
    #[method(name = "_finish")]
    pub fn finish_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        crate::events::emit(scope, this, c"prefinish", &[])?;
        Ok(())
    }

    /// Write an informational response with `statusCode` and `headers` before the response. It
    /// has no effect for a served request's response.
    #[method(length = 3)]
    pub fn write_information(
        scope: &Scope<'_>,
        this: Object<'_>,
        status: HandleValue<'_>,
        headers: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if !matches!(state.data().transport, Transport::Wire) {
            return Ok(());
        }
        super::super::wire::write_information(scope, this, &state, status, headers, callback)
    }

    /// <https://nodejs.org/api/http.html#responsewriteheadstatuscode-statusmessage-headers>:
    /// `statusCode` is truncated to an integer, as with `statusCode | 0`.
    #[method(length = 3)]
    pub fn write_head<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        status: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
        headers: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().header_stored {
            return Err(headers_sent("write").into());
        }
        let code = super::super::to_int32(f64::from_jsval(scope, status, ())?);
        if !(100..=999).contains(&code) {
            let options = crate::inspect::InspectOptions::current(scope)?;
            let shown = crate::inspect::format(
                scope,
                &[js::class::get_or_init_atom(scope, c"%s")?, status],
                &options,
            )?;
            return Err(NodeError::new(
                ErrorKind::RangeError,
                "ERR_HTTP_INVALID_STATUS_CODE",
                format!("Invalid status code: {shown}"),
            )
            .into());
        }
        let headers = match message {
            Some(message) if message.is_string() => {
                this.set_property(scope, c"statusMessage", message)?;
                headers
            }
            message => {
                if !js::conversion::to_boolean(this.get_property(scope, c"statusMessage")?) {
                    let text = status_text(code as u16).unwrap_or("unknown");
                    this.set_property(scope, c"statusMessage", text)?;
                }
                headers
                    .filter(|headers| !headers.is_null_or_undefined())
                    .or(message)
            }
        };
        this.set_property(scope, c"statusCode", code)?;
        // Headers given with headers set before are set too. Without, they are only sent.
        let progressive = state.data().headers.is_some();
        if let Some(headers) = headers.filter(|_| progressive) {
            super::apply_headers(scope, this, headers)?;
        }
        let status_message = this.get_property(scope, c"statusMessage")?;
        let status_message = String::from_jsval(scope, status_message, ())?;
        if has_invalid_header_char(&status_message) {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_CHAR",
                "Invalid character in statusMessage",
            )
            .into());
        }
        if code == 204 || code == 304 || (100..=199).contains(&code) {
            this.set_property(scope, c"_hasBody", false)?;
        }
        if js::conversion::to_boolean(this.get_property(scope, c"_expect_continue")?)
            && !js::conversion::to_boolean(this.get_property(scope, c"_sent100")?)
        {
            this.set_property(scope, c"shouldKeepAlive", false)?;
        }
        if let Some(headers) =
            headers.filter(|headers| !progressive && js::conversion::to_boolean(*headers))
        {
            super::store_head_headers(scope, &state, headers)?;
        }
        let line = super::super::wire::status_line(code, &status_message);
        if let Err(thrown) = super::super::wire::store_header(scope, this, &state, &line) {
            state.data_mut().head_headers.clear();
            return Err(thrown);
        }
        Ok(this)
    }

    /// Call `this.writeHead(this.statusCode)`, as the first `write` or `end` does.
    #[method(name = "_implicitHeader")]
    pub fn implicit_header(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let status = this.get_property(scope, c"statusCode")?;
        Function::call_by_name(scope, this.handle(), c"writeHead", &[status])?;
        Ok(())
    }

    /// Define `writeHeader`, the same function as `writeHead`, and `statusCode` and
    /// `statusMessage` with their defaults.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
        let write_head = proto.get_property(scope, c"writeHead")?;
        proto.define_property(scope, c"writeHeader", write_head, enumerable)?;
        proto.define_property(scope, c"statusCode", 200, enumerable)?;
        proto.define_property(
            scope,
            c"statusMessage",
            HandleValue::undefined(),
            enumerable,
        )
    }

    /// <https://nodejs.org/api/http.html#responsewritecontinue>: write a `100 Continue` response.
    /// It has no effect for a served request's response, since the host sends informational
    /// responses.
    #[method(length = 1)]
    pub fn write_continue(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if !matches!(state.data().transport, Transport::Wire) {
            return Ok(());
        }
        let status = scope.root_value(js::value::from_f64(100.0));
        super::super::wire::write_information(scope, this, &state, status, None, callback)?;
        this.set_property(scope, c"_sent100", true)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#responsewriteprocessing>: write a `102 Processing`
    /// response. It has no effect for a served request's response.
    #[method(length = 1)]
    pub fn write_processing(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if !matches!(state.data().transport, Transport::Wire) {
            return Ok(());
        }
        let status = scope.root_value(js::value::from_f64(102.0));
        super::super::wire::write_information(scope, this, &state, status, None, callback)
    }

    /// <https://nodejs.org/api/http.html#responsewriteearlyhintshints-callback>: write a `103
    /// Early Hints` response with the `Link` header of `hints.link` and the other hints as
    /// headers. Nothing is written without links. For a served request's response, it has no
    /// effect besides calling `callback`.
    #[method(length = 2)]
    pub fn write_early_hints(
        scope: &Scope<'_>,
        this: Object<'_>,
        hints: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if !matches!(state.data().transport, Transport::Wire) {
            if let Some(callback) = callback {
                next_tick_call(scope, callback, &[])?;
            }
            return Ok(());
        }
        let hints = crate::errors::validate_plain_object(scope, hints, "hints", false)?;
        let link = hints.get_property(scope, c"link")?;
        if link.is_null_or_undefined() {
            return Ok(());
        }
        let link = super::super::wire::link_header_value(scope, link)?;
        if link.is_empty() {
            return Ok(());
        }
        if has_invalid_header_char(&link) {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_CHAR",
                "Invalid character in header content [\"Link\"]",
            )
            .into());
        }
        let headers = Object::new_with_null_prototype(scope)?;
        headers.set_property(scope, c"Link", link.as_str())?;
        for key in hints.own_enumerable_keys(scope)? {
            if !crate::support::is_str(scope, key, c"link")? {
                let value = crate::http::get_by_key(scope, hints, key)?;
                crate::http::set_by_key(scope, headers, key, value)?;
            }
        }
        let status = scope.root_value(js::value::from_f64(103.0));
        let headers = headers.to_jsval_throwing(scope)?;
        super::super::wire::write_information(scope, this, &state, status, Some(headers), callback)
    }
}
