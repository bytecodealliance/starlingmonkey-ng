// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `ComponentError` class, which represents the `err` case of a WIT
//! `result` in JavaScript.
//!
//! A call to an imported function that returns the `err` case of a `result`
//! throws a `ComponentError`, or rejects with one if the function is async.
//! The error's `payload` property holds the lifted `err` value. If the `err`
//! type is a named WIT type other than a resource, the error is an instance of
//! a subclass of `ComponentError` named after the type (see [`ErrorClass`]).
//!
//! An exported function that returns a `result` returns its `err` case by
//! throwing, or by rejecting if it is async. `err_payload` converts the
//! thrown value into the `err` value.
//!
//! The constructor sets the error's `message` from the payload:
//!
//! - A string payload is the message.
//! - An object with a string `tag` property, such as a lifted variant, gives
//!   the message `tag: val` if its `val` property is a string, number, BigInt
//!   or boolean, `tag` if `val` is `undefined`, and `tag (see error.payload)`
//!   otherwise.
//! - An `undefined` payload sets no `message`, so `message` is the empty string
//!   inherited from `Error.prototype`.
//! - Any other payload gives the message `<payload> (see error.payload)`, with
//!   the payload converted to a string.
//!
//! An error created by `throw_err` for an enum `err` type has the case name
//! as its message instead.
//!
//! `Object.prototype.toString` returns `[object Error]` for an instance.

use std::cell::RefCell;
use std::collections::HashMap;

use core_runtime::{jsclass, jsmethods};

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::handle::{Heap, RootedHeap};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use wit_name_mapping::mangle_resource_name;

use crate::value::{TypeShape, ENUMERATE};

const MESSAGE_FIELD: &std::ffi::CStr = c"message";

#[jsclass(js_proto = "Error", to_string_tag = "Error")]
pub struct ComponentError {
    /// The `err` payload of this error, `undefined` until the constructor runs.
    payload: Heap<Value>,
}

#[jsmethods]
impl ComponentError {
    /// `new ComponentError(value)` stores `value` as the payload and sets
    /// `message` from it as described in the module documentation.
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

        // Defining the property instead of assigning it makes it non-enumerable,
        // like the `message` of a native `Error`.
        self.define_property(scope, MESSAGE_FIELD, message, 0)?;
        Ok(())
    }

    /// The `err` payload of this error.
    #[getter]
    fn payload<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        self.data().payload.get(scope)
    }
}

/// Define `ComponentError.prototype.name` as "ComponentError".
///
/// Error subclasses use value properties for their `name` instead of getters,
/// so this matches the behavior of native `Error` subclasses.
pub(crate) fn name_prototype(scope: &Scope<'_>) {
    let proto = js::class::get_prototype_object_for::<ComponentErrorImpl>(scope)
        .expect("`ComponentError` is registered before its prototype is named");
    // Writable, configurable and not enumerable, as `Error.prototype.name` is.
    proto
        .define_property(scope, c"name", "ComponentError", 0)
        .expect("defining a property of a fresh prototype can only fail due to OOM");
}

/// Return the message for a payload that is an object with a string `tag`
/// property, in the format described in the module documentation, or `None`
/// for any other payload.
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

/// Return the payload of `value` if it is a `ComponentError`, including an
/// instance of a subclass, or `None` otherwise.
pub fn component_error_payload<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Option<HandleValue<'s>> {
    let err = ComponentError::from_jsval(scope, value, ()).ok()?;
    // Binding to a local drops the `data()` borrow guard before return.
    let payload = err.data().payload.get(scope);
    Some(payload)
}

/// Convert `reason`, a thrown exception or a rejection reason, into the `err`
/// value of a `result`. `err_type` is the type of the `result`'s `err` case, or
/// `None` if that case has no value. No exception may be pending.
///
/// If `err_type` is `None`, `undefined` is returned, and `reason` is printed to
/// stderr with its message and stack. Nothing is printed if `reason` is
/// `undefined`, or a `ComponentError` whose payload is `undefined`.
///
/// Otherwise, the `err` value is:
///
/// - the payload, if `reason` is a `ComponentError`, including an instance of a
///   subclass,
/// - `reason` itself, if it isn't an `Error` object, without checking it
///   against `err_type`,
/// - a value derived from the `message` of an `Error` object (see
///   `err_from_message`).
///
/// Returns `Err` with the reason if an `Error` object can't be converted.
pub(crate) fn err_payload<'s>(
    scope: &'s Scope<'_>,
    reason: HandleValue<'s>,
    err_type: Option<&TypeShape>,
) -> Result<HandleValue<'s>, String> {
    let payload = component_error_payload(scope, reason);
    let Some(err_type) = err_type else {
        if !payload.unwrap_or(reason).is_undefined() {
            report_dropped(scope, reason);
        }
        return Ok(HandleValue::undefined());
    };
    if let Some(payload) = payload {
        return Ok(payload);
    }
    if !js::exception::is_error_object(scope, reason) {
        return Ok(reason);
    }
    err_from_message(scope, reason, err_type)
}

/// Print `value`, which was thrown for an `err` case without a value, to stderr
/// with its message and stack.
fn report_dropped(scope: &Scope<'_>, value: HandleValue<'_>) {
    let captured = crate::exports::capture_value(scope, value);
    eprintln!(
        "{}",
        captured
            .with_context("the `err` case of a `result` has no value, so this error was dropped")
    );
}

/// Convert the `Error` object `error` into an `err` value of type `err_type`,
/// based on its `message` converted to a string:
///
/// - For a `string` type, the value is the message.
/// - For an `enum` type, the value is the index of the case named by the
///   message.
/// - For a `variant` type, the value is `{tag: message}`. The case named by the
///   message must have no value.
///
/// Returns `Err` with the reason if `err_type` is any other type, if no case
/// has that name, or if reading `message` or converting it to a string throws.
/// The exception is cleared.
fn err_from_message<'s>(
    scope: &'s Scope<'_>,
    error: HandleValue<'s>,
    err_type: &TypeShape,
) -> Result<HandleValue<'s>, String> {
    let thrown = |context: &str| ExnThrown::capture(scope).with_context(context);
    let error = Object::from_value(scope, error).expect("an `Error` is an object");
    let message = error
        .get_property(scope, MESSAGE_FIELD)
        .map_err(|_| thrown("reading the error's `message` threw"))?;
    let text = String::from_jsval_throwing(scope, message, ())
        .map_err(|_| thrown("converting the error's `message` to a string threw"))?;
    let message = if message.is_string() {
        message
    } else {
        let string = js::JSString::from_str(scope, &text)
            .map_err(|_| thrown("converting the error's `message` to a string threw"))?;
        scope.root_value(string.as_value())
    };
    match err_type {
        TypeShape::String => Ok(message),
        TypeShape::Enum(names) => match names.iter().position(|name| *name == text) {
            Some(index) => Ok(scope.root_value(js::value::from_u32(index as u32))),
            None => Err(format!(
                "the error's message \"{text}\" names no case of the `err` type, which has the \
                 cases {}",
                names.join(", ")
            )),
        },
        TypeShape::Variant(cases) => {
            if !cases
                .iter()
                .any(|(name, payload)| payload.is_none() && *name == text)
            {
                return Err(format!(
                    "the error's message \"{text}\" names no case without a value of the `err` \
                     type"
                ));
            }
            let wrapper =
                Object::new_plain(scope).map_err(|_| thrown("creating the `err` value threw"))?;
            wrapper
                .define_property(scope, c"tag", message, ENUMERATE)
                .map_err(|_| thrown("creating the `err` value threw"))?;
            Ok(scope.root_value(wrapper.as_value()))
        }
        _ => Err(
            "an `Error` can only be converted into an `err` value of type `string`, or of \
                  an enum or variant type. Throw a `ComponentError` holding the `err` value \
                  instead"
                .to_string(),
        ),
    }
}

/// A named WIT type, other than a resource, used as the `err` type of a
/// `result`. Errors with a payload of this type are instances of a subclass of
/// `ComponentError` whose name is the type's name in UpperCamelCase (see
/// [`create_error_class`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ErrorClass {
    /// The WIT name of the interface defining the type, or `None` for a type
    /// the world defines.
    pub interface: Option<String>,
    /// The type's WIT name.
    pub name: String,
    /// The mangled case names in declaration order if the type is an enum, and
    /// empty otherwise. `throw_err` uses them as error messages.
    pub enum_cases: Vec<String>,
}

impl ErrorClass {
    /// The class's JS name.
    pub fn js_name(&self) -> String {
        mangle_resource_name(&self.name)
    }
}

js::instance_local! {
    /// Every class created by [`create_error_class`], keyed by the type it was
    /// created for. The runtime has a single global, so the classes don't need
    /// to be keyed by realm.
    static ERROR_CLASSES: RefCell<HashMap<ErrorClass, RootedHeap<js::object::Object>>> =
        RefCell::new(HashMap::new());
}

/// The address of this static is the key under which [`install_class_factory`]
/// registers the class factory.
static CLASS_FACTORY_KEY: u8 = 0;

fn class_factory_key() -> usize {
    std::ptr::from_ref(&CLASS_FACTORY_KEY).addr()
}

/// The JS source of a function that takes a base class and returns a class
/// factory. The factory takes a name and returns a subclass of the base class
/// with that name.
const CLASS_FACTORY_SOURCE: &str = "(Base) => (name) => ({ [name]: class extends Base {} })[name]";

/// Create the class factory used by [`create_error_class`], with
/// `ComponentError` as the base class, and register it as a shared object of
/// the global. Must be called after `ComponentError` is registered.
pub(crate) fn install_class_factory(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let make = js::compile::evaluate(scope, CLASS_FACTORY_SOURCE)?;
    let base = js::class::class_constructor::<ComponentErrorImpl>(scope)?;
    let factory = Function::call(scope, HandleValue::undefined(), make, &[base])?;
    let factory = Object::from_value(scope, factory).map_err(|e| e.throw(scope))?;
    js::class::get_or_init_shared_object(scope, class_factory_key(), |_| Ok(factory))?;
    Ok(())
}

/// Create the subclass of `ComponentError` for `class`, or return the one
/// created by an earlier call. The subclass's prototype has the class's JS name
/// as its `name`.
pub fn create_error_class<'s>(
    scope: &'s Scope<'_>,
    class: &ErrorClass,
) -> Result<Object<'s>, ExnThrown> {
    let existing = ERROR_CLASSES.with(|cell| cell.borrow().get(class).map(|heap| heap.get(scope)));
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let factory = js::class::get_or_init_shared_object(scope, class_factory_key(), |_| {
        panic!("the error class factory is installed when the global is set up")
    })?;
    let name = js::JSString::from_str(scope, &class.js_name())?;
    let created = Function::call(scope, HandleValue::undefined(), factory, &[name])?;
    let created = Object::from_value(scope, created).map_err(|e| e.throw(scope))?;
    let proto = created.get_property(scope, c"prototype")?;
    let proto = Object::from_value(scope, proto).map_err(|e| e.throw(scope))?;
    // Writable, configurable and not enumerable, as `Error.prototype.name` is.
    proto.define_property(scope, c"name", name, 0)?;
    ERROR_CLASSES.with(|cell| {
        cell.borrow_mut()
            .insert(class.clone(), RootedHeap::new(created))
    });
    Ok(created)
}

/// Return the subclass created by [`create_error_class`] for `class`.
///
/// # Panics
///
/// Panics if no class was created for `class`'s type.
pub fn error_class<'s>(scope: &'s Scope<'_>, class: &ErrorClass) -> Object<'s> {
    ERROR_CLASSES.with(|cell| {
        cell.borrow()
            .get(class)
            .map(|heap| heap.get(scope))
            .expect("the error classes of every type in the world are created at init")
    })
}

/// Set a new error with the payload `payload` as the pending exception.
///
/// The error is an instance of the subclass for `class`, or a `ComponentError`
/// if `class` is `None`. If `class` is an enum, the error's message is the name
/// of the case whose index is `payload`. If creating the error throws, that
/// exception is left pending instead.
pub(crate) fn throw_err(
    scope: &Scope<'_>,
    payload: HandleValue<'_>,
    class: Option<&ErrorClass>,
) -> ExnThrown {
    let error = match class {
        Some(class) => Function::construct(scope, error_class(scope, class), &[payload]),
        None => ComponentError::new(scope, payload).map(|error| *error),
    };
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
        Err(exn) => exn,
    }
}

/// Drop every error class. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    ERROR_CLASSES.with(|cell| cell.borrow_mut().clear());
}
