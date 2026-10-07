// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The connections of a listening `http.Server`, ported from Node.js's `lib/_http_server.js`.
//!
//! The server's `connection` listener reads the socket's `data` events into a [`Parser`] held in
//! a `ParserHandle`, which is the socket's `parser`. Each request head becomes an
//! `IncomingMessage` with a `ServerResponse`, emitted as a `request` event. A connection's state
//! is a [`ConnectionState`], the payload of the socket listeners: the server, the parser, the
//! requests waiting for their responses, the responses waiting for the socket, and the bytes the
//! waiting responses queued.

use std::collections::VecDeque;
use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::incoming::{set_join_duplicates, set_lines, MessageHead};
use super::outgoing::{emit_close_tick, ConnectionFields};
use super::parser::{Event, Head, Kind, ParseError, Parser};
use super::server::{server_class, HttpServerState};
use super::wire::socket_destroyed;
use crate::buffer::{buffer_from_vec, new_buffer};
use crate::errors::{ErrorKind, NodeError};

/// The parser of a connection's requests or of a client request's response, which is its
/// socket's `parser`.
#[core_runtime::jsclass(hidden)]
pub struct ParserHandle {
    #[no_trace]
    pub(super) parser: Parser,
    /// The message being parsed.
    pub(super) incoming: Option<Heap<js::object::Object>>,
    /// The socket of a client request's parser.
    pub(super) socket: Option<Heap<js::object::Object>>,
    /// Whether a client response joins duplicates of the single-valued headers.
    #[no_trace]
    pub(super) join_duplicates: bool,
    /// The most header lines a client response keeps, or `None` for all.
    #[no_trace]
    pub(super) max_headers: Option<usize>,
}

#[core_runtime::jsmethods]
impl ParserHandle {
    /// The message being parsed, or `null`.
    #[getter]
    fn incoming<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        match self.data().incoming.get(scope) {
            Some(incoming) => scope.root_value(incoming.as_value()),
            None => HandleValue::null(),
        }
    }

    #[setter]
    fn set_incoming(&self, scope: &Scope<'_>, value: HandleValue<'_>) {
        self.data_mut().incoming = Object::from_value(scope, *value).ok().map(Heap::from);
    }
}

/// The state of a connection of a listening server.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ConnectionState {
    server: Heap<js::object::Object>,
    socket: Heap<js::object::Object>,
    /// The parser, until the connection is upgraded or closed.
    parser: Option<Heap<ParserHandleImpl>>,
    /// The requests waiting for their responses.
    incoming: VecDeque<Heap<js::object::Object>>,
    /// The responses waiting for the socket.
    outgoing: VecDeque<Heap<js::object::Object>>,
    /// The response the socket writes: the socket's `_httpMessage`.
    pub(super) current: Option<Heap<js::object::Object>>,
    /// The socket listeners, which an upgrade removes: `data`, `end`, `close`, `drain`, `error`
    /// and `timeout`.
    listeners: Vec<Heap<js::object::Object>>,
    /// The `_onPendingData` of the responses.
    pending_data: Option<Heap<js::object::Object>>,
    /// The bytes the waiting responses queued.
    #[no_trace]
    outgoing_data: f64,
    #[no_trace]
    requests_count: f64,
    #[no_trace]
    keep_alive_timeout_set: bool,
    /// Whether the server paused the socket, which its `_paused` mirrors.
    #[no_trace]
    paused: bool,
}

#[core_runtime::jsmethods]
impl ConnectionState {}

/// The socket of the connection `state`.
pub(super) fn socket_of<'s>(scope: &'s Scope<'_>, state: ConnectionState<'_>) -> Object<'s> {
    state.data().socket.get(scope)
}

/// The largest request head a server accepts by default, in bytes.
const DEFAULT_MAX_HEADER_SIZE: f64 = 16384.0;

/// The most header lines a request may have by default.
const DEFAULT_MAX_HEADERS: usize = 1000;

/// The milliseconds after which a connection of a closed server that is still between requests
/// is destroyed.
const CLOSING_IDLE_DELAY: u64 = 500;

/// The socket listeners of a connection, in the order of [`ConnectionState::listeners`].
const LISTENERS: [(&std::ffi::CStr, js::function::Callback, u32); 6] = [
    (c"data", socket_on_data, 1),
    (c"end", socket_on_end, 0),
    (c"close", socket_on_close, 0),
    (c"drain", socket_on_drain, 0),
    (c"error", socket_on_error, 1),
    (c"timeout", socket_on_timeout, 0),
];

/// The number `value` is if it is a number, and `default` otherwise.
fn number_or(value: HandleValue<'_>, default: f64) -> f64 {
    if value.is_number() {
        value.to_number()
    } else {
        default
    }
}

/// The `connection` listener of an `http.Server`, whose `this` is the server: handle the
/// requests of the socket, as Node.js's `connectionListenerInternal` does.
pub(super) fn on_connection(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = crate::support::this_object(scope, &args)?;
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    socket.set_property(scope, c"server", server)?;
    let timeout = server.get_property(scope, c"timeout")?;
    if js::conversion::to_boolean(timeout)
        && crate::support::has_method(scope, socket, c"setTimeout")?
    {
        Function::call_by_name(scope, socket.handle(), c"setTimeout", &[timeout])?;
    }
    let max_header_size = number_or(server.get_property(scope, c"maxHeaderSize")?, 0.0);
    let max_header_size = if max_header_size > 0.0 {
        max_header_size
    } else {
        DEFAULT_MAX_HEADER_SIZE
    };
    let max_headers = server.get_property(scope, c"maxHeadersCount")?;
    let max_headers = if max_headers.is_number() {
        let pairs = super::to_int32(max_headers.to_number()).wrapping_shl(1);
        (pairs > 0).then_some(pairs as usize / 2)
    } else {
        Some(DEFAULT_MAX_HEADERS)
    };
    let lenient = super::lenient_parser(
        scope,
        server.get_property(scope, c"httpValidation")?,
        server.get_property(scope, c"insecureHTTPParser")?,
    )?;
    let parser =
        js::class::create_instance_with::<ParserHandleImpl>(scope, |_| ParserHandleImpl {
            parser: Parser::new(
                Kind::Request,
                max_header_size as usize,
                max_headers,
                lenient,
            ),
            ..Default::default()
        })?;
    socket.set_property(scope, c"parser", parser)?;
    let state =
        js::class::create_instance_with::<ConnectionStateImpl>(scope, |_| ConnectionStateImpl {
            server: Heap::from(server),
            socket: Heap::from(socket),
            parser: Some(Heap::from(parser)),
            ..Default::default()
        })?;
    for (event, body, nargs) in LISTENERS {
        let listener = Function::new_callback(scope, c"", nargs, body, state)?;
        crate::events::listen(scope, socket, c"on", event, *listener)?;
        state.data_mut().listeners.push(Heap::from(*listener));
    }
    let pending_data =
        Function::new_callback(scope, c"updateOutgoingData", 1, on_pending_data, state)?;
    state.data_mut().pending_data = Some(Heap::from(*pending_data));
    let set_encoding = crate::support::shared_function(scope, socket_set_encoding, 0)?;
    socket.set_property(scope, c"setEncoding", set_encoding)?;
    socket.set_property(scope, c"_paused", false)?;
    server_class::state_of(scope, server)?
        .data_mut()
        .connections
        .push(Heap::from(state.as_object()));
    Ok(js::value::undefined())
}

/// The `setEncoding` of a socket a server reads requests from: throw
/// `ERR_HTTP_SOCKET_ENCODING`.
fn socket_set_encoding(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Err(NodeError::new(
        ErrorKind::Error,
        "ERR_HTTP_SOCKET_ENCODING",
        "Changing the socket encoding is not allowed per RFC7230 Section 3.",
    )
    .throw(scope))
}

/// The `_onPendingData` of the responses of a connection, whose payload is the connection: add
/// the argument to the bytes the waiting responses queued.
fn on_pending_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConnectionState>(scope, payload);
    update_outgoing_data(scope, state, number_or(args.get(0), 0.0))?;
    Ok(js::value::undefined())
}

/// The error for the parse error `error` of the bytes `raw`, as Node.js's parser and
/// `prepareError` create it.
pub(super) fn parse_error_value<'s>(
    scope: &'s Scope<'_>,
    error: &ParseError,
    raw: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let message = format!("Parse Error: {}", error.reason);
    let value = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &message)?;
    let object = Object::from_value(scope, *value).map_err(|e| e.throw(scope))?;
    object.set_property(scope, c"bytesParsed", error.bytes_parsed as f64)?;
    object.set_property(scope, c"code", error.code)?;
    object.set_property(scope, c"reason", error.reason)?;
    object.set_property(scope, c"rawPacket", raw)?;
    Ok(value)
}

/// The `data` listener of a connection, whose payload is the connection: parse the chunk and
/// handle what it completes, as Node.js's `socketOnData` and the parser callbacks do. Parsing
/// stops once the socket is destroyed or upgraded.
fn socket_on_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConnectionState>(scope, payload);
    let Some(handle) = state.data().parser.get(scope) else {
        return Ok(js::value::undefined());
    };
    let socket = state.data().socket.get(scope);
    let chunk = args.get(0);
    let Some(view) = Object::from_value(scope, chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
    else {
        return Ok(js::value::undefined());
    };
    {
        let mut data = handle.data_mut();
        let parser = &mut data.parser;
        view.with_bytes(|bytes| parser.feed(bytes));
    }
    let tcp = crate::net::socket::is_tcp(scope, socket)?;
    loop {
        let same = state.data().parser.as_ref().is_some_and(|current| {
            current.get(scope).as_object().handle().get() == handle.as_object().handle().get()
        });
        if !same || socket_destroyed(scope, socket)? {
            break;
        }
        let event = handle.data_mut().parser.next();
        match event {
            Ok(None) => break,
            Ok(Some(Event::Head(head))) => {
                if on_head(scope, state, handle, socket, head)? {
                    break;
                }
            }
            Ok(Some(Event::Body(body))) => {
                on_body(scope, state, socket, body)?;
                if tcp {
                    js::jobs::run_jobs(scope);
                }
            }
            Ok(Some(Event::Complete(trailers))) => {
                if !trailers.is_empty() && tcp {
                    js::jobs::run_jobs(scope);
                }
                on_complete(scope, state, handle, trailers)?;
            }
            Err(error) => {
                let error = parse_error_value(scope, &error, chunk)?;
                socket_error(scope, state, error)?;
                break;
            }
        }
    }
    Ok(js::value::undefined())
}

/// Handle the request head `head`, as Node.js's `parserOnHeadersComplete` does. Returns whether
/// the connection was upgraded.
fn on_head(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    handle: ParserHandle<'_>,
    socket: Object<'_>,
    head: Head,
) -> Result<bool, ExnThrown> {
    let server = state.data().server.get(scope);
    let class = server_class::state_of(scope, server)?
        .data()
        .incoming_class
        .get(scope);
    let socket_value = socket.to_jsval_throwing(scope)?;
    let mut head = head;
    let req = match class {
        Some(class) => {
            let req = Function::construct(scope, class, &[socket_value])?;
            req.set_property(scope, c"httpVersionMajor", head.major)?;
            req.set_property(scope, c"httpVersionMinor", head.minor)?;
            let version = format!("{}.{}", head.major, head.minor);
            req.set_property(scope, c"httpVersion", version.as_str())?;
            let join =
                js::conversion::to_boolean(server.get_property(scope, c"joinDuplicateHeaders")?);
            set_join_duplicates(scope, req, join)?;
            req.set_property(scope, c"url", head.url.as_str())?;
            req.set_property(scope, c"upgrade", head.upgrade)?;
            set_lines(scope, req, std::mem::take(&mut head.headers), false)?;
            req.set_property(scope, c"method", head.method.as_str())?;
            req
        }
        None => {
            let join =
                js::conversion::to_boolean(server.get_property(scope, c"joinDuplicateHeaders")?);
            let fields = MessageHead {
                version: Some((u32::from(head.major), u32::from(head.minor))),
                method: Some(&head.method),
                url: Some(&head.url),
                upgrade: Some(head.upgrade),
                join_duplicates: Some(join),
                headers: Some(std::mem::take(&mut head.headers)),
                ..MessageHead::default()
            };
            super::incoming::new_message(scope, socket_value, fields)?
        }
    };
    super::incoming::set_wire_source(scope, req, Some(state))?;
    handle.data_mut().incoming = Some(Heap::from(req));
    if !on_incoming(scope, state, socket, req, &head.method, head.keep_alive)? {
        return Ok(false);
    }
    upgrade(scope, state, socket, server, req, &head.method)?;
    Ok(true)
}

/// Give the upgraded or `CONNECT` connection of `req` to the server's `upgrade` or `connect`
/// listeners, with the bytes after the request head, or destroy it if the server has none.
fn upgrade(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    socket: Object<'_>,
    server: Object<'_>,
    req: Object<'_>,
    method: &str,
) -> Result<(), ExnThrown> {
    let rest = match state.data().parser.get(scope) {
        Some(handle) => handle.data_mut().parser.take_rest(),
        None => Vec::new(),
    };
    let listeners: Vec<Object<'_>> = state
        .data()
        .listeners
        .iter()
        .map(|listener| listener.get(scope))
        .collect();
    for ((event, _, _), listener) in LISTENERS.iter().zip(listeners) {
        crate::events::listen(scope, socket, c"removeListener", event, listener)?;
    }
    free_parser(scope, state)?;
    let event = if method == "CONNECT" {
        c"connect"
    } else {
        c"upgrade"
    };
    if crate::events::listener_count(scope, server, js::class::get_or_init_atom(scope, event)?)?
        > 0.0
    {
        let head = buffer_from_vec(scope, rest)?.to_jsval_throwing(scope)?;
        socket.set_property(scope, c"readableFlowing", js::value::null())?;
        crate::events::emit(
            scope,
            server,
            event,
            &[
                req.to_jsval_throwing(scope)?,
                socket.to_jsval_throwing(scope)?,
                head,
            ],
        )?;
    } else {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    }
    Ok(())
}

/// Release the parser of the connection `state`, and stop tracking the connection.
fn free_parser(scope: &Scope<'_>, state: ConnectionState<'_>) -> Result<(), ExnThrown> {
    let (socket, server) = {
        let mut data = state.data_mut();
        data.parser = None;
        (data.socket.get(scope), data.server.get(scope))
    };
    socket.set_property(scope, c"parser", js::value::null())?;
    if let Some(server) = server_class::try_state_of(scope, server)? {
        let state_object = state.as_object();
        server.data_mut().connections.retain(|connection| {
            connection.get(scope).handle().get() != state_object.handle().get()
        });
    }
    Ok(())
}

/// Push the body bytes `body` onto the request being parsed, unless it was dumped, and pause the
/// socket if the request's buffer is full, as Node.js's `parserOnBody` does.
fn on_body(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    socket: Object<'_>,
    body: Vec<u8>,
) -> Result<(), ExnThrown> {
    let req = match state.data().parser.get(scope) {
        Some(handle) => handle.data().incoming.get(scope),
        None => None,
    };
    let Some(req) = req else {
        return Ok(());
    };
    if js::conversion::to_boolean(req.get_property(scope, c"_dumped")?) {
        return Ok(());
    }
    let chunk = buffer_from_vec(scope, body)?.to_jsval_throwing(scope)?;
    let more = Function::call_by_name(scope, req.handle(), c"push", &[chunk])?;
    if !js::conversion::to_boolean(more) {
        Function::call_by_name(scope, socket.handle(), c"pause", js::function::EmptyArgs)?;
    }
    Ok(())
}

/// End the request being parsed with the trailers `trailers`, and resume the socket, as Node.js's
/// `parserOnMessageComplete` does.
fn on_complete(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    handle: ParserHandle<'_>,
    trailers: Vec<(String, String)>,
) -> Result<(), ExnThrown> {
    let req = handle.data().incoming.get(scope);
    if let Some(req) = req {
        req.set_property(scope, c"complete", true)?;
        if !trailers.is_empty() {
            set_lines(scope, req, trailers, true)?;
        }
        Function::call_by_name(scope, req.handle(), c"push", &[HandleValue::null()])?;
    }
    read_start(scope, state)
}

/// Resume the socket of the connection `state` unless the server paused it or it isn't
/// readable, as Node.js's `readStart` does.
pub(super) fn read_start(scope: &Scope<'_>, state: ConnectionState<'_>) -> Result<(), ExnThrown> {
    let (paused, socket) = {
        let data = state.data();
        (data.paused, data.socket.get(scope))
    };
    if paused {
        return Ok(());
    }
    let readable = match crate::stream::state::ReadableSide::try_of(scope, socket)? {
        Some(side) => side.get().is_readable(),
        None => js::conversion::to_boolean(socket.get_property(scope, c"readable")?),
    };
    if readable {
        Function::call_by_name(scope, socket.handle(), c"resume", js::function::EmptyArgs)?;
    }
    Ok(())
}

/// Whether `server` upgrades the connection of `req`: through its `shouldUpgradeCallback`, which
/// by default checks for `upgrade` listeners.
fn should_upgrade(
    scope: &Scope<'_>,
    server: Object<'_>,
    req: Object<'_>,
) -> Result<bool, ExnThrown> {
    let callback = server.get_property(scope, c"shouldUpgradeCallback")?;
    if js::function::is_callable(callback) {
        let result = Function::call(
            scope,
            server.to_jsval_throwing(scope)?,
            callback,
            &[req.to_jsval_throwing(scope)?],
        )?;
        return Ok(js::conversion::to_boolean(result));
    }
    Ok(crate::events::listener_count(
        scope,
        server,
        js::class::get_or_init_atom(scope, c"upgrade")?,
    )? > 0.0)
}

/// The default `shouldUpgradeCallback` of a server, whose `this` is the server: whether it has
/// `upgrade` listeners.
pub(super) fn default_should_upgrade(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = crate::support::this_object(scope, &args)?;
    let count = crate::events::listener_count(
        scope,
        server,
        js::class::get_or_init_atom(scope, c"upgrade")?,
    )?;
    Ok(js::value::from_bool(count > 0.0))
}

/// Pause the socket of the connection `state` for the server, and record it in `_paused`.
fn pause(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    socket: Object<'_>,
) -> Result<(), ExnThrown> {
    state.data_mut().paused = true;
    socket.set_property(scope, c"_paused", true)?;
    Function::call_by_name(scope, socket.handle(), c"pause", js::function::EmptyArgs)?;
    Ok(())
}

/// Create the response to `req` and emit the request, as Node.js's `parserOnIncoming` does.
/// Returns whether the connection is upgraded instead.
fn on_incoming(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    socket: Object<'_>,
    req: Object<'_>,
    method: &str,
    keep_alive: bool,
) -> Result<bool, ExnThrown> {
    let server = state.data().server.get(scope);
    reset_socket_timeout(scope, server, socket, state)?;
    if js::conversion::to_boolean(req.get_property(scope, c"upgrade")?) {
        let upgrade = method == "CONNECT" || should_upgrade(scope, server, req)?;
        req.set_property(scope, c"upgrade", upgrade)?;
        if upgrade {
            return Ok(true);
        }
    }
    state.data_mut().incoming.push_back(Heap::from(req));
    let high_water_mark = match crate::stream::state::WritableSide::try_of(scope, socket)? {
        Some(side) => side.get().high_water_mark,
        None => number_or(socket.get_property(scope, c"writableHighWaterMark")?, 0.0),
    };
    if !state.data().paused {
        let need_drain = crate::stream::writable_need_drain(scope, socket)?;
        if need_drain || state.data().outgoing_data >= high_water_mark {
            pause(scope, state, socket)?;
        }
    }

    let response_class = server_class::state_of(scope, server)?
        .data()
        .response_class
        .get(scope);
    let reject = server.get_property(scope, c"rejectNonStandardBodyWrites")?;
    let req_value = req.to_jsval_throwing(scope)?;
    let keep_alive_timeout = server.get_property(scope, c"keepAliveTimeout")?;
    let max_requests = server.get_property(scope, c"maxRequestsPerSocket")?;
    let res = match response_class {
        Some(response_class) => {
            let options = Object::new_plain(scope)?;
            options.set_property(scope, c"highWaterMark", high_water_mark)?;
            options.set_property(scope, c"rejectNonStandardBodyWrites", reject)?;
            let options = options.to_jsval_throwing(scope)?;
            let res = Function::construct(scope, response_class, &[req_value, options])?;
            super::outgoing::state_of(scope, res)?.data_mut().connection = Some(Heap::from(state));
            res.set_property(scope, c"_keepAliveTimeout", keep_alive_timeout)?;
            res.set_property(scope, c"_maxRequestsPerSocket", max_requests)?;
            let pending_data = state.data().pending_data.get(scope);
            if let Some(pending_data) = pending_data {
                res.set_property(scope, c"_onPendingData", pending_data)?;
            }
            res.set_property(scope, c"shouldKeepAlive", keep_alive)?;
            res
        }
        None => {
            let fields = ConnectionFields {
                keep_alive,
                keep_alive_timeout,
                max_requests_per_socket: max_requests,
                on_pending_data: state.data().pending_data.get(scope),
            };
            let reject = js::conversion::to_boolean(reject);
            let res =
                super::outgoing::new_response(scope, req, Some(fields), high_water_mark, reject)?;
            super::outgoing::state_of(scope, res)?.data_mut().connection = Some(Heap::from(state));
            res
        }
    };
    let unique = server_class::state_of(scope, server)?
        .data()
        .unique_headers
        .clone();
    super::outgoing::state_of(scope, res)?
        .data_mut()
        .unique_headers = unique;
    let writing = state.data().current.is_some();
    if writing {
        state.data_mut().outgoing.push_back(Heap::from(res));
    } else {
        Function::call_by_name(
            scope,
            res.handle(),
            c"assignSocket",
            &[socket.to_jsval_throwing(scope)?],
        )?;
    }
    let on_finish = crate::support::shared_function(scope, res_on_finish, 0)?;
    crate::events::listen(scope, res, c"on", c"finish", on_finish)?;

    let res_value = res.to_jsval_throwing(scope)?;
    let mut handled = false;
    let major = number_or(req.get_property(scope, c"httpVersionMajor")?, 0.0);
    let minor = number_or(req.get_property(scope, c"httpVersionMinor")?, 0.0);
    if major == 1.0 && minor == 1.0 {
        let require_host =
            js::conversion::to_boolean(server.get_property(scope, c"requireHostHeader")?);
        if require_host && super::incoming::header_text(scope, req, "host")?.is_none() {
            let headers = js::Array::with_contents(
                scope,
                &[
                    js::class::get_or_init_atom(scope, c"Connection")?,
                    js::class::get_or_init_atom(scope, c"close")?,
                ],
            )?;
            let status = scope.root_value(js::value::from_f64(400.0));
            Function::call_by_name(
                scope,
                res.handle(),
                c"writeHead",
                &[status, headers.to_jsval_throwing(scope)?],
            )?;
            Function::call_by_name(scope, res.handle(), c"end", js::function::EmptyArgs)?;
            return Ok(false);
        }
        let max = number_or(max_requests, 0.0);
        let count = if max > 0.0 {
            let count = {
                let mut data = state.data_mut();
                data.requests_count += 1.0;
                data.requests_count
            };
            res.set_property(scope, c"maxRequestsOnConnectionReached", max <= count)?;
            count
        } else {
            0.0
        };
        let expect = super::incoming::header_text(scope, req, "expect")?;
        if max > 0.0 && max < count {
            handled = true;
            crate::events::emit(
                scope,
                server,
                c"dropRequest",
                &[req_value, socket.to_jsval_throwing(scope)?],
            )?;
            let status = scope.root_value(js::value::from_f64(503.0));
            Function::call_by_name(scope, res.handle(), c"writeHead", &[status])?;
            Function::call_by_name(scope, res.handle(), c"end", js::function::EmptyArgs)?;
        } else if let Some(expect) = expect {
            handled = true;
            if super::wire::has_word(&expect, "100-continue") {
                res.set_property(scope, c"_expect_continue", true)?;
                let listeners = crate::events::listener_count(
                    scope,
                    server,
                    js::class::get_or_init_atom(scope, c"checkContinue")?,
                )?;
                if listeners > 0.0 {
                    crate::events::emit(scope, server, c"checkContinue", &[req_value, res_value])?;
                } else {
                    Function::call_by_name(
                        scope,
                        res.handle(),
                        c"writeContinue",
                        js::function::EmptyArgs,
                    )?;
                    crate::events::emit(scope, server, c"request", &[req_value, res_value])?;
                }
            } else if crate::events::listener_count(
                scope,
                server,
                js::class::get_or_init_atom(scope, c"checkExpectation")?,
            )? > 0.0
            {
                crate::events::emit(scope, server, c"checkExpectation", &[req_value, res_value])?;
            } else {
                let status = scope.root_value(js::value::from_f64(417.0));
                Function::call_by_name(scope, res.handle(), c"writeHead", &[status])?;
                Function::call_by_name(scope, res.handle(), c"end", js::function::EmptyArgs)?;
            }
        }
    }
    if !handled {
        crate::events::emit(scope, server, c"request", &[req_value, res_value])?;
    }
    Ok(false)
}

/// Restore the server's timeout on `socket` if the keep-alive timeout replaced it.
fn reset_socket_timeout(
    scope: &Scope<'_>,
    server: Object<'_>,
    socket: Object<'_>,
    state: ConnectionState<'_>,
) -> Result<(), ExnThrown> {
    if !std::mem::take(&mut state.data_mut().keep_alive_timeout_set) {
        return Ok(());
    }
    let timeout = server.get_property(scope, c"timeout")?;
    let timeout = if js::conversion::to_boolean(timeout) {
        timeout
    } else {
        scope.root_value(js::value::from_f64(0.0))
    };
    Function::call_by_name(scope, socket.handle(), c"setTimeout", &[timeout])?;
    Ok(())
}

/// Close `socket` once it wrote everything, through its `destroySoon` if it has one.
fn destroy_soon(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    let method = if crate::support::has_method(scope, socket, c"destroySoon")? {
        c"destroySoon"
    } else {
        c"end"
    };
    Function::call_by_name(scope, socket.handle(), method, js::function::EmptyArgs)?;
    Ok(())
}

/// The `finish` listener of a response of a connection, whose `this` is the response: read the
/// rest of the request if nothing reads it, detach the response from the socket, and close the
/// connection if the response was the last, or give the socket to the next response, as
/// Node.js's `resOnFinish` does. Between requests, the socket times out after the server's
/// `keepAliveTimeout` plus `keepAliveTimeoutBuffer`. After a response that finishes once `close`
/// was called on the server, the connection is destroyed if it is still between requests
/// [`CLOSING_IDLE_DELAY`] milliseconds later.
fn res_on_finish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let res = crate::support::this_object(scope, &args)?;
    let response = super::outgoing::state_of(scope, res)?;
    let (req, state) = {
        let data = response.data();
        (data.request.get(scope), data.connection.get(scope))
    };
    let (Some(req), Some(state)) = (req, state) else {
        return Ok(js::value::undefined());
    };
    let (socket, server) = {
        let mut data = state.data_mut();
        data.incoming.pop_front();
        (data.socket.get(scope), data.server.get(scope))
    };
    let resume_scheduled = crate::stream::readable_resume_scheduled(scope, req)?;
    if !js::conversion::to_boolean(req.get_property(scope, c"_consuming")?) && !resume_scheduled {
        Function::call_by_name(scope, req.handle(), c"_dump", js::function::EmptyArgs)?;
    }
    Function::call_by_name(
        scope,
        res.handle(),
        c"detachSocket",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    clear_incoming(scope, state, req)?;
    js::jobs::queue_native_tick(scope, emit_close_tick, res, &[])?;
    let (waiting, next) = {
        let mut data = state.data_mut();
        let waiting = !data.outgoing.is_empty();
        let next = data.outgoing.front().map(|next| next.get(scope));
        drop(data.outgoing.pop_front());
        (waiting, next)
    };
    if js::conversion::to_boolean(res.get_property(scope, c"_last")?) {
        destroy_soon(scope, socket)?;
    } else if !waiting {
        let closing = server_class::state_of(scope, server)?.data().closing;
        if closing {
            let check = crate::support::shared_function(scope, close_if_idle, 0)?;
            let check = js::Callable::from_jsval(scope, check.to_jsval_throwing(scope)?, ())
                .map_err(|e| e.throw(scope))?;
            if let Some(id) = core_runtime::event_loop::timer::start_timer(
                scope,
                check,
                state.as_object().to_jsval_throwing(scope)?,
                &[],
                Duration::from_millis(CLOSING_IDLE_DELAY),
                false,
                None,
            ) {
                core_runtime::event_loop::with_active_event_loop(|el| {
                    el.set_js_timer_ref(id, false)
                });
            }
            return Ok(js::value::undefined());
        }
        let timeout = number_or(server.get_property(scope, c"keepAliveTimeout")?, 0.0).max(0.0);
        let buffer = server.get_property(scope, c"keepAliveTimeoutBuffer")?;
        let buffer = if buffer.is_number() && buffer.to_number() >= 0.0 {
            buffer.to_number()
        } else {
            1000.0
        };
        if timeout != 0.0 && crate::support::has_method(scope, socket, c"setTimeout")? {
            let delay = scope.root_value(js::value::from_f64(timeout + buffer));
            Function::call_by_name(scope, socket.handle(), c"setTimeout", &[delay])?;
            state.data_mut().keep_alive_timeout_set = true;
        }
    } else if let Some(next) = next {
        Function::call_by_name(
            scope,
            next.handle(),
            c"assignSocket",
            &[socket.to_jsval_throwing(scope)?],
        )?;
    }
    Ok(js::value::undefined())
}

/// The timer callback for a connection of a closed server, whose `this` is the connection:
/// destroy the socket if it is between requests and has no response in progress.
fn close_if_idle(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    let state = this
        .cast::<ConnectionState>()
        .map_err(|_| js::error::TypeError("not a connection".into()).throw(scope))?;
    let (socket, writing, active) = {
        let data = state.data();
        let active = match data.parser.get(scope) {
            Some(handle) => handle.data().parser.active(),
            None => true,
        };
        (data.socket.get(scope), data.current.is_some(), active)
    };
    if socket_destroyed(scope, socket)? || writing || active {
        return Ok(js::value::undefined());
    }
    Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// Clear the request the parser of the connection `state` parses if it is `req`, once `req`
/// ended, as Node.js's `clearIncoming` does.
fn clear_incoming(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    req: Object<'_>,
) -> Result<(), ExnThrown> {
    let Some(handle) = state.data().parser.get(scope) else {
        return Ok(());
    };
    let parsing = handle
        .data()
        .incoming
        .get(scope)
        .is_some_and(|incoming| incoming.handle().get() == req.handle().get());
    if !parsing {
        return Ok(());
    }
    let ended = crate::stream::state::ReadableSide::try_of(scope, req)?
        .is_some_and(|side| side.has(crate::stream::state::R::END_EMITTED));
    if ended {
        handle.data_mut().incoming = None;
    } else {
        let listener = Function::new_callback(scope, c"", 0, clear_incoming_on_end, state)?;
        crate::events::listen(scope, req, c"on", c"end", *listener)?;
    }
    Ok(())
}

/// The `end` listener of a request whose parser waits for its end, whose `this` is the request
/// and whose payload is the connection.
fn clear_incoming_on_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let req = crate::support::this_object(scope, &args)?;
    clear_incoming(scope, cast_payload::<ConnectionState>(scope, payload), req)?;
    Ok(js::value::undefined())
}

/// The `end` listener of a connection, whose payload is the connection: report a request the
/// peer cut off, end a request whose body lasts until the connection's end, and otherwise end the
/// socket, as Node.js's `socketOnEnd` does.
fn socket_on_end(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConnectionState>(scope, payload);
    let (socket, server) = {
        let data = state.data();
        (data.socket.get(scope), data.server.get(scope))
    };
    let handle = state.data().parser.get(scope);
    let result = match handle {
        Some(handle) => handle.data_mut().parser.finish(),
        None => Ok(None),
    };
    match result {
        Err(error) => {
            let raw = new_buffer(scope, &[])?.to_jsval_throwing(scope)?;
            let error = parse_error_value(scope, &error, raw)?;
            socket_error(scope, state, error)?;
            return Ok(js::value::undefined());
        }
        Ok(Some(Event::Complete(trailers))) => {
            if let Some(handle) = handle {
                on_complete(scope, state, handle, trailers)?;
            }
        }
        Ok(_) => {}
    }
    let (last, current) = {
        let data = state.data();
        (
            data.outgoing.back().map(|last| last.get(scope)),
            data.current.get(scope),
        )
    };
    if !js::conversion::to_boolean(server.get_property(scope, c"httpAllowHalfOpen")?) {
        Function::call_by_name(scope, socket.handle(), c"end", js::function::EmptyArgs)?;
    } else if let Some(last) = last {
        last.set_property(scope, c"_last", true)?;
    } else if let Some(message) = current {
        message.set_property(scope, c"_last", true)?;
    } else {
        Function::call_by_name(scope, socket.handle(), c"end", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

/// The `close` listener of a connection, whose payload is the connection: release the parser and
/// destroy the requests and responses still waiting with an `ECONNRESET` error, as Node.js's
/// `socketOnClose` does.
fn socket_on_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConnectionState>(scope, payload);
    free_parser(scope, state)?;
    let waiting: Vec<Object<'_>> = {
        let mut data = state.data_mut();
        let waiting = data
            .incoming
            .iter()
            .chain(&data.outgoing)
            .map(|message| message.get(scope))
            .collect();
        data.incoming.clear();
        data.outgoing.clear();
        waiting
    };
    for message in waiting {
        let error = NodeError::new(ErrorKind::Error, "ECONNRESET", "aborted").to_value(scope)?;
        Function::call_by_name(scope, message.handle(), c"destroy", &[error])?;
    }
    Ok(js::value::undefined())
}

/// Resume the socket of the connection `state` once the responses waiting for it queue less than
/// its high water mark, and emit `drain` on its current response once that has nothing left to
/// write, as Node.js's `socketOnDrain` does.
fn drain(scope: &Scope<'_>, state: ConnectionState<'_>) -> Result<(), ExnThrown> {
    let (socket, paused, outgoing_data, current) = {
        let data = state.data();
        (
            data.socket.get(scope),
            data.paused,
            data.outgoing_data,
            data.current.get(scope),
        )
    };
    let high_water_mark = match crate::stream::state::WritableSide::try_of(scope, socket)? {
        Some(side) => side.get().high_water_mark,
        None => number_or(socket.get_property(scope, c"writableHighWaterMark")?, 0.0),
    };
    if paused && outgoing_data <= high_water_mark {
        state.data_mut().paused = false;
        socket.set_property(scope, c"_paused", false)?;
        Function::call_by_name(scope, socket.handle(), c"resume", js::function::EmptyArgs)?;
    }
    let Some(message) = current else {
        return Ok(());
    };
    let response = super::outgoing::state_of(scope, message)?;
    let (ended, need_drain, output) = {
        let data = response.data();
        (data.ended, data.need_drain, data.output_size)
    };
    let pending = output + super::outgoing::socket_writable_length(scope, socket)?;
    if !ended && need_drain && pending == 0.0 {
        response.data_mut().need_drain = false;
        crate::events::emit(scope, message, c"drain", &[])?;
    }
    Ok(())
}

/// The `drain` listener of a connection, whose payload is the connection.
fn socket_on_drain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    drain(scope, cast_payload::<ConnectionState>(scope, payload))?;
    Ok(js::value::undefined())
}

/// Add `delta` to the bytes the responses of the connection `state` queued, and resume the
/// socket or emit `drain` as that allows.
pub(super) fn update_outgoing_data(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    delta: f64,
) -> Result<(), ExnThrown> {
    state.data_mut().outgoing_data += delta;
    drain(scope, state)
}

/// The response a server writes to a connection whose request failed with the error code `code`.
fn error_response(code: &str) -> &'static [u8] {
    match code {
        "HPE_HEADER_OVERFLOW" => {
            b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n"
        }
        "HPE_CHUNK_EXTENSIONS_OVERFLOW" => {
            b"HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\n\r\n"
        }
        "ERR_HTTP_REQUEST_TIMEOUT" => b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n",
        _ => b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n",
    }
}

/// Report the error `error` of the connection `state` as Node.js's `socketOnError` does: emit
/// `clientError` on the server with the error and the socket, or, without listeners, write the
/// response for the error unless the current response sent its header, and destroy the socket.
fn socket_error(
    scope: &Scope<'_>,
    state: ConnectionState<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let (socket, server, error_listener, current) = {
        let data = state.data();
        (
            data.socket.get(scope),
            data.server.get(scope),
            data.listeners.get(4).map(|listener| listener.get(scope)),
            data.current.get(scope),
        )
    };
    if let Some(listener) = error_listener {
        crate::events::listen(scope, socket, c"removeListener", c"error", listener)?;
    }
    let error_event = js::class::get_or_init_atom(scope, c"error")?;
    let noop = crate::support::noop_function(scope)?;
    let noops = Function::call_by_name(
        scope,
        socket.handle(),
        c"listenerCount",
        &[error_event, noop.to_jsval_throwing(scope)?],
    )?;
    if !js::conversion::to_boolean(noops) {
        crate::events::listen(scope, socket, c"on", c"error", noop)?;
    }
    let handled = crate::events::emit(
        scope,
        server,
        c"clientError",
        &[error, socket.to_jsval_throwing(scope)?],
    )?;
    if handled {
        return Ok(());
    }
    let header_sent = match current {
        Some(message) => {
            let response = super::outgoing::state_of(scope, message)?;
            let data = response.data();
            data.header_stored && data.header.is_none()
        }
        None => false,
    };
    let writable = match crate::stream::state::WritableSide::try_of(scope, socket)? {
        Some(side) => side.get().is_writable(),
        None => js::conversion::to_boolean(socket.get_property(scope, c"writable")?),
    };
    if writable && !header_sent {
        let code = match Object::from_value(scope, *error) {
            Ok(error) => crate::support::rust_string(scope, error.get_property(scope, c"code")?),
            Err(_) => None,
        };
        let response = error_response(code.as_deref().unwrap_or_default());
        let response = new_buffer(scope, response)?.to_jsval_throwing(scope)?;
        Function::call_by_name(scope, socket.handle(), c"write", &[response])?;
    }
    Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
    Ok(())
}

/// The `error` listener of a connection, whose payload is the connection.
fn socket_on_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    socket_error(
        scope,
        cast_payload::<ConnectionState>(scope, payload),
        args.get(0),
    )?;
    Ok(js::value::undefined())
}

/// The `timeout` listener of a connection, whose payload is the connection: emit `timeout` with
/// the socket on the request being parsed, the current response and the server, and destroy the
/// socket if none of them has a listener, as Node.js's `socketOnTimeout` does.
fn socket_on_timeout(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<ConnectionState>(scope, payload);
    let (socket, server, req, res) = {
        let data = state.data();
        let req = match data.parser.get(scope) {
            Some(handle) => handle.data().incoming.get(scope),
            None => None,
        };
        (
            data.socket.get(scope),
            data.server.get(scope),
            req,
            data.current.get(scope),
        )
    };
    let socket_value = socket.to_jsval_throwing(scope)?;
    let mut handled = false;
    if let Some(req) = req {
        if !js::conversion::to_boolean(req.get_property(scope, c"complete")?) {
            handled |= crate::events::emit(scope, req, c"timeout", &[socket_value])?;
        }
    }
    if let Some(res) = res {
        handled |= crate::events::emit(scope, res, c"timeout", &[socket_value])?;
    }
    handled |= crate::events::emit(scope, server, c"timeout", &[socket_value])?;
    if !handled {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    }
    Ok(js::value::undefined())
}

/// Destroy the open connections of `server`, or only those between requests and without a
/// response in progress if `idle_only`.
pub(super) fn close_connections(
    scope: &Scope<'_>,
    server: Object<'_>,
    idle_only: bool,
) -> Result<(), ExnThrown> {
    let connections: Vec<Object<'_>> = server_class::state_of(scope, server)?
        .data()
        .connections
        .iter()
        .map(|connection| connection.get(scope))
        .collect();
    for connection in connections {
        let Ok(state) = connection.cast::<ConnectionState>() else {
            continue;
        };
        let (socket, active, current) = {
            let data = state.data();
            let active = match data.parser.get(scope) {
                Some(handle) => handle.data().parser.active(),
                None => false,
            };
            (data.socket.get(scope), active, data.current.get(scope))
        };
        if idle_only {
            if active {
                continue;
            }
            if let Some(message) = current {
                if !super::outgoing::state_of(scope, message)?.data().ended {
                    continue;
                }
            }
        }
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    }
    Ok(())
}

/// The callback of the timer that checks the connections of a listening server, whose `this` is
/// the server: fail the requests that exceeded the server's `headersTimeout` or `requestTimeout`
/// with `ERR_HTTP_REQUEST_TIMEOUT`, as Node.js's `checkConnections` does.
fn check_connections(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = crate::support::this_object(scope, &args)?;
    let timeout = |name: &'static std::ffi::CStr| -> Result<f64, ExnThrown> {
        let value = server.get_property(scope, name)?;
        Ok(
            if value.is_number() && value.to_number().is_finite() && value.to_number() >= 0.0 {
                value.to_number()
            } else {
                0.0
            },
        )
    };
    let headers_timeout = timeout(c"headersTimeout")?;
    let request_timeout = timeout(c"requestTimeout")?;
    if headers_timeout == 0.0 && request_timeout == 0.0 {
        return Ok(js::value::undefined());
    }
    let connections: Vec<Object<'_>> = server_class::state_of(scope, server)?
        .data()
        .connections
        .iter()
        .map(|connection| connection.get(scope))
        .collect();
    let now = super::parser::now_ms();
    for connection in connections {
        let Ok(state) = connection.cast::<ConnectionState>() else {
            continue;
        };
        let Some(handle) = state.data().parser.get(scope) else {
            continue;
        };
        let expired = {
            let mut data = handle.data_mut();
            let expired = data.parser.expired(now, headers_timeout, request_timeout);
            if expired {
                data.parser.forget_start();
            }
            expired
        };
        if expired {
            let error = NodeError::new(
                ErrorKind::Error,
                "ERR_HTTP_REQUEST_TIMEOUT",
                "Request timeout",
            )
            .to_value(scope)?;
            socket_error(scope, state, error)?;
        }
    }
    Ok(js::value::undefined())
}

/// The `listening` listener of an `http.Server`, whose `this` is the server: check its
/// connections for timed-out requests every `connectionsCheckingInterval` milliseconds, with a
/// timer that doesn't keep the event loop alive.
pub(super) fn on_listening(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = crate::support::this_object(scope, &args)?;
    if !super::server::listens_itself() {
        return Ok(js::value::undefined());
    }
    stop_checking(scope, server)?;
    let interval = number_or(
        server.get_property(scope, c"connectionsCheckingInterval")?,
        0.0,
    );
    let check = crate::support::shared_function(scope, check_connections, 0)?;
    let check = js::Callable::from_jsval(scope, check.to_jsval_throwing(scope)?, ())
        .map_err(|e| e.throw(scope))?;
    let delay = Duration::from_millis(interval.max(1.0) as u64);
    let id = core_runtime::event_loop::timer::start_timer(
        scope,
        check,
        server.to_jsval_throwing(scope)?,
        &[],
        delay,
        true,
        None,
    );
    if let Some(id) = id {
        core_runtime::event_loop::with_active_event_loop(|el| el.set_js_timer_ref(id, false));
    }
    server_class::state_of(scope, server)?
        .data_mut()
        .check_timer = id;
    Ok(js::value::undefined())
}

/// Stop checking the connections of `server` for timed-out requests.
pub(super) fn stop_checking(scope: &Scope<'_>, server: Object<'_>) -> Result<(), ExnThrown> {
    let state: HttpServerState<'_> = server_class::state_of(scope, server)?;
    if let Some(id) = state.data_mut().check_timer.take() {
        core_runtime::event_loop::with_active_event_loop(|el| el.clear_js_timer(id));
    }
    Ok(())
}
