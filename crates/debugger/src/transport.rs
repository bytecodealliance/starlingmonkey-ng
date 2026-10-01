// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! DAP message framing. Each message is a `Content-Length: N` header, a blank line, and `N`
//! bytes of JSON. Other headers are ignored.

use std::fmt;
use std::io::{self, BufRead, Read, Write};

/// Upper bound on the buffer reserved before the declared body length has been received.
const MAX_PREALLOCATION: usize = 64 * 1024;

/// The largest accepted `Content-Length`.
pub const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;

/// The largest accepted header, in bytes up to and including the blank line that ends it.
pub const MAX_HEADER_LENGTH: usize = 64 * 1024;

/// Why a message could not be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The header has no `Content-Length` line.
    MissingContentLength,
    /// The `Content-Length` value is not a non-negative integer.
    InvalidContentLength,
    /// The `Content-Length` value exceeds [`MAX_CONTENT_LENGTH`].
    ContentTooLong,
    /// The header exceeds [`MAX_HEADER_LENGTH`] without ending.
    HeaderTooLong,
    /// The header or body is not UTF-8.
    NotUtf8,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FrameError::MissingContentLength => "DAP header without Content-Length",
            FrameError::InvalidContentLength => "invalid Content-Length",
            FrameError::ContentTooLong => "Content-Length exceeds 16 MiB",
            FrameError::HeaderTooLong => "DAP header exceeds 64 KiB",
            FrameError::NotUtf8 => "DAP message is not UTF-8",
        })
    }
}

impl std::error::Error for FrameError {}

impl FrameError {
    /// Whether the input cannot continue after this error, because the end of the offending
    /// message is unknown.
    pub fn ends_input(self) -> bool {
        matches!(self, FrameError::ContentTooLong | FrameError::HeaderTooLong)
    }
}

impl From<FrameError> for io::Error {
    fn from(error: FrameError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// The declared body length in `header_lines`. Other headers are ignored. If `Content-Length`
/// occurs more than once, the last occurrence is used. A length over [`MAX_CONTENT_LENGTH`] is
/// [`FrameError::ContentTooLong`].
fn content_length<'a>(
    header_lines: impl IntoIterator<Item = &'a str>,
) -> Result<usize, FrameError> {
    let mut content_length = None;
    for line in header_lines {
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| FrameError::InvalidContentLength)?,
            );
        }
    }
    match content_length {
        None => Err(FrameError::MissingContentLength),
        Some(length) if length > MAX_CONTENT_LENGTH => Err(FrameError::ContentTooLong),
        Some(length) => Ok(length),
    }
}

/// Read one message body. Returns `Ok(None)` if the input ends before a message starts. A header
/// longer than [`MAX_HEADER_LENGTH`] is [`FrameError::HeaderTooLong`].
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut header = String::new();
    let mut line = String::new();
    let mut header_length = 0;
    loop {
        line.clear();
        let remaining = MAX_HEADER_LENGTH - header_length;
        let read = reader
            .by_ref()
            .take(remaining as u64)
            .read_line(&mut line)?;
        header_length += read;
        if read == remaining && !line.ends_with('\n') {
            return Err(FrameError::HeaderTooLong.into());
        }
        if read == 0 {
            if !header.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "input ended inside a DAP header",
                ));
            }
            return Ok(None);
        }
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            if header.is_empty() {
                continue;
            }
            break;
        }
        header.push_str(&line);
    }
    let length = content_length(header.lines())?;
    let mut body = Vec::with_capacity(length.min(MAX_PREALLOCATION));
    reader.take(length as u64).read_to_end(&mut body)?;
    if body.len() != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "input ended inside a DAP message body",
        ));
    }
    String::from_utf8(body)
        .map(Some)
        .map_err(|_| FrameError::NotUtf8.into())
}

/// Assembles messages from byte chunks of arbitrary size. Feed input with `push` and take
/// complete messages with `next_message`.
#[derive(Default)]
pub struct Framer {
    buffer: Vec<u8>,
}

impl Framer {
    /// Append received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Remove and return the next complete message body, or `Ok(None)` if more bytes are
    /// needed. After an error the offending header (and body, if its length was known) has been
    /// discarded, so the next call continues with the following message. After an error for which
    /// [`FrameError::ends_input`] is `true`, the buffer is empty and the input cannot continue.
    pub fn next_message(&mut self) -> Result<Option<String>, FrameError> {
        let blank = self
            .buffer
            .iter()
            .take_while(|&&byte| byte == b'\r' || byte == b'\n')
            .count();
        self.buffer.drain(..blank);
        let Some(header_end) = header_end(&self.buffer[..self.buffer.len().min(MAX_HEADER_LENGTH)])
        else {
            if self.buffer.len() >= MAX_HEADER_LENGTH {
                self.buffer = Vec::new();
                return Err(FrameError::HeaderTooLong);
            }
            return Ok(None);
        };
        let length = std::str::from_utf8(&self.buffer[..header_end])
            .map_err(|_| FrameError::NotUtf8)
            .and_then(|header| content_length(header.lines()));
        let length = match length {
            Ok(length) => length,
            Err(error) => {
                if error.ends_input() {
                    self.buffer = Vec::new();
                } else {
                    self.buffer.drain(..header_end);
                }
                return Err(error);
            }
        };
        let Some(message_end) = header_end.checked_add(length) else {
            self.buffer.drain(..header_end);
            return Err(FrameError::InvalidContentLength);
        };
        if self.buffer.len() < message_end {
            return Ok(None);
        }
        let body = self.buffer[header_end..message_end].to_vec();
        self.buffer.drain(..message_end);
        String::from_utf8(body)
            .map(Some)
            .map_err(|_| FrameError::NotUtf8)
    }
}

/// The offset just past the blank line that ends the header in `buffer`. The blank line is
/// `\r\n\r\n` or `\n\n`.
fn header_end(buffer: &[u8]) -> Option<usize> {
    (0..buffer.len()).find_map(|i| {
        if buffer[i] != b'\n' {
            return None;
        }
        match buffer.get(i + 1..i + 3) {
            Some([b'\r', b'\n']) => Some(i + 3),
            Some([b'\n', _]) => Some(i + 2),
            Some(_) => None,
            None => (buffer.get(i + 1) == Some(&b'\n')).then_some(i + 2),
        }
    })
}

/// Write one message and flush.
pub fn write_message(writer: &mut impl Write, body: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}

/// Assigns the `seq` of outgoing messages, which DAP requires to count up from 1 across all
/// messages an adapter sends.
#[derive(Default)]
pub struct Sequencer {
    last: u64,
}

impl Sequencer {
    /// Add `seq`, set to the next number, as the first member of `body`, a JSON object without a
    /// `seq` member. `body` is not parsed. A body that does not start with `{` is returned
    /// unchanged.
    pub fn stamp(&mut self, body: &str) -> String {
        let Some(members) = body.strip_prefix('{') else {
            return body.to_string();
        };
        self.last += 1;
        let separator = if members.trim_start().starts_with('}') {
            ""
        } else {
            ","
        };
        format!("{{\"seq\":{}{separator}{members}", self.last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_messages_read_back() {
        let mut wire = Vec::new();
        write_message(&mut wire, r#"{"a":1}"#).unwrap();
        write_message(&mut wire, r#"{"b":"ü"}"#).unwrap();
        let mut reader = io::BufReader::new(&wire[..]);
        assert_eq!(
            read_message(&mut reader).unwrap().as_deref(),
            Some(r#"{"a":1}"#)
        );
        assert_eq!(
            read_message(&mut reader).unwrap().as_deref(),
            Some(r#"{"b":"ü"}"#)
        );
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn content_length_counts_bytes() {
        let mut wire = Vec::new();
        write_message(&mut wire, "ü").unwrap();
        assert!(wire.starts_with(b"Content-Length: 2\r\n\r\n"));
    }

    #[test]
    fn other_headers_are_ignored() {
        let wire = b"Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let mut reader = io::BufReader::new(&wire[..]);
        assert_eq!(read_message(&mut reader).unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn a_header_without_content_length_is_an_error() {
        let wire = b"Content-Type: application/json\r\n\r\n{}";
        let mut reader = io::BufReader::new(&wire[..]);
        assert_eq!(
            read_message(&mut reader).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn input_ending_inside_a_message_is_an_error() {
        let wire = b"Content-Length: 10\r\n\r\n{}";
        let mut reader = io::BufReader::new(&wire[..]);
        assert_eq!(
            read_message(&mut reader).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn the_largest_declared_length_with_a_short_body_is_an_error() {
        let wire = format!("Content-Length: {MAX_CONTENT_LENGTH}\r\n\r\n{{}}");
        let mut reader = io::BufReader::new(wire.as_bytes());
        assert_eq!(
            read_message(&mut reader).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_declared_length_over_the_maximum_is_an_error() {
        let wire = format!("Content-Length: {}\r\n\r\n{{}}", MAX_CONTENT_LENGTH + 1);
        let mut reader = io::BufReader::new(wire.as_bytes());
        let error = read_message(&mut reader).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = error.get_ref().and_then(|error| error.downcast_ref());
        assert_eq!(error, Some(&FrameError::ContentTooLong));
    }

    #[test]
    fn sequencer_numbers_messages_from_one() {
        let mut sequencer = Sequencer::default();
        assert_eq!(
            sequencer.stamp(r#"{"type":"event","body":{"b":1,"a":2}}"#),
            r#"{"seq":1,"type":"event","body":{"b":1,"a":2}}"#
        );
        assert_eq!(sequencer.stamp("{}"), r#"{"seq":2}"#);
        assert_eq!(sequencer.stamp("[1]"), "[1]");
        assert_eq!(sequencer.stamp(r#"{"a":1}"#), r#"{"seq":3,"a":1}"#);
    }

    #[test]
    fn a_header_over_the_maximum_length_is_an_error() {
        let line = format!("X: {}\r\n", "x".repeat(1000));
        let wire = line.repeat(MAX_HEADER_LENGTH / line.len() + 1);
        let mut reader = io::BufReader::new(wire.as_bytes());
        let error = read_message(&mut reader).unwrap_err();
        let error = error.get_ref().and_then(|error| error.downcast_ref());
        assert_eq!(error, Some(&FrameError::HeaderTooLong));
    }

    #[test]
    fn a_header_line_over_the_maximum_length_is_an_error() {
        let wire = "x".repeat(MAX_HEADER_LENGTH + 1);
        let mut reader = io::BufReader::new(wire.as_bytes());
        let error = read_message(&mut reader).unwrap_err();
        let error = error.get_ref().and_then(|error| error.downcast_ref());
        assert_eq!(error, Some(&FrameError::HeaderTooLong));
    }

    #[test]
    fn a_header_of_the_maximum_length_is_read() {
        let prefix = "Content-Length: 2\r\nX: ";
        let padding = "x".repeat(MAX_HEADER_LENGTH - prefix.len() - "\r\n\r\n".len());
        let wire = format!("{prefix}{padding}\r\n\r\n{{}}");
        let mut reader = io::BufReader::new(wire.as_bytes());
        assert_eq!(read_message(&mut reader).unwrap().as_deref(), Some("{}"));
        let mut framer = Framer::default();
        framer.push(wire.as_bytes());
        assert_eq!(framer.next_message().unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn framer_rejects_a_header_over_the_maximum_length_and_empties_its_buffer() {
        let mut framer = Framer::default();
        let line = b"X: 1\r\n";
        framer.push(&line.repeat(MAX_HEADER_LENGTH / line.len()));
        assert_eq!(framer.next_message(), Ok(None));
        framer.push(line);
        assert_eq!(framer.next_message(), Err(FrameError::HeaderTooLong));
        assert_eq!(framer.buffer.capacity(), 0);
    }

    #[test]
    fn framer_assembles_messages_split_across_pushes() {
        let mut wire = Vec::new();
        write_message(&mut wire, r#"{"a":1}"#).unwrap();
        write_message(&mut wire, r#"{"b":2}"#).unwrap();
        let mut framer = Framer::default();
        for byte in &wire {
            framer.push(std::slice::from_ref(byte));
        }
        assert_eq!(
            framer.next_message().unwrap().as_deref(),
            Some(r#"{"a":1}"#)
        );
        assert_eq!(
            framer.next_message().unwrap().as_deref(),
            Some(r#"{"b":2}"#)
        );
        assert_eq!(framer.next_message().unwrap(), None);
    }

    #[test]
    fn framer_waits_for_the_whole_body() {
        let mut framer = Framer::default();
        framer.push(b"Content-Length: 4\r\n\r\n{}");
        assert_eq!(framer.next_message().unwrap(), None);
        framer.push(b"{}");
        assert_eq!(framer.next_message().unwrap().as_deref(), Some("{}{}"));
    }

    #[test]
    fn framer_rejects_a_header_without_content_length() {
        let mut framer = Framer::default();
        framer.push(b"X: 1\r\n\r\n");
        assert!(matches!(
            framer.next_message(),
            Err(FrameError::MissingContentLength)
        ));
    }

    #[test]
    fn framer_rejects_an_invalid_content_length() {
        let mut framer = Framer::default();
        framer.push(b"Content-Length: x\r\n\r\n");
        assert!(matches!(
            framer.next_message(),
            Err(FrameError::InvalidContentLength)
        ));
    }

    #[test]
    fn framer_skips_leading_blank_lines_and_accepts_bare_newlines() {
        let mut framer = Framer::default();
        framer.push(b"\r\n\nContent-Length: 2\n\n{}");
        assert_eq!(framer.next_message().unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn framer_ignores_other_headers() {
        let mut framer = Framer::default();
        framer.push(b"Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(framer.next_message().unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn framer_rejects_a_body_that_is_not_utf8_and_recovers() {
        let mut framer = Framer::default();
        framer.push(b"Content-Length: 1\r\n\r\n\xff");
        framer.push(b"Content-Length: 2\r\n\r\n{}");
        assert!(matches!(framer.next_message(), Err(FrameError::NotUtf8)));
        assert_eq!(framer.next_message().unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn framer_does_not_reserve_the_declared_length() {
        let mut framer = Framer::default();
        framer.push(format!("Content-Length: {MAX_CONTENT_LENGTH}\r\n\r\n{{}}").as_bytes());
        assert_eq!(framer.next_message().unwrap(), None);
        assert!(framer.buffer.capacity() < MAX_PREALLOCATION);
    }

    #[test]
    fn framer_rejects_a_declared_length_over_the_maximum() {
        let mut framer = Framer::default();
        framer.push(format!("Content-Length: {}\r\n\r\n{{}}", MAX_CONTENT_LENGTH + 1).as_bytes());
        assert_eq!(framer.next_message(), Err(FrameError::ContentTooLong));
    }

    #[test]
    fn framer_keeps_a_body_made_of_line_breaks() {
        let body = "\r\n\r\n{}";
        let mut wire = Vec::new();
        write_message(&mut wire, body).unwrap();
        write_message(&mut wire, "{}").unwrap();
        for chunk_size in 1..=5 {
            let mut framer = Framer::default();
            let mut messages = Vec::new();
            for chunk in wire.chunks(chunk_size) {
                framer.push(chunk);
                while let Some(message) = framer.next_message().unwrap() {
                    messages.push(message);
                }
            }
            assert_eq!(messages, [body, "{}"], "chunk size {chunk_size}");
        }
    }

    #[test]
    fn framer_and_read_message_agree_on_header_terminators() {
        for terminator in ["\n\n", "\r\n\n", "\n\r\n", "\r\n\r\n"] {
            let wire = format!(
                "Content-Length: 7{terminator}{{\"a\":1}}Content-Length: 2{terminator}{{}}"
            );
            let mut reader = io::BufReader::new(wire.as_bytes());
            let mut from_reader = Vec::new();
            while let Some(message) = read_message(&mut reader).unwrap() {
                from_reader.push(message);
            }
            let mut framer = Framer::default();
            framer.push(wire.as_bytes());
            let mut from_framer = Vec::new();
            while let Some(message) = framer.next_message().unwrap() {
                from_framer.push(message);
            }
            assert_eq!(from_reader, [r#"{"a":1}"#, "{}"], "{terminator:?}");
            assert_eq!(from_framer, from_reader, "{terminator:?}");
        }
    }

    #[test]
    fn an_invalid_content_length_is_an_error() {
        let wire = b"Content-Length: abc\r\n\r\n{}";
        let mut reader = io::BufReader::new(&wire[..]);
        assert_eq!(
            read_message(&mut reader).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn input_ending_inside_a_header_is_an_error() {
        let wire = b"Content-Length: abc\r\n";
        let mut reader = io::BufReader::new(&wire[..]);
        assert_eq!(
            read_message(&mut reader).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
