// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Node.js errors: JS errors with a `code` property, such as `ERR_INVALID_ARG_TYPE`, and the
//! argument validation that throws them.
//!
//! See <https://nodejs.org/api/errors.html#nodejs-error-codes>.

use std::ffi::CStr;

use js::class_spec::JSProtoKey;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::JSExnType;
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

mod web;

/// `Number.MAX_SAFE_INTEGER`.
pub(crate) const MAX_SAFE_INTEGER: f64 = 9007199254740991.0;

/// A `SystemError` with the code `key` and the message `message`: an `Error` named `SystemError`
/// with an own `kIsNodeError` property, a `toString` that includes the code, and the `info` object
/// `fill_info` fills as a read-only enumerable property. `errno` and `syscall` are accessors over
/// `info`'s properties of the same names, as are `path` and `dest` if `info` has them.
pub(crate) fn system_error<'s>(
    scope: &'s Scope<'_>,
    key: &'static str,
    message: String,
    fill_info: impl FnOnce(Object<'_>) -> Result<(), ExnThrown>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let error = NodeError::new(ErrorKind::Error, key, message).to_native_value(scope)?;
    let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
    let readonly = js::class_spec::JSPROP_READONLY as u32;
    object.define_value_by_id(scope, K_IS_NODE_ERROR.id(scope)?, true, readonly)?;
    object.define_property(scope, c"name", "SystemError", 0)?;
    let to_string = js::Function::new_callback(scope, c"toString", 0, system_error_to_string, ())?;
    object.define_property(scope, c"toString", to_string, 0)?;
    let info = Object::new_plain(scope)?;
    fill_info(info)?;
    let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
    object.define_property(scope, c"info", info, enumerable | readonly)?;
    let accessors: [(&CStr, js::function::Callback, js::function::Callback, bool); 4] = [
        (c"errno", get_info::<0>, set_info::<0>, true),
        (c"syscall", get_info::<1>, set_info::<1>, true),
        (c"path", get_info::<2>, set_info::<2>, false),
        (c"dest", get_info::<3>, set_info::<3>, false),
    ];
    for (name, getter, setter, always) in accessors {
        if !always && info.get_property(scope, name)?.is_undefined() {
            continue;
        }
        let getter = js::Function::new_callback(scope, name, 0, getter, info)?;
        let setter = js::Function::new_callback(scope, name, 1, setter, info)?;
        object.define_accessor(scope, name, Some(*getter), Some(*setter), enumerable)?;
    }
    Ok(error)
}

/// The `info` property a `SystemError` accessor reads and writes: `errno`, `syscall`, `path` and
/// `dest` for `KEY` 0 to 3.
fn info_key(key: u8) -> &'static CStr {
    [c"errno", c"syscall", c"path", c"dest"][usize::from(key)]
}

/// The getter of a `SystemError`'s accessor over its `info`, which is the payload. `path` and
/// `dest` are converted with `toString` unless they are `null` or `undefined`.
fn get_info<const KEY: u8>(
    scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let info = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let value = info.get_property(scope, info_key(KEY))?;
    if KEY < 2 || value.is_null_or_undefined() {
        return Ok(value.get());
    }
    let object = Object::from_value_coerce(scope, value)?;
    Ok(
        js::Function::call_by_name(scope, object.handle(), c"toString", js::function::EmptyArgs)?
            .get(),
    )
}

/// The setter of a `SystemError`'s accessor over its `info`, which is the payload. `path` and
/// `dest` are stored as a `Buffer` of the value's string, or `undefined` for a falsy value.
fn set_info<const KEY: u8>(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let info = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let value = args.get(0);
    if KEY < 2 {
        info.set_property(scope, info_key(KEY), value)?;
    } else if js::conversion::to_boolean(value) {
        let text = js::JSString::from_value(scope, value)?.to_utf8(scope)?;
        let buffer = crate::buffer::new_buffer(scope, text.as_bytes())?;
        info.set_property(scope, info_key(KEY), buffer)?;
    } else {
        info.set_property(scope, info_key(KEY), js::value::undefined())?;
    }
    Ok(js::value::undefined())
}

/// The `toString` of a `SystemError`: `${name} [${code}]: ${message}`.
fn system_error_to_string(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    let part = |name| -> Result<String, ExnThrown> {
        js::JSString::from_value(scope, this.get_property(scope, name)?)?.to_utf8(scope)
    };
    let text = format!(
        "{} [{}]: {}",
        part(c"name")?,
        part(c"code")?,
        part(c"message")?
    );
    Ok(js::JSString::from_str(scope, &text)?.as_value())
}

/// `ERR_BLOCKING_NOT_ALLOWED` for `what`, such as `fs.readFileSync`, where blocking host I/O
/// isn't allowed, and `None` where it is.
pub(crate) fn blocking_not_allowed(what: &str) -> Option<NodeError> {
    let reason = core_runtime::event_loop::blocking_host_io_refusal()?;
    Some(NodeError::new(
        ErrorKind::Error,
        "ERR_BLOCKING_NOT_ALLOWED",
        format!("{what} would block {reason}"),
    ))
}

/// The JS error constructor a [`NodeError`] is an instance of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Error,
    TypeError,
    RangeError,
    UriError,
}

impl ErrorKind {
    /// The exception type of errors of this kind.
    fn exception_type(self) -> JSExnType {
        match self {
            ErrorKind::Error => JSExnType::JSEXN_ERR,
            ErrorKind::TypeError => JSExnType::JSEXN_TYPEERR,
            ErrorKind::RangeError => JSExnType::JSEXN_RANGEERR,
            ErrorKind::UriError => JSExnType::JSEXN_URIERR,
        }
    }

    /// The key of this kind's constructor and prototype.
    fn proto_key(self) -> JSProtoKey {
        match self {
            ErrorKind::Error => JSProtoKey::JSProto_Error,
            ErrorKind::TypeError => JSProtoKey::JSProto_TypeError,
            ErrorKind::RangeError => JSProtoKey::JSProto_RangeError,
            ErrorKind::UriError => JSProtoKey::JSProto_URIError,
        }
    }
}

/// A Node.js error: an `Error`, `TypeError`, `RangeError` or `URIError` with a `code` property.
#[derive(Debug)]
pub struct NodeError {
    pub kind: ErrorKind,
    pub code: &'static str,
    pub message: String,
}

impl NodeError {
    pub fn new(kind: ErrorKind, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            code,
            message: message.into(),
        }
    }

    /// `ERR_ILLEGAL_CONSTRUCTOR`, for a constructor that only the runtime calls.
    pub fn illegal_constructor() -> Self {
        Self::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        )
    }

    /// `ERR_INVALID_THIS`: `this` is not an instance of `class`.
    pub fn invalid_this(class: &str) -> Self {
        Self::new(
            ErrorKind::TypeError,
            "ERR_INVALID_THIS",
            format!("Value of \"this\" must be of type {class}"),
        )
    }

    /// `ERR_INVALID_ARG_TYPE`: the argument `name` is `received`, which is not `expected`, a
    /// description such as "of type string" or "an instance of Buffer".
    pub fn invalid_arg_type(
        scope: &Scope<'_>,
        name: &str,
        expected: &str,
        received: HandleValue<'_>,
    ) -> Self {
        // Node.js names an argument that is described by its position, such as "first argument",
        // without quotes.
        let subject = if name.ends_with(" argument") {
            name.to_string()
        } else if name.contains('.') {
            format!("\"{name}\" property")
        } else {
            format!("\"{name}\" argument")
        };
        Self::new(
            ErrorKind::TypeError,
            "ERR_INVALID_ARG_TYPE",
            format!(
                "The {subject} must be {expected}. {}",
                describe_received(scope, received)
            ),
        )
    }

    /// `ERR_INVALID_ARG_VALUE`: the argument `name` is `received`, which is invalid because of
    /// `reason`. A `name` with a `.` is called a property.
    pub fn invalid_arg_value(
        scope: &Scope<'_>,
        name: &str,
        received: HandleValue<'_>,
        reason: &str,
    ) -> Self {
        Self::new(
            ErrorKind::TypeError,
            "ERR_INVALID_ARG_VALUE",
            format!(
                "The {} '{name}' {reason}. Received {}",
                if name.contains('.') {
                    "property"
                } else {
                    "argument"
                },
                inspect_received(scope, received)
            ),
        )
    }

    /// `ERR_OUT_OF_RANGE`: the value of `name` is `received`, which is not `range`.
    pub fn out_of_range(name: &str, range: &str, received: &str) -> Self {
        Self::new(
            ErrorKind::RangeError,
            "ERR_OUT_OF_RANGE",
            format!(
                "The value of \"{name}\" is out of range. It must be {range}. Received {received}"
            ),
        )
    }

    /// `ERR_MISSING_ARGS`: the arguments `names` must be specified.
    pub fn missing_args(names: &[&str]) -> Self {
        let names: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
        let list = match names.as_slice() {
            [name] => format!("{name} argument"),
            [first, second] => format!("{first} and {second} arguments"),
            [rest @ .., last] => format!("{}, and {last} arguments", rest.join(", ")),
            [] => "arguments".to_string(),
        };
        Self::new(
            ErrorKind::TypeError,
            "ERR_MISSING_ARGS",
            format!("The {list} must be specified"),
        )
    }

    /// `ERR_METHOD_NOT_IMPLEMENTED`: `what` cannot work in this runtime.
    pub fn not_implemented(what: &str) -> Self {
        Self::new(
            ErrorKind::Error,
            "ERR_METHOD_NOT_IMPLEMENTED",
            format!("The {what} method is not implemented"),
        )
    }
}

/// `String(value)`, which converts a symbol to its descriptive string where `ToString` throws.
pub(crate) fn js_string(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    if value.is_symbol() {
        return Ok(crate::inspect::symbol_to_string(scope, value));
    }
    String::from_jsval(scope, value, ()).map_err(|_| ExnThrown)
}

/// The message of the `RangeError` that Node.js throws when the call stack overflows.
const STACK_OVERFLOW_MESSAGE: &std::ffi::CStr = c"Maximum call stack size exceeded";

/// The message of SpiderMonkey's `InternalError` for a script that overflows the stack.
const OVER_RECURSED_MESSAGE: &std::ffi::CStr = c"too much recursion";

/// Throw the `RangeError` "Maximum call stack size exceeded", which Node.js throws when the call
/// stack overflows.
pub(crate) fn stack_overflow(scope: &Scope<'_>) -> ExnThrown {
    js::error::throw_range_error(scope, STACK_OVERFLOW_MESSAGE)
}

/// Enter a level of recursion with [`js::context::RecursionGuard`], or throw [`stack_overflow`]'s
/// error at the recursion limit. Native code that recurses over a structure from content code
/// calls this at each level, and holds the guard until the level returns.
pub(crate) fn check_stack<'s>(
    scope: &'s Scope<'_>,
) -> Result<js::context::RecursionGuard<'s>, ExnThrown> {
    js::context::RecursionGuard::enter(scope).ok_or_else(|| stack_overflow(scope))
}

/// Whether the pending exception is a stack overflow, as Node.js's `isStackOverflowError` checks
/// it: the error of [`stack_overflow`], or the `InternalError` SpiderMonkey throws for a script
/// that overflows the stack. Leaves the exception pending.
pub(crate) fn is_stack_overflow_pending(scope: &Scope<'_>) -> bool {
    let Some(error) = js::exception::get_pending(scope) else {
        return false;
    };
    if !error.is_object() {
        return false;
    }
    let Some(object) = std::ptr::NonNull::new(error.to_object()) else {
        return false;
    };
    let object = scope.root_object(object);
    let Some(report) = js::exception::error_from_exception(scope, object) else {
        return false;
    };
    let expected = match report.exnType {
        kind if kind == JSExnType::JSEXN_RANGEERR as i16 => STACK_OVERFLOW_MESSAGE,
        kind if kind == JSExnType::JSEXN_INTERNALERR as i16 => OVER_RECURSED_MESSAGE,
        _ => return false,
    };
    let message = report._base.message_.data_;
    // SAFETY: a non-null `message_` is a NUL-terminated UTF-8 string that the report owns, and the
    // report lives as long as `object`.
    !message.is_null() && unsafe { std::ffi::CStr::from_ptr(message) } == expected
}

/// Throw `ERR_INVALID_THIS` for a `this` that isn't an instance of `class`. This is the
/// `invalid_this` function of the `#[jsfunction_class]`es with state.
pub(crate) fn invalid_this(scope: &Scope<'_>, class: &str) -> ExnThrown {
    NodeError::invalid_this(class).throw(scope)
}

impl NodeError {
    /// The error as a JS error object, with its `code`, without throwing it. The errors of Node's
    /// `ERR_` codes inherit from [`node_error_prototype`]'s prototype for their kind and code.
    /// Errors with other codes, such as zlib's `Z_DATA_ERROR` and Brotli's `ERR__ERROR_` codes,
    /// inherit from their kind's prototype, as in Node.
    pub fn to_value<'s>(self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
        self.create(scope, true)
    }

    /// The error as [`to_value`](Self::to_value) creates it, but with its kind's prototype for
    /// every code, as Node's C++ code creates errors.
    pub fn to_native_value<'s>(self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
        self.create(scope, false)
    }

    fn create<'s>(
        self,
        scope: &'s Scope<'_>,
        node_prototype: bool,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let error = js::error::new_error(scope, self.kind.exception_type(), &self.message)?;
        let object = Object::from_value(scope, error).map_err(|e| e.throw(scope))?;
        let internal = self.code.starts_with("ERR_") && !self.code.starts_with("ERR__");
        if internal && node_prototype {
            let prototype = node_error_prototype(scope, self.kind, self.code)?;
            object.set_prototype(scope, prototype.handle())?;
        }
        object.set_property(scope, c"code", self.code)?;
        Ok(error)
    }
}

impl ThrowException for NodeError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        // Only an out-of-memory error can stop this, and that one is then pending instead.
        if let Ok(error) = self.to_value(scope) {
            js::exception::set_pending(scope, error, js::native::ExceptionStackBehavior::Capture);
        }
        ExnThrown
    }
}

/// `kIsNodeError`, the symbol of the property that marks the errors of Node's `ERR_` codes and
/// `SystemError`s.
static K_IS_NODE_ERROR: js::symbol::CachedSymbol = js::symbol::CachedSymbol::new(c"kIsNodeError");

/// The keys of the per-global caches of [`node_error_prototype`], one for each [`ErrorKind`].
static NODE_ERROR_PROTOTYPE_KEYS: [u8; 4] = [0; 4];

/// The prototype of the errors of kind `kind` with the `ERR_` code `code`, the prototype of the
/// `NodeError` class that Node's `makeNodeErrorWithCode` creates for them. It inherits from
/// `kind`'s prototype, and has a `constructor` getter returning `kind`'s constructor, a `toString`
/// returning `` `${this.name} [${code}]: ${this.message}` ``, and a `kIsNodeError` getter returning
/// `true`, all non-enumerable. Each global creates one for each kind and code, on first use.
fn node_error_prototype<'s>(
    scope: &'s Scope<'_>,
    kind: ErrorKind,
    code: &'static str,
) -> Result<Object<'s>, ExnThrown> {
    let key = std::ptr::from_ref(&NODE_ERROR_PROTOTYPE_KEYS[kind as usize]).addr();
    js::class::get_or_init_named_shared_object(scope, key, code, |scope| {
        let base = js::class::get_class_prototype(scope, kind.proto_key())?;
        let prototype = Object::new_plain_with_proto(scope, base)?;
        let constructor = crate::support::class_object(scope, kind.proto_key())?;
        let getter = js::Function::new_callback(
            scope,
            c"get constructor",
            0,
            base_constructor,
            constructor,
        )?;
        prototype.define_accessor(scope, c"constructor", Some(*getter), None, 0)?;
        let to_string =
            js::Function::new_callback(scope, c"toString", 0, node_error_to_string, code)?;
        prototype.define_property(scope, c"toString", *to_string, 0)?;
        let marker = crate::support::shared_named_function(
            scope,
            is_node_error_getter,
            c"get [kIsNodeError]",
            0,
        )?;
        prototype.define_accessor_by_id(
            scope,
            K_IS_NODE_ERROR.id(scope)?,
            Some(marker),
            None,
            0,
        )?;
        Ok(prototype)
    })
}

/// The `constructor` getter of a [`node_error_prototype`], whose payload is the constructor it
/// returns.
fn base_constructor(
    _scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    Ok(*payload)
}

/// The `kIsNodeError` getter of a [`node_error_prototype`].
fn is_node_error_getter(
    _scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    Ok(js::value::from_bool(true))
}

/// The `kIsNodeError` symbol, `internal/errors.kIsNodeError`.
pub(crate) fn is_node_error_symbol<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    K_IS_NODE_ERROR.value(scope)
}

/// Whether `error` has a `kIsNodeError` property, own or inherited, as the errors of Node's `ERR_`
/// codes that [`NodeError::to_value`] creates and [`system_error`]'s errors have. Runs the traps
/// of a proxy in `error`'s prototype chain.
pub(crate) fn is_node_error(scope: &Scope<'_>, error: Object<'_>) -> Result<bool, ExnThrown> {
    error.has_property_by_id(scope, K_IS_NODE_ERROR.id(scope)?)
}

/// The `toString` of a [`node_error_prototype`], whose payload is the error code:
/// `` `${this.name} [${code}]: ${this.message}` ``.
fn node_error_to_string(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let this = Object::from_value_coerce(scope, scope.root_value(args.this()))?;
    let name = this.get_property(scope, c"name")?;
    let name = String::from_jsval(scope, name, ()).map_err(|_| ExnThrown)?;
    let code = String::from_jsval(scope, payload, ()).map_err(|_| ExnThrown)?;
    let message = this.get_property(scope, c"message")?;
    let message = String::from_jsval(scope, message, ()).map_err(|_| ExnThrown)?;
    let text = format!("{name} [{code}]: {message}");
    js::conversion::ToJSVal::to_jsval_raw_throwing(&js::JSString::from_str(scope, &text)?, scope)
}

/// The end of an `ERR_INVALID_ARG_TYPE` message describing `value`, such as "Received type number
/// (42)" or "Received an instance of Object". An exception thrown while an object is described is
/// cleared, and the object is described as `[object Object]`.
fn describe_received(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    if value.is_undefined() {
        return "Received undefined".into();
    }
    if value.is_null() {
        return "Received null".into();
    }
    if let Ok(object) = Object::from_value(scope, value) {
        return match describe_object(scope, object) {
            Ok(text) => format!("Received {text}"),
            Err(ExnThrown) => {
                js::exception::clear(scope);
                "Received [object Object]".into()
            }
        };
    }
    format!("Received {}", describe_primitive(scope, value))
}

/// The object `object` as Node.js's `determineSpecificType` describes it: `function ${name}` for
/// a function, `an instance of ${name}` for an object whose `constructor` has a `name` property,
/// and `util.inspect(object, { depth: -1 })` otherwise. Errors from reading those properties are
/// returned with the exception pending.
fn describe_object(scope: &Scope<'_>, object: Object<'_>) -> Result<String, ExnThrown> {
    if object.is_callable() {
        let name = object.get_property(scope, c"name")?;
        return Ok(format!(
            "function {}",
            js::JSString::from_value(scope, name)?.to_utf8(scope)?
        ));
    }
    let constructor = object.get_property(scope, c"constructor")?;
    if let Ok(constructor) = Object::from_value(scope, constructor) {
        if constructor.has_property(scope, c"name")? {
            let name = constructor.get_property(scope, c"name")?;
            let name = js::JSString::from_value(scope, name)?.to_utf8(scope)?;
            return Ok(format!("an instance of {name}"));
        }
    }
    let options = crate::inspect::InspectOptions {
        depth: Some(-1.0),
        ..Default::default()
    };
    crate::inspect::inspect(scope, object.to_jsval_throwing(scope)?, options)
}

/// The primitive `value`, which is neither `null` nor `undefined`, as Node.js's
/// `determineSpecificType` describes it, such as `type number (70000)` or `type string ('abc')`.
fn describe_primitive(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    if value.is_string() {
        let text = String::from_jsval(scope, value, ()).unwrap_or_default();
        let text = if text.encode_utf16().count() > 28 {
            let head: Vec<u16> = text.encode_utf16().take(25).collect();
            format!("{}...", String::from_utf16_lossy(&head))
        } else {
            text
        };
        if !text.contains('\'') {
            return format!("type string ('{text}')");
        }
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("type string (\"{escaped}\")");
    }
    if value.is_symbol() {
        return format!(
            "type symbol ({})",
            crate::inspect::symbol_to_string(scope, value)
        );
    }
    if value.is_number() && value.to_number() == 0.0 && value.to_number().is_sign_negative() {
        return "type number (-0)".into();
    }
    let suffix = if value.is_bigint() { "n" } else { "" };
    let text = js::JSString::from_value(scope, value)
        .and_then(|text| text.to_utf8(scope))
        .unwrap_or_default();
    format!("type {} ({text}{suffix})", type_of(value))
}

/// `value` as Node.js's `determineSpecificType` describes it in an error message, such as "type
/// number (42)" or "an instance of Object".
pub fn describe_value(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    let received = describe_received(scope, value);
    received
        .strip_prefix("Received ")
        .map(String::from)
        .unwrap_or(received)
}

/// The `typeof` of a primitive `value`.
fn type_of(value: HandleValue<'_>) -> &'static str {
    if value.is_string() {
        "string"
    } else if value.is_number() {
        "number"
    } else if value.is_boolean() {
        "boolean"
    } else if value.is_symbol() {
        "symbol"
    } else if value.is_bigint() {
        "bigint"
    } else if value.is_undefined() {
        "undefined"
    } else {
        "object"
    }
}

/// A short rendering of `value` for error messages: strings quoted and truncated to 25 characters,
/// other primitives as `String(value)`.
/// `util.inspect` of `value`, cut to 128 characters followed by `...` if longer, as Node.js's
/// `ERR_INVALID_ARG_VALUE` shows the value it received.
fn inspect_received(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    let inspected = crate::inspect::InspectOptions::current(scope)
        .and_then(|options| crate::inspect::inspect(scope, value, options));
    let Ok(text) = inspected else {
        js::exception::clear(scope);
        return inspect_short(scope, value);
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > 128 {
        format!("{}...", String::from_utf16_lossy(&units[..128]))
    } else {
        text
    }
}

fn inspect_short(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    if value.is_string() {
        let string = String::from_jsval(scope, value, ()).unwrap_or_default();
        let mut chars = string.chars();
        let head: String = chars.by_ref().take(25).collect();
        return if chars.next().is_some() {
            format!("'{head}...'")
        } else {
            format!("'{head}'")
        };
    }
    if value.is_symbol() {
        return "Symbol()".into();
    }
    js::JSString::from_value(scope, value)
        .and_then(|string| string.to_utf8(scope))
        .unwrap_or_default()
}

/// `value` as a string, or `ERR_INVALID_ARG_TYPE` naming the argument `name` if it isn't one.
pub fn validate_string(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<String, NodeError> {
    if !value.is_string() {
        return Err(NodeError::invalid_arg_type(
            scope,
            name,
            "of type string",
            value,
        ));
    }
    String::from_jsval(scope, value, ()).map_err(|_| {
        NodeError::new(
            ErrorKind::Error,
            "ERR_INTERNAL_ASSERTION",
            "converting a string failed",
        )
    })
}

/// The argument `value`, named `name`, as an integer from `min` to `max`, as Node.js's
/// `validateInteger` checks it.
pub fn validate_integer(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    min: f64,
    max: f64,
) -> Result<f64, Thrown> {
    if !value.is_number() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type number", value).into());
    }
    let number = value.to_number();
    if !number.is_finite() || number.fract() != 0.0 {
        return Err(
            NodeError::out_of_range(name, "an integer", &out_of_range_received(number)).into(),
        );
    }
    if !(min..=max).contains(&number) {
        return Err(NodeError::out_of_range(
            name,
            &format!(
                ">= {} && <= {}",
                number_to_string(min),
                number_to_string(max)
            ),
            &out_of_range_received(number),
        )
        .into());
    }
    Ok(number)
}

/// The argument `value`, named `name`, as a number of at least `min`, as Node.js's
/// `validateNumber(value, name, min)` checks it. `NaN` is out of range.
pub fn validate_number(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    min: f64,
) -> Result<f64, Thrown> {
    if !value.is_number() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type number", value).into());
    }
    let number = value.to_number();
    if number.is_nan() || number < min {
        let range = format!(">= {}", number_to_string(min));
        return Err(NodeError::out_of_range(name, &range, &out_of_range_received(number)).into());
    }
    Ok(number)
}

/// `number` as JS's `Number.prototype.toString()` converts it, such as `1e+21` for `1e21` and
/// `1.5e-7` for `0.00000015`.
pub fn number_to_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".into();
    }
    if number == 0.0 {
        return "0".into();
    }
    if number.is_infinite() {
        return if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .into();
    }
    let sign = if number < 0.0 { "-" } else { "" };
    // `{:e}` writes the shortest digits that round-trip, as `d.ddde<exponent>`.
    let scientific = format!("{:e}", number.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` writes an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().expect("the exponent is an integer") + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let exponent = n - 1;
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        let (head, tail) = digits.split_at(1);
        let fraction = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        format!("{head}{fraction}e{exponent_sign}{}", exponent.abs())
    };
    format!("{sign}{body}")
}

/// `number` as Node.js's `ERR_OUT_OF_RANGE` message shows a received number: as `inspect` shows
/// it, with `_` between groups of three digits for an integer above 2**32 in magnitude.
pub fn out_of_range_received(number: f64) -> String {
    if number.is_finite() && number.fract() == 0.0 && number.abs() > 4294967296.0 {
        return add_numerical_separator(&number_to_string(number));
    }
    if number == 0.0 && number.is_sign_negative() {
        return "-0".into();
    }
    number_to_string(number)
}

/// Node.js's `addNumericalSeparator`: `_` inserted before every third character from the end,
/// after a leading `-`.
pub(crate) fn add_numerical_separator(text: &str) -> String {
    let start = usize::from(text.starts_with('-'));
    let mut end = text.len();
    let mut groups = Vec::new();
    while end >= start + 4 {
        groups.push(&text[end - 3..end]);
        end -= 3;
    }
    let mut out = text[..end].to_string();
    for group in groups.iter().rev() {
        out.push('_');
        out.push_str(group);
    }
    out
}

/// `value` as an object, or `ERR_INVALID_ARG_TYPE` naming the argument `name`, as Node.js's
/// `validateObject` checks it: `null` and functions are rejected, and arrays too unless
/// `allow_array`.
pub fn validate_plain_object<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    allow_array: bool,
) -> Result<Object<'s>, Thrown> {
    let invalid = || NodeError::invalid_arg_type(scope, name, "of type object", value);
    let object = Object::from_value(scope, *value).map_err(|_| invalid())?;
    if object.is_callable() || (!allow_array && js::Array::is_array(scope, object.handle())?) {
        return Err(invalid().into());
    }
    Ok(object)
}

/// `value` as an object, or `ERR_INVALID_ARG_TYPE` naming the argument `name` if it isn't one, as
/// Node.js's `validateObject` checks it with `kValidateObjectAllowObjects`: arrays and functions
/// are accepted. [`validate_plain_object`] is the check without those flags.
pub fn validate_object<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<Object<'s>, NodeError> {
    Object::from_value(scope, *value)
        .map_err(|_| NodeError::invalid_arg_type(scope, name, "of type object", value))
}

/// `ERR_INVALID_ARG_TYPE` naming the argument `name` unless `value` is `undefined` or a
/// non-callable object with an `aborted` property, as an `AbortSignal` has.
pub fn validate_abort_signal(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<(), Thrown> {
    if value.is_undefined() {
        return Ok(());
    }
    let has_aborted = match Object::from_value(scope, *value) {
        Ok(object) if !object.is_callable() => object.has_property(scope, c"aborted")?,
        _ => false,
    };
    if !has_aborted {
        return Err(
            NodeError::invalid_arg_type(scope, name, "an instance of AbortSignal", value).into(),
        );
    }
    Ok(())
}

/// A new `AbortError`: an instance of a subclass of `Error`, with the message "The operation was
/// aborted", the own properties `code` (`ABORT_ERR`) and `name` (`AbortError`) in that order, and
/// `cause` as its non-enumerable `cause` if given.
pub fn abort_error<'s>(
    scope: &'s Scope<'_>,
    cause: Option<HandleValue<'_>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let error = js::error::new_error(scope, JSExnType::JSEXN_ERR, "The operation was aborted")?;
    let object = Object::from_value(scope, error).map_err(|e| e.throw(scope))?;
    let proto = abort_error_class::prototype_object(scope)?;
    object.set_prototype(scope, proto.handle())?;
    object.set_property(scope, c"code", "ABORT_ERR")?;
    object.set_property(scope, c"name", "AbortError")?;
    if let Some(cause) = cause {
        object.define_property(scope, c"cause", cause, 0)?;
    }
    Ok(error)
}

#[core_runtime::jsfunction_class(name = "AbortError", class_syntax)]
pub mod abort_error_class {
    use super::*;

    /// Node.js's internal `AbortError`: [`abort_error`] with `options.cause`, if `options` is
    /// an object.
    #[constructor]
    pub fn create<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let cause = match options.map(|options| Object::from_value(scope, *options)) {
            Some(Ok(options)) if options.has_property(scope, c"cause")? => {
                Some(options.get_property(scope, c"cause")?)
            }
            _ => None,
        };
        let error = abort_error(scope, cause)?;
        Object::from_value(scope, error).map_err(|e| e.throw(scope))
    }

    /// Make `AbortError.prototype` inherit from `Error.prototype`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let error_ctor = crate::stack_trace::error_constructor(scope)?;
        let error_proto =
            js::class::get_class_prototype(scope, js::class_spec::JSProtoKey::JSProto_Error)?;
        proto.set_prototype(scope, error_proto)?;
        ctor.set_prototype(scope, error_ctor.handle())
    }
}

/// `value` as a callable object, or `ERR_INVALID_ARG_TYPE` naming the argument `name` if it
/// isn't one.
/// Node.js's `validateBoolean`: `value`, or `ERR_INVALID_ARG_TYPE` naming `name` if it isn't a
/// boolean.
pub(crate) fn validate_boolean(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<bool, Thrown> {
    if !value.is_boolean() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type boolean", value).into());
    }
    Ok(value.to_boolean())
}

pub fn validate_function<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<Object<'s>, NodeError> {
    Object::from_value(scope, *value)
        .ok()
        .filter(|object| object.is_callable())
        .ok_or_else(|| NodeError::invalid_arg_type(scope, name, "of type function", value))
}

/// An error from a builtin whose body mixes `NodeError`s with JS operations returning
/// `ExnThrown`: a pending JS exception, or a Node.js error to throw.
pub enum Thrown {
    /// A JS exception is pending.
    Pending,
    /// A Node.js error to throw.
    Node(NodeError),
}

impl From<ExnThrown> for Thrown {
    fn from(_: ExnThrown) -> Self {
        Thrown::Pending
    }
}

impl From<NodeError> for Thrown {
    fn from(error: NodeError) -> Self {
        Thrown::Node(error)
    }
}

impl From<js::conversion::ConversionError> for Thrown {
    fn from(error: js::conversion::ConversionError) -> Self {
        match error {
            js::conversion::ConversionError::ExnPending => Thrown::Pending,
            js::conversion::ConversionError::Failure(message) => Thrown::Node(NodeError::new(
                ErrorKind::TypeError,
                "ERR_INVALID_ARG_TYPE",
                message.to_string_lossy(),
            )),
        }
    }
}

impl ThrowException for Thrown {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        match self {
            Thrown::Pending => ExnThrown,
            Thrown::Node(error) => error.throw(scope),
        }
    }
}

/// The Node.js error code, a POSIX errno name, of the I/O error `error`.
pub fn io_error_code(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match error.kind() {
        NotFound => "ENOENT",
        PermissionDenied => "EACCES",
        AlreadyExists => "EEXIST",
        NotADirectory => "ENOTDIR",
        IsADirectory => "EISDIR",
        DirectoryNotEmpty => "ENOTEMPTY",
        ReadOnlyFilesystem => "EROFS",
        InvalidInput => "EINVAL",
        ConnectionRefused => "ECONNREFUSED",
        ConnectionReset => "ECONNRESET",
        ConnectionAborted => "ECONNABORTED",
        TimedOut => "ETIMEDOUT",
        AddrInUse => "EADDRINUSE",
        AddrNotAvailable => "EADDRNOTAVAIL",
        BrokenPipe => "EPIPE",
        Unsupported => "ENOSYS",
        _ => "EIO",
    }
}

/// Make the web classes throw Node's errors in this runtime, as `--node-compat` mode does.
pub(crate) fn install_web_errors() {
    js::error::set_binding_error_hook(Some(web::binding_error));
}
