// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Finding and calling the JavaScript implementations of the component's
//! exports.
//!
//! The main module exports an implementation of each item of the world's
//! exports under one of the names returned by
//! [`wit_name_mapping::possible_export_names`]. [`ExportTable::build`] finds
//! the implementation of each export after the main module is evaluated.
//! [`call_export`] calls an implementation with lifted arguments, and returns
//! the result for lowering.

use std::cell::RefCell;

use js::conversion::FromJSVal;
use js::error::{throw_type_error, CapturedError, ExnThrown, ThrowException};
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use std::rc::Rc;

use wit_name_mapping::{item_js_name, mangle_name, mangle_resource_name, FunctionName, Shape};

use crate::value::{to_cstring, TypeShape};

/// The kind of an export's result type, which determines how the result is
/// handled.
#[derive(Clone, Debug, PartialEq)]
pub enum ResultShape {
    /// No result.
    None,
    /// A plain value.
    Plain,
    /// A `future`. A promise returned by an async export is lowered as the
    /// future instead of being awaited, so the call completes before the future
    /// settles.
    Future,
    /// A `result`. `ok` is set if the `ok` case has a value. `err` is the type
    /// of the `err` case's value, or `None` if that case has no value.
    Result {
        ok: bool,
        err: Option<Rc<TypeShape>>,
    },
}

impl ResultShape {
    /// Return `None` if this isn't a `result`. Otherwise, return the type of the
    /// `err` case's value, or `Some(None)` if that case has no value.
    pub fn err(&self) -> Option<Option<&TypeShape>> {
        match self {
            ResultShape::Result { err, .. } => Some(err.as_deref()),
            _ => None,
        }
    }
}

/// Specification for an export that must be resolved during initialization.
pub struct ExportSpec {
    /// Name of the WIT interface, or `None` for top-level exports.
    pub interface: Option<String>,
    /// Unqualified name of the export within the interface or top-level namespace.
    pub name: String,
    /// The shapes in which the main module may export the export's item (see
    /// [`wit_name_mapping::item_js_name`]), as returned by
    /// [`wit_name_mapping::possible_export_names`].
    pub shapes: Vec<Shape>,
    pub result: ResultShape,
    /// Whether the main module may leave the export out. Calling an export that
    /// was left out panics.
    pub optional: bool,
}

/// Description of a resolved export.
struct ExportedFunction {
    kind: ExportedFunctionKind,
    /// The callable: the function, the class for a constructor, or the name of
    /// a method. Undefined for an absent export.
    function: RootedHeap<Value>,
    /// The receiver for a plain function (the namespace or interface object) or
    /// a static (the class). `None` for the other kinds.
    this: Option<RootedHeap<js::object::Object>>,
    result: ResultShape,
}

#[derive(Clone, Copy)]
enum ExportedFunctionKind {
    Plain,
    Method,
    Static,
    Constructor,
    /// An [`optional`](ExportSpec::optional) export the main module left out.
    Absent,
}

/// The world's exported functions, including the constructors, methods and
/// static functions of exported resources, resolved after the main module is
/// evaluated. The entries are indexed by each function's position in the
/// wit-dylib export table.
pub struct ExportTable {
    entries: Vec<ExportedFunction>,
}

js::instance_local! {
    /// The table built by [`install_table`].
    static EXPORT_TABLE: RefCell<Option<ExportTable>> = const { RefCell::new(None) };
}

impl ExportTable {
    /// Resolve each of `specs` against the main module's namespace `ns`.
    ///
    /// The main module must export each spec's item in exactly one of the
    /// spec's shapes. Fails with an error message if an item is exported in more
    /// than one shape, or if an item that isn't
    /// [`optional`](ExportSpec::optional) isn't exported. The message for a
    /// missing item lists the names under which it can be exported.
    pub fn build(
        scope: &Scope<'_>,
        ns: &Object<'_>,
        specs: impl IntoIterator<Item = ExportSpec>,
    ) -> Result<ExportTable, String> {
        let mut entries = Vec::new();
        for spec in specs {
            let Some(container) = find_provider(scope, ns, &spec)? else {
                if !spec.optional {
                    return Err(missing_export_message(&spec));
                }
                entries.push(ExportedFunction {
                    kind: ExportedFunctionKind::Absent,
                    function: RootedHeap::from(js::value::undefined()),
                    this: None,
                    result: spec.result,
                });
                continue;
            };
            // The item is there, so a missing member is a missing class member.
            let resolved =
                resolve_member(scope, &container, &spec.name).map_err(|error| match error {
                    ResolveError::Missing(_) => missing_export_message(&spec),
                    ResolveError::Threw(_) => {
                        let message = ExnThrown::capture(scope).message.unwrap_or_default();
                        format!(
                            "resolving the WIT export `{}` threw: {message}",
                            wit_name(&spec)
                        )
                    }
                })?;
            let entry =
                |kind, function: HandleValue<'_>, this: Option<Object<'_>>| ExportedFunction {
                    kind,
                    function: RootedHeap::from(function.get()),
                    this: this.map(RootedHeap::new),
                    result: spec.result,
                };
            entries.push(match resolved {
                Resolved::Plain { function, this } => {
                    entry(ExportedFunctionKind::Plain, function, Some(this))
                }
                Resolved::Method { name } => entry(ExportedFunctionKind::Method, name, None),
                Resolved::Static { function, class } => {
                    entry(ExportedFunctionKind::Static, function, Some(class))
                }
                Resolved::Constructor { class } => {
                    entry(ExportedFunctionKind::Constructor, class, None)
                }
            });
        }
        Ok(ExportTable { entries })
    }

    /// The resolved callable for export `index`, rooted on `scope`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is past the end of the table, or names an
    /// [`optional`](ExportSpec::optional) export the main module left out.
    pub fn resolved<'s>(&self, scope: &'s Scope<'_>, index: usize) -> Resolved<'s> {
        let entry = &self.entries[index];
        let function = entry.function.get(scope);
        let this = || {
            entry
                .this
                .as_ref()
                .expect("a plain or static export has a receiver")
                .get(scope)
        };
        match entry.kind {
            ExportedFunctionKind::Plain => Resolved::Plain {
                function,
                this: this(),
            },
            ExportedFunctionKind::Method => Resolved::Method { name: function },
            ExportedFunctionKind::Static => Resolved::Static {
                function,
                class: this(),
            },
            ExportedFunctionKind::Constructor => Resolved::Constructor { class: function },
            ExportedFunctionKind::Absent => {
                panic!("export {index} was left out by the main module")
            }
        }
    }

    /// Whether the main module provides export `index`. Only an
    /// [`optional`](ExportSpec::optional) export can be missing.
    ///
    /// # Panics
    ///
    /// Panics if `index` is past the end of the table.
    pub fn provided(&self, index: usize) -> bool {
        !matches!(self.entries[index].kind, ExportedFunctionKind::Absent)
    }

    /// The result shape of export `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is past the end of the table.
    pub fn result(&self, index: usize) -> ResultShape {
        self.entries[index].result.clone()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build the table for `specs` and install it for [`with_export_table`].
///
/// Returns the error message of [`ExportTable::build`] on failure.
pub fn install_table(
    scope: &Scope<'_>,
    ns: &Object<'_>,
    specs: impl IntoIterator<Item = ExportSpec>,
) -> Result<(), String> {
    let table = ExportTable::build(scope, ns, specs)?;
    EXPORT_TABLE.with(|cell| *cell.borrow_mut() = Some(table));
    Ok(())
}

/// Run `f` with the installed table.
///
/// # Panics
///
/// Panics if [`install_table`] has not run.
pub fn with_export_table<R>(f: impl FnOnce(&ExportTable) -> R) -> R {
    EXPORT_TABLE.with(|cell| {
        f(cell
            .borrow()
            .as_ref()
            .expect("the export table is installed at init"))
    })
}

/// Drop the installed table. See [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    EXPORT_TABLE.with(|cell| *cell.borrow_mut() = None);
}

/// The WIT name of `spec`'s export, such as `ns:pkg/iface#name`.
fn wit_name(spec: &ExportSpec) -> String {
    wit_name_mapping::function_wit_name(spec.interface.as_deref(), &spec.name)
}

/// Return the object that has `spec`'s item as a property, or `None` if no
/// shape has it. Fails if more than one shape has the item, or if reading a
/// property throws.
fn find_provider<'s>(
    scope: &'s Scope<'_>,
    ns: &Object<'s>,
    spec: &ExportSpec,
) -> Result<Option<Object<'s>>, String> {
    let item = item_js_name(&spec.name);
    let key = to_cstring(&item);
    let mut found: Option<(&Shape, Object<'s>)> = None;
    for shape in &spec.shapes {
        let path = shape.path(&item);
        let read_failed = |_| {
            let message = ExnThrown::capture(scope).message.unwrap_or_default();
            format!(
                "reading `{path}` for the WIT export `{}` threw: {message}",
                wit_name(spec)
            )
        };
        let Some(container) = shape_container(scope, ns, shape).map_err(read_failed)? else {
            continue;
        };
        if container
            .get_property_by_name(scope, &key)
            .map_err(read_failed)?
            .is_undefined()
        {
            continue;
        }
        if let Some((first, _)) = &found {
            return Err(format!(
                "the main module provides the WIT export `{}` twice, as `{}` and as `{path}`. \
                 Export it in one shape only",
                wit_name(spec),
                first.path(&item)
            ));
        }
        found = Some((shape, container));
    }
    Ok(found.map(|(_, container)| container))
}

/// Return the object that has the items of `shape` as properties: `ns` for
/// [`Shape::Bare`], or an object nested in `ns` for the other shapes. Returns
/// `None` if a nested object is missing or isn't an object.
fn shape_container<'s>(
    scope: &'s Scope<'_>,
    ns: &Object<'s>,
    shape: &Shape,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let member = |object: &Object<'s>, name: &str| -> Result<Option<Object<'s>>, ExnThrown> {
        let value = object.get_property_by_name(scope, &to_cstring(name))?;
        Ok(Object::from_value(scope, value).ok())
    };
    match shape {
        Shape::Bare => Ok(Some(*ns)),
        Shape::Interface(interface) | Shape::Versioned(interface) => member(ns, interface),
        Shape::Package { package, interface } => match member(ns, package)? {
            Some(package) => member(&package, interface),
            None => Ok(None),
        },
    }
}

/// Return the error message for an export that the main module doesn't
/// provide. It names the missing item, lists the names under which it can be
/// exported, and gives an example export statement.
fn missing_export_message(spec: &ExportSpec) -> String {
    let item = item_js_name(&spec.name);
    // A description of the missing item and, for a resource, an example class
    // body.
    let (member, class_body) = match FunctionName::parse(&spec.name) {
        FunctionName::Constructor { .. } => (
            format!("a class `{item}` with a constructor"),
            Some("constructor(...) { ... }".to_string()),
        ),
        FunctionName::Method { name, .. } => {
            let method = mangle_name(name);
            (
                format!("a class `{item}` with a method `{method}`"),
                Some(format!("{method}(...) {{ ... }}")),
            )
        }
        FunctionName::Static { name, .. } => {
            let method = mangle_name(name);
            (
                format!("a class `{item}` with a static method `{method}`"),
                Some(format!("static {method}(...) {{ ... }}")),
            )
        }
        FunctionName::Plain(_) => (format!("a function `{item}`"), None),
    };
    // The item as a member of an object literal.
    let in_object = match &class_body {
        Some(body) => format!("{item}: class {{ {body} }}"),
        None => format!("{item}(...) {{ ... }}"),
    };
    let statement = match spec.shapes.last() {
        Some(Shape::Package { package, interface }) => {
            format!("export const {package} = {{ {interface}: {{ {in_object} }} }};")
        }
        Some(Shape::Interface(interface)) => {
            format!("export const {interface} = {{ {in_object} }};")
        }
        Some(Shape::Versioned(wit_name)) => {
            format!("const impl = {{ {in_object} }}; export {{ impl as \"{wit_name}\" }};")
        }
        Some(Shape::Bare) | None => match &class_body {
            Some(body) => format!("export class {item} {{ {body} }}"),
            None => format!("export function {item}(...) {{ ... }}"),
        },
    };
    let paths: Vec<String> = spec
        .shapes
        .iter()
        .map(|shape| format!("`{}`", shape.path(&item)))
        .collect();
    format!(
        "the main module does not provide {member} for the WIT export `{}`. Export it as {}, \
         for example: {statement}",
        wit_name(spec),
        paths.join(" or ")
    )
}

/// A resolved export: the JavaScript function, and the values needed to call
/// it.
pub enum Resolved<'s> {
    /// A plain exported function, invoked with the namespace object as `this`.
    Plain {
        function: HandleValue<'s>,
        this: Object<'s>,
    },
    /// An instance method (`[method]R.m`), which `R.prototype` must have. `name`
    /// is the method's JS name. The method is looked up on the receiver, which
    /// is the first argument, at each call, so an override in a subclass is
    /// called.
    Method { name: HandleValue<'s> },
    /// A static method (`[static]R.m`), invoked with the constructor as `this`.
    Static {
        function: HandleValue<'s>,
        class: Object<'s>,
    },
    /// A resource constructor (`[constructor]R`), invoked via `new`.
    Constructor { class: HandleValue<'s> },
}

/// The result of an export call.
pub enum Outcome<'s> {
    /// The call returned normally, with the return value.
    Returned(HandleValue<'s>),
    /// The call threw or its promise rejected, and the export's result type is
    /// a `result`. Holds the `err` value converted from the exception.
    Threw(HandleValue<'s>),
}

/// The method named `name` on `receiver`, which must be a function.
fn method_on<'s>(
    scope: &'s Scope<'_>,
    receiver: HandleValue<'_>,
    name: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let receiver = Object::from_value(scope, receiver)
        .map_err(|_| throw_type_error(scope, c"a resource method's receiver is not an object"))?;
    let id = scope.root_id(js::id::value_to_id(scope, name)?);
    let method = receiver.get_property_by_id(scope, id)?;
    if !js::function::is_callable(method) {
        let name = String::from_jsval_throwing(scope, name, ())?;
        return Err(js::error::TypeError(format!(
            "the resource's `{name}` method is not a function"
        ))
        .throw(scope));
    }
    Ok(method)
}

/// Resolve the export `name` to a JavaScript function.
///
/// `container` is the object that has the export's item as a property: the
/// main module's namespace, or an object nested in it. `name` is the WIT export
/// name, which can start with `[constructor]`, `[method]` or `[static]`.
///
/// Throws a `TypeError` if the function, class or method is missing. An
/// exception thrown by a getter is left pending.
pub fn resolve_export<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    name: &str,
) -> Result<Resolved<'s>, ExnThrown> {
    resolve_member(scope, container, name).map_err(|error| match error {
        ResolveError::Missing(message) => throw_type_error(scope, message),
        ResolveError::Threw(exn) => exn,
    })
}

/// Why [`resolve_member`] failed.
enum ResolveError {
    /// The export is missing. The string describes what is missing.
    Missing(&'static std::ffi::CStr),
    /// Reading the export threw. The exception is pending.
    Threw(ExnThrown),
}

impl From<ExnThrown> for ResolveError {
    fn from(exn: ExnThrown) -> Self {
        ResolveError::Threw(exn)
    }
}

/// Like [`resolve_export`], but reports a missing export instead of throwing.
fn resolve_member<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    name: &str,
) -> Result<Resolved<'s>, ResolveError> {
    let container = *container;

    match FunctionName::parse(name) {
        FunctionName::Constructor { resource } => {
            let class = lookup_class(scope, &container, resource)?;
            Ok(Resolved::Constructor { class })
        }
        FunctionName::Method { resource, name } => {
            let class_obj = lookup_class_object(scope, &container, resource)?;
            let proto = class_obj.get_property(scope, c"prototype")?;
            let proto_obj = Object::from_value(scope, proto)
                .map_err(|_| ResolveError::Missing(c"resource class has no prototype object"))?;
            let member = mangle_name(name);
            lookup_member(scope, &proto_obj, &member)?;
            let name = js::JSString::from_str(scope, &member)?;
            Ok(Resolved::Method {
                name: scope.root_value(name.as_value()),
            })
        }
        FunctionName::Static { resource, name } => {
            let class_obj = lookup_class_object(scope, &container, resource)?;
            let function = lookup_member(scope, &class_obj, &mangle_name(name))?;
            Ok(Resolved::Static {
                function,
                class: class_obj,
            })
        }
        FunctionName::Plain(name) => {
            let function = lookup_member(scope, &container, &mangle_name(name))?;
            Ok(Resolved::Plain {
                function,
                this: container,
            })
        }
    }
}

/// Call a resolved export.
///
/// `args` are the lifted arguments. For [`Resolved::Method`], `args[0]` is the
/// receiver, passed as `this`, and `args[1..]` are the method arguments. For
/// the other variants, `args` are the arguments in order.
///
/// If the call throws and `result` is a `result`, the exception is converted
/// into an `err` value (see `component_error::err_payload`), which is returned
/// as [`Outcome::Threw`]. Otherwise, `Err` holds the captured exception. If the
/// exception can't be converted, its message includes the reason.
///
/// A returned promise is marked as handled, so that its rejection isn't
/// reported as unhandled.
pub fn call_export<'s>(
    scope: &'s Scope<'_>,
    resolved: Resolved<'s>,
    args: &[HandleValue<'s>],
    result: &ResultShape,
) -> Result<Outcome<'s>, CapturedError> {
    let call_result: Result<HandleValue<'s>, ExnThrown> = match resolved {
        Resolved::Plain { function, this } => Function::call(scope, this, function, args),
        Resolved::Static { function, class } => Function::call(scope, class, function, args),
        Resolved::Method { name } => {
            let this = args.first().copied().unwrap_or(HandleValue::undefined());
            let rest = args.get(1..).unwrap_or(&[]);
            method_on(scope, this, name)
                .and_then(|function| Function::call(scope, this, function, rest))
        }
        Resolved::Constructor { class } => {
            Function::construct(scope, class, args).map(|obj| scope.root_value(obj.as_value()))
        }
    };

    match call_result {
        Ok(value) => {
            if let Ok(promise) = js::Promise::from_jsval(scope, value, ()) {
                promise
                    .set_any_is_handled(scope)
                    .map_err(|_| ExnThrown::capture(scope))?;
            }
            Ok(Outcome::Returned(value))
        }
        Err(ExnThrown) => {
            let pending = js::exception::get_pending(scope);
            // `err_payload` requires that no exception is pending, so the
            // exception is captured first. The capture keeps the stack and error
            // report for a trap.
            let mut captured = ExnThrown::capture(scope);
            if let (Some(err), Some(pending)) = (result.err(), pending) {
                match crate::component_error::err_payload(scope, pending, err) {
                    Ok(payload) => return Ok(Outcome::Threw(payload)),
                    Err(why) => {
                        let message = captured.message.take();
                        let message = message.as_deref().unwrap_or("JavaScript exception");
                        captured.message = Some(format!(
                            "{message} (not converted into an `err` value: {why})"
                        ));
                    }
                }
            }
            Err(captured)
        }
    }
}

/// Convert the settled promise returned by an async export into an
/// [`Outcome`]. A rejection is handled like an exception thrown in
/// [`call_export`].
///
/// Returns `Err` with an error message if the export's result type isn't a
/// `result`, or if the rejection reason can't be converted into an `err` value.
/// The message includes the reason's message and stack.
///
/// # Panics
///
/// Panics if `promise` is pending.
pub fn settled_promise_outcome<'s>(
    scope: &'s Scope<'_>,
    promise: &js::Promise<'s>,
    result_shape: &ResultShape,
) -> Result<Outcome<'s>, String> {
    let result = promise
        .result(scope)
        .expect("settled promise has a result value");
    if !promise.is_rejected() {
        return Ok(Outcome::Returned(result));
    }
    if let Some(err) = result_shape.err() {
        return match crate::component_error::err_payload(scope, result, err) {
            Ok(payload) => Ok(Outcome::Threw(payload)),
            Err(why) => Err(rejection_message(
                scope,
                result,
                &format!(
                    "async export rejected with a reason that can't be converted into its \
                     `err` value ({why})"
                ),
            )),
        };
    }
    Err(rejection_message(
        scope,
        result,
        "non-`result` async export rejected",
    ))
}

/// Formats `context`, followed by the message, location and stack of the rejection `reason`.
pub(crate) fn rejection_message(
    scope: &Scope<'_>,
    reason: HandleValue<'_>,
    context: &str,
) -> String {
    capture_value(scope, reason).with_context(context)
}

/// Captures `value` as an uncaught exception, with the message and stack that
/// an uncaught exception reports.
pub(crate) fn capture_value(scope: &Scope<'_>, value: HandleValue<'_>) -> CapturedError {
    js::exception::set_pending(
        scope,
        value,
        js::native::ExceptionStackBehavior::DoNotCapture,
    );
    ExnThrown::capture(scope)
}

/// Whether `value` is a settled promise or isn't a promise.
#[cfg(target_arch = "wasm32")]
pub(crate) fn promise_is_settled(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    match js::Promise::from_jsval(scope, value, ()) {
        Ok(promise) => !promise.is_pending(),
        Err(_) => true,
    }
}

/// Look up the class of the resource `ty` on `container`. Fails if the
/// property is `undefined`.
fn lookup_class<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    ty: &str,
) -> Result<HandleValue<'s>, ResolveError> {
    let key = to_cstring(&mangle_resource_name(ty));
    let value = container.get_property_by_name(scope, &key)?;
    if value.is_undefined() {
        return Err(ResolveError::Missing(
            c"exported resource class is not defined",
        ));
    }
    Ok(value)
}

/// Like [`lookup_class`], but also fails if the class isn't an object.
fn lookup_class_object<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    ty: &str,
) -> Result<Object<'s>, ResolveError> {
    let class = lookup_class(scope, container, ty)?;
    Object::from_value(scope, class)
        .map_err(|_| ResolveError::Missing(c"resource class is not an object"))
}

/// Look up the property `mangled` of `container`. Fails if it is `undefined`.
fn lookup_member<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    mangled: &str,
) -> Result<HandleValue<'s>, ResolveError> {
    let key = to_cstring(mangled);
    let value = container.get_property_by_name(scope, &key)?;
    if value.is_undefined() {
        return Err(ResolveError::Missing(c"exported member is not defined"));
    }
    Ok(value)
}
