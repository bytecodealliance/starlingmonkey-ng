// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The connections of a listening `http.Server`, ported from Node.js's `lib/_http_server.js`.
//!
//! The server's `connection` listener reads the socket's `data` events into a [`Parser`] held in
//! a `ParserHandle` under the socket's `parser`. Each request head becomes an `IncomingMessage`
//! with a `ServerResponse`, emitted as a `request` event. The connection's state is a plain
//! object under the socket's `_starlingHttp`: the server, the parser, the requests waiting for
//! their responses (`incoming`), the responses waiting for the socket (`outgoing`), the request
//! being parsed (`current`), the bytes the waiting responses queued (`outgoingData`), and the
//! socket listeners. The server's open connections are its `_starlingConnections`.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::incoming::{incoming_message_class, set_header_lines};
use super::outgoing::{elements, emit_close_job, server_response_class};
use super::parser::{Event, Head, ParseError, Parser};
use crate::buffer::new_buffer;
use crate::errors::{ErrorKind, NodeError};
use crate::stream::{call_method, closure, has_method, string, truthy, Captures, Props};

/// The request parser of a connection.
#[core_runtime::jsclass(hidden)]
pub struct ParserHandle {
    #[no_trace]
    parser: Parser,
}

#[core_runtime::jsmethods]
impl ParserHandle {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// The largest request head a server accepts by default, in bytes.
const DEFAULT_MAX_HEADER_SIZE: f64 = 16384.0;

/// The most header lines a request may have by default.
const DEFAULT_MAX_HEADERS: usize = 1000;

/// The connection state of `socket`, if a server handles its requests.
fn state_of<'s>(scope: &'s Scope<'_>, socket: Object<'_>) -> Result<Option<Object<'s>>, ExnThrown> {
    socket.obj(scope, c"_starlingHttp")
}

/// The parser of the connection `state`, unless it was freed.
fn parser_of<'s>(
    scope: &'s Scope<'_>,
    state: Object<'_>,
) -> Result<Option<ParserHandle<'s>>, ExnThrown> {
    Ok(state
        .obj(scope, c"parser")?
        .and_then(|handle| handle.cast::<ParserHandle>().ok()))
}

/// A shared function of the current global for `body`, with `nargs` arguments, so listeners added
/// with it can be removed with it.
fn shared<'s>(
    scope: &'s Scope<'_>,
    body: js::function::Callback,
    nargs: u32,
) -> Result<Object<'s>, ExnThrown> {
    let function =
        js::class::get_or_init_shared_function(scope, body as *const () as usize, |scope| {
            Function::new_callback(scope, c"", nargs, body, ())
        })?;
    Ok(*function)
}

/// `array.length`.
fn length(scope: &Scope<'_>, array: Object<'_>) -> Result<u32, ExnThrown> {
    array
        .cast::<js::Array>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?
        .length(scope)
}

/// Remove `item` from `array`, if it is in it.
fn remove_from(scope: &Scope<'_>, array: Object<'_>, item: Object<'_>) -> Result<(), ExnThrown> {
    let index = call_method(scope, array, c"indexOf", &[item.to_jsval_throwing(scope)?])?;
    if index.is_number() && index.to_number() >= 0.0 {
        let one = scope.root_value(js::value::from_f64(1.0));
        call_method(scope, array, c"splice", &[index, one])?;
    }
    Ok(())
}

/// The open connections of `server`, created on first use.
pub(super) fn connections<'s>(
    scope: &'s Scope<'_>,
    server: Object<'_>,
) -> Result<Object<'s>, ExnThrown> {
    if let Some(list) = server.obj(scope, c"_starlingConnections")? {
        return Ok(list);
    }
    let list = *js::Array::new(scope, 0)?;
    server.define_property(scope, c"_starlingConnections", list, 0)?;
    Ok(list)
}

/// Call `socket.on(event, listener)`, or another listener method such as `removeListener`.
fn listen(
    scope: &Scope<'_>,
    socket: Object<'_>,
    method: &'static std::ffi::CStr,
    event: &str,
    listener: Object<'_>,
) -> Result<(), ExnThrown> {
    crate::stream::listen(scope, socket, method, event, listener)
}

/// The `connection` listener of an `http.Server`, whose `this` is the server: handle the
/// requests of the socket, as Node.js's `connectionListenerInternal` does.
pub(super) fn on_connection(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = crate::stream::this_object(scope, &args)?;
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    socket.set_property(scope, c"server", server)?;
    let timeout = server.val(scope, c"timeout")?;
    if truthy(scope, timeout) && has_method(scope, socket, c"setTimeout")? {
        call_method(scope, socket, c"setTimeout", &[timeout])?;
    }
    listen(
        scope,
        socket,
        c"on",
        "timeout",
        shared(scope, socket_on_timeout, 0)?,
    )?;

    let max_header_size = server.val(scope, c"maxHeaderSize")?;
    let max_header_size = if max_header_size.is_number() && max_header_size.to_number() > 0.0 {
        max_header_size.to_number()
    } else {
        DEFAULT_MAX_HEADER_SIZE
    };
    let max_headers = server.val(scope, c"maxHeadersCount")?;
    let max_headers = if max_headers.is_number() {
        let pairs = super::to_int32(max_headers.to_number()).wrapping_shl(1);
        (pairs > 0).then_some(pairs as usize / 2)
    } else {
        Some(DEFAULT_MAX_HEADERS)
    };
    let parser =
        js::class::create_instance_with::<ParserHandleImpl>(scope, |_| ParserHandleImpl {
            parser: Parser::new(max_header_size as usize, max_headers),
        })?;
    parser.set_property(scope, c"incoming", js::value::null())?;
    socket.set_property(scope, c"parser", parser)?;

    let state = Object::new_plain(scope)?;
    state.set_property(scope, c"server", server)?;
    state.set_property(scope, c"socket", socket)?;
    state.set_property(scope, c"parser", parser)?;
    state.set_property(scope, c"incoming", *js::Array::new(scope, 0)?)?;
    state.set_property(scope, c"outgoing", *js::Array::new(scope, 0)?)?;
    state.set_property(scope, c"current", js::value::null())?;
    state.set_property(scope, c"outgoingData", 0)?;
    state.set_property(scope, c"requestsCount", 0)?;
    state.set_property(scope, c"keepAliveTimeoutSet", false)?;
    socket.define_property(scope, c"_starlingHttp", state, 0)?;

    for (name, event, body, nargs) in [
        (
            c"onData",
            "data",
            socket_on_data as js::function::Callback,
            1,
        ),
        (c"onEnd", "end", socket_on_end, 0),
        (c"onClose", "close", socket_on_close, 0),
        (c"onDrain", "drain", socket_on_drain, 0),
    ] {
        let listener = closure(scope, nargs, body, &[state.to_jsval_raw_throwing(scope)?])?;
        state.set_property(scope, name, listener)?;
        listen(scope, socket, c"on", event, listener)?;
        if event == "data" {
            listen(
                scope,
                socket,
                c"on",
                "error",
                shared(scope, socket_on_error, 1)?,
            )?;
        }
    }
    let set_encoding = shared(scope, socket_set_encoding, 0)?;
    socket.set_property(scope, c"setEncoding", set_encoding)?;
    socket.set_property(scope, c"_paused", false)?;
    call_method(
        scope,
        connections(scope, server)?,
        c"push",
        &[socket.to_jsval_throwing(scope)?],
    )?;
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

/// The error for the parse error `error` of the bytes `raw`, as Node.js's parser and
/// `prepareError` create it.
fn parse_error_value<'s>(
    scope: &'s Scope<'_>,
    error: &ParseError,
    raw: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let message = std::ffi::CString::new(format!("Parse Error: {}", error.reason))
        .expect("reasons have no NUL");
    js::error::throw_plain_error(scope, &message);
    let value = crate::stream::take_exception(scope);
    let object = Object::from_value(scope, *value).map_err(|e| e.throw(scope))?;
    object.set_property(scope, c"bytesParsed", error.bytes_parsed as f64)?;
    object.set_property(scope, c"code", error.code)?;
    object.set_property(scope, c"reason", error.reason)?;
    object.set_property(scope, c"rawPacket", raw)?;
    Ok(value)
}

/// The `data` listener of a connection, whose captures are `[state]`: parse the chunk and handle
/// what it completes, as Node.js's `socketOnData` and the parser callbacks do. Parsing stops once
/// the socket is destroyed or upgraded.
fn socket_on_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    let Some(handle) = parser_of(scope, state)? else {
        return Ok(js::value::undefined());
    };
    let socket = state.obj(scope, c"socket")?.ok_or(ExnThrown)?;
    let chunk = args.get(0);
    let Some(bytes) = Object::from_value(scope, chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
        .map(|view| view.copy_bytes(scope))
        .transpose()?
    else {
        return Ok(js::value::undefined());
    };
    handle.data_mut().parser.feed(&bytes);
    loop {
        let same = match parser_of(scope, state)? {
            Some(current) => current.handle().get() == handle.handle().get(),
            None => false,
        };
        if !same || socket.flag(scope, c"destroyed")? {
            break;
        }
        let event = handle.data_mut().parser.next();
        match event {
            Ok(None) => break,
            Ok(Some(Event::Head(head))) => {
                if on_head(scope, state, socket, head)? {
                    break;
                }
            }
            Ok(Some(Event::Body(body))) => {
                on_body(scope, state, socket, &body)?;
                drain_after_callback(scope, socket)?;
            }
            Ok(Some(Event::Complete(trailers))) => {
                if !trailers.is_empty() {
                    drain_after_callback(scope, socket)?;
                }
                on_complete(scope, state, socket, &trailers)?;
            }
            Err(error) => {
                let error = parse_error_value(scope, &error, chunk)?;
                socket_error(scope, socket, error)?;
                break;
            }
        }
    }
    Ok(js::value::undefined())
}

/// Run the tick queue and the microtasks, if `socket` is a TCP socket. The server calls this after
/// a body chunk and before the end of a request with trailers, which matches the order of events
/// Node.js emits for a TCP connection. Microtasks don't run when this is called from a promise
/// reaction.
fn drain_after_callback(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    if socket.has_own_property(scope, c"_starlingHandle")? {
        js::jobs::run_jobs(scope);
    }
    Ok(())
}

/// Handle the request head `head`, as Node.js's `parserOnHeadersComplete` does. Returns whether
/// the connection was upgraded.
fn on_head(
    scope: &Scope<'_>,
    state: Object<'_>,
    socket: Object<'_>,
    head: Head,
) -> Result<bool, ExnThrown> {
    let server = state.obj(scope, c"server")?.ok_or(ExnThrown)?;
    let class = match server.obj(scope, c"_starlingIncomingMessage")? {
        Some(class) => class,
        None => incoming_message_class::class_constructor(scope)?,
    };
    let req = Function::construct(scope, class, &[socket.to_jsval_throwing(scope)?])?;
    req.define_property(scope, c"_starlingWire", true, 0)?;
    req.set_property(scope, c"httpVersionMajor", head.major)?;
    req.set_property(scope, c"httpVersionMinor", head.minor)?;
    let version = format!("{}.{}", head.major, head.minor);
    req.set_property(scope, c"httpVersion", version.as_str())?;
    let join = truthy(scope, server.val(scope, c"joinDuplicateHeaders")?);
    req.set_property(scope, c"joinDuplicateHeaders", join)?;
    req.set_property(scope, c"url", head.url.as_str())?;
    req.set_property(scope, c"upgrade", head.upgrade)?;
    set_header_lines(scope, req, &head.headers, false, join)?;
    req.set_property(scope, c"method", head.method.as_str())?;
    state.set_property(scope, c"current", req)?;
    if let Some(parser) = state.obj(scope, c"parser")? {
        parser.set_property(scope, c"incoming", req)?;
    }
    if !on_incoming(scope, state, socket, req, head.keep_alive)? {
        return Ok(false);
    }
    upgrade(scope, state, socket, server, req)?;
    Ok(true)
}

/// Give the upgraded or `CONNECT` connection of `req` to the server's `upgrade` or `connect`
/// listeners, with the bytes after the request head, or destroy it if the server has none.
fn upgrade(
    scope: &Scope<'_>,
    state: Object<'_>,
    socket: Object<'_>,
    server: Object<'_>,
    req: Object<'_>,
) -> Result<(), ExnThrown> {
    let rest = match parser_of(scope, state)? {
        Some(handle) => handle.data_mut().parser.take_rest(),
        None => Vec::new(),
    };
    for (name, event) in [
        (c"onData", "data"),
        (c"onEnd", "end"),
        (c"onClose", "close"),
        (c"onDrain", "drain"),
    ] {
        if let Some(listener) = state.obj(scope, name)? {
            listen(scope, socket, c"removeListener", event, listener)?;
        }
    }
    listen(
        scope,
        socket,
        c"removeListener",
        "error",
        shared(scope, socket_on_error, 1)?,
    )?;
    listen(
        scope,
        socket,
        c"removeListener",
        "timeout",
        shared(scope, socket_on_timeout, 0)?,
    )?;
    free_parser(scope, state, socket)?;
    let connect = crate::stream::is_str(scope, req.val(scope, c"method")?, c"CONNECT")?;
    let event = if connect { "connect" } else { "upgrade" };
    if crate::stream::listener_count(scope, server, string(scope, event)?)? > 0.0 {
        let head = new_buffer(scope, &rest)?.to_jsval_throwing(scope)?;
        socket.set_property(scope, c"readableFlowing", js::value::null())?;
        crate::stream::emit(
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
        call_method(scope, socket, c"destroy", &[])?;
    }
    Ok(())
}

/// Release the parser of the connection `state`, and stop tracking the connection.
fn free_parser(scope: &Scope<'_>, state: Object<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    state.set_property(scope, c"parser", js::value::null())?;
    socket.set_property(scope, c"parser", js::value::null())?;
    if let Some(server) = state.obj(scope, c"server")? {
        remove_from(scope, connections(scope, server)?, socket)?;
    }
    Ok(())
}

/// Push the body bytes `body` onto the request being parsed, unless it was dumped, and pause the
/// socket if the request's buffer is full, as Node.js's `parserOnBody` does.
fn on_body(
    scope: &Scope<'_>,
    state: Object<'_>,
    socket: Object<'_>,
    body: &[u8],
) -> Result<(), ExnThrown> {
    let Some(req) = state.obj(scope, c"current")? else {
        return Ok(());
    };
    if req.flag(scope, c"_dumped")? {
        return Ok(());
    }
    let chunk = new_buffer(scope, body)?.to_jsval_throwing(scope)?;
    let more = call_method(scope, req, c"push", &[chunk])?;
    if !truthy(scope, more) {
        call_method(scope, socket, c"pause", &[])?;
    }
    Ok(())
}

/// End the request being parsed with the trailers `trailers`, and resume the socket, as Node.js's
/// `parserOnMessageComplete` does.
fn on_complete(
    scope: &Scope<'_>,
    state: Object<'_>,
    socket: Object<'_>,
    trailers: &[(String, String)],
) -> Result<(), ExnThrown> {
    if let Some(req) = state.obj(scope, c"current")? {
        req.set_property(scope, c"complete", true)?;
        if !trailers.is_empty() {
            let join = req.flag(scope, c"joinDuplicateHeaders")?;
            set_header_lines(scope, req, trailers, true, join)?;
        }
        call_method(scope, req, c"push", &[scope.root_value(js::value::null())])?;
    }
    read_start(scope, socket)
}

/// Resume `socket` unless the server paused it or it isn't readable, as Node.js's `readStart`
/// does.
pub(super) fn read_start(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    if !socket.flag(scope, c"_paused")? && socket.flag(scope, c"readable")? {
        call_method(scope, socket, c"resume", &[])?;
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
    let callback = server.val(scope, c"shouldUpgradeCallback")?;
    if crate::stream::is_function(scope, callback) {
        let result = crate::stream::call(
            scope,
            server.to_jsval_throwing(scope)?,
            callback,
            &[req.to_jsval_throwing(scope)?],
        )?;
        return Ok(truthy(scope, result));
    }
    Ok(crate::stream::listener_count(scope, server, string(scope, "upgrade")?)? > 0.0)
}

/// The default `shouldUpgradeCallback` of a server, whose `this` is the server: whether it has
/// `upgrade` listeners.
pub(super) fn default_should_upgrade(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = crate::stream::this_object(scope, &args)?;
    let count = crate::stream::listener_count(scope, server, string(scope, "upgrade")?)?;
    Ok(js::value::from_bool(count > 0.0))
}

/// The value of the header `name` of `req`, or `undefined`.
fn header<'s>(
    scope: &'s Scope<'_>,
    req: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<HandleValue<'s>, ExnThrown> {
    match req.obj(scope, c"headers")? {
        Some(headers) => headers.val(scope, name),
        None => Ok(scope.root_value(js::value::undefined())),
    }
}

/// Create the response to `req` and emit the request, as Node.js's `parserOnIncoming` does.
/// Returns whether the connection is upgraded instead.
fn on_incoming(
    scope: &Scope<'_>,
    state: Object<'_>,
    socket: Object<'_>,
    req: Object<'_>,
    keep_alive: bool,
) -> Result<bool, ExnThrown> {
    let server = state.obj(scope, c"server")?.ok_or(ExnThrown)?;
    reset_socket_timeout(scope, server, socket, state)?;
    if req.flag(scope, c"upgrade")? {
        let connect = crate::stream::is_str(scope, req.val(scope, c"method")?, c"CONNECT")?;
        let upgrade = connect || should_upgrade(scope, server, req)?;
        req.set_property(scope, c"upgrade", upgrade)?;
        if upgrade {
            return Ok(true);
        }
    }
    let incoming = state.obj(scope, c"incoming")?.ok_or(ExnThrown)?;
    call_method(scope, incoming, c"push", &[req.to_jsval_throwing(scope)?])?;
    if !socket.flag(scope, c"_paused")? {
        let need_drain = match socket.obj(scope, c"_writableState")? {
            Some(writable) => writable.flag(scope, c"needDrain")?,
            None => false,
        };
        let high_water_mark = socket.num(scope, c"writableHighWaterMark")?;
        if need_drain || state.num(scope, c"outgoingData")? >= high_water_mark {
            socket.set_property(scope, c"_paused", true)?;
            call_method(scope, socket, c"pause", &[])?;
        }
    }

    let class = match server.obj(scope, c"_starlingServerResponse")? {
        Some(class) => class,
        None => server_response_class::class_constructor(scope)?,
    };
    let options = Object::new_plain(scope)?;
    options.set_property(
        scope,
        c"highWaterMark",
        socket.val(scope, c"writableHighWaterMark")?,
    )?;
    let reject = server.val(scope, c"rejectNonStandardBodyWrites")?;
    options.set_property(scope, c"rejectNonStandardBodyWrites", reject)?;
    let res = Function::construct(
        scope,
        class,
        &[
            req.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ],
    )?;
    res.define_property(scope, c"_starlingWire", true, 0)?;
    res.define_property(
        scope,
        c"_starlingRejectBodyWrites",
        truthy(scope, reject),
        0,
    )?;
    res.define_property(scope, c"_starlingHttp", state, 0)?;
    res.set_property(
        scope,
        c"_keepAliveTimeout",
        server.val(scope, c"keepAliveTimeout")?,
    )?;
    let max_requests = server.val(scope, c"maxRequestsPerSocket")?;
    res.set_property(scope, c"_maxRequestsPerSocket", max_requests)?;
    res.set_property(scope, c"shouldKeepAlive", keep_alive)?;
    res.set_property(scope, c"socket", js::value::null())?;
    res.set_property(scope, c"connection", js::value::null())?;
    if truthy(scope, socket.val(scope, c"_httpMessage")?) {
        let outgoing = state.obj(scope, c"outgoing")?.ok_or(ExnThrown)?;
        call_method(scope, outgoing, c"push", &[res.to_jsval_throwing(scope)?])?;
    } else {
        call_method(
            scope,
            res,
            c"assignSocket",
            &[socket.to_jsval_throwing(scope)?],
        )?;
    }
    let on_finish = closure(
        scope,
        0,
        res_on_finish,
        &[
            req.to_jsval_raw_throwing(scope)?,
            res.to_jsval_raw_throwing(scope)?,
            socket.to_jsval_raw_throwing(scope)?,
            state.to_jsval_raw_throwing(scope)?,
            server.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    listen(scope, res, c"on", "finish", on_finish)?;

    let req_value = req.to_jsval_throwing(scope)?;
    let res_value = res.to_jsval_throwing(scope)?;
    let mut handled = false;
    if req.num(scope, c"httpVersionMajor")? == 1.0 && req.num(scope, c"httpVersionMinor")? == 1.0 {
        if server.flag(scope, c"requireHostHeader")? && header(scope, req, c"host")?.is_undefined()
        {
            let headers = js::Array::with_contents(
                scope,
                &[string(scope, "Connection")?, string(scope, "close")?],
            )?;
            let status = scope.root_value(js::value::from_f64(400.0));
            call_method(
                scope,
                res,
                c"writeHead",
                &[status, headers.to_jsval_throwing(scope)?],
            )?;
            call_method(scope, res, c"end", &[])?;
            return Ok(false);
        }
        let max = if max_requests.is_number() {
            max_requests.to_number()
        } else {
            0.0
        };
        let count = if max > 0.0 {
            let count = state.num(scope, c"requestsCount")? + 1.0;
            state.set_property(scope, c"requestsCount", count)?;
            res.set_property(scope, c"maxRequestsOnConnectionReached", max <= count)?;
            count
        } else {
            0.0
        };
        let expect = header(scope, req, c"expect")?;
        if max > 0.0 && max < count {
            handled = true;
            crate::stream::emit(
                scope,
                server,
                "dropRequest",
                &[req_value, socket.to_jsval_throwing(scope)?],
            )?;
            let status = scope.root_value(js::value::from_f64(503.0));
            call_method(scope, res, c"writeHead", &[status])?;
            call_method(scope, res, c"end", &[])?;
        } else if !expect.is_undefined() {
            handled = true;
            let expect = String::from_jsval(scope, expect, ()).unwrap_or_default();
            if super::wire::has_word(&expect, "100-continue") {
                res.set_property(scope, c"_expect_continue", true)?;
                let listeners =
                    crate::stream::listener_count(scope, server, string(scope, "checkContinue")?)?;
                if listeners > 0.0 {
                    crate::stream::emit(scope, server, "checkContinue", &[req_value, res_value])?;
                } else {
                    call_method(scope, res, c"writeContinue", &[])?;
                    crate::stream::emit(scope, server, "request", &[req_value, res_value])?;
                }
            } else if crate::stream::listener_count(
                scope,
                server,
                string(scope, "checkExpectation")?,
            )? > 0.0
            {
                crate::stream::emit(scope, server, "checkExpectation", &[req_value, res_value])?;
            } else {
                let status = scope.root_value(js::value::from_f64(417.0));
                call_method(scope, res, c"writeHead", &[status])?;
                call_method(scope, res, c"end", &[])?;
            }
        }
    }
    if !handled {
        crate::stream::emit(scope, server, "request", &[req_value, res_value])?;
    }
    Ok(false)
}

/// Restore the server's timeout on `socket` if the keep-alive timeout replaced it.
fn reset_socket_timeout(
    scope: &Scope<'_>,
    server: Object<'_>,
    socket: Object<'_>,
    state: Object<'_>,
) -> Result<(), ExnThrown> {
    if !state.flag(scope, c"keepAliveTimeoutSet")? {
        return Ok(());
    }
    let timeout = server.val(scope, c"timeout")?;
    let timeout = if truthy(scope, timeout) {
        timeout
    } else {
        scope.root_value(js::value::from_f64(0.0))
    };
    call_method(scope, socket, c"setTimeout", &[timeout])?;
    state.set_property(scope, c"keepAliveTimeoutSet", false)?;
    Ok(())
}

/// Close `socket` once it wrote everything, through its `destroySoon` if it has one.
fn destroy_soon(scope: &Scope<'_>, socket: Object<'_>) -> Result<(), ExnThrown> {
    if has_method(scope, socket, c"destroySoon")? {
        call_method(scope, socket, c"destroySoon", &[])?;
    } else {
        call_method(scope, socket, c"end", &[])?;
    }
    Ok(())
}

/// The `finish` listener of a response, whose captures are `[req, res, socket, state, server]`:
/// read the rest of the request if nothing reads it, detach the response from the socket, and
/// close the connection if the response was the last, or give the socket to the next response,
/// as Node.js's `resOnFinish` does. Between requests, the socket times out after the server's
/// `keepAliveTimeout` plus `keepAliveTimeoutBuffer`. After a response that finishes once `close`
/// was called on the server, the connection is destroyed if it is still between requests
/// [`CLOSING_IDLE_DELAY`] milliseconds later.
fn res_on_finish(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let req = captures.obj(0)?;
    let res = captures.obj(1)?;
    let socket = captures.obj(2)?;
    let state = captures.obj(3)?;
    let server = captures.obj(4)?;
    let incoming = state.obj(scope, c"incoming")?.ok_or(ExnThrown)?;
    remove_from(scope, incoming, req)?;
    let resume_scheduled = match req.obj(scope, c"_readableState")? {
        Some(readable) => readable.flag(scope, c"resumeScheduled")?,
        None => false,
    };
    if !req.flag(scope, c"_consuming")? && !resume_scheduled {
        call_method(scope, req, c"_dump", &[])?;
    }
    call_method(
        scope,
        res,
        c"detachSocket",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    clear_incoming(scope, req)?;
    let job = Function::new_callback(scope, c"", 0, emit_close_job, res)?;
    js::jobs::queue_tick(scope, job.to_jsval_throwing(scope)?, &[])?;
    let outgoing = state.obj(scope, c"outgoing")?.ok_or(ExnThrown)?;
    if res.flag(scope, c"_last")? {
        destroy_soon(scope, socket)?;
    } else if length(scope, outgoing)? == 0 {
        if server.flag(scope, c"_starlingClosing")? {
            let check = closure(
                scope,
                0,
                close_if_idle,
                &[socket.to_jsval_raw_throwing(scope)?],
            )?;
            let set = scope.global().val(scope, c"setTimeout")?;
            let delay = scope.root_value(js::value::from_f64(CLOSING_IDLE_DELAY));
            let timer = Function::call(
                scope,
                js::value::undefined(),
                set,
                &[check.to_jsval_throwing(scope)?, delay],
            )?;
            if let Ok(timer) = Object::from_value(scope, *timer) {
                if has_method(scope, timer, c"unref")? {
                    call_method(scope, timer, c"unref", &[])?;
                }
            }
            return Ok(js::value::undefined());
        }
        let timeout = server.val(scope, c"keepAliveTimeout")?;
        let timeout = if timeout.is_number() && timeout.to_number() >= 0.0 {
            timeout.to_number()
        } else {
            0.0
        };
        let buffer = server.val(scope, c"keepAliveTimeoutBuffer")?;
        let buffer = if buffer.is_number() && buffer.to_number() >= 0.0 {
            buffer.to_number()
        } else {
            1000.0
        };
        if timeout != 0.0 && has_method(scope, socket, c"setTimeout")? {
            let delay = scope.root_value(js::value::from_f64(timeout + buffer));
            call_method(scope, socket, c"setTimeout", &[delay])?;
            state.set_property(scope, c"keepAliveTimeoutSet", true)?;
        }
    } else {
        let next = call_method(scope, outgoing, c"shift", &[])?;
        if let Ok(next) = Object::from_value(scope, *next) {
            call_method(
                scope,
                next,
                c"assignSocket",
                &[socket.to_jsval_throwing(scope)?],
            )?;
        }
    }
    Ok(js::value::undefined())
}

/// The milliseconds after which a connection of a closed server that is still between requests
/// is destroyed.
const CLOSING_IDLE_DELAY: f64 = 500.0;

/// The timer callback of a connection of a closed server, whose captures are `[socket]`: destroy
/// the socket if it is between requests and has no response in progress.
fn close_if_idle(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Captures::of(scope, payload)?.obj(0)?;
    if socket.flag(scope, c"destroyed")? || truthy(scope, socket.val(scope, c"_httpMessage")?) {
        return Ok(js::value::undefined());
    }
    let active = match state_of(scope, socket)? {
        Some(state) => match parser_of(scope, state)? {
            Some(handle) => handle.data().parser.active(),
            None => true,
        },
        None => true,
    };
    if !active {
        call_method(scope, socket, c"destroy", &[])?;
    }
    Ok(js::value::undefined())
}

/// Clear the `incoming` request of the parser of the socket of `req` once `req` ended, as
/// Node.js's `clearIncoming` does.
fn clear_incoming(scope: &Scope<'_>, req: Object<'_>) -> Result<(), ExnThrown> {
    let Some(socket) = req.obj(scope, c"socket")? else {
        return Ok(());
    };
    let Some(parser) = socket.obj(scope, c"parser")? else {
        return Ok(());
    };
    let incoming = parser.val(scope, c"incoming")?;
    if !crate::stream::same(scope, incoming, req.to_jsval_throwing(scope)?)? {
        return Ok(());
    }
    if req.flag(scope, c"readableEnded")? {
        parser.set_property(scope, c"incoming", js::value::null())?;
    } else {
        listen(
            scope,
            req,
            c"on",
            "end",
            shared(scope, clear_incoming_on_end, 0)?,
        )?;
    }
    Ok(())
}

/// The `end` listener of a request whose `incoming` parser entry waits for its end, whose `this`
/// is the request.
fn clear_incoming_on_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let req = crate::stream::this_object(scope, &args)?;
    clear_incoming(scope, req)?;
    Ok(js::value::undefined())
}

/// The `end` listener of a connection, whose captures are `[state]`: report a request the peer
/// cut off, and otherwise end the socket, as Node.js's `socketOnEnd` does.
fn socket_on_end(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    let socket = state.obj(scope, c"socket")?.ok_or(ExnThrown)?;
    let server = state.obj(scope, c"server")?.ok_or(ExnThrown)?;
    let result = match parser_of(scope, state)? {
        Some(handle) => handle.data_mut().parser.finish(),
        None => Ok(()),
    };
    if let Err(error) = result {
        let raw = new_buffer(scope, &[])?.to_jsval_throwing(scope)?;
        let error = parse_error_value(scope, &error, raw)?;
        socket_error(scope, socket, error)?;
        return Ok(js::value::undefined());
    }
    let outgoing = state.obj(scope, c"outgoing")?.ok_or(ExnThrown)?;
    if !server.flag(scope, c"httpAllowHalfOpen")? {
        call_method(scope, socket, c"end", &[])?;
    } else if length(scope, outgoing)? > 0 {
        let last = elements(scope, outgoing)?.pop().ok_or(ExnThrown)?;
        let last = Object::from_value(scope, *last).map_err(|e| e.throw(scope))?;
        last.set_property(scope, c"_last", true)?;
    } else if let Some(message) = socket.obj(scope, c"_httpMessage")? {
        message.set_property(scope, c"_last", true)?;
    } else {
        call_method(scope, socket, c"end", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `close` listener of a connection, whose captures are `[state]`: release the parser and
/// destroy the requests and responses still waiting with an `ECONNRESET` error, as Node.js's
/// `socketOnClose` does.
fn socket_on_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    let socket = state.obj(scope, c"socket")?.ok_or(ExnThrown)?;
    free_parser(scope, state, socket)?;
    for name in [c"incoming", c"outgoing"] {
        let queue = state.obj(scope, name)?.ok_or(ExnThrown)?;
        let messages = elements(scope, queue)?;
        state.set_property(scope, name, *js::Array::new(scope, 0)?)?;
        for message in messages {
            let message = Object::from_value(scope, *message).map_err(|e| e.throw(scope))?;
            let error = crate::stream::error_value(
                scope,
                NodeError::new(ErrorKind::Error, "ECONNRESET", "aborted"),
            )?;
            call_method(scope, message, c"destroy", &[error])?;
        }
    }
    Ok(js::value::undefined())
}

/// Resume the socket of the connection `state` once the responses waiting for it queue less than
/// its high water mark, and emit `drain` on its current response once that has nothing left to
/// write, as Node.js's `socketOnDrain` does.
fn drain(scope: &Scope<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    let Some(socket) = state.obj(scope, c"socket")? else {
        return Ok(());
    };
    let need_pause =
        state.num(scope, c"outgoingData")? > socket.num(scope, c"writableHighWaterMark")?;
    if socket.flag(scope, c"_paused")? && !need_pause {
        socket.set_property(scope, c"_paused", false)?;
        call_method(scope, socket, c"resume", &[])?;
    }
    let Some(message) = socket.obj(scope, c"_httpMessage")? else {
        return Ok(());
    };
    let pending = message.num(scope, c"outputSize")? + socket.num(scope, c"writableLength")?;
    if !message.flag(scope, c"finished")?
        && message.flag(scope, c"writableNeedDrain")?
        && pending == 0.0
    {
        message.set_property(scope, c"writableNeedDrain", false)?;
        call_method(scope, message, c"emit", &[string(scope, "drain")?])?;
    }
    Ok(())
}

/// The `drain` listener of a connection, whose captures are `[state]`.
fn socket_on_drain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    drain(scope, state)?;
    Ok(js::value::undefined())
}

/// Add `delta` to the bytes the responses of the connection of `message` queued, and resume the
/// socket or emit `drain` as that allows.
pub(super) fn update_outgoing_data(
    scope: &Scope<'_>,
    message: Object<'_>,
    delta: f64,
) -> Result<(), ExnThrown> {
    let Some(state) = message.obj(scope, c"_starlingHttp")? else {
        return Ok(());
    };
    let total = state.num(scope, c"outgoingData")? + delta;
    state.set_property(scope, c"outgoingData", total)?;
    drain(scope, state)
}

/// A listener that does nothing, which keeps a socket's later errors from being thrown.
fn noop(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
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

/// Report the error `error` of the connection of `socket` as Node.js's `socketOnError` does: emit
/// `clientError` on the server with the error and the socket, or, without listeners, write the
/// response for the error unless the current response sent its header, and destroy the socket.
fn socket_error(
    scope: &Scope<'_>,
    socket: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    listen(
        scope,
        socket,
        c"removeListener",
        "error",
        shared(scope, socket_on_error, 1)?,
    )?;
    let error_event = string(scope, "error")?;
    let noop = shared(scope, noop, 0)?;
    let noops = call_method(
        scope,
        socket,
        c"listenerCount",
        &[error_event, noop.to_jsval_throwing(scope)?],
    )?;
    if !truthy(scope, noops) {
        listen(scope, socket, c"on", "error", noop)?;
    }
    let Some(server) = socket.obj(scope, c"server")? else {
        call_method(scope, socket, c"destroy", &[error])?;
        return Ok(());
    };
    let handled = crate::stream::emit(
        scope,
        server,
        "clientError",
        &[error, socket.to_jsval_throwing(scope)?],
    )?;
    if handled {
        return Ok(());
    }
    let header_sent = match socket.obj(scope, c"_httpMessage")? {
        Some(message) => {
            message.val(scope, c"_header")?.is_string()
                && !message.flag(scope, c"_starlingHeaderPending")?
        }
        None => false,
    };
    if socket.flag(scope, c"writable")? && !header_sent {
        let code = match Object::from_value(scope, *error) {
            Ok(error) => crate::stream::rust_string(scope, error.val(scope, c"code")?),
            Err(_) => None,
        };
        let response = error_response(code.as_deref().unwrap_or_default());
        let response = new_buffer(scope, response)?.to_jsval_throwing(scope)?;
        call_method(scope, socket, c"write", &[response])?;
    }
    call_method(scope, socket, c"destroy", &[error])?;
    Ok(())
}

/// The `error` listener of a connection, whose `this` is the socket.
fn socket_on_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::stream::this_object(scope, &args)?;
    socket_error(scope, socket, args.get(0))?;
    Ok(js::value::undefined())
}

/// The `timeout` listener of a connection, whose `this` is the socket: emit `timeout` with the
/// socket on the request being parsed, the current response and the server, and destroy the
/// socket if none of them has a listener, as Node.js's `socketOnTimeout` does.
fn socket_on_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::stream::this_object(scope, &args)?;
    let socket_value = socket.to_jsval_throwing(scope)?;
    let mut handled = false;
    if let Some(parser) = socket.obj(scope, c"parser")? {
        if let Some(req) = parser.obj(scope, c"incoming")? {
            if !req.flag(scope, c"complete")? {
                handled |= crate::stream::emit(scope, req, "timeout", &[socket_value])?;
            }
        }
    }
    if let Some(res) = socket.obj(scope, c"_httpMessage")? {
        handled |= crate::stream::emit(scope, res, "timeout", &[socket_value])?;
    }
    if let Some(server) = socket.obj(scope, c"server")? {
        handled |= crate::stream::emit(scope, server, "timeout", &[socket_value])?;
    }
    if !handled {
        call_method(scope, socket, c"destroy", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `close` listener a response adds to its socket, whose `this` is the socket: emit `close`
/// on the socket's current response.
fn on_response_socket_close(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::stream::this_object(scope, &args)?;
    if let Some(message) = socket.obj(scope, c"_httpMessage")? {
        super::outgoing::emit_close(scope, message)?;
    }
    Ok(js::value::undefined())
}

/// The shared `close` listener a response adds to the socket it is assigned.
pub(super) fn response_close_listener<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    shared(scope, on_response_socket_close, 0)
}

/// Destroy the open connections of `server`, or only those between requests and without a
/// response in progress if `idle_only`.
pub(super) fn close_connections(
    scope: &Scope<'_>,
    server: Object<'_>,
    idle_only: bool,
) -> Result<(), ExnThrown> {
    let Some(list) = server.obj(scope, c"_starlingConnections")? else {
        return Ok(());
    };
    for socket in elements(scope, list)? {
        let socket = Object::from_value(scope, *socket).map_err(|e| e.throw(scope))?;
        if idle_only {
            let active = match state_of(scope, socket)? {
                Some(state) => match parser_of(scope, state)? {
                    Some(handle) => handle.data().parser.active(),
                    None => false,
                },
                None => false,
            };
            if active {
                continue;
            }
            if let Some(message) = socket.obj(scope, c"_httpMessage")? {
                if !message.flag(scope, c"finished")? {
                    continue;
                }
            }
        }
        call_method(scope, socket, c"destroy", &[])?;
    }
    Ok(())
}

/// The interval callback of a listening server, whose captures are `[server]`: fail the requests
/// that exceeded the server's `headersTimeout` or `requestTimeout` with
/// `ERR_HTTP_REQUEST_TIMEOUT`, as Node.js's `checkConnections` does.
fn check_connections(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = Captures::of(scope, payload)?.obj(0)?;
    let timeout = |name: &'static std::ffi::CStr| -> Result<f64, ExnThrown> {
        let value = server.val(scope, name)?;
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
    let Some(list) = server.obj(scope, c"_starlingConnections")? else {
        return Ok(js::value::undefined());
    };
    let now = super::parser::now_ms();
    for socket in elements(scope, list)? {
        let socket = Object::from_value(scope, *socket).map_err(|e| e.throw(scope))?;
        let Some(state) = state_of(scope, socket)? else {
            continue;
        };
        let Some(handle) = parser_of(scope, state)? else {
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
            let error = crate::stream::error_value(
                scope,
                NodeError::new(
                    ErrorKind::Error,
                    "ERR_HTTP_REQUEST_TIMEOUT",
                    "Request timeout",
                ),
            )?;
            socket_error(scope, socket, error)?;
        }
    }
    Ok(js::value::undefined())
}

/// The `listening` listener of an `http.Server`, whose `this` is the server: check its
/// connections for timed-out requests every `connectionsCheckingInterval` milliseconds, with an
/// interval that doesn't keep the event loop alive.
pub(super) fn on_listening(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let server = crate::stream::this_object(scope, &args)?;
    if !super::server::listens_itself() {
        return Ok(js::value::undefined());
    }
    stop_checking(scope, server)?;
    let check = closure(
        scope,
        0,
        check_connections,
        &[server.to_jsval_raw_throwing(scope)?],
    )?;
    let interval = server.val(scope, c"connectionsCheckingInterval")?;
    let set = scope.global().val(scope, c"setInterval")?;
    let timer = Function::call(
        scope,
        js::value::undefined(),
        set,
        &[check.to_jsval_throwing(scope)?, interval],
    )?;
    if let Ok(timer) = Object::from_value(scope, *timer) {
        if has_method(scope, timer, c"unref")? {
            call_method(scope, timer, c"unref", &[])?;
        }
    }
    server
        .define_property(scope, c"_starlingCheckInterval", timer, 0)
        .map(|()| js::value::undefined())
}

/// Stop checking the connections of `server` for timed-out requests.
pub(super) fn stop_checking(scope: &Scope<'_>, server: Object<'_>) -> Result<(), ExnThrown> {
    let timer = server.val(scope, c"_starlingCheckInterval")?;
    if !timer.is_undefined() {
        let clear = scope.global().val(scope, c"clearInterval")?;
        Function::call(scope, js::value::undefined(), clear, &[timer])?;
        server.set_property(scope, c"_starlingCheckInterval", js::value::undefined())?;
    }
    Ok(())
}
