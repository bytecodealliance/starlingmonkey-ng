// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The HTTP/1.1 wire format of an `OutgoingMessage` that writes to a socket, a `ServerResponse` or
//! a `ClientRequest`, ported from Node.js's `lib/_http_outgoing.js`.
//!
//! `writeHead` or `_implicitHeader` stores the serialized header block in the message's state,
//! and sets `_header` to it. The first write sends it before its own bytes. A message writes to
//! its `socket` while it owns the socket, and queues its bytes otherwise, until `assignSocket` or
//! `onSocket` gives it the socket. The body is framed by `Content-Length` when `end` gives the
//! whole body before the header is stored, and with chunked transfer coding otherwise. `finish`
//! is emitted once the socket wrote the last bytes.
//!
//! The message's own data properties that Node.js reads to frame the message, such as
//! `chunkedEncoding`, `shouldKeepAlive` and `_hasBody`, are read once per operation, since code
//! can change them.

use std::cell::RefCell;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::outgoing::{
    header_entries, implicit_header, on_error, socket_writable_length, values, write_after_end,
    OutgoingState, Queued,
};
use super::status_text;
use crate::buffer::new_buffer;
use crate::errors::{number_to_string, ErrorKind, NodeError, Thrown};
use crate::support::{chunk_bytes, next_tick_call};

js::instance_local! {
    /// The HTTP date of the current second, and the second it is of.
    static DATE: RefCell<Option<(u64, String)>> = const { RefCell::new(None) };
}

/// The current time as an HTTP date, such as `Sun, 06 Nov 1994 08:49:37 GMT`, formatted once a
/// second.
pub(super) fn utc_date() -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if let Some((second, date)) = DATE.with_borrow(|date| date.clone()) {
        if second == now {
            return date;
        }
    }
    let seconds = now as i64;
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
    let date = format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        time / 3600,
        time % 3600 / 60,
        time % 60,
    );
    DATE.with_borrow_mut(|cached| *cached = Some((now, date.clone())));
    date
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

/// The own data properties of a message that frame it, read once to store its header block.
#[derive(Clone, Copy, PartialEq)]
struct Framing {
    last: bool,
    keep_alive: bool,
    chunked: bool,
    removed_connection: bool,
    removed_content_length: bool,
    removed_transfer_encoding: bool,
    default_keep_alive: bool,
    /// `_contentLength`, if it is a number.
    content_length: Option<f64>,
}

/// The properties [`Framing`] reads, in the order of its boolean fields.
const FRAMING_FLAGS: [&std::ffi::CStr; 7] = [
    c"_last",
    c"shouldKeepAlive",
    c"chunkedEncoding",
    c"_removedConnection",
    c"_removedContLen",
    c"_removedTE",
    c"_defaultKeepAlive",
];

impl Framing {
    /// The framing properties of `message`.
    fn read(scope: &Scope<'_>, message: Object<'_>) -> Result<Self, ExnThrown> {
        let mut flags = [false; 7];
        for (flag, name) in flags.iter_mut().zip(FRAMING_FLAGS) {
            *flag = js::conversion::to_boolean(message.get_property(scope, name)?);
        }
        let content_length = message.get_property(scope, c"_contentLength")?;
        let [last, keep_alive, chunked, removed_connection, removed_content_length, removed_transfer_encoding, default_keep_alive] =
            flags;
        Ok(Framing {
            last,
            keep_alive,
            chunked,
            removed_connection,
            removed_content_length,
            removed_transfer_encoding,
            default_keep_alive,
            content_length: content_length
                .is_number()
                .then(|| content_length.to_number()),
        })
    }

    /// The boolean fields, in the order of [`FRAMING_FLAGS`].
    fn flags(&self) -> [bool; 7] {
        [
            self.last,
            self.keep_alive,
            self.chunked,
            self.removed_connection,
            self.removed_content_length,
            self.removed_transfer_encoding,
            self.default_keep_alive,
        ]
    }

    /// Set the properties of `message` that differ from `read`, the framing they were read as.
    fn write(
        &self,
        scope: &Scope<'_>,
        message: Object<'_>,
        read: &Framing,
    ) -> Result<(), ExnThrown> {
        for ((now, before), name) in self.flags().iter().zip(read.flags()).zip(FRAMING_FLAGS) {
            if *now != before {
                message.set_property(scope, name, *now)?;
            }
        }
        if self.content_length != read.content_length {
            if let Some(length) = self.content_length {
                message.set_property(scope, c"_contentLength", length)?;
            }
        }
        Ok(())
    }
}

/// Append the header line `name: value` to `header`, and record what it means for the framing
/// in `framing`, as Node.js's `matchHeader` does.
fn add_line(
    framing: &mut Framing,
    state: &mut HeaderState,
    header: &mut String,
    name: &str,
    value: &str,
) {
    header.push_str(name);
    header.push_str(": ");
    header.push_str(value);
    header.push_str("\r\n");
    match name.to_ascii_lowercase().as_str() {
        "connection" => {
            state.connection = true;
            framing.removed_connection = false;
            if has_word(value, "close") {
                framing.last = true;
            } else {
                framing.keep_alive = true;
            }
        }
        "transfer-encoding" => {
            state.transfer_encoding = true;
            framing.removed_transfer_encoding = false;
            if has_word(value, "chunked") {
                framing.chunked = true;
            }
        }
        "content-length" => {
            state.content_length = true;
            let trimmed = value.trim();
            framing.content_length = Some(if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            });
            framing.removed_content_length = false;
        }
        "date" => state.date = true,
        "expect" => state.expect = true,
        "trailer" => state.trailer = true,
        "keep-alive" => framing.default_keep_alive = false,
        _ => {}
    }
}

/// Store the header block of `message`, starting with `first_line`, as Node.js's `_storeHeader`
/// builds it: the headers set, in the order they were set, then `Date` if `sendDate` is set,
/// `Connection` unless set, and `Content-Length` or `Transfer-Encoding: chunked` unless either
/// is set. An array value is a line per element, except for `Cookie` and the headers of
/// `uniqueHeaders`, whose values are joined with `; `. The block becomes `_header`.
pub(super) fn store_header(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    first_line: &str,
) -> Result<(), Thrown> {
    let mut header = first_line.to_string();
    let mut lines = HeaderState::default();
    let read = Framing::read(scope, message)?;
    let mut framing = read;
    for (name, value) in header_entries(scope, state) {
        let is_array = js::Array::is_array_value(scope, value)?;
        let mut texts = values(scope, value)?
            .into_iter()
            .map(|value| String::from_jsval_throwing(scope, value, ()))
            .collect::<Result<Vec<_>, _>>()?;
        // As in Node.js, the `Content-Disposition` values of a message with a `Content-Length` are
        // decoded as UTF-8 from their Latin-1 bytes.
        if name.eq_ignore_ascii_case("content-disposition")
            && framing
                .content_length
                .is_some_and(|length| length != 0.0 && !length.is_nan())
        {
            for value in &mut texts {
                let bytes: Vec<u8> = value.chars().map(|c| c as u32 as u8).collect();
                *value = String::from_utf8_lossy(&bytes).into_owned();
            }
        }
        if (texts.len() >= 2 && name.eq_ignore_ascii_case("cookie"))
            || (is_array && super::outgoing::is_unique(state, &name))
        {
            let joined = texts.join("; ");
            add_line(&mut framing, &mut lines, &mut header, &name, &joined);
        } else {
            for value in &texts {
                add_line(&mut framing, &mut lines, &mut header, &name, value);
            }
        }
    }
    if js::conversion::to_boolean(message.get_property(scope, c"sendDate")?) && !lines.date {
        header.push_str("Date: ");
        header.push_str(&utc_date());
        header.push_str("\r\n");
    }
    let status = message.get_property(scope, c"statusCode")?;
    let status = if status.is_number() {
        status.to_number()
    } else {
        0.0
    };
    if framing.chunked && (status == 204.0 || status == 304.0) {
        framing.chunked = false;
        framing.keep_alive = false;
    }
    let use_chunked_default =
        js::conversion::to_boolean(message.get_property(scope, c"useChunkedEncodingByDefault")?);
    if framing.removed_connection {
        framing.last = !framing.keep_alive;
    } else if !lines.connection {
        let send_keep_alive = framing.keep_alive
            && (lines.content_length
                || use_chunked_default
                || js::conversion::to_boolean(message.get_property(scope, c"agent")?));
        let max_reached = js::conversion::to_boolean(
            message.get_property(scope, c"maxRequestsOnConnectionReached")?,
        );
        if send_keep_alive && max_reached {
            header.push_str("Connection: close\r\n");
        } else if send_keep_alive {
            header.push_str("Connection: keep-alive\r\n");
            let timeout = message.get_property(scope, c"_keepAliveTimeout")?;
            let timeout = if timeout.is_number() {
                timeout.to_number()
            } else {
                0.0
            };
            if timeout != 0.0 && framing.default_keep_alive {
                let max = message.get_property(scope, c"_maxRequestsPerSocket")?;
                let max = if max.is_number() {
                    super::to_int32(max.to_number())
                } else {
                    0
                };
                let max = if max > 0 {
                    format!(", max={max}")
                } else {
                    String::new()
                };
                let seconds = number_to_string((timeout / 1000.0).floor());
                header.push_str(&format!("Keep-Alive: timeout={seconds}{max}\r\n"));
            }
        } else {
            framing.last = true;
            header.push_str("Connection: close\r\n");
        }
    }
    if !lines.content_length && !lines.transfer_encoding {
        let has_body = js::conversion::to_boolean(message.get_property(scope, c"_hasBody")?);
        if !has_body {
            framing.chunked = false;
        } else if !use_chunked_default {
            framing.last = true;
        } else if !lines.trailer
            && !framing.removed_content_length
            && framing.content_length.is_some()
        {
            let length = number_to_string(framing.content_length.unwrap_or_default());
            header.push_str(&format!("Content-Length: {length}\r\n"));
        } else if !framing.removed_transfer_encoding {
            header.push_str("Transfer-Encoding: chunked\r\n");
            framing.chunked = true;
        } else {
            framing.last = true;
        }
    }
    framing.write(scope, message, &read)?;
    if !framing.chunked && lines.trailer {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_HTTP_TRAILER_INVALID",
            "Trailers are invalid with this transfer encoding",
        )
        .into());
    }
    header.push_str("\r\n");
    message.set_property(scope, c"_header", header.as_str())?;
    {
        let mut data = state.data_mut();
        data.header = Some(header);
        data.header_stored = true;
    }
    if lines.expect {
        send(scope, message, state, Vec::new(), None, HeaderJoin::Utf8)?;
    }
    Ok(())
}

/// The status line of a response with `status` and `message`.
pub(super) fn status_line(status: i32, message: &str) -> String {
    format!("HTTP/1.1 {status} {message}\r\n")
}

/// The bytes of `header`, as UTF-8 if `utf8`, and as Latin-1 otherwise.
fn header_bytes(header: String, utf8: bool) -> Vec<u8> {
    if utf8 {
        return header.into_bytes();
    }
    header.chars().map(|c| c as u32 as u8).collect()
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
    state: &OutgoingState<'_>,
    data: Vec<u8>,
    callback: Option<HandleValue<'_>>,
    join: HeaderJoin,
) -> Result<bool, ExnThrown> {
    let header = state.data_mut().header.take();
    let Some(header) = header else {
        return write_raw(scope, message, state, data, callback);
    };
    message.set_property(scope, c"_headerSent", true)?;
    let mut bytes = header_bytes(header, join == HeaderJoin::Utf8);
    if join == HeaderJoin::Front {
        queue_output(scope, message, state, bytes, None, true)?;
        return write_raw(scope, message, state, data, callback);
    }
    bytes.extend_from_slice(&data);
    write_raw(scope, message, state, bytes, callback)
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
    let encoding = encoding.filter(|encoding| js::conversion::to_boolean(*encoding));
    match encoding.and_then(|encoding| crate::support::rust_string(scope, encoding)) {
        None if encoding.is_none() => HeaderJoin::Utf8,
        Some(name) if name == "utf8" => HeaderJoin::Utf8,
        Some(name) if name == "latin1" => HeaderJoin::Latin1,
        _ => HeaderJoin::Front,
    }
}

/// Whether `socket` was destroyed.
pub(super) fn socket_destroyed(scope: &Scope<'_>, socket: Object<'_>) -> Result<bool, ExnThrown> {
    if let Some(state) = crate::net::socket::state_of(scope, socket)? {
        return Ok(state.data().destroyed());
    }
    Ok(js::conversion::to_boolean(
        socket.get_property(scope, c"destroyed")?,
    ))
}

/// Whether `socket` can be written to.
pub(super) fn socket_writable(scope: &Scope<'_>, socket: Object<'_>) -> Result<bool, ExnThrown> {
    if let Some(side) = crate::stream::state::WritableSide::try_of(scope, socket)? {
        return Ok(side.get().is_writable());
    }
    Ok(js::conversion::to_boolean(
        socket.get_property(scope, c"writable")?,
    ))
}

/// Whether `socket` writes `message`: whether `message` is its `_httpMessage`.
fn writes(scope: &Scope<'_>, socket: Object<'_>, message: Object<'_>) -> Result<bool, ExnThrown> {
    let current = socket.get_property(scope, c"_httpMessage")?;
    Ok(current.is_object() && current.get().to_object() == message.handle().get())
}

/// The socket of the message of `state` if the socket writes `message` and is writable.
fn owned_socket<'s>(
    scope: &'s Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let socket = state.data().socket.get(scope);
    match socket {
        Some(socket) if writes(scope, socket, message)? && socket_writable(scope, socket)? => {
            Ok(Some(socket))
        }
        _ => Ok(None),
    }
}

/// Write `data` to the socket of `message` if it owns the socket, and queue it otherwise, as
/// Node.js's `_writeRaw` does. Nothing is written to a destroyed socket.
pub(super) fn write_raw(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    data: Vec<u8>,
    callback: Option<HandleValue<'_>>,
) -> Result<bool, ExnThrown> {
    let socket = state.data().socket.get(scope);
    if let Some(socket) = socket {
        if socket_destroyed(scope, socket)? {
            return Ok(false);
        }
    }
    if let Some(socket) = owned_socket(scope, message, state)? {
        if !state.data().output.is_empty() {
            flush_output(scope, message, state, socket)?;
        }
        let buffer = new_buffer(scope, &data)?.to_jsval_throwing(scope)?;
        let callback = callback.unwrap_or(HandleValue::undefined());
        let result = Function::call_by_name(scope, socket.handle(), c"write", &[buffer, callback])?;
        return Ok(js::conversion::to_boolean(result));
    }
    let size = queue_output(scope, message, state, data, callback, false)?;
    Ok(size < state.data().high_water_mark)
}

/// Queue `data` with `callback` in the output of the message of `state`, first if `front`, and
/// return the bytes queued.
fn queue_output(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    data: Vec<u8>,
    callback: Option<HandleValue<'_>>,
    front: bool,
) -> Result<f64, ExnThrown> {
    let length = data.len() as f64;
    let (size, connection) = {
        let mut data_mut = state.data_mut();
        if front {
            data_mut.output.push_front(Queued {
                data,
                callback: callback.map(|callback| Heap::from(callback.get())),
            });
        } else {
            data_mut.output.push_back(Queued {
                data,
                callback: callback.map(|callback| Heap::from(callback.get())),
            });
        }
        data_mut.output_size += length;
        (data_mut.output_size, data_mut.connection.get(scope))
    };
    message.set_property(scope, c"outputSize", size)?;
    if let Some(connection) = connection {
        super::connection::update_outgoing_data(scope, connection, length)?;
    }
    Ok(size)
}

/// Write the queued output of `message` to `socket`, as Node.js's `_flushOutput` does: cork the
/// socket for each `cork` of the message not undone, and write the queue corked. Returns the
/// result of the last write, or `None` if nothing was queued.
pub(super) fn flush_output(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    socket: Object<'_>,
) -> Result<Option<bool>, ExnThrown> {
    let corked = std::mem::take(&mut state.data_mut().corked);
    for _ in 0..corked as u32 {
        Function::call_by_name(scope, socket.handle(), c"cork", js::function::EmptyArgs)?;
    }
    let (queued, size, connection) = {
        let mut data = state.data_mut();
        let queued: Vec<(Vec<u8>, HandleValue<'_>)> = data
            .output
            .iter_mut()
            .map(|queued| {
                let callback = queued.callback.as_ref().map(|callback| callback.get(scope));
                (
                    std::mem::take(&mut queued.data),
                    callback.unwrap_or(HandleValue::undefined()),
                )
            })
            .collect();
        data.output.clear();
        let size = std::mem::take(&mut data.output_size);
        (queued, size, data.connection.get(scope))
    };
    if queued.is_empty() {
        return Ok(None);
    }
    let writes: Vec<(HandleValue<'_>, HandleValue<'_>)> = queued
        .into_iter()
        .map(|(bytes, callback)| {
            let buffer = new_buffer(scope, bytes.as_slice())?.to_jsval_throwing(scope)?;
            Ok((buffer, callback))
        })
        .collect::<Result<_, ExnThrown>>()?;
    Function::call_by_name(scope, socket.handle(), c"cork", js::function::EmptyArgs)?;
    let mut result = true;
    for (buffer, callback) in writes {
        let written =
            Function::call_by_name(scope, socket.handle(), c"write", &[buffer, callback])?;
        result = js::conversion::to_boolean(written);
    }
    Function::call_by_name(scope, socket.handle(), c"uncork", js::function::EmptyArgs)?;
    message.set_property(scope, c"outputSize", 0)?;
    if let Some(connection) = connection {
        super::connection::update_outgoing_data(scope, connection, -size)?;
    }
    Ok(Some(result))
}

/// Write what `message` queued to its socket, and emit `prefinish` if it ended, or `drain` if it
/// waits for one and the socket took everything, as Node.js's `_flush` does.
pub(super) fn flush(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
) -> Result<(), ExnThrown> {
    let Some(socket) = state.data().socket.get(scope) else {
        return Ok(());
    };
    if !socket_writable(scope, socket)? {
        return Ok(());
    }
    let result = flush_output(scope, message, state, socket)?;
    let (ended, need_drain) = {
        let data = state.data();
        (data.ended, data.need_drain)
    };
    if ended {
        crate::events::emit(scope, message, c"prefinish", &[])?;
    } else if need_drain && result == Some(true) {
        state.data_mut().need_drain = false;
        crate::events::emit(scope, message, c"drain", &[])?;
    }
    Ok(())
}

/// The bytes the message of `state` has not handed to the operating system yet.
fn writable_length(scope: &Scope<'_>, state: &OutgoingState<'_>) -> Result<f64, ExnThrown> {
    let socket = match state.data().socket.get(scope) {
        Some(socket) => socket_writable_length(scope, socket)?,
        None => 0.0,
    };
    Ok(state.data().output_size + socket)
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

/// The declared `Content-Length` of `message` if it checks its body against it, as Node.js's
/// `strictContentLength` checks.
fn strict_content_length(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
) -> Result<Option<f64>, Thrown> {
    if !js::conversion::to_boolean(message.get_property(scope, c"strictContentLength")?) {
        return Ok(None);
    }
    let length = message.get_property(scope, c"_contentLength")?;
    if !length.is_number()
        || !js::conversion::to_boolean(message.get_property(scope, c"_hasBody")?)
        || js::conversion::to_boolean(message.get_property(scope, c"_removedContLen")?)
        || js::conversion::to_boolean(message.get_property(scope, c"chunkedEncoding")?)
        || super::outgoing::header_value(scope, state, "transfer-encoding").is_some()
    {
        return Ok(None);
    }
    Ok(Some(length.to_number()))
}

/// The bytes of `chunk` in `encoding` and their length, or `ERR_STREAM_NULL_VALUES` for `null`
/// and `ERR_INVALID_ARG_TYPE` for a chunk that is neither a string nor a `Uint8Array`.
pub(super) fn written_bytes(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
) -> Result<(Vec<u8>, f64), Thrown> {
    if chunk.is_null() {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_STREAM_NULL_VALUES",
            "May not write null values to stream",
        )
        .into());
    }
    Ok(chunk_bytes(scope, chunk, encoding)?.ok_or_else(|| {
        NodeError::invalid_arg_type(
            scope,
            "chunk",
            "of type string or an instance of Buffer or Uint8Array",
            chunk,
        )
    })?)
}

/// Write `chunk` in `encoding` to `message`, as Node.js's `write_` does: the header block is
/// stored first if it wasn't, a message without a body drops the chunk, and a chunked message
/// frames it. `encoded` is the chunk's bytes and length, if they were computed before. `from_end`
/// marks the chunk `end` was given, whose length becomes the `Content-Length` of a header block
/// stored now. Returns whether more can be written without waiting for `drain`.
pub(super) fn write(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
    from_end: bool,
    encoded: Option<(Vec<u8>, f64)>,
) -> Result<bool, Thrown> {
    let (bytes, length) = match encoded {
        Some(encoded) => encoded,
        None => written_bytes(scope, chunk, encoding)?,
    };
    let (ended, destroyed) = {
        let data = state.data();
        (data.ended, data.destroyed)
    };
    let failure = if ended {
        Some(write_after_end())
    } else if destroyed {
        Some(NodeError::new(
            ErrorKind::Error,
            "ERR_STREAM_DESTROYED",
            "Cannot call write after a stream was destroyed",
        ))
    } else {
        None
    };
    if let Some(error) = failure {
        on_error(scope, message, state, error, callback)?;
        return Ok(false);
    }
    if js::conversion::to_boolean(message.get_property(scope, c"strictContentLength")?) {
        let written = state.data().bytes_written + length;
        if let Some(expected) = strict_content_length(scope, message, state)? {
            let mismatch = if from_end {
                written != expected
            } else {
                written > expected
            };
            if mismatch {
                return Err(length_mismatch(written, expected).into());
            }
        }
        state.data_mut().bytes_written = written;
    }
    if !state.data().header_stored {
        if from_end {
            message.set_property(scope, c"_contentLength", length)?;
        }
        implicit_header(scope, message, state)?;
    }
    if !js::conversion::to_boolean(message.get_property(scope, c"_hasBody")?) {
        if state.data().reject_body_writes {
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
        cork_until_next_tick(scope, state)?;
    }
    // As in Node.js, a chunk is framed when its length as a string, or as bytes, isn't 0, and its
    // chunk size is the length `Buffer.byteLength` computes, whatever the encoded bytes are.
    let nonempty = if chunk.is_string() {
        !js::JSString::from_value(scope, chunk)?.is_empty()
    } else {
        !bytes.is_empty()
    };
    if nonempty && js::conversion::to_boolean(message.get_property(scope, c"chunkedEncoding")?) {
        let mut data = format!("{:x}\r\n", length as u64).into_bytes();
        data.extend_from_slice(&bytes);
        data.extend_from_slice(b"\r\n");
        return Ok(send(
            scope,
            message,
            state,
            data,
            callback,
            HeaderJoin::Latin1,
        )?);
    }
    let join = header_join(scope, chunk, encoding);
    Ok(send(scope, message, state, bytes, callback, join)?)
}

/// The tick callback that uncorks a socket, whose payload is the socket.
fn uncork_socket(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let socket = js::function::cast_payload::<js::Object>(scope, payload);
    Function::call_by_name(scope, socket.handle(), c"uncork", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// Whether `socket` is corked.
fn socket_corked(scope: &Scope<'_>, socket: Object<'_>) -> Result<bool, ExnThrown> {
    if let Some(side) = crate::stream::state::WritableSide::try_of(scope, socket)? {
        return Ok(side.corked());
    }
    Ok(js::conversion::to_boolean(
        socket.get_property(scope, c"writableCorked")?,
    ))
}

/// Cork the socket of the message of `state` until the tick queue runs, unless it is corked, so
/// the writes of this tick go out together, as Node.js's `write_` does.
fn cork_until_next_tick(scope: &Scope<'_>, state: &OutgoingState<'_>) -> Result<(), ExnThrown> {
    let Some(socket) = state.data().socket.get(scope) else {
        return Ok(());
    };
    if socket_corked(scope, socket)? || !crate::support::has_method(scope, socket, c"cork")? {
        return Ok(());
    }
    Function::call_by_name(scope, socket.handle(), c"cork", js::function::EmptyArgs)?;
    js::jobs::queue_native_tick(scope, uncork_socket, socket, &[])
}

/// Cork the socket of the message of `state`, if it has one.
fn cork_socket(scope: &Scope<'_>, state: &OutgoingState<'_>) -> Result<(), ExnThrown> {
    let socket = state.data().socket.get(scope);
    if let Some(socket) = socket {
        if crate::support::has_method(scope, socket, c"cork")? {
            Function::call_by_name(scope, socket.handle(), c"cork", js::function::EmptyArgs)?;
        }
    }
    Ok(())
}

/// Uncork the socket of the message of `state` fully, if it has one, as `end` does.
fn uncork_fully(scope: &Scope<'_>, state: &OutgoingState<'_>) -> Result<(), ExnThrown> {
    state.data_mut().corked = 0.0;
    let Some(socket) = state.data().socket.get(scope) else {
        return Ok(());
    };
    if !crate::support::has_method(scope, socket, c"uncork")? {
        return Ok(());
    }
    crate::stream::set_writable_corked(scope, socket, 1.0)?;
    Function::call_by_name(scope, socket.handle(), c"uncork", js::function::EmptyArgs)?;
    Ok(())
}

/// The callback of the last write of a message, whose `this` is the socket and whose payload is
/// the message: emit `finish`, unless the write failed or the message emitted it.
fn on_finish(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let message = js::function::cast_payload::<js::Object>(scope, payload);
    if js::conversion::to_boolean(args.get(0)) {
        return Ok(js::value::undefined());
    }
    let state = super::outgoing::state_of(scope, message)?;
    let emitted = std::mem::replace(&mut state.data_mut().finish_emitted, true);
    if !emitted {
        crate::events::emit(scope, message, c"finish", &[])?;
    }
    Ok(js::value::undefined())
}

/// A new callback that emits `finish` on `message` once the last write succeeds.
fn finish_callback<'s>(
    scope: &'s Scope<'_>,
    message: Object<'s>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let finish = Function::new_callback(scope, c"onFinish", 1, on_finish, message)?;
    (*finish).to_jsval_throwing(scope)
}

/// Add `callback` as a `finish` listener of `message`, or call it with
/// `ERR_STREAM_ALREADY_FINISHED` if `message` finished.
fn queue_end_callback(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    callback: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if state.data().finish_emitted {
        let error = NodeError::new(
            ErrorKind::Error,
            "ERR_STREAM_ALREADY_FINISHED",
            "Cannot call end after a stream was finished",
        )
        .to_value(scope)?;
        Function::call(scope, HandleValue::undefined(), callback, &[error])?;
        return Ok(());
    }
    let callback = Object::from_value(scope, *callback).map_err(|e| e.throw(scope))?;
    crate::events::listen(scope, message, c"once", c"finish", callback)
}

/// End `message` with `chunk`, as Node.js's `OutgoingMessage.prototype.end` does: a message that
/// stored no header block yet gets one with the length of `chunk` as its `Content-Length`, a
/// chunked message sends the last chunk and the trailers, and `finish` follows once the socket
/// wrote the last bytes. `callback` is called on `finish`.
pub(super) fn end(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
    chunk: Option<HandleValue<'_>>,
    encoding: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let mut callback = callback;
    let mut finish = None;
    if let Some(chunk) = chunk {
        if state.data().ended {
            on_error(scope, message, state, write_after_end(), callback)?;
            return Ok(());
        }
        cork_socket(scope, state)?;
        let final_chunk = (chunk.is_string()
            || Object::from_value(scope, *chunk)
                .is_ok_and(|object| js::ArrayBufferView::from_object(object).is_some()))
            && !state.data().destroyed
            && !js::conversion::to_boolean(message.get_property(scope, c"strictContentLength")?);
        let mut encoded = None;
        let final_chunk = final_chunk && {
            if !state.data().header_stored {
                let (bytes, length) = written_bytes(scope, chunk, encoding)?;
                encoded = Some((bytes, length));
                message.set_property(scope, c"_contentLength", length)?;
                implicit_header(scope, message, state)?;
            }
            state.data().header_stored
                && js::conversion::to_boolean(message.get_property(scope, c"_hasBody")?)
                && !js::conversion::to_boolean(message.get_property(scope, c"chunkedEncoding")?)
        };
        if final_chunk {
            if let Some(callback) = callback.take() {
                queue_end_callback(scope, message, state, callback)?;
            }
            finish = Some(finish_callback(scope, message)?);
        }
        write(
            scope, message, state, chunk, encoding, finish, true, encoded,
        )?;
    } else if state.data().ended {
        if let Some(callback) = callback {
            queue_end_callback(scope, message, state, callback)?;
        }
        return Ok(());
    } else if !state.data().header_stored {
        cork_socket(scope, state)?;
        message.set_property(scope, c"_contentLength", 0)?;
        implicit_header(scope, message, state)?;
    }
    if let Some(callback) = callback {
        queue_end_callback(scope, message, state, callback)?;
    }
    if let Some(expected) = strict_content_length(scope, message, state)? {
        let written = state.data().bytes_written;
        if written != expected {
            return Err(length_mismatch(written, expected).into());
        }
    }
    if finish.is_none() {
        let finish = finish_callback(scope, message)?;
        let has_body = js::conversion::to_boolean(message.get_property(scope, c"_hasBody")?);
        if has_body && js::conversion::to_boolean(message.get_property(scope, c"chunkedEncoding")?)
        {
            let trailer = message.get_property(scope, c"_trailer")?;
            let trailer = crate::support::rust_string(scope, trailer).unwrap_or_default();
            let data = format!("0\r\n{trailer}\r\n");
            send(
                scope,
                message,
                state,
                data.chars().map(|c| c as u32 as u8).collect(),
                Some(finish),
                HeaderJoin::Latin1,
            )?;
        } else if state.data().header.is_some()
            || writable_length(scope, state)? > 0.0
            || chunk.is_some()
        {
            send(
                scope,
                message,
                state,
                Vec::new(),
                Some(finish),
                HeaderJoin::Latin1,
            )?;
        } else {
            next_tick_call(scope, finish, &[])?;
        }
    }
    uncork_fully(scope, state)?;
    state.data_mut().ended = true;
    message.set_property(scope, c"finished", true)?;
    let (queued, socket) = {
        let data = state.data();
        (data.output.len(), data.socket.get(scope))
    };
    let writing = match socket {
        Some(socket) => writes(scope, socket, message)?,
        None => false,
    };
    if queued == 0 && writing {
        crate::events::emit(scope, message, c"prefinish", &[])?;
    }
    Ok(())
}

/// Send the stored header block of `message` now, storing it first if needed, as Node.js's
/// `flushHeaders` does.
pub(super) fn flush_headers(
    scope: &Scope<'_>,
    message: Object<'_>,
    state: &OutgoingState<'_>,
) -> Result<(), ExnThrown> {
    implicit_header(scope, message, state)?;
    send(scope, message, state, Vec::new(), None, HeaderJoin::Utf8)?;
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
    state: &OutgoingState<'_>,
    status: HandleValue<'_>,
    headers: Option<HandleValue<'_>>,
    callback: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    if state.data().header_stored {
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
    if let Some(headers) = headers.filter(|headers| !headers.is_null_or_undefined()) {
        if !js::Array::is_array_value(scope, headers)? {
            crate::errors::validate_plain_object(scope, headers, "headers", false)?;
        }
        let pairs = super::outgoing::header_args(scope, headers, false)?;
        for (name, value) in pairs {
            head.push_str(&information_line(scope, name, value)?);
        }
    }
    head.push_str("\r\n");
    let callback = callback.filter(|callback| js::function::is_callable(*callback));
    write_raw(scope, message, state, head.into_bytes(), callback)?;
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
    if let Some(items) = crate::support::array_elements(scope, link)? {
        let mut links = Vec::new();
        for item in items {
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
