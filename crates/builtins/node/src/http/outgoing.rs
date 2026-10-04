// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `OutgoingMessage` and its subclass `ServerResponse`, which responds to a served request with a
//! web `Response`, or writes HTTP/1.1 to a socket (see [`super::wire`]).
//!
//! Headers are kept in a null-prototype object under `_starlingHeaders`, mapping each lowercase
//! name to the pair of the name as set and the value. `write` and `end` pass their bytes to the
//! subclass's `_starlingSend(chunk, last)`, except for a response that writes to a socket.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::{HandleId, HandleObject, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use super::{has_invalid_header_char, status_text, validate_name, validate_value};
use crate::buffer::{encode_string, new_buffer, Encoding};
use crate::errors::{validate_string, ErrorKind, NodeError, Thrown};
use crate::stream::{
    call_method, closure, is_function, next_tick_call, string, truthy, Captures, Props,
};

/// The headers that framing and connection management own, which the host sets itself.
pub(super) const HOST_HEADERS: [&str; 4] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
];

/// The number of bytes a response body stream queues before `write` returns `false`.
pub(super) const HIGH_WATER_MARK: f64 = 16384.0;

/// A new object without a prototype.
pub(super) fn null_object<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let object = Object::new_plain(scope)?;
    object.set_prototype(scope, HandleObject::null())?;
    Ok(object)
}

/// The elements of the array `array`.
pub(super) fn elements<'s>(
    scope: &'s Scope<'_>,
    array: Object<'_>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let length = array
        .cast::<js::Array>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?
        .length(scope)?;
    (0..length)
        .map(|index| array.get_element(scope, index))
        .collect()
}

/// `value` as an array, if it is one.
pub(super) fn as_array<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object) if js::Array::is_array(scope, object.handle())? => Ok(Some(object)),
        _ => Ok(None),
    }
}

/// The enumerable own string keys of `object`, as `Object.keys` returns them.
pub(super) fn object_keys<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    object.own_enumerable_keys(scope)
}

/// The header store of `message`, if a header was ever set.
pub(super) fn existing_store<'s>(
    scope: &'s Scope<'_>,
    message: Object<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    message.obj(scope, c"_starlingHeaders")
}

/// The header store of `message`, created on first use.
fn header_store<'s>(scope: &'s Scope<'_>, message: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(store) = existing_store(scope, message)? {
        return Ok(store);
    }
    let store = null_object(scope)?;
    message.define_property(scope, c"_starlingHeaders", store, 0)?;
    Ok(store)
}

/// The header store key for the header name `name`.
fn key<'s>(scope: &'s Scope<'_>, name: &str) -> Result<HandleId<'s>, ExnThrown> {
    let name = string(scope, &name.to_lowercase())?;
    Ok(scope.root_id(js::id::value_to_id(scope, name)?))
}

/// The store entry of the header `name` of `message`, if it is set.
fn entry_of<'s>(
    scope: &'s Scope<'_>,
    message: Object<'_>,
    name: &str,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let Some(store) = existing_store(scope, message)? else {
        return Ok(None);
    };
    let entry = store.get_property_by_id(scope, key(scope, name)?)?;
    Ok(Object::from_value(scope, entry).ok())
}

/// The headers of `message` as (name, value) pairs in the order they were set, with array values
/// expanded and every value converted to a string.
pub(super) fn header_pairs(
    scope: &Scope<'_>,
    message: Object<'_>,
) -> Result<Vec<(String, String)>, ExnThrown> {
    let mut pairs = Vec::new();
    for (name, value) in header_entries(scope, message)? {
        let values = match as_array(scope, value)? {
            Some(array) => elements(scope, array)?,
            None => vec![value],
        };
        for value in values {
            pairs.push((name.clone(), String::from_jsval_throwing(scope, value, ())?));
        }
    }
    Ok(pairs)
}

/// The headers of `message` as (name, value) pairs, where a value can be an array: the headers set
/// in the order they were set, then the headers `writeHead` was given without any header set
/// before.
pub(super) fn header_entries<'s>(
    scope: &'s Scope<'_>,
    message: Object<'_>,
) -> Result<Vec<(String, HandleValue<'s>)>, ExnThrown> {
    let mut entries = Vec::new();
    if let Some(store) = existing_store(scope, message)? {
        for key in store.own_keys(scope)? {
            let entry = Object::from_value(scope, crate::http::get_by_key(scope, store, key)?)
                .map_err(|e| e.throw(scope))?;
            let name =
                String::from_jsval(scope, entry.get_element(scope, 0)?, ()).unwrap_or_default();
            entries.push((name, entry.get_element(scope, 1)?));
        }
    }
    if let Some(head) = message.obj(scope, c"_starlingHeadHeaders")? {
        for entry in elements(scope, head)? {
            let entry = Object::from_value(scope, *entry).map_err(|e| e.throw(scope))?;
            let name =
                String::from_jsval(scope, entry.get_element(scope, 0)?, ()).unwrap_or_default();
            entries.push((name, entry.get_element(scope, 1)?));
        }
    }
    Ok(entries)
}

/// The headers `headers` that `writeHead` was given without any header set before, as an array of
/// `[name, value]` pairs, after checking each name and value as `setHeader` does: from an object,
/// a flat array of names and values, or an array of pairs.
fn head_headers<'s>(scope: &'s Scope<'_>, headers: HandleValue<'_>) -> Result<Object<'s>, Thrown> {
    let mut pairs = Vec::new();
    if let Some(array) = as_array(scope, headers)? {
        let items = elements(scope, array)?;
        match items.first() {
            Some(first) if as_array(scope, *first)?.is_some() => {
                for item in &items {
                    let pair = Object::from_value(scope, **item)
                        .map_err(|e| Thrown::from(e.throw(scope)))?;
                    pairs.push((pair.get_element(scope, 0)?, pair.get_element(scope, 1)?));
                }
            }
            _ => {
                if items.len() % 2 != 0 {
                    return Err(NodeError::invalid_arg_value(
                        scope,
                        "headers",
                        headers,
                        "is invalid",
                    )
                    .into());
                }
                pairs.extend(items.chunks(2).map(|pair| (pair[0], pair[1])));
            }
        }
    } else if let Ok(object) = Object::from_value(scope, *headers) {
        for key in object_keys(scope, object)? {
            pairs.push((key, crate::http::get_by_key(scope, object, key)?));
        }
    }
    let mut entries = Vec::with_capacity(pairs.len());
    for (name, value) in pairs {
        let text = validate_name(scope, name)?;
        validate_value(scope, &text, value)?;
        let entry = js::Array::with_contents(
            scope,
            &[
                js::JSString::from_str(scope, &text)?.to_jsval_throwing(scope)?,
                value,
            ],
        )?;
        entries.push((*entry).to_jsval_throwing(scope)?);
    }
    Ok(*js::Array::with_contents(scope, &entries)?)
}

/// `ERR_HTTP_HEADERS_SENT` for `action`, such as "set".
fn headers_sent(action: &str) -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_HTTP_HEADERS_SENT",
        format!("Cannot {action} headers after they are sent to the client"),
    )
}

/// The bytes of the chunk `chunk`, a string in `encoding` or a `Buffer` or `Uint8Array`, or
/// `ERR_UNKNOWN_ENCODING` for an encoding name Node.js doesn't know.
pub(super) fn chunk_bytes(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
) -> Result<Vec<u8>, Thrown> {
    if chunk.is_string() {
        let encoding = match encoding.filter(|encoding| encoding.is_string()) {
            Some(encoding) => {
                let name = String::from_jsval(scope, encoding, ())?;
                Encoding::parse(&name).ok_or_else(|| {
                    NodeError::new(
                        ErrorKind::TypeError,
                        "ERR_UNKNOWN_ENCODING",
                        format!("Unknown encoding: {name}"),
                    )
                })?
            }
            None => Encoding::default(),
        };
        return Ok(encode_string(
            scope,
            js::JSString::from_value(scope, chunk)?,
            encoding,
        )?);
    }
    Object::from_value(scope, *chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
        .map(|view| view.copy_bytes(scope))
        .transpose()?
        .ok_or_else(|| {
            NodeError::invalid_arg_type(
                scope,
                "chunk",
                "of type string or an instance of Buffer or Uint8Array",
                chunk,
            )
            .into()
        })
}

/// Pass `bytes` to `message`'s `_starlingSend`, as the last chunk if `last`, and return its
/// result.
fn send<'s>(
    scope: &'s Scope<'_>,
    message: Object<'_>,
    bytes: &[u8],
    last: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let chunk = new_buffer(scope, bytes)?.to_jsval_throwing(scope)?;
    let last = scope.root_value(js::value::from_bool(last));
    call_method(scope, message, c"_starlingSend", &[chunk, last])
}

/// Call `_implicitHeader` on `message`, if it has one and no header was sent yet.
fn implicit_header(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    if !message.flag(scope, c"_headerSent")?
        && crate::stream::has_method(scope, message, c"_implicitHeader")?
    {
        call_method(scope, message, c"_implicitHeader", &[])?;
    }
    Ok(())
}

/// The tick callback that reports a write after `end`, whose captures are `[message, error,
/// callback]`: call the callback with the error, and emit it as `error` unless the message was
/// destroyed.
fn emit_write_error(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let message = captures.obj(0)?;
    let error = captures.get(1)?;
    let callback = captures.get(2)?;
    if is_function(scope, callback) {
        Function::call(scope, js::value::undefined(), callback, &[error])?;
    }
    if !message.flag(scope, c"destroyed")? {
        call_method(scope, message, c"emit", &[string(scope, "error")?, error])?;
    }
    Ok(js::value::undefined())
}

/// Report `ERR_STREAM_WRITE_AFTER_END` for `message` to `callback` in the tick queue, and as an
/// `error` event unless the message was destroyed.
fn write_after_end(
    scope: &Scope<'_>,
    message: Object<'_>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let error = crate::stream::error_value(
        scope,
        NodeError::new(
            ErrorKind::Error,
            "ERR_STREAM_WRITE_AFTER_END",
            "write after end",
        ),
    )?;
    let message = message.to_jsval_throwing(scope)?;
    let report = closure(
        scope,
        0,
        emit_write_error,
        &[
            message.get(),
            error.get(),
            callback.map_or(js::value::undefined(), |callback| callback.get()),
        ],
    )?;
    js::jobs::queue_tick(scope, report.to_jsval_throwing(scope)?, &[])
}

/// The body of the tick callback that finishes `message`, whose payload is the message: mark it
/// finished and emit `finish`, then, for the response to a served request, emit `close` and read
/// the rest of the request if nothing reads it.
fn finish(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    message.set_property(scope, c"writableFinished", true)?;
    call_method(scope, message, c"emit", &[string(scope, "finish")?])?;
    let served = message.has_own_property(scope, c"_starlingRespond")?;
    if let Some(request) = message.obj(scope, c"req")?.filter(|_| served) {
        emit_close(scope, message)?;
        if !request.flag(scope, c"_consuming")?
            && crate::stream::has_method(scope, request, c"_dump")?
        {
            call_method(scope, request, c"_dump", &[])?;
        }
    }
    Ok(js::value::undefined())
}

/// The tick callback whose payload is a message: mark it destroyed and emit `close` on it, unless
/// it already emitted `close`.
pub(super) fn emit_close_job(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    emit_close(scope, message)?;
    Ok(js::value::undefined())
}

/// Mark `message` destroyed and emit `close` on it, unless it already emitted `close`.
pub(super) fn emit_close(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    if message.flag(scope, c"_closed")? {
        return Ok(());
    }
    message.set_property(scope, c"_closed", true)?;
    message.set_property(scope, c"destroyed", true)?;
    call_method(scope, message, c"emit", &[string(scope, "close")?])?;
    Ok(())
}

/// Remove the header `name`, a validated string, from `message`. Removing `Connection`,
/// `Content-Length` or `Transfer-Encoding` keeps the header block from adding it, and removing
/// `Date` clears `sendDate`.
fn remove_named(scope: &Scope<'_>, message: Object<'_>, name: &str) -> Result<(), Thrown> {
    if message.flag(scope, c"_headerSent")? {
        return Err(headers_sent("remove").into());
    }
    match name.to_lowercase().as_str() {
        "connection" => message.set_property(scope, c"_removedConnection", true)?,
        "content-length" => message.set_property(scope, c"_removedContLen", true)?,
        "transfer-encoding" => message.set_property(scope, c"_removedTE", true)?,
        "date" => message.set_property(scope, c"sendDate", false)?,
        _ => {}
    }
    if let Some(store) = existing_store(scope, message)? {
        store.delete_property_by_id(scope, key(scope, name)?)?;
    }
    Ok(())
}

/// Set the headers of `message` from `headers`, as `writeHead` does: from an object, a flat
/// array of names and values, or, if `message` has no headers yet, an array of `[name, value]`
/// pairs. A flat array replaces the headers it names, and keeps its own duplicates.
fn apply_headers(
    scope: &Scope<'_>,
    message: Object<'_>,
    headers: HandleValue<'_>,
) -> Result<(), Thrown> {
    if let Some(array) = as_array(scope, headers)? {
        let items = elements(scope, array)?;
        let progressive = existing_store(scope, message)?.is_some();
        let pairs = match items.first() {
            Some(first) if !progressive && as_array(scope, *first)?.is_some() => {
                let mut pairs = Vec::new();
                for item in &items {
                    let pair = Object::from_value(scope, **item)
                        .map_err(|e| Thrown::from(e.throw(scope)))?;
                    pairs.push((pair.get_element(scope, 0)?, pair.get_element(scope, 1)?));
                }
                pairs
            }
            _ => {
                if items.len() % 2 != 0 {
                    return Err(NodeError::invalid_arg_value(
                        scope,
                        "headers",
                        headers,
                        "is invalid",
                    )
                    .into());
                }
                if progressive {
                    for name in items.iter().step_by(2) {
                        outgoing_message_class::remove_header(scope, message, *name)?;
                    }
                }
                items
                    .chunks(2)
                    .filter(|pair| truthy(scope, pair[0]))
                    .map(|pair| (pair[0], pair[1]))
                    .collect()
            }
        };
        for (name, value) in pairs {
            outgoing_message_class::append_header(scope, message, name, value)?;
        }
        return Ok(());
    }
    if !truthy(scope, headers) {
        return Ok(());
    }
    let headers = Object::from_value(scope, *headers).map_err(|e| Thrown::from(e.throw(scope)))?;
    for key in object_keys(scope, headers)? {
        if truthy(scope, key) {
            let value = crate::http::get_by_key(scope, headers, key)?;
            outgoing_message_class::set_header(scope, message, key, value)?;
        }
    }
    Ok(())
}

#[core_runtime::jsfunction_class(name = "OutgoingMessage", extends = "crate::stream::stream_class")]
pub mod outgoing_message_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpoutgoingmessage>
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        crate::stream::stream_class::init(scope, this, None)?;
        for (name, value) in [
            (c"_headerSent", false),
            (c"finished", false),
            (c"writableEnded", false),
            (c"writableFinished", false),
            (c"writableNeedDrain", false),
            (c"sendDate", false),
            (c"destroyed", false),
            (c"writable", true),
            (c"_last", false),
            (c"chunkedEncoding", false),
            (c"shouldKeepAlive", true),
            (c"maxRequestsOnConnectionReached", false),
            (c"_defaultKeepAlive", true),
            (c"useChunkedEncodingByDefault", true),
            (c"_removedConnection", false),
            (c"_removedContLen", false),
            (c"_removedTE", false),
            (c"strictContentLength", false),
            (c"_hasBody", true),
            (c"_closed", false),
        ] {
            this.set_property(scope, name, value)?;
        }
        this.set_property(scope, c"outputData", *js::Array::new(scope, 0)?)?;
        this.set_property(scope, c"outputSize", 0)?;
        this.set_property(scope, c"_contentLength", js::value::null())?;
        this.set_property(scope, c"_trailer", "")?;
        this.set_property(scope, c"_header", js::value::null())?;
        this.set_property(scope, c"_keepAliveTimeout", 0)?;
        this.set_property(scope, c"socket", js::value::null())?;
        this.set_property(scope, c"connection", js::value::null())?;
        this.define_property(scope, c"_starlingErrored", js::value::null(), 0)?;
        this.define_property(scope, c"_starlingCorked", 0, 0)?;
        this.define_property(scope, c"_starlingHeaderPending", false, 0)?;
        this.define_property(scope, c"_starlingBytesWritten", 0, 0)?;
        this.define_property(scope, c"_starlingWire", true, 0)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagesetheadername-value>
    pub fn set_header<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        name: HandleValue<'_>,
        value: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        if this.flag(scope, c"_headerSent")? {
            return Err(headers_sent("set").into());
        }
        let name = validate_name(scope, name)?;
        validate_value(scope, &name, value)?;
        let entry = js::Array::with_contents(
            scope,
            &[
                js::JSString::from_str(scope, &name)?.to_jsval_throwing(scope)?,
                scope.root_value(value.get()),
            ],
        )?;
        header_store(scope, this)?.set_property_by_id(scope, key(scope, &name)?, *entry)?;
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
        if this.flag(scope, c"_headerSent")? {
            return Err(headers_sent("set").into());
        }
        let map_like = match Object::from_value(scope, *headers) {
            Ok(object) if !js::Array::is_array(scope, object.handle())? => {
                is_function(scope, object.val(scope, c"keys")?)
                    && is_function(scope, object.val(scope, c"get")?)
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
        let array_ctor = scope.global().obj(scope, c"Array")?.ok_or(ExnThrown)?;
        let entries = call_method(scope, array_ctor, c"from", &[headers])?;
        let entries = Object::from_value(scope, *entries)?;
        let mut cookies = Vec::new();
        for entry in elements(scope, entries)? {
            let entry = Object::from_value(scope, *entry)?;
            let name = entry.get_element(scope, 0)?;
            let value = entry.get_element(scope, 1)?;
            if name.is_string() && String::from_jsval(scope, name, ())? == "set-cookie" {
                match as_array(scope, value)? {
                    Some(values) => cookies.extend(elements(scope, values)?),
                    None => cookies.push(value),
                }
                continue;
            }
            call_method(scope, this, c"setHeader", &[name, value])?;
        }
        if !cookies.is_empty() {
            let cookies = js::Array::with_contents(scope, &cookies)?;
            call_method(
                scope,
                this,
                c"setHeader",
                &[
                    string(scope, "set-cookie")?,
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
        if this.flag(scope, c"_headerSent")? {
            return Err(headers_sent("append").into());
        }
        let text = validate_name(scope, name)?;
        validate_value(scope, &text, value)?;
        let Some(entry) = entry_of(scope, this, &text)? else {
            return outgoing_message_class::set_header(scope, this, name, value);
        };
        let previous = entry.get_element(scope, 1)?;
        let existing = match as_array(scope, previous)? {
            Some(array) => array,
            None => {
                let array = js::Array::with_contents(scope, &[previous])?;
                entry.set_element(scope, 1, *array)?;
                *array
            }
        };
        let added = match as_array(scope, value)? {
            Some(array) => elements(scope, array)?,
            None => vec![value],
        };
        for item in added {
            call_method(scope, existing, c"push", &[item])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetheadername>
    pub fn get_header<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let name = validate_string(scope, name, "name")?;
        match entry_of(scope, this, &name)? {
            Some(entry) => Ok(entry.get_element(scope, 1)?),
            None => Ok(scope.root_value(js::value::undefined())),
        }
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetheaders>
    pub fn get_headers<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        let headers = null_object(scope)?;
        let Some(store) = existing_store(scope, this)? else {
            return Ok(headers);
        };
        for key in store.own_keys(scope)? {
            let entry = Object::from_value(scope, crate::http::get_by_key(scope, store, key)?)
                .map_err(|e| e.throw(scope))?;
            let value = entry.get_element(scope, 1)?;
            crate::http::set_by_key(scope, headers, key, value)?;
        }
        Ok(headers)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetheadernames>
    pub fn get_header_names<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<js::Array<'s>, ExnThrown> {
        let keys = match existing_store(scope, this)? {
            Some(store) => store.own_keys(scope)?,
            None => Vec::new(),
        };
        js::Array::with_contents(scope, &keys)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagegetrawheadernames>
    pub fn get_raw_header_names<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<js::Array<'s>, ExnThrown> {
        let mut names = Vec::new();
        if let Some(store) = existing_store(scope, this)? {
            for key in store.own_keys(scope)? {
                let entry = Object::from_value(scope, crate::http::get_by_key(scope, store, key)?)
                    .map_err(|e| e.throw(scope))?;
                names.push(entry.get_element(scope, 0)?);
            }
        }
        js::Array::with_contents(scope, &names)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagehasheadername>
    pub fn has_header(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let name = validate_string(scope, name, "name")?;
        Ok(entry_of(scope, this, &name)?.is_some())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageremoveheadername>
    pub fn remove_header(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let name = validate_string(scope, name, "name")?;
        remove_named(scope, this, &name)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageflushheaders>
    pub fn flush_headers(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if super::super::wire::is_wire(scope, this)? {
            return super::super::wire::flush_headers(scope, this);
        }
        implicit_header(scope, this)?;
        this.set_property(scope, c"_headerSent", true)?;
        send(scope, this, &[], false)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagewritechunk-encoding-callback>: `false`
    /// once the body stream of a served response queues more than 16 KiB, or the socket of a
    /// response that writes to one does, and `drain` follows once it has room again.
    pub fn write<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let (encoding, callback) = match encoding {
            Some(encoding) if is_function(scope, encoding) => (None, Some(encoding)),
            _ => (encoding, callback),
        };
        if super::super::wire::is_wire(scope, this)? {
            let callback = callback.filter(|callback| is_function(scope, *callback));
            let room = super::super::wire::write(scope, this, chunk, encoding, callback, false)?;
            if !room {
                this.set_property(scope, c"writableNeedDrain", true)?;
            }
            return Ok(scope.root_value(js::value::from_bool(room)));
        }
        if this.flag(scope, c"finished")? {
            write_after_end(scope, this, callback)?;
            return Ok(scope.root_value(js::value::from_bool(false)));
        }
        let bytes = chunk_bytes(scope, chunk, encoding)?;
        implicit_header(scope, this)?;
        this.set_property(scope, c"_headerSent", true)?;
        let result = send(scope, this, &bytes, false)?;
        if let Some(callback) = callback {
            next_tick_call(scope, callback, &[])?;
        }
        Ok(result)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageendchunk-encoding-callback>
    pub fn end<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        chunk: Option<HandleValue<'_>>,
        encoding: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let (chunk, encoding, callback) = match (chunk, encoding) {
            (Some(chunk), _) if is_function(scope, chunk) => (None, None, Some(chunk)),
            (chunk, Some(encoding)) if is_function(scope, encoding) => {
                (chunk, None, Some(encoding))
            }
            (chunk, encoding) => (chunk, encoding, callback),
        };
        let callback = callback.filter(|callback| is_function(scope, *callback));
        let chunk = chunk.filter(|chunk| truthy(scope, *chunk));
        if super::super::wire::is_wire(scope, this)? {
            super::super::wire::end(scope, this, chunk, encoding, callback)?;
            this.set_property(scope, c"_headerSent", true)?;
            return Ok(this);
        }
        if this.flag(scope, c"finished")? {
            if chunk.is_some() {
                write_after_end(scope, this, callback)?;
            } else if let Some(callback) = callback {
                if this.flag(scope, c"writableFinished")? {
                    next_tick_call(scope, callback, &[])?;
                } else {
                    call_method(scope, this, c"once", &[string(scope, "finish")?, callback])?;
                }
            }
            return Ok(this);
        }
        let bytes = match chunk {
            Some(chunk) => chunk_bytes(scope, chunk, encoding)?,
            None => Vec::new(),
        };
        implicit_header(scope, this)?;
        if let Some(callback) = callback {
            call_method(scope, this, c"once", &[string(scope, "finish")?, callback])?;
        }
        this.set_property(scope, c"_headerSent", true)?;
        this.set_property(scope, c"finished", true)?;
        this.set_property(scope, c"writableEnded", true)?;
        send(scope, this, &bytes, true)?;
        let job = Function::new_callback(scope, c"", 0, finish, this)?;
        js::jobs::queue_tick(scope, job.to_jsval_throwing(scope)?, &[])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagesettimeoutmsecs-callback>: sets the
    /// timeout of the socket, once a response that writes to a socket has one. `callback`
    /// becomes a `timeout` listener. The timeout has no effect for a served request's response.
    pub fn set_timeout<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        msecs: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let msecs = msecs.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        if let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) {
            call_method(scope, this, c"on", &[string(scope, "timeout")?, callback])?;
        }
        match this.obj(scope, c"socket")? {
            Some(socket) if crate::stream::has_method(scope, socket, c"setTimeout")? => {
                call_method(scope, socket, c"setTimeout", &[msecs])?;
            }
            Some(_) => {}
            None if super::super::wire::is_wire(scope, this)? => {
                let set = closure(scope, 1, set_socket_timeout, &[msecs.get()])?;
                call_method(
                    scope,
                    this,
                    c"once",
                    &[string(scope, "socket")?, set.to_jsval_throwing(scope)?],
                )?;
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
            for item in elements(scope, headers)? {
                let pair =
                    Object::from_value(scope, *item).map_err(|e| Thrown::from(e.throw(scope)))?;
                pairs.push((pair.get_element(scope, 0)?, pair.get_element(scope, 1)?));
            }
            pairs
        } else {
            let mut pairs = Vec::new();
            for key in object_keys(scope, headers)? {
                pairs.push((key, crate::http::get_by_key(scope, headers, key)?));
            }
            pairs
        };
        for (field, value) in pairs {
            super::super::module::validate_header_name(scope, field, Some("Trailer name".into()))?;
            let field = String::from_jsval(scope, field, ())?;
            let values = match as_array(scope, value)? {
                Some(array) => {
                    let items = elements(scope, array)?;
                    if items.len() > 1 {
                        items
                    } else {
                        let joined = call_method(scope, array, c"join", &[string(scope, "; ")?])?;
                        vec![joined]
                    }
                }
                None => vec![value],
            };
            for value in values {
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

    /// <https://nodejs.org/api/http.html#outgoingmessagedestroyerror>
    pub fn destroy<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        error: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        if this.flag(scope, c"destroyed")? {
            return Ok(this);
        }
        this.set_property(scope, c"destroyed", true)?;
        let error = error.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        this.set_property(scope, c"_starlingErrored", error)?;
        call_method(scope, this, c"_starlingDestroy", &[error])?;
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagecork>: count the call in
    /// `writableCorked`, and cork the socket, if the message has one.
    pub fn cork(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let corked = this.num(scope, c"_starlingCorked")?;
        this.set_property(scope, c"_starlingCorked", corked + 1.0)?;
        if let Some(socket) = this.obj(scope, c"socket")? {
            if crate::stream::has_method(scope, socket, c"cork")? {
                call_method(scope, socket, c"cork", &[])?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageuncork>: undo a `cork` call, and uncork
    /// the socket, if the message has one.
    pub fn uncork(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let corked = this.num(scope, c"_starlingCorked")?;
        this.set_property(scope, c"_starlingCorked", corked - 1.0)?;
        if let Some(socket) = this.obj(scope, c"socket")? {
            if crate::stream::has_method(scope, socket, c"uncork")? {
                call_method(scope, socket, c"uncork", &[])?;
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

    /// The subclass's part of `destroy`, called with its error. This one emits `close` in the
    /// tick queue, unless it was emitted.
    #[method(name = "_starlingDestroy")]
    pub fn destroy_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let job = Function::new_callback(scope, c"", 0, emit_close_job, this)?;
        js::jobs::queue_tick(scope, job.to_jsval_throwing(scope)?, &[])
    }

    /// Define `headersSent`, `closed`, `errored`, `writableLength`, `writableHighWaterMark`,
    /// `writableCorked` and `writableObjectMode`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"headersSent", 0, headers_sent_getter, ())?;
        proto.define_accessor(scope, c"headersSent", Some(*getter), None, 0)?;
        for (index, name) in OUTGOING_GETTERS.iter().enumerate() {
            let getter = Function::new_callback(scope, name, 0, outgoing_getter, index as f64)?;
            proto.define_accessor(scope, name, Some(*getter), None, 0)?;
        }
        Ok(())
    }
}

/// The names of the getters [`outgoing_getter`] implements, in the order of their payloads.
const OUTGOING_GETTERS: [&std::ffi::CStr; 6] = [
    c"closed",
    c"errored",
    c"writableLength",
    c"writableHighWaterMark",
    c"writableCorked",
    c"writableObjectMode",
];

/// The getter of the `OutgoingMessage` property whose index in [`OUTGOING_GETTERS`] is the
/// payload: whether the message emitted `close`, the error it was destroyed with, the bytes it
/// and its socket haven't written, the high water mark of its socket or 16 KiB without one, the
/// number of `cork` calls not undone, and `false`.
fn outgoing_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let socket = this.obj(scope, c"socket")?;
    Ok(match payload.to_number() as usize {
        0 => js::value::from_bool(this.flag(scope, c"_closed")?),
        1 => this.val(scope, c"_starlingErrored")?.get(),
        2 => {
            let socket = match socket {
                Some(socket) => socket.num(scope, c"writableLength")?,
                None => 0.0,
            };
            js::value::from_f64(this.num(scope, c"outputSize")? + socket)
        }
        3 => match socket {
            Some(socket) if socket.val(scope, c"writableHighWaterMark")?.is_number() => {
                js::value::from_f64(socket.num(scope, c"writableHighWaterMark")?)
            }
            _ => js::value::from_f64(HIGH_WATER_MARK),
        },
        4 => js::value::from_f64(this.num(scope, c"_starlingCorked")?),
        _ => js::value::from_bool(false),
    })
}

/// The `socket` listener of `setTimeout`, whose captures are `[msecs]`: set the timeout of the
/// socket.
fn set_socket_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let msecs = Captures::of(scope, payload)?.get(0)?;
    let socket = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    call_method(scope, socket, c"setTimeout", &[msecs])?;
    Ok(js::value::undefined())
}

/// The getter of `headersSent`.
fn headers_sent_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    Ok(js::value::from_bool(this.flag(scope, c"_headerSent")?))
}

/// The function that discards a promise's rejection, shared per global.
fn ignore_rejection<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    fn ignore(
        _scope: &Scope<'_>,
        _args: CallbackArgs<'_>,
        _payload: HandleValue,
    ) -> Result<Value, ExnThrown> {
        Ok(js::value::undefined())
    }
    let function =
        js::class::get_or_init_shared_function(scope, ignore as *const () as usize, |scope| {
            Function::new_callback(scope, c"", 1, ignore, ())
        })?;
    Ok(*function)
}

/// Call `method` on `writer` with `args`, ignoring a rejection of the promise it returns.
fn writer_call(
    scope: &Scope<'_>,
    writer: Object<'_>,
    method: &'static std::ffi::CStr,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    let promise = call_method(scope, writer, method, args)?;
    if let Ok(promise) = Object::from_value(scope, *promise) {
        let ignore = ignore_rejection(scope)?.to_jsval_throwing(scope)?;
        let undefined = scope.root_value(js::value::undefined());
        call_method(scope, promise, c"then", &[undefined, ignore])?;
    }
    Ok(())
}

/// Write `chunk` to the body stream writer `writer` of `response`, and return whether the
/// stream has room for more. Without room, `drain` is emitted once it has.
fn write_body(
    scope: &Scope<'_>,
    response: Object<'_>,
    writer: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    writer_call(scope, writer, c"write", &[chunk])?;
    let desired = writer.val(scope, c"desiredSize")?;
    if !desired.is_number() || desired.to_number() > 0.0 {
        return Ok(true);
    }
    if !response.flag(scope, c"writableNeedDrain")? {
        response.set_property(scope, c"writableNeedDrain", true)?;
        let ready = writer.obj(scope, c"ready")?.ok_or(ExnThrown)?;
        let drain = closure(
            scope,
            0,
            emit_drain,
            &[response.to_jsval_raw_throwing(scope)?],
        )?;
        let ignore = ignore_rejection(scope)?;
        call_method(
            scope,
            ready,
            c"then",
            &[
                drain.to_jsval_throwing(scope)?,
                ignore.to_jsval_throwing(scope)?,
            ],
        )?;
    }
    Ok(false)
}

/// The fulfillment reaction of a body stream writer's `ready`, whose captures are `[response]`:
/// emit `drain`.
fn emit_drain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let response = Captures::of(scope, payload)?.obj(0)?;
    if response.flag(scope, c"writableNeedDrain")? {
        response.set_property(scope, c"writableNeedDrain", false)?;
        call_method(scope, response, c"emit", &[string(scope, "drain")?])?;
    }
    Ok(js::value::undefined())
}

/// The rejection reaction of a body stream writer's `closed`, whose captures are `[response]`:
/// the client disconnected before the body was written, so emit `close` on the response and
/// destroy its request with an `ECONNRESET` error.
fn on_body_aborted(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let response = Captures::of(scope, payload)?.obj(0)?;
    if response.flag(scope, c"_closed")? {
        return Ok(js::value::undefined());
    }
    emit_close(scope, response)?;
    if let Some(request) = response.obj(scope, c"req")? {
        let error = crate::stream::error_value(
            scope,
            NodeError::new(ErrorKind::Error, "ECONNRESET", "aborted"),
        )?;
        call_method(scope, request, c"destroy", &[error])?;
    }
    Ok(js::value::undefined())
}

/// The interest a served response with a streamed body holds in its request's event loop.
#[core_runtime::jsclass(hidden)]
pub struct BodyInterest {
    /// The interest, or `None` once it was released.
    #[no_trace]
    handle: Option<core_runtime::event_loop::InterestHandle>,
}

#[core_runtime::jsmethods]
impl BodyInterest {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// Keep the current event loop running until the promise `closed` settles. A served response's
/// body writer closes when the response ends, and errors when it is destroyed or the client
/// disconnects. Until then, another request's handler can write to the body.
fn hold_event_loop(scope: &Scope<'_>, closed: Object<'_>) -> Result<(), ExnThrown> {
    let Some(handle) =
        core_runtime::event_loop::with_active_event_loop(|el| el.acquire_interest_handle())
    else {
        return Ok(());
    };
    let interest =
        js::class::create_instance_with::<BodyInterestImpl>(scope, |_| BodyInterestImpl {
            handle: Some(handle),
        })?;
    let release = closure(
        scope,
        1,
        release_interest,
        &[interest.to_jsval_raw_throwing(scope)?],
    )?;
    let release = release.to_jsval_throwing(scope)?;
    call_method(scope, closed, c"then", &[release, release])?;
    Ok(())
}

/// The reaction to a body writer's `closed`, whose captures are `[interest]`: release the
/// interest.
fn release_interest(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interest = Captures::of(scope, payload)?.obj(0)?;
    if let Ok(interest) = interest.cast::<BodyInterest>() {
        interest.data_mut().handle.take();
    }
    Ok(js::value::undefined())
}

/// Whether the response `response` has no body: a `HEAD` request's, or one with status 1xx, 204
/// or 304.
fn without_body(scope: &Scope<'_>, response: Object<'_>) -> Result<bool, ExnThrown> {
    let status = response.num(scope, c"statusCode")? as u16;
    if (100..200).contains(&status) || status == 204 || status == 304 {
        return Ok(true);
    }
    let method = match response.obj(scope, c"req")? {
        Some(request) => request.val(scope, c"method")?,
        None => return Ok(false),
    };
    Ok(String::from_jsval(scope, method, ()).is_ok_and(|method| method == "HEAD"))
}

/// The pending response promise of `response`, which this takes, so nothing settles it twice.
fn take_respond<'s>(
    scope: &'s Scope<'_>,
    response: Object<'_>,
) -> Result<Option<js::Promise<'s>>, ExnThrown> {
    let Some(promise) = response.obj(scope, c"_starlingRespond")? else {
        return Ok(None);
    };
    let promise = promise
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?;
    response.set_property(scope, c"_starlingRespond", true)?;
    Ok(Some(promise))
}

/// The web `Response` for `response`, with its status and headers and with `body` as its body.
/// The headers the host owns are left out, except `Content-Length`, which frames the response
/// unless the response has no body.
fn web_response<'s>(
    scope: &'s Scope<'_>,
    response: Object<'_>,
    body: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let status = response.val(scope, c"statusCode")?;
    let status = super::to_int32(f64::from_jsval_throwing(scope, status, ())?);
    let message = response.val(scope, c"statusMessage")?;
    let init = Object::new_plain(scope)?;
    init.set_property(scope, c"status", status)?;
    if message.is_string() {
        init.set_property(scope, c"statusText", message)?;
    } else if let Some(text) = u16::try_from(status).ok().and_then(status_text) {
        init.set_property(scope, c"statusText", text)?;
    }
    let mut headers = Vec::new();
    for (name, value) in header_pairs(scope, response)? {
        let lower = name.to_ascii_lowercase();
        if HOST_HEADERS.contains(&lower.as_str()) && lower != "content-length" {
            continue;
        }
        let pair = js::Array::with_contents(
            scope,
            &[
                js::JSString::from_str(scope, &name)?.to_jsval_throwing(scope)?,
                js::JSString::from_str(scope, &value)?.to_jsval_throwing(scope)?,
            ],
        )?;
        headers.push((*pair).to_jsval_throwing(scope)?);
    }
    init.set_property(
        scope,
        c"headers",
        *js::Array::with_contents(scope, &headers)?,
    )?;
    let class = scope.global().obj(scope, c"Response")?.ok_or(ExnThrown)?;
    let body = scope.root_value(body.get());
    Function::construct(scope, class, &[body, init.to_jsval_throwing(scope)?])?
        .to_jsval_throwing(scope)
}

/// Resolve the response promise of `response` with a web `Response` with its status and headers,
/// and with `body` as its body. If the `Response` can't be built, such as for a status outside
/// 200 to 599, the promise is rejected with the error, which stays pending.
fn respond(
    scope: &Scope<'_>,
    response: Object<'_>,
    body: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let Some(promise) = take_respond(scope, response)? else {
        return web_response(scope, response, body).map(drop);
    };
    match web_response(scope, response, body) {
        Ok(web_response) => promise.resolve(scope, web_response),
        Err(thrown) => {
            let error = crate::stream::take_exception(scope);
            promise.reject(scope, error)?;
            js::exception::set_pending(
                scope,
                error,
                js::native::ExceptionStackBehavior::DoNotCapture,
            );
            Err(thrown)
        }
    }
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
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        req: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::outgoing_message_class::init(scope, this)?;
        this.set_property(scope, c"statusCode", 200)?;
        this.set_property(scope, c"statusMessage", js::value::undefined())?;
        this.set_property(scope, c"sendDate", true)?;
        this.set_property(scope, c"_sent100", false)?;
        this.set_property(scope, c"_expect_continue", false)?;
        this.define_property(scope, c"_starlingWire", true, 0)?;
        if let Some(req) = req {
            this.set_property(scope, c"req", req)?;
            if let Ok(req) = Object::from_value(scope, *req) {
                let method = req.val(scope, c"method")?;
                if crate::stream::is_str(scope, method, c"HEAD")? {
                    this.set_property(scope, c"_hasBody", false)?;
                }
                let major = req.val(scope, c"httpVersionMajor")?;
                let minor = req.val(scope, c"httpVersionMinor")?;
                if (major.is_number() && major.to_number() < 1.0)
                    || (minor.is_number() && minor.to_number() < 1.0)
                {
                    let te = match req.obj(scope, c"headers")? {
                        Some(headers) => headers.val(scope, c"te")?,
                        None => scope.root_value(js::value::undefined()),
                    };
                    let te = String::from_jsval(scope, te, ()).unwrap_or_default();
                    let chunked = super::super::wire::has_word(&te, "chunked");
                    this.set_property(scope, c"useChunkedEncodingByDefault", chunked)?;
                    this.set_property(scope, c"shouldKeepAlive", false)?;
                }
            }
        }
        Ok(())
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
        if truthy(scope, socket.val(scope, c"_httpMessage")?) {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_HTTP_SOCKET_ASSIGNED",
                "ServerResponse has an already assigned socket",
            )
            .into());
        }
        socket.set_property(scope, c"_httpMessage", this)?;
        let on_close = super::super::connection::response_close_listener(scope)?;
        call_method(
            scope,
            socket,
            c"on",
            &[string(scope, "close")?, on_close.to_jsval_throwing(scope)?],
        )?;
        this.set_property(scope, c"socket", socket)?;
        this.set_property(scope, c"connection", socket)?;
        call_method(
            scope,
            this,
            c"emit",
            &[string(scope, "socket")?, socket.to_jsval_throwing(scope)?],
        )?;
        super::super::wire::flush(scope, this)?;
        Ok(())
    }

    /// Stop writing to `socket`.
    pub fn detach_socket(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        if let Ok(socket) = Object::from_value(scope, *socket) {
            let on_close = super::super::connection::response_close_listener(scope)?;
            call_method(
                scope,
                socket,
                c"removeListener",
                &[string(scope, "close")?, on_close.to_jsval_throwing(scope)?],
            )?;
            socket.set_property(scope, c"_httpMessage", js::value::null())?;
        }
        this.set_property(scope, c"socket", js::value::null())?;
        this.set_property(scope, c"connection", js::value::null())?;
        Ok(())
    }

    /// Write what the response queued to its socket, as Node.js's `_flush` does.
    #[method(name = "_flush")]
    pub fn flush_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        super::super::wire::flush(scope, this)
    }

    /// Emit `prefinish`, as Node.js's `_finish` does once all data was handed to the socket.
    #[method(name = "_finish")]
    pub fn finish_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        call_method(scope, this, c"emit", &[string(scope, "prefinish")?])?;
        Ok(())
    }

    /// Write an informational response with `statusCode` and `headers` before the response. It
    /// has no effect for a served request's response.
    pub fn write_information(
        scope: &Scope<'_>,
        this: Object<'_>,
        status: HandleValue<'_>,
        headers: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if !super::super::wire::is_wire(scope, this)? {
            return Ok(());
        }
        super::super::wire::write_information(scope, this, status, headers, callback)
    }

    /// <https://nodejs.org/api/http.html#responsewriteheadstatuscode-statusmessage-headers>:
    /// `statusCode` is truncated to an integer, as with `statusCode | 0`.
    pub fn write_head<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        status: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
        headers: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        if this.flag(scope, c"_headerSent")? {
            return Err(headers_sent("write").into());
        }
        let code = super::super::to_int32(f64::from_jsval(scope, status, ())?);
        if !(100..=999).contains(&code) {
            let options = crate::inspect::InspectOptions::current(scope)?;
            let shown = crate::inspect::format(scope, &[string(scope, "%s")?, status], &options)?;
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
                if !truthy(scope, this.val(scope, c"statusMessage")?) {
                    let text = status_text(code as u16).unwrap_or("unknown");
                    this.set_property(scope, c"statusMessage", text)?;
                }
                headers
                    .filter(|headers| !headers.is_null() && !headers.is_undefined())
                    .or(message)
            }
        };
        this.set_property(scope, c"statusCode", code)?;
        // Headers given with headers set before are set too. Without, they are only sent.
        let progressive = super::existing_store(scope, this)?.is_some();
        if let Some(headers) = headers.filter(|_| progressive) {
            super::apply_headers(scope, this, headers)?;
        }
        let status_message = this.val(scope, c"statusMessage")?;
        if has_invalid_header_char(&String::from_jsval(scope, status_message, ())?) {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_CHAR",
                "Invalid character in statusMessage",
            )
            .into());
        }
        let wire = super::super::wire::is_wire(scope, this)?;
        if wire {
            if code == 204 || code == 304 || (100..=199).contains(&code) {
                this.set_property(scope, c"_hasBody", false)?;
            }
            if this.flag(scope, c"_expect_continue")? && !this.flag(scope, c"_sent100")? {
                this.set_property(scope, c"shouldKeepAlive", false)?;
            }
        }
        if let Some(headers) = headers.filter(|headers| !progressive && truthy(scope, *headers)) {
            let entries = super::head_headers(scope, headers)?;
            this.define_property(scope, c"_starlingHeadHeaders", entries, 0)?;
        }
        if wire {
            let status_message = String::from_jsval(scope, status_message, ())?;
            let line = super::super::wire::status_line(code, &status_message);
            if let Err(thrown) = super::super::wire::store_header(scope, this, &line) {
                this.delete_property(scope, c"_starlingHeadHeaders")?;
                return Err(thrown);
            }
        }
        this.set_property(scope, c"_headerSent", true)?;
        Ok(this)
    }

    /// Call `this.writeHead(this.statusCode)`, as the first `write` or `end` does.
    #[method(name = "_implicitHeader")]
    pub fn implicit_header(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let status = this.val(scope, c"statusCode")?;
        call_method(scope, this, c"writeHead", &[status])?;
        Ok(())
    }

    /// Define `writeHeader`, the same function as `writeHead`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let write_head = proto.get_property(scope, c"writeHead")?;
        proto.define_property(
            scope,
            c"writeHeader",
            write_head,
            js::class_spec::JSPROP_ENUMERATE as u32,
        )
    }

    /// <https://nodejs.org/api/http.html#responsewritecontinue>: write a `100 Continue` response.
    /// It has no effect for a served request's response, since the host sends informational
    /// responses.
    pub fn write_continue(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if !super::super::wire::is_wire(scope, this)? {
            return Ok(());
        }
        let status = scope.root_value(js::value::from_f64(100.0));
        super::super::wire::write_information(scope, this, status, None, callback)?;
        this.set_property(scope, c"_sent100", true)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#responsewriteprocessing>: write a `102 Processing`
    /// response. It has no effect for a served request's response.
    pub fn write_processing(
        scope: &Scope<'_>,
        this: Object<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if !super::super::wire::is_wire(scope, this)? {
            return Ok(());
        }
        let status = scope.root_value(js::value::from_f64(102.0));
        super::super::wire::write_information(scope, this, status, None, callback)
    }

    /// <https://nodejs.org/api/http.html#responsewriteearlyhintshints-callback>: write a `103
    /// Early Hints` response with the `Link` header of `hints.link` and the other hints as
    /// headers. Nothing is written without links. For a served request's response, it has no
    /// effect besides calling `callback`.
    pub fn write_early_hints(
        scope: &Scope<'_>,
        this: Object<'_>,
        hints: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if !super::super::wire::is_wire(scope, this)? {
            if let Some(callback) = callback {
                next_tick_call(scope, callback, &[])?;
            }
            return Ok(());
        }
        let hints = crate::errors::validate_plain_object(scope, hints, "hints", false)?;
        let link = hints.val(scope, c"link")?;
        if link.is_null() || link.is_undefined() {
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
        let headers = null_object(scope)?;
        headers.set_property(scope, c"Link", link.as_str())?;
        for key in object_keys(scope, hints)? {
            if !crate::stream::is_str(scope, key, c"link")? {
                let value = crate::http::get_by_key(scope, hints, key)?;
                crate::http::set_by_key(scope, headers, key, value)?;
            }
        }
        let status = scope.root_value(js::value::from_f64(103.0));
        let headers = headers.to_jsval_throwing(scope)?;
        super::super::wire::write_information(scope, this, status, Some(headers), callback)
    }

    /// Respond to the request: with the whole body if `last` and nothing was sent before, and
    /// otherwise with a stream of the chunks, starting with `chunk`. The result is whether the
    /// stream has room for more.
    #[method(name = "_starlingSend")]
    pub fn send_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        last: bool,
    ) -> Result<bool, ExnThrown> {
        let empty = js::ArrayBufferView::from_object(
            Object::from_value(scope, *chunk).map_err(|e| e.throw(scope))?,
        )
        .is_none_or(|view| view.byte_length() == 0);
        if let Some(writer) = this.obj(scope, c"_starlingWriter")? {
            let mut room = true;
            if !empty {
                room = super::write_body(scope, this, writer, chunk)?;
            }
            if last {
                writer_call(scope, writer, c"close", &[])?;
            }
            return Ok(room);
        }
        if !this.val(scope, c"_starlingRespond")?.is_object() {
            // Already responded to without a stream, or not a served request's response.
            return Ok(true);
        }
        if without_body(scope, this)? {
            respond(scope, this, scope.root_value(js::value::null()))?;
            return Ok(true);
        }
        if last {
            respond(scope, this, chunk)?;
            return Ok(true);
        }
        let strategy_class = scope
            .global()
            .obj(scope, c"ByteLengthQueuingStrategy")?
            .ok_or(ExnThrown)?;
        let strategy_init = Object::new_plain(scope)?;
        strategy_init.set_property(scope, c"highWaterMark", super::HIGH_WATER_MARK)?;
        let strategy = Function::construct(
            scope,
            strategy_class,
            &[strategy_init.to_jsval_throwing(scope)?],
        )?;
        let class = scope
            .global()
            .obj(scope, c"TransformStream")?
            .ok_or(ExnThrown)?;
        let transformer = Object::new_plain(scope)?;
        let transform = Function::construct(
            scope,
            class,
            &[
                transformer.to_jsval_throwing(scope)?,
                strategy.to_jsval_throwing(scope)?,
            ],
        )?;
        let writable = transform.obj(scope, c"writable")?.ok_or(ExnThrown)?;
        let writer = call_method(scope, writable, c"getWriter", &[])?;
        let writer = Object::from_value(scope, *writer).map_err(|e| e.throw(scope))?;
        this.define_property(scope, c"_starlingWriter", writer, 0)?;
        let closed = writer.obj(scope, c"closed")?.ok_or(ExnThrown)?;
        let aborted = closure(
            scope,
            1,
            super::on_body_aborted,
            &[this.to_jsval_raw_throwing(scope)?],
        )?;
        let undefined = scope.root_value(js::value::undefined());
        call_method(
            scope,
            closed,
            c"then",
            &[undefined, aborted.to_jsval_throwing(scope)?],
        )?;
        super::hold_event_loop(scope, closed)?;
        respond(scope, this, transform.val(scope, c"readable")?)?;
        if empty {
            return Ok(true);
        }
        super::write_body(scope, this, writer, chunk)
    }

    /// Error the response body if it is a stream, or reject the response, and emit `close`. A
    /// response that writes to a socket destroys the socket with `error`, or emits `close` in the
    /// tick queue without one.
    #[method(name = "_starlingDestroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        if super::super::wire::is_wire(scope, this)? {
            if let Some(socket) = this.obj(scope, c"socket")? {
                call_method(scope, socket, c"destroy", &[error])?;
            } else {
                let job = Function::new_callback(scope, c"", 0, emit_close_job, this)?;
                js::jobs::queue_tick(scope, job.to_jsval_throwing(scope)?, &[])?;
            }
            return Ok(());
        }
        if let Some(writer) = this.obj(scope, c"_starlingWriter")? {
            writer_call(scope, writer, c"abort", &[error])?;
        } else if let Some(promise) = super::take_respond(scope, this)? {
            let error = if error.is_undefined() {
                crate::stream::error_value(
                    scope,
                    NodeError::new(
                        ErrorKind::Error,
                        "ERR_STREAM_DESTROYED",
                        "The response was destroyed",
                    ),
                )?
            } else {
                scope.root_value(error.get())
            };
            promise.reject(scope, error)?;
        }
        super::emit_close(scope, this)
    }
}
