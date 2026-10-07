// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `IncomingMessage`: a request a listening server parses from its socket, a served request, read
//! from the body the host received, or a client response, which a client request parses from its
//! socket or reads from the `platform::http` response body.
//!
//! A message's state is an [`IncomingState`], which extends the state of the stream with where
//! its body comes from, and its header lines, from which `headers`, `trailers`,
//! `headersDistinct` and `trailersDistinct` are built when they are first read.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyInit, PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::http::IncomingBody;
use web_globals::signals::abort_controller::{AbortController, AbortControllerImpl};
use web_streams::readable::default_reader::DefaultReaderImpl;

use crate::buffer::new_buffer;
use crate::errors::Thrown;
use crate::stream::state::{ReadableSide, StreamState, StreamStateImpl, R};
use crate::support::{spawn_cancellable, until_cancelled, Canceller};

/// The headers of which Node.js keeps only the first value.
const SINGLE_VALUED: [&str; 18] = [
    "age",
    "authorization",
    "content-length",
    "content-type",
    "etag",
    "expires",
    "from",
    "host",
    "if-modified-since",
    "if-unmodified-since",
    "last-modified",
    "location",
    "max-forwards",
    "proxy-authorization",
    "referer",
    "retry-after",
    "server",
    "user-agent",
];

/// Where a message's body comes from.
#[derive(Default)]
pub(crate) enum Source {
    /// Nowhere: the message has no body.
    #[default]
    None,
    /// The socket the message is parsed from, which pushes the body.
    Wire,
    /// A body the host received, which a read in progress takes, and the URL its errors name.
    Host {
        body: Option<IncomingBody>,
        url: Option<String>,
    },
    /// The body stream of a served request that was read before, through `reader`.
    Web,
}

/// The state of an `IncomingMessage`.
#[core_runtime::jsclass(hidden, extends = StreamState)]
pub(crate) struct IncomingState {
    parent: StreamStateImpl,
    #[no_trace]
    pub(super) source: Source,
    /// The reader of the body stream, for [`Source::Web`].
    reader: Option<Heap<DefaultReaderImpl>>,
    /// The cancellation of the body read in progress.
    #[no_trace]
    read_cancel: Option<Canceller>,
    /// The connection of [`Source::Wire`] for a request a listening server parses.
    wire: Option<Heap<super::connection::ConnectionStateImpl>>,
    /// Whether the parser drops the rest of the message's body: Node's `kSkipPendingData`.
    #[no_trace]
    pub(super) skip_pending: bool,
    /// The `close` listener `signal` added to the socket, and the socket.
    signal_listener: Option<(Heap<js::object::Object>, Heap<js::object::Object>)>,
    /// The error and the callback of `_destroy`, while it waits for the socket to close.
    destroying: Option<(Heap<Value>, Heap<Value>)>,
    /// The header lines, which `rawHeaders` holds too.
    #[no_trace]
    pub(super) raw_headers: std::rc::Rc<Vec<(String, String)>>,
    /// The trailer lines, which `rawTrailers` holds too.
    #[no_trace]
    raw_trailers: std::rc::Rc<Vec<(String, String)>>,
    /// Whether duplicates of the single-valued headers are joined with `, `.
    #[no_trace]
    join_duplicates: bool,
    headers: Option<Heap<Value>>,
    trailers: Option<Heap<Value>>,
    headers_distinct: Option<Heap<Value>>,
    trailers_distinct: Option<Heap<Value>>,
    /// The controller of `signal`, once it was read.
    abort_controller: Option<Heap<AbortControllerImpl>>,
}

#[core_runtime::jsmethods]
impl IncomingState {}

impl IncomingStateImpl {
    /// The value of the accessor whose index in [`LINE_ACCESSORS`] is `index`, if it was set or
    /// built.
    fn line_value(&mut self, index: usize) -> &mut Option<Heap<Value>> {
        match index {
            0 => &mut self.headers,
            1 => &mut self.trailers,
            2 => &mut self.headers_distinct,
            _ => &mut self.trailers_distinct,
        }
    }
}

/// The state of `message`, or `ERR_INVALID_THIS`.
pub(super) fn state_of<'s>(
    scope: &'s Scope<'_>,
    message: Object<'s>,
) -> Result<IncomingState<'s>, ExnThrown> {
    incoming_message_class::state_of(scope, message)
}

/// The state of `message`, if it is an `IncomingMessage`.
pub(super) fn try_state_of<'s>(
    scope: &'s Scope<'_>,
    message: Object<'s>,
) -> Result<Option<IncomingState<'s>>, ExnThrown> {
    incoming_message_class::try_state_of(scope, message)
}

/// The value Node.js's `headers` has for the header lowercased as `lower` in `lines`: the first
/// value of a single-valued header, unless `join` joins its duplicates, the values of `cookie`
/// joined with `; `, and the values of other headers joined with `, `. `set-cookie` values are
/// joined with `, ` here, where `headers` keeps them in an array.
fn joined_value(lines: &[(String, String)], lower: &str, join: bool) -> Option<String> {
    let mut found = lines
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(lower))
        .map(|(_, value)| value.as_str());
    let first = found.next()?;
    if !join && SINGLE_VALUED.contains(&lower) {
        return Some(first.to_string());
    }
    let separator = if lower == "cookie" { "; " } else { ", " };
    Some(found.fold(first.to_string(), |mut joined, value| {
        joined.push_str(separator);
        joined.push_str(value);
        joined
    }))
}

/// The value `req.headers[name]` has, from the header lines of `req` if it is an
/// `IncomingMessage`, and from its `headers` otherwise.
pub(super) fn header_text(
    scope: &Scope<'_>,
    req: Object<'_>,
    name: &str,
) -> Result<Option<String>, ExnThrown> {
    if let Some(state) = incoming_message_class::try_state_of(scope, req)? {
        let data = state.data();
        return Ok(joined_value(&data.raw_headers, name, data.join_duplicates));
    }
    let headers = req.get_property(scope, c"headers")?;
    let Ok(headers) = Object::from_value(scope, *headers) else {
        return Ok(None);
    };
    let key = name.to_jsval_throwing(scope)?;
    let value = crate::http::get_by_key(scope, headers, key)?;
    Ok(crate::support::rust_string(scope, value))
}

/// Set the header lines of `message` to `lines`, and its `rawHeaders` to them, or its trailer
/// lines and `rawTrailers` if `trailers`.
pub(super) fn set_lines(
    scope: &Scope<'_>,
    message: Object<'_>,
    lines: Vec<(String, String)>,
    trailers: bool,
) -> Result<(), ExnThrown> {
    let raw = store_lines(scope, &state_of(scope, message)?, lines, trailers)?;
    let name = if trailers {
        c"rawTrailers"
    } else {
        c"rawHeaders"
    };
    message.set_property(scope, name, *raw)
}

/// Store `lines` as the header lines of `state`, or its trailer lines if `trailers`, and return
/// them as a `rawHeaders` array.
fn store_lines<'s>(
    scope: &'s Scope<'_>,
    state: &IncomingState<'_>,
    lines: Vec<(String, String)>,
    trailers: bool,
) -> Result<js::Array<'s>, ExnThrown> {
    let mut raw = Vec::with_capacity(lines.len() * 2);
    for (name, value) in &lines {
        raw.push(js::JSString::from_str(scope, name)?.to_jsval_throwing(scope)?);
        raw.push(js::JSString::from_str(scope, value)?.to_jsval_throwing(scope)?);
    }
    let raw = js::Array::with_contents(scope, &raw)?;
    {
        let mut data = state.data_mut();
        if trailers {
            data.raw_trailers = std::rc::Rc::new(lines);
            data.trailers = None;
            data.trailers_distinct = None;
        } else {
            data.raw_headers = std::rc::Rc::new(lines);
            data.headers = None;
            data.headers_distinct = None;
        }
    }
    Ok(raw)
}

/// Set whether duplicates of the single-valued headers of `message` are joined, and its
/// `joinDuplicateHeaders`.
pub(super) fn set_join_duplicates(
    scope: &Scope<'_>,
    message: Object<'_>,
    join: bool,
) -> Result<(), ExnThrown> {
    state_of(scope, message)?.data_mut().join_duplicates = join;
    message.set_property(scope, c"joinDuplicateHeaders", join)
}

/// Make the socket `message` is parsed from its body source: the socket of the connection
/// `connection` of a listening server, or the `socket` of a client response for `None`.
pub(super) fn set_wire_source(
    scope: &Scope<'_>,
    message: Object<'_>,
    connection: Option<super::connection::ConnectionState<'_>>,
) -> Result<(), ExnThrown> {
    let state = state_of(scope, message)?;
    let mut data = state.data_mut();
    data.source = Source::Wire;
    data.wire = connection.map(Heap::from);
    Ok(())
}

/// Remove the `close` listener the `signal` of `message` added to its socket, as Node.js's
/// `kDetachAbortSignal` does.
pub(super) fn detach_abort_signal(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    let listener = match incoming_message_class::try_state_of(scope, message)? {
        Some(state) => {
            let mut data = state.data_mut();
            let listener = data
                .signal_listener
                .as_ref()
                .map(|(socket, listener)| (socket.get(scope), listener.get(scope)));
            data.signal_listener = None;
            listener
        }
        None => None,
    };
    if let Some((socket, listener)) = listener {
        crate::events::listen(scope, socket, c"removeListener", c"close", listener)?;
    }
    Ok(())
}

/// Resume `socket` unless it is paused for its server or isn't readable, as Node.js's
/// `readStart` does.
pub(super) fn read_start_socket(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    if js::conversion::to_boolean(socket.get_property(scope, c"_paused")?) {
        return Ok(());
    }
    let readable = match ReadableSide::try_of(scope, socket)? {
        Some(side) => side.get().is_readable(),
        None => js::conversion::to_boolean(socket.get_property(scope, c"readable")?),
    };
    if readable {
        Function::call_by_name(scope, socket.handle(), c"resume", js::function::EmptyArgs)?;
    }
    Ok(())
}

/// Make `body` the body `message` reads from, whose errors name `url`.
pub(super) fn set_host_source(
    scope: &Scope<'_>,
    message: Object<'_>,
    body: IncomingBody,
    url: Option<String>,
) -> Result<(), ExnThrown> {
    state_of(scope, message)?.data_mut().source = Source::Host {
        body: Some(body),
        url,
    };
    Ok(())
}

/// Make the body stream `stream` of a served request the body `message` reads from.
pub(super) fn set_web_source(
    scope: &Scope<'_>,
    message: Object<'_>,
    stream: web_streams::readable::ReadableStream<'_>,
) -> Result<(), ExnThrown> {
    let reader = web_streams::readable::native_read::acquire_native_reader(scope, &stream)?;
    let state = state_of(scope, message)?;
    let mut data = state.data_mut();
    data.source = Source::Web;
    data.reader = Some(Heap::from(reader));
    Ok(())
}

/// Push `chunk`, a `Buffer`, onto `message`, or end it for `null`.
fn push(scope: &Scope<'_>, message: Object<'_>, chunk: HandleValue<'_>) -> Result<(), ExnThrown> {
    if chunk.is_null() {
        message.set_property(scope, c"complete", true)?;
    }
    Function::call_by_name(scope, message.handle(), c"push", &[chunk])?;
    Ok(())
}

/// The completion of a read of a host body: push the chunk read, or `null` at the end, or
/// destroy the message with the error.
fn host_read_done(
    scope: &Scope<'_>,
    message: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    state_of(scope, message)?.data_mut().read_cancel = None;
    match result {
        Ok(chunk) => push(scope, message, chunk),
        Err(error) => {
            Function::call_by_name(scope, message.handle(), c"destroy", &[error])?;
            Ok(())
        }
    }
}

/// Read the next chunk of the host body of `message`, unless a read is in progress.
fn read_host_body(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &IncomingState<'_>,
) -> Result<(), ExnThrown> {
    let body = match &mut state.data_mut().source {
        Source::Host { body, .. } => body.take(),
        _ => None,
    };
    let Some(mut body) = body else {
        return Ok(());
    };
    let canceller = spawn_cancellable(
        scope,
        message,
        |cancelled| async move {
            let chunk = until_cancelled(cancelled, body.next_chunk()).await?;
            Some((body, chunk))
        },
        |scope, message, (body, chunk)| {
            let state = state_of(scope, message)?;
            let url = match &mut state.data_mut().source {
                Source::Host {
                    body: source, url, ..
                } => {
                    if matches!(chunk, Ok(Some(_))) {
                        *source = Some(body);
                    }
                    url.clone()
                }
                _ => None,
            };
            match chunk {
                Ok(Some(bytes)) => new_buffer(scope, &bytes)?.to_jsval_throwing(scope),
                Ok(None) => Ok(HandleValue::null()),
                Err(error) => Err(super::client::transport_error(
                    scope,
                    &error.0,
                    url.as_deref(),
                )),
            }
        },
        host_read_done,
    )?;
    state.data_mut().read_cancel = Some(canceller);
    Ok(())
}

/// The read steps of the body stream of a served request, whose payload is the message.
const WEB_READ_STEPS: web_streams::readable::native_read::NativeReadSteps =
    web_streams::readable::native_read::NativeReadSteps {
        chunk: web_chunk,
        close: web_close,
        error: web_error,
    };

/// Push the bytes of `chunk`, a chunk of the body stream of a served request, onto `message`.
fn web_chunk(
    scope: &Scope<'_>,
    message: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let bytes = Object::from_value(scope, *chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
        .map(|view| view.copy_bytes(scope))
        .transpose()?
        .unwrap_or_default();
    let buffer = new_buffer(scope, &bytes)?.to_jsval_throwing(scope)?;
    push(scope, message, buffer)
}

/// End `message` at the end of the body stream of a served request.
fn web_close(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    push(scope, message, HandleValue::null())
}

/// Destroy `message` with the error of the body stream of a served request.
fn web_error(
    scope: &Scope<'_>,
    message: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    Function::call_by_name(scope, message.handle(), c"destroy", &[error])?;
    Ok(())
}

/// Build the value of `headers` or `trailers` from `lines`, as Node.js's `_addHeaderLine`
/// does: duplicates of single-valued headers are dropped unless `join`, `set-cookie` values are
/// collected in an array, `cookie` values are joined with `; `, and other values with `, `.
fn headers_object<'s>(
    scope: &'s Scope<'_>,
    lines: &[(String, String)],
    join: bool,
) -> Result<Object<'s>, ExnThrown> {
    let mut joined: Vec<(String, Vec<&str>)> = Vec::new();
    for (name, value) in lines {
        let lower = name.to_ascii_lowercase();
        match joined.iter_mut().find(|(name, _)| *name == lower) {
            Some((_, values))
                if join || lower == "set-cookie" || !SINGLE_VALUED.contains(&lower.as_str()) =>
            {
                values.push(value)
            }
            Some(_) => {}
            None => joined.push((lower, vec![value])),
        }
    }
    let headers = Object::new_plain(scope)?;
    for (name, values) in joined {
        let value = if name == "set-cookie" {
            let values = values
                .iter()
                .map(|value| js::JSString::from_str(scope, value)?.to_jsval_throwing(scope))
                .collect::<Result<Vec<_>, _>>()?;
            js::Array::with_contents(scope, &values)?.to_jsval_throwing(scope)?
        } else {
            let separator = if name == "cookie" { "; " } else { ", " };
            js::JSString::from_str(scope, &values.join(separator))?.to_jsval_throwing(scope)?
        };
        let key = js::JSString::from_str(scope, &name)?.to_jsval_throwing(scope)?;
        crate::http::set_by_key(scope, headers, key, value)?;
    }
    Ok(headers)
}

/// Build the value of `headersDistinct` or `trailersDistinct` from `lines`, without a
/// prototype: each lowercase name with the array of its values.
fn distinct_object<'s>(
    scope: &'s Scope<'_>,
    lines: &[(String, String)],
) -> Result<Object<'s>, ExnThrown> {
    let mut grouped: Vec<(String, Vec<&str>)> = Vec::new();
    for (name, value) in lines {
        let lower = name.to_ascii_lowercase();
        match grouped.iter_mut().find(|(name, _)| *name == lower) {
            Some((_, values)) => values.push(value),
            None => grouped.push((lower, vec![value])),
        }
    }
    let distinct = Object::new_with_null_prototype(scope)?;
    for (name, values) in grouped {
        let values = values
            .iter()
            .map(|value| js::JSString::from_str(scope, value)?.to_jsval_throwing(scope))
            .collect::<Result<Vec<_>, _>>()?;
        let values = js::Array::with_contents(scope, &values)?.to_jsval_throwing(scope)?;
        let key = js::JSString::from_str(scope, &name)?.to_jsval_throwing(scope)?;
        crate::http::set_by_key(scope, distinct, key, values)?;
    }
    Ok(distinct)
}

/// The names of the accessors [`get_lines`] and [`set_lines_value`] implement, in the order of
/// their payloads.
const LINE_ACCESSORS: [&std::ffi::CStr; 4] = [
    c"headers",
    c"trailers",
    c"headersDistinct",
    c"trailersDistinct",
];

/// The getter of the accessor whose index in [`LINE_ACCESSORS`] is the payload: the value set,
/// or the object built from the header or trailer lines.
fn get_lines(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let Some(state) = incoming_message_class::try_state_of(scope, this)? else {
        return Ok(js::value::undefined());
    };
    let index = payload.to_number() as usize;
    let stored = state
        .data_mut()
        .line_value(index)
        .as_ref()
        .map(|value| value.get(scope));
    if let Some(stored) = stored.filter(|stored| js::conversion::to_boolean(*stored)) {
        return Ok(stored.get());
    }
    let (lines, join) = {
        let data = state.data();
        let lines = if index.is_multiple_of(2) {
            std::rc::Rc::clone(&data.raw_headers)
        } else {
            std::rc::Rc::clone(&data.raw_trailers)
        };
        (lines, data.join_duplicates)
    };
    let built = if index < 2 {
        headers_object(scope, &lines, join)?
    } else {
        distinct_object(scope, &lines)?
    };
    *state.data_mut().line_value(index) = Some(Heap::from(built.as_value()));
    Ok(built.as_value())
}

/// The setter of the accessor whose index in [`LINE_ACCESSORS`] is the payload.
fn set_lines_value(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    if let Some(state) = incoming_message_class::try_state_of(scope, this)? {
        let value = args.get(0);
        *state.data_mut().line_value(payload.to_number() as usize) = Some(Heap::from(value.get()));
    }
    Ok(js::value::undefined())
}

/// The getter of `connection`: `socket`.
fn get_connection(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    Ok(this.get_property(scope, c"socket")?.get())
}

/// The setter of `connection`: set `socket`.
fn set_connection(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    this.set_property(scope, c"socket", args.get(0))?;
    Ok(js::value::undefined())
}

/// Abort the signal of `message`, if `signal` was read.
fn abort_signal(scope: &Scope<'_>, state: &IncomingState<'_>) -> Result<(), ExnThrown> {
    let controller = state.data().abort_controller.get(scope);
    if let Some(controller) = controller {
        controller.abort(
            scope,
            HandleValue::undefined(),
            web_globals::events::algorithms::ScriptStackState::NonEmpty,
        )?;
    }
    Ok(())
}

/// The `close` listener the signal of a message adds to its socket, whose payload is the
/// message: abort the signal unless the message is complete.
fn on_socket_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = js::function::cast_payload::<js::Object>(scope, payload);
    if !js::conversion::to_boolean(message.get_property(scope, c"complete")?) {
        abort_signal(scope, &state_of(scope, message)?)?;
    }
    Ok(js::value::undefined())
}

/// The getter of `signal`: an `AbortSignal` that aborts if the message is destroyed before it
/// ended, or its socket closes before it is complete.
fn get_signal(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let state = state_of(scope, this)?;
    let controller = state.data().abort_controller.get(scope);
    if let Some(controller) = controller {
        return Ok(controller.signal(scope).as_value());
    }
    let controller = AbortController::new(scope)?;
    state.data_mut().abort_controller = Some(Heap::from(controller));
    let (destroyed, ended) = match ReadableSide::try_of(scope, this)? {
        Some(side) => (side.has(R::DESTROYED), side.has(R::END_EMITTED)),
        None => (false, false),
    };
    let complete = js::conversion::to_boolean(this.get_property(scope, c"complete")?);
    if destroyed && (!ended || !complete) {
        abort_signal(scope, &state)?;
    } else if let Ok(socket) = Object::from_value(scope, *this.get_property(scope, c"socket")?) {
        let destroyed = js::conversion::to_boolean(socket.get_property(scope, c"destroyed")?);
        if destroyed {
            abort_signal(scope, &state)?;
        } else {
            let listener = Function::new_callback(scope, c"", 0, on_socket_close, this)?;
            crate::events::listen(scope, socket, c"once", c"close", *listener)?;
            state.data_mut().signal_listener = Some((Heap::from(socket), Heap::from(*listener)));
        }
    }
    Ok(controller.signal(scope).as_value())
}

/// Initialize `message` as the `IncomingMessage` constructor does for `socket`, creating its
/// properties as `init` creates them, with the fields of `head`.
///
/// # Safety
///
/// With [`PropertyInit::Define`], `message` is an instance [`new_message`] created that nothing
/// else references.
unsafe fn init_message<'w>(
    scope: &'w Scope<'_>,
    message: Object<'w>,
    socket: HandleValue<'_>,
    head: MessageHead<'_>,
    init: PropertyInit,
) -> Result<(), Thrown> {
    let socket_object = Object::from_value(scope, *socket).ok();
    // The high water mark of a socket of this crate is read from its state.
    let socket_mark = match (init, socket_object) {
        (PropertyInit::Define, Some(socket)) => {
            ReadableSide::try_of(scope, socket)?.map(|side| side.high_water_mark())
        }
        _ => None,
    };
    let options = match socket_object {
        Some(socket) if socket_mark.is_none() => {
            let options = Object::new_plain(scope)?;
            let mark = socket.get_property(scope, c"readableHighWaterMark")?;
            options.set_property(scope, c"highWaterMark", mark)?;
            Some(options.to_jsval_throwing(scope)?)
        }
        _ => None,
    };
    // SAFETY: the caller guarantees the requirements of `Define`. Only the writer's own reads run
    // JavaScript that can reach `message`, until it is flushed.
    let mut properties = unsafe { message.property_writer(scope, &MESSAGE_PROPERTIES, init) }?;
    match init {
        PropertyInit::Define => {
            crate::stream::init_new_readable(scope, message, &mut properties, options, socket_mark)?
        }
        PropertyInit::Assign => crate::stream::readable_class::init(scope, message, options)?,
    }
    ReadableSide::of(scope, message)?.set(R::READING_MORE, true);
    let state = state_of(scope, message)?;
    let raw_headers = match head.headers {
        Some(lines) => store_lines(scope, &state, lines, false)?,
        None => js::Array::new(scope, 0)?,
    };
    let join = head.join_duplicates.unwrap_or(false);
    state.data_mut().join_duplicates = join;
    let null = HandleValue::null();
    properties.continue_with(scope, &MESSAGE_PROPERTIES)?;
    properties.write(scope, c"socket", socket)?;
    match head.version {
        Some((major, minor)) => {
            properties.write(scope, c"httpVersionMajor", major)?;
            properties.write(scope, c"httpVersionMinor", minor)?;
            let version = if (major, minor) == (1, 1) {
                js::class::get_or_init_atom(scope, c"1.1")?
            } else {
                format!("{major}.{minor}")
                    .as_str()
                    .to_jsval_throwing(scope)?
            };
            properties.write(scope, c"httpVersion", version)?;
        }
        None => {
            properties.write(scope, c"httpVersionMajor", null)?;
            properties.write(scope, c"httpVersionMinor", null)?;
            properties.write(scope, c"httpVersion", null)?;
        }
    }
    properties.write(scope, c"complete", false)?;
    properties.write(scope, c"rawHeaders", *raw_headers)?;
    properties.write(scope, c"rawTrailers", *js::Array::new(scope, 0)?)?;
    properties.write(scope, c"joinDuplicateHeaders", join)?;
    properties.write(scope, c"aborted", false)?;
    match head.upgrade {
        Some(upgrade) => properties.write(scope, c"upgrade", upgrade)?,
        None => properties.write(scope, c"upgrade", null)?,
    }
    properties.write(scope, c"url", head.url.unwrap_or(""))?;
    match head.method {
        Some(method) => properties.write(scope, c"method", method)?,
        None => properties.write(scope, c"method", null)?,
    }
    match head.status {
        Some((code, text)) => {
            properties.write(scope, c"statusCode", f64::from(code))?;
            properties.write(scope, c"statusMessage", text)?;
        }
        None => {
            properties.write(scope, c"statusCode", null)?;
            properties.write(scope, c"statusMessage", null)?;
        }
    }
    properties.write(scope, c"client", socket)?;
    properties.write(scope, c"_consuming", false)?;
    properties.write(scope, c"_dumped", false)?;
    Ok(properties.finish(scope)?)
}

#[core_runtime::jsfunction_class(
    name = "IncomingMessage",
    extends = "crate::stream::readable_class",
    state = "IncomingState",
    invalid_this = "crate::errors::invalid_this",
    templates = "crate::stream::READABLE_PROPERTIES, crate::events::EMITTER_PROPERTIES, MESSAGE_PROPERTIES"
)]
pub mod incoming_message_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpincomingmessage>: a message from `socket`,
    /// buffering as much as the socket does.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let socket = socket.unwrap_or(HandleValue::undefined());
        // SAFETY: `Assign` has no requirements.
        unsafe {
            init_message(
                scope,
                this,
                socket,
                MessageHead::default(),
                PropertyInit::Assign,
            )
        }
    }

    /// Read the next chunk from the body source: a host body or a body stream, or the socket the
    /// message is parsed from, which this resumes unless its server paused it. A message without
    /// a body source ends.
    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if !js::conversion::to_boolean(this.get_property(scope, c"_consuming")?) {
            ReadableSide::of(scope, this)?.set(R::READING_MORE, false);
            this.set_property(scope, c"_consuming", true)?;
        }
        let state = state_of(scope, this)?;
        let (kind, connection, reader) = {
            let data = state.data();
            let kind = match &data.source {
                Source::None => 0,
                Source::Wire => 1,
                Source::Host { .. } => 2,
                Source::Web => 3,
            };
            (kind, data.wire.get(scope), data.reader.get(scope))
        };
        match kind {
            1 => match connection {
                Some(connection) => super::super::connection::read_start(scope, connection),
                None => match Object::from_value(scope, *this.get_property(scope, c"socket")?) {
                    Ok(socket) => read_start_socket(scope, socket),
                    Err(_) => Ok(()),
                },
            },
            2 => read_host_body(scope, this, &state),
            3 => match reader {
                Some(reader) => web_streams::readable::native_read::native_reader_read(
                    scope,
                    reader,
                    WEB_READ_STEPS,
                    this,
                ),
                None => Ok(()),
            },
            _ => push(scope, this, HandleValue::null()),
        }
    }

    /// <https://nodejs.org/api/http.html#messagesettimeoutmsecs-callback>: sets the timeout of
    /// the socket. `callback` becomes a `timeout` listener.
    #[method(length = 2)]
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        msecs: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        if let Some(callback) = callback.filter(|callback| js::conversion::to_boolean(*callback)) {
            let timeout = js::class::get_or_init_atom(scope, c"timeout")?;
            Function::call_by_name(scope, this.handle(), c"on", &[timeout, callback])?;
        }
        if let Ok(socket) = Object::from_value(scope, *this.get_property(scope, c"socket")?) {
            if crate::support::has_method(scope, socket, c"setTimeout")? {
                let msecs = msecs.unwrap_or(HandleValue::undefined());
                Function::call_by_name(scope, socket.handle(), c"setTimeout", &[msecs])?;
            }
        }
        Ok(this)
    }

    /// Set `aborted` and emit `aborted` if the message didn't end, then call `callback` with
    /// `error` in the tick queue. The error is passed on only if the message has an `error`
    /// listener. An aborted message destroys its socket with `error`, unless the socket was
    /// destroyed. A client response calls `callback` once its socket finished then, with the
    /// socket's error if it has one.
    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        state.data_mut().read_cancel = None;
        let ended = ReadableSide::of(scope, this)?.has(R::END_EMITTED);
        if !ended || !js::conversion::to_boolean(this.get_property(scope, c"complete")?) {
            this.set_property(scope, c"aborted", true)?;
            crate::events::emit(scope, this, c"aborted", &[])?;
            abort_signal(scope, &state)?;
        }
        let aborted = js::conversion::to_boolean(this.get_property(scope, c"aborted")?);
        if aborted {
            let (wire, connection) = {
                let data = state.data();
                (matches!(data.source, Source::Wire), data.wire.get(scope))
            };
            if let Some(connection) = connection {
                let socket = super::super::connection::socket_of(scope, connection);
                if !js::conversion::to_boolean(socket.get_property(scope, c"destroyed")?) {
                    Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
                }
            } else if wire {
                let socket = this.get_property(scope, c"socket")?;
                if let Ok(socket) = Object::from_value(scope, *socket) {
                    if !js::conversion::to_boolean(socket.get_property(scope, c"destroyed")?) {
                        Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
                        state.data_mut().destroying =
                            Some((Heap::from(error.get()), Heap::from(callback.get())));
                        let options = crate::stream::EosOptions::default();
                        let done = crate::stream::EosCallback::Native(
                            socket_finished,
                            this.to_jsval_throwing(scope)?,
                        );
                        crate::stream::eos_native(scope, socket, options, done)
                            .map_err(|thrown| thrown.throw(scope))?;
                        return Ok(());
                    }
                }
            }
        }
        js::jobs::queue_native_tick(scope, call_destroy_callback, this, &[error, callback])
    }

    /// Define `connection`, an alias of `socket`, `headers`, `trailers`, `headersDistinct`,
    /// `trailersDistinct`, and `signal`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"connection", 0, get_connection, ())?;
        let setter = Function::new_callback(scope, c"connection", 1, set_connection, ())?;
        proto.define_accessor(scope, c"connection", Some(*getter), Some(*setter), 0)?;
        for (index, name) in LINE_ACCESSORS.iter().enumerate() {
            let payload = index as f64;
            let getter = Function::new_callback(scope, name, 0, get_lines, payload)?;
            let setter = Function::new_callback(scope, name, 1, set_lines_value, payload)?;
            proto.define_accessor(scope, name, Some(*getter), Some(*setter), 0)?;
        }
        let getter = Function::new_callback(scope, c"signal", 0, get_signal, ())?;
        proto.define_accessor(scope, c"signal", Some(*getter), None, 0)
    }

    /// Read the rest of the message and drop it, unless that already started.
    #[method(name = "_dump")]
    pub fn dump(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if js::conversion::to_boolean(this.get_property(scope, c"_dumped")?) {
            return Ok(());
        }
        this.set_property(scope, c"_dumped", true)?;
        Function::call_by_name(
            scope,
            this.handle(),
            c"removeAllListeners",
            &[js::class::get_or_init_atom(scope, c"data")?],
        )?;
        Function::call_by_name(scope, this.handle(), c"resume", js::function::EmptyArgs)?;
        Ok(())
    }
}

/// The `finished` callback of the socket a client response destroyed, whose payload is the
/// response: call the callback of `_destroy` in the tick queue, with the socket's error unless it
/// closed prematurely, or with the error `_destroy` was given.
fn socket_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = js::function::cast_payload::<js::Object>(scope, payload);
    let destroying = {
        let state = state_of(scope, message)?;
        let mut data = state.data_mut();
        let destroying = data
            .destroying
            .as_ref()
            .map(|(error, callback)| (error.get(scope), callback.get(scope)));
        data.destroying = None;
        destroying
    };
    let Some((error, callback)) = destroying else {
        return Ok(js::value::undefined());
    };
    let finished = args.get(0);
    let premature = match Object::from_value(scope, *finished) {
        Ok(finished) => crate::support::is_str(
            scope,
            finished.get_property(scope, c"code")?,
            c"ERR_STREAM_PREMATURE_CLOSE",
        )?,
        Err(_) => false,
    };
    let error = if js::conversion::to_boolean(finished) && !premature {
        finished
    } else {
        error
    };
    js::jobs::queue_native_tick(scope, call_destroy_callback, message, &[error, callback])?;
    Ok(js::value::undefined())
}

/// The tick callback of `_destroy`, whose payload is the message and whose arguments are the
/// error and the callback: call the callback with the error if the message has an `error`
/// listener, and without it otherwise.
fn call_destroy_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = js::function::cast_payload::<js::Object>(scope, payload);
    let error = js::class::get_or_init_atom(scope, c"error")?;
    let listeners = crate::events::listener_count(scope, message, error)?;
    let callback = args.get(1);
    if listeners > 0.0 {
        Function::call(scope, HandleValue::undefined(), callback, &[args.get(0)])?;
    } else {
        Function::call(
            scope,
            HandleValue::undefined(),
            callback,
            js::function::EmptyArgs,
        )?;
    }
    Ok(js::value::undefined())
}

/// The own properties the `IncomingMessage` constructor creates after those of `Readable`, in
/// Node's order.
pub(crate) static MESSAGE_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"socket"),
    TemplateKey::Name(c"httpVersionMajor"),
    TemplateKey::Name(c"httpVersionMinor"),
    TemplateKey::Name(c"httpVersion"),
    TemplateKey::Name(c"complete"),
    TemplateKey::Name(c"rawHeaders"),
    TemplateKey::Name(c"rawTrailers"),
    TemplateKey::Name(c"joinDuplicateHeaders"),
    TemplateKey::Name(c"aborted"),
    TemplateKey::Name(c"upgrade"),
    TemplateKey::Name(c"url"),
    TemplateKey::Name(c"method"),
    TemplateKey::Name(c"statusCode"),
    TemplateKey::Name(c"statusMessage"),
    TemplateKey::Name(c"client"),
    TemplateKey::Name(c"_consuming"),
    TemplateKey::Name(c"_dumped"),
]);

/// The head of a message that [`new_message`] creates. A field that is `None` keeps the value the
/// constructor gives it.
#[derive(Default)]
pub(super) struct MessageHead<'a> {
    /// `httpVersionMajor` and `httpVersionMinor`, from which `httpVersion` is made.
    pub version: Option<(u32, u32)>,
    pub method: Option<&'a str>,
    pub url: Option<&'a str>,
    pub upgrade: Option<bool>,
    /// `statusCode` and `statusMessage`.
    pub status: Option<(u16, &'a str)>,
    /// `joinDuplicateHeaders`, which is also stored in the state.
    pub join_duplicates: Option<bool>,
    /// The header lines, stored in the state and as `rawHeaders`.
    pub headers: Option<Vec<(String, String)>>,
}

/// A new `IncomingMessage` from `socket` with `head`, as `new IncomingMessage(socket)` creates it
/// with the module's own constructor and the assignments of `head`'s fields that follow it.
pub(super) fn new_message<'s>(
    scope: &'s Scope<'_>,
    socket: HandleValue<'_>,
    head: MessageHead<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let message = incoming_message_class::new_instance(scope)?;
    // SAFETY: `message` was just created, and nothing else references it.
    unsafe { init_message(scope, message, socket, head, PropertyInit::Define) }
        .map_err(|thrown| thrown.throw(scope))?;
    Ok(message)
}
