// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Standalone algorithms from <https://fetch.spec.whatwg.org/>

use std::borrow::Cow;

use crate::body_mixin::{BodyInit, BodyMixin};
use crate::headers::{Guard, HeaderList, Headers, HeadersInit};
use crate::incoming_body::HostBackedBodyOwner;
use crate::response::{is_null_body_status, Response, ResponseInit, ResponseRecord};
use js::conversion::ToJSVal;
use js::error::{throw_type_error, ExnThrown, RangeError};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{ArrayBuffer, Promise, Uint8Array};
use smallvec::SmallVec;
use web_streams::readable::readable_stream::ReadableStream;

/// The source of a [`Body`]'s bytes.
///
/// <https://fetch.spec.whatwg.org/#concept-body-source>
#[derive(Debug, Clone, Default)]
pub enum BodySource {
    /// No byte source — e.g. the body is a user-provided `ReadableStream`, whose
    /// bytes are not available without reading the stream.
    #[default]
    Null,
    /// An in-memory byte sequence (from a string/buffer/URLSearchParams body).
    /// Refcounted: cloning a body shares the bytes instead of copying them.
    Bytes(bytes::Bytes),
}

/// A body's data: source bytes and length, plus a consumed flag.
///
/// <https://fetch.spec.whatwg.org/#concept-body>
///
/// The spec's _body_ is an internal struct with no WebIDL surface. The actual
/// stream is stored separately on the owning `Request`/`Response` object to
/// simplify rooting.
#[derive(Debug, Clone, Default)]
pub struct Body {
    /// <https://fetch.spec.whatwg.org/#concept-body-source>
    pub source: BodySource,
    /// <https://fetch.spec.whatwg.org/#concept-body-total-bytes>
    /// A length (in bytes) if known, or null.
    pub length: Option<u64>,
    /// Whether the body has been consumed (its stream disturbed) via a
    /// byte-sequence fast-path read, which does not touch the stream object.
    pub source_disturbed: bool,
}

/// `extract a body`'s _object_: a byte sequence or a [`BodyInit`] object.
pub(crate) enum BodyInitOrBytes<'a> {
    BodyInit(BodyInit<'a>),
    Bytes(bytes::Bytes),
}

/// <https://fetch.spec.whatwg.org/#concept-bodyinit-extract>
/// To extract a body with type from a byte sequence or [`BodyInit`] object _object_,
/// with an optional boolean _keepalive_ (default `false`), run these steps:
pub(crate) fn extract_body<'r>(
    scope: &'r Scope<'_>,
    object: BodyInitOrBytes<'r>,
    keepalive: bool,
) -> Result<BodyWithType<'r>, ExnThrown> {
    // Step 1: Let _stream_ be null.
    let mut stream: Option<ReadableStream<'r>> = None;
    // Step 2: If _object_ is a `ReadableStream` object, then set _stream_ to _object_.
    // (In the Step 10 `ReadableStream` arm below, after that arm's checks.)
    // Step 3: Otherwise, if _object_ is a `Blob` object, set _stream_ to the result of running
    //     _object_’s `get stream`.
    // Not yet supported.
    // Step 4: Otherwise, set _stream_ to a `new` `ReadableStream` object, and `set up` _stream_
    //     with byte reading support.
    // Note: For byte-sequence sources the stream is materialized lazily from `source` (see the
    // `body` getter), so no stream is created here for the non-`ReadableStream` cases.

    // Step 5: `Assert`: _stream_ is a `ReadableStream` object.
    // n/a, see Step 4.
    // Step 6: Let _action_ be null.
    // n/a: byte-sequence sources are read synchronously below.
    // Step 7: Let _source_ be null.
    let mut source = BodySource::Null;
    // Step 8: Let _length_ be null.
    // (Computed from _source_ after Step 10, see Step 11.)
    // Step 9: Let _type_ be null.
    let mut content_type: Option<Cow<'static, str>> = None;

    // Step 10: Switch on _object_:
    // Note: Blob and FormData are not yet supported (the `Blob` and `FormData` arms of Step 10).
    match object {
        // Step 10 `byte sequence`: Set _source_ to _object_.
        BodyInitOrBytes::Bytes(bytes) => {
            source = BodySource::Bytes(bytes);
        }
        // Step 10 `BufferSource`: Set _source_ to a `copy of the bytes` held by _object_.
        BodyInitOrBytes::BodyInit(BodyInit::ArrayBuffer(buffer)) => {
            source = BodySource::Bytes(bytes::Bytes::from(buffer.copy_bytes(scope)?));
        }
        BodyInitOrBytes::BodyInit(BodyInit::ArrayBufferView(view)) => {
            source = BodySource::Bytes(bytes::Bytes::from(view.copy_bytes(scope)?));
        }
        // Step 10 `URLSearchParams`: Set _source_ to the result of running the
        //     `application/x-www-form-urlencoded` serializer` with _object_’s `list`. Set _type_
        //     to `application/x-www-form-urlencoded;charset=UTF-8`.
        BodyInitOrBytes::BodyInit(BodyInit::URLSearchParams(params)) => {
            let serialized = params.to_string();
            source = BodySource::Bytes(bytes::Bytes::from(serialized.into_bytes()));
            content_type = Some(Cow::Borrowed(
                "application/x-www-form-urlencoded;charset=UTF-8",
            ));
        }
        // Step 10 `scalar value string`: Set _source_ to the `UTF-8 encoding` of _object_. Set
        //     _type_ to `text/plain;charset=UTF-8`.
        BodyInitOrBytes::BodyInit(BodyInit::USVString(string)) => {
            source = BodySource::Bytes(bytes::Bytes::from(string.into_bytes()));
            content_type = Some(Cow::Borrowed("text/plain;charset=UTF-8"));
        }
        // Step 10 `ReadableStream`: If _keepalive_ is true, then `throw` a `TypeError`. If
        //     _object_ is `disturbed` or `locked`, then `throw` a `TypeError`.
        BodyInitOrBytes::BodyInit(BodyInit::Stream(rs)) => {
            if keepalive {
                return Err(throw_type_error(
                    scope,
                    c"a keepalive request cannot have a ReadableStream body",
                ));
            }
            if rs.is_locked() || rs.is_disturbed() {
                return Err(throw_type_error(
                    scope,
                    c"a ReadableStream body must be neither disturbed nor locked",
                ));
            }
            // Step 2: Set _stream_ to _object_.
            stream = Some(rs);
        }
    }

    // Step 11: If _source_ is a `byte sequence`, then set _action_ to a step that returns _source_
    //     and _length_ to _source_’s `length`.
    let length = match &source {
        BodySource::Bytes(bytes) => Some(bytes.len() as u64),
        BodySource::Null => None,
    };
    // Step 12: If _action_ is non-null, then run these steps `in parallel`:
    // Step 12.1: Run _action_. Whenever one or more bytes are available and _stream_ is not
    //     `errored`, `enqueue` the result of `creating` a `Uint8Array` from the available bytes
    //     into _stream_. When running _action_ is done, `close` _stream_.
    // Note: The byte sequence is already in `source`, the stream is materialized lazily.
    // Step 13: Let _body_ be a `body` whose `stream` is _stream_, `source` is _source_, and
    //     `length` is _length_.
    // Note: The stream is returned alongside the body record, to be stored on the body owner.
    let body = Body {
        source,
        length,
        source_disturbed: false,
    };
    // Step 14: Return (_body_, _type_).
    Ok((body, stream, content_type))
}

/// <https://fetch.spec.whatwg.org/#header-list-contains>
/// A header list list contains a header name name if list contains a header whose name is a byte-case-insensitive match for name.
pub(crate) fn contains(list: &HeaderList, name: &str) -> bool {
    // Step 1: A header list list contains a header name name if list contains a header whose name
    //     is a byte-case-insensitive match for name.
    list.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// <https://fetch.spec.whatwg.org/#concept-header-list-get>
/// To get a header name name from a header list list, run these steps. They return null or a header value.
pub(crate) fn get_header_for_name<'l>(list: &'l HeaderList, name: &str) -> Option<Cow<'l, str>> {
    debug_assert!(is_header_name(name));

    // Step 1: If _list_ `does not contain` _name_, then return null.
    // Subsumed by the search below: no matching header yields `None`.
    // Step 2: Return the `values` of all `headers` in _list_ whose `name` is a
    //     `byte-case-insensitive` match for _name_, separated from each other by 0x2C 0x20, in
    //     order.
    // A single value — the common case — is borrowed rather than combined into a fresh allocation.
    // We lowercase _name_ once, so each comparison lowercases only the list's names.
    let lowercase: SmallVec<[u8; 64]> = name.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let mut values = list
        .iter()
        .filter(|(n, _)| {
            n.len() == lowercase.len()
                && n.bytes()
                    .zip(&lowercase)
                    .all(|(a, &b)| a.to_ascii_lowercase() == b)
        })
        .map(|(_, v)| v.as_str());
    let first = values.next()?;
    let Some(second) = values.next() else {
        return Some(Cow::Borrowed(first));
    };
    let mut combined = String::with_capacity(first.len() + second.len() + 2);
    combined.push_str(first);
    for value in std::iter::once(second).chain(values) {
        combined.push_str(", ");
        combined.push_str(value);
    }
    Some(Cow::Owned(combined))
}

/// <https://fetch.spec.whatwg.org/#concept-header-list-append>
/// To append a header (name, value) to a header list _list_:
pub(crate) fn append_a_header(list: &mut HeaderList, name: String, value: String) {
    debug_assert!(is_header_name(name.as_str()));
    debug_assert!(is_header_value(value.as_str()));

    // Step 1: If _list_ `contains` _name_, then set _name_ to the first such `header`’s `name`.
    //     This reuses the casing of the `name` of the `header` already in _list_, if any. If there
    //     are multiple matched `headers` their `names` will all be identical.
    let name = list
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(&name))
        .map(|(n, _)| n.clone())
        .unwrap_or(name);
    // Step 2: `Append` (_name_, _value_) to _list_.
    list.push((name, value));
}

/// <https://fetch.spec.whatwg.org/#concept-header-list-delete>
/// To delete a header name _name_ from a header list _list_, remove all headers whose name is a
/// byte-case-insensitive match for _name_ from _list_.
pub(crate) fn delete_header(list: &mut HeaderList, name: &str) {
    debug_assert!(is_header_name(name));

    // Step 1: To delete a header name name from a header list list, remove all headers whose name
    //     is a byte-case-insensitive match for name from list.
    list.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
}

/// <https://fetch.spec.whatwg.org/#concept-header-list-set>
/// To set a header (name, value) in a header list _list_:
pub(crate) fn set_a_header(list: &mut HeaderList, name: String, value: String) {
    debug_assert!(is_header_name(name.as_str()));
    debug_assert!(is_header_value(value.as_str()));

    // Step 1: If _list_ `contains` _name_, then set the `value` of the first such `header` to
    //     _value_ and `remove` the others.
    if let Some(first) = list.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name)) {
        list[first].1 = value;
        let mut seen_first = false;
        list.retain(|(n, _)| {
            if !n.eq_ignore_ascii_case(&name) {
                return true;
            }
            if seen_first {
                false
            } else {
                seen_first = true;
                true
            }
        });
    } else {
        // Step 2: Otherwise, `append` (_name_, _value_) to _list_.
        list.push((name, value));
    }
}

/// <https://fetch.spec.whatwg.org/#convert-header-names-to-a-sorted-lowercase-set>
/// To convert header names to a sorted-lowercase set, given a list of names headerNames,
/// run these steps. They return an ordered set of header names.
pub(crate) fn convert_header_names_to_sorted_lowercase_set<'a>(
    header_names: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    // Step 1: Let _headerNamesSet_ be a new `ordered set`.
    let mut set: Vec<String> = Vec::new();
    // Step 2: `For each` _name_ of _headerNames_, `append` the result of `byte-lowercasing` _name_
    //     to _headerNamesSet_.
    for name in header_names {
        let lower = name.to_ascii_lowercase();
        if !set.contains(&lower) {
            set.push(lower);
        }
    }
    // Step 3: Return the result of `sorting` _headerNamesSet_ in ascending order with `byte less
    //     than`.
    set.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    set
}

/// <https://fetch.spec.whatwg.org/#concept-header-list-sort-and-combine>
/// To sort and combine a header list list, run these steps. They return a header list.
pub(crate) fn sort_and_combine_a_header_list(list: &HeaderList) -> HeaderList {
    // Step 1: Let _headers_ be a `header list`.
    let mut headers: HeaderList = Vec::new();
    // Step 2: Let _names_ be the result of `convert header names to a sorted-lowercase set` with
    //     all the `names` of the `headers` in _list_.
    let names = convert_header_names_to_sorted_lowercase_set(list.iter().map(|(n, _)| n.as_str()));
    // Step 3: `For each` _name_ of _names_:
    for name in names {
        // Step 3.1: If _name_ is ``set-cookie``, then:
        if name == "set-cookie" {
            // Step 3.1.1: Let _values_ be a list of all `values` of `headers` in _list_ whose
            //     `name` is a `byte-case-insensitive` match for _name_, in order.
            // Step 3.1.2: `For each` _value_ of _values_:
            // Step 3.1.2.1: `Append` (_name_, _value_) to _headers_.
            for (n, v) in list.iter() {
                if n.eq_ignore_ascii_case(&name) {
                    headers.push((name.clone(), v.clone()));
                }
            }
        } else {
            // Step 3.2: Otherwise:
            // Step 3.2.1: Let _value_ be the result of `getting` _name_ from _list_.
            // Step 3.2.2: `Assert`: _value_ is non-null.
            // Step 3.2.3: `Append` (_name_, _value_) to _headers_.
            let value = get_header_for_name(list, &name)
                .expect("name is in list")
                .into_owned();
            headers.push((name, value));
        }
    }
    // Step 4: Return _headers_.
    headers
}

/// <https://fetch.spec.whatwg.org/#concept-header-value-normalize>
/// To normalize a byte sequence potentialValue, remove any leading and trailing HTTP whitespace bytes from potentialValue.
pub(crate) fn normalize_byte_sequence(potential_value: String) -> String {
    // Step 1: To normalize a byte sequence potentialValue, remove any leading and trailing HTTP
    //     whitespace bytes from potentialValue.
    // HTTP whitespace is 0x09 (HT), 0x0A (LF), 0x0D (CR), or 0x20 (SP).
    let trimmed = potential_value.trim_matches(['\t', '\n', '\r', ' ']);
    if trimmed.len() == potential_value.len() {
        potential_value
    } else {
        trimmed.to_string()
    }
}

/// The byte set `CORS-safelisted request-header` allows in ``Accept-Language`` /
/// ``Content-Language`` values: ASCII alphanumerics, 0x20 (SP), 0x2A (*), 0x2C (,),
/// 0x2D (-), 0x2E (.), 0x3B (;), or 0x3D (=).
fn is_safelisted_language_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')
}

/// Whether `essence` is one of the essences `CORS-safelisted request-header` allows
/// for ``Content-Type``: `application/x-www-form-urlencoded`, `multipart/form-data`,
/// or `text/plain`.
fn is_safelisted_content_type_essence(essence: &str) -> bool {
    essence.eq_ignore_ascii_case("application/x-www-form-urlencoded")
        || essence.eq_ignore_ascii_case("multipart/form-data")
        || essence.eq_ignore_ascii_case("text/plain")
}

/// <https://fetch.spec.whatwg.org/#cors-safelisted-request-header>
/// To determine whether a header (name, value) is a CORS-safelisted request-header, run these steps:
pub(crate) fn is_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    // Step 1: If _value_’s `length` is greater than 128, then return false.
    // (A byte sequence is held as a string of code points ≤ U+00FF, one per byte.)
    if value.chars().count() > 128 {
        return false;
    }

    // Step 2: `Byte-lowercase` _name_ and switch on the result:
    // (compared case-insensitively rather than allocating a lowercased copy)
    // Step 3: Return true.
    // Step 2 `accept`: If _value_ contains a `CORS-unsafe request-header byte`, then return false.
    if name.eq_ignore_ascii_case("accept") {
        !value.bytes().any(is_cors_unsafe_request_header_byte)
    }
    // Step 2 `accept-language`, `content-language`: If _value_ contains a byte that is not in the
    //     range 0x30 (0) to 0x39 (9), inclusive, is not in the range 0x41 (A) to 0x5A (Z),
    //     inclusive, is not in the range 0x61 (a) to 0x7A (z), inclusive, and is not 0x20 (SP),
    //     0x2A (*), 0x2C (,), 0x2D (-), 0x2E (.), 0x3B (;), or 0x3D (=), then return false.
    else if name.eq_ignore_ascii_case("accept-language")
        || name.eq_ignore_ascii_case("content-language")
    {
        value.bytes().all(is_safelisted_language_byte)
    } else if name.eq_ignore_ascii_case("content-type") {
        // Step 2 `content-type`.1: If _value_ contains a `CORS-unsafe request-header byte`, then
        //     return false.
        if value.bytes().any(is_cors_unsafe_request_header_byte) {
            return false;
        }

        // Step 2 `content-type`.2: Let _mimeType_ be the result of `parsing` the result of
        //     `isomorphic decoding` _value_.
        // Step 2 `content-type`.3: If _mimeType_ is failure, then return false.
        // Step 2 `content-type`.4: If _mimeType_’s `essence` is not
        //     "`application/x-www-form-urlencoded`", "`multipart/form-data`", or "`text/plain`",
        //     then return false.
        let essence = value.split(';').next().unwrap_or("").trim();
        is_safelisted_content_type_essence(essence)
    }
    // Step 2 `range`.1: Let _rangeValue_ be the result of `parsing a single range header value`
    //     given _value_ and false.
    // Step 2 `range`.2: If _rangeValue_ is failure, then return false.
    // Step 2 `range`.3: If _rangeValue_[0] is null, then return false. As web browsers have
    //     historically not emitted ranges such as ``bytes=-500`` this algorithm does not safelist
    //     them.
    else if name.eq_ignore_ascii_case("range") {
        eprintln!("TODO: support range header");
        false
    }
    // Step 2 Otherwise: Return false.
    // Divergence: returns true. Unreachable in practice — the sole caller,
    // `is_no_cors_safelisted_request_header`, only passes a `no-CORS-safelisted request-header
    // name`, and every one of those has an arm above.
    else {
        true
    }
}

/// <https://fetch.spec.whatwg.org/#forbidden-request-header>
/// A header (name, value) is forbidden request-header if these steps return true:
pub(crate) fn forbidden_request_header(name: &str, value: &str) -> bool {
    // Step 1: If _name_ is a `byte-case-insensitive` match for one of:
    const FORBIDDEN_NAMES: &[&str] = &[
        "accept-charset",
        "accept-encoding",
        "access-control-request-headers",
        "access-control-request-method",
        "connection",
        "content-length",
        "cookie",
        "cookie2",
        "date",
        "dnt",
        "expect",
        "host",
        "keep-alive",
        "origin",
        "referer",
        "set-cookie",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "via",
    ];
    // Step 1 (continued): then return true.
    if FORBIDDEN_NAMES
        .iter()
        .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
    {
        return true;
    }
    // Step 2: If _name_ when `byte-lowercased` `starts with` ``proxy-`` or ``sec-``, then return
    //     true.
    if name
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("proxy-"))
        || name
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sec-"))
    {
        return true;
    }
    // Step 3: If _name_ is a `byte-case-insensitive` match for one of:
    //   - ``X-HTTP-Method``
    //   - ``X-HTTP-Method-Override``
    //   - ``X-Method-Override``
    //     then:
    if name.eq_ignore_ascii_case("x-http-method")
        || name.eq_ignore_ascii_case("x-http-method-override")
        || name.eq_ignore_ascii_case("x-method-override")
    {
        // Step 3.1: Let _parsedValues_ be the result of `getting, decoding, and splitting` _value_.
        // Step 3.2: `For each` _method_ of _parsedValues_: if the `isomorphic encoding` of _method_
        //     is a `forbidden method`, then return true.
        // (inlined)
        // [inlined get, decode, and split](https://fetch.spec.whatwg.org/#header-value-get-decode-and-split):
        // split on 0x2C (,), trimming HTTP tab or space — without the quoted-string handling of the
        // full algorithm, which cannot matter here: a `method` token contains neither `,` nor `"`,
        // so a quoted or comma-containing piece can never match a `forbidden method`.
        for method in value.split(',') {
            if is_forbidden_method(method.trim_matches([' ', '\t'])) {
                return true;
            }
        }
    }
    // Step 4: Return false.
    false
}

/// <https://fetch.spec.whatwg.org/#simple-range-header-value>
/// To parse a single range header value from a byte sequence value and a boolean allowWhitespace,
/// run these steps:
#[allow(dead_code)]
pub(crate) fn parse_a_single_range_header_value() {
    // Step 1: Let _data_ be the `isomorphic decoding` of _value_.
    // Step 2: If _data_ does not `start with` "`bytes`", then return failure.
    // Step 3: Let _position_ be a `position variable` for _data_, initially pointing at the 5th
    //     `code point` of _data_.
    // Step 4: If _allowWhitespace_ is true, `collect a sequence of code points` that are `HTTP tab
    //     or space`, from _data_ given _position_.
    // Step 5: If the `code point` at _position_ within _data_ is not U+003D (=), then return
    //     failure.
    // Step 6: Advance _position_ by 1.
    // Step 7: If _allowWhitespace_ is true, `collect a sequence of code points` that are `HTTP tab
    //     or space`, from _data_ given _position_.
    // Step 8: Let _rangeStart_ be the result of `collecting a sequence of code points` that are
    //     `ASCII digits`, from _data_ given _position_.
    // Step 9: Let _rangeStartValue_ be _rangeStart_, interpreted as decimal number, if _rangeStart_
    //     is not the empty string; otherwise null.
    // Step 10: If _allowWhitespace_ is true, `collect a sequence of code points` that are `HTTP tab
    //     or space`, from _data_ given _position_.
    // Step 11: If the `code point` at _position_ within _data_ is not U+002D (-), then return
    //     failure.
    // Step 12: Advance _position_ by 1.
    // Step 13: If _allowWhitespace_ is true, `collect a sequence of code points` that are `HTTP tab
    //     or space`, from _data_ given _position_.
    // Step 14: Let _rangeEnd_ be the result of `collecting a sequence of code points` that are
    //     `ASCII digits`, from _data_ given _position_.
    // Step 15: Let _rangeEndValue_ be _rangeEnd_, interpreted as decimal number, if _rangeEnd_ is
    //     not the empty string; otherwise null.
    // Step 16: If _position_ is not past the end of _data_, then return failure.
    // Step 17: If _rangeEndValue_ and _rangeStartValue_ are null, then return failure.
    // Step 18: If _rangeStartValue_ and _rangeEndValue_ are numbers, and _rangeStartValue_ is
    //     greater than _rangeEndValue_, then return failure.
    // Step 19: Return (_rangeStartValue_, _rangeEndValue_). The range end or start can be omitted,
    //     e.g., ``bytes=0-`` or ``bytes=-500`` are valid ranges.
    todo!("Needed for Range requests")
}

/// <https://fetch.spec.whatwg.org/#concept-body-clone>
/// To clone a body _body_, run these steps:
///
/// The caller supplies the body's already-materialized `stream` (in this codebase
/// a body's stream is filled lazily; the caller resolves the host/byte source to a
/// concrete `ReadableStream` before teeing). The function tees that stream and
/// returns both branches together with the cloned body record; the caller wires
/// `out1` back into the source's stream slot (Step 2) and gives the clone `out2`,
/// since the two stream slots live on the owning `Request`/`Response` interface
/// objects, not on the `Body` record.
pub(crate) fn clone_a_body<'r>(
    scope: &'r Scope<'_>,
    body_owner: &impl HostBackedBodyOwner,
    stream: &ReadableStream<'_>,
) -> Result<(ReadableStream<'r>, Body), ExnThrown> {
    let body = body_owner.body_record().expect("Owner has a body");

    // Step 1: Let « _out1_, _out2_ » be the result of `teeing` _body_’s `stream`.
    let (out1, out2) = stream.tee(scope, true)?;

    // Step 2: Set _body_’s `stream` to _out1_.
    body_owner.replace_body_stream_after_tee(out1);

    // Step 3: Return a `body` whose `stream` is _out2_ and other members are copied from _body_.
    // _out2_ is returned alongside the cloned record. The source bytes now live in the teed
    // streams, so the cloned body's `source` is null (it reads via _out2_); the length is copied.
    let cloned_body = Body {
        source: BodySource::Null,
        length: body.length,
        source_disturbed: false,
    };
    Ok((out2, cloned_body))
}

/// <https://fetch.spec.whatwg.org/#concept-header-extract-mime-type>
/// To extract a MIME type from a header list headers, run these steps. They return failure or a MIME type.
#[allow(dead_code)]
pub(crate) fn extract_a_mime_type() {
    // Step 1: Let _charset_ be null.
    // Step 2: Let _essence_ be null.
    // Step 3: Let _mimeType_ be null.
    // Step 4: Let _values_ be the result of `getting, decoding, and splitting` ``Content-Type``
    //     from _headers_.
    // Step 5: If _values_ is null, then return failure.
    // Step 6: `For each` _value_ of _values_:
    // Step 6.1: Let _temporaryMimeType_ be the result of `parsing` _value_.
    // Step 6.2: If _temporaryMimeType_ is failure or its `essence` is "`*/*`", then `continue`.
    // Step 6.3: Set _mimeType_ to _temporaryMimeType_.
    // Step 6.4: If _mimeType_’s `essence` is not _essence_, then:
    // Step 6.4.1: Set _charset_ to null.
    // Step 6.4.2: If _mimeType_’s `parameters`["`charset`"] `exists`, then set _charset_ to
    //     _mimeType_’s `parameters`["`charset`"].
    // Step 6.4.3: Set _essence_ to _mimeType_’s `essence`.
    // Step 6.5: Otherwise, if _mimeType_’s `parameters`["`charset`"] does not `exist`, and
    //     _charset_ is non-null, set _mimeType_’s `parameters`["`charset`"] to _charset_.
    // Step 7: If _mimeType_ is null, then return failure.
    // Step 8: Return _mimeType_.
    todo!("Needed for FormData/Blob")
}

/// Whether `byte` is an HTTP token byte (RFC 9110 `tchar`): `!#$%&'*+-.^_`|~`,
/// DIGIT, or ALPHA.
pub(crate) fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// <https://fetch.spec.whatwg.org/#header-name>
/// A header name is a byte sequence that matches the `field-name` token production.
pub(crate) fn is_header_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_http_token_byte)
}

/// <https://fetch.spec.whatwg.org/#header-value>
/// A header value has no leading or trailing HTTP tab or space bytes and contains no 0x00
/// (NUL), 0x0A (LF), or 0x0D (CR) bytes.
pub(crate) fn is_header_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    if let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) {
        if matches!(first, b'\t' | b' ') || matches!(last, b'\t' | b' ') {
            return false;
        }
    }
    !bytes.iter().any(|&b| matches!(b, 0x00 | 0x0A | 0x0D))
}

/// <https://fetch.spec.whatwg.org/#forbidden-response-header-name>
/// A forbidden response-header name is a byte-case-insensitive match for `Set-Cookie` or
/// `Set-Cookie2`.
pub(crate) fn is_forbidden_response_header_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("set-cookie") || name.eq_ignore_ascii_case("set-cookie2")
}

/// <https://fetch.spec.whatwg.org/#forbidden-method>
/// A forbidden method is a byte-case-insensitive match for `CONNECT`, `TRACE`, or `TRACK`.
pub(crate) fn is_forbidden_method(method: &str) -> bool {
    method.eq_ignore_ascii_case("connect")
        || method.eq_ignore_ascii_case("trace")
        || method.eq_ignore_ascii_case("track")
}

/// <https://fetch.spec.whatwg.org/#concept-method>
/// A method is a byte sequence that matches the `method` token production.
pub(crate) fn is_method(method: &str) -> bool {
    !method.is_empty() && method.bytes().all(is_http_token_byte)
}

/// <https://fetch.spec.whatwg.org/#cors-safelisted-method>
/// A CORS-safelisted method is a byte-case-insensitive match for `GET`, `HEAD`, or `POST`.
pub(crate) fn is_cors_safelisted_method(method: &str) -> bool {
    method.eq_ignore_ascii_case("get")
        || method.eq_ignore_ascii_case("head")
        || method.eq_ignore_ascii_case("post")
}

/// <https://fetch.spec.whatwg.org/#concept-method-normalize>
/// To normalize a method, if it is a byte-case-insensitive match for `DELETE`, `GET`, `HEAD`,
/// `OPTIONS`, `POST`, or `PUT`, byte-uppercase it. Otherwise return it unchanged.
pub(crate) fn normalize_a_method(method: String) -> String {
    const NORMALIZED: &[&str] = &["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"];
    for normalized in NORMALIZED {
        if method.eq_ignore_ascii_case(normalized) {
            return normalized.to_string();
        }
    }
    method
}

/// <https://fetch.spec.whatwg.org/#no-cors-safelisted-request-header>
/// To determine whether a header (name, value) is a no-CORS-safelisted request-header, run these steps:
///
/// Range parsing (the `range` branch of CORS-safelisting) is not relevant here (`range` is
/// not a no-CORS-safelisted name) so only the `accept`/`accept-language`/`content-language`/
/// `content-type` value rules are applied.
pub(crate) fn is_no_cors_safelisted_request_header(name: &str, value: &str) -> bool {
    // Step 1: If _name_ is not a `no-CORS-safelisted request-header name`, then return false.
    if !is_no_cors_safelisted_request_header_name(name) {
        return false;
    }
    // Step 2: Return whether (_name_, _value_) is a `CORS-safelisted request-header`.
    is_cors_safelisted_request_header(name, value)
}

/// [`is_no_cors_safelisted_request_header`] applied to `append to a Headers object`
/// Steps 3.1–3.4's temporary value — _existing_, followed by 0x2C 0x20, followed by
/// _value_ — without materializing that combined string.
///
/// Each clause mirrors the corresponding `CORS-safelisted request-header` clause
/// over the virtual concatenation; the joiner bytes 0x2C 0x20 are inert in every
/// one of them (neither is a `CORS-unsafe request-header byte`, and both are in
/// the language byte set), so the byte checks decompose into the two parts.
pub(crate) fn is_no_cors_safelisted_request_header_after_append(
    name: &str,
    existing: Option<&str>,
    value: &str,
) -> bool {
    // No existing value: the temporary value is _value_ itself.
    let Some(existing) = existing else {
        return is_no_cors_safelisted_request_header(name, value);
    };
    if !is_no_cors_safelisted_request_header_name(name) {
        return false;
    }
    // `CORS-safelisted request-header` Step 1: the combined value's length.
    if existing.chars().count() + 2 + value.chars().count() > 128 {
        return false;
    }
    // ``accept``: a `CORS-unsafe request-header byte` in the combined value is one in either part.
    if name.eq_ignore_ascii_case("accept") {
        !existing.bytes().any(is_cors_unsafe_request_header_byte)
            && !value.bytes().any(is_cors_unsafe_request_header_byte)
    }
    // ``accept-language`` | ``content-language``: every byte of both parts must be safelisted.
    else if name.eq_ignore_ascii_case("accept-language")
        || name.eq_ignore_ascii_case("content-language")
    {
        existing.bytes().all(is_safelisted_language_byte)
            && value.bytes().all(is_safelisted_language_byte)
    }
    // ``content-type`` — the only remaining `no-CORS-safelisted request-header name`.
    else {
        if existing.bytes().any(is_cors_unsafe_request_header_byte)
            || value.bytes().any(is_cors_unsafe_request_header_byte)
        {
            return false;
        }
        // The combined value's essence is everything up to its first ';'. When _existing_
        // contains a ';', that essence is _existing_'s own prefix. Otherwise the essence spans
        // the `, ` joiner (or, for an empty _existing_, starts with it), and no string
        // containing a comma equals one of the safelisted essences (Step 6 of `parse a MIME
        // type` would already have failed on it).
        match existing.find(';') {
            Some(index) => is_safelisted_content_type_essence(existing[..index].trim()),
            None => false,
        }
    }
}

/// <https://fetch.spec.whatwg.org/#cors-unsafe-request-header-byte>
/// A CORS-unsafe request-header byte: < 0x20 and not 0x09, or one of a fixed punctuation set.
fn is_cors_unsafe_request_header_byte(byte: u8) -> bool {
    // Step 1: A CORS-unsafe request-header byte is a byte _byte_ for which one of the following is
    //     true:
    //   - _byte_ is less than 0x20 and is not 0x09 HT
    (byte < 0x20 && byte != 0x09)
    //   - _byte_ is 0x22 ("), 0x28 (left parenthesis), 0x29 (right parenthesis), 0x3A (:), 0x3C
    //     (<), 0x3E (>), 0x3F (?), 0x40 (@), 0x5B ([), 0x5C (\), 0x5D (]), 0x7B ({), 0x7D (}), or
    //     0x7F DEL.
        || matches!(
            byte,
            0x22 | 0x28
                | 0x29
                | 0x3A
                | 0x3C
                | 0x3E
                | 0x3F
                | 0x40
                | 0x5B
                | 0x5C
                | 0x5D
                | 0x7B
                | 0x7D
                | 0x7F
        )
}

/// <https://fetch.spec.whatwg.org/#no-cors-safelisted-request-header-name>
/// A no-CORS-safelisted request-header name is a byte-case-insensitive match for `Accept`,
/// `Accept-Language`, `Content-Language`, or `Content-Type`.
pub(crate) fn is_no_cors_safelisted_request_header_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("accept")
        || name.eq_ignore_ascii_case("accept-language")
        || name.eq_ignore_ascii_case("content-language")
        || name.eq_ignore_ascii_case("content-type")
}

/// <https://fetch.spec.whatwg.org/#privileged-no-cors-request-header-name>
/// The privileged no-CORS request-header names are « `Range` ».
pub(crate) fn is_privileged_no_cors_request_header_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("range")
}

/// <https://fetch.spec.whatwg.org/#concept-headers-append>
/// To append a header (name, value) to a Headers object headers, run these steps:
pub(crate) fn append_to_headers(
    scope: &Scope<'_>,
    headers: &Headers<'_>,
    name: String,
    value: String,
) -> Result<(), ExnThrown> {
    // Step 1: `Normalize` _value_.
    let value = normalize_byte_sequence(value);
    // Step 2: If `validating` (_name_, _value_) for _headers_ returns false, then return.
    if !validate_header_name(scope, headers, &name, &value)? {
        return Ok(());
    }
    // Step 3: If _headers_’s `guard` is "`request-no-cors`":
    // Step 3.1: Let _temporaryValue_ be the result of `getting` _name_ from _headers_’s `header
    //     list`.
    // Step 3.2: If _temporaryValue_ is null, then set _temporaryValue_ to _value_.
    // Step 3.3: Otherwise, set _temporaryValue_ to _temporaryValue_, followed by 0x2C 0x20,
    //     followed by _value_.
    // Step 3.4: If (_name_, _temporaryValue_) is not a `no-CORS-safelisted request-header`, then
    //     return.
    // Steps 3.1–3.4 run inside `is_no_cors_safelisted_request_header_after_append`, which checks
    // the combined value without materializing it.
    if core_runtime::config::web_security_restrictions()
        && headers.data().guard == Guard::RequestNoCors
    {
        let safelisted = {
            let data = headers.data();
            let existing = get_header_for_name(&data.header_list, &name);
            is_no_cors_safelisted_request_header_after_append(&name, existing.as_deref(), &value)
        };
        if !safelisted {
            return Ok(());
        }
    }
    // Step 4: `Append` (_name_, _value_) to _headers_’s `header list`.
    append_a_header(&mut headers.data_mut().header_list, name, value);
    // Step 5: If _headers_’s `guard` is "`request-no-cors`", then `remove privileged no-CORS
    //     request-headers` from _headers_.
    if headers.data().guard == Guard::RequestNoCors {
        remove_privileged_no_cors_request_headers(headers);
    }
    Ok(())
}

/// <https://fetch.spec.whatwg.org/#headers-validate>
/// To validate a header (name, value) for a Headers object headers:
pub(crate) fn validate_header_name(
    scope: &Scope<'_>,
    headers: &Headers<'_>,
    name: &str,
    value: &str,
) -> Result<bool, ExnThrown> {
    // Step 1: If _name_ is not a `header name` or _value_ is not a `header value`, then `throw` a
    //     ``TypeError``.
    if !is_header_name(name) {
        return Err(throw_type_error(scope, c"Invalid header name"));
    }
    if !is_header_value(value) {
        return Err(throw_type_error(scope, c"Invalid header value"));
    }
    // Step 2: If _headers_’s `guard` is "`immutable`", then `throw` a ``TypeError``.
    if headers.data().guard == Guard::Immutable {
        return Err(throw_type_error(scope, c"Headers object is immutable"));
    }
    // Step 3: If _headers_’s `guard` is "`request`" and (_name_, _value_) is a `forbidden
    //     request-header`, then return false.
    // Forbidden request-headers are a browser-security policy; skip when an embedder has disabled
    // request restrictions (server-side mode).
    if core_runtime::config::web_security_restrictions()
        && headers.data().guard == Guard::Request
        && forbidden_request_header(name, value)
    {
        return Ok(false);
    }
    // Step 4: If _headers_’s `guard` is "`response`" and _name_ is a `forbidden response-header
    //     name`, then return false.
    if core_runtime::config::web_security_restrictions()
        && headers.data().guard == Guard::Response
        && is_forbidden_response_header_name(name)
    {
        return Ok(false);
    }
    // Step 5: Return true.
    Ok(true)
}

/// <https://fetch.spec.whatwg.org/#concept-headers-fill>
/// To fill a Headers object headers with a given object object, run these steps:
pub(crate) fn fill_headers(
    scope: &Scope<'_>,
    headers: &Headers<'_>,
    object: HeadersInit,
) -> Result<(), ExnThrown> {
    match object {
        // Step 1: If _object_ is a `sequence`, then `for each` _header_ of _object_:
        HeadersInit::Sequence(sequence) => {
            for header in sequence {
                // Step 1.1: If _header_’s `size` is not 2, then `throw` a ``TypeError``.
                if header.len() != 2 {
                    return Err(throw_type_error(
                        scope,
                        c"Header sequence entry must contain exactly two items",
                    ));
                }
                // Step 1.2: `Append` (_header_[0], _header_[1]) to _headers_.
                let mut iter = header.into_iter();
                let name = iter.next().unwrap().into_string();
                let value = iter.next().unwrap().into_string();
                append_to_headers(scope, headers, name, value)?;
            }
        }
        // Step 2: Otherwise, _object_ is a `record`, then `for each` _key_ → _value_ of _object_,
        //     `append` (_key_, _value_) to _headers_.
        HeadersInit::Record(record) => {
            for (key, value) in record {
                append_to_headers(scope, headers, key.into_string(), value.into_string())?;
            }
        }
    }
    Ok(())
}

/// The `Request` constructor's Step 33 sanitization for the branch whose source
/// is `this`'s own header list (no _init_["``headers``"] given): "Empty `this`'s
/// `headers`'s `header list`, then `for each` _header_ of its `header list`,
/// `append` _header_ to `this`’s `headers`".
///
/// Each of those appends would run the full `append to a Headers object`, but on
/// entries taken from an existing header list most of it is already known to
/// hold, so this runs only what can still change the result:
///
/// - `append` Step 1 (`normalize`) and `validate` step 1 is a no-op: entries are already normalized.
/// - `validate` Steps 1 & 2 cannot throw: the guard here is "`request`" or
///   "`request-no-cors`" (constructor steps 31–32), never "`immutable`".
/// - `append` Step 5 (`remove privileged no-CORS request-headers`) is a no-op:
///   a header the no-cors filter admits has one of the four safelisted names,
///   never ``Range``.
///
/// What remains is the guard's policy filtering, which is Step 33's entire purpose:
/// - `validate` Step 3's forbidden request-header check
/// - `append` Steps 3.1–3.4's no-CORS safelist check over the would-be combined value.
pub(crate) fn refill_headers_from_own_list(headers: &Headers<'_>) {
    // With request restrictions off, both filters below are disabled, and re-appending an
    // existing header list's entries results in the same list. So we can just do nothing.
    if !core_runtime::config::web_security_restrictions() {
        return;
    }

    let guard = headers.data().guard;
    debug_assert!(matches!(guard, Guard::Request | Guard::RequestNoCors));
    let list = std::mem::take(&mut *headers.data_mut().header_list);
    for (name, value) in list {
        match guard {
            // `validate` Step 3: If _headers_’s `guard` is "`request`" and (_name_, _value_) is a
            //     `forbidden request-header`, then return false.
            Guard::Request => {
                if forbidden_request_header(&name, &value) {
                    continue;
                }
            }
            // `append` Steps 3.1–3.4: the no-CORS safelist check of (_name_, _temporaryValue_),
            //     without materializing the combined value.
            Guard::RequestNoCors => {
                let safelisted = {
                    let data = headers.data();
                    let existing = get_header_for_name(&data.header_list, &name);
                    is_no_cors_safelisted_request_header_after_append(
                        &name,
                        existing.as_deref(),
                        &value,
                    )
                };
                if !safelisted {
                    continue;
                }
            }
            // Unreachable per the assert above.
            _ => {}
        }
        // `append` Step 4: `Append` (_name_, _value_) to _headers_’s `header list`.
        append_a_header(&mut headers.data_mut().header_list, name, value);
    }
}

/// <https://fetch.spec.whatwg.org/#concept-headers-remove-privileged-no-cors-request-headers>
/// To remove privileged no-CORS request-headers from a Headers object (headers), run these steps:
pub(crate) fn remove_privileged_no_cors_request_headers(headers: &Headers<'_>) {
    // Step 1: `For each` _headerName_ of `privileged no-CORS request-header names`:
    // Step 1.1: `Delete` _headerName_ from _headers_’s `header list`.
    // Note: The privileged no-CORS request-header names are « `Range` ».
    delete_header(&mut headers.data_mut().header_list, "range");
}

/// <https://fetch.spec.whatwg.org/#concept-body-mime-type>
/// To get the MIME type, given a Request or Response object requestOrResponse:
#[allow(dead_code)]
pub(crate) fn get_the_mime_type() {
    // Step 1: Let _headers_ be null.
    // Step 2: If _requestOrResponse_ is a ``Request`` object, then set _headers_ to
    //     _requestOrResponse_’s `request`’s `header list`.
    // Step 3: Otherwise, set _headers_ to _requestOrResponse_’s `response`’s `header list`.
    // Step 4: Let _mimeType_ be the result of `extracting a MIME type` from _headers_.
    // Step 5: If _mimeType_ is failure, then return null.
    // Step 6: Return _mimeType_.
    todo!("Needed for FormData/Blob")
}

/// The `convertBytesToJSValue` algorithm a `consume body` caller supplies: how the
/// fully-read byte sequence becomes the resolved value.
#[derive(Clone, Copy, Default)]
pub(crate) enum ConsumeType {
    /// `arrayBuffer()` — an `ArrayBuffer` over the bytes.
    #[default]
    ArrayBuffer,
    /// `bytes()` — a `Uint8Array` over the bytes.
    Bytes,
    /// `text()` — the UTF-8 decoding of the bytes.
    Text,
    /// `json()` — `parse JSON from bytes`.
    Json,
    /// `blob()` — a `Blob` over the bytes. Blob is not yet available in this runtime, so the
    /// conversion fails; the body is still consumed (disturbed) first, per `consume body`.
    Blob,
    /// `formData()` — parse the body as form data. Not yet supported; consumes then fails.
    FormData,
}

/// <https://encoding.spec.whatwg.org/#utf-8-decode>
/// UTF-8 decode: strip a leading UTF-8 BOM, then decode (malformed sequences become U+FFFD).
fn utf8_decode(bytes: &[u8]) -> Cow<'_, str> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    // Validating first is much faster than `from_utf8_lossy` for the common, valid case.
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => String::from_utf8_lossy(bytes),
    }
}

/// Run `convertBytesToJSValue` for the given `ConsumeType`.
pub(crate) fn convert_bytes_to_js_value<'r>(
    scope: &'r Scope<'_>,
    bytes: &[u8],
    conversion: ConsumeType,
) -> Result<HandleValue<'r>, ExnThrown> {
    match conversion {
        ConsumeType::ArrayBuffer => {
            let buffer = ArrayBuffer::with_data(scope, bytes)?;
            Ok(scope.root_value(buffer.as_value()))
        }
        ConsumeType::Bytes => {
            let array = Uint8Array::with_data(scope, bytes)?;
            Ok(scope.root_value(array.as_value()))
        }
        ConsumeType::Text => {
            let text = utf8_decode(bytes);
            text.as_ref().to_jsval_throwing(scope)
        }
        ConsumeType::Json => {
            let text = utf8_decode(bytes);
            js::json::parse(scope, &text)
        }
        ConsumeType::Blob => Err(throw_type_error(scope, c"blob() is not yet supported")),
        ConsumeType::FormData => Err(throw_type_error(scope, c"formData() is not yet supported")),
    }
}

/// Like [`convert_bytes_to_js_value`] but consumes `bytes`, so the `ArrayBuffer`
/// and `Uint8Array` results can be backed by the host bytes directly — no copy
/// when uniquely owned and aligned. Text/JSON borrow and delegate. Used on the
/// host consume path, where the whole body is owned.
pub(crate) fn convert_owned_bytes_to_js_value<'r>(
    scope: &'r Scope<'_>,
    bytes: platform::http::BodyBytes,
    conversion: ConsumeType,
) -> Result<HandleValue<'r>, ExnThrown> {
    use crate::incoming_body::array_buffer_from_body_bytes;
    match conversion {
        ConsumeType::ArrayBuffer => {
            let buffer = array_buffer_from_body_bytes(scope, bytes)?;
            Ok(scope.root_value(buffer.as_value()))
        }
        ConsumeType::Bytes => {
            let len = bytes.len();
            let buffer = array_buffer_from_body_bytes(scope, bytes)?;
            let array = js::typedarray::construct_view(
                scope,
                js::typedarray::ViewKind::Uint8,
                buffer,
                0,
                len,
            )?;
            Ok(scope.root_value(array.as_value()))
        }
        _ => convert_bytes_to_js_value(scope, &bytes, conversion),
    }
}

/// <https://fetch.spec.whatwg.org/#concept-body-consume-body>
/// The consume body algorithm, given an object that includes Body object and an algorithm that
/// takes a byte sequence and returns a JavaScript value or throws an exception
/// convertBytesToJSValue, runs these steps:
pub(crate) fn consume_body<'r>(
    scope: &'r Scope<'_>,
    object: &impl BodyMixin,
    conversion: ConsumeType,
) -> Result<Promise<'r>, ExnThrown> {
    // Step 1: If _object_ is `unusable`, then return `a promise rejected with` a ``TypeError``.
    if object.is_unusable(scope) {
        let _ = throw_type_error(scope, c"Body has already been consumed");
        return Promise::new_rejected_with_pending_error(scope);
    }
    // Step 2: Let _promise_ be `a new promise`.
    // Step 3: Let _errorSteps_ given _error_ be to `reject` _promise_ with _error_.
    // Step 4: Let _successSteps_ given a `byte sequence` _data_ be to `resolve` _promise_ with the
    //     result of running _convertBytesToJSValue_ with _data_. If that threw an exception, then
    //     run _errorSteps_ with that exception.
    // Step 5: If _object_’s `body` is null, then run _successSteps_ with an empty `byte sequence`.
    // Step 6: Otherwise, `fully read` _object_’s `body` given _successSteps_, _errorSteps_, and
    //     _object_’s `relevant global object`.
    //
    // Note: byte-sequence sources are ready immediately and don't need to go through the
    // ReadableStream machinery, so _successSteps_ is inlined here for those.
    // For actual stream sources, it's realized in [`consume::convert_bytes`].
    let bytes: bytes::Bytes = match object.body_record() {
        None => bytes::Bytes::new(),
        Some(body) => {
            object.set_source_disturbed();
            if let Some(stream) = object.body_stream(scope) {
                return crate::consume::fully_read_stream_to_value(scope, stream, conversion);
            }
            match &body.source {
                // No stream materialized: read the byte source directly (the
                // synchronous fast path; steps 2–4 settle without a microtask).
                BodySource::Bytes(bytes) => bytes.clone(),
                // No stream and a null source: an empty body.
                BodySource::Null => bytes::Bytes::new(),
            }
        }
    };
    match convert_bytes_to_js_value(scope, &bytes, conversion) {
        Ok(value) => Promise::new_resolved_with_value(scope, value),
        Err(_) => Promise::new_rejected_with_pending_error(scope),
    }
    // Step 7: Return _promise_.
    // (implicit)
}

/// Whether `text` matches the HTTP `reason-phrase` token production: every byte is HTAB
/// (0x09), SP (0x20), a VCHAR (0x21–0x7E), or obs-text (0x80–0xFF).
fn is_reason_phrase(text: &str) -> bool {
    text.bytes()
        .all(|b| b == 0x09 || b == 0x20 || (0x21..=0x7E).contains(&b) || b >= 0x80)
}

/// A body with its (optional) materialized stream and (optional) Content-Type, as produced by
/// `extract a body`. The type is borrowed when it is one of the spec's literal values.
pub(crate) type BodyWithType<'r> = (Body, Option<ReadableStream<'r>>, Option<Cow<'static, str>>);

/// <https://fetch.spec.whatwg.org/#response-create>
/// To create a Response object, given a response _response_, headers guard _guard_, and realm\
/// _realm_, run these steps:
pub(crate) fn create_a_response_object<'r>(
    scope: &'r Scope<'_>,
    record: ResponseRecord,
    header_list: HeaderList,
    guard: Guard,
    body: Option<Body>,
    stream: Option<ReadableStream<'r>>,
) -> Result<Response<'r>, ExnThrown> {
    // Step 1: Let _responseObject_ be a `new` ``Response`` object with _realm_.
    // Step 2: Set _responseObject_’s `response` to _response_.
    // Step 3: Set _responseObject_’s `headers` to a `new` ``Headers`` object with _realm_, whose
    //     `headers list` is _response_’s `headers list` and `guard` is _guard_.
    // Step 4: Return _responseObject_.
    let headers = Headers::from_list(scope, header_list, guard)?;
    let response = Response::from_record_headers_body(scope, record, headers, body, stream)?;
    response.sync_body_accounting();
    Ok(response)
}

/// <https://fetch.spec.whatwg.org/#initialize-a-response>
/// To initialize a response, given a Response object _response_, ResponseInit _init_, and `null`
/// or a body with type _body_:
pub(crate) fn initialize_a_response<'r>(
    scope: &Scope<'_>,
    response: &Response<'_>,
    init: Option<ResponseInit>,
    body: Option<BodyWithType<'r>>,
) -> Result<(), ExnThrown> {
    let (status, status_text, init_headers) = match init {
        Some(init) => (init.status, init.status_text, init.headers),
        None => (200, String::new(), None),
    };
    // Step 1: If _init_["``status``"] is not in the range 200 to 599, inclusive, then `throw` a
    //     ``RangeError``.
    if !(200..=599).contains(&status) {
        return Err(RangeError(format!(
            "Response status {status} is outside the range 200–599"
        ))
        .throw(scope));
    }
    // Step 2: If _init_["``statusText``"] is not the empty string and does not match the
    //     `reason-phrase` token production, then `throw` a ``TypeError``.
    if !status_text.is_empty() && !is_reason_phrase(&status_text) {
        return Err(throw_type_error(scope, c"Invalid response statusText"));
    }
    // Step 3: Set _response_’s `response`’s `status` to _init_["``status``"].
    response.data_mut().response.status = status;
    // Step 4: Set _response_’s `response`’s `status message` to _init_["``statusText``"].
    response.data_mut().response.status_message = status_text;
    // Step 5: If _init_["``headers``"] `exists`, then `fill` _response_’s `headers` with
    //     _init_["``headers``"].
    if let Some(init_headers) = init_headers {
        let headers = response.data().headers.get(scope);
        fill_headers(scope, &headers, init_headers)?;
    }
    // Step 6: If _body_ is non-null, then:
    if let Some((body_record, stream, content_type)) = body {
        // Step 6.1: If _response_’s `status` is a `null body status`, then `throw` a ``TypeError``.
        //     101 and 103 are included in `null body status` due to their use elsewhere. They do
        //     not affect this step.
        if is_null_body_status(response.status()) {
            return Err(throw_type_error(
                scope,
                c"Response with a null body status cannot have a body",
            ));
        }
        // Step 6.2: Set _response_’s `body` to _body_’s `body`.
        response.data_mut().body.record = Some(body_record);
        if let Some(stream) = stream {
            response.data_mut().body.stream = Some(Heap::from(stream));
        }
        // Step 6.3: If _body_’s `type` is non-null and _response_’s `header list` `does not
        //     contain` ``Content-Type``, then `append` (``Content-Type``, _body_’s `type`) to
        //     _response_’s `header list`.
        if let Some(content_type) = content_type {
            // The spec appends to the `header list` here — the list operation, not the guarded
            // `append to a Headers object` — so no validation or guard filtering applies. The
            // list was just checked not to contain ``Content-Type``, so `append a header`'s
            // casing reuse has nothing to find and the append is a plain push.
            let headers = response.headers(scope);
            if !contains(&headers.data().header_list, "Content-Type") {
                headers
                    .data_mut()
                    .header_list
                    .push(("Content-Type".to_string(), content_type.into_owned()));
            }
        }
    }
    response.sync_body_accounting();
    Ok(())
}

/// A `data:` URL struct: the result of the `data:` URL processor.
///
/// <https://fetch.spec.whatwg.org/#data-url-struct>
#[derive(Debug, Clone)]
pub struct DataUrlStruct {
    /// <https://mimesniff.spec.whatwg.org/#mime-type> — `data_url::mime::Mime`
    /// is the MIME type record: `type`/`subtype` plus the ordered parameter map,
    /// with `FromStr` as "parse a MIME type" and `Display` as "serialize a MIME
    /// type".
    /// <https://fetch.spec.whatwg.org/#data-url-struct-mime-type>
    pub mime_type: data_url::mime::Mime,
    /// <https://fetch.spec.whatwg.org/#data-url-struct-body>
    pub body: Vec<u8>,
}

/// What running the fetch pipeline produced, for [`crate::globals::fetch`] (which owns
/// the promise) to act on as its _processResponse_.
pub(crate) enum FetchOutcome<'r> {
    /// A response produced in-process — the "`data`" arm of `scheme fetch`.
    Response(Response<'r>),
    /// A `network error`.
    NetworkError,
    /// An HTTP(S) request for the host transport to send: `HTTP fetch` onwards,
    /// which lives in [`crate::transport::send_following_redirects`].
    Network,
}

/// <https://fetch.spec.whatwg.org/#concept-fetch>
/// To fetch, given a request request, an optional algorithm processRequestBodyChunkLength, an
/// optional algorithm processRequestEndOfBody, an optional algorithm processEarlyHintsResponse, an
/// optional algorithm processResponse, an optional algorithm processResponseEndOfBody, an optional
/// algorithm processResponseConsumeBody, and an optional boolean useParallelQueue (default false),
/// run the steps below.
pub(crate) fn fetch<'r>(
    scope: &'r Scope<'_>,
    request: &crate::request::Request<'_>,
) -> Result<FetchOutcome<'r>, ExnThrown> {
    // Step 1: `Assert`: _request_’s `mode` is "`navigate`" or _processEarlyHintsResponse_ is null.
    // No navigations here, and early hints are not surfaced.
    // TODO: support early hints
    // Steps 2, 3, 5–8: `taskDestination`, `crossOriginIsolatedCapability`, `timing info` and the
    //     `fetch params` record.
    // The callback-and-task-queue plumbing this runtime replaces with promises and futures. No
    // client, so Step 5 has nothing to read.
    // Step 4: `Populate request from client` given _request_.
    // No client, so `origin`, `policy container` and `traversable for user prompts` are left unset.
    // Step 9: If _request_’s `body` is a `byte sequence`, then set _request_’s `body` to
    //     _request_’s `body` `as a body`.
    // Already a `body`: `extract a body` ran in the `Request` constructor.
    // Step 10: `WebDriver BiDi clone network request body`.
    // No WebDriver.
    // Step 11: `preloaded response candidate`.
    // Needs a ``Window`` client.
    // Step 12: If _request_’s `header list` `does not contain` ``Accept``, then:
    // Step 12.1: Let _value_ be ``*/*``.
    // Steps 12.2–12.3: the "`prefetch`"-initiator and `destination`-keyed ``Accept`` values do not
    //     apply (we don't have initiators or destinations), leaving ``*/*``.
    // Step 12.4: `Append` (``Accept``, _value_) to _request_’s `header list`.
    // Applied at send time in `Request::platform_request` instead of here, so it does not become
    // visible on the `Request` object's `headers`.
    // Steps 13–14: ``Accept-Language``.
    // Not sent: no language settings to derive one from.
    // Step 15: `internal priority`.
    // Left to the transport.
    // Step 16: `fetch records` on the `client`’s `fetch group`.
    // No client; the keepalive and `fetchLater` machinery that reads them is out of scope.
    // Step 17: Run `main fetch` given _fetchParams_.
    let outcome = main_fetch(scope, request)?;
    // Step 18: Return _fetchParams_’s `controller`.
    // There is no controller: the caller holds the promise and drives it from the outcome.
    Ok(outcome)
}

/// <https://fetch.spec.whatwg.org/#concept-main-fetch>
/// To main fetch, given a fetch params fetchParams and an optional boolean recursive (default false), run these steps:
fn main_fetch<'r>(
    scope: &'r Scope<'_>,
    request: &crate::request::Request<'_>,
) -> Result<FetchOutcome<'r>, ExnThrown> {
    // Step 1: Let _request_ be _fetchParams_’s `request`.
    // Step 2: Let _response_ be null.
    // Step 3: `local-URLs-only flag`.
    // Only navigation machinery sets it.
    // Step 4: `report Content Security Policy violations`.
    // No CSP.
    // Steps 5–7: `upgrade to a potentially trustworthy URL` (HSTS, mixed content) and the bad-port
    //     / mixed-content / CSP / Integrity Policy blocking checks.
    // Browser-security policies keyed on a `client` and its `policy container`, which do not exist
    // here.
    // Steps 8–9: `referrer policy` and `determine _request_’s referrer`.
    // No referrer is sent.
    // Step 10: HSTS upgrade of the `current URL`’s `scheme`.
    // No HSTS store.
    // Step 11: If _recursive_ is false, then run the remaining steps `in parallel`.
    // The HTTP(S) arm of Step 12 runs on a future; the "`data`" arm is synchronous and needs none.
    // Step 12: If _response_ is null, then set _response_ to the result of running the steps
    //     corresponding to the first matching statement:
    // The switch keys on `origin`, `response tainting` and `mode`. This runtime has no requester
    // `origin`, so no arm that compares one can be evaluated; what is left is a dispatch on the
    // `current URL`’s `scheme`, and only two arms are reachable.
    let current_url = request.current_url();
    match current_url.scheme() {
        // Step 12 _request_’s `current URL`’s `scheme` is "`data`".1: Set _request_’s `response
        //     tainting` to "`basic`".
        // Step 12 _request_’s `current URL`’s `scheme` is "`data`".2: Return the result of running
        //     `override fetch` given "`scheme-fetch`" and _fetchParams_.
        // (`override fetch` Steps 1–3 consult `potentially override response for a request`, whose
        // default implementation returns null, so that is `scheme fetch` directly.)
        "data" => {
            let Some(response) = scheme_fetch(scope, &current_url)? else {
                return Ok(FetchOutcome::NetworkError);
            };
            // Steps 13–21: `recursive` return, `filtered response` construction and the
            //     `CORS-exposed header-name list`, `URL list`/`redirect taint`/timing-flag
            //     bookkeeping, navigation timing, and the mixed-content/CSP/MIME/nosniff and
            //     opaque-range blocking checks.
            // All need the response-tainting, origin or policy models this runtime lacks;
            // `response_from_data_url` sets the `URL list` (Step 16) and the "`basic`" type of
            // Step 14 when it builds the response.
            // Step 22: If _response_ is not a `network error` and either _request_’s `method` is
            //     ``HEAD`` or ``CONNECT``, or _internalResponse_’s `status` is a `null body
            //     status`, set _internalResponse_’s `body` to null and disregard any enqueuing
            //     toward it (if any).
            // A `data:` response is always 200, so only the method matters here.
            let method = request.http_method();
            if method.eq_ignore_ascii_case("HEAD") || method.eq_ignore_ascii_case("CONNECT") {
                response.clear_body();
            }
            // Step 23: `integrity metadata`.
            // Subresource integrity is not implemented; a request carrying it is fetched as if it
            // had none.
            // Step 24: Otherwise, run `fetch response handover` given _fetchParams_ and _response_.
            // The handover's timing and `process response` bookkeeping collapses to returning the
            // response to the caller, which resolves the promise with it.
            Ok(FetchOutcome::Response(response))
        }
        // Step 12's origin/mode switch and the tainting it derives live in
        // `transport::apply_main_fetch_switch`, run against the initial URL and every redirect
        // hop. `response_from_platform` applies Step 14.2's filtering (and Step 22) from the
        // accumulated tainting. On this path the response does not exist until the transport
        // future completes.
        // `HTTP fetch` onwards is the host transport.
        "http" | "https" => Ok(FetchOutcome::Network),
        // Step 12 _request_’s `current URL`’s `scheme` is not an `HTTP(S) scheme`: Return a
        //     `network error`.
        // (Reached for every scheme except "`data`", handled above.)
        _ => Ok(FetchOutcome::NetworkError),
    }
}

/// <https://fetch.spec.whatwg.org/#concept-scheme-fetch>
/// To scheme fetch, given a fetch params fetchParams:
///
/// Returns null for a `network error`. Only the "`data`" arm of Step 3 is
/// reachable: `main fetch` sends HTTP(S) URLs to the network path, and makes
/// every other scheme a network error before reaching here.
fn scheme_fetch<'r>(
    scope: &'r Scope<'_>,
    current_url: &url::Url,
) -> Result<Option<Response<'r>>, ExnThrown> {
    // Step 1: If _fetchParams_ is `canceled`, then return the `appropriate network error` for
    //     _fetchParams_.
    // A `data:` fetch resolves synchronously, so there is no window in which it could be canceled
    // part-way.
    // Step 2: Let _request_ be _fetchParams_’s `request`.
    // Step 3: Switch on _request_’s `current URL`’s `scheme` and run the associated steps:
    // (The other schemes currently not supported)
    debug_assert_eq!(current_url.scheme(), "data");
    // Step 3 "`data`".1: Let _dataURLStruct_ be the result of running the ``data:` URL processor`
    //     on _request_’s `current URL`.
    // Step 3 "`data`".2: If _dataURLStruct_ is failure, then return a `network error`.
    // Step 3 "`data`".3: Let _mimeType_ be _dataURLStruct_’s `MIME type`, `serialized`.
    // Step 3 "`data`".4: Return a new `response` whose `status message` is ``OK``, `header list` is
    //     « (``Content-Type``, _mimeType_) », and `body` is _dataURLStruct_’s `body` `as a body`.
    crate::response::response_from_data_url(scope, current_url)
    // Step 4: Return a `network error`.
    // `response_from_data_url` returns null both for the failure case of Step 3 and, were another
    // scheme to reach here, for this step.
}

/// <https://fetch.spec.whatwg.org/#data-url-processor>
/// The data: URL processor takes a URL dataURL and then runs these steps:
pub(crate) fn data_url_processor(data_url: &url::Url) -> Option<DataUrlStruct> {
    // Step 1: `Assert`: _dataURL_’s `scheme` is "`data`".
    debug_assert_eq!(data_url.scheme(), "data");
    // Step 2: Let _input_ be the result of running the `URL serializer` on _dataURL_ with `_exclude
    //     fragment_` set to true.
    let mut serialized = data_url.clone();
    serialized.set_fragment(None);
    // Steps 3–14: the `data-url` crate's processor.
    let processed = data_url::DataUrl::process(serialized.as_str()).ok()?;
    let (body, _fragment) = processed.decode_to_vec().ok()?;
    // Step 15: Return a new ``data:` URL struct` whose `MIME type` is _mimeTypeRecord_ and `body`
    //     is _body_.
    Some(DataUrlStruct {
        mime_type: processed.mime_type().clone(),
        body,
    })
}
