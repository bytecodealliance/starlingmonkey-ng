// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `ComponentError` global.
//!
//! `ComponentError` represents the `err` arm of a WIT `result<_, err>` when it
//! crosses the JS boundary. An imported function whose declared result is
//! `result<_, err>` rejects or throws a `ComponentError` holding the `err`
//! payload. An exported function declared `result<_, _>` that throws a
//! `ComponentError` has its `.payload` lifted as the `err` value (see
//! [`crate::exports::call_export`]).
//!
//! An instance holds a non-enumerable own `message`, derived from the payload,
//! and the payload itself behind a non-enumerable `payload` getter on the
//! prototype. A string payload becomes the message verbatim. A variant payload
//! `{tag, val?}` becomes its tag, followed by `: val` when `val` is a string,
//! number, BigInt or boolean. An `undefined` payload yields no own `message`,
//! so `message` is the empty string `Error.prototype` has. Any other payload
//! yields `"<ToString(payload)> (see error.payload)"`. An enum payload thrown as
//! an instance of its type's class becomes the case name.
//!
//! `Object.prototype.toString` gives `[object Error]` for an instance, as it
//! does for an instance of a JavaScript class extending `Error`.
//!
//! An `err` arm whose type is a named WIT type other than a resource has a class
//! of its own extending `ComponentError`, named after the type (see
//! [`ErrorClass`]). Its payloads cross into JS as instances of that class.

use std::cell::RefCell;
use std::collections::HashMap;

use core_runtime::{jsclass, jsmethods};

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::JSTracer;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::value::{mangle_resource_name, TypeShape};

/// Own-property under which the constructor stores the message it computed.
const MESSAGE_FIELD: &std::ffi::CStr = c"message";

#[jsclass(js_proto = "Error", name = "ComponentError", to_string_tag = "Error")]
pub struct ComponentError {
    /// The `err` payload of this error, `undefined` until the constructor runs.
    payload: Heap<Value>,
}

#[jsmethods]
impl ComponentError {
    /// `new ComponentError(value)`: store `value` as the payload and derive an
    /// `Error` message from it, as the module documentation describes.
    #[constructor]
    fn new(&self, scope: &Scope<'_>, value: HandleValue<'_>) -> Result<(), ExnThrown> {
        self.data_mut().payload.set(value.get());
        let message = match tagged_message(scope, value)? {
            Some(message) => message,
            None if value.is_undefined() => return Ok(()),
            None if value.is_string() => String::from_jsval_throwing(scope, value, ())?,
            None => format!(
                "{} (see error.payload)",
                String::from_jsval_throwing(scope, value, ())?
            ),
        };

        // `Error.prototype` has only an empty `message`, so `toString` and
        // `stack` surface this own-property instead. It is defined rather than
        // assigned, so it is non-enumerable like a native `Error`'s own
        // `message`, and omitted by `Object.keys` and `JSON.stringify`.
        self.define_property(scope, MESSAGE_FIELD, message, 0)?;
        Ok(())
    }

    /// The `err` payload of this error.
    #[getter]
    fn payload<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        self.data().payload.get(scope)
    }
}

/// Name `ComponentError.prototype`, so an instance's `name`, and the prefix its
/// `toString` gives, is `ComponentError` rather than the `Error` it inherits.
pub(crate) fn name_prototype(scope: &Scope<'_>) {
    if let Some(proto) = js::class::get_prototype_object_for::<ComponentErrorImpl>(scope) {
        // Writable, configurable and not enumerable, as `Error.prototype.name` is.
        let _ = proto.define_property(scope, c"name", "ComponentError", 0);
    }
}

/// The message of a variant payload `{tag, val?}`: the tag, followed by `: val`
/// when `val` is a string, number, BigInt or boolean. `None` for any other value.
fn tagged_message(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Option<String>, ExnThrown> {
    let Ok(obj) = Object::from_value(scope, value) else {
        return Ok(None);
    };
    let tag = obj.get_property(scope, c"tag")?;
    if !tag.is_string() {
        return Ok(None);
    }
    let tag = String::from_jsval_throwing(scope, tag, ())?;
    let val = obj.get_property(scope, c"val")?;
    Ok(Some(
        if val.is_string() || val.is_number() || val.is_bigint() || val.is_boolean() {
            format!("{tag}: {}", String::from_jsval_throwing(scope, val, ())?)
        } else if val.is_undefined() {
            tag
        } else {
            format!("{tag} (see error.payload)")
        },
    ))
}

/// If `value` is a `ComponentError` instance, return its `.payload`, and `None`
/// otherwise.
///
/// The brand check is by class identity, so an unrelated object that merely names
/// itself `ComponentError` does not pass.
pub fn component_error_payload<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Option<HandleValue<'s>> {
    let err = ComponentError::from_jsval(scope, value, ()).ok()?;
    // `get` roots the payload on `scope`, so the caller can hold it across a later
    // allocation. Binding to a local drops the `data()` borrow guard before return.
    let payload = err.data().payload.get(scope);
    Some(payload)
}

/// The `err` payload that `reason`, a thrown exception or a rejection reason,
/// lowers as in a `result` whose `err` arm has type `err`, which is `None` for an
/// arm without a payload. Returns `None` if `reason` has no such payload.
///
/// - A `ComponentError`, including an instance of a class extending it, lowers
///   as its `payload`.
/// - Any other reason, for an arm without a payload, lowers as that arm and is
///   logged to stderr, since the arm cannot hold it.
/// - Any other `Error` lowers as its `message` if `err` is a `string`, as the
///   index of the case of that name if `err` is an `enum`, and as the case of
///   that name if `err` is a `variant` with a payload-less case of that name. It
///   has no payload otherwise.
/// - Any other value lowers as itself, which lowering then checks against `err`.
pub(crate) fn err_payload<'s>(
    scope: &'s Scope<'_>,
    reason: HandleValue<'s>,
    err: Option<&TypeShape>,
) -> Option<HandleValue<'s>> {
    if let Some(payload) = component_error_payload(scope, reason) {
        return Some(payload);
    }
    let Some(err) = err else {
        match String::from_jsval(scope, reason, ()) {
            Ok(text) => eprintln!("a `result`'s `err` arm without a payload dropped: {text}"),
            Err(_) => js::exception::clear(scope),
        }
        return Some(HandleValue::undefined());
    };
    if !js::exception::is_error_object(scope, reason) {
        return Some(reason);
    }
    let error = js::Object::from_value(scope, reason).ok()?;
    let message = error.get_property(scope, MESSAGE_FIELD).ok()?;
    let text = String::from_jsval(scope, message, ()).ok()?;
    match err {
        TypeShape::String => Some(message),
        TypeShape::Enum(names) => {
            let index = names.iter().position(|name| *name == text)?;
            Some(scope.root_value(js::value::from_u32(index as u32)))
        }
        TypeShape::Variant(cases)
            if cases
                .iter()
                .any(|(name, payload)| payload.is_none() && *name == text) =>
        {
            let wrapper = js::Object::new_plain(scope).ok()?;
            wrapper.set_property(scope, c"tag", message).ok()?;
            Some(scope.root_value(wrapper.as_value()))
        }
        _ => None,
    }
}

/// A named WIT type in the `err` arm of a `result`, other than a resource. Its
/// payloads cross into JS as instances of a class extending `ComponentError`,
/// named after the type in upper camel case (see [`error_class`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ErrorClass {
    /// The WIT name of the interface defining the type, or `None` for a type
    /// the world defines.
    pub interface: Option<String>,
    /// The type's WIT name.
    pub name: String,
    /// The type's case names, mangled, in declaration order, if it is an enum,
    /// and empty otherwise. An enum payload's error message is its case name.
    pub enum_cases: Vec<String>,
}

impl ErrorClass {
    /// The class's JS name.
    pub fn js_name(&self) -> String {
        mangle_resource_name(&self.name)
    }
}

js::instance_local! {
    /// The function [`error_class`] creates classes with, created on first use.
    static CLASS_FACTORY: RefCell<Option<Heap<Value>>> = const { RefCell::new(None) };
}

js::instance_local! {
    /// Every class [`error_class`] created, by the type it was created for.
    static ERROR_CLASSES: RefCell<HashMap<ErrorClass, Heap<js::object::Object>>> =
        RefCell::new(HashMap::new());
}

/// The source of the function creating an error class. It takes the prototype
/// of `ComponentError` and the class name, and returns a class extending
/// `ComponentError` with that name, whose instances' `name` is also that name.
const CLASS_FACTORY_SOURCE: &str = r#"
(proto, name) => {
    const Base = proto.constructor;
    const Class = ({ [name]: class extends Base {} })[name];
    Object.defineProperty(Class.prototype, "name", {
        value: name,
        writable: true,
        configurable: true,
    });
    return Class;
}
"#;

/// The class for payloads of `class`'s type, created on first use and shared by
/// every later use in the same instance.
pub fn error_class<'s>(scope: &'s Scope<'_>, class: &ErrorClass) -> Result<Object<'s>, ExnThrown> {
    let existing = ERROR_CLASSES.with(|cell| cell.borrow().get(class).map(|heap| heap.get(scope)));
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let factory = CLASS_FACTORY.with(|cell| cell.borrow().as_ref().map(|heap| heap.get(scope)));
    let factory = match factory {
        Some(factory) => factory,
        None => {
            let factory = js::compile::evaluate(scope, CLASS_FACTORY_SOURCE)?;
            CLASS_FACTORY.with(|cell| *cell.borrow_mut() = Some(Heap::from(factory.get())));
            factory
        }
    };
    let proto =
        js::class::get_prototype_object_for::<ComponentErrorImpl>(scope).ok_or_else(|| {
            js::error::throw_type_error(scope, c"ComponentError is not installed on this global")
        })?;
    let name = scope.root_value(js::JSString::from_str(scope, &class.js_name())?.as_value());
    let proto = scope.root_value(proto.as_value());
    let created = Function::call(scope, HandleValue::undefined(), factory, &[proto, name])?;
    let created = Object::from_value(scope, created).map_err(|_| {
        js::error::throw_type_error(scope, c"the error class factory returned no class")
    })?;
    ERROR_CLASSES.with(|cell| cell.borrow_mut().insert(class.clone(), Heap::from(created)));
    Ok(created)
}

/// Set a new error holding the `err` payload `payload` as the pending exception:
/// an instance of `class`'s class, or a `ComponentError` without one.
pub(crate) fn throw_err(
    scope: &Scope<'_>,
    payload: HandleValue<'_>,
    class: Option<&ErrorClass>,
) -> ExnThrown {
    let error = match class {
        Some(class) => error_class(scope, class).and_then(|class| {
            let class = scope.root_value(class.as_value());
            Function::construct(scope, class, &[payload])
        }),
        None => ComponentError::new(scope, payload).map(|error| *error),
    };
    // An enum payload is a case index, and the case name makes the better message.
    let error = error.and_then(|error| {
        let case = class.and_then(|class| {
            let index = payload.is_int32().then(|| payload.to_int32())?;
            class.enum_cases.get(usize::try_from(index).ok()?)
        });
        if let Some(case) = case {
            error.define_property(scope, MESSAGE_FIELD, case.as_str(), 0)?;
        }
        Ok(error)
    });
    match error {
        Ok(error) => js::exception::set_pending(
            scope,
            scope.root_value(error.as_value()),
            js::native::ExceptionStackBehavior::Capture,
        ),
        // Building the error itself threw. Its pending exception is used instead.
        Err(exn) => exn,
    }
}

/// Trace the error-class factory and every error class.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_error_classes(trc: *mut JSTracer) {
    CLASS_FACTORY.with(|cell| {
        if let Some(factory) = &*cell.as_ptr() {
            factory.trace(trc);
        }
    });
    ERROR_CLASSES.with(|cell| {
        for class in (*cell.as_ptr()).values() {
            class.trace(trc);
        }
    });
}

/// Drop the error-class factory and every error class. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    CLASS_FACTORY.with(|cell| *cell.borrow_mut() = None);
    ERROR_CLASSES.with(|cell| cell.borrow_mut().clear());
}
