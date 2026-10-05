// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:url`. See <https://nodejs.org/api/url.html>.
//!
//! `URL` and `URLSearchParams` are the global WHATWG classes. The legacy API (`Url`, `parse`,
//! `format`, `resolve`, `resolveObject`) is a port of Node.js v24's `lib/url.js`. Hostnames are
//! converted with the `idna` crate's UTS #46 processing.

use std::cell::Cell;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::object::{PropertyInit, PropertyTemplate, PropertyWriter, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_string, ErrorKind, NodeError, Thrown};
use crate::process::{called_from_node_modules, deprecation_warning};

/// The protocols whose URLs always have `//` before the host.
const SLASHED_PROTOCOLS: &[&str] = &[
    "http", "http:", "https", "https:", "ftp", "ftp:", "gopher", "gopher:", "file", "file:", "ws",
    "ws:", "wss", "wss:",
];

/// The protocols whose URLs never have a host, and whose paths aren't escaped.
const HOSTLESS_PROTOCOLS: &[&str] = &["javascript", "javascript:"];

/// The characters `url.parse` rejects in a hostname after IDNA conversion.
const FORBIDDEN_HOST_CHARS: &[char] = &[
    '\0', '\t', '\n', '\r', ' ', '#', '%', '/', ':', '<', '>', '?', '@', '[', '\\', ']', '^', '|',
];

/// The characters `url.parse` rejects in a bracketed IPv6 hostname.
const FORBIDDEN_IPV6_HOST_CHARS: &[char] = &[
    '\0', '\t', '\n', '\r', ' ', '#', '%', '/', '<', '>', '?', '@', '\\', '^', '|',
];

js::instance_local! {
    /// Whether `url.parse` emitted DEP0169.
    static PARSE_WARNED: Cell<bool> = const { Cell::new(false) };
    /// Whether `url.parse` emitted DEP0170.
    static PORT_WARNED: Cell<bool> = const { Cell::new(false) };
}

fn is_slashed(protocol: Option<&str>) -> bool {
    protocol.is_some_and(|protocol| SLASHED_PROTOCOLS.contains(&protocol))
}

fn is_hostless(protocol: Option<&str>) -> bool {
    protocol.is_some_and(|protocol| HOSTLESS_PROTOCOLS.contains(&protocol))
}

/// The realm's `URL` constructor.
fn url_constructor<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::class_constructor::<web_url::url::URLImpl>(scope)
}

/// `decodeURIComponent(text)`, or `None` for a `%` that doesn't start two hex digits or a result
/// that isn't UTF-8, for which `decodeURIComponent` throws.
pub(crate) fn decode_uri_component(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = (input.next()? as char).to_digit(16)?;
        let low = (input.next()? as char).to_digit(16)?;
        bytes.push((high * 16 + low) as u8);
    }
    String::from_utf8(bytes).ok()
}

/// Whether `value` passes Node's `isURL` check: an object with a truthy `href` and `protocol`, and
/// with `auth` and `path` undefined.
pub(crate) fn is_url(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    let Ok(object) = Object::from_value(scope, *value) else {
        return Ok(false);
    };
    let truthy = |name| crate::support::truthy(scope, object, name);
    Ok(truthy(c"href")?
        && truthy(c"protocol")?
        && object.get_property(scope, c"auth")?.is_undefined()
        && object.get_property(scope, c"path")?.is_undefined())
}

/// `decodeURIComponent(input)`, which throws a `URIError` for a malformed escape or invalid UTF-8.
fn decode_uri_component_throwing(scope: &Scope<'_>, input: &str) -> Result<String, Thrown> {
    decode_uri_component(input).ok_or_else(|| {
        js::error::throw_uri_error(scope, c"URI malformed");
        Thrown::Pending
    })
}

/// The path of the `file:` URL `href`, as `fileURLToPath` returns it.
pub fn file_url_to_path(scope: &Scope<'_>, href: &str) -> Result<String, Thrown> {
    let url = url::Url::parse(href)
        .map_err(|_| NodeError::new(ErrorKind::TypeError, "ERR_INVALID_URL", "Invalid URL"))?;
    if url.scheme() != "file" {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_URL_SCHEME",
            "The URL must be of scheme file",
        )
        .into());
    }
    if url.host_str().is_some_and(|host| !host.is_empty()) {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_FILE_URL_HOST",
            format!(
                "File URL host must be \"localhost\" or empty on {}",
                crate::process::platform()
            ),
        )
        .into());
    }
    let path = url.path();
    if path.to_ascii_lowercase().contains("%2f") {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_FILE_URL_PATH",
            "File URL path must not include encoded / characters",
        )
        .into());
    }
    decode_uri_component_throwing(scope, path)
}

/// The `file:` URL of `path`, resolved against the working directory, as `pathToFileURL` builds
/// it: `%`, `?`, `#`, `\`, and control characters are percent-encoded, and a trailing `/` is kept.
fn path_to_file_href(path: &str) -> String {
    let mut resolved = crate::path::resolve_paths(&[path.to_string()]);
    if path.ends_with('/') && !resolved.ends_with('/') {
        resolved.push('/');
    }
    let mut href = String::from("file://");
    for c in resolved.chars() {
        match c {
            '%' => href.push_str("%25"),
            '?' => href.push_str("%3F"),
            '#' => href.push_str("%23"),
            '\\' => href.push_str("%5C"),
            '\n' => href.push_str("%0A"),
            '\r' => href.push_str("%0D"),
            '\t' => href.push_str("%09"),
            ' ' => href.push_str("%20"),
            c => href.push(c),
        }
    }
    href
}

/// The URL object a `query` object came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    This,
    Relative,
}

/// The `query` of a legacy URL.
#[derive(Clone, Debug, Default)]
enum Query {
    #[default]
    Null,
    Text(String),
    /// The object `querystring.parse` makes of the text.
    Parsed(String),
    /// The `query` object of a URL object, and its `querystring.stringify` result.
    Object(Origin, String),
}

/// An error of the legacy parser.
enum LegacyError {
    /// `ERR_INVALID_URL`.
    InvalidUrl,
    /// The `URIError` of `decodeURIComponent`.
    UriMalformed,
}

/// The fields of a `url.Url`. `None` stands for `null`, and for the falsy values of a URL object
/// passed in.
#[derive(Clone, Debug, Default)]
struct LegacyUrl {
    protocol: Option<String>,
    slashes: bool,
    auth: Option<String>,
    host: Option<String>,
    port: Option<String>,
    hostname: Option<String>,
    hash: Option<String>,
    search: Option<String>,
    query: Query,
    pathname: Option<String>,
    path: Option<String>,
    href: Option<String>,
}

/// The properties of the relative URL object that [`LegacyUrl::resolve_object`] copies to the
/// result.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Copy {
    Nothing,
    All,
    AllButProtocol,
}

/// Whether `text` is a non-empty string.
fn truthy(text: &Option<String>) -> bool {
    text.as_deref().is_some_and(|text| !text.is_empty())
}

/// Whether `hostname` is in brackets.
fn is_ipv6_hostname(hostname: &str) -> bool {
    hostname.starts_with('[') && hostname.ends_with(']')
}

/// Whether `rest` matches Node's `hostPattern`, `/^\/\/[^@/]+@[^@/]+/`.
fn host_pattern(rest: &str) -> bool {
    let Some(rest) = rest.strip_prefix("//") else {
        return false;
    };
    let Some(at) = rest.find(['@', '/']) else {
        return false;
    };
    at > 0
        && rest.as_bytes()[at] == b'@'
        && at + 1 < rest.len()
        && !rest[at + 1..].starts_with(['@', '/'])
}

/// `rest` split by Node's `simplePathPattern`, `/^(\/\/?(?!\/)[^?\s]*)(\?[^\s]*)?$/`, into the
/// path and the search.
fn simple_path(rest: &str) -> Option<(&str, Option<&str>)> {
    let is_space = |c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}';
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'/') {
        return None;
    }
    let start = match (bytes.get(1), bytes.get(2)) {
        (Some(b'/'), Some(b'/')) => return None,
        (Some(b'/'), _) => 2,
        _ => 1,
    };
    let end = rest[start..]
        .find(|c: char| c == '?' || is_space(c))
        .map_or(rest.len(), |end| start + end);
    if end == rest.len() {
        return Some((rest, None));
    }
    if bytes[end] != b'?' || rest[end..].contains(is_space) {
        return None;
    }
    Some((&rest[..end], Some(&rest[end..])))
}

/// `rest` with the characters Node's `autoEscapeStr` escapes percent-encoded.
fn auto_escape(rest: &str) -> String {
    let mut escaped = String::with_capacity(rest.len());
    for c in rest.chars() {
        match c {
            '\t' => escaped.push_str("%09"),
            '\n' => escaped.push_str("%0A"),
            '\r' => escaped.push_str("%0D"),
            ' ' => escaped.push_str("%20"),
            '"' => escaped.push_str("%22"),
            '\'' => escaped.push_str("%27"),
            '<' => escaped.push_str("%3C"),
            '>' => escaped.push_str("%3E"),
            '\\' => escaped.push_str("%5C"),
            '^' => escaped.push_str("%5E"),
            '`' => escaped.push_str("%60"),
            '{' => escaped.push_str("%7B"),
            '|' => escaped.push_str("%7C"),
            '}' => escaped.push_str("%7D"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// `text` split at `/`, or no parts if it is empty or `None`.
fn split_path(text: &Option<String>) -> Vec<String> {
    match text.as_deref().filter(|text| !text.is_empty()) {
        Some(text) => text.split('/').map(String::from).collect(),
        None => Vec::new(),
    }
}

/// Make `host` the first part of `path`, replacing an empty first part.
fn prepend_host(path: &mut Vec<String>, host: String) {
    match path.first_mut() {
        Some(first) if first.is_empty() => *first = host,
        _ => path.insert(0, host),
    }
}

impl LegacyUrl {
    /// `Url.prototype.parse` of `url`, and whether its host held an invalid port, for which
    /// Node.js emits DEP0170.
    fn parse(
        url: &str,
        parse_query_string: bool,
        slashes_denote_host: bool,
    ) -> Result<(Self, bool), LegacyError> {
        let mut result = LegacyUrl::default();
        let mut invalid_port = false;

        // Trim whitespace, and turn backslashes before the query or hash into slashes.
        let mut has_hash = false;
        let mut has_at = false;
        let mut start = None;
        let mut end = None;
        let mut rest = String::new();
        let mut last_pos = 0;
        let mut in_ws = false;
        let mut split = false;
        for (i, c) in url.char_indices() {
            let is_ws = (c as u32) < 33 || c == '\u{a0}' || c == '\u{feff}';
            if start.is_none() {
                if is_ws {
                    continue;
                }
                start = Some(i);
                last_pos = i;
            } else if in_ws {
                if !is_ws {
                    end = None;
                    in_ws = false;
                }
            } else if is_ws {
                end = Some(i);
                in_ws = true;
            }
            if !split {
                match c {
                    '@' => has_at = true,
                    '#' => {
                        has_hash = true;
                        split = true;
                    }
                    '?' => split = true,
                    '\\' => {
                        rest.push_str(&url[last_pos..i]);
                        rest.push('/');
                        last_pos = i + 1;
                    }
                    _ => {}
                }
            } else if c == '#' {
                has_hash = true;
            }
        }
        if let Some(start) = start {
            if last_pos == start {
                rest = url[start..end.unwrap_or(url.len())].to_string();
            } else if end.is_none() && last_pos < url.len() {
                rest.push_str(&url[last_pos..]);
            } else if let Some(end) = end.filter(|&end| last_pos < end) {
                rest.push_str(&url[last_pos..end]);
            }
        }

        if !slashes_denote_host && !has_hash && !has_at {
            if let Some((pathname, search)) = simple_path(&rest) {
                result.path = Some(rest.clone());
                result.href = Some(rest.clone());
                result.pathname = Some(pathname.to_string());
                if let Some(search) = search {
                    result.search = Some(search.to_string());
                    let query = search[1..].to_string();
                    result.query = if parse_query_string {
                        Query::Parsed(query)
                    } else {
                        Query::Text(query)
                    };
                } else if parse_query_string {
                    result.query = Query::Parsed(String::new());
                }
                return Ok((result, false));
            }
        }

        let proto_end = rest
            .find(':')
            .filter(|&end| {
                end > 0
                    && rest[..end]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
            })
            .map(|end| end + 1);
        let proto = proto_end.map(|end| rest[..end].to_string());
        let lower_proto = proto.as_ref().map(|proto| proto.to_ascii_lowercase());
        if let Some(end) = proto_end {
            result.protocol = lower_proto.clone();
            rest = rest[end..].to_string();
        }
        let lower_proto = lower_proto.as_deref();

        let mut slashes = false;
        if slashes_denote_host || proto.is_some() || host_pattern(&rest) {
            slashes = rest.starts_with("//");
            if slashes && !(proto.is_some() && is_hostless(lower_proto)) {
                rest = rest[2..].to_string();
                result.slashes = true;
            }
        }

        if !is_hostless(lower_proto)
            && (slashes || (proto.is_some() && !is_slashed(proto.as_deref())))
        {
            // The first `/`, `?` or `#` ends the host. A character a host can't hold ends it
            // too, unless an `@` follows.
            let mut chars: Vec<char> = rest.chars().collect();
            let mut host_end = None;
            let mut at_sign = None;
            let mut non_host = None;
            let mut i = 0;
            while i < chars.len() {
                match chars[i] {
                    '\t' | '\n' | '\r' => {
                        chars.remove(i);
                        continue;
                    }
                    ' ' | '"' | '%' | '\'' | ';' | '<' | '>' | '\\' | '^' | '`' | '{' | '|'
                    | '}' => {
                        non_host.get_or_insert(i);
                    }
                    '#' | '/' | '?' => {
                        non_host.get_or_insert(i);
                        host_end = Some(i);
                    }
                    '@' => {
                        at_sign = Some(i);
                        non_host = None;
                    }
                    _ => {}
                }
                if host_end.is_some() {
                    break;
                }
                i += 1;
            }
            let mut host_start = 0;
            if let Some(at_sign) = at_sign {
                let auth: String = chars[..at_sign].iter().collect();
                result.auth = Some(decode_uri_component(&auth).ok_or(LegacyError::UriMalformed)?);
                host_start = at_sign + 1;
            }
            let host_end = non_host.unwrap_or(chars.len());
            result.host = Some(chars[host_start.min(host_end)..host_end].iter().collect());
            rest = chars[host_end..].iter().collect();
            result.parse_host();

            let hostname = result.hostname.clone().unwrap_or_default();
            let ipv6_hostname = is_ipv6_hostname(&hostname);
            let mut hostname = match hostname.find(['/', '\\', '#', '?', ':']) {
                Some(i) if !ipv6_hostname => {
                    invalid_port = hostname.as_bytes()[i] == b':';
                    rest = format!("/{}{rest}", &hostname[i..]);
                    hostname[..i].to_string()
                }
                _ => hostname,
            };
            if hostname.encode_utf16().count() > 255 {
                hostname = String::new();
            } else {
                hostname = hostname.to_lowercase();
            }

            if !hostname.is_empty() {
                if ipv6_hostname {
                    if hostname.contains(FORBIDDEN_IPV6_HOST_CHARS) {
                        return Err(LegacyError::InvalidUrl);
                    }
                } else {
                    hostname = idna::domain_to_ascii(&hostname).unwrap_or_default();
                    if hostname.is_empty() || hostname.contains(FORBIDDEN_HOST_CHARS) {
                        return Err(LegacyError::InvalidUrl);
                    }
                }
            }

            result.host = Some(match result.port.as_deref() {
                Some(port) if !port.is_empty() => format!("{hostname}:{port}"),
                _ => hostname.clone(),
            });
            if ipv6_hostname {
                // `hostname` is empty here if it was longer than 255 code units.
                if let Some(inner) = hostname.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
                    hostname = inner.to_string();
                }
                if !rest.starts_with('/') {
                    rest.insert(0, '/');
                }
            }
            result.hostname = Some(hostname);
        }

        if !is_hostless(lower_proto) {
            rest = auto_escape(&rest);
        }

        let hash_index = rest.find('#');
        let question_index = rest
            .find('?')
            .filter(|&question| hash_index.is_none_or(|hash| question < hash));
        if let Some(hash_index) = hash_index {
            result.hash = Some(rest[hash_index..].to_string());
        }
        if let Some(question_index) = question_index {
            let search_end = hash_index.unwrap_or(rest.len());
            result.search = Some(rest[question_index..search_end].to_string());
            let query = rest[question_index + 1..search_end].to_string();
            result.query = if parse_query_string {
                Query::Parsed(query)
            } else {
                Query::Text(query)
            };
        } else if parse_query_string {
            result.search = None;
            result.query = Query::Parsed(String::new());
        }

        match question_index.or(hash_index) {
            None if !rest.is_empty() => result.pathname = Some(rest.clone()),
            Some(first) if first > 0 => result.pathname = Some(rest[..first].to_string()),
            _ => {}
        }
        if is_slashed(lower_proto) && truthy(&result.hostname) && !truthy(&result.pathname) {
            result.pathname = Some("/".into());
        }
        if truthy(&result.pathname) || truthy(&result.search) {
            result.update_path();
        }
        result.href = Some(result.format());
        Ok((result, invalid_port))
    }

    /// `Url.prototype.parseHost`: move a trailing `:` and digits from `host` to `port`, and set
    /// `hostname` to the rest of `host` unless that is empty.
    fn parse_host(&mut self) {
        let mut host = self.host.clone().unwrap_or_default();
        if let Some(colon) = host.rfind(':') {
            if host[colon + 1..].bytes().all(|b| b.is_ascii_digit()) {
                if colon + 1 < host.len() {
                    self.port = Some(host[colon + 1..].to_string());
                }
                host.truncate(colon);
            }
        }
        if !host.is_empty() {
            self.hostname = Some(host);
        }
    }

    /// Set `path` to `pathname` followed by `search`.
    fn update_path(&mut self) {
        self.path = Some(format!(
            "{}{}",
            self.pathname.as_deref().unwrap_or_default(),
            self.search.as_deref().unwrap_or_default()
        ));
    }

    /// `Url.prototype.format`.
    fn format(&self) -> String {
        let mut auth = self.auth.clone().unwrap_or_default();
        if !auth.is_empty() {
            auth = crate::querystring::escape_str(&auth).replace("%3A", ":");
            auth.push('@');
        }
        let mut protocol = self.protocol.clone().unwrap_or_default();
        if !protocol.is_empty() && !protocol.ends_with(':') {
            protocol.push(':');
        }
        let mut host = String::new();
        if let Some(own_host) = self.host.as_deref().filter(|host| !host.is_empty()) {
            host = format!("{auth}{own_host}");
        } else if let Some(hostname) = self.hostname.as_deref().filter(|h| !h.is_empty()) {
            host = if hostname.contains(':') && !is_ipv6_hostname(hostname) {
                format!("{auth}[{hostname}]")
            } else {
                format!("{auth}{hostname}")
            };
            if let Some(port) = self.port.as_deref().filter(|port| !port.is_empty()) {
                host.push(':');
                host.push_str(port);
            }
        }
        let query = match &self.query {
            Query::Parsed(text) | Query::Object(_, text) => text.as_str(),
            Query::Null | Query::Text(_) => "",
        };
        let mut search = match self.search.as_deref().filter(|search| !search.is_empty()) {
            Some(search) => search.to_string(),
            None if !query.is_empty() => format!("?{query}"),
            None => String::new(),
        };
        let mut pathname = self
            .pathname
            .as_deref()
            .unwrap_or_default()
            .replace('#', "%23")
            .replace('?', "%3F");
        if self.slashes || SLASHED_PROTOCOLS.contains(&protocol.as_str()) {
            if self.slashes || !host.is_empty() {
                if !pathname.is_empty() && !pathname.starts_with('/') {
                    pathname.insert(0, '/');
                }
                host.insert_str(0, "//");
            } else if protocol.starts_with("file") {
                host = "//".into();
            }
        }
        search = search.replace('#', "%23");
        let mut hash = self.hash.clone().unwrap_or_default();
        if !hash.is_empty() && !hash.starts_with('#') {
            hash.insert(0, '#');
        }
        if !search.is_empty() && !search.starts_with('?') {
            search.insert(0, '?');
        }
        format!("{protocol}{host}{pathname}{search}{hash}")
    }

    /// `Url.prototype.resolveObject`, with `self` as `this`, and the properties of `relative`'s
    /// object to copy to the result object before setting the fields.
    fn resolve_object(&self, mut relative: LegacyUrl) -> (LegacyUrl, Copy) {
        let mut result = self.clone();
        result.hash = relative.hash.clone();

        if relative.href.as_deref() == Some("") {
            result.href = Some(result.format());
            return (result, Copy::Nothing);
        }

        if relative.slashes && !truthy(&relative.protocol) {
            result = LegacyUrl {
                protocol: result.protocol,
                ..relative
            };
            if is_slashed(result.protocol.as_deref())
                && truthy(&result.hostname)
                && !truthy(&result.pathname)
            {
                result.pathname = Some("/".into());
                result.path = Some("/".into());
            }
            result.href = Some(result.format());
            return (result, Copy::AllButProtocol);
        }

        if truthy(&relative.protocol) && relative.protocol != result.protocol {
            if !is_slashed(relative.protocol.as_deref()) {
                result = relative;
                result.href = Some(result.format());
                return (result, Copy::All);
            }
            result.protocol = relative.protocol.clone();
            let protocol = relative.protocol.as_deref().unwrap_or_default();
            if !truthy(&relative.host)
                && protocol != "file"
                && protocol != "file:"
                && !is_hostless(Some(protocol))
            {
                let pathname = relative.pathname.clone().unwrap_or_default();
                let mut rel_path = pathname.split('/').map(String::from);
                for part in rel_path.by_ref() {
                    let found = !part.is_empty();
                    relative.host = Some(part);
                    if found {
                        break;
                    }
                }
                let mut rel_path: Vec<String> = rel_path.collect();
                if !truthy(&relative.host) {
                    relative.host = Some(String::new());
                }
                if !truthy(&relative.hostname) {
                    relative.hostname = Some(String::new());
                }
                if rel_path.first().is_none_or(|part| !part.is_empty()) {
                    rel_path.insert(0, String::new());
                }
                if rel_path.len() < 2 {
                    rel_path.insert(0, String::new());
                }
                result.pathname = Some(rel_path.join("/"));
            } else {
                result.pathname = relative.pathname.clone();
            }
            result.search = relative.search.clone();
            result.query = relative.query.clone();
            result.host = Some(relative.host.clone().unwrap_or_default());
            result.auth = relative.auth.clone();
            result.hostname = if truthy(&relative.hostname) {
                relative.hostname.clone()
            } else {
                relative.host.clone()
            };
            result.port = relative.port.clone();
            if truthy(&result.pathname) || truthy(&result.search) {
                result.update_path();
            }
            result.slashes |= relative.slashes;
            result.href = Some(result.format());
            return (result, Copy::Nothing);
        }

        let starts_with_slash =
            |text: &Option<String>| text.as_deref().is_some_and(|text| text.starts_with('/'));
        let is_source_abs = starts_with_slash(&result.pathname);
        let is_rel_abs = truthy(&relative.host) || starts_with_slash(&relative.pathname);
        let mut must_end_abs =
            is_rel_abs || is_source_abs || (truthy(&result.host) && truthy(&relative.pathname));
        let remove_all_dots = must_end_abs;
        let mut src_path = split_path(&result.pathname);
        let mut rel_path = split_path(&relative.pathname);
        let no_leading_slashes =
            truthy(&result.protocol) && !is_slashed(result.protocol.as_deref());

        // The host of a URL without slashes is the first part of the path, so `..` reaches it.
        if no_leading_slashes {
            result.hostname = Some(String::new());
            result.port = None;
            if let Some(host) = result.host.take().filter(|host| !host.is_empty()) {
                prepend_host(&mut src_path, host);
            }
            result.host = Some(String::new());
            if truthy(&relative.protocol) {
                relative.hostname = None;
                relative.port = None;
                result.auth = None;
                if let Some(host) = relative.host.take().filter(|host| !host.is_empty()) {
                    prepend_host(&mut rel_path, host);
                }
            }
            must_end_abs = must_end_abs
                && (rel_path.first().is_some_and(String::is_empty)
                    || src_path.first().is_some_and(String::is_empty));
        }

        if is_rel_abs {
            if relative.host.is_some() {
                if result.host != relative.host {
                    result.auth = None;
                }
                result.host = relative.host.clone();
                result.port = relative.port.clone();
            }
            if relative.hostname.is_some() {
                if result.hostname != relative.hostname {
                    result.auth = None;
                }
                result.hostname = relative.hostname.clone();
            }
            result.search = relative.search.clone();
            result.query = relative.query.clone();
            src_path = rel_path;
        } else if !rel_path.is_empty() {
            src_path.pop();
            src_path.extend(rel_path);
            result.search = relative.search.clone();
            result.query = relative.query.clone();
        } else if relative.search.is_some() {
            if no_leading_slashes {
                let host = (!src_path.is_empty()).then(|| src_path.remove(0));
                result.hostname = host.clone();
                result.host = host;
                result.split_auth_from_host();
            }
            result.search = relative.search.clone();
            result.query = relative.query.clone();
            if result.pathname.is_some() || result.search.is_some() {
                result.update_path();
            }
            result.href = Some(result.format());
            return (result, Copy::Nothing);
        }

        if src_path.is_empty() {
            result.pathname = None;
            result.path = result
                .search
                .as_deref()
                .filter(|search| !search.is_empty())
                .map(|search| format!("/{search}"));
            result.href = Some(result.format());
            return (result, Copy::Nothing);
        }

        // A path that ends in `.` or `..` gets a trailing slash.
        let last = src_path.last().cloned().unwrap_or_default();
        let has_trailing_slash =
            ((truthy(&result.host) || truthy(&relative.host) || src_path.len() > 1)
                && (last == "." || last == ".."))
                || last.is_empty();

        let mut up = 0;
        for i in (0..src_path.len()).rev() {
            if src_path[i] == "." {
                src_path.remove(i);
            } else if src_path[i] == ".." {
                src_path.remove(i);
                up += 1;
            } else if up > 0 {
                src_path.remove(i);
                up -= 1;
            }
        }
        if !must_end_abs && !remove_all_dots {
            for _ in 0..up {
                src_path.insert(0, "..".into());
            }
        }

        if must_end_abs
            && src_path
                .first()
                .is_none_or(|first| !first.is_empty() && !first.starts_with('/'))
        {
            src_path.insert(0, String::new());
        }
        if has_trailing_slash && !src_path.join("/").ends_with('/') {
            src_path.push(String::new());
        }

        let is_absolute = src_path
            .first()
            .is_some_and(|first| first.is_empty() || first.starts_with('/'));
        if no_leading_slashes {
            let host = if is_absolute || src_path.is_empty() {
                String::new()
            } else {
                src_path.remove(0)
            };
            result.hostname = Some(host.clone());
            result.host = Some(host);
            result.split_auth_from_host();
        }

        must_end_abs = must_end_abs || (truthy(&result.host) && !src_path.is_empty());
        if must_end_abs && !is_absolute {
            src_path.insert(0, String::new());
        }
        if src_path.is_empty() {
            result.pathname = None;
            result.path = None;
        } else {
            result.pathname = Some(src_path.join("/"));
        }
        if result.pathname.is_some() || result.search.is_some() {
            result.update_path();
        }
        if truthy(&relative.auth) {
            result.auth = relative.auth.clone();
        }
        result.slashes |= relative.slashes;
        result.href = Some(result.format());
        (result, Copy::Nothing)
    }

    /// For a `host` of the form `user@host`, set `auth` to `user`, and `host` and `hostname` to
    /// the part up to the next `@`.
    fn split_auth_from_host(&mut self) {
        let Some(host) = self.host.clone() else {
            return;
        };
        if host.find('@').is_some_and(|at| at > 0) {
            let mut parts = host.split('@');
            self.auth = parts.next().map(String::from);
            let host = parts.next().map(String::from);
            self.hostname = host.clone();
            self.host = host;
        }
    }
}

/// `Url.prototype.parse` of `input`. DEP0170 is emitted once per instance for an invalid port.
fn parse_legacy(
    scope: &Scope<'_>,
    input: &str,
    parse_query_string: bool,
    slashes_denote_host: bool,
) -> Result<LegacyUrl, Thrown> {
    let (url, invalid_port) = LegacyUrl::parse(input, parse_query_string, slashes_denote_host)
        .map_err(|error| match error {
            LegacyError::InvalidUrl => {
                NodeError::new(ErrorKind::TypeError, "ERR_INVALID_URL", "Invalid URL").into()
            }
            LegacyError::UriMalformed => {
                js::error::throw_uri_error(scope, c"URI malformed");
                Thrown::Pending
            }
        })?;
    if invalid_port && !PORT_WARNED.with(|warned| warned.replace(true)) {
        let message =
            format!("The URL {input} is invalid. Future versions of Node.js will throw an error.");
        deprecation_warning(scope, &message, "DEP0170")?;
    }
    Ok(url)
}

/// Emit DEP0169 for `url.parse`, once per instance and only if the caller isn't in
/// `node_modules`.
fn warn_parse(scope: &Scope<'_>) -> Result<(), Thrown> {
    if PARSE_WARNED.with(Cell::get) || called_from_node_modules(scope)? {
        return Ok(());
    }
    PARSE_WARNED.with(|warned| warned.set(true));
    deprecation_warning(
        scope,
        "`url.parse()` behavior is not standardized and prone to errors that have security \
         implications. Use the WHATWG URL API instead. CVEs are not issued for `url.parse()` \
         vulnerabilities.",
        "DEP0169",
    )?;
    Ok(())
}

/// Property `name` of `object` as a legacy URL field: `None` if it's falsy and not a string,
/// and the string it converts to otherwise.
fn read_field(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Option<String>, Thrown> {
    let value = object.get_property(scope, name)?;
    if value.is_string() {
        return Ok(Some(String::from_jsval(scope, value, ())?));
    }
    if !js::conversion::to_boolean(value) {
        return Ok(None);
    }
    Ok(Some(
        js::JSString::from_value(scope, value)?.to_utf8(scope)?,
    ))
}

/// The legacy URL fields of `object`. A `query` object gets the origin `origin`.
fn read_legacy_url(
    scope: &Scope<'_>,
    object: Object<'_>,
    origin: Origin,
) -> Result<LegacyUrl, Thrown> {
    let query = object.get_property(scope, c"query")?;
    let query = match Object::from_value(scope, query) {
        Ok(query_object) if !query_object.is_callable() => {
            let text = crate::querystring::module::stringify(scope, query, None, None, None)?;
            Query::Object(origin, text.to_utf8(scope)?)
        }
        _ => match read_field(scope, object, c"query")? {
            Some(text) => Query::Text(text),
            None => Query::Null,
        },
    };
    let slashes = object.get_property(scope, c"slashes")?;
    Ok(LegacyUrl {
        protocol: read_field(scope, object, c"protocol")?,
        slashes: js::conversion::to_boolean(slashes),
        auth: read_field(scope, object, c"auth")?,
        host: read_field(scope, object, c"host")?,
        port: read_field(scope, object, c"port")?,
        hostname: read_field(scope, object, c"hostname")?,
        hash: read_field(scope, object, c"hash")?,
        search: read_field(scope, object, c"search")?,
        query,
        pathname: read_field(scope, object, c"pathname")?,
        path: read_field(scope, object, c"path")?,
        href: read_field(scope, object, c"href")?,
    })
}

/// The fields of a `url.Url`, in the order its constructor creates them.
pub(crate) static URL_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"protocol"),
    TemplateKey::Name(c"slashes"),
    TemplateKey::Name(c"auth"),
    TemplateKey::Name(c"host"),
    TemplateKey::Name(c"port"),
    TemplateKey::Name(c"hostname"),
    TemplateKey::Name(c"hash"),
    TemplateKey::Name(c"search"),
    TemplateKey::Name(c"query"),
    TemplateKey::Name(c"pathname"),
    TemplateKey::Name(c"path"),
    TemplateKey::Name(c"href"),
]);

/// The key of the per-global `url.Url` whose fields are all `null`, which the constructor copies.
static EMPTY_URL: u8 = 0;

/// Write the field `name` of a `url.Url`: `value`, or `null` for `None`.
fn write_field<'w>(
    scope: &'w Scope<'_>,
    properties: &mut PropertyWriter<'w>,
    name: &'static std::ffi::CStr,
    value: &Option<String>,
) -> Result<(), ExnThrown> {
    match value {
        Some(value) => properties.write(scope, name, value.as_str()),
        None => properties.write(scope, name, HandleValue::null()),
    }
}

/// Set the fields of `target` to those of `url`, creating them as `init` creates properties. A
/// `query` object is the `query` of `this` or `relative`, by its origin.
fn write_legacy_url(
    scope: &Scope<'_>,
    target: Object<'_>,
    url: &LegacyUrl,
    this: Option<Object<'_>>,
    relative: Option<Object<'_>>,
    init: PropertyInit,
) -> Result<(), Thrown> {
    // SAFETY: the caller guarantees the requirements of `Define`. The JavaScript that runs before
    // the writer finishes can't reach `target`.
    let mut properties = unsafe { target.property_writer(scope, &URL_PROPERTIES, init) }?;
    write_field(scope, &mut properties, c"protocol", &url.protocol)?;
    if url.slashes {
        properties.write(scope, c"slashes", true)?;
    } else {
        properties.write(scope, c"slashes", HandleValue::null())?;
    }
    write_field(scope, &mut properties, c"auth", &url.auth)?;
    write_field(scope, &mut properties, c"host", &url.host)?;
    write_field(scope, &mut properties, c"port", &url.port)?;
    write_field(scope, &mut properties, c"hostname", &url.hostname)?;
    write_field(scope, &mut properties, c"hash", &url.hash)?;
    write_field(scope, &mut properties, c"search", &url.search)?;
    match &url.query {
        Query::Null => properties.write(scope, c"query", HandleValue::null())?,
        Query::Text(text) => properties.write(scope, c"query", text.as_str())?,
        Query::Parsed(text) => {
            let text = js::JSString::from_str(scope, text)?.to_jsval_throwing(scope)?;
            let query = crate::querystring::module::parse(scope, text, None, None, None)?;
            properties.write(scope, c"query", query)?;
        }
        Query::Object(origin, _) => {
            let source = match origin {
                Origin::This => this,
                Origin::Relative => relative,
            };
            match source {
                Some(source) => {
                    properties.write(scope, c"query", source.get_property(scope, c"query")?)?
                }
                None => properties.write(scope, c"query", HandleValue::null())?,
            }
        }
    }
    write_field(scope, &mut properties, c"pathname", &url.pathname)?;
    write_field(scope, &mut properties, c"path", &url.path)?;
    write_field(scope, &mut properties, c"href", &url.href)?;
    properties.finish(scope)?;
    Ok(())
}

/// A new `url.Url` with the fields of `url`, as `new Url()` creates it followed by assignments of
/// the fields.
fn new_url<'s>(scope: &'s Scope<'_>, url: &LegacyUrl) -> Result<Object<'s>, Thrown> {
    let object = url_class::new_instance(scope)?;
    write_legacy_url(scope, object, url, None, None, PropertyInit::Define)?;
    Ok(object)
}

/// `url.parse(input, parseQueryString, slashesDenoteHost)`, which returns a `url.Url` input as
/// is.
fn url_parse<'s>(
    scope: &'s Scope<'_>,
    input: HandleValue<'_>,
    parse_query_string: bool,
    slashes_denote_host: bool,
) -> Result<Object<'s>, Thrown> {
    warn_parse(scope)?;
    if let Ok(object) = Object::from_value(scope, *input) {
        if url_class::class_constructor(scope)?.ordinary_has_instance(scope, input)? {
            return Ok(object);
        }
    }
    let input = validate_string(scope, input, "url")?;
    let parsed = parse_legacy(scope, &input, parse_query_string, slashes_denote_host)?;
    new_url(scope, &parsed)
}

/// `Url.prototype.resolveObject` with `this` and the relative URL object `relative`.
fn resolve_object_of<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    relative: Option<Object<'_>>,
) -> Result<Object<'s>, Thrown> {
    let source = read_legacy_url(scope, this, Origin::This)?;
    let relative_fields = match relative {
        Some(relative) => read_legacy_url(scope, relative, Origin::Relative)?,
        None => LegacyUrl::default(),
    };
    let (resolved, copy) = source.resolve_object(relative_fields);
    let result = new_url(scope, &LegacyUrl::default())?;
    result.assign(scope, this)?;
    if let Some(relative) = relative.filter(|_| copy != Copy::Nothing) {
        result.assign(scope, relative)?;
    }
    write_legacy_url(
        scope,
        result,
        &resolved,
        Some(this),
        relative,
        PropertyInit::Assign,
    )?;
    Ok(result)
}

/// `Url.prototype.resolveObject` with `this` and `relative`, a URL string, which is parsed with
/// `slashesDenoteHost`, or a URL object.
fn resolve_object_value<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    relative: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    if relative.is_string() {
        let input = String::from_jsval(scope, relative, ())?;
        let relative = new_url(scope, &parse_legacy(scope, &input, false, true)?)?;
        return resolve_object_of(scope, this, Some(relative));
    }
    resolve_object_of(scope, this, Object::from_value(scope, *relative).ok())
}

/// `Url.prototype.resolve` with `this` and `relative`.
fn resolve_of(
    scope: &Scope<'_>,
    this: Object<'_>,
    relative: HandleValue<'_>,
) -> Result<String, Thrown> {
    let relative = url_parse(scope, relative, false, true)?;
    let resolved = resolve_object_of(scope, this, Some(relative))?;
    Ok(read_legacy_url(scope, resolved, Origin::This)?.format())
}

/// Whether `value` is present and truthy.
fn flag(value: Option<HandleValue<'_>>) -> bool {
    value.is_some_and(js::conversion::to_boolean)
}

#[core_runtime::jsfunction_class(name = "Url")]
pub mod url_class {
    use super::*;

    /// <https://nodejs.org/api/url.html#legacy-urlobject>: every field `null`, assigned in order.
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let key = std::ptr::from_ref(&EMPTY_URL).addr();
        let empty = js::class::get_shared_object(scope, key).ok_or_else(|| {
            js::error::TypeError(String::from("Url isn't registered")).throw(scope)
        })?;
        Ok(this.assign(scope, empty)?)
    }

    /// Create the `url.Url` with every field `null` that the constructor copies.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let key = std::ptr::from_ref(&EMPTY_URL).addr();
        js::class::get_or_init_shared_object(scope, key, |scope| {
            let empty = Object::new_plain_with_proto(scope, proto.handle())?;
            write_legacy_url(
                scope,
                empty,
                &LegacyUrl::default(),
                None,
                None,
                PropertyInit::Define,
            )
            .map_err(|thrown| thrown.throw(scope))?;
            Ok(empty)
        })?;
        Ok(())
    }

    /// Set the fields of this object to those of the URL `url`.
    #[method(length = 3)]
    pub fn parse<'s>(
        scope: &'s Scope<'_>,
        this: Object<'s>,
        url: HandleValue<'_>,
        parse_query_string: Option<HandleValue<'_>>,
        slashes_denote_host: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let input = validate_string(scope, url, "url")?;
        let parsed = parse_legacy(
            scope,
            &input,
            flag(parse_query_string),
            flag(slashes_denote_host),
        )?;
        write_legacy_url(scope, this, &parsed, None, None, PropertyInit::Assign)?;
        Ok(this)
    }

    /// The URL string of this object's fields.
    pub fn format(scope: &Scope<'_>, this: Object<'_>) -> Result<String, Thrown> {
        Ok(read_legacy_url(scope, this, Origin::This)?.format())
    }

    /// The URL string of `relative` resolved against this URL.
    pub fn resolve(
        scope: &Scope<'_>,
        this: Object<'_>,
        relative: HandleValue<'_>,
    ) -> Result<String, Thrown> {
        resolve_of(scope, this, relative)
    }

    /// A new `Url` of `relative` resolved against this URL.
    pub fn resolve_object<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        relative: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        resolve_object_value(scope, this, relative)
    }

    /// Move a trailing `:` and digits from `host` to `port`, and set `hostname` to the rest of
    /// `host` unless that is empty.
    pub fn parse_host(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let mut url = LegacyUrl {
            host: read_field(scope, this, c"host")?,
            ..LegacyUrl::default()
        };
        url.parse_host();
        if let Some(port) = &url.port {
            this.set_property(scope, c"port", port.as_str())?;
        }
        if let Some(hostname) = &url.hostname {
            this.set_property(scope, c"hostname", hostname.as_str())?;
        }
        Ok(())
    }
}

/// `ERR_MISSING_ARGS` for the argument `name`.
fn missing_args(name: &str) -> NodeError {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_MISSING_ARGS",
        format!("The \"{name}\" argument must be specified"),
    )
}

/// The hostname of the `ws:` URL whose hostname is set to `domain`, or `None` if that fails or
/// `domain` is empty.
fn ascii_hostname(scope: &Scope<'_>, domain: HandleValue<'_>) -> Result<Option<String>, Thrown> {
    let domain = js::JSString::from_value(scope, domain)?.to_utf8(scope)?;
    if domain.is_empty() {
        return Ok(None);
    }
    let mut url = url::Url::parse("ws://x").expect("the URL is valid");
    if url::quirks::set_hostname(&mut url, &domain).is_err() {
        return Ok(None);
    }
    Ok(Some(url::quirks::hostname(&url).to_string()))
}

#[core_runtime::jsmodule(node_args, name = "node:url", default_namespace)]
pub mod module {
    use super::*;

    pub use super::url_class as Url;

    /// <https://nodejs.org/api/url.html#urlfileurltopathurl-options>
    #[function(name = "fileURLToPath")]
    pub fn file_url_to_path(scope: &Scope<'_>, url: HandleValue<'_>) -> Result<String, Thrown> {
        let href = if url.is_string() {
            String::from_jsval(scope, url, ())?
        } else if is_url(scope, url)? {
            let object =
                Object::from_value(scope, *url).map_err(|e| Thrown::from(e.throw(scope)))?;
            js::JSString::from_value(scope, object.get_property(scope, c"href")?)?.to_utf8(scope)?
        } else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "path",
                "of type string or an instance of URL",
                url,
            )
            .into());
        };
        super::file_url_to_path(scope, &href)
    }

    /// <https://nodejs.org/api/url.html#urlpathtofileurlpath-options>
    #[function(name = "pathToFileURL", length = 2)]
    pub fn path_to_file_url<'s>(
        scope: &'s Scope<'_>,
        path: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let path = validate_string(scope, path, "path")?;
        let href = path_to_file_href(&path);
        Ok(Function::construct(
            scope,
            url_constructor(scope)?,
            &[href.as_str()],
        )?)
    }

    /// <https://nodejs.org/api/url.html#urlurltohttpoptionsurl>: an object without a prototype,
    /// with the own enumerable properties of `url` followed by the request options.
    pub fn url_to_http_options<'s>(
        scope: &'s Scope<'_>,
        url: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let url = crate::errors::validate_object(scope, url, "url")?;
        let options = Object::new_plain(scope)?;
        options.set_prototype(scope, js::native::HandleObject::null())?;
        options.assign(scope, url)?;
        let string = |value: HandleValue<'_>| -> Result<String, Thrown> {
            Ok(js::JSString::from_value(scope, value)?.to_utf8(scope)?)
        };
        options.set_property(scope, c"protocol", url.get_property(scope, c"protocol")?)?;
        let hostname = url.get_property(scope, c"hostname")?;
        let bracketed = hostname.is_string() && string(hostname)?.starts_with('[');
        if bracketed {
            let hostname = string(hostname)?;
            let inner = hostname.get(1..hostname.len() - 1).unwrap_or_default();
            options.set_property(scope, c"hostname", inner)?;
        } else {
            options.set_property(scope, c"hostname", hostname)?;
        }
        options.set_property(scope, c"hash", url.get_property(scope, c"hash")?)?;
        let search = url.get_property(scope, c"search")?;
        options.set_property(scope, c"search", search)?;
        let pathname = url.get_property(scope, c"pathname")?;
        options.set_property(scope, c"pathname", pathname)?;
        let part = |value: HandleValue<'_>| -> Result<String, Thrown> {
            match js::conversion::to_boolean(value) {
                true => string(value),
                false => Ok(String::new()),
            }
        };
        let path = format!("{}{}", part(pathname)?, part(search)?);
        options.set_property(scope, c"path", path.as_str())?;
        options.set_property(scope, c"href", url.get_property(scope, c"href")?)?;
        let port = url.get_property(scope, c"port")?;
        if !port.is_string() || !string(port)?.is_empty() {
            let port = js::JSString::from_value(scope, port)?.to_jsval_throwing(scope)?;
            options.set_property(scope, c"port", f64::from_jsval(scope, port, ())?)?;
        }
        let username = url.get_property(scope, c"username")?;
        let password = url.get_property(scope, c"password")?;
        if js::conversion::to_boolean(username) || js::conversion::to_boolean(password) {
            let auth = format!(
                "{}:{}",
                decode_uri_component_throwing(scope, &string(username)?)?,
                decode_uri_component_throwing(scope, &string(password)?)?
            );
            options.set_property(scope, c"auth", auth.as_str())?;
        }
        Ok(options)
    }

    /// <https://nodejs.org/api/url.html#urldomaintoasciidomain>: empty if it's invalid.
    #[function(name = "domainToASCII", length = 1)]
    pub fn domain_to_ascii(
        scope: &Scope<'_>,
        domain: Option<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        let domain = domain.ok_or_else(|| missing_args("domain"))?;
        Ok(ascii_hostname(scope, domain)?.unwrap_or_default())
    }

    /// <https://nodejs.org/api/url.html#urldomaintounicodedomain>: empty if it's invalid.
    #[function(length = 1)]
    pub fn domain_to_unicode(
        scope: &Scope<'_>,
        domain: Option<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        let domain = domain.ok_or_else(|| missing_args("domain"))?;
        Ok(ascii_hostname(scope, domain)?
            .map(|host| idna::domain_to_unicode(&host).0)
            .unwrap_or_default())
    }

    /// <https://nodejs.org/api/url.html#urlparseurlstring-parsequerystring-slashesdenotehost>:
    /// a `url.Url`, with `query` an object if `parseQueryString`. A `url.Url` is returned as is.
    #[function(length = 3)]
    pub fn parse<'s>(
        scope: &'s Scope<'_>,
        url: HandleValue<'_>,
        parse_query_string: Option<HandleValue<'_>>,
        slashes_denote_host: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        url_parse(
            scope,
            url,
            flag(parse_query_string),
            flag(slashes_denote_host),
        )
    }

    /// <https://nodejs.org/api/url.html#urlformaturlobject-options>: the URL string of a legacy
    /// URL object or string, or the `href` of a WHATWG `URL` without the parts `options` turns
    /// off, and with a Unicode host for `options.unicode`.
    #[function(length = 2)]
    pub fn format(
        scope: &Scope<'_>,
        url: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        if url.is_string() {
            let parsed = url_parse(scope, url, false, false)?;
            return Ok(read_legacy_url(scope, parsed, Origin::This)?.format());
        }
        let Ok(object) = Object::from_value(scope, *url) else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "urlObject",
                "one of type object or string",
                url,
            )
            .into());
        };
        if !is_url(scope, url)? {
            return Ok(read_legacy_url(scope, object, Origin::This)?.format());
        }
        let mut fragment = true;
        let mut unicode = false;
        let mut search = true;
        let mut auth = true;
        if let Some(options) = options.filter(|options| js::conversion::to_boolean(*options)) {
            let options = crate::errors::validate_object(scope, options, "options")?;
            for (name, flag) in [
                (c"fragment", &mut fragment),
                (c"unicode", &mut unicode),
                (c"search", &mut search),
                (c"auth", &mut auth),
            ] {
                let value = options.get_property(scope, name)?;
                if !value.is_null() && !value.is_undefined() {
                    *flag = js::conversion::to_boolean(value);
                }
            }
        }
        let href = js::JSString::from_value(scope, object.get_property(scope, c"href")?)?
            .to_utf8(scope)?;
        let Ok(mut parsed) = url::Url::parse(&href) else {
            return Ok(href);
        };
        if !fragment {
            parsed.set_fragment(None);
        }
        if !search {
            parsed.set_query(None);
        }
        if !auth {
            // URLs that cannot have credentials have none to remove.
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
        }
        if let Some(url::Host::Domain(domain)) = parsed.host().filter(|_| unicode) {
            let domain = idna::domain_to_unicode(domain).0;
            let before = &parsed[..url::Position::BeforeHost];
            let after = &parsed[url::Position::AfterHost..];
            return Ok(format!("{before}{domain}{after}"));
        }
        Ok(parsed.into())
    }

    /// <https://nodejs.org/api/url.html#urlresolvefrom-to>
    pub fn resolve(
        scope: &Scope<'_>,
        from: HandleValue<'_>,
        to: HandleValue<'_>,
    ) -> Result<String, Thrown> {
        let source = url_parse(scope, from, false, true)?;
        resolve_of(scope, source, to)
    }

    /// `url.resolveObject(source, relative)`: `relative` if `source` is falsy, and otherwise a
    /// new `url.Url` of `relative` resolved against `source`.
    pub fn resolve_object<'s>(
        scope: &'s Scope<'_>,
        source: HandleValue<'_>,
        relative: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        if !js::conversion::to_boolean(source) {
            return Ok(scope.root_value(relative.get()));
        }
        let source = url_parse(scope, source, false, true)?;
        let resolved = resolve_object_value(scope, source, relative)?;
        Ok(resolved.to_jsval_throwing(scope)?)
    }
}

/// Register `node:url`, whose `URL` and `URLSearchParams` are the global's.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let globals = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        Ok(vec![
            ("URL", url_constructor(scope)?.to_jsval_throwing(scope)?),
            (
                "URLSearchParams",
                js::class::class_constructor::<web_url::url_search_params::URLSearchParamsImpl>(
                    scope,
                )?
                .to_jsval_throwing(scope)?,
            ),
        ])
    };
    let Ok(overrides) = globals() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &overrides)
}
