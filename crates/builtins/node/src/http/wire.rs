// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The HTTP/1.1 wire format of a `ServerResponse` that writes to a socket, ported from Node.js's
//! `lib/_http_outgoing.js`.
//!
//! `writeHead` stores the serialized header block in `_header`. The first write sends it before
//! its own bytes. A response writes to its `socket` while it is the socket's `_httpMessage`, and
//! queues its bytes in `outputData` otherwise, until `assignSocket` gives it the socket. The body
//! is framed by `Content-Length` when `end` gives the whole body before the header is stored, and
//! with chunked transfer coding otherwise. `finish` is emitted once the socket wrote the last
//! bytes.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::outgoing::{as_array, chunk_bytes, elements, header_entries, HIGH_WATER_MARK};
use super::status_text;
use crate::buffer::new_buffer;
use crate::errors::{number_to_string, ErrorKind, NodeError, Thrown};
use crate::stream::{call_method, closure, is_function, next_tick_call, string, truthy, Props};

/// Whether `message` writes HTTP/1.1 to a socket rather than responding to a served request.
pub(super) fn is_wire(scope: &Scope<'_>, message: Object<'_>) -> Result<bool, ExnThrown> {
    message.flag(scope, c"_starlingWire")
}

/// The current time as an HTTP date, such as `Sun, 06 Nov 1994 08:49:37 GMT`.
pub(super) fn utc_date() -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs()) as i64;
    let days = seconds.div_euclid(86400);
    let time = seconds.rem_euclid(86400);
    // The civil date of a day count, from Howard Hinnant's `civil_from_days`.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        time / 3600,
        time % 3600 / 60,
        time % 60,
    )
}

/// Whether `value` has `word` with no word character before or after it, ignoring case, as
/// `/(?:^|\W)word(?:$|\W)/i` matches.
pub(super) fn has_word(value: &str, word: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    lower.match_indices(word).any(|(start, _)| {
        let end = start + word.len();
        (start == 0 || !is_word(lower.as_bytes()[start - 1]))
            && (end == lower.len() || !is_word(lower.as_bytes()[end]))
    })
}

/// The headers of the header block being built that its framing depends on.
#[derive(Default)]
struct HeaderState {
    connection: bool,
    content_length: bool,
    transfer_encoding: bool,
    date: bool,
    expect: bool,
    trailer: bool,
}

/// Append the header line `name: value` to `header`, and record what it means for the framing
/// of `message`, as Node.js's `matchHeader` does.
fn add_line(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &mut HeaderState,
    header: &mut String,
    name: &str,
    value: &str,
) -> Result<(), ExnThrown> {
    header.push_str(name);
    header.push_str(": ");
    header.push_str(value);
    header.push_str("\r\n");
    match name.to_ascii_lowercase().as_str() {
        "connection" => {
            state.connection = true;
            message.set_property(scope, c"_removedConnection", false)?;
            if has_word(value, "close") {
                message.set_property(scope, c"_last", true)?;
            } else {
                message.set_property(scope, c"shouldKeepAlive", true)?;
            }
        }
        "transfer-encoding" => {
            state.transfer_encoding = true;
            message.set_property(scope, c"_removedTE", false)?;
            if has_word(value, "chunked") {
                message.set_property(scope, c"chunkedEncoding", true)?;
            }
        }
        "content-length" => {
            state.content_length = true;
            let trimmed = value.trim();
            let length = if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            };
            message.set_property(scope, c"_contentLength", length)?;
            message.set_property(scope, c"_removedContLen", false)?;
        }
        "date" => state.date = true,
        "expect" => state.expect = true,
        "trailer" => state.trailer = true,
        "keep-alive" => message.set_property(scope, c"_defaultKeepAlive", false)?,
        _ => {}
    }
    Ok(())
}

/// Store the header block of `message`, starting with `first_line`, in `_header`, as Node.js's
/// `_storeHeader` builds it: the headers set, in the order they were set, then `Date` if
/// `sendDate` is set, `Connection` unless set, and `Content-Length` or `Transfer-Encoding:
/// chunked` unless either is set. An array value is a line per element, except for `Cookie`,
/// whose values are joined with `; `.
pub(super) fn store_header(
    scope: &Scope<'_>,
    message: Object<'_>,
    first_line: &str,
) -> Result<(), Thrown> {
    let mut header = first_line.to_string();
    let mut state = HeaderState::default();
    for (name, value) in header_entries(scope, message)? {
        let values = match as_array(scope, value)? {
            Some(array) => elements(scope, array)?,
            None => vec![value],
        };
        let mut values = values
            .into_iter()
            .map(|value| String::from_jsval_throwing(scope, value, ()))
            .collect::<Result<Vec<_>, _>>()?;
        // As in Node.js, the `Content-Disposition` values of a message with a `Content-Length` are
        // decoded as UTF-8 from their Latin-1 bytes.
        if name.eq_ignore_ascii_case("content-disposition")
            && truthy(scope, message.val(scope, c"_contentLength")?)
        {
            for value in &mut values {
                let bytes: Vec<u8> = value.chars().map(|c| c as u32 as u8).collect();
                *value = String::from_utf8_lossy(&bytes).into_owned();
            }
        }
        if values.len() >= 2 && name.eq_ignore_ascii_case("cookie") {
            let joined = values.join("; ");
            add_line(scope, message, &mut state, &mut header, &name, &joined)?;
        } else {
            for value in &values {
                add_line(scope, message, &mut state, &mut header, &name, value)?;
            }
        }
    }
    if message.flag(scope, c"sendDate")? && !state.date {
        header.push_str(&format!("Date: {}\r\n", utc_date()));
    }
    let status = message.num(scope, c"statusCode")?;
    if message.flag(scope, c"chunkedEncoding")? && (status == 204.0 || status == 304.0) {
        message.set_property(scope, c"chunkedEncoding", false)?;
        message.set_property(scope, c"shouldKeepAlive", false)?;
    }
    let keep_alive = message.flag(scope, c"shouldKeepAlive")?;
    if message.flag(scope, c"_removedConnection")? {
        message.set_property(scope, c"_last", !keep_alive)?;
    } else if !state.connection {
        let send_keep_alive = keep_alive
            && (state.content_length
                || message.flag(scope, c"useChunkedEncodingByDefault")?
                || truthy(scope, message.val(scope, c"agent")?));
        if send_keep_alive && message.flag(scope, c"maxRequestsOnConnectionReached")? {
            header.push_str("Connection: close\r\n");
        } else if send_keep_alive {
            header.push_str("Connection: keep-alive\r\n");
            let timeout = message.num(scope, c"_keepAliveTimeout")?;
            if timeout != 0.0 && message.flag(scope, c"_defaultKeepAlive")? {
                let max = super::to_int32(message.num(scope, c"_maxRequestsPerSocket")?);
                let max = if max > 0 {
                    format!(", max={max}")
                } else {
                    String::new()
                };
                let seconds = number_to_string((timeout / 1000.0).floor());
                header.push_str(&format!("Keep-Alive: timeout={seconds}{max}\r\n"));
            }
        } else {
            message.set_property(scope, c"_last", true)?;
            header.push_str("Connection: close\r\n");
        }
    }
    if !state.content_length && !state.transfer_encoding {
        let content_length = message.val(scope, c"_contentLength")?;
        if !message.flag(scope, c"_hasBody")? {
            message.set_property(scope, c"chunkedEncoding", false)?;
        } else if !message.flag(scope, c"useChunkedEncodingByDefault")? {
            message.set_property(scope, c"_last", true)?;
        } else if !state.trailer
            && !message.flag(scope, c"_removedContLen")?
            && content_length.is_number()
        {
            let length = number_to_string(content_length.to_number());
            header.push_str(&format!("Content-Length: {length}\r\n"));
        } else if !message.flag(scope, c"_removedTE")? {
            header.push_str("Transfer-Encoding: chunked\r\n");
            message.set_property(scope, c"chunkedEncoding", true)?;
        } else {
            message.set_property(scope, c"_last", true)?;
        }
    }
    if !message.flag(scope, c"chunkedEncoding")? && state.trailer {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_HTTP_TRAILER_INVALID",
            "Trailers are invalid with this transfer encoding",
        )
        .into());
    }
    header.push_str("\r\n");
    message.set_property(scope, c"_header", header.as_str())?;
    message.set_property(scope, c"_starlingHeaderPending", true)?;
    if state.expect {
        send(scope, message, Vec::new(), None, HeaderJoin::Utf8)?;
    }
    Ok(())
}

/// The status line of a response with `status` and `message`.
pub(super) fn status_line(status: i32, message: &str) -> String {
    format!("HTTP/1.1 {status} {message}\r\n")
}

/// The bytes of the header block `message` stored, as UTF-8 if `utf8`, and as Latin-1 otherwise.
fn header_bytes(scope: &Scope<'_>, message: Object<'_>, utf8: bool) -> Result<Vec<u8>, ExnThrown> {
    let header = message.val(scope, c"_header")?;
    if !header.is_string() {
        return Ok(Vec::new());
    }
    let header = String::from_jsval_throwing(scope, header, ())?;
    if utf8 {
        return Ok(header.into_bytes());
    }
    Ok(header.chars().map(|c| c as u32 as u8).collect())
}

/// How [`send`] sends a header block that wasn't sent with the data, as Node.js's `_send` does.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum HeaderJoin {
    /// Before the data, encoded as UTF-8: the data is a string without an encoding or in `utf8`,
    /// or the empty string `flushHeaders` sends.
    Utf8,
    /// Before the data, encoded as Latin-1: the data is a string in `latin1`.
    Latin1,
    /// Encoded as Latin-1, first in the queued output: the data is bytes, or a string in another
    /// encoding.
    Front,
}

/// Send `data`, after the stored header block if it wasn't sent, joined according to `join`, and
/// call `callback` once the socket wrote it. Returns whether more can be written without waiting
/// for `drain`.
pub(super) fn send(
    scope: &Scope<'_>,
    message: Object<'_>,
    data: Vec<u8>,
    callback: Option<HandleValue<'_>>,
    join: HeaderJoin,
) -> Result<bool, ExnThrown> {
    if !message.flag(scope, c"_starlingHeaderPending")? {
        return write_raw(scope, message, data, callback);
    }
    message.set_property(scope, c"_starlingHeaderPending", false)?;
    let mut bytes = header_bytes(scope, message, join == HeaderJoin::Utf8)?;
    if join == HeaderJoin::Front && owns_socket(scope, message)?.is_none() {
        queue_output(scope, message, bytes, None, true)?;
        return write_raw(scope, message, data, callback);
    }
    bytes.extend_from_slice(&data);
    write_raw(scope, message, bytes, callback)
}

/// How a string or bytes chunk `chunk` in `encoding` joins the header block when sent unframed.
fn header_join(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
) -> HeaderJoin {
    if !chunk.is_string() {
        return HeaderJoin::Front;
    }
    let encoding = encoding.filter(|encoding| crate::stream::truthy(scope, *encoding));
    match encoding.and_then(|encoding| crate::stream::rust_string(scope, encoding)) {
        None if encoding.is_none() => HeaderJoin::Utf8,
        Some(name) if name == "utf8" => HeaderJoin::Utf8,
        Some(name) if name == "latin1" => HeaderJoin::Latin1,
        _ => HeaderJoin::Front,
    }
}

/// The length Node.js's `Buffer.byteLength` computes for `chunk` in `encoding`, which for a string
/// in an encoding other than UTF-8 comes from its length rather than from its encoded bytes.
fn declared_length(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    bytes: &[u8],
) -> Result<f64, Thrown> {
    if chunk.is_string() {
        return crate::buffer::buffer::byte_length(scope, chunk, encoding);
    }
    Ok(bytes.len() as f64)
}

/// The socket of `message` if `message` is its current message and it is writable.
fn owns_socket<'s>(
    scope: &'s Scope<'_>,
    message: Object<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let Some(socket) = message.obj(scope, c"socket")? else {
        return Ok(None);
    };
    let owns = is_current_message(scope, message)?;
    Ok((owns && socket.flag(scope, c"writable")?).then_some(socket))
}

/// Write `data` to the socket of `message` if it owns the socket, and queue it in `outputData`
/// otherwise, as Node.js's `_writeRaw` does. Nothing is written to a destroyed socket.
pub(super) fn write_raw(
    scope: &Scope<'_>,
    message: Object<'_>,
    data: Vec<u8>,
    callback: Option<HandleValue<'_>>,
) -> Result<bool, ExnThrown> {
    if let Some(socket) = message.obj(scope, c"socket")? {
        if socket.flag(scope, c"destroyed")? {
            return Ok(false);
        }
    }
    if let Some(socket) = owns_socket(scope, message)? {
        flush_output(scope, message, socket)?;
        let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        let buffer = new_buffer(scope, &data)?.to_jsval_throwing(scope)?;
        let result = call_method(scope, socket, c"write", &[buffer, callback])?;
        return Ok(truthy(scope, result));
    }
    let size = queue_output(scope, message, data, callback, false)?;
    Ok(size < HIGH_WATER_MARK)
}

/// Queue `data` with `callback` in the `outputData` of `message`, first if `front`, and return the
/// bytes queued.
fn queue_output(
    scope: &Scope<'_>,
    message: Object<'_>,
    data: Vec<u8>,
    callback: Option<HandleValue<'_>>,
    front: bool,
) -> Result<f64, ExnThrown> {
    let callback = callback.unwrap_or_else(|| scope.root_value(js::value::undefined()));
    let length = data.len() as f64;
    let buffer = new_buffer(scope, &data)?.to_jsval_throwing(scope)?;
    let entry = Object::new_plain(scope)?;
    entry.set_property(scope, c"data", buffer)?;
    entry.set_property(scope, c"encoding", "latin1")?;
    entry.set_property(scope, c"callback", callback)?;
    let output = output_data(scope, message)?;
    let method = if front { c"unshift" } else { c"push" };
    call_method(scope, output, method, &[entry.to_jsval_throwing(scope)?])?;
    let size = message.num(scope, c"outputSize")? + length;
    message.set_property(scope, c"outputSize", size)?;
    super::connection::update_outgoing_data(scope, message, length)?;
    Ok(size)
}

/// The `outputData` array of `message`, created if it is missing.
fn output_data<'s>(scope: &'s Scope<'_>, message: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(output) = message.obj(scope, c"outputData")? {
        return Ok(output);
    }
    let output = *js::Array::new(scope, 0)?;
    message.set_property(scope, c"outputData", output)?;
    Ok(output)
}

/// Write the queued `outputData` of `message` to `socket`, as Node.js's `_flushOutput` does.
/// Returns the result of the last write, or `None` if nothing was queued.
pub(super) fn flush_output(
    scope: &Scope<'_>,
    message: Object<'_>,
    socket: Object<'_>,
) -> Result<Option<bool>, ExnThrown> {
    let output = output_data(scope, message)?;
    let entries = elements(scope, output)?;
    if entries.is_empty() {
        return Ok(None);
    }
    message.set_property(scope, c"outputData", *js::Array::new(scope, 0)?)?;
    let mut result = true;
    for entry in entries {
        let entry = Object::from_value(scope, *entry).map_err(|e| e.throw(scope))?;
        let data = entry.val(scope, c"data")?;
        let callback = entry.val(scope, c"callback")?;
        result = truthy(
            scope,
            call_method(scope, socket, c"write", &[data, callback])?,
        );
    }
    let size = message.num(scope, c"outputSize")?;
    message.set_property(scope, c"outputSize", 0)?;
    super::connection::update_outgoing_data(scope, message, -size)?;
    Ok(Some(result))
}

/// Write what `message` queued to its socket, and emit `prefinish` if it ended, or `drain` if it
/// waits for one and the socket took everything, as Node.js's `_flush` does.
pub(super) fn flush(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    let Some(socket) = message.obj(scope, c"socket")? else {
        return Ok(());
    };
    if !socket.flag(scope, c"writable")? {
        return Ok(());
    }
    let result = flush_output(scope, message, socket)?;
    if message.flag(scope, c"finished")? {
        call_method(scope, message, c"emit", &[string(scope, "prefinish")?])?;
    } else if message.flag(scope, c"writableNeedDrain")? && result != Some(false) {
        message.set_property(scope, c"writableNeedDrain", false)?;
        call_method(scope, message, c"emit", &[string(scope, "drain")?])?;
    }
    Ok(())
}

/// The number of bytes `message` has not handed to the operating system yet.
fn writable_length(scope: &Scope<'_>, message: Object<'_>) -> Result<f64, ExnThrown> {
    let socket = match message.obj(scope, c"socket")? {
        Some(socket) => socket.num(scope, c"writableLength")?,
        None => 0.0,
    };
    Ok(message.num(scope, c"outputSize")? + socket)
}

/// The tick callback that reports a failed write, whose captures are `[message, error,
/// callback]`: call the callback with the error, and emit it as `error` unless the message was
/// destroyed.
fn report_write_error(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = crate::stream::Captures::of(scope, payload)?;
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

/// Report `error` to `callback` in the tick queue, and as an `error` event of `message` unless it
/// was destroyed, as Node.js's `onError` does.
fn on_error(
    scope: &Scope<'_>,
    message: Object<'_>,
    error: NodeError,
    callback: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let error = crate::stream::error_value(scope, error)?;
    if message.flag(scope, c"destroyed")? {
        if let Some(callback) = callback {
            next_tick_call(scope, callback, &[error])?;
        }
        return Ok(());
    }
    let report = closure(
        scope,
        0,
        report_write_error,
        &[
            message.to_jsval_raw_throwing(scope)?,
            error.get(),
            callback.map_or(js::value::undefined(), |callback| callback.get()),
        ],
    )?;
    js::jobs::queue_tick(scope, report.to_jsval_throwing(scope)?, &[])
}

/// `ERR_HTTP_CONTENT_LENGTH_MISMATCH` for a body of `actual` bytes where `expected` were declared.
fn length_mismatch(actual: f64, expected: f64) -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_HTTP_CONTENT_LENGTH_MISMATCH",
        format!(
            "Response body's content-length of {} byte(s) does not match the content-length of {} byte(s) set in header",
            number_to_string(actual),
            number_to_string(expected)
        ),
    )
}

/// Whether `message` checks its body against its `Content-Length`, as Node.js's
/// `strictContentLength` checks.
fn strict_content_length(scope: &Scope<'_>, message: Object<'_>) -> Result<bool, Thrown> {
    Ok(message.flag(scope, c"strictContentLength")?
        && message.val(scope, c"_contentLength")?.is_number()
        && message.flag(scope, c"_hasBody")?
        && !message.flag(scope, c"_removedContLen")?
        && !message.flag(scope, c"chunkedEncoding")?
        && !super::outgoing::outgoing_message_class::has_header(
            scope,
            message,
            string(scope, "transfer-encoding")?,
        )?)
}

/// Whether `message` stored its header block.
fn has_header_block(scope: &Scope<'_>, message: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(message.val(scope, c"_header")?.is_string())
}

/// Store the header block of `message` through its `_implicitHeader`.
fn implicit_header(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    call_method(scope, message, c"_implicitHeader", &[])?;
    Ok(())
}

/// Write `chunk` in `encoding` to `message`, as Node.js's `write_` does: the header block is
/// stored first if it wasn't, a message without a body drops the chunk, and a chunked message
/// frames it. `from_end` marks the chunk `end` was given, whose length becomes the
/// `Content-Length` of a header block stored now. Returns whether more can be written without
/// waiting for `drain`.
pub(super) fn write(
    scope: &Scope<'_>,
    message: Object<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
    from_end: bool,
) -> Result<bool, Thrown> {
    if chunk.is_null() {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_STREAM_NULL_VALUES",
            "May not write null values to stream",
        )
        .into());
    }
    let bytes = chunk_bytes(scope, chunk, encoding)?;
    let failure = if message.flag(scope, c"finished")? {
        Some(NodeError::new(
            ErrorKind::Error,
            "ERR_STREAM_WRITE_AFTER_END",
            "write after end",
        ))
    } else if message.flag(scope, c"destroyed")? {
        Some(NodeError::new(
            ErrorKind::Error,
            "ERR_STREAM_DESTROYED",
            "Cannot call write after a stream was destroyed",
        ))
    } else {
        None
    };
    if let Some(error) = failure {
        on_error(scope, message, error, callback)?;
        return Ok(false);
    }
    let length = declared_length(scope, chunk, encoding, &bytes)?;
    if message.flag(scope, c"strictContentLength")? {
        let written = message.num(scope, c"_starlingBytesWritten")? + length;
        let expected = message.num(scope, c"_contentLength")?;
        if strict_content_length(scope, message)?
            && (if from_end {
                written != expected
            } else {
                written > expected
            })
        {
            return Err(length_mismatch(written, expected).into());
        }
        message.set_property(scope, c"_starlingBytesWritten", written)?;
    }
    if !has_header_block(scope, message)? {
        if from_end {
            message.set_property(scope, c"_contentLength", length)?;
        }
        implicit_header(scope, message)?;
    }
    if !message.flag(scope, c"_hasBody")? {
        if message.flag(scope, c"_starlingRejectBodyWrites")? {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_HTTP_BODY_NOT_ALLOWED",
                "Adding content for this request method or response status is not allowed.",
            )
            .into());
        }
        if let Some(callback) = callback {
            next_tick_call(scope, callback, &[])?;
        }
        return Ok(true);
    }
    if !from_end {
        cork_until_next_tick(scope, message)?;
    }
    // As in Node.js, a chunk is framed when its length as a string, or as bytes, isn't 0, and its
    // chunk size is the length `Buffer.byteLength` computes, whatever the encoded bytes are.
    let nonempty = if chunk.is_string() {
        !js::JSString::from_value(scope, chunk)?.is_empty()
    } else {
        !bytes.is_empty()
    };
    if message.flag(scope, c"chunkedEncoding")? && nonempty {
        let mut data = format!("{:x}\r\n", length as u64).into_bytes();
        data.extend_from_slice(&bytes);
        data.extend_from_slice(b"\r\n");
        return Ok(send(scope, message, data, callback, HeaderJoin::Latin1)?);
    }
    let join = header_join(scope, chunk, encoding);
    Ok(send(scope, message, bytes, callback, join)?)
}

/// The tick callback that uncorks a socket, whose captures are `[socket]`.
fn uncork_socket(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = crate::stream::Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, socket, c"uncork", &[])?;
    Ok(js::value::undefined())
}

/// Cork the socket of `message` until the tick queue runs, unless it is corked, so the writes of
/// this tick go out together, as Node.js's `write_` does.
fn cork_until_next_tick(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    let Some(socket) = message.obj(scope, c"socket")? else {
        return Ok(());
    };
    if !crate::stream::has_method(scope, socket, c"cork")?
        || truthy(scope, socket.val(scope, c"writableCorked")?)
    {
        return Ok(());
    }
    call_method(scope, socket, c"cork", &[])?;
    let uncork = closure(
        scope,
        0,
        uncork_socket,
        &[socket.to_jsval_raw_throwing(scope)?],
    )?;
    js::jobs::queue_tick(scope, uncork.to_jsval_throwing(scope)?, &[])
}

/// Cork the socket of `message`, if it has one.
fn cork_socket(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    if let Some(socket) = message.obj(scope, c"socket")? {
        if crate::stream::has_method(scope, socket, c"cork")? {
            call_method(scope, socket, c"cork", &[])?;
        }
    }
    Ok(())
}

/// Uncork the socket of `message` fully, if it has one, as `end` does.
fn uncork_fully(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    let Some(socket) = message.obj(scope, c"socket")? else {
        return Ok(());
    };
    if !crate::stream::has_method(scope, socket, c"uncork")? {
        return Ok(());
    }
    if let Some(state) = socket.obj(scope, c"_writableState")? {
        state.set_property(scope, c"corked", 1)?;
    }
    call_method(scope, socket, c"uncork", &[])?;
    Ok(())
}

/// The callback of the last write of `message`, whose captures are `[message]`: mark it finished
/// and emit `finish`, unless the write failed.
fn on_finish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = crate::stream::Captures::of(scope, payload)?.obj(0)?;
    if truthy(scope, args.get(0)) || message.flag(scope, c"writableFinished")? {
        return Ok(js::value::undefined());
    }
    message.set_property(scope, c"writableFinished", true)?;
    call_method(scope, message, c"emit", &[string(scope, "finish")?])?;
    Ok(js::value::undefined())
}

/// Add `callback` as a `finish` listener of `message`, or call it with
/// `ERR_STREAM_ALREADY_FINISHED` if `message` finished.
fn queue_end_callback(
    scope: &Scope<'_>,
    message: Object<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if message.flag(scope, c"writableFinished")? {
        let error = crate::stream::error_value(
            scope,
            NodeError::new(
                ErrorKind::Error,
                "ERR_STREAM_ALREADY_FINISHED",
                "Cannot call end after a stream was finished",
            ),
        )?;
        Function::call(scope, js::value::undefined(), callback, &[error])?;
        return Ok(());
    }
    call_method(
        scope,
        message,
        c"once",
        &[string(scope, "finish")?, scope.root_value(callback.get())],
    )?;
    Ok(())
}

/// End `message` with `chunk`, as Node.js's `OutgoingMessage.prototype.end` does: a message that
/// stored no header block yet gets one with the length of `chunk` as its `Content-Length`, a
/// chunked message sends the last chunk and the trailers, and `finish` follows once the socket
/// wrote the last bytes. `callback` is called on `finish`.
pub(super) fn end(
    scope: &Scope<'_>,
    message: Object<'_>,
    chunk: Option<HandleValue<'_>>,
    encoding: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let mut callback = callback;
    let mut finish_callback = None;
    if let Some(chunk) = chunk {
        if message.flag(scope, c"finished")? {
            let error = NodeError::new(
                ErrorKind::Error,
                "ERR_STREAM_WRITE_AFTER_END",
                "write after end",
            );
            on_error(scope, message, error, callback)?;
            return Ok(());
        }
        cork_socket(scope, message)?;
        let final_chunk = (chunk.is_string()
            || Object::from_value(scope, *chunk)
                .is_ok_and(|object| js::ArrayBufferView::from_object(object).is_some()))
            && !message.flag(scope, c"destroyed")?
            && !message.flag(scope, c"strictContentLength")?;
        let final_chunk = final_chunk && {
            if !has_header_block(scope, message)? {
                let bytes = chunk_bytes(scope, chunk, encoding)?;
                let length = declared_length(scope, chunk, encoding, &bytes)?;
                message.set_property(scope, c"_contentLength", length)?;
                implicit_header(scope, message)?;
            }
            has_header_block(scope, message)?
                && message.flag(scope, c"_hasBody")?
                && !message.flag(scope, c"chunkedEncoding")?
        };
        if final_chunk {
            if let Some(callback) = callback.take() {
                queue_end_callback(scope, message, callback)?;
            }
            finish_callback = Some(closure(
                scope,
                1,
                on_finish,
                &[message.to_jsval_raw_throwing(scope)?],
            )?);
        }
        let finish = finish_callback
            .map(|callback| callback.to_jsval_throwing(scope))
            .transpose()?;
        write(scope, message, chunk, encoding, finish, true)?;
    } else if message.flag(scope, c"finished")? {
        if let Some(callback) = callback {
            queue_end_callback(scope, message, callback)?;
        }
        return Ok(());
    } else if !has_header_block(scope, message)? {
        cork_socket(scope, message)?;
        message.set_property(scope, c"_contentLength", 0)?;
        implicit_header(scope, message)?;
    }
    if let Some(callback) = callback {
        queue_end_callback(scope, message, callback)?;
    }
    if strict_content_length(scope, message)? {
        let written = message.num(scope, c"_starlingBytesWritten")?;
        let expected = message.num(scope, c"_contentLength")?;
        if written != expected {
            return Err(length_mismatch(written, expected).into());
        }
    }
    if finish_callback.is_none() {
        let finish = closure(
            scope,
            1,
            on_finish,
            &[message.to_jsval_raw_throwing(scope)?],
        )?
        .to_jsval_throwing(scope)?;
        if message.flag(scope, c"_hasBody")? && message.flag(scope, c"chunkedEncoding")? {
            let trailer = message.val(scope, c"_trailer")?;
            let trailer = if trailer.is_string() {
                String::from_jsval_throwing(scope, trailer, ())?
            } else {
                String::new()
            };
            let data = format!("0\r\n{trailer}\r\n");
            send(
                scope,
                message,
                data.chars().map(|c| c as u32 as u8).collect(),
                Some(finish),
                HeaderJoin::Latin1,
            )?;
        } else if message.flag(scope, c"_starlingHeaderPending")?
            || writable_length(scope, message)? > 0.0
            || chunk.is_some()
        {
            send(scope, message, Vec::new(), Some(finish), HeaderJoin::Latin1)?;
        } else {
            next_tick_call(scope, finish, &[])?;
        }
    }
    uncork_fully(scope, message)?;
    message.set_property(scope, c"finished", true)?;
    message.set_property(scope, c"writableEnded", true)?;
    let queued = output_data(scope, message)?
        .cast::<js::Array>()
        .map_err(|_| js::error::TypeError("unexpected object".into()).throw(scope))?
        .length(scope)?;
    if queued == 0 && is_current_message(scope, message)? {
        call_method(scope, message, c"emit", &[string(scope, "prefinish")?])?;
    }
    Ok(())
}

/// Whether `message` has a socket whose current message it is.
fn is_current_message(scope: &Scope<'_>, message: Object<'_>) -> Result<bool, ExnThrown> {
    let Some(socket) = message.obj(scope, c"socket")? else {
        return Ok(false);
    };
    let current = socket.val(scope, c"_httpMessage")?;
    crate::stream::same(scope, current, message.to_jsval_throwing(scope)?)
}

/// Send the stored header block of `message` now, storing it first if needed, as Node.js's
/// `flushHeaders` does.
pub(super) fn flush_headers(scope: &Scope<'_>, message: Object<'_>) -> Result<(), ExnThrown> {
    if !has_header_block(scope, message)? {
        implicit_header(scope, message)?;
    }
    send(scope, message, Vec::new(), None, HeaderJoin::Utf8)?;
    Ok(())
}

/// The header line `name: value` of an informational response, after checking `name` and
/// `value` as `setHeader` does.
fn information_line(
    scope: &Scope<'_>,
    name: HandleValue<'_>,
    value: HandleValue<'_>,
) -> Result<String, Thrown> {
    let name = super::validate_name(scope, name)?;
    super::module::validate_header_value(scope, name.clone(), value)?;
    let value = String::from_jsval(scope, value, ())?;
    Ok(format!("{name}: {value}\r\n"))
}

/// Write an informational response with `status`, from 100 to 199 but not 101, and `headers`, an
/// object, a flat array of names and values, or an array of pairs, as Node.js's
/// `writeInformation` does.
pub(super) fn write_information(
    scope: &Scope<'_>,
    message: Object<'_>,
    status: HandleValue<'_>,
    headers: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    if has_header_block(scope, message)? {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_HTTP_HEADERS_SENT",
            "Cannot write headers after they are sent to the client",
        )
        .into());
    }
    let code = crate::errors::validate_integer(scope, status, "statusCode", 100.0, 199.0)?;
    if code == 101.0 {
        return Err(NodeError::new(
            ErrorKind::RangeError,
            "ERR_HTTP_INVALID_STATUS_CODE",
            "Invalid status code: 101",
        )
        .into());
    }
    let reason = status_text(code as u16).unwrap_or("unknown");
    let mut head = status_line(code as i32, reason);
    if let Some(headers) = headers.filter(|headers| !headers.is_null() && !headers.is_undefined()) {
        if let Some(array) = as_array(scope, headers)? {
            let items = elements(scope, array)?;
            match items.first() {
                Some(first) if as_array(scope, *first)?.is_some() => {
                    for item in &items {
                        let pair = Object::from_value(scope, **item)
                            .map_err(|e| Thrown::from(e.throw(scope)))?;
                        let name = pair.get_element(scope, 0)?;
                        let value = pair.get_element(scope, 1)?;
                        head.push_str(&information_line(scope, name, value)?);
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
                    for pair in items.chunks(2) {
                        head.push_str(&information_line(scope, pair[0], pair[1])?);
                    }
                }
            }
        } else {
            let object = crate::errors::validate_plain_object(scope, headers, "headers", false)?;
            for key in object.own_enumerable_keys(scope)? {
                let value = crate::http::get_by_key(scope, object, key)?;
                head.push_str(&information_line(scope, key, value)?);
            }
        }
    }
    head.push_str("\r\n");
    let callback = callback.filter(|callback| is_function(scope, *callback));
    write_raw(scope, message, head.into_bytes(), callback)?;
    Ok(())
}

/// Whether `value` is a link header value of the form `</styles.css>; rel=preload; as=style`, as
/// Node.js's `validateLinkHeaderFormat` checks it.
fn valid_link(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('<') else {
        return false;
    };
    let Some(close) = rest.find('>') else {
        return false;
    };
    let mut rest = &rest[close + 1..];
    let is_param_char = |c: char| c != ';' && c != '"' && !c.is_whitespace();
    loop {
        if rest.is_empty() {
            return true;
        }
        rest = rest.trim_start();
        let Some(after) = rest.strip_prefix(';') else {
            return false;
        };
        rest = after.trim_start();
        let token_end = rest.find(|c: char| !is_param_char(c)).unwrap_or(rest.len());
        if token_end == 0 {
            return false;
        }
        let token = &rest[..token_end];
        rest = &rest[token_end..];
        if let Some(quoted) = rest.strip_prefix('"') {
            if !token.ends_with('=') || token.len() < 2 {
                return false;
            }
            let value_end = quoted
                .find(|c: char| !is_param_char(c))
                .unwrap_or(quoted.len());
            let Some(after) = quoted[value_end..].strip_prefix('"') else {
                return false;
            };
            rest = after;
        }
    }
}

/// The `Link` header value of the early hints `link`: a string, or an array of strings joined
/// with `, `, each of the form `</styles.css>; rel=preload`, as Node.js's
/// `validateLinkHeaderValue` checks it.
pub(super) fn link_header_value(
    scope: &Scope<'_>,
    link: HandleValue<'_>,
) -> Result<String, Thrown> {
    let invalid = |value: HandleValue<'_>| {
        NodeError::invalid_arg_value(
            scope,
            "hints",
            value,
            "must be an array or string of format \"</styles.css>; rel=preload; as=style\"",
        )
    };
    if link.is_string() {
        let text = String::from_jsval(scope, link, ())?;
        if !valid_link(&text) {
            return Err(invalid(link).into());
        }
        return Ok(text);
    }
    if let Some(array) = as_array(scope, link)? {
        let mut links = Vec::new();
        for item in elements(scope, array)? {
            let text = if item.is_string() {
                String::from_jsval(scope, item, ())?
            } else {
                return Err(invalid(item).into());
            };
            if !valid_link(&text) {
                return Err(invalid(item).into());
            }
            links.push(text);
        }
        return Ok(links.join(", "));
    }
    Err(invalid(link).into())
}
