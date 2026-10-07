// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The HTTP/1.x parser of the listening `http.Server` and of client requests: request and
//! response heads through `httparse`, and bodies framed by `Content-Length`, decoded from chunked
//! transfer coding, or, for responses, read until the connection closes.
//!
//! [`Parser::feed`] appends received bytes, and each [`Parser::next`] returns the next event the
//! buffered bytes complete. Error codes and reasons are those of Node.js's `llhttp`. After an
//! error, each further chunk fed reports the error again. A lenient parser accepts what
//! `llhttp`'s lenient flags accept: header values with control characters, line feeds without a
//! carriage return, `Transfer-Encoding` with `Content-Length`, and data after a message that
//! closes the connection.

/// The parsed head of a message.
pub(super) struct Head {
    /// The method of a request, and empty for a response.
    pub method: String,
    /// The request target of a request, and empty for a response.
    pub url: String,
    /// The status code of a response, and 0 for a request.
    pub status: u16,
    /// The reason phrase of a response as received, decoded as Latin-1, and empty for a request.
    pub reason: String,
    /// The major and minor HTTP version: 0.9, 1.0, 1.1 or 2.0 for a request, and any digits for
    /// a response.
    pub major: u8,
    pub minor: u8,
    /// The header lines as received, with values decoded as Latin-1.
    pub headers: Vec<(String, String)>,
    /// Whether the request is a `CONNECT` request, or asks to upgrade the connection with an
    /// `Upgrade` header and an `upgrade` token in `Connection`. A response upgrades the
    /// connection with both headers and status 101.
    pub upgrade: bool,
    /// Whether the connection stays open after the message: HTTP/1.1 without `Connection: close`,
    /// or another version with `Connection: keep-alive`. A response whose body lasts until the
    /// connection closes doesn't keep it open.
    pub keep_alive: bool,
}

/// What the buffered bytes complete.
pub(super) enum Event {
    /// A message head.
    Head(Head),
    /// Bytes of the body of the current message.
    Body(Vec<u8>),
    /// The end of the current message, with its trailer lines.
    Complete(Vec<(String, String)>),
}

/// The messages a parser parses.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Request,
    Response,
}

/// What follows a head, which [`Parser::after_head`] sets.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum HeadAction {
    /// The body the head frames.
    Body,
    /// No body: the message is complete.
    SkipBody,
    /// No body, and the rest of the connection's bytes belong to another protocol.
    Upgrade,
}

/// A parse error, with `llhttp`'s code and reason.
#[derive(Clone, Debug)]
pub(super) struct ParseError {
    pub code: &'static str,
    pub reason: &'static str,
    /// The number of bytes of the last chunk fed that were parsed before the error.
    pub bytes_parsed: usize,
}

/// A parse error before its position in the last chunk is known: the code, the reason, and the
/// position in the parser's buffer where `llhttp` reports it, or `None` for the end of the bytes
/// parsed.
struct Failure(&'static str, &'static str, Option<usize>);

/// How the body of the current message ends.
enum Body {
    /// After this many more bytes.
    Length(u64),
    /// Chunked transfer coding, at this point of a chunk.
    Chunked(Chunk),
    /// When the connection closes.
    UntilClose,
}

/// The position in a chunked body.
enum Chunk {
    /// At a chunk size line.
    Size,
    /// In a chunk's data, with this many bytes left.
    Data(u64),
    /// At the line break after a chunk's data.
    DataEnd,
    /// In the trailer section after the last chunk.
    Trailers,
}

/// What the parser expects next.
enum State {
    /// A message head.
    Head,
    /// The body of the current message.
    Body(Body),
    /// The end of a message whose connection is upgraded, then [`State::Done`].
    Upgraded,
    /// Nothing: the connection was upgraded.
    Done,
    /// Nothing but line breaks, after a message that closes the connection.
    Closed,
    /// Nothing, after this error. `true` while the last chunk fed hasn't reported it.
    Failed(ParseError, bool),
}

/// The longest chunk size line accepted, including chunk extensions.
const MAX_CHUNK_LINE: usize = 16 * 1024;

/// The parser of one connection's requests or responses.
pub(super) struct Parser {
    kind: Kind,
    /// Whether the parser accepts what `llhttp`'s lenient flags accept.
    lenient: bool,
    /// The bytes received and not parsed yet.
    buffer: Vec<u8>,
    /// The position of the first byte of `buffer` in the connection's bytes.
    position: usize,
    /// The position of the first byte of the last chunk fed in the connection's bytes.
    chunk_start: usize,
    state: State,
    /// The largest message head accepted, in bytes.
    max_header_size: usize,
    /// The most header lines a message may have, or `None` for no limit.
    max_headers: Option<usize>,
    /// When the current message began, in milliseconds of the monotonic clock, or `None` between
    /// messages. A connection's first message begins when the parser is created.
    started: Option<f64>,
    /// Whether a byte of the current message arrived.
    in_message: bool,
    /// Whether the current message closes the connection.
    closing: bool,
    /// Whether the head of the current message was parsed.
    headers_complete: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Parser::new(Kind::Request, 16384, None, false)
    }
}

/// The current monotonic clock reading in milliseconds.
pub(super) fn now_ms() -> f64 {
    platform::clock::monotonic_ns() as f64 / 1e6
}

/// Whether the comma-separated header value `value` has the token `token`, ignoring case.
pub(super) fn has_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(token))
}

/// `bytes` decoded as Latin-1.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

/// Whether `bytes` has a byte other than a line break.
fn has_content(bytes: &[u8]) -> bool {
    bytes.iter().any(|&byte| byte != b'\r' && byte != b'\n')
}

/// The position after the first line break in `bytes`, if it has one.
fn line_end(bytes: &[u8]) -> Option<usize> {
    memchr::memchr(b'\n', bytes).map(|index| index + 1)
}

/// The position of the first carriage return in `bytes` that a byte other than a line feed
/// follows.
fn bare_carriage_return(bytes: &[u8]) -> Option<usize> {
    memchr::memchr_iter(b'\r', bytes)
        .find(|&index| bytes.get(index + 1).is_some_and(|&next| next != b'\n'))
}

impl Parser {
    /// A parser of `kind` messages accepting heads of up to `max_header_size` bytes with up to
    /// `max_headers` header lines, leniently if `lenient`.
    pub fn new(
        kind: Kind,
        max_header_size: usize,
        max_headers: Option<usize>,
        lenient: bool,
    ) -> Self {
        Parser {
            kind,
            lenient,
            buffer: Vec::new(),
            position: 0,
            chunk_start: 0,
            state: State::Head,
            max_header_size,
            max_headers,
            started: Some(now_ms()),
            in_message: false,
            closing: false,
            headers_complete: false,
        }
    }

    /// Append received bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.chunk_start = self.position + self.buffer.len();
        match &mut self.state {
            State::Done => return,
            State::Failed(_, report) => {
                *report = !bytes.is_empty();
                return;
            }
            _ => {}
        }
        if !self.in_message && has_content(bytes) {
            if self.started.is_none() {
                self.started = Some(now_ms());
                self.headers_complete = false;
            }
            self.in_message = true;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// Whether a message is in progress: its first byte arrived and it didn't complete.
    pub fn active(&self) -> bool {
        self.in_message
    }

    /// Whether the message in progress has been longer than `headers_timeout` milliseconds
    /// without a complete head, or longer than `request_timeout` milliseconds. A timeout of 0
    /// never expires.
    pub fn expired(&self, now: f64, headers_timeout: f64, request_timeout: f64) -> bool {
        let Some(started) = self.started else {
            return false;
        };
        let elapsed = now - started;
        (!self.headers_complete && headers_timeout > 0.0 && elapsed > headers_timeout)
            || (request_timeout > 0.0 && elapsed > request_timeout)
    }

    /// Stop timing the message in progress, so [`Parser::expired`] no longer reports it.
    pub fn forget_start(&mut self) {
        self.started = None;
    }

    /// Fail with `code` and `llhttp`'s reason for a callback error, which the next bytes fed
    /// report.
    pub fn fail_callback(&mut self, code: &'static str) {
        let error = ParseError {
            code,
            reason: "User callback error",
            bytes_parsed: 0,
        };
        self.state = State::Failed(error, false);
    }

    /// Stop parsing, and return the bytes received and not parsed.
    pub fn take_rest(&mut self) -> Vec<u8> {
        self.state = State::Done;
        self.position += self.buffer.len();
        std::mem::take(&mut self.buffer)
    }

    /// Set what follows the head [`Parser::next`] returned last, unless it upgraded the
    /// connection.
    pub fn after_head(&mut self, action: HeadAction) {
        if !matches!(self.state, State::Body(_)) {
            return;
        }
        match action {
            HeadAction::Body => {}
            HeadAction::SkipBody => self.state = State::Body(Body::Length(0)),
            HeadAction::Upgrade => {
                self.closing = false;
                self.state = State::Upgraded;
            }
        }
    }

    /// Check that the peer may end the connection now: between messages, or in a message whose
    /// body lasts until the end, a response's or a lenient parser's request's. Returns the end of
    /// that message.
    pub fn finish(&mut self) -> Result<Option<Event>, ParseError> {
        match self.state {
            State::Head if !has_content(&self.buffer) => Ok(None),
            State::Done | State::Closed | State::Failed(..) => Ok(None),
            State::Body(Body::UntilClose) => {
                self.closing = true;
                Ok(Some(self.complete(Vec::new())))
            }
            _ => {
                let error = ParseError {
                    code: "HPE_INVALID_EOF_STATE",
                    reason: "Invalid EOF state",
                    bytes_parsed: 0,
                };
                self.state = State::Failed(error.clone(), false);
                Err(error)
            }
        }
    }

    /// The next event the buffered bytes complete, or `None` if they complete none.
    pub fn next(&mut self) -> Result<Option<Event>, ParseError> {
        let result = match &mut self.state {
            State::Head => self.parse_head(),
            State::Body(Body::Length(_)) => Ok(self.length_body()),
            State::Body(Body::Chunked(_)) => self.chunked_body(),
            State::Body(Body::UntilClose) => {
                Ok((!self.buffer.is_empty()).then(|| Event::Body(self.consume(self.buffer.len()))))
            }
            State::Upgraded => {
                self.state = State::Done;
                self.in_message = false;
                self.started = None;
                Ok(Some(Event::Complete(Vec::new())))
            }
            State::Done => Ok(None),
            State::Closed if self.lenient => {
                self.consume(self.buffer.len());
                Ok(None)
            }
            State::Closed => match self
                .buffer
                .iter()
                .position(|&byte| byte != b'\r' && byte != b'\n')
            {
                Some(index) => Err(Failure(
                    "HPE_CLOSED_CONNECTION",
                    "Data after `Connection: close`",
                    Some(index + 1),
                )),
                None => {
                    self.consume(self.buffer.len());
                    Ok(None)
                }
            },
            State::Failed(error, report) => {
                if !*report {
                    return Ok(None);
                }
                *report = false;
                let mut error = error.clone();
                error.bytes_parsed = 0;
                return Err(error);
            }
        };
        result.map_err(|Failure(code, reason, at)| {
            let at = self.position + at.unwrap_or(0);
            let error = ParseError {
                code,
                reason,
                bytes_parsed: at.saturating_sub(self.chunk_start),
            };
            self.state = State::Failed(error.clone(), false);
            error
        })
    }

    /// Drop the first `count` buffered bytes.
    fn consume(&mut self, count: usize) -> Vec<u8> {
        self.position += count;
        self.buffer.drain(..count).collect()
    }

    /// End the current message.
    fn complete(&mut self, trailers: Vec<(String, String)>) -> Event {
        self.state = if self.closing {
            State::Closed
        } else {
            State::Head
        };
        self.in_message = !self.closing && has_content(&self.buffer);
        self.started = self.in_message.then(now_ms);
        self.headers_complete = false;
        Event::Complete(trailers)
    }

    fn parse_head(&mut self) -> Result<Option<Event>, Failure> {
        // Line breaks before a start line are ignored.
        let leading = self
            .buffer
            .iter()
            .take_while(|&&byte| byte == b'\r' || byte == b'\n')
            .count();
        if leading > 0 {
            self.consume(leading);
        }
        if self.buffer.is_empty() {
            return Ok(None);
        }
        let overflow =
            |length: usize| Failure("HPE_HEADER_OVERFLOW", "Header overflow", Some(length));
        let line = match self.kind {
            Kind::Request => request_line(&self.buffer)?,
            Kind::Response => status_line(&self.buffer)?,
        };
        let Some(line) = line else {
            if self.buffer.len() > self.max_header_size {
                return Err(overflow(self.buffer.len()));
            }
            return Ok(None);
        };
        let Some((head_end, parsed)) = self.header_fields(line.end)? else {
            if self.buffer.len() > self.max_header_size {
                return Err(overflow(self.buffer.len()));
            }
            return Ok(None);
        };
        if !self.lenient {
            if let Some(index) = bare_carriage_return(&self.buffer[..head_end]) {
                return Err(lf_expected(index));
            }
        }
        if head_end > self.max_header_size {
            return Err(overflow(self.max_header_size));
        }
        // The position in the buffer of the first byte of `bytes`, a slice of it.
        let offset = |bytes: &[u8]| bytes.as_ptr() as usize - self.buffer.as_ptr() as usize;
        if let Some(excess) = self.max_headers.and_then(|max| parsed.get(max)) {
            return Err(overflow(offset(excess.name) + excess.name.len() + 1));
        }

        let mut connection_close = false;
        let mut connection_keep_alive = false;
        let mut connection_upgrade = false;
        let mut has_upgrade = false;
        let mut content_length: Option<u64> = None;
        let mut chunked = false;
        let mut transfer_encoding = false;
        for header in &parsed {
            let name = header.name;
            let value: &[u8] = &header.value;
            let at = header.at;
            if name.eq_ignore_ascii_case(b"connection") {
                let value = latin1(value);
                connection_close |= has_token(&value, "close");
                connection_keep_alive |= has_token(&value, "keep-alive");
                connection_upgrade |= has_token(&value, "upgrade");
            } else if name.eq_ignore_ascii_case(b"upgrade") {
                has_upgrade = true;
            } else if name.eq_ignore_ascii_case(b"content-length") {
                if let Some(index) = value.iter().position(|byte| !byte.is_ascii_digit()) {
                    return Err(Failure(
                        "HPE_INVALID_CONTENT_LENGTH",
                        "Invalid character in Content-Length",
                        Some(at + index),
                    ));
                }
                if value.is_empty() {
                    return Err(Failure(
                        "HPE_INVALID_CONTENT_LENGTH",
                        "Invalid character in Content-Length",
                        Some(at),
                    ));
                }
                if content_length.is_some() {
                    return Err(Failure(
                        "HPE_UNEXPECTED_CONTENT_LENGTH",
                        "Duplicate Content-Length",
                        Some(at),
                    ));
                }
                let length = latin1(value).parse::<u64>().map_err(|_| {
                    Failure(
                        "HPE_INVALID_CONTENT_LENGTH",
                        "Content-Length overflow",
                        Some(at),
                    )
                })?;
                content_length = Some(length);
            } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
                transfer_encoding = true;
                let mut start = 0;
                for token in value.split(|&byte| byte == b',') {
                    if chunked && !self.lenient {
                        return Err(Failure(
                            "HPE_INVALID_TRANSFER_ENCODING",
                            "Invalid `Transfer-Encoding` header value",
                            Some(at + start),
                        ));
                    }
                    chunked = latin1(token).trim().eq_ignore_ascii_case("chunked");
                    start += token.len() + 1;
                }
            }
        }
        let request = self.kind == Kind::Request;
        let connect = request && line.method == "CONNECT";
        let upgrade = if request {
            connect || (has_upgrade && connection_upgrade)
        } else {
            has_upgrade && connection_upgrade && line.status == 101
        };
        let mut keep_alive = if line.major > 0 && line.minor > 0 {
            !connection_close
        } else {
            connection_keep_alive
        };
        // Responses with status 1xx, 204 or 304 have no body without framing headers.
        let bodiless = !request
            && ((100..200).contains(&line.status) || line.status == 204 || line.status == 304);
        let body = match (transfer_encoding, content_length) {
            (true, Some(_)) if !self.lenient => {
                return Err(Failure(
                    "HPE_INVALID_TRANSFER_ENCODING",
                    "Transfer-Encoding can't be present with Content-Length",
                    Some(head_end),
                ))
            }
            (true, _) if chunked => Body::Chunked(Chunk::Size),
            (true, _) if request && !self.lenient => {
                return Err(Failure(
                    "HPE_INVALID_TRANSFER_ENCODING",
                    "Request has invalid `Transfer-Encoding`",
                    Some(head_end),
                ))
            }
            (true, _) => Body::UntilClose,
            (false, Some(length)) => Body::Length(length),
            (false, None) if request || bodiless => Body::Length(0),
            (false, None) => Body::UntilClose,
        };
        if matches!(body, Body::UntilClose) {
            keep_alive = false;
        }
        let headers: Vec<(String, String)> = parsed
            .iter()
            .map(|header| (latin1(header.name), latin1(&header.value)))
            .collect();
        let StartLine {
            method,
            url,
            status,
            reason,
            major,
            minor,
            ..
        } = line;
        self.consume(head_end);
        self.headers_complete = true;
        self.closing = !keep_alive;
        self.state = if connect {
            State::Done
        } else if !request && status == 101 {
            // A 101 response ends the connection's HTTP messages, as in `llhttp`.
            State::Upgraded
        } else {
            State::Body(body)
        };
        Ok(Some(Event::Head(Head {
            method,
            url,
            status,
            reason,
            major,
            minor,
            headers,
            upgrade,
            keep_alive,
        })))
    }

    /// The header fields after the start line, which ends at `start`, and the position after the
    /// head, or `None` if the buffer ends before the head does.
    fn header_fields(&self, start: usize) -> Result<Option<(usize, Vec<Field<'_>>)>, Failure> {
        let fields = &self.buffer[start..];
        if self.lenient {
            return Ok(
                lenient_fields(fields, start)?.map(|(length, parsed)| (start + length, parsed))
            );
        }
        let mut slots = header_slots(fields, self.max_header_size, start)?;
        let line_break = line_break_error(fields, start);
        match httparse::parse_headers(fields, &mut slots) {
            Ok(httparse::Status::Complete((length, parsed))) => {
                if let Some(failure) = line_break {
                    return Err(failure);
                }
                let parsed = parsed
                    .iter()
                    .map(|header| Field {
                        name: header.name.as_bytes(),
                        value: std::borrow::Cow::Borrowed(header.value),
                        at: header.value.as_ptr() as usize - self.buffer.as_ptr() as usize,
                    })
                    .collect();
                Ok(Some((start + length, parsed)))
            }
            Ok(httparse::Status::Partial) => match line_break {
                Some(failure) => Err(failure),
                None => Ok(None),
            },
            Err(httparse::Error::TooManyHeaders) => Err(Failure(
                "HPE_HEADER_OVERFLOW",
                "Header overflow",
                Some(self.max_header_size),
            )),
            Err(_) => Err(first_failure(header_error(fields, start), line_break)),
        }
    }

    fn length_body(&mut self) -> Option<Event> {
        let State::Body(Body::Length(remaining)) = &mut self.state else {
            return None;
        };
        if *remaining == 0 {
            return Some(self.complete(Vec::new()));
        }
        if self.buffer.is_empty() {
            return None;
        }
        let take = (*remaining).min(self.buffer.len() as u64) as usize;
        *remaining -= take as u64;
        Some(Event::Body(self.consume(take)))
    }

    fn chunked_body(&mut self) -> Result<Option<Event>, Failure> {
        loop {
            let State::Body(Body::Chunked(chunk)) = &mut self.state else {
                return Ok(None);
            };
            match chunk {
                Chunk::Size => {
                    let Some(end) = line_end(&self.buffer) else {
                        if self.buffer.len() > MAX_CHUNK_LINE {
                            return Err(Failure(
                                "HPE_CHUNK_EXTENSIONS_OVERFLOW",
                                "Chunk extensions overflow",
                                Some(self.buffer.len()),
                            ));
                        }
                        return Ok(None);
                    };
                    let line = &self.buffer[..end];
                    let Some(line) = line.strip_suffix(b"\r\n") else {
                        return Err(Failure(
                            "HPE_LF_EXPECTED",
                            "Expected LF after chunk size",
                            Some(end),
                        ));
                    };
                    let size_end = line
                        .iter()
                        .position(|&byte| byte == b';')
                        .unwrap_or(line.len());
                    let digits = &line[..size_end];
                    if digits.is_empty() || !digits.iter().all(u8::is_ascii_hexdigit) {
                        return Err(Failure(
                            "HPE_INVALID_CHUNK_SIZE",
                            "Invalid character in chunk size",
                            Some(0),
                        ));
                    }
                    let Ok(size) = u64::from_str_radix(&latin1(digits), 16) else {
                        return Err(Failure(
                            "HPE_INVALID_CHUNK_SIZE",
                            "Chunk size overflow",
                            Some(0),
                        ));
                    };
                    *chunk = if size == 0 {
                        Chunk::Trailers
                    } else {
                        Chunk::Data(size)
                    };
                    self.consume(end);
                }
                Chunk::Data(remaining) => {
                    if self.buffer.is_empty() {
                        return Ok(None);
                    }
                    let take = (*remaining).min(self.buffer.len() as u64) as usize;
                    *remaining -= take as u64;
                    if *remaining == 0 {
                        *chunk = Chunk::DataEnd;
                    }
                    return Ok(Some(Event::Body(self.consume(take))));
                }
                Chunk::DataEnd => {
                    if self.buffer.len() < 2 {
                        return Ok(None);
                    }
                    if &self.buffer[..2] != b"\r\n" {
                        return Err(Failure(
                            "HPE_STRICT",
                            "Expected LF after chunk data",
                            Some(0),
                        ));
                    }
                    *chunk = Chunk::Size;
                    self.consume(2);
                }
                Chunk::Trailers => {
                    if self.buffer.starts_with(b"\r\n") {
                        self.consume(2);
                        return Ok(Some(self.complete(Vec::new())));
                    }
                    let mut headers = header_slots(&self.buffer, self.max_header_size, 0)?;
                    let line_break = if self.lenient {
                        None
                    } else {
                        line_break_error(&self.buffer, 0)
                    };
                    match httparse::parse_headers(&self.buffer, &mut headers) {
                        Ok(httparse::Status::Complete((length, parsed))) => {
                            if let Some(failure) = line_break {
                                return Err(failure);
                            }
                            let trailers = parsed
                                .iter()
                                .map(|header| (header.name.to_string(), latin1(header.value)))
                                .collect();
                            self.consume(length);
                            return Ok(Some(self.complete(trailers)));
                        }
                        Ok(httparse::Status::Partial) => {
                            if let Some(failure) = line_break {
                                return Err(failure);
                            }
                            if self.buffer.len() > self.max_header_size {
                                return Err(Failure(
                                    "HPE_HEADER_OVERFLOW",
                                    "Header overflow",
                                    Some(self.buffer.len()),
                                ));
                            }
                            return Ok(None);
                        }
                        Err(httparse::Error::TooManyHeaders) => {
                            return Err(Failure(
                                "HPE_HEADER_OVERFLOW",
                                "Header overflow",
                                Some(self.max_header_size),
                            ))
                        }
                        Err(_) => {
                            return Err(first_failure(header_error(&self.buffer, 0), line_break))
                        }
                    }
                }
            }
        }
    }
}

/// The slots for `httparse` to parse the header lines at the start of `bytes` into: one for each
/// line within the first `max_header_size` + 1 bytes, since longer header lines fail. Fails with
/// `HPE_HEADER_OVERFLOW` at the position `at` if the slots can't be allocated.
fn header_slots<'h>(
    bytes: &[u8],
    max_header_size: usize,
    at: usize,
) -> Result<Vec<httparse::Header<'h>>, Failure> {
    let scanned = &bytes[..bytes.len().min(max_header_size.saturating_add(1))];
    let lines = memchr::memchr_iter(b'\n', scanned).count() + 1;
    let mut slots = Vec::new();
    if slots.try_reserve_exact(lines).is_err() {
        return Err(Failure("HPE_HEADER_OVERFLOW", "Header overflow", Some(at)));
    }
    slots.resize(lines, httparse::EMPTY_HEADER);
    Ok(slots)
}

/// The `llhttp` error in strict mode for the first malformed line break in the header lines
/// `bytes` before the empty line that ends them, at its position counted from `base`: a carriage
/// return without a line feed, a line feed without a carriage return that ends a header line, and
/// a line feed without a carriage return in place of the empty line.
fn line_break_error(bytes: &[u8], base: usize) -> Option<Failure> {
    let mut line_start = 0;
    let mut breaks = memchr::memchr2_iter(b'\r', b'\n', bytes);
    while let Some(index) = breaks.next() {
        if bytes[index] == b'\n' {
            return Some(if index == line_start {
                Failure(
                    "HPE_INVALID_HEADER_TOKEN",
                    "Invalid header field char",
                    Some(base + index + 1),
                )
            } else {
                Failure(
                    "HPE_CR_EXPECTED",
                    "Missing expected CR after header value",
                    Some(base + index),
                )
            });
        }
        match bytes.get(index + 1) {
            None => return None,
            Some(b'\n') if index == line_start => return None,
            Some(b'\n') => {
                breaks.next();
                line_start = index + 2;
            }
            Some(_) => return Some(lf_expected(base + index)),
        }
    }
    None
}

/// `error`, or `line_break` if it comes before `error`.
fn first_failure(error: Failure, line_break: Option<Failure>) -> Failure {
    match line_break {
        Some(line_break) if line_break.2 < error.2 => line_break,
        _ => error,
    }
}

/// `HPE_LF_EXPECTED` for the carriage return at `index` of the buffer.
fn lf_expected(index: usize) -> Failure {
    Failure(
        "HPE_LF_EXPECTED",
        "Missing expected LF after header value",
        Some(index + 1),
    )
}

/// The methods `llhttp` parses besides Node.js's: those of RTSP, and `PRI`. A request with one of
/// them is invalid, but `llhttp` reports the error at the first byte that matches no method.
const OTHER_METHODS: [&str; 12] = [
    "DESCRIBE",
    "ANNOUNCE",
    "SETUP",
    "PLAY",
    "PAUSE",
    "TEARDOWN",
    "GET_PARAMETER",
    "SET_PARAMETER",
    "REDIRECT",
    "RECORD",
    "FLUSH",
    "PRI",
];

/// A parsed request line or status line.
struct StartLine {
    /// The method of a request line, and empty for a status line.
    method: String,
    /// The request target of a request line, and empty for a status line.
    url: String,
    /// The status code of a status line, and 0 for a request line.
    status: u16,
    /// The reason phrase of a status line, and empty for a request line.
    reason: String,
    major: u8,
    minor: u8,
    /// The position after the line's line break.
    end: usize,
}

/// A header field of a head: its name, a slice of the parser's buffer, its value, a slice of the
/// buffer unless obsolete line folding continues it, and the position of the value in the buffer.
struct Field<'b> {
    name: &'b [u8],
    value: std::borrow::Cow<'b, [u8]>,
    at: usize,
}

/// The request line at the start of `bytes`, or `None` if `bytes` ends before it does, as
/// `llhttp` parses it in its strict mode: a method of `METHODS`, a URL of visible ASCII characters,
/// and HTTP/0.9, 1.0, 1.1 or 2.0, separated by single spaces and ended by CRLF.
fn request_line(bytes: &[u8]) -> Result<Option<StartLine>, Failure> {
    let method_end = bytes.iter().position(|&byte| byte == b' ');
    let token = &bytes[..method_end.unwrap_or(bytes.len())];
    let matched = (0..=token.len())
        .rev()
        .find(|&length| {
            super::METHODS
                .iter()
                .chain(OTHER_METHODS.iter())
                .any(|method| method.as_bytes().starts_with(&token[..length]))
        })
        .unwrap_or(0);
    let Some(method_end) = method_end else {
        if matched < token.len() {
            return Err(invalid_method(matched));
        }
        return Ok(None);
    };
    let method = latin1(token);
    if !super::METHODS.contains(&method.as_str()) {
        return Err(invalid_method(matched));
    }
    let url_start = method_end + 1;
    if bytes.get(url_start).is_some_and(|&byte| byte == b' ') {
        return Err(Failure(
            "HPE_INVALID_URL",
            "Unexpected start char in url",
            Some(url_start),
        ));
    }
    let mut section = "Invalid char in url path";
    let mut url_end = None;
    for (index, &byte) in bytes.iter().enumerate().skip(url_start) {
        match byte {
            b' ' => {
                url_end = Some(index);
                break;
            }
            b'?' if section == "Invalid char in url path" => section = "Invalid char in url query",
            b'#' => section = "Invalid char in url fragment",
            0x21..=0x7e => {}
            b'\r' | b'\n' => {
                return Err(Failure(
                    "HPE_INVALID_CONSTANT",
                    "Expected HTTP/, RTSP/ or ICE/",
                    Some(index),
                ))
            }
            _ => return Err(Failure("HPE_INVALID_URL", section, Some(index))),
        }
    }
    let Some(url_end) = url_end else {
        return Ok(None);
    };
    let url = latin1(&bytes[url_start..url_end]);
    let protocol = url_end + 1;
    for (offset, &expected) in b"HTTP/".iter().enumerate() {
        match bytes.get(protocol + offset) {
            None => return Ok(None),
            Some(&byte) if byte == expected => {}
            Some(_) => {
                return Err(Failure(
                    "HPE_INVALID_CONSTANT",
                    "Expected HTTP/, RTSP/ or ICE/",
                    Some(protocol + offset),
                ))
            }
        }
    }
    let version = protocol + 5;
    let byte_at = |index: usize| bytes.get(index).copied();
    let Some((major, minor)) = http_version(bytes, version)? else {
        return Ok(None);
    };
    // `llhttp` reports a line feed without a carriage return at the position after it, and any
    // other byte at its own position.
    match byte_at(version + 3) {
        None => return Ok(None),
        Some(b'\r') => {}
        Some(byte) => {
            return Err(Failure(
                "HPE_INVALID_VERSION",
                "Expected CRLF after version",
                Some(version + 3 + usize::from(byte == b'\n')),
            ))
        }
    }
    match byte_at(version + 4) {
        None => return Ok(None),
        Some(b'\n') => {}
        Some(_) => {
            return Err(Failure(
                "HPE_STRICT",
                "Expected CRLF after version",
                Some(version + 4),
            ))
        }
    }
    Ok(Some(StartLine {
        method,
        url,
        status: 0,
        reason: String::new(),
        major,
        minor,
        end: version + 5,
    }))
}

/// The HTTP version at `at` in `bytes`, after `HTTP/`, or `None` if `bytes` ends before it does:
/// a digit, a dot and a digit, of 0.9, 1.0, 1.1 or 2.0.
fn http_version(bytes: &[u8], at: usize) -> Result<Option<(u8, u8)>, Failure> {
    let reasons = [
        "Invalid major version",
        "Expected dot",
        "Invalid minor version",
    ];
    for (offset, reason) in reasons.into_iter().enumerate() {
        match bytes.get(at + offset).copied() {
            None => return Ok(None),
            Some(byte)
                if (offset == 1) == (byte == b'.') && (offset == 1 || byte.is_ascii_digit()) => {}
            Some(_) => return Err(Failure("HPE_INVALID_VERSION", reason, Some(at + offset))),
        }
    }
    let major = bytes[at] - b'0';
    let minor = bytes[at + 2] - b'0';
    if !matches!((major, minor), (0, 9) | (1, 0) | (1, 1) | (2, 0)) {
        return Err(Failure(
            "HPE_INVALID_VERSION",
            "Invalid HTTP version",
            Some(at + 3),
        ));
    }
    Ok(Some((major, minor)))
}

/// The status line at the start of `bytes`, or `None` if `bytes` ends before it does, as `llhttp`
/// parses it in its strict mode: `HTTP/` with a version of 0.9, 1.0, 1.1 or 2.0, a space, a
/// status code of three digits, and a reason phrase after a space, which can be empty or left
/// out, ended by CRLF.
fn status_line(bytes: &[u8]) -> Result<Option<StartLine>, Failure> {
    for (offset, &expected) in b"HTTP/".iter().enumerate() {
        match bytes.get(offset) {
            None => return Ok(None),
            Some(&byte) if byte == expected => {}
            Some(_) => {
                return Err(Failure(
                    "HPE_INVALID_CONSTANT",
                    "Expected HTTP/, RTSP/ or ICE/",
                    Some(offset),
                ))
            }
        }
    }
    let Some((major, minor)) = http_version(bytes, 5)? else {
        return Ok(None);
    };
    match bytes.get(8) {
        None => return Ok(None),
        Some(b' ') => {}
        Some(_) => {
            return Err(Failure(
                "HPE_INVALID_VERSION",
                "Expected space after version",
                Some(8),
            ))
        }
    }
    let mut status = 0;
    for index in 9..12 {
        match bytes.get(index) {
            None => return Ok(None),
            Some(&byte) if byte.is_ascii_digit() => status = status * 10 + u16::from(byte - b'0'),
            Some(_) => {
                return Err(Failure(
                    "HPE_INVALID_STATUS",
                    "Invalid status code",
                    Some(index),
                ))
            }
        }
    }
    let reason_start = match bytes.get(12) {
        None => return Ok(None),
        Some(b' ') => 13,
        Some(b'\r') => 12,
        Some(&byte) => {
            // The reported position includes a line feed, as `llhttp`'s does.
            return Err(Failure(
                "HPE_INVALID_STATUS",
                "Invalid response status",
                Some(12 + usize::from(byte == b'\n')),
            ));
        }
    };
    let Some(index) = memchr::memchr2(b'\r', b'\n', &bytes[reason_start.min(bytes.len())..]) else {
        return Ok(None);
    };
    let line_break = reason_start + index;
    if bytes[line_break] == b'\n' {
        return Err(Failure(
            "HPE_CR_EXPECTED",
            "Missing expected CR after response line",
            Some(line_break + 1),
        ));
    }
    match bytes.get(line_break + 1) {
        None => return Ok(None),
        Some(b'\n') => {}
        Some(_) => {
            return Err(Failure(
                "HPE_STRICT",
                "Expected LF after CR",
                Some(line_break + 1),
            ))
        }
    }
    Ok(Some(StartLine {
        method: String::new(),
        url: String::new(),
        status,
        reason: latin1(&bytes[reason_start..line_break]),
        major,
        minor,
        end: line_break + 2,
    }))
}

/// The header fields at the start of `bytes`, up to the empty line that ends them, as `llhttp`
/// parses them with its lenient flags: names of token characters, values of any bytes but CR and
/// LF without the spaces and tabs around them, and lines ended by CRLF or LF. A line that starts
/// with a space or a tab continues the value of the field before it, after a space. `base` is the
/// position of `bytes` in the parser's buffer. Returns the length of the fields with the empty
/// line, or `None` if `bytes` ends before the empty line.
fn lenient_fields(bytes: &[u8], base: usize) -> Result<Option<(usize, Vec<Field<'_>>)>, Failure> {
    let mut fields = Vec::new();
    let mut position = 0;
    loop {
        let Some(index) = memchr::memchr(b'\n', &bytes[position..]) else {
            return Ok(None);
        };
        let line_end = position + index;
        let line = &bytes[position..line_end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return Ok(Some((line_end + 1, fields)));
        }
        let is_space = |byte: &u8| *byte == b' ' || *byte == b'\t';
        if line.first().is_some_and(is_space) {
            if let Some(field) = fields.last_mut() {
                let start = line
                    .iter()
                    .position(|byte| !is_space(byte))
                    .unwrap_or(line.len());
                let end = line
                    .iter()
                    .rposition(|byte| !is_space(byte))
                    .map_or(start, |end| end + 1);
                let value = field.value.to_mut();
                value.push(b' ');
                value.extend_from_slice(&line[start..end]);
                position = line_end + 1;
                continue;
            }
        }
        let name_end = line
            .iter()
            .position(|&byte| !is_token_byte(byte))
            .unwrap_or(line.len());
        if name_end == 0 || line.get(name_end) != Some(&b':') {
            return Err(Failure(
                "HPE_INVALID_HEADER_TOKEN",
                "Invalid header token",
                Some(base + position + name_end),
            ));
        }
        let value = &line[name_end + 1..];
        let start = value
            .iter()
            .position(|byte| !is_space(byte))
            .unwrap_or(value.len());
        let end = value
            .iter()
            .rposition(|byte| !is_space(byte))
            .map_or(start, |end| end + 1);
        fields.push(Field {
            name: &line[..name_end],
            value: std::borrow::Cow::Borrowed(&value[start..end]),
            at: base + position + name_end + 1 + start,
        });
        position = line_end + 1;
    }
}

/// `HPE_INVALID_METHOD` at the position `at`.
fn invalid_method(at: usize) -> Failure {
    Failure("HPE_INVALID_METHOD", "Invalid method encountered", Some(at))
}

/// Whether `byte` is a token character.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// The `llhttp` error for the header lines `bytes`, which `httparse` rejected, at the position of
/// the byte it rejects, counted from `base`: a name with a byte other than a token character, a
/// value with a control character other than a tab, a carriage return without a line feed, or a
/// line feed without a carriage return.
fn header_error(bytes: &[u8], base: usize) -> Failure {
    let mut position = 0;
    while position < bytes.len() && !bytes[position..].starts_with(b"\r\n") {
        let name_end = bytes[position..]
            .iter()
            .position(|&byte| !is_token_byte(byte))
            .map_or(bytes.len(), |index| position + index);
        if name_end == position || bytes.get(name_end) != Some(&b':') {
            return Failure(
                "HPE_INVALID_HEADER_TOKEN",
                "Invalid header token",
                Some(base + name_end),
            );
        }
        let mut index = name_end + 1;
        while let Some(&byte) = bytes.get(index) {
            match byte {
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => break,
                b'\r' => return lf_expected(base + index),
                b'\n' => {
                    return Failure(
                        "HPE_CR_EXPECTED",
                        "Missing expected CR after header value",
                        Some(base + index),
                    )
                }
                b'\t' => {}
                0x00..=0x1f | 0x7f => {
                    return Failure(
                        "HPE_INVALID_HEADER_TOKEN",
                        "Invalid header value char",
                        Some(base + index),
                    )
                }
                _ => {}
            }
            index += 1;
        }
        position = index + 2;
    }
    Failure(
        "HPE_INVALID_HEADER_TOKEN",
        "Invalid header token",
        Some(base),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The events `parser` returns for the bytes fed so far, up to the first error.
    fn drain(parser: &mut Parser) -> Result<Vec<Event>, ParseError> {
        let mut events = Vec::new();
        while let Some(event) = parser.next()? {
            events.push(event);
        }
        Ok(events)
    }

    /// The events `parser` returns for the bytes fed so far. Panics on a parse error.
    fn events(parser: &mut Parser) -> Vec<Event> {
        drain(parser).unwrap_or_else(|error| panic!("{error:?}"))
    }

    #[test]
    fn line_breaks_after_a_closing_message_are_consumed() {
        let mut parser = Parser::new(Kind::Request, 16384, None, false);
        parser.feed(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
        let received = events(&mut parser);
        assert!(matches!(received.last(), Some(Event::Complete(_))));
        for _ in 0..100 {
            parser.feed(&b"\r\n".repeat(512));
            assert!(events(&mut parser).is_empty());
            assert!(parser.buffer.is_empty());
        }
        parser.feed(b"\r\nX");
        let error = drain(&mut parser)
            .err()
            .expect("data after the closing message");
        assert_eq!(error.code, "HPE_CLOSED_CONNECTION");
        assert_eq!(error.bytes_parsed, 3);
    }

    #[test]
    fn trailers_beyond_the_header_limit_overflow() {
        let mut parser = Parser::new(Kind::Request, 64, None, false);
        parser.feed(b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n");
        assert_eq!(events(&mut parser).len(), 1);
        parser.feed(&b"A: b\r\n".repeat(64));
        let error = drain(&mut parser).err().expect("too many trailer lines");
        assert_eq!(error.code, "HPE_HEADER_OVERFLOW");
    }

    #[test]
    fn trailers_within_the_header_limit_are_parsed() {
        let mut parser = Parser::new(Kind::Request, 64, None, false);
        parser.feed(b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n");
        parser.feed(b"1\r\nx\r\n0\r\nA: b\r\nC: d\r\n\r\n");
        let received = events(&mut parser);
        let Some(Event::Complete(trailers)) = received.last() else {
            panic!("the message completes");
        };
        assert_eq!(
            trailers,
            &[("A".to_string(), "b".to_string()), ("C".into(), "d".into())]
        );
    }

    #[test]
    fn malformed_line_breaks_after_the_request_version() {
        let cases: [(&[u8], &str, usize); 4] = [
            (b"GET / HTTP/1.1X\r\n\r\n", "HPE_INVALID_VERSION", 14),
            (b"GET / HTTP/1.1 \r\n\r\n", "HPE_INVALID_VERSION", 14),
            (b"GET / HTTP/1.1\n\r\n", "HPE_INVALID_VERSION", 15),
            (b"GET / HTTP/1.1\rX\r\n\r\n", "HPE_STRICT", 15),
        ];
        for (request, code, bytes_parsed) in cases {
            let mut parser = Parser::new(Kind::Request, 16384, None, false);
            parser.feed(request);
            let error = drain(&mut parser).err().expect("a malformed request line");
            assert_eq!(
                (error.code, error.reason, error.bytes_parsed),
                (code, "Expected CRLF after version", bytes_parsed)
            );
        }
    }
}
