// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `IncomingMessage`: a served request, read from the web `Request`'s body stream, or a client
//! response, read from the `platform::http` response body.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};
use platform::http::IncomingBody;

use crate::buffer::new_buffer;
use crate::errors::Thrown;
use crate::stream::{call_method, closure, string, Captures, Props};

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

/// Set `headers` and `rawHeaders` of `message` from the header list `list`. Duplicates of
/// [`SINGLE_VALUED`] headers are dropped, `set-cookie` values are collected in an array, `cookie`
/// values are joined with `; `, and other values with `, `.
pub(super) fn set_headers(
    scope: &Scope<'_>,
    message: Object<'_>,
    list: &[(String, String)],
) -> Result<(), ExnThrown> {
    let mut raw = Vec::with_capacity(list.len() * 2);
    let mut joined: Vec<(String, Vec<&str>)> = Vec::new();
    for (name, value) in list {
        raw.push(js::JSString::from_str(scope, name)?.to_jsval_throwing(scope)?);
        raw.push(js::JSString::from_str(scope, value)?.to_jsval_throwing(scope)?);
        let lower = name.to_ascii_lowercase();
        match joined.iter_mut().find(|(name, _)| *name == lower) {
            Some((_, values))
                if lower == "set-cookie" || !SINGLE_VALUED.contains(&lower.as_str()) =>
            {
                values.push(value)
            }
            Some(_) => {}
            None => joined.push((lower, vec![value])),
        }
    }
    let headers = Object::new_plain(scope)?;
    for (name, values) in joined {
        let key = std::ffi::CString::new(name.as_str()).expect("header names are tokens");
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
        headers.define_property_by_name(
            scope,
            &key,
            value,
            js::class_spec::JSPROP_ENUMERATE as u32,
        )?;
    }
    message.set_property(scope, c"headers", headers)?;
    message.set_property(
        scope,
        c"rawHeaders",
        *js::Array::with_contents(scope, &raw)?,
    )?;
    Ok(())
}

/// The response body of a client request, which `_read` takes while a read is in progress.
#[core_runtime::jsclass(hidden)]
pub struct ResponseBody {
    #[no_trace]
    body: Option<IncomingBody>,
}

#[core_runtime::jsmethods]
impl ResponseBody {
    #[constructor]
    fn new() -> Result<Self, crate::errors::NodeError> {
        Err(crate::errors::NodeError::new(
            crate::errors::ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// Make `body` the body `message` reads from.
pub(super) fn set_response_body(
    scope: &Scope<'_>,
    message: Object<'_>,
    body: IncomingBody,
) -> Result<(), ExnThrown> {
    let handle = js::class::create_instance_with::<ResponseBodyImpl>(scope, |_| {
        ResponseBodyImpl { body: Some(body) }
    })?;
    message.define_property(scope, c"_starlingBody", handle.to_jsval_throwing(scope)?, 0)
}

/// Push `chunk`, a `Buffer`, onto `message`, or end it for `null`.
fn push(scope: &Scope<'_>, message: Object<'_>, chunk: HandleValue<'_>) -> Result<(), ExnThrown> {
    if chunk.is_null() {
        message.set_property(scope, c"complete", true)?;
    }
    call_method(scope, message, c"push", &[chunk])?;
    Ok(())
}

/// The fulfillment reaction of a body read, whose captures are `[message]`: push the chunk the
/// promise was fulfilled with, a `Buffer` or `null`.
fn on_chunk(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = Captures::of(scope, payload)?.obj(0)?;
    push(scope, message, args.get(0))?;
    Ok(js::value::undefined())
}

/// The rejection reaction of a body read, whose captures are `[message]`: destroy the message
/// with the error.
fn on_read_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, message, c"destroy", &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// The fulfillment reaction of a web stream read, whose captures are `[message]`: push the
/// bytes of the result's `value`, or end the message once the result is `done`.
fn on_web_chunk(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = Captures::of(scope, payload)?.obj(0)?;
    let result = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    if result.flag(scope, c"done")? {
        push(scope, message, scope.root_value(js::value::null()))?;
        return Ok(js::value::undefined());
    }
    let value = result.val(scope, c"value")?;
    let chunk = match Object::from_value(scope, *value) {
        Ok(array) if array.cast::<js::Uint8Array>().is_ok() => {
            let buffer = crate::buffer::buffer::class_constructor(scope)?;
            let proto = buffer.obj(scope, c"prototype")?.ok_or(ExnThrown)?;
            array.set_prototype(scope, proto.handle())?;
            value
        }
        _ => new_buffer(scope, &[])?.to_jsval_throwing(scope)?,
    };
    push(scope, message, chunk)?;
    Ok(js::value::undefined())
}

/// Read the next chunk of the client response body `handle` into `message`.
fn read_response_body(
    scope: &Scope<'_>,
    message: Object<'_>,
    handle: Object<'_>,
) -> Result<(), ExnThrown> {
    let body_handle = handle
        .cast::<ResponseBody>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?;
    let Some(mut body) = body_handle.data_mut().body.take() else {
        // A read is in progress, or the body ended.
        return Ok(());
    };
    let handle = RootedHeap::<js::object::Object>::new(handle);
    let message_url = RootedHeap::<js::object::Object>::new(message);
    let future = async move {
        let chunk: Result<Option<platform::http::BodyBytes>, platform::http::Error> =
            body.next_chunk().await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| match chunk {
            Ok(Some(bytes)) => {
                if let Ok(handle) = handle.get(scope).cast::<ResponseBody>() {
                    handle.data_mut().body = Some(body);
                }
                new_buffer(scope, &bytes)?.to_jsval_throwing(scope)
            }
            Ok(None) => Ok(scope.root_value(js::value::null())),
            Err(error) => {
                let url = match message_url.get(scope).obj(scope, c"req")? {
                    Some(request) => {
                        let url = request.val(scope, c"_starlingUrl")?;
                        url.is_string()
                            .then(|| String::from_jsval_throwing(scope, url, ()))
                            .transpose()?
                    }
                    None => None,
                };
                Err(super::client::transport_error(
                    scope,
                    &error.0,
                    url.as_deref(),
                ))
            }
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    let on_chunk = closure(scope, 1, on_chunk, &[message.to_jsval_raw_throwing(scope)?])?;
    let on_error = closure(
        scope,
        1,
        on_read_error,
        &[message.to_jsval_raw_throwing(scope)?],
    )?;
    promise.then(scope, Some(on_chunk), Some(on_error))?;
    Ok(())
}

#[core_runtime::jsfunction_class(
    name = "IncomingMessage",
    extends = "crate::stream::readable_class"
)]
pub mod incoming_message_class {
    use super::*;

    /// <https://nodejs.org/api/http.html#class-httpincomingmessage>: a message from `socket`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        socket: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::stream::readable_class::init(scope, this, None)?;
        let socket = socket.unwrap_or_else(|| scope.root_value(js::value::null()));
        this.set_property(scope, c"socket", socket)?;
        this.set_property(scope, c"connection", socket)?;
        this.set_property(scope, c"httpVersion", "1.1")?;
        this.set_property(scope, c"httpVersionMajor", 1)?;
        this.set_property(scope, c"httpVersionMinor", 1)?;
        this.set_property(scope, c"headers", Object::new_plain(scope)?)?;
        this.set_property(scope, c"rawHeaders", *js::Array::new(scope, 0)?)?;
        this.set_property(scope, c"trailers", Object::new_plain(scope)?)?;
        this.set_property(scope, c"rawTrailers", *js::Array::new(scope, 0)?)?;
        this.set_property(scope, c"complete", false)?;
        this.set_property(scope, c"aborted", false)?;
        this.set_property(scope, c"url", "")?;
        for name in [c"method", c"statusCode", c"statusMessage"] {
            this.set_property(scope, name, js::value::null())?;
        }
        Ok(())
    }

    /// Read the next chunk from the body source: a web stream reader, a client response body,
    /// or none, which ends the message.
    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        this.set_property(scope, c"_consuming", true)?;
        if let Some(reader) = this.obj(scope, c"_starlingReader")? {
            let read = call_method(scope, reader, c"read", &[])?;
            let promise = Object::from_value(scope, *read)
                .map_err(|e| e.throw(scope))?
                .cast::<js::Promise>()
                .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?;
            let on_chunk = closure(
                scope,
                1,
                on_web_chunk,
                &[this.to_jsval_raw_throwing(scope)?],
            )?;
            let on_error = closure(
                scope,
                1,
                on_read_error,
                &[this.to_jsval_raw_throwing(scope)?],
            )?;
            promise.then(scope, Some(on_chunk), Some(on_error))?;
            return Ok(());
        }
        if let Some(handle) = this.obj(scope, c"_starlingBody")? {
            return read_response_body(scope, this, handle);
        }
        push(scope, this, scope.root_value(js::value::null()))
    }

    /// <https://nodejs.org/api/http.html#messagesettimeoutmsecs-callback>: has no effect.
    pub fn set_timeout<'s>(this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        Ok(this)
    }

    /// Set `aborted` and emit `aborted` if the message didn't end, then call `callback` with
    /// `error` in the tick queue. The error is passed on only if the message has an `error`
    /// listener.
    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        if !this.flag(scope, c"readableEnded")? || !this.flag(scope, c"complete")? {
            this.set_property(scope, c"aborted", true)?;
            call_method(scope, this, c"emit", &[string(scope, "aborted")?])?;
        }
        let call = closure(
            scope,
            0,
            super::call_destroy_callback,
            &[
                this.to_jsval_raw_throwing(scope)?,
                error.get(),
                callback.get(),
            ],
        )?;
        js::jobs::queue_tick(scope, call.to_jsval_throwing(scope)?, &[])
    }

    /// Read the rest of the message and drop it, unless that already started.
    #[method(name = "_dump")]
    pub fn dump(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if this.flag(scope, c"_dumped")? {
            return Ok(());
        }
        this.set_property(scope, c"_dumped", true)?;
        call_method(
            scope,
            this,
            c"removeAllListeners",
            &[string(scope, "data")?],
        )?;
        call_method(scope, this, c"resume", &[])?;
        Ok(())
    }
}

/// The tick callback of `_destroy`, whose captures are `[message, error, callback]`: call the
/// callback with the error if the message has an `error` listener, and without it otherwise.
fn call_destroy_callback(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let message = captures.obj(0)?;
    let listeners = call_method(scope, message, c"listenerCount", &[string(scope, "error")?])?;
    let args = if listeners.is_number() && listeners.to_number() > 0.0 {
        vec![captures.get(1)?]
    } else {
        Vec::new()
    };
    Function::call(scope, js::value::undefined(), captures.get(2)?, &args)?;
    Ok(js::value::undefined())
}
