// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `OutgoingMessage` and its subclass `ServerResponse`, which responds to a served request with a
//! web `Response`.
//!
//! Headers are kept in a null-prototype object under `_starlingHeaders`, mapping each lowercase
//! name to the pair of the name as set and the value. `write` and `end` pass their bytes to the
//! subclass's `_starlingSend(chunk, last)`.

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
const HIGH_WATER_MARK: f64 = 16384.0;

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
fn existing_store<'s>(
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
    let Some(store) = existing_store(scope, message)? else {
        return Ok(Vec::new());
    };
    let mut pairs = Vec::new();
    for key in store.own_keys(scope)? {
        let entry = Object::from_value(scope, crate::http::get_by_key(scope, store, key)?)
            .map_err(|e| e.throw(scope))?;
        let name = String::from_jsval(scope, entry.get_element(scope, 0)?, ()).unwrap_or_default();
        let value = entry.get_element(scope, 1)?;
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
fn chunk_bytes(
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
    let view = Object::from_value(scope, *chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
        .ok_or_else(|| {
            NodeError::invalid_arg_type(
                scope,
                "chunk",
                "of type string or an instance of Buffer or Uint8Array",
                chunk,
            )
        })?;
    Ok(view.copy_bytes(scope)?)
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

/// Mark `message` destroyed and emit `close` on it, unless it already emitted `close`.
fn emit_close(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    if message.flag(scope, c"_closed")? {
        return Ok(());
    }
    message.set_property(scope, c"_closed", true)?;
    message.set_property(scope, c"destroyed", true)?;
    call_method(scope, message, c"emit", &[string(scope, "close")?])?;
    Ok(())
}

/// Remove the header `name`, a validated string, from `message`.
fn remove_named(scope: &Scope<'_>, message: Object<'_>, name: &str) -> Result<(), Thrown> {
    if message.flag(scope, c"_headerSent")? {
        return Err(headers_sent("remove").into());
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
        ] {
            this.set_property(scope, name, value)?;
        }
        this.set_property(scope, c"socket", js::value::null())?;
        this.set_property(scope, c"connection", js::value::null())?;
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
        implicit_header(scope, this)?;
        this.set_property(scope, c"_headerSent", true)?;
        send(scope, this, &[], false)?;
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagewritechunk-encoding-callback>: `false`
    /// once the body stream of a served response queues more than 16 KiB, and `drain` follows
    /// once it has room again.
    pub fn write<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        chunk: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let callback = match encoding {
            Some(encoding) if is_function(scope, encoding) => Some(encoding),
            _ => callback,
        };
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

    /// <https://nodejs.org/api/http.html#outgoingmessagesettimeoutmsecs-callback>: has no effect.
    pub fn set_timeout<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
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
        call_method(
            scope,
            this,
            c"_starlingDestroy",
            &[error.unwrap_or_else(|| scope.root_value(js::value::undefined()))],
        )?;
        Ok(this)
    }

    /// <https://nodejs.org/api/http.html#outgoingmessagecork>: has no effect, since the body
    /// is not sent before `end` or the first `write`.
    pub fn cork() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#outgoingmessageuncork>: has no effect.
    pub fn uncork() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// The subclass's part of `destroy`, called with its error. This one has no effect.
    #[method(name = "_starlingDestroy")]
    pub fn destroy_impl() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// Define `headersSent`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"headersSent", 0, headers_sent_getter, ())?;
        proto.define_accessor(scope, c"headersSent", Some(*getter), None, 0)
    }
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
        if HOST_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
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

    /// <https://nodejs.org/api/http.html#class-httpserverresponse>: the response to `req`.
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
        if let Some(req) = req {
            this.set_property(scope, c"req", req)?;
            if let Ok(req) = Object::from_value(scope, *req) {
                let socket = req.val(scope, c"socket")?;
                this.set_property(scope, c"socket", socket)?;
                this.set_property(scope, c"connection", socket)?;
            }
        }
        Ok(())
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
            return Err(NodeError::new(
                ErrorKind::RangeError,
                "ERR_HTTP_INVALID_STATUS_CODE",
                format!(
                    "Invalid status code: {}",
                    String::from_jsval(scope, status, ())?
                ),
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
        if let Some(headers) = headers {
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

    /// <https://nodejs.org/api/http.html#responsewritecontinue>: has no effect, since the host
    /// sends informational responses.
    pub fn write_continue() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#responsewriteprocessing>: has no effect.
    pub fn write_processing() -> Result<(), ExnThrown> {
        Ok(())
    }

    /// <https://nodejs.org/api/http.html#responsewriteearlyhintshints-callback>: has no effect
    /// besides calling `callback`.
    pub fn write_early_hints(
        scope: &Scope<'_>,
        _hints: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        if let Some(callback) = callback {
            next_tick_call(scope, callback, &[])?;
        }
        Ok(())
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
        respond(scope, this, transform.val(scope, c"readable")?)?;
        if empty {
            return Ok(true);
        }
        super::write_body(scope, this, writer, chunk)
    }

    /// Error the response body if it is a stream, or reject the response, and emit `close`.
    #[method(name = "_starlingDestroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
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
