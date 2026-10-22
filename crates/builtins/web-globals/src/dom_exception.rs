// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `DOMException` implementation.
//!
//! Implements the [`DOMException`] interface from the WebIDL specification,
//! including the constructor, prototype accessors, legacy error code constants,
//! and the `Symbol.toStringTag` property.
//!
//! [`DOMException`]: https://webidl.spec.whatwg.org/#idl-DOMException

use core_runtime::{webidl_interface, webidl_methods};
use js::class::Ref;
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::native::ExceptionStackBehavior;
use js::prelude::ToJSVal;

/// https://webidl.spec.whatwg.org/#dfn-error-names-table
///
/// Maps DOMException error names to their legacy error code values.
/// Names not present in this table have legacy code 0.
const ERROR_NAMES: &[(&str, u16)] = &[
    ("IndexSizeError", 1),
    ("HierarchyRequestError", 3),
    ("WrongDocumentError", 4),
    ("InvalidCharacterError", 5),
    ("NoModificationAllowedError", 7),
    ("NotFoundError", 8),
    ("NotSupportedError", 9),
    ("InUseAttributeError", 10),
    ("InvalidStateError", 11),
    ("SyntaxError", 12),
    ("InvalidModificationError", 13),
    ("NamespaceError", 14),
    ("InvalidAccessError", 15),
    ("TypeMismatchError", 17),
    ("SecurityError", 18),
    ("NetworkError", 19),
    ("AbortError", 20),
    ("URLMismatchError", 21),
    ("QuotaExceededError", 22),
    ("TimeoutError", 23),
    ("InvalidNodeTypeError", 24),
    ("DataCloneError", 25),
];

/// Look up the legacy error code for a DOMException name.
///
/// https://webidl.spec.whatwg.org/#dom-domexception-code
/// "Return the legacy code indicated in the error names table for this
/// DOMException object's name, or 0 if no such entry exists."
fn legacy_code_for_name(name: &str) -> u16 {
    ERROR_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(0, |&(_, code)| code)
}

/// <https://webidl.spec.whatwg.org/#idl-DOMException>
#[webidl_interface(js_proto = "Error")]
pub struct DOMException {
    name: String,
    message: String,
}

#[webidl_methods]
impl DOMException {
    pub const INDEX_SIZE_ERR: u16 = 1;
    pub const DOMSTRING_SIZE_ERR: u16 = 2;
    pub const HIERARCHY_REQUEST_ERR: u16 = 3;
    pub const WRONG_DOCUMENT_ERR: u16 = 4;
    pub const INVALID_CHARACTER_ERR: u16 = 5;
    pub const NO_DATA_ALLOWED_ERR: u16 = 6;
    pub const NO_MODIFICATION_ALLOWED_ERR: u16 = 7;
    pub const NOT_FOUND_ERR: u16 = 8;
    pub const NOT_SUPPORTED_ERR: u16 = 9;
    pub const INUSE_ATTRIBUTE_ERR: u16 = 10;
    pub const INVALID_STATE_ERR: u16 = 11;
    pub const SYNTAX_ERR: u16 = 12;
    pub const INVALID_MODIFICATION_ERR: u16 = 13;
    pub const NAMESPACE_ERR: u16 = 14;
    pub const INVALID_ACCESS_ERR: u16 = 15;
    pub const VALIDATION_ERR: u16 = 16;
    pub const TYPE_MISMATCH_ERR: u16 = 17;
    pub const SECURITY_ERR: u16 = 18;
    pub const NETWORK_ERR: u16 = 19;
    pub const ABORT_ERR: u16 = 20;
    pub const URL_MISMATCH_ERR: u16 = 21;
    pub const QUOTA_EXCEEDED_ERR: u16 = 22;
    pub const TIMEOUT_ERR: u16 = 23;
    pub const INVALID_NODE_TYPE_ERR: u16 = 24;
    pub const DATA_CLONE_ERR: u16 = 25;

    /// https://webidl.spec.whatwg.org/#dom-domexception-domexception
    ///
    /// `constructor(optional DOMString message = "", optional DOMString name = "Error")`
    #[constructor]
    fn new_js(message: Option<String>, name: Option<String>) -> Self {
        Self {
            // Step 1: Set `this`'s `name` to _name_.
            name: name.unwrap_or_else(|| "Error".to_string()),
            // Step 2: Set `this`'s `message` to _message_.
            message: message.unwrap_or_default(),
        }
    }

    /// Creates a new `DOMException`.
    ///
    /// In difference to the JS-facing constructor `new_js`, both arguments
    /// to `new` are required, with no defaults.
    pub fn new(message: String, name: String) -> Self {
        Self { name, message }
    }

    /// https://webidl.spec.whatwg.org/#dom-domexception-name
    #[getter]
    pub fn name(&self) -> Ref<'_, str> {
        // Return `this`'s `name`.
        Ref::map(self.data(), |data| data.name.as_str())
    }

    /// https://webidl.spec.whatwg.org/#dom-domexception-message
    #[getter]
    pub fn message(&self) -> Ref<'_, str> {
        // Return `this`'s `message`.
        Ref::map(self.data(), |data| data.message.as_str())
    }

    /// https://webidl.spec.whatwg.org/#dom-domexception-code
    ///
    /// Returns the legacy code indicated in the error names table for this
    /// DOMException object's name, or 0 if no such entry exists.
    #[getter]
    pub fn code(&self) -> i32 {
        legacy_code_for_name(&self.data().name) as i32
    }
}

/// Throw a DOMException with the given name and message.
///
/// This creates a new DOMException object and sets it as the pending exception.
pub fn throw_dom_exception(scope: &Scope<'_>, name: &str, message: &str) -> ExnThrown {
    let exception = match DOMException::new(scope, message.into(), name.into()) {
        Ok(exc) => exc,
        Err(_) => return ExnThrown,
    };

    let exc_val = exception
        .to_jsval(scope)
        .expect("Value conversion doesn't fail");
    js::exception::set_pending(scope, exc_val, ExceptionStackBehavior::Capture)
}

/// A typed error representing a `DOMException` to be thrown.
///
/// Use this as the `Err` type in `Result<T, DOMExceptionError>` in
/// `#[jsmethods]`, `#[jsglobals]`, etc. The proc macro's error dispatch
/// will construct a JavaScript `DOMException` object and set it as the
/// pending exception.
///
/// # Example
///
/// ```rust,ignore
/// use web_globals::dom_exception::DOMExceptionError;
///
/// #[method]
/// fn decode(input: String) -> Result<String, DOMExceptionError> {
///     if !is_valid(&input) {
///         return Err(DOMExceptionError::new("InvalidCharacterError", "invalid input"));
///     }
///     Ok(do_decode(&input))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct DOMExceptionError {
    /// The DOMException error name (e.g. "InvalidCharacterError", "NotFoundError").
    pub name: &'static str,
    /// The error message.
    pub message: String,
}

impl DOMExceptionError {
    /// Create a new `DOMExceptionError` with the given name and message.
    pub fn new(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DOMExceptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}

impl std::error::Error for DOMExceptionError {}

impl ThrowException for DOMExceptionError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        throw_dom_exception(scope, self.name, &self.message)
    }
}
