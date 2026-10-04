// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The HTTP/1.x request parser of the listening `http.Server`: request heads through `httparse`,
//! and bodies framed by `Content-Length` or decoded from chunked transfer coding.
//!
//! [`Parser::feed`] appends received bytes, and each [`Parser::next`] returns the next event the
//! buffered bytes complete. Error codes and reasons are those of Node.js's `llhttp`. After an
//! error, each further chunk fed reports the error again.

/// The parsed head of a request.
pub(super) struct Head {
    pub method: String,
    pub url: String,
    /// The major and minor HTTP version: 0.9, 1.0, 1.1 or 2.0.
    pub major: u8,
    pub minor: u8,
    /// The header lines as received, with values decoded as Latin-1.
    pub headers: Vec<(String, String)>,
    /// Whether the request is a `CONNECT` request, or asks to upgrade the connection with an
    /// `Upgrade` header and an `upgrade` token in `Connection`.
    pub upgrade: bool,
    /// Whether the connection stays open after the message: HTTP/1.1 without `Connection: close`,
    /// or another version with `Connection: keep-alive`.
    pub keep_alive: bool,
}

/// What the buffered bytes complete.
pub(super) enum Event {
    /// A request head.
    Head(Head),
    /// Bytes of the body of the current request.
    Body(Vec<u8>),
    /// The end of the current request, with its trailer lines.
    Complete(Vec<(String, String)>),
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

/// How the body of the current request ends.
enum Body {
    /// After this many more bytes.
    Length(u64),
    /// Chunked transfer coding, at this point of a chunk.
    Chunked(Chunk),
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
    /// A request head.
    Head,
    /// The body of the current request.
    Body(Body),
    /// Nothing: the connection was upgraded.
    Done,
    /// Nothing but line breaks, after a message that closes the connection.
    Closed,
    /// Nothing, after this error. `true` while the last chunk fed hasn't reported it.
    Failed(ParseError, bool),
}

/// The longest chunk size line accepted, including chunk extensions.
const MAX_CHUNK_LINE: usize = 16 * 1024;

/// The parser of one connection's requests.
pub(super) struct Parser {
    /// The bytes received and not parsed yet.
    buffer: Vec<u8>,
    /// The position of the first byte of `buffer` in the connection's bytes.
    position: usize,
    /// The position of the first byte of the last chunk fed in the connection's bytes.
    chunk_start: usize,
    state: State,
    /// The largest request head accepted, in bytes.
    max_header_size: usize,
    /// The most header lines a request may have, or `None` for no limit.
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
        Parser::new(16384, None)
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
    /// A parser accepting request heads of up to `max_header_size` bytes with up to
    /// `max_headers` header lines.
    pub fn new(max_header_size: usize, max_headers: Option<usize>) -> Self {
        Parser {
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

    /// Stop parsing, and return the bytes received and not parsed.
    pub fn take_rest(&mut self) -> Vec<u8> {
        self.state = State::Done;
        self.position += self.buffer.len();
        std::mem::take(&mut self.buffer)
    }

    /// Check that the peer may end the connection now: between messages, or in a message whose
    /// body lasts until the end, which requests don't have.
    pub fn finish(&mut self) -> Result<(), ParseError> {
        match self.state {
            State::Head if !has_content(&self.buffer) => Ok(()),
            State::Done | State::Closed | State::Failed(..) => Ok(()),
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
            State::Done => Ok(None),
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
                None => Ok(None),
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
        // Line breaks before a request line are ignored.
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
        let Some(line) = request_line(&self.buffer)? else {
            if self.buffer.len() > self.max_header_size {
                return Err(overflow(self.buffer.len()));
            }
            return Ok(None);
        };
        let fields = &self.buffer[line.end..];
        let lines = memchr::memchr_iter(b'\n', fields).count() + 1;
        let mut slots = vec![httparse::EMPTY_HEADER; lines];
        let (head_end, count) = match httparse::parse_headers(fields, &mut slots) {
            Ok(httparse::Status::Complete((length, parsed))) => (line.end + length, parsed.len()),
            Ok(httparse::Status::Partial) => {
                if let Some(index) = bare_carriage_return(fields) {
                    return Err(lf_expected(line.end + index));
                }
                if self.buffer.len() > self.max_header_size {
                    return Err(overflow(self.buffer.len()));
                }
                return Ok(None);
            }
            Err(_) => {
                // The error is the first of the rejected header byte and a carriage return
                // without a line feed, which a later request in the buffer can have.
                let error = header_error(fields, line.end);
                return Err(match bare_carriage_return(fields) {
                    Some(index) if Some(line.end + index) < error.2 => {
                        lf_expected(line.end + index)
                    }
                    _ => error,
                });
            }
        };
        if let Some(index) = bare_carriage_return(&self.buffer[..head_end]) {
            return Err(lf_expected(index));
        }
        if head_end > self.max_header_size {
            return Err(overflow(self.max_header_size));
        }
        let parsed = &slots[..count];
        // The position in the buffer of the first byte of `bytes`, a slice of it.
        let offset = |bytes: &[u8]| bytes.as_ptr() as usize - self.buffer.as_ptr() as usize;
        if let Some(excess) = self.max_headers.and_then(|max| parsed.get(max)) {
            return Err(overflow(
                offset(excess.name.as_bytes()) + excess.name.len() + 1,
            ));
        }

        let mut connection_close = false;
        let mut connection_keep_alive = false;
        let mut connection_upgrade = false;
        let mut has_upgrade = false;
        let mut content_length: Option<u64> = None;
        let mut chunked = false;
        let mut transfer_encoding = false;
        for header in parsed {
            let name = header.name;
            let value = header.value;
            if name.eq_ignore_ascii_case("connection") {
                let value = latin1(value);
                connection_close |= has_token(&value, "close");
                connection_keep_alive |= has_token(&value, "keep-alive");
                connection_upgrade |= has_token(&value, "upgrade");
            } else if name.eq_ignore_ascii_case("upgrade") {
                has_upgrade = true;
            } else if name.eq_ignore_ascii_case("content-length") {
                if let Some(index) = value.iter().position(|byte| !byte.is_ascii_digit()) {
                    return Err(Failure(
                        "HPE_INVALID_CONTENT_LENGTH",
                        "Invalid character in Content-Length",
                        Some(offset(value) + index),
                    ));
                }
                if value.is_empty() {
                    return Err(Failure(
                        "HPE_INVALID_CONTENT_LENGTH",
                        "Invalid character in Content-Length",
                        Some(offset(value)),
                    ));
                }
                if content_length.is_some() {
                    return Err(Failure(
                        "HPE_UNEXPECTED_CONTENT_LENGTH",
                        "Duplicate Content-Length",
                        Some(offset(value)),
                    ));
                }
                let length = latin1(value).parse::<u64>().map_err(|_| {
                    Failure(
                        "HPE_INVALID_CONTENT_LENGTH",
                        "Content-Length overflow",
                        Some(offset(value)),
                    )
                })?;
                content_length = Some(length);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                transfer_encoding = true;
                let mut start = 0;
                for token in value.split(|&byte| byte == b',') {
                    if chunked {
                        return Err(Failure(
                            "HPE_INVALID_TRANSFER_ENCODING",
                            "Invalid `Transfer-Encoding` header value",
                            Some(offset(value) + start),
                        ));
                    }
                    chunked = latin1(token).trim().eq_ignore_ascii_case("chunked");
                    start += token.len() + 1;
                }
            }
        }
        let connect = line.method == "CONNECT";
        let upgrade = connect || (has_upgrade && connection_upgrade);
        let keep_alive = if line.major > 0 && line.minor > 0 {
            !connection_close
        } else {
            connection_keep_alive
        };
        let body = match (transfer_encoding, content_length) {
            (true, Some(_)) => {
                return Err(Failure(
                    "HPE_INVALID_TRANSFER_ENCODING",
                    "Transfer-Encoding can't be present with Content-Length",
                    Some(head_end),
                ))
            }
            (true, None) if !chunked => {
                return Err(Failure(
                    "HPE_INVALID_TRANSFER_ENCODING",
                    "Request has invalid `Transfer-Encoding`",
                    Some(head_end),
                ))
            }
            (true, None) => Body::Chunked(Chunk::Size),
            (false, length) => Body::Length(length.unwrap_or(0)),
        };
        let headers: Vec<(String, String)> = parsed
            .iter()
            .map(|header| (header.name.to_string(), latin1(header.value)))
            .collect();
        let RequestLine {
            method,
            url,
            major,
            minor,
            ..
        } = line;
        self.consume(head_end);
        self.headers_complete = true;
        self.closing = !keep_alive;
        self.state = if connect {
            State::Done
        } else {
            State::Body(body)
        };
        Ok(Some(Event::Head(Head {
            method,
            url,
            major,
            minor,
            headers,
            upgrade,
            keep_alive,
        })))
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
                    let lines = memchr::memchr_iter(b'\n', &self.buffer).count() + 1;
                    let mut headers = vec![httparse::EMPTY_HEADER; lines];
                    match httparse::parse_headers(&self.buffer, &mut headers) {
                        Ok(httparse::Status::Complete((length, parsed))) => {
                            let trailers = parsed
                                .iter()
                                .map(|header| (header.name.to_string(), latin1(header.value)))
                                .collect();
                            self.consume(length);
                            return Ok(Some(self.complete(trailers)));
                        }
                        Ok(httparse::Status::Partial) => {
                            if self.buffer.len() > self.max_header_size {
                                return Err(Failure(
                                    "HPE_HEADER_OVERFLOW",
                                    "Header overflow",
                                    Some(self.buffer.len()),
                                ));
                            }
                            return Ok(None);
                        }
                        Err(_) => return Err(header_error(&self.buffer, 0)),
                    }
                }
            }
        }
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

/// A parsed request line.
struct RequestLine {
    method: String,
    url: String,
    major: u8,
    minor: u8,
    /// The position after the line's line break.
    end: usize,
}

/// The request line at the start of `bytes`, or `None` if `bytes` ends before it does, as
/// `llhttp` parses it in its strict mode: a method of `METHODS`, a URL of visible ASCII characters,
/// and HTTP/0.9, 1.0, 1.1 or 2.0, separated by single spaces and ended by CRLF.
fn request_line(bytes: &[u8]) -> Result<Option<RequestLine>, Failure> {
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
    let reasons = [
        "Invalid major version",
        "Expected dot",
        "Invalid minor version",
    ];
    for (offset, reason) in reasons.into_iter().enumerate() {
        match byte_at(version + offset) {
            None => return Ok(None),
            Some(byte)
                if (offset == 1) == (byte == b'.') && (offset == 1 || byte.is_ascii_digit()) => {}
            Some(_) => {
                return Err(Failure(
                    "HPE_INVALID_VERSION",
                    reason,
                    Some(version + offset),
                ))
            }
        }
    }
    let major = bytes[version] - b'0';
    let minor = bytes[version + 2] - b'0';
    if !matches!((major, minor), (0, 9) | (1, 0) | (1, 1) | (2, 0)) {
        return Err(Failure(
            "HPE_INVALID_VERSION",
            "Invalid HTTP version",
            Some(version + 3),
        ));
    }
    match byte_at(version + 3) {
        None => return Ok(None),
        Some(b'\r') => {}
        Some(_) => {
            return Err(Failure(
                "HPE_INVALID_VERSION",
                "Expected CRLF after version",
                Some(version + 4),
            ))
        }
    }
    match byte_at(version + 4) {
        None => return Ok(None),
        Some(b'\n') => {}
        Some(_) => {
            return Err(Failure(
                "HPE_LF_EXPECTED",
                "Expected LF after version",
                Some(version + 5),
            ))
        }
    }
    Ok(Some(RequestLine {
        method,
        url,
        major,
        minor,
        end: version + 5,
    }))
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
