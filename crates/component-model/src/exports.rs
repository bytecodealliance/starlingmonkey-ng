// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Export resolution and dispatch.
//!
//! A componentized application exposes its WIT exports as JavaScript values
//! reachable from the main module's namespace object, in the shapes
//! [`crate::naming`] allows. This module finds the one shape that provides each
//! export, turns the export's WIT-level name into a concrete JavaScript
//! callable ([`resolve_export`]), then invokes it with the lowered argument
//! values and lifts the JavaScript result back out ([`call_export`]).
//!
//! Names are mangled with the same rules as everything else: resource types
//! upper-camel-case to their class name ([`mangle_resource_name`]), members and
//! plain functions lower-camel-case ([`mangle_name`]).

use std::cell::RefCell;

use js::conversion::FromJSVal;
use js::error::{throw_type_error, CapturedError, ExnThrown, ThrowException};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::{JSTracer, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use std::rc::Rc;

use crate::naming::Shape;
use crate::value::{mangle_name, mangle_resource_name, to_cstring, TypeShape};

/// How an export's WIT result lowers, which is all the result handling needs
/// from the result type.
#[derive(Clone, Debug, PartialEq)]
pub enum ResultShape {
    /// A `()` result: nothing is lowered.
    None,
    /// A plain value.
    Plain,
    /// A `future<T>`. An async export's returned promise is lowered as the future
    /// itself rather than awaited, so the call completes before the future
    /// settles.
    Future,
    /// A `result<ok, err>`, with whether the `ok` arm has a payload and the type of
    /// the `err` arm's payload, if it has one.
    Result {
        ok: bool,
        err: Option<Rc<TypeShape>>,
    },
}

impl ResultShape {
    pub fn is_result(&self) -> bool {
        matches!(self, ResultShape::Result { .. })
    }

    /// For a `result`, the type of its `err` arm's payload, which is `None` for
    /// an arm without one. `None` for any other result.
    pub fn err(&self) -> Option<Option<&TypeShape>> {
        match self {
            ResultShape::Result { err, .. } => Some(err.as_deref()),
            _ => None,
        }
    }
}

/// One export to resolve at `init`: its WIT interface and name, the shapes the
/// main module may provide it in, and its result shape.
pub struct ExportSpec {
    pub interface: Option<String>,
    pub name: String,
    /// The shapes that may provide the export's item (see [`export_item`]), in
    /// the order [`crate::naming::plan`] gives them.
    pub shapes: Vec<Shape>,
    pub result: ResultShape,
    /// Whether the main module may leave the export out. An export left out
    /// stays unresolved, and calling it panics.
    pub optional: bool,
}

/// The JS name of the item an export belongs to: the function's name, or the
/// class name for a `[constructor]`, `[method]` or `[static]` export.
pub fn export_item(name: &str) -> String {
    let resource = name
        .strip_prefix("[constructor]")
        .or_else(|| name.strip_prefix("[method]"))
        .or_else(|| name.strip_prefix("[static]"))
        .map(|rest| rest.split_once('.').map_or(rest, |(ty, _)| ty));
    match resource {
        Some(ty) => mangle_resource_name(ty),
        None => mangle_name(name),
    }
}

/// One resolved export, kept across calls.
#[js::allow_unrooted_interior]
struct ExportEntry {
    kind: EntryKind,
    /// The callable: the function, the class for a constructor, or the name of
    /// a method.
    function: Heap<Value>,
    /// The receiver for a plain function (the namespace or interface object) or
    /// a static (the class). Undefined for the other kinds.
    this: Heap<Value>,
    result: ResultShape,
}

#[derive(Clone, Copy)]
enum EntryKind {
    Plain,
    Method,
    Static,
    Constructor,
    /// An [`optional`](ExportSpec::optional) export the main module left out.
    Absent,
}

/// Every export of the world, resolved once after the main module evaluated
/// and indexed by the export's position in the wit-dylib export table.
#[js::allow_unrooted_interior]
pub struct ExportTable {
    entries: Vec<ExportEntry>,
}

js::instance_local! {
    /// The table [`install_table`] built, traced by [`trace_export_table`].
    static EXPORT_TABLE: RefCell<Option<ExportTable>> = const { RefCell::new(None) };
}

impl ExportTable {
    /// Resolve every spec against `ns`, the main module's namespace, in order.
    ///
    /// Each spec's item must be provided in exactly one of its shapes. A spec
    /// whose item no shape provides fails with a message naming the JS exports
    /// that would, unless the spec is [`optional`](ExportSpec::optional). A spec
    /// whose item more than one shape provides fails as well.
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
                entries.push(ExportEntry {
                    kind: EntryKind::Absent,
                    function: Heap::default(),
                    this: Heap::default(),
                    result: spec.result,
                });
                continue;
            };
            // The item is there, so a failure is a missing class member.
            let resolved = resolve_export(scope, &container, &spec.name).map_err(|_| {
                js::exception::clear(scope);
                missing_export_message(&spec)
            })?;
            let (kind, function, this) = match resolved {
                Resolved::Plain { function, this } => {
                    (EntryKind::Plain, function.get(), this.as_value())
                }
                Resolved::Method { name } => {
                    (EntryKind::Method, name.get(), js::value::undefined())
                }
                Resolved::Static { function, class } => {
                    (EntryKind::Static, function.get(), class.as_value())
                }
                Resolved::Constructor { class } => {
                    (EntryKind::Constructor, class.get(), js::value::undefined())
                }
            };
            entries.push(ExportEntry {
                kind,
                function: Heap::from(function),
                this: Heap::from(this),
                result: spec.result,
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
        match entry.kind {
            EntryKind::Plain => Resolved::Plain {
                function,
                this: Object::from_value(scope, &entry.this)
                    .expect("a plain export's receiver is an object"),
            },
            EntryKind::Method => Resolved::Method { name: function },
            EntryKind::Static => Resolved::Static {
                function,
                class: Object::from_value(scope, &entry.this)
                    .expect("a static export's class is an object"),
            },
            EntryKind::Constructor => Resolved::Constructor { class: function },
            EntryKind::Absent => panic!("export {index} was left out by the main module"),
        }
    }

    /// Whether the main module provides export `index`, which only an
    /// [`optional`](ExportSpec::optional) export may not.
    ///
    /// # Panics
    ///
    /// Panics if `index` is past the end of the table.
    pub fn provided(&self, index: usize) -> bool {
        !matches!(self.entries[index].kind, EntryKind::Absent)
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
/// Returns the build error unchanged, so a missing export fails the caller with
/// the message naming what the main module must export.
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

/// Trace the installed table's callables and receivers.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_export_table(trc: *mut JSTracer) {
    EXPORT_TABLE.with(|cell| {
        if let Some(table) = &*cell.as_ptr() {
            for entry in &table.entries {
                entry.function.trace(trc);
                entry.this.trace(trc);
            }
        }
    });
}

/// The WIT name of `spec`'s export, such as `ns:pkg/iface#name`.
fn wit_name(spec: &ExportSpec) -> String {
    match &spec.interface {
        Some(interface) => format!("{interface}#{}", spec.name),
        None => spec.name.clone(),
    }
}

/// The object that provides `spec`'s item in the one shape that does, or
/// `None` if no shape does. Fails when more than one shape provides it, or
/// reading a candidate throws.
fn find_provider<'s>(
    scope: &'s Scope<'_>,
    ns: &Object<'s>,
    spec: &ExportSpec,
) -> Result<Option<Object<'s>>, String> {
    let item = export_item(&spec.name);
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
            .get_property(scope, &key)
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

/// The object whose members are the items `shape` provides: `ns` for a bare
/// export, or the layer objects below it. `None` if a layer is missing or not
/// an object.
fn shape_container<'s>(
    scope: &'s Scope<'_>,
    ns: &Object<'s>,
    shape: &Shape,
) -> Result<Option<Object<'s>>, ExnThrown> {
    let member = |object: &Object<'s>, name: &str| -> Result<Option<Object<'s>>, ExnThrown> {
        let value = object.get_property(scope, &to_cstring(name))?;
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

/// The message for an export the main module does not provide: which JS name
/// is missing, the paths that would provide it, and an export statement that
/// does.
fn missing_export_message(spec: &ExportSpec) -> String {
    let item = export_item(&spec.name);
    let member_of = |rest: &str| mangle_name(rest.split_once('.').map_or("", |(_, name)| name));
    // What is missing, and for a resource, the body of a class that provides it.
    let (member, class_body) = if spec.name.starts_with("[constructor]") {
        (
            format!("a class `{item}` with a constructor"),
            Some("constructor(...) { ... }".to_string()),
        )
    } else if let Some(rest) = spec.name.strip_prefix("[method]") {
        let method = member_of(rest);
        (
            format!("a class `{item}` with a method `{method}`"),
            Some(format!("{method}(...) {{ ... }}")),
        )
    } else if let Some(rest) = spec.name.strip_prefix("[static]") {
        let method = member_of(rest);
        (
            format!("a class `{item}` with a static method `{method}`"),
            Some(format!("static {method}(...) {{ ... }}")),
        )
    } else {
        (format!("a function `{item}`"), None)
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

/// A resolved export: a JavaScript callable plus enough context to invoke it. All
/// variants hold handles rooted on the scope [`resolve_export`] and
/// [`call_export`] share.
pub enum Resolved<'s> {
    /// A plain exported function, invoked with the namespace object as `this`.
    Plain {
        function: HandleValue<'s>,
        this: Object<'s>,
    },
    /// An instance method (`[method]R.m`), which `R.prototype` must have. `name`
    /// is the method's JavaScript name as a string, looked up on the receiver at
    /// call time, so a subclass's override applies. The receiver is supplied at
    /// call time as the first argument.
    Method { name: HandleValue<'s> },
    /// A static method (`[static]R.m`), invoked with the constructor as `this`.
    Static {
        function: HandleValue<'s>,
        class: Object<'s>,
    },
    /// A resource constructor (`[constructor]R`), invoked via `new`.
    Constructor { class: HandleValue<'s> },
}

/// The result of dispatching an export call, rooted on the scope [`call_export`]
/// and the lowering step share.
pub enum Outcome<'s> {
    /// The call returned normally, with the return value.
    Returned(HandleValue<'s>),
    /// The call threw and the export's declared result is a `result<_, _>`, so
    /// the thrown value becomes the `err` payload rather than a trap.
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
    if !Object::from_value(scope, method).is_ok_and(|method| method.is_callable()) {
        let name = String::from_jsval_throwing(scope, name, ())?;
        return Err(js::error::TypeError(format!(
            "the resource's `{name}` method is not a function"
        ))
        .throw(scope));
    }
    Ok(method)
}

/// Resolve an export name to a concrete JavaScript callable.
///
/// `container` is the object holding the export's item: the main module's
/// namespace, or a layer object below it. `name` is the WIT-level export name,
/// which may carry a `[constructor]`/`[method]`/`[static]` prefix.
///
/// Returns `Err(ExnThrown)` with a pending `TypeError` if the named member is
/// missing or, for the prefixed forms, the named class is missing.
pub fn resolve_export<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    name: &str,
) -> Result<Resolved<'s>, ExnThrown> {
    let container = *container;

    if let Some(ty) = name.strip_prefix("[constructor]") {
        let class = lookup_class(scope, &container, ty)?;
        Ok(Resolved::Constructor { class })
    } else if let Some(rest) = name.strip_prefix("[method]") {
        let (ty, method) = split_resource_member(scope, rest)?;
        let class_obj = lookup_class_object(scope, &container, ty)?;
        let proto = class_obj.get_property(scope, c"prototype")?;
        let proto_obj = Object::from_value(scope, proto)
            .map_err(|_| throw_type_error(scope, c"resource class has no prototype object"))?;
        let member = mangle_name(method);
        lookup_member(scope, &proto_obj, &member)?;
        let name = js::JSString::from_str(scope, &member)?;
        Ok(Resolved::Method {
            name: scope.root_value(name.as_value()),
        })
    } else if let Some(rest) = name.strip_prefix("[static]") {
        let (ty, method) = split_resource_member(scope, rest)?;
        let class_obj = lookup_class_object(scope, &container, ty)?;
        let function = lookup_member(scope, &class_obj, &mangle_name(method))?;
        Ok(Resolved::Static {
            function,
            class: class_obj,
        })
    } else {
        let function = lookup_member(scope, &container, &mangle_name(name))?;
        Ok(Resolved::Plain {
            function,
            this: container,
        })
    }
}

/// Dispatch a resolved export call.
///
/// `args` are the already-lowered argument values. For [`Resolved::Method`],
/// `args[0]` is the receiver, passed as `this`, and `args[1..]` are the method
/// arguments. For every other form, `args` are the call arguments in order.
///
/// `result` is the export's declared WIT result. It controls exception
/// handling: for a `result<_, _>`, a JS exception that converts to an `err`
/// payload (see [`err_payload`](crate::component_error::err_payload)) becomes an
/// [`Outcome::Threw`] holding it. Any other exception is a trap, surfaced as
/// `Err(CapturedError)` with the thrown message.
///
/// A returned promise is marked as handled, since its rejection becomes the
/// export's `err` or a trap instead of an unhandled rejection.
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
            if let Some(promise) = as_promise(scope, value) {
                promise
                    .set_any_is_handled(scope)
                    .map_err(|_| ExnThrown::capture(scope))?;
            }
            Ok(Outcome::Returned(value))
        }
        Err(ExnThrown) => {
            let pending = js::exception::get_pending(scope);
            // Captured before `err_payload` runs, since it reads the exception's
            // properties, which requires that no exception is pending. The capture
            // keeps the exception's stack and error report for the trap.
            let captured = ExnThrown::capture(scope);
            if let (Some(err), Some(pending)) = (result.err(), pending) {
                if let Some(payload) = crate::component_error::err_payload(scope, pending, err) {
                    return Ok(Outcome::Threw(payload));
                }
            }
            Err(captured)
        }
    }
}

/// Turn the settled promise an async export returned into an [`Outcome`],
/// applying the same rules as [`call_export`]'s synchronous catch branch to the
/// promise's reason.
///
/// Returns `Err` with a human-readable message on a rejection that cannot be
/// absorbed: a non-`result` export, or a `result` export rejected with a reason
/// that converts to no `err` payload. The message includes the rejection reason's
/// message and stack. The caller traps the component call with it.
pub fn settled_promise_outcome<'s>(
    scope: &'s Scope<'_>,
    promise: &js::Promise<'s>,
    result_shape: &ResultShape,
) -> Result<Outcome<'s>, String> {
    // Callers trap on a still-pending promise before reaching here.
    let result = promise
        .result(scope)
        .expect("settled promise has a result value");
    if !promise.is_rejected() {
        return Ok(Outcome::Returned(result));
    }
    // Only a `result<_, _>`-typed export absorbs the rejection as its `err`
    // payload, and only when the reason converts to one.
    if let Some(err) = result_shape.err() {
        if let Some(payload) = crate::component_error::err_payload(scope, result, err) {
            return Ok(Outcome::Threw(payload));
        }
        return Err(rejection_message(
            scope,
            result,
            "async export rejected with a reason its error type cannot hold",
        ));
    }
    Err(rejection_message(
        scope,
        result,
        "non-`result` async export rejected",
    ))
}

/// `context`, followed by the message, location and stack of the rejection `reason`.
pub(crate) fn rejection_message(
    scope: &Scope<'_>,
    reason: HandleValue<'_>,
    context: &str,
) -> String {
    js::exception::set_pending(
        scope,
        reason,
        js::native::ExceptionStackBehavior::DoNotCapture,
    );
    ExnThrown::capture(scope).with_context(context)
}

/// Cast a JS value to a [`Promise`](js::Promise), or `None` if it is not one.
pub(crate) fn as_promise<'s>(
    scope: &'s Scope<'_>,
    value: impl js::conversion::ToJSVal<'s>,
) -> Option<js::Promise<'s>> {
    Object::from_value(scope, value)
        .ok()
        .and_then(|obj| obj.cast::<js::Promise>().ok())
}

/// Whether the value an async-shaped call returned has settled, so the drive loop
/// can stop. A non-promise value, as a plain function returns, counts as settled.
///
/// Only the async export drivers need this, and they exist only on wasm.
#[cfg(target_arch = "wasm32")]
pub(crate) fn promise_is_settled(scope: &Scope<'_>, value: Value) -> bool {
    match as_promise(scope, value) {
        Some(promise) => !promise.is_pending(),
        None => true,
    }
}

/// Look up a resource class on `container` by its upper-camel-cased name,
/// erroring if the member is missing or not an object/function.
fn lookup_class<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    ty: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let key = to_cstring(&mangle_resource_name(ty));
    let value = container.get_property(scope, &key)?;
    if value.is_undefined() {
        return Err(throw_type_error(
            scope,
            c"exported resource class is not defined",
        ));
    }
    Ok(value)
}

/// Look up a resource class as [`lookup_class`] does, also erroring if it is not an object.
fn lookup_class_object<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    ty: &str,
) -> Result<Object<'s>, ExnThrown> {
    let class = lookup_class(scope, container, ty)?;
    Object::from_value(scope, class)
        .map_err(|_| throw_type_error(scope, c"resource class is not an object"))
}

/// Look up a member function on `container` by its already-mangled name,
/// erroring if it is missing.
fn lookup_member<'s>(
    scope: &'s Scope<'_>,
    container: &Object<'s>,
    mangled: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let key = to_cstring(mangled);
    let value = container.get_property(scope, &key)?;
    if value.is_undefined() {
        return Err(throw_type_error(scope, c"exported member is not defined"));
    }
    Ok(value)
}

/// Split a `[method]`/`[static]` payload `TYPE.MEMBER` into its parts, erroring
/// on a malformed name (no `.`).
fn split_resource_member<'a>(
    scope: &Scope<'_>,
    rest: &'a str,
) -> Result<(&'a str, &'a str), ExnThrown> {
    rest.split_once('.')
        .ok_or_else(|| throw_type_error(scope, c"resource member name must be `type.member`"))
}
