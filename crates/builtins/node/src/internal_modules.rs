// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Node.js's legacy internal modules that packages still import: `_stream_duplex`,
//! `_stream_passthrough`, `_stream_readable`, `_stream_transform` and `_stream_writable`, which are
//! the `stream` classes, and `_http_agent`, `_http_client`, `_http_common`, `_http_incoming`,
//! `_http_outgoing` and `_http_server`, which hold `http` classes and helpers. Their other exports
//! are stubs, as are the other internal modules' exports.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::stream::{string, Props};

/// The internal modules with implemented exports, and the module and export each one's value
/// comes from, or `None` for the modules whose value is an object of `http` members.
const ALIASES: &[(&str, Option<(&str, &std::ffi::CStr)>)] = &[
    ("node:_http_agent", None),
    ("node:_http_client", None),
    ("node:_http_common", None),
    ("node:_http_incoming", None),
    ("node:_http_outgoing", None),
    ("node:_http_server", None),
    ("node:_stream_duplex", Some(("node:stream", c"Duplex"))),
    (
        "node:_stream_passthrough",
        Some(("node:stream", c"PassThrough")),
    ),
    ("node:_stream_readable", Some(("node:stream", c"Readable"))),
    (
        "node:_stream_transform",
        Some(("node:stream", c"Transform")),
    ),
    ("node:_stream_writable", Some(("node:stream", c"Writable"))),
];

/// The `http` members each `_http_*` module holds: the name in the module, then the name in
/// `http`.
const HTTP_MEMBERS: &[(&str, &[(&std::ffi::CStr, &std::ffi::CStr)])] = &[
    (
        "node:_http_agent",
        &[(c"Agent", c"Agent"), (c"globalAgent", c"globalAgent")],
    ),
    ("node:_http_client", &[(c"ClientRequest", c"ClientRequest")]),
    ("node:_http_common", &[]),
    (
        "node:_http_incoming",
        &[(c"IncomingMessage", c"IncomingMessage")],
    ),
    (
        "node:_http_outgoing",
        &[
            (c"OutgoingMessage", c"OutgoingMessage"),
            (c"validateHeaderName", c"validateHeaderName"),
            (c"validateHeaderValue", c"validateHeaderValue"),
        ],
    ),
    (
        "node:_http_server",
        &[
            (c"STATUS_CODES", c"STATUS_CODES"),
            (c"Server", c"Server"),
            (c"ServerResponse", c"ServerResponse"),
        ],
    ),
];

/// Whether `name` is an internal module with implemented exports.
pub(crate) fn is_implemented(name: &str) -> bool {
    ALIASES.iter().any(|(alias, _)| *alias == name)
}

/// The `default` export of builtin module `name`.
pub(crate) fn module_default<'s>(
    scope: &'s Scope<'_>,
    name: &str,
) -> Result<Object<'s>, ExnThrown> {
    let namespace = core_runtime::module::builtin_namespace(scope, name)?.ok_or_else(|| {
        js::error::TypeError(format!("{name} is not a builtin module")).throw(scope)
    })?;
    namespace
        .obj(scope, c"default")?
        .ok_or_else(|| js::error::TypeError(format!("{name} has no default export")).throw(scope))
}

/// `_http_common._checkIsHttpToken(value)`: whether `value` is a non-empty HTTP token.
fn check_is_http_token(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let value = String::from_jsval(scope, args.get(0), ()).unwrap_or_default();
    Ok(js::value::from_bool(crate::http::is_token(&value)))
}

/// `_http_common._checkInvalidHeaderChar(value)`: whether `value` has a character a header value
/// can't.
fn check_invalid_header_char(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let value = String::from_jsval(scope, args.get(0), ()).unwrap_or_default();
    Ok(js::value::from_bool(crate::http::has_invalid_header_char(
        &value,
    )))
}

/// A case-insensitive regular expression for `pattern`.
fn regexp<'s>(scope: &'s Scope<'_>, pattern: &str) -> Result<Object<'s>, ExnThrown> {
    let class = scope.global().obj(scope, c"RegExp")?.ok_or(ExnThrown)?;
    Function::construct(
        scope,
        class,
        &[string(scope, pattern)?, string(scope, "i")?],
    )
}

/// The value of internal module `name`: a `stream` class, or an object of `http` members.
fn module_value<'s>(scope: &'s Scope<'_>, name: &str) -> Result<Object<'s>, ExnThrown> {
    if let Some((_, Some((module, export)))) = ALIASES.iter().find(|(alias, _)| *alias == name) {
        return module_default(scope, module)?
            .obj(scope, export)?
            .ok_or_else(|| js::error::TypeError(format!("{name} is missing")).throw(scope));
    }
    let http = module_default(scope, "node:http")?;
    let value = Object::new_plain(scope)?;
    let members = HTTP_MEMBERS
        .iter()
        .find(|(module, _)| *module == name)
        .map_or(&[][..], |(_, members)| members);
    for (target, source) in members {
        value.set_property(scope, target, http.val(scope, source)?)?;
    }
    if name == "node:_http_common" {
        let methods = http.obj(scope, c"METHODS")?.ok_or(ExnThrown)?;
        let methods = crate::stream::call_method(scope, methods, c"slice", &[])?;
        value.set_property(scope, c"methods", methods)?;
        value.set_property(scope, c"CRLF", "\r\n")?;
        value.set_property(
            scope,
            c"chunkExpression",
            regexp(scope, "(?:^|\\W)chunked(?:$|\\W)")?,
        )?;
        value.set_property(
            scope,
            c"continueExpression",
            regexp(scope, "(?:^|\\W)100-continue(?:$|\\W)")?,
        )?;
        let token = Function::new_callback(scope, c"checkIsHttpToken", 1, check_is_http_token, ())?;
        value.set_property(scope, c"_checkIsHttpToken", *token)?;
        let invalid = Function::new_callback(
            scope,
            c"checkInvalidHeaderChar",
            1,
            check_invalid_header_char,
            (),
        )?;
        value.set_property(scope, c"_checkInvalidHeaderChar", *invalid)?;
    }
    Ok(value)
}

/// Register internal module `name`, whose value is the `default` export. Its named exports are the
/// value's properties of the names Node.js exports, and stubs for the names the value lacks.
pub(crate) fn register(scope: &Scope<'_>, name: &'static str) -> bool {
    let register = || -> Result<(), ExnThrown> {
        let value = module_value(scope, name)?;
        let mut exports = Vec::new();
        for &(export, kind) in crate::stub::exports_of(name).unwrap_or_default() {
            let c_export = std::ffi::CString::new(export).map_err(|_| ExnThrown)?;
            let member = value.get_property_by_name(scope, &c_export)?;
            let member = if member.is_undefined() {
                crate::stub::stub_value(scope, name, export, kind)?
            } else {
                member
            };
            exports.push((export, member));
        }
        exports.push(("default", value.to_jsval_throwing(scope)?));
        core_runtime::module::register_synthetic_module(scope, name, &exports)
    };
    register().is_ok()
}
