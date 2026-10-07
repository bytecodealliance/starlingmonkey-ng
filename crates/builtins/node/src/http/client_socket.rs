// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A `ClientRequest` that writes HTTP/1.1 to a socket, ported from Node.js's
//! `lib/_http_client.js`.
//!
//! The request gets its socket from its agent's `addRequest`, from its `createConnection`
//! option, or from `net.createConnection`, through `onSocket`. It writes its header block and
//! body as [`super::wire`] frames them, and parses the response with a [`Parser`] in the socket's
//! `parser`. The socket listeners are shared functions, which read the request from the socket's
//! `_httpMessage` and the parser from its `parser`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};

use super::client::with_client;
use super::connection::{ParserHandle, ParserHandleImpl};
use super::incoming::{new_message, MessageHead};
use super::outgoing::OutgoingState;
use super::parser::{Event, Head, HeadAction, Kind, Parser};
use crate::buffer::buffer_from_vec;
use crate::errors::{ErrorKind, NodeError, Thrown};

/// The largest response head accepted by default, in bytes: `http.maxHeaderSize`.
const DEFAULT_MAX_HEADER_SIZE: usize = 16384;

/// The most header lines of a response kept by default.
const DEFAULT_MAX_HEADERS: usize = 1000;

/// The properties of the object an `information` event passes, in Node's order.
pub(crate) static INFORMATION_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"statusCode"),
    TemplateKey::Name(c"statusMessage"),
    TemplateKey::Name(c"httpVersion"),
    TemplateKey::Name(c"httpVersionMajor"),
    TemplateKey::Name(c"httpVersionMinor"),
    TemplateKey::Name(c"headers"),
    TemplateKey::Name(c"rawHeaders"),
]);

/// The most freed parsers kept for reuse, as in Node.js's `parsers` free list.
const MAX_FREE_PARSERS: usize = 1000;

/// The parsers of client requests that were freed, which later requests reuse, as Node.js's
/// `parsers` free list keeps them.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ParserPool {
    free: Vec<Heap<ParserHandleImpl>>,
}

#[core_runtime::jsmethods]
impl ParserPool {}

/// The key of the global's parser pool.
static PARSER_POOL_KEY: u8 = 0;

/// The parser pool of the current global.
fn parser_pool<'s>(scope: &'s Scope<'_>) -> Result<ParserPool<'s>, ExnThrown> {
    let pool = js::class::get_or_init_shared_object(
        scope,
        &PARSER_POOL_KEY as *const u8 as usize,
        |scope| {
            let pool = js::class::create_instance_with::<ParserPoolImpl>(scope, |_| {
                ParserPoolImpl::default()
            })?;
            Ok(pool.as_object())
        },
    )?;
    pool.cast::<ParserPool>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))
}

/// A socket method call that waits for the request's socket, from `_deferToConnect`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DeferredCall {
    request: Heap<js::object::Object>,
    method: Heap<Value>,
    args: Heap<Value>,
}

#[core_runtime::jsmethods]
impl DeferredCall {}

/// `ECONNRESET` with `message`, as Node.js's `ConnResetException` creates it.
fn conn_reset<'s>(scope: &'s Scope<'_>, message: &str) -> Result<HandleValue<'s>, ExnThrown> {
    NodeError::new(ErrorKind::Error, "ECONNRESET", message.to_string()).to_value(scope)
}

/// The request `socket` belongs to: its `_httpMessage`, if that is an object.
fn http_message<'s>(
    scope: &'s Scope<'_>,
    socket: Object<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let message = socket.get_property(scope, c"_httpMessage")?;
    Ok(Object::from_value(scope, *message).ok())
}

/// The parser of `socket`: its `parser`, if that is a parser.
fn parser_of<'s>(
    scope: &'s Scope<'_>,
    socket: Object<'_>,
) -> Result<Option<ParserHandle<'s>>, ExnThrown> {
    let parser = socket.get_property(scope, c"parser")?;
    Ok(Object::from_value(scope, *parser)
        .ok()
        .and_then(|parser| parser.cast::<ParserHandle>().ok()))
}

/// The socket of the request of `state`.
fn socket_of<'s>(scope: &'s Scope<'_>, state: &OutgoingState<'_>) -> Option<Object<'s>> {
    state.data().socket.get(scope)
}

/// The listener functions of the socket of a request, created once per global, so they can be
/// removed with the same functions.
fn listener<'s>(
    scope: &'s Scope<'_>,
    body: js::function::Callback,
    name: &'static std::ffi::CStr,
    nargs: u32,
) -> Result<Object<'s>, ExnThrown> {
    crate::support::shared_named_function(scope, body, name, nargs)
}

/// Remove the listener `body` of the socket listeners from `event` of `emitter`.
fn remove(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: &'static std::ffi::CStr,
    body: js::function::Callback,
    name: &'static std::ffi::CStr,
) -> Result<(), ExnThrown> {
    let function = listener(scope, body, name, 0)?;
    crate::events::listen(scope, emitter, c"removeListener", event, function)
}

/// Emit `error` on `request`, as Node.js's `emitErrorEvent` does.
fn emit_error_event(
    scope: &Scope<'_>,
    request: Object<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    crate::events::emit(scope, request, c"error", &[error])?;
    Ok(())
}

/// Mark `request` closed and emit `close` on it.
fn close_request(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    super::outgoing::state_of(scope, request)?.data_mut().closed = true;
    request.set_property(scope, c"_closed", true)?;
    crate::events::emit(scope, request, c"close", &[])?;
    Ok(())
}

/// Mark `request` destroyed.
fn mark_destroyed(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    super::outgoing::state_of(scope, request)?
        .data_mut()
        .destroyed = true;
    request.set_property(scope, c"destroyed", true)
}

/// Set `_hadError` on `socket`, unless it was set, and return whether it was set before.
fn had_error(scope: &Scope<'_>, socket: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Ok(socket) = Object::from_value(scope, *socket) else {
        return Ok(false);
    };
    if crate::http::flag(scope, socket, c"_hadError")? {
        return Ok(true);
    }
    socket.set_property(scope, c"_hadError", true)?;
    Ok(false)
}

/// Connect the request `request`, whose options with its port and host are `options`, as the
/// end of Node.js's `ClientRequest` constructor does: through `agent.addRequest`, through the
/// `createConnection` option, or with `net.createConnection`.
pub(super) fn connect(
    scope: &Scope<'_>,
    request: Object<'_>,
    agent: Option<Object<'_>>,
    options: Object<'_>,
) -> Result<(), Thrown> {
    if let Some(agent) = agent {
        let args = [
            request.to_jsval_throwing(scope)?,
            options.to_jsval_throwing(scope)?,
        ];
        Function::call_by_name(scope, agent.handle(), c"addRequest", &args)?;
        return Ok(());
    }
    let path = options.get_property(scope, c"path")?;
    let socket_path = options.get_property(scope, c"socketPath")?;
    let options = if js::conversion::to_boolean(path) || js::conversion::to_boolean(socket_path) {
        let copy = Object::new_plain(scope)?;
        copy.assign(scope, options)?;
        if js::conversion::to_boolean(socket_path) {
            copy.set_property(scope, c"path", socket_path)?;
        } else {
            copy.set_property(scope, c"path", HandleValue::undefined())?;
        }
        copy
    } else {
        options
    };
    let create = options.get_property(scope, c"createConnection")?;
    if js::function::is_callable(create) {
        let created = Function::new_callback(scope, c"", 2, on_created, request)?;
        let options = options.to_jsval_throwing(scope)?;
        match Function::call(
            scope,
            options,
            create,
            &[options, (*created).to_jsval_throwing(scope)?],
        ) {
            Ok(socket) => {
                if js::conversion::to_boolean(socket) {
                    created_socket(scope, request, HandleValue::null(), socket)?;
                }
            }
            Err(ExnThrown) => {
                let error = js::exception::take_pending_or_undefined(scope);
                created_socket(scope, request, error, HandleValue::undefined())?;
            }
        }
        return Ok(());
    }
    let socket = crate::net::connect(scope, &[options.to_jsval_throwing(scope)?])?;
    Function::call_by_name(
        scope,
        request.handle(),
        c"onSocket",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    Ok(())
}

/// The callback the `createConnection` option gets, whose payload is the request.
fn on_created(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    created_socket(scope, request, args.get(0), args.get(1))?;
    Ok(js::value::undefined())
}

/// The first result of the `createConnection` option of `request`: emit `error` in the tick queue,
/// or give the request `socket`. Later results are ignored.
fn created_socket(
    scope: &Scope<'_>,
    request: Object<'_>,
    error: HandleValue<'_>,
    socket: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = super::outgoing::state_of(scope, request)?;
    let first = with_client(&state, |client| {
        !std::mem::replace(&mut client.created, true)
    });
    if first != Some(true) {
        return Ok(());
    }
    if js::conversion::to_boolean(error) {
        return js::jobs::queue_native_tick(scope, emit_error_tick, request, &[error]);
    }
    Function::call_by_name(scope, request.handle(), c"onSocket", &[socket])?;
    Ok(())
}

/// The tick callback whose payload is a request and whose argument is an error: emit `error`.
fn emit_error_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    emit_error_event(scope, request, args.get(0))?;
    Ok(js::value::undefined())
}

/// `onSocket`: forward the errors of `socket` to `request`, and assign it or fail the request in
/// the tick queue.
pub(super) fn on_socket(
    scope: &Scope<'_>,
    request: Object<'_>,
    socket: HandleValue<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if let Ok(socket) = Object::from_value(scope, *socket) {
        if !js::conversion::to_boolean(error) {
            socket.set_property(scope, c"_httpMessage", request)?;
            let on_error = listener(scope, socket_error_listener, c"socketErrorListener", 1)?;
            crate::events::listen(scope, socket, c"on", c"error", on_error)?;
        }
    }
    js::jobs::queue_native_tick(scope, on_socket_tick, request, &[socket, error])
}

/// The tick of `onSocket`, whose payload is the request and whose arguments are the socket and
/// the error, as Node.js's `onSocketNT`: give the request the socket and write what it queued,
/// unless it was destroyed or there is an error. Then a socket the agent can reuse is freed, and
/// another is destroyed, and the request emits `error` and `close` once it closed.
fn on_socket_tick(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    let socket = args.get(0);
    let error = args.get(1);
    let state = super::outgoing::state_of(scope, request)?;
    if !state.data().destroyed && !js::conversion::to_boolean(error) {
        let socket = Object::from_value(scope, *socket).map_err(|e| e.throw(scope))?;
        tick_on_socket(scope, request, socket)?;
        Function::call_by_name(scope, request.handle(), c"_flush", js::function::EmptyArgs)?;
        return Ok(js::value::undefined());
    }
    mark_destroyed(scope, request)?;
    let kerror = with_client(&state, |client| {
        client.error.as_ref().map(|error| error.get(scope))
    })
    .flatten();
    if let Ok(socket_object) = Object::from_value(scope, *socket) {
        let agent = crate::http::flag(scope, request, c"agent")?;
        if !js::conversion::to_boolean(error)
            && agent
            && !super::wire::socket_destroyed(scope, socket_object)?
        {
            crate::events::emit(scope, socket_object, c"free", &[])?;
            remove(
                scope,
                socket_object,
                c"error",
                socket_error_listener,
                c"socketErrorListener",
            )?;
        } else {
            let reason = if js::conversion::to_boolean(error) {
                error
            } else {
                kerror.unwrap_or(HandleValue::undefined())
            };
            let destroyed =
                Function::call_by_name(scope, socket_object.handle(), c"destroy", &[reason])?;
            let stream = Object::from_value(scope, *destroyed).unwrap_or(socket_object);
            with_client(&state, |client| {
                client.socket_error = Some(Heap::from(error.get()));
                client.destroyed_socket = Some(Heap::from(socket_object));
            });
            let done = crate::stream::EosCallback::Native(socket_finished, payload);
            crate::stream::eos_native(scope, stream, crate::stream::EosOptions::default(), done)
                .map_err(|thrown| thrown.throw(scope))?;
            return Ok(js::value::undefined());
        }
    }
    let reason = if js::conversion::to_boolean(error) {
        error
    } else {
        kerror.unwrap_or(HandleValue::undefined())
    };
    fail_request(scope, request, socket, reason)?;
    Ok(js::value::undefined())
}

/// The `finished` callback of the socket `onSocket` destroyed, whose payload is the request: fail
/// the request with the socket's error, unless the socket closed prematurely, or with the error
/// `onSocket` got.
fn socket_finished(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    let state = super::outgoing::state_of(scope, request)?;
    let (error, socket) = with_client(&state, |client| {
        let error = client.socket_error.as_ref().map(|error| error.get(scope));
        client.socket_error = None;
        (error, client.destroyed_socket.take_rooted(scope))
    })
    .unwrap_or_default();
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
        error.unwrap_or(HandleValue::undefined())
    };
    let socket = match socket {
        Some(socket) => socket.to_jsval_throwing(scope)?,
        None => HandleValue::undefined(),
    };
    fail_request(scope, request, socket, error)?;
    Ok(js::value::undefined())
}

/// Emit `error` and `close` on `request`, which got no socket, as the `_destroy` of Node.js's
/// `onSocketNT` does: the error is `error`, or an `ECONNRESET` "socket hang up" error unless the
/// request was aborted. The error isn't emitted again if `socket` forwarded an error before.
fn fail_request(
    scope: &Scope<'_>,
    request: Object<'_>,
    socket: HandleValue<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let error =
        if !js::conversion::to_boolean(error) && !crate::http::flag(scope, request, c"aborted")? {
            conn_reset(scope, "socket hang up")?
        } else {
            error
        };
    if js::conversion::to_boolean(error) {
        let tunnel = match Object::from_value(scope, *error) {
            Ok(error) => crate::support::is_str(
                scope,
                error.get_property(scope, c"code")?,
                c"ERR_PROXY_TUNNEL",
            )?,
            Err(_) => false,
        };
        let forwarded = match Object::from_value(scope, *socket) {
            Ok(socket) => crate::http::flag(scope, socket, c"_hadError")?,
            Err(_) => false,
        };
        if !tunnel && !forwarded {
            emit_error_event(scope, request, error)?;
        }
    }
    close_request(scope, request)
}

/// Give `request` `socket`, as Node.js's `tickOnSocket` does: create the response parser, make
/// the socket the request's, listen to the socket, and emit `socket`.
fn tick_on_socket(
    scope: &Scope<'_>,
    request: Object<'_>,
    socket: Object<'_>,
) -> Result<(), ExnThrown> {
    let max_header_size = request.get_property(scope, c"maxHeaderSize")?;
    let max_header_size = match max_header_size.is_number() && max_header_size.to_number() > 0.0 {
        true => max_header_size.to_number().min(usize::MAX as f64) as usize,
        false => DEFAULT_MAX_HEADER_SIZE,
    };
    let lenient = super::lenient_parser(
        scope,
        request.get_property(scope, c"httpValidation")?,
        request.get_property(scope, c"insecureHTTPParser")?,
    )?;
    let max_headers = request.get_property(scope, c"maxHeadersCount")?;
    let max_headers = if max_headers.is_number() {
        let pairs = super::to_int32(max_headers.to_number()).wrapping_shl(1);
        (pairs > 0).then_some(pairs as usize / 2)
    } else {
        Some(DEFAULT_MAX_HEADERS)
    };
    let join_duplicates = crate::http::flag(scope, request, c"joinDuplicateHeaders")?;
    let parser = Parser::new(Kind::Response, max_header_size, None, lenient);
    let reused = {
        let pool = parser_pool(scope)?;
        let mut data = pool.data_mut();
        let handle = data.free.last().map(|handle| handle.get(scope));
        let remaining = data.free.len().saturating_sub(1);
        data.free.truncate(remaining);
        handle
    };
    let parser = match reused {
        Some(handle) => {
            {
                let mut data = handle.data_mut();
                data.parser = parser;
                data.incoming = None;
                data.socket = Some(Heap::from(socket));
                data.join_duplicates = join_duplicates;
                data.max_headers = max_headers;
            }
            handle
        }
        None => js::class::create_instance_with::<ParserHandleImpl>(scope, |_| ParserHandleImpl {
            parser,
            incoming: None,
            socket: Some(Heap::from(socket)),
            join_duplicates,
            max_headers,
        })?,
    };
    request.set_property(scope, c"socket", socket)?;
    request.set_property(scope, c"parser", parser)?;
    socket.set_property(scope, c"parser", parser)?;
    socket.set_property(scope, c"_httpMessage", request)?;
    for (event, body, name, nargs) in [
        (
            c"data",
            socket_on_data as js::function::Callback,
            c"socketOnData",
            1,
        ),
        (c"end", socket_on_end, c"socketOnEnd", 0),
        (c"close", socket_close_listener, c"socketCloseListener", 0),
        (c"drain", on_drain, c"ondrain", 0),
    ] {
        let function = listener(scope, body, name, nargs)?;
        crate::events::listen(scope, socket, c"on", event, function)?;
    }
    let agent_timeout = match Object::from_value(scope, *request.get_property(scope, c"agent")?) {
        Ok(agent) => match Object::from_value(scope, *agent.get_property(scope, c"options")?) {
            Ok(options) => crate::http::flag(scope, options, c"timeout")?,
            Err(_) => false,
        },
        Err(_) => false,
    };
    if !request.get_property(scope, c"timeout")?.is_undefined() || agent_timeout {
        listen_socket_timeout(scope, request)?;
    }
    crate::events::emit(
        scope,
        request,
        c"socket",
        &[socket.to_jsval_throwing(scope)?],
    )?;
    Ok(())
}

/// Clear the parser `handle` of `request` and `socket`, and keep it for reuse, as Node.js's
/// `freeParser` does.
fn free_parser(
    scope: &Scope<'_>,
    handle: Option<ParserHandle<'_>>,
    request: Option<Object<'_>>,
    socket: Option<Object<'_>>,
) -> Result<(), ExnThrown> {
    if let Some(handle) = handle {
        let freed = {
            let mut data = handle.data_mut();
            data.incoming = None;
            let freed = data.socket.is_some();
            data.socket = None;
            freed
        };
        let pool = parser_pool(scope)?;
        let mut pool = pool.data_mut();
        if freed && pool.free.len() < MAX_FREE_PARSERS {
            pool.free.push(Heap::from(handle));
        }
    }
    if let Some(request) = request {
        request.set_property(scope, c"parser", HandleValue::null())?;
    }
    if let Some(socket) = socket {
        socket.set_property(scope, c"parser", HandleValue::null())?;
    }
    Ok(())
}

/// End the message the parser `handle` parses if the connection's end ends it, as Node.js's
/// `parser.finish()` does. Errors are ignored.
fn finish_parser(scope: &Scope<'_>, handle: ParserHandle<'_>) -> Result<(), ExnThrown> {
    let finished = handle.data_mut().parser.finish();
    if let Ok(Some(Event::Complete(trailers))) = finished {
        on_message_complete(scope, handle, trailers)?;
    }
    Ok(())
}

/// The `data` listener of the socket of a request, whose `this` is the socket: parse the chunk
/// and handle what it completes, as Node.js's `socketOnData` and the parser callbacks do. A
/// response that upgrades the connection gives the request's `upgrade` or `connect` listeners the
/// socket, and a complete response frees the parser.
fn socket_on_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let (Some(request), Some(handle)) = (http_message(scope, socket)?, parser_of(scope, socket)?)
    else {
        return Ok(js::value::undefined());
    };
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
    loop {
        let event = handle.data_mut().parser.next();
        let (result, code) = match event {
            Ok(None) => break,
            Ok(Some(Event::Head(head))) => {
                let result = on_headers_complete(scope, handle, socket, head)
                    .map(|action| handle.data_mut().parser.after_head(action));
                (result, "HPE_CB_HEADERS_COMPLETE")
            }
            Ok(Some(Event::Body(body))) => (on_body(scope, handle, body), "HPE_CB_BODY"),
            Ok(Some(Event::Complete(trailers))) => (
                on_message_complete(scope, handle, trailers),
                "HPE_CB_MESSAGE_COMPLETE",
            ),
            Err(error) => {
                let error = super::connection::parse_error_value(scope, &error, chunk)?;
                free_parser(scope, Some(handle), Some(request), Some(socket))?;
                remove(scope, socket, c"data", socket_on_data, c"socketOnData")?;
                remove(scope, socket, c"end", socket_on_end, c"socketOnEnd")?;
                Function::call_by_name(
                    scope,
                    socket.handle(),
                    c"destroy",
                    js::function::EmptyArgs,
                )?;
                socket.set_property(scope, c"_hadError", true)?;
                emit_error_event(scope, request, error)?;
                return Ok(js::value::undefined());
            }
        };
        if result.is_err() {
            // As in Node.js, the exception a listener threw is uncaught, and parsing stops.
            core_runtime::event_loop::handle_and_clear_exception(scope);
            handle.data_mut().parser.fail_callback(code);
            return Ok(js::value::undefined());
        }
    }
    let response = handle.data().incoming.get(scope);
    let Some(response) = response else {
        return Ok(js::value::undefined());
    };
    if crate::http::flag(scope, response, c"upgrade")? {
        upgrade(scope, request, socket, handle, response)?;
    } else if crate::http::flag(scope, response, c"complete")?
        && !informational(status_of(scope, response)?)
    {
        remove(scope, socket, c"data", socket_on_data, c"socketOnData")?;
        remove(scope, socket, c"end", socket_on_end, c"socketOnEnd")?;
        remove(scope, socket, c"drain", on_drain, c"ondrain")?;
        free_parser(scope, Some(handle), Some(request), Some(socket))?;
    }
    Ok(js::value::undefined())
}

/// The `statusCode` of `response`, or 0 if it isn't a number.
fn status_of(scope: &Scope<'_>, response: Object<'_>) -> Result<f64, ExnThrown> {
    let status = response.get_property(scope, c"statusCode")?;
    Ok(if status.is_number() {
        status.to_number()
    } else {
        0.0
    })
}

/// Whether `status` is informational: from 100 to 199, but not 101.
fn informational(status: f64) -> bool {
    (100.0..200.0).contains(&status) && status != 101.0
}

/// Give the socket of `request` to the request's `upgrade` or `connect` listeners with the bytes
/// after the head of `response`, which upgrades the connection or is the response to a `CONNECT`
/// request, or destroy the socket without listeners.
fn upgrade(
    scope: &Scope<'_>,
    request: Object<'_>,
    socket: Object<'_>,
    handle: ParserHandle<'_>,
    response: Object<'_>,
) -> Result<(), ExnThrown> {
    request.set_property(scope, c"res", response)?;
    remove(scope, socket, c"data", socket_on_data, c"socketOnData")?;
    remove(scope, socket, c"end", socket_on_end, c"socketOnEnd")?;
    remove(scope, socket, c"drain", on_drain, c"ondrain")?;
    let timeout_listener = request.get_property(scope, c"timeoutCb")?;
    if let Ok(timeout_listener) = Object::from_value(scope, *timeout_listener) {
        crate::events::listen(
            scope,
            socket,
            c"removeListener",
            c"timeout",
            timeout_listener,
        )?;
    }
    remove(
        scope,
        socket,
        c"timeout",
        response_on_timeout,
        c"responseOnTimeout",
    )?;
    finish_parser(scope, handle)?;
    let rest = handle.data_mut().parser.take_rest();
    free_parser(scope, Some(handle), Some(request), Some(socket))?;
    let head = buffer_from_vec(scope, rest)?.to_jsval_throwing(scope)?;
    let method = request.get_property(scope, c"method")?;
    let event = if crate::support::is_str(scope, method, c"CONNECT")? {
        c"connect"
    } else {
        c"upgrade"
    };
    let listeners =
        crate::events::listener_count(scope, request, js::class::get_or_init_atom(scope, event)?)?;
    if listeners == 0.0 {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        return Ok(());
    }
    request.set_property(scope, c"upgradeOrConnect", true)?;
    crate::events::emit(scope, socket, c"agentRemove", &[])?;
    remove(
        scope,
        socket,
        c"close",
        socket_close_listener,
        c"socketCloseListener",
    )?;
    remove(
        scope,
        socket,
        c"error",
        socket_error_listener,
        c"socketErrorListener",
    )?;
    socket.set_property(scope, c"_httpMessage", HandleValue::null())?;
    socket.set_property(scope, c"readableFlowing", HandleValue::null())?;
    let args = [
        response.to_jsval_throwing(scope)?,
        socket.to_jsval_throwing(scope)?,
        head,
    ];
    crate::events::emit(scope, request, event, &args)?;
    mark_destroyed(scope, request)?;
    close_request(scope, request)
}

/// Create the response for the head `head` that the parser `handle` of `socket` parsed, as
/// Node.js's `parserOnHeadersComplete` does, and handle it, as its `parserOnIncomingClient` does.
/// Returns what follows the head.
fn on_headers_complete(
    scope: &Scope<'_>,
    handle: ParserHandle<'_>,
    socket: Object<'_>,
    head: Head,
) -> Result<HeadAction, ExnThrown> {
    let Head {
        status,
        reason,
        major,
        minor,
        mut headers,
        upgrade,
        keep_alive,
        ..
    } = head;
    let (join_duplicates, max_headers) = {
        let data = handle.data();
        (data.join_duplicates, data.max_headers)
    };
    if let Some(max) = max_headers {
        headers.truncate(max);
    }
    let fields = MessageHead {
        version: Some((u32::from(major), u32::from(minor))),
        upgrade: Some(upgrade),
        status: Some((status, &reason)),
        join_duplicates: Some(join_duplicates),
        headers: Some(headers),
        ..MessageHead::default()
    };
    let response = new_message(scope, socket.to_jsval_throwing(scope)?, fields)?;
    super::incoming::set_wire_source(scope, response, None)?;
    handle.data_mut().incoming = Some(Heap::from(response));
    on_incoming(scope, socket, response, keep_alive)
}

/// Handle the response `response` of the request of `socket`, as Node.js's
/// `parserOnIncomingClient` does: a second response destroys the socket, an informational one
/// emits `information`, and `continue` for 100, and another emits `response`. Returns what
/// follows the head.
fn on_incoming(
    scope: &Scope<'_>,
    socket: Object<'_>,
    response: Object<'_>,
    keep_alive: bool,
) -> Result<HeadAction, ExnThrown> {
    let Some(request) = http_message(scope, socket)? else {
        return Ok(HeadAction::Body);
    };
    let previous = request.get_property(scope, c"res")?;
    if let Ok(previous) = Object::from_value(scope, *previous) {
        Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
        if let Some(handle) = parser_of(scope, socket)? {
            handle.data_mut().incoming = Some(Heap::from(previous));
            if let Some(state) = super::incoming::try_state_of(scope, previous)? {
                state.data_mut().skip_pending = true;
            }
        }
        return Ok(HeadAction::Body);
    }
    request.set_property(scope, c"res", response)?;
    if crate::http::flag(scope, response, c"upgrade")? {
        return Ok(HeadAction::Upgrade);
    }
    let method = request.get_property(scope, c"method")?;
    if crate::support::is_str(scope, method, c"CONNECT")? {
        response.set_property(scope, c"upgrade", true)?;
        return Ok(HeadAction::Upgrade);
    }
    let status = status_of(scope, response)?;
    if informational(status) {
        request.set_property(scope, c"res", HandleValue::null())?;
        if status == 100.0 {
            crate::events::emit(scope, request, c"continue", &[])?;
        }
        let mut values = Vec::with_capacity(INFORMATION_PROPERTIES.len());
        for name in [
            c"statusCode",
            c"statusMessage",
            c"httpVersion",
            c"httpVersionMajor",
            c"httpVersionMinor",
            c"headers",
            c"rawHeaders",
        ] {
            values.push(response.get_property(scope, name)?);
        }
        let information = Object::new_plain_with_template(scope, &INFORMATION_PROPERTIES, &values)?;
        crate::events::emit(
            scope,
            request,
            c"information",
            &[information.to_jsval_throwing(scope)?],
        )?;
        return Ok(HeadAction::SkipBody);
    }
    if crate::http::flag(scope, request, c"shouldKeepAlive")?
        && !keep_alive
        && !crate::http::flag(scope, request, c"upgradeOrConnect")?
    {
        request.set_property(scope, c"shouldKeepAlive", false)?;
    }
    request.set_property(scope, c"res", response)?;
    response.set_property(scope, c"req", request)?;
    let on_end = listener(scope, response_on_end, c"responseOnEnd", 0)?;
    crate::events::listen(scope, response, c"on", c"end", on_end)?;
    let on_finish = listener(scope, request_on_finish, c"requestOnFinish", 0)?;
    crate::events::listen(scope, request, c"on", c"finish", on_finish)?;
    let on_timeout = listener(scope, response_on_timeout, c"responseOnTimeout", 0)?;
    crate::events::listen(scope, socket, c"on", c"timeout", on_timeout)?;
    let listened = !crate::http::flag(scope, request, c"aborted")?
        && crate::events::emit(
            scope,
            request,
            c"response",
            &[response.to_jsval_throwing(scope)?],
        )?;
    if !listened {
        Function::call_by_name(scope, response.handle(), c"_dump", js::function::EmptyArgs)?;
    }
    if crate::support::is_str(scope, method, c"HEAD")? {
        return Ok(HeadAction::SkipBody);
    }
    if status == 304.0 {
        response.set_property(scope, c"complete", true)?;
        return Ok(HeadAction::SkipBody);
    }
    Ok(HeadAction::Body)
}

/// Push the body bytes `body` onto the response the parser `handle` parses, unless it was dumped
/// or its pending data is skipped, and pause the socket if the response's buffer is full, as
/// Node.js's `parserOnBody` does.
fn on_body(scope: &Scope<'_>, handle: ParserHandle<'_>, body: Vec<u8>) -> Result<(), ExnThrown> {
    let (response, socket) = {
        let data = handle.data();
        (data.incoming.get(scope), data.socket.get(scope))
    };
    let Some(response) = response else {
        return Ok(());
    };
    if skips_pending(scope, response)? || crate::http::flag(scope, response, c"_dumped")? {
        return Ok(());
    }
    let chunk = buffer_from_vec(scope, body)?.to_jsval_throwing(scope)?;
    let more = Function::call_by_name(scope, response.handle(), c"push", &[chunk])?;
    if !js::conversion::to_boolean(more) {
        if let Some(socket) = socket {
            Function::call_by_name(scope, socket.handle(), c"pause", js::function::EmptyArgs)?;
        }
    }
    Ok(())
}

/// Whether the parser drops the rest of the body of `response`.
fn skips_pending(scope: &Scope<'_>, response: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(super::incoming::try_state_of(scope, response)?
        .is_some_and(|state| state.data().skip_pending))
}

/// End the response the parser `handle` parses with the trailers `trailers`, unless its pending
/// data is skipped, and resume the socket, as Node.js's `parserOnMessageComplete` does.
fn on_message_complete(
    scope: &Scope<'_>,
    handle: ParserHandle<'_>,
    trailers: Vec<(String, String)>,
) -> Result<(), ExnThrown> {
    let (response, socket) = {
        let data = handle.data();
        (data.incoming.get(scope), data.socket.get(scope))
    };
    if let Some(response) = response {
        if !skips_pending(scope, response)? {
            response.set_property(scope, c"complete", true)?;
            if !trailers.is_empty() {
                super::incoming::set_lines(scope, response, trailers, true)?;
            }
            Function::call_by_name(scope, response.handle(), c"push", &[HandleValue::null()])?;
        }
    }
    if let Some(socket) = socket {
        super::incoming::read_start_socket(scope, socket)?;
    }
    Ok(())
}

/// The `end` listener of the socket of a request, whose `this` is the socket: fail a request
/// without a response with `ECONNRESET`, end the response the connection's end ends, and destroy
/// the socket, as Node.js's `socketOnEnd` does.
fn socket_on_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let request = http_message(scope, socket)?;
    if let Some(request) = request {
        let request_socket = request.get_property(scope, c"socket")?;
        if !crate::http::flag(scope, request, c"res")? && !had_error(scope, request_socket)? {
            emit_error_event(scope, request, conn_reset(scope, "socket hang up")?)?;
        }
    }
    if let Some(handle) = parser_of(scope, socket)? {
        finish_parser(scope, handle)?;
        free_parser(scope, Some(handle), request, Some(socket))?;
    }
    Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// The `close` listener of the socket of a request, whose `this` is the socket, as Node.js's
/// `socketCloseListener`: destroy an incomplete response with `ECONNRESET`, or fail a request
/// without a response with it, and emit `close` on the request.
fn socket_close_listener(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let Some(request) = http_message(scope, socket)? else {
        return Ok(js::value::undefined());
    };
    let handle = parser_of(scope, socket)?;
    let response = Object::from_value(scope, *request.get_property(scope, c"res")?).ok();
    mark_destroyed(scope, request)?;
    match response {
        Some(response) => {
            if !crate::http::flag(scope, response, c"complete")? {
                let error = conn_reset(scope, "aborted")?;
                Function::call_by_name(scope, response.handle(), c"destroy", &[error])?;
            }
            close_request(scope, request)?;
            if !crate::http::flag(scope, response, c"aborted")?
                && crate::http::flag(scope, response, c"readable")?
            {
                Function::call_by_name(scope, response.handle(), c"push", &[HandleValue::null()])?;
            }
        }
        None => {
            let request_socket = request.get_property(scope, c"socket")?;
            if !had_error(scope, request_socket)? {
                emit_error_event(scope, request, conn_reset(scope, "socket hang up")?)?;
            }
            close_request(scope, request)?;
        }
    }
    let state = super::outgoing::state_of(scope, request)?;
    {
        let mut data = state.data_mut();
        data.output.clear();
        data.output_size = 0.0;
    }
    if let Some(handle) = handle {
        finish_parser(scope, handle)?;
        free_parser(scope, Some(handle), Some(request), Some(socket))?;
    }
    Ok(js::value::undefined())
}

/// The `error` listener of the socket of a request, whose `this` is the socket, as Node.js's
/// `socketErrorListener`: emit the error on the request, free the parser, stop reading and
/// destroy the socket.
fn socket_error_listener(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let request = http_message(scope, socket)?;
    if let Some(request) = request {
        socket.set_property(scope, c"_hadError", true)?;
        emit_error_event(scope, request, args.get(0))?;
    }
    if let Some(handle) = parser_of(scope, socket)? {
        finish_parser(scope, handle)?;
        free_parser(scope, Some(handle), request, Some(socket))?;
    }
    remove(scope, socket, c"data", socket_on_data, c"socketOnData")?;
    remove(scope, socket, c"end", socket_on_end, c"socketOnEnd")?;
    Function::call_by_name(scope, socket.handle(), c"destroy", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// The `drain` listener of the socket of a request, whose `this` is the socket: emit `drain` on
/// the request if it waits for one and didn't end.
fn on_drain(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let Some(message) = http_message(scope, socket)? else {
        return Ok(js::value::undefined());
    };
    let Some(state) = super::outgoing::try_state_of(scope, message)? else {
        return Ok(js::value::undefined());
    };
    if !crate::http::flag(scope, message, c"finished")?
        && std::mem::take(&mut state.data_mut().need_drain)
    {
        crate::events::emit(scope, message, c"drain", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `timeout` listener of the socket of a request with a response, whose `this` is the
/// socket: emit `timeout` on the response.
fn response_on_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    if let Some(request) = http_message(scope, socket)? {
        if let Ok(response) = Object::from_value(scope, *request.get_property(scope, c"res")?) {
            crate::events::emit(scope, response, c"timeout", &[])?;
        }
    }
    Ok(js::value::undefined())
}

/// The `timeout` listener of the socket of a request with a timeout, whose `this` is the socket:
/// emit `timeout` on the request.
fn emit_request_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    if let Some(request) = http_message(scope, socket)? {
        crate::events::emit(scope, request, c"timeout", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `end` listener of a response, whose `this` is the response, as Node.js's `responseOnEnd`:
/// close the connection unless the request keeps it alive, or return the socket to the agent
/// once the request finished.
fn response_on_end(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let response = crate::support::this_object(scope, &args)?;
    let Ok(request) = Object::from_value(scope, *response.get_property(scope, c"req")?) else {
        return Ok(js::value::undefined());
    };
    super::incoming::detach_abort_signal(scope, response)?;
    let state = super::outgoing::state_of(scope, request)?;
    let socket = socket_of(scope, &state);
    if let Some(socket) = socket {
        if crate::http::flag(scope, request, c"timeoutCb")? {
            remove(
                scope,
                socket,
                c"timeout",
                emit_request_timeout,
                c"emitRequestTimeout",
            )?;
        }
        remove(
            scope,
            socket,
            c"timeout",
            response_on_timeout,
            c"responseOnTimeout",
        )?;
    }
    request.set_property(scope, c"_ended", true)?;
    if !crate::http::flag(scope, request, c"shouldKeepAlive")? {
        if let Some(socket) = socket {
            if super::wire::socket_writable(scope, socket)? {
                let method = if crate::support::has_method(scope, socket, c"destroySoon")? {
                    c"destroySoon"
                } else {
                    c"end"
                };
                Function::call_by_name(scope, socket.handle(), method, js::function::EmptyArgs)?;
            }
        }
    } else if crate::http::flag(scope, request, c"writableFinished")?
        && !crate::http::flag(scope, response, c"aborted")?
    {
        response_keep_alive(scope, request)?;
    }
    Ok(js::value::undefined())
}

/// Release the socket of `request` for its agent once the tick queue runs, as Node.js's
/// `responseKeepAlive` does: stop listening to the socket, mark the request destroyed, and detach
/// the response from the socket.
fn response_keep_alive(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    let state = super::outgoing::state_of(scope, request)?;
    let Some(socket) = socket_of(scope, &state) else {
        return Ok(());
    };
    let timeout_listener = request.get_property(scope, c"timeoutCb")?;
    if js::conversion::to_boolean(timeout_listener) {
        let zero = scope.root_value(js::value::from_f64(0.0));
        Function::call_by_name(
            scope,
            socket.handle(),
            c"setTimeout",
            &[zero, timeout_listener],
        )?;
        request.set_property(scope, c"timeoutCb", HandleValue::null())?;
    }
    remove(
        scope,
        socket,
        c"close",
        socket_close_listener,
        c"socketCloseListener",
    )?;
    remove(
        scope,
        socket,
        c"error",
        socket_error_listener,
        c"socketErrorListener",
    )?;
    remove(scope, socket, c"data", socket_on_data, c"socketOnData")?;
    remove(scope, socket, c"end", socket_on_end, c"socketOnEnd")?;
    js::jobs::queue_native_tick(scope, emit_free_tick, request, &[])?;
    mark_destroyed(scope, request)?;
    if let Ok(response) = Object::from_value(scope, *request.get_property(scope, c"res")?) {
        response.set_property(scope, c"socket", HandleValue::null())?;
    }
    Ok(())
}

/// The tick callback whose payload is a request whose socket is released: emit `close` on the
/// request and `free` on the socket.
fn emit_free_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = js::function::cast_payload::<js::Object>(scope, payload);
    close_request(scope, request)?;
    let state = super::outgoing::state_of(scope, request)?;
    if let Some(socket) = socket_of(scope, &state) {
        crate::events::emit(scope, socket, c"free", &[])?;
    }
    Ok(js::value::undefined())
}

/// The `finish` listener of a request with a response, whose `this` is the request: release the
/// socket if the response ended before the request finished.
fn request_on_finish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let request = crate::support::this_object(scope, &args)?;
    let destroyed = super::outgoing::state_of(scope, request)?.data().destroyed;
    if crate::http::flag(scope, request, c"shouldKeepAlive")?
        && crate::http::flag(scope, request, c"_ended")?
        && !destroyed
    {
        response_keep_alive(scope, request)?;
    }
    Ok(js::value::undefined())
}

/// `destroy` of a request that writes to a socket: mark it destroyed, dump its response, keep
/// `error`, and destroy the socket with it.
pub(super) fn destroy(
    scope: &Scope<'_>,
    request: Object<'_>,
    state: &OutgoingState<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    if state.data().destroyed {
        return Ok(());
    }
    mark_destroyed(scope, request)?;
    if let Ok(response) = Object::from_value(scope, *request.get_property(scope, c"res")?) {
        Function::call_by_name(scope, response.handle(), c"_dump", js::function::EmptyArgs)?;
    }
    let error = error.unwrap_or(HandleValue::undefined());
    with_client(state, |client| client.error = Some(Heap::from(error.get())));
    if let Some(socket) = socket_of(scope, state) {
        Function::call_by_name(scope, socket.handle(), c"destroy", &[error])?;
    }
    Ok(())
}

/// Listen to `timeout` on the socket of `request`, now or once it has one, to emit `timeout` on
/// the request, unless that listener was added, as Node.js's `listenSocketTimeout` does.
fn listen_socket_timeout(scope: &Scope<'_>, request: Object<'_>) -> Result<(), ExnThrown> {
    if crate::http::flag(scope, request, c"timeoutCb")? {
        return Ok(());
    }
    let timeout_listener = listener(scope, emit_request_timeout, c"emitRequestTimeout", 0)?;
    request.set_property(scope, c"timeoutCb", timeout_listener)?;
    let state = super::outgoing::state_of(scope, request)?;
    match socket_of(scope, &state) {
        Some(socket) => crate::events::listen(scope, socket, c"once", c"timeout", timeout_listener),
        None => {
            let on_socket = listener(scope, listen_timeout_on_socket, c"", 1)?;
            crate::events::listen(scope, request, c"on", c"socket", on_socket)
        }
    }
}

/// The `socket` listener of [`listen_socket_timeout`]: listen to `timeout` on the socket.
fn listen_timeout_on_socket(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    let timeout_listener = listener(scope, emit_request_timeout, c"emitRequestTimeout", 0)?;
    crate::events::listen(scope, socket, c"once", c"timeout", timeout_listener)?;
    Ok(js::value::undefined())
}

/// `setTimeout` of a request that writes to a socket, as Node.js's: listen to the socket's
/// `timeout`, add `callback` as a `timeout` listener, and set the timeout of the socket now or
/// once the request has one. A request whose response ended is left alone.
pub(super) fn set_timeout(
    scope: &Scope<'_>,
    request: Object<'_>,
    msecs: HandleValue<'_>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    if crate::http::flag(scope, request, c"_ended")? {
        return Ok(());
    }
    listen_socket_timeout(scope, request)?;
    let msecs = crate::timers::get_timer_duration(scope, msecs, "msecs")?;
    if let Some(callback) = callback.filter(|callback| js::conversion::to_boolean(*callback)) {
        let timeout = js::class::get_or_init_atom(scope, c"timeout")?;
        Function::call_by_name(scope, request.handle(), c"once", &[timeout, callback])?;
    }
    let state = super::outgoing::state_of(scope, request)?;
    match socket_of(scope, &state) {
        Some(socket) => set_socket_timeout(scope, socket, msecs)?,
        None => {
            let set = Function::new_callback(scope, c"", 1, set_timeout_on_socket, msecs)?;
            crate::events::listen(scope, request, c"once", c"socket", *set)?;
        }
    }
    Ok(())
}

/// The `socket` listener of `setTimeout`, whose payload is the timeout: set the timeout of the
/// socket.
fn set_timeout_on_socket(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    set_socket_timeout(scope, socket, payload.to_number())?;
    Ok(js::value::undefined())
}

/// Set the timeout of `socket` to `msecs`, once it connected if it is connecting.
fn set_socket_timeout(scope: &Scope<'_>, socket: Object<'_>, msecs: f64) -> Result<(), ExnThrown> {
    if crate::http::flag(scope, socket, c"connecting")? {
        let set = Function::new_callback(scope, c"", 0, set_timeout_on_connect, msecs)?;
        return crate::events::listen(scope, socket, c"once", c"connect", *set);
    }
    let msecs = scope.root_value(js::value::from_f64(msecs));
    Function::call_by_name(scope, socket.handle(), c"setTimeout", &[msecs])?;
    Ok(())
}

/// The `connect` listener of [`set_socket_timeout`], whose `this` is the socket and whose payload
/// is the timeout.
fn set_timeout_on_connect(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    Function::call_by_name(scope, socket.handle(), c"setTimeout", &[payload])?;
    Ok(js::value::undefined())
}

/// `_deferToConnect`: call the method `method` of the socket of `request` with the elements of
/// `args` once the request has a socket and the socket is writable.
pub(super) fn defer_to_connect(
    scope: &Scope<'_>,
    request: Object<'_>,
    method: HandleValue<'_>,
    args: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let call = js::class::create_instance_with::<DeferredCallImpl>(scope, |_| DeferredCallImpl {
        request: Heap::from(request),
        method: Heap::from(method.get()),
        args: Heap::from(args.get()),
    })?;
    let state = super::outgoing::state_of(scope, request)?;
    match socket_of(scope, &state) {
        Some(socket) => deferred_on_socket(scope, call, socket),
        None => {
            let on_socket = Function::new_callback(scope, c"", 0, deferred_socket, call)?;
            crate::events::listen(scope, request, c"once", c"socket", *on_socket)
        }
    }
}

/// The `socket` listener of `_deferToConnect`, whose payload is the deferred call.
fn deferred_socket(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let call = js::function::cast_payload::<DeferredCall>(scope, payload);
    let request = call.data().request.get(scope);
    let state = super::outgoing::state_of(scope, request)?;
    if let Some(socket) = socket_of(scope, &state) {
        deferred_on_socket(scope, call, socket)?;
    }
    Ok(js::value::undefined())
}

/// Make the deferred call `call` on `socket` now if it is writable, and once it connects
/// otherwise.
fn deferred_on_socket(
    scope: &Scope<'_>,
    call: DeferredCall<'_>,
    socket: Object<'_>,
) -> Result<(), ExnThrown> {
    if super::wire::socket_writable(scope, socket)? {
        return call_deferred(scope, call, socket);
    }
    let on_connect = Function::new_callback(scope, c"", 0, deferred_connect, call)?;
    crate::events::listen(scope, socket, c"once", c"connect", *on_connect)
}

/// The `connect` listener of `_deferToConnect`, whose `this` is the socket and whose payload is
/// the deferred call.
fn deferred_connect(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::support::this_object(scope, &args)?;
    let call = js::function::cast_payload::<DeferredCall>(scope, payload);
    call_deferred(scope, call, socket)?;
    Ok(js::value::undefined())
}

/// Call the method of the deferred call `call` on `socket` with its arguments, if it has a method.
fn call_deferred(
    scope: &Scope<'_>,
    call: DeferredCall<'_>,
    socket: Object<'_>,
) -> Result<(), ExnThrown> {
    let (method, args) = {
        let data = call.data();
        (data.method.get(scope), data.args.get(scope))
    };
    if !js::conversion::to_boolean(method) {
        return Ok(());
    }
    let key = scope.root_id(js::id::value_to_id(scope, method)?);
    let function = socket.get_property_by_id(scope, key)?;
    let args = crate::support::array_elements(scope, args)?.unwrap_or_default();
    Function::call(scope, socket.to_jsval_throwing(scope)?, function, &args)?;
    Ok(())
}
