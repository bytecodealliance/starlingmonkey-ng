// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Error types for the `mozjs::js` API.
//!
//! This module provides:
//!
//! - [`ExnThrown`] — a lightweight unit struct indicating that a JavaScript
//!   exception is pending on the context. Used as the error type in
//!   `Result<T, ExnThrown>` throughout the safe API.
//! - [`CapturedError`] — rich error details captured from a pending exception
//!   via [`ExnThrown::capture`]. Includes the message, source location, stack
//!   trace, and (when available) source line and column range.
//! - [`ConversionError`] — error type for pure Rust type-extraction failures
//!   (e.g., extracting an `i32` from a non-integer `JSVal`).
//!
//! # Throwing errors
//!
//! Use [`throw_type_error`], [`throw_range_error`], or [`throw_internal_error`]
//! to set a pending exception on the context and return `Err(ExnThrown)`.

use std::ffi::{CStr, CString};
use std::fmt;
use std::ptr;

use crate::exception::is_pending;
use crate::gc::scope::Scope;
use crate::Object;
use mozjs::jsapi::{
    JSErrorFormatString, JSExnType, JSString, JS_ReportErrorNumberUTF8, StackFormat,
};
use mozjs::rooted;
use mozjs::rust::wrappers2;
#[cfg(debug_assertions)]
use mozjs::rust::Runtime;

/// A lightweight marker indicating that a JavaScript exception is pending on
/// the context.
///
/// This error type carries no data — it simply signals that a SpiderMonkey
/// operation failed and an exception is pending. Use [`ExnThrown::capture`] to
/// extract details into a [`CapturedError`].
///
/// # Why a unit struct?
///
/// Most code only needs to propagate errors with `?`. The heavy lifting of
/// extracting error messages, filenames, and stack traces is deferred to the
/// few call sites that actually need it (e.g., error reporters, REPL shells).
/// This keeps `Result<T, ExnThrown>` zero-cost in the common case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExnThrown;

impl ExnThrown {
    /// Check whether a SpiderMonkey call succeeded and convert its boolean
    /// result into `Result<(), ExnThrown>`.
    ///
    /// If `ok` is `true`, returns `Ok(())`. Otherwise returns `Err(ExnThrown)`.
    ///
    /// This is the primary bridge between SpiderMonkey's `bool`-returning C++
    /// API and Rust's `Result` type.
    #[inline]
    pub fn check(ok: bool) -> Result<(), ExnThrown> {
        if ok {
            #[cfg(debug_assertions)]
            {
                if let Some(rt) = Runtime::get() {
                    let raw_cx = std::ptr::NonNull::new(rt.as_ptr()).unwrap();
                    let cx = unsafe { mozjs::context::JSContext::from_ptr(raw_cx) };
                    debug_assert!(
                        !unsafe { mozjs::rust::wrappers2::JS_IsExceptionPending(&cx) },
                        "Native JS function returned true but an exception is pending",
                    );
                }
            }
            Ok(())
        } else {
            // Note that we can't do the inverse of the above assert: `false` without a pending
            // exception is legitimate, since SpiderMonkey's API can return `false` without a
            // pending exception for uncatchable exceptions.
            Err(ExnThrown)
        }
    }

    /// Capture the pending exception from the context, clearing it.
    ///
    /// Returns a [`CapturedError`] with whatever information can be extracted
    /// from the pending exception. If no exception is pending, returns a
    /// [`CapturedError`] with default (empty) fields.
    // TODO: use CapturedJSStack here.
    pub fn capture(scope: &Scope<'_>) -> CapturedError {
        // SAFETY: all FFI calls here require a realm to be entered, which is
        // guaranteed by the `Scope` parameter.
        unsafe {
            let raw = scope.cx_mut().raw_cx();
            let Some(exc_val) = crate::exception::take_pending(scope) else {
                return CapturedError::default();
            };

            // For object exceptions, extract the attached stack and, for
            // `Error` objects, the engine's error report.
            let mut stack = None;
            if exc_val.is_object() {
                let exc_obj = Object::from_value(scope, exc_val).unwrap();

                // Try to extract the stack trace from the exception object.
                let maybe_stack =
                    Object::from_raw(scope, wrappers2::ExceptionStackOrNull(exc_obj.handle()));
                if let Some(stack_obj) = maybe_stack {
                    rooted!(in(raw) let mut stack_str: *mut JSString = ptr::null_mut());
                    let ok = wrappers2::BuildStackString(
                        scope.cx_mut(),
                        ptr::null_mut(),
                        stack_obj.handle(),
                        stack_str.handle_mut(),
                        0,
                        StackFormat::Default,
                    );
                    if ok && !stack_str.get().is_null() {
                        stack = Some(crate::conversion::jsstr_to_string(
                            scope,
                            ptr::NonNull::new(stack_str.get()).unwrap(),
                        ));
                    }
                }

                if let Some(report) =
                    crate::exception::error_from_exception(scope, exc_obj.handle())
                {
                    let msg_ptr = report._base.message_.data_;
                    let message = if msg_ptr.is_null() {
                        None
                    } else {
                        Some(CStr::from_ptr(msg_ptr).to_string_lossy().into_owned())
                    };
                    let fn_ptr = report._base.filename.data_;
                    let filename = if fn_ptr.is_null() {
                        None
                    } else {
                        Some(CStr::from_ptr(fn_ptr).to_string_lossy().into_owned())
                    };
                    return CapturedError {
                        message,
                        filename,
                        lineno: report._base.lineno,
                        column: report._base.column._base,
                        stack,
                    };
                }
            }

            // No error report (a primitive throw like `throw "boom"`, or a non-`Error` object
            // without engine error data): stringify the value itself.
            // If an attached stack was extracted above, it gets added to the captured error.
            // Otherwise, the `stack` property is read from the object and and attached.
            if stack.is_none() && exc_val.is_object() {
                let exc_obj = Object::from_value(scope, exc_val).unwrap();
                match exc_obj.get_property(scope, c"stack") {
                    Ok(value) if value.is_string() => {
                        stack = crate::string::Str::from_value(scope, value)
                            .ok()
                            .and_then(|s| s.to_utf8(scope).ok())
                            .filter(|s: &String| !s.is_empty());
                    }
                    Ok(_) => {}
                    Err(ExnThrown) => crate::exception::clear(scope),
                }
            }
            let message = match crate::string::Str::from_value(scope, exc_val) {
                Ok(s) => s
                    .to_utf8(scope)
                    .unwrap_or_else(|_| "(non-Error exception)".into()),
                Err(ExnThrown) => {
                    // ToString itself threw (a symbol, or a throwing
                    // `toString`); discard that secondary exception.
                    crate::exception::clear(scope);
                    "(non-Error exception)".into()
                }
            };
            CapturedError {
                message: Some(message),
                filename: None,
                lineno: 0,
                column: 0,
                stack,
            }
        } // unsafe
    }
}

impl fmt::Display for ExnThrown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JavaScript exception pending")
    }
}

impl std::error::Error for ExnThrown {}

/// Captured details from a JavaScript exception.
///
/// Created by [`ExnThrown::capture`]. Contains the message, source location,
/// and stack trace extracted from the pending exception.
#[derive(Debug, Clone, Default)]
pub struct CapturedError {
    /// The error message, if available.
    pub message: Option<String>,
    /// The source filename, if available.
    pub filename: Option<String>,
    /// The 1-based line number in the source, or 0 if unknown.
    pub lineno: u32,
    /// The 1-based column number in the source (`JSErrorReport::column` is
    /// 1-origin), or 0 if unknown.
    pub column: u32,
    /// The stack trace string, if available.
    pub stack: Option<String>,
}

impl fmt::Display for CapturedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(msg) = &self.message {
            write!(f, "{msg}")?;
        } else {
            write!(f, "JavaScript exception")?;
        }
        // The location is left out when the stack's first frame names it.
        if let Some(file) = &self.filename {
            let location = format!("{file}:{}:{}", self.lineno, self.column);
            let in_stack = self
                .stack
                .as_deref()
                .and_then(|stack| stack.lines().next())
                .is_some_and(|frame| frame.ends_with(&location));
            if !in_stack {
                write!(f, " at {location}")?;
            }
        }
        if let Some(s) = &self.stack {
            write!(f, "\n{s}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CapturedError {}

impl CapturedError {
    /// `context: error`, with the error as [`Display`](fmt::Display) formats it, or `context`
    /// alone if nothing was captured.
    pub fn with_context(&self, context: &str) -> String {
        if self.message.is_none() && self.filename.is_none() && self.stack.is_none() {
            return context.to_string();
        }
        let error = self.to_string();
        format!("{context}: {}", error.trim_end())
    }
}

// ---------------------------------------------------------------------------
// Throw helpers
// ---------------------------------------------------------------------------

/// Format string used to throw JavaScript errors from Rust.
static ERROR_FORMAT_STRING: &CStr = c"{0}";

/// Format string struct for `Error`.
static mut ERROR_FMT: JSErrorFormatString = JSErrorFormatString {
    name: c"RUSTMSG_ERROR".as_ptr(),
    format: ERROR_FORMAT_STRING.as_ptr(),
    argCount: 1,
    exnType: JSExnType::JSEXN_ERR as i16,
};

/// Format string struct for `TypeError`.
static mut TYPE_ERROR_FMT: JSErrorFormatString = JSErrorFormatString {
    name: c"RUSTMSG_TYPE_ERROR".as_ptr(),
    format: ERROR_FORMAT_STRING.as_ptr(),
    argCount: 1,
    exnType: JSExnType::JSEXN_TYPEERR as i16,
};

/// Format string struct for `RangeError`.
static mut RANGE_ERROR_FMT: JSErrorFormatString = JSErrorFormatString {
    name: c"RUSTMSG_RANGE_ERROR".as_ptr(),
    format: ERROR_FORMAT_STRING.as_ptr(),
    argCount: 1,
    exnType: JSExnType::JSEXN_RANGEERR as i16,
};

/// Format string struct for `InternalError`.
static mut INTERNAL_ERROR_FMT: JSErrorFormatString = JSErrorFormatString {
    name: c"RUSTMSG_INTERNAL_ERROR".as_ptr(),
    format: ERROR_FORMAT_STRING.as_ptr(),
    argCount: 1,
    exnType: JSExnType::JSEXN_INTERNALERR as i16,
};

/// Format string struct for `SyntaxError`.
static mut SYNTAX_ERROR_FMT: JSErrorFormatString = JSErrorFormatString {
    name: c"RUSTMSG_SYNTAX_ERROR".as_ptr(),
    format: ERROR_FORMAT_STRING.as_ptr(),
    argCount: 1,
    exnType: JSExnType::JSEXN_SYNTAXERR as i16,
};

/// Format string struct for `URIError`.
static mut URI_ERROR_FMT: JSErrorFormatString = JSErrorFormatString {
    name: c"RUSTMSG_URI_ERROR".as_ptr(),
    format: ERROR_FORMAT_STRING.as_ptr(),
    argCount: 1,
    exnType: JSExnType::JSEXN_URIERR as i16,
};

/// Callback for [`JS_ReportErrorNumberUTF8`].
unsafe extern "C" fn get_error_message(
    _user_ref: *mut std::os::raw::c_void,
    error_number: std::os::raw::c_uint,
) -> *const JSErrorFormatString {
    let num: JSExnType = std::mem::transmute(error_number);
    match num {
        JSExnType::JSEXN_ERR => &raw const ERROR_FMT,
        JSExnType::JSEXN_TYPEERR => &raw const TYPE_ERROR_FMT,
        JSExnType::JSEXN_RANGEERR => &raw const RANGE_ERROR_FMT,
        JSExnType::JSEXN_INTERNALERR => &raw const INTERNAL_ERROR_FMT,
        JSExnType::JSEXN_SYNTAXERR => &raw const SYNTAX_ERROR_FMT,
        JSExnType::JSEXN_URIERR => &raw const URI_ERROR_FMT,
        _ => panic!("Bad error number: {error_number}"),
    }
}

/// Throw an `Error` with the given message and return `ExnThrown`.
pub fn throw_plain_error(scope: &Scope<'_>, error: &CStr) -> ExnThrown {
    throw_js_error(scope, error, JSExnType::JSEXN_ERR as u32)
}

/// Throw a `TypeError` with the given message and return `ExnThrown`.
///
/// The message must be a valid C string (no interior NUL bytes).
pub fn throw_type_error(scope: &Scope<'_>, error: &CStr) -> ExnThrown {
    throw_js_error(scope, error, JSExnType::JSEXN_TYPEERR as u32)
}

/// Throw a `RangeError` with the given message and return `ExnThrown`.
pub fn throw_range_error(scope: &Scope<'_>, error: &CStr) -> ExnThrown {
    throw_js_error(scope, error, JSExnType::JSEXN_RANGEERR as u32)
}

/// Throw an `InternalError` with the given message and return `ExnThrown`.
pub fn throw_internal_error(scope: &Scope<'_>, error: &CStr) -> ExnThrown {
    throw_js_error(scope, error, JSExnType::JSEXN_INTERNALERR as u32)
}

/// Throw a `SyntaxError` with the given message and return `ExnThrown`.
pub fn throw_syntax_error(scope: &Scope<'_>, error: &CStr) -> ExnThrown {
    throw_js_error(scope, error, JSExnType::JSEXN_SYNTAXERR as u32)
}

/// Throw a `URIError` with the given message and return `ExnThrown`.
pub fn throw_uri_error(scope: &Scope<'_>, error: &CStr) -> ExnThrown {
    throw_js_error(scope, error, JSExnType::JSEXN_URIERR as u32)
}

fn throw_js_error(scope: &Scope<'_>, error: &CStr, error_number: u32) -> ExnThrown {
    unsafe {
        JS_ReportErrorNumberUTF8(
            scope.cx_mut().raw_cx(),
            Some(get_error_message),
            std::ptr::null_mut(),
            error_number,
            error.as_ptr(),
        )
    };
    ExnThrown
}

/// Report a simple ASCII error message, setting a pending exception.
///
/// This is a lightweight alternative to the typed error functions above.
/// The message is reported as a generic `Error` (not `TypeError`, etc.).
pub fn report_error_ascii(scope: &Scope<'_>, msg: &CStr) -> ExnThrown {
    unsafe { mozjs::rust::wrappers2::ReportErrorASCII(scope.cx_mut(), msg.as_ptr()) };
    ExnThrown
}

// ---------------------------------------------------------------------------
// Newtype error wrappers
// ---------------------------------------------------------------------------

/// Defines a newtype error wrapper that throws as the named JS error type.
macro_rules! js_error_newtype {
    ($(#[doc = $doc:expr])* $Name:ident, $throw_fn:ident, $fallback:literal) => {
        $(#[doc = $doc])*
        #[derive(Debug, Clone)]
        pub struct $Name(pub String);

        impl $Name {
            /// Throw this error as the corresponding pending JavaScript
            /// exception.
            pub fn throw(&self, scope: &Scope<'_>) -> ExnThrown {
                let c_msg = CString::new(self.0.as_str())
                    .unwrap_or_else(|_| CString::from($fallback));
                $throw_fn(scope, &c_msg)
            }
        }

        impl fmt::Display for $Name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}: {}", stringify!($Name), self.0)
            }
        }

        impl std::error::Error for $Name {}

        impl ThrowException for $Name {
            fn throw(self, scope: &Scope<'_>) -> ExnThrown {
                $Name::throw(&self, scope)
            }
        }
    };
}

js_error_newtype!(
    /// A JavaScript `TypeError` with a message.
    ///
    /// Use this as the `Err` type in `Result<T, TypeError>` to throw a
    /// `TypeError` when a function returns an error. The proc macro system
    /// (`#[jsmethods]`, `#[jsglobals]`, etc.) dispatches via the
    /// [`ThrowException`] trait to call the appropriate SpiderMonkey error
    /// API.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use js::error::TypeError;
    ///
    /// #[method]
    /// fn parse(input: String) -> Result<i32, TypeError> {
    ///     input.parse().map_err(|_| TypeError(format!("invalid integer: {input}")))
    /// }
    /// ```
    TypeError,
    throw_type_error,
    c"type error"
);

js_error_newtype!(
    /// A JavaScript `RangeError` with a message.
    ///
    /// See [`TypeError`] for usage patterns.
    RangeError,
    throw_range_error,
    c"range error"
);

js_error_newtype!(
    /// A JavaScript `SyntaxError` with a message.
    ///
    /// See [`TypeError`] for usage patterns.
    SyntaxError,
    throw_syntax_error,
    c"syntax error"
);

// ---------------------------------------------------------------------------
// ThrowException trait — typed error dispatch for proc macros
// ---------------------------------------------------------------------------

/// Trait for converting a Rust error value into a pending JavaScript exception.
///
/// The `#[jsmethods]`, `#[jsglobals]`, and `#[jsmodule]` proc macros use this
/// trait to dispatch `Err` values to the correct SpiderMonkey error API. For
/// example, returning `Err(TypeError("bad argument".into()))` throws a
/// JavaScript `TypeError`, while returning `Err(String)` throws a `TypeError`.
///
/// # Implementing
///
/// Custom error types (like `DOMExceptionError`) can implement this trait to
/// throw domain-specific exception objects.
pub trait ThrowException {
    /// Set a pending JavaScript exception from this error value.
    ///
    /// After this call, a JS exception must be pending on the context.
    fn throw(self, scope: &Scope<'_>) -> ExnThrown;
}

impl ThrowException for String {
    /// Throw a `TypeError` with this string as the message.
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        throw_error(scope, &self)
    }
}

impl ThrowException for ExnThrown {
    /// No-op: `ExnThrown` indicates an exception is already pending on the
    /// context, so there is nothing additional to throw.
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        debug_assert!(
            is_pending(scope),
            "ExnThrown::throw called but no exception is pending"
        );
        self
    }
}

// ---------------------------------------------------------------------------
// throw_error — convenience helper for proc macro codegen
// ---------------------------------------------------------------------------

/// Throw a `TypeError` with the given message.
///
/// This is used by the `#[jsmethods]` macro to convert Rust `Err` values
/// into JS exceptions. The error message is converted to a `CString`.
///
/// # Safety
///
/// A realm must be entered on the scope's context.
pub fn throw_error(scope: &Scope<'_>, msg: &str) -> ExnThrown {
    let c_msg = CString::new(msg).unwrap_or_else(|_| CString::new("unknown error").unwrap());
    throw_type_error(scope, &c_msg)
}

// ---------------------------------------------------------------------------
// capture_stack_from_error
// ---------------------------------------------------------------------------

/// Capture the current call stack as a `SavedFrame` and store it in `obj`'s
/// [`ERROR_STACK_SLOT`](crate::class::ERROR_STACK_SLOT), for the `stack` accessor on the class's
/// prototype to format when it is read.
///
/// Used by error-like classes (e.g. DOMException, or any class with
/// `js_proto = "Error"`) that need `[[ErrorData]]` behavior. `obj` must be an instance of such a
/// class.
///
/// Leaves the slot empty if no script is running, or if the capture fails. A failed capture's
/// exception is cleared.
pub fn capture_stack_from_error(scope: &Scope<'_>, obj: &Object<'_>) {
    // SpiderMonkey's `Error` keeps at most this many frames (`MAX_REPORTED_STACK_DEPTH`).
    const MAX_FRAMES: u32 = 128;
    match crate::stack::capture_saved_frame(scope, MAX_FRAMES) {
        Ok(Some(frame)) => {
            // SAFETY: `obj` is an instance of an error-data class, which has the slot.
            unsafe {
                crate::object::set_reserved_slot(
                    obj.as_raw(),
                    crate::class::ERROR_STACK_SLOT,
                    &crate::value::from_object(frame.as_raw()),
                )
            }
        }
        Ok(None) => {}
        Err(ExnThrown) => crate::exception::clear(scope),
    }
}
