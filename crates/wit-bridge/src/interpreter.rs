// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The implementation of `wit_dylib_ffi`'s [`Interpreter`] and [`Call`] traits.
//!
//! The `Call` methods call the conversion functions in [`crate::value`], with
//! [`TypeShape`]s created from the `wit_dylib_ffi` type metadata. This module
//! compiles only for wasm, because the metadata exists only in the generated
//! bindings.
//!
//! The `Call` methods receive no [`Scope`], so each creates one with
//! [`init::with_scope`] for its conversion. The values are kept on the
//! [`CallStack`], which is traced by the GC, so they outlive these scopes.
//!
//! The `Call` methods can't report errors, and the generated code calls the
//! next method regardless of failures, so a conversion error traps. Import
//! arguments are checked before the call, so that an argument that can't be
//! lowered throws a `TypeError` instead (see [`crate::validate`]). A trap while
//! lowering an export's result describes the part of the result that failed.

#![cfg(target_arch = "wasm32")]

use std::alloc::Layout;
use std::cell::RefCell;
use std::ffi::CStr;
use std::rc::Rc;

use crate::component_error::ErrorClass;
use crate::exports::{ExportSpec, ResultShape};

use js::conversion::FromJSVal;
use js::error::{throw_type_error, CapturedError, ExnThrown};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use wit_dylib_ffi::{
    Call, Enum, ExportFunction, Flags, Future, Interpreter, List, Map, Record, Resource, Stream,
    Tuple, Type, Variant, Wit, WitOption, WitResult,
};

use wit_name_mapping::{mangle_name, mangle_resource_name, FunctionName, InterfaceNames};

use crate::imports::{
    self, EnumDesc, ImportFuncDesc, ImportInvoker, InterfaceDesc, ResourceTypeDesc, WorldDesc,
};
use crate::init;
use crate::resources::{self, ResourceDesc};
use crate::stack::CallStack;
use crate::value::{self, TypeShape, TypeShapeValue};

// ===========================================================================
// The world's WIT metadata, stashed by `initialize`
// ===========================================================================

js::instance_local! {
    /// Set by [`StarlingInterpreter::initialize`]. `Wit` is a `Copy` wrapper over
    /// a `&'static` FFI struct, so storing it by value is sound.
    static WIT: RefCell<Option<Wit>> = const { RefCell::new(None) };

    /// The description of the world's imports, built by
    /// [`StarlingInterpreter::initialize`].
    static WORLD: RefCell<Option<WorldDesc>> = const { RefCell::new(None) };

    /// Mangled field names per record type index, built on first use.
    static RECORD_NAMES: RefCell<Vec<Option<Rc<[&'static CStr]>>>> =
        const { RefCell::new(Vec::new()) };

    /// Mangled case names, paired with whether the case has a payload, per
    /// variant type index, built on first use.
    static VARIANT_CASES: RefCell<Vec<Option<Rc<[(&'static CStr, bool)]>>>> =
        const { RefCell::new(Vec::new()) };
}

/// Return the world's metadata.
///
/// # Panics
///
/// Panics if [`StarlingInterpreter::initialize`] hasn't run.
fn wit() -> Wit {
    WIT.with(|cell| cell.borrow().expect("wit_dylib_initialize has not run yet"))
}

pub fn wit_stream(index: usize) -> Stream {
    wit().stream(index)
}

pub fn wit_future(index: usize) -> Future {
    wit().future(index)
}

/// Return the metadata of the resource type `index`, or `None` if there is no
/// such type. Use this for indices that come from JS, because a panic in a JS
/// callback aborts the instance.
pub fn wit_resource_checked(index: usize) -> Option<Resource> {
    let wit = wit();
    let count = wit.iter_resources().len();
    (index < count).then(|| wit.resource(index))
}

/// If `value` is a wrapper of the imported resource `resource` of the interface
/// `interface`, such as `request` of `wasi:http/types@0.3.0`, or of an
/// interface for which [`wit_name_mapping::same_interface`] returns `true`,
/// remove its handle and return it. Returns `None` for any other value.
///
/// The handle is removed as when the wrapper is lowered as an `own`, so the
/// wrapper can't be used afterwards.
pub fn take_imported_handle(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    interface: &str,
    resource: &str,
) -> Result<Option<u32>, ExnThrown> {
    let Some(wrapper) = resources::imported_wrapper(scope, value) else {
        return Ok(None);
    };
    let type_idx = wrapper.data().type_idx;
    let Some(ty) = wit_resource_checked(type_idx as usize) else {
        return Ok(None);
    };
    let matches = ty
        .interface()
        .is_some_and(|name| wit_name_mapping::same_interface(name, interface));
    if ty.new().is_some() || !matches || ty.name() != resource {
        return Ok(None);
    }
    resources::unwrap_to_canon(scope, value, type_idx, true).map(Some)
}

/// The payload type of a `future` type.
pub struct FuturePayload {
    /// The type of `T`, or `None` for a `future` without one.
    pub shape: Option<TypeShape>,
    /// The error class of the `err` case, if `T` is a `result` whose `err` type
    /// has one.
    pub err_class: Option<ErrorClass>,
    /// Whether `T` contains an owned resource handle.
    pub owns_resources: bool,
}

/// The element type of a `stream<T>` type.
pub struct StreamElement {
    /// The type of `T`, or `None` for a `stream` without one.
    pub shape: Option<TypeShape>,
    /// Whether `T` contains an owned resource handle.
    pub owns_resources: bool,
}

js::instance_local! {
    /// The [`FuturePayload`] of each `future` type index, built on first use.
    static FUTURE_PAYLOADS: RefCell<Vec<Option<Rc<FuturePayload>>>> =
        const { RefCell::new(Vec::new()) };

    /// The [`StreamElement`] of each `stream` type index, built on first use.
    static STREAM_ELEMENTS: RefCell<Vec<Option<Rc<StreamElement>>>> =
        const { RefCell::new(Vec::new()) };
}

/// The element type of the `stream<T>` with type index `index`.
pub fn stream_element(index: u32) -> Rc<StreamElement> {
    let index = index as usize;
    STREAM_ELEMENTS.with(|cache| {
        cached(&mut cache.borrow_mut(), index, || {
            let shape = wit_stream(index).ty().map(type_shape);
            let owns_resources = shape.as_ref().is_some_and(TypeShape::contains_own_resource);
            Rc::new(StreamElement {
                shape,
                owns_resources,
            })
        })
    })
}

/// Return the payload type of the `future` type with index `index`.
pub fn future_payload(index: u32) -> Rc<FuturePayload> {
    let index = index as usize;
    FUTURE_PAYLOADS.with(|cache| {
        cached(&mut cache.borrow_mut(), index, || {
            let ty = wit_future(index).ty();
            let shape = ty.map(type_shape);
            let owns_resources = shape.as_ref().is_some_and(TypeShape::contains_own_resource);
            Rc::new(FuturePayload {
                shape,
                err_class: err_class_of(ty),
                owns_resources,
            })
        })
    })
}

/// Add to `classes` the error classes of the `result` types in `ty` whose `err`
/// values are lifted as errors: `ty` itself if `direct` is set, and the payload
/// type of each `future` in `ty`.
fn collect_err_classes(ty: Type, direct: bool, classes: &mut Vec<ErrorClass>) {
    if direct {
        if let Some(class) = err_class_of(Some(ty)) {
            if !classes.contains(&class) {
                classes.push(class);
            }
        }
    }
    match ty {
        Type::Alias(alias) => collect_err_classes(alias.ty(), direct, classes),
        Type::Future(future) => {
            if let Some(payload) = future.ty() {
                collect_err_classes(payload, true, classes);
            }
        }
        Type::List(list) => collect_err_classes(list.ty(), false, classes),
        Type::Option(option) => collect_err_classes(option.ty(), false, classes),
        Type::Result(result) => {
            for arm in [result.ok(), result.err()].into_iter().flatten() {
                collect_err_classes(arm, false, classes);
            }
        }
        Type::Tuple(tuple) => {
            for element in tuple.types() {
                collect_err_classes(element, false, classes);
            }
        }
        Type::Record(record) => {
            for (_, field) in record.fields() {
                collect_err_classes(field, false, classes);
            }
        }
        Type::Variant(variant) => {
            for payload in variant.cases().filter_map(|(_, payload)| payload) {
                collect_err_classes(payload, false, classes);
            }
        }
        _ => {}
    }
}

/// Return the error class of the `err` case if `ty` is a `result` whose `err`
/// type has one.
fn err_class_of(ty: Option<Type>) -> Option<ErrorClass> {
    let Type::Result(result) = dealias(ty?) else {
        return None;
    };
    let err = result.err()?;
    let (interface, name) = match err {
        Type::Alias(alias) => {
            if matches!(dealias(alias.ty()), Type::Own(_) | Type::Borrow(_)) {
                return None;
            }
            // An interface that the world only exports has no module from which
            // the application could import the class, so an alias in it uses
            // the class of the type it refers to.
            match alias.interface() {
                Some(interface) if is_export_only(interface) => {
                    return err_class_of_named(alias.ty());
                }
                interface => (interface, alias.name()),
            }
        }
        other => return err_class_of_named(other),
    };
    Some(error_class_for(interface, name, err))
}

/// Return the error class of `err` if it is a named type. Aliases in
/// interfaces that the world only exports are followed to the type they refer
/// to.
fn err_class_of_named(err: Type) -> Option<ErrorClass> {
    let (interface, name) = match err {
        Type::Alias(alias) => match alias.interface() {
            Some(interface) if is_export_only(interface) => {
                return err_class_of_named(alias.ty());
            }
            interface => (interface, alias.name()),
        },
        Type::Record(record) => (record.interface(), record.name()),
        Type::Variant(variant) => (variant.interface(), variant.name()),
        Type::Enum(e) => (e.interface(), e.name()),
        Type::Flags(flags) => (flags.interface(), flags.name()),
        Type::Tuple(tuple) => (tuple.interface(), tuple.name()?),
        Type::Option(option) => (option.interface(), option.name()?),
        Type::Result(result) => (result.interface(), result.name()?),
        Type::List(list) => (list.interface(), list.name()?),
        _ => return None,
    };
    Some(error_class_for(interface, name, err))
}

/// Whether the world exports the interface named `interface` and does not
/// import it.
fn is_export_only(interface: &str) -> bool {
    let wit = wit();
    let named = |func_interface: Option<&str>| func_interface == Some(interface);
    wit.iter_export_funcs().any(|f| named(f.interface()))
        && !wit.iter_import_funcs().any(|f| named(f.interface()))
}

/// The [`ErrorClass`] named `name` in `interface`, for the `err` type `err`.
fn error_class_for(interface: Option<&str>, name: &str, err: Type) -> ErrorClass {
    let enum_cases = match dealias(err) {
        Type::Enum(e) => e.names().map(mangle_name).collect(),
        _ => Vec::new(),
    };
    ErrorClass {
        interface: interface.map(str::to_string),
        name: name.to_string(),
        enum_cases,
    }
}

/// Return the description of the world's imports, or `None` for a world
/// without exports, for which [`StarlingInterpreter::initialize`] isn't
/// called.
pub fn world_desc_opt() -> Option<WorldDesc> {
    WORLD.with(|cell| cell.borrow().clone())
}

/// Resolve every export of the world against the main module's namespace `ns`,
/// and install the export table (see [`crate::exports::install_table`]). Fails
/// with an error message if the main module doesn't provide an export.
///
/// Each export is looked up under the names returned by
/// [`wit_name_mapping::possible_export_names`]. If `cli` is set, the main
/// module must export a `run` function, and no other export can use the name
/// `run`.
///
/// `raw_http_handler` is the name of the exported interface that has the
/// runtime's own declaration of `wasi:http/handler`. Its functions are looked
/// up under the names for `wasi:http/handler`, and the main module may leave
/// them out. Returns whether and where the main module provides them, or `None`
/// if `raw_http_handler` is `None`.
///
/// A world without exports has no metadata, so its table is empty.
pub fn install_exports(
    scope: &Scope<'_>,
    ns: &Object<'_>,
    cli: bool,
    raw_http_handler: Option<&str>,
) -> Result<Option<RawHttpHandlerExport>, String> {
    RAW_HTTP_HANDLER.with(|cell| *cell.borrow_mut() = raw_http_handler.map(str::to_string));
    if cli {
        let callable = match crate::exports::resolve_export(scope, ns, "run") {
            Ok(crate::exports::Resolved::Plain { function, .. }) => {
                js::function::is_callable(function)
            }
            _ => false,
        };
        if !callable {
            js::exception::clear(scope);
            return Err(
                "the main module exports no `run` function, which the component's \
                 `wasi:cli/run` export calls. Export one, for example: \
                 export async function run() { ... }"
                    .to_string(),
            );
        }
    }
    let Some(wit) = WIT.with(|cell| *cell.borrow()) else {
        crate::exports::install_table(scope, ns, [])?;
        return Ok(None);
    };
    let items: Vec<wit_name_mapping::Item> = wit
        .iter_export_funcs()
        .map(|func| {
            let names = func.interface().map(|interface| {
                if Some(interface) == raw_http_handler {
                    InterfaceNames::of(HTTP_HANDLER_INTERFACE)
                } else {
                    InterfaceNames::of(interface)
                }
            });
            (names, wit_name_mapping::item_js_name(func.name()))
        })
        .collect();
    let fixed: &[&str] = if cli { &["run"] } else { &[] };
    let shapes = wit_name_mapping::possible_export_names(&items, fixed)?;
    let mut raw_paths = Vec::new();
    let specs: Vec<ExportSpec> = wit
        .iter_export_funcs()
        .zip(items.into_iter().zip(shapes))
        .map(|(func, ((names, _), shapes))| {
            let raw = raw_http_handler.is_some() && func.interface() == raw_http_handler;
            if raw {
                let item = wit_name_mapping::item_js_name(func.name());
                raw_paths.extend(shapes.iter().map(|shape| shape.path(&item)));
            }
            ExportSpec {
                interface: names.map(|names| names.wit_name),
                name: func.name().to_string(),
                shapes,
                result: result_shape(func.result()),
                optional: raw,
            }
        })
        .collect();
    crate::exports::install_table(scope, ns, specs)?;
    Ok(raw_http_handler.map(|raw| RawHttpHandlerExport {
        provided: crate::exports::with_export_table(|table| {
            wit.iter_export_funcs()
                .any(|func| func.interface() == Some(raw) && table.provided(func.index()))
        }),
        paths: raw_paths,
    }))
}

/// Whether and where the main module provides the functions of the runtime's
/// own `wasi:http/handler` export (see [`install_exports`]).
pub struct RawHttpHandlerExport {
    /// Whether the main module provides it.
    pub provided: bool,
    /// The names under which the main module may export them, such as
    /// `handler.handle`.
    pub paths: Vec<String>,
}

/// The interface whose export names are used for the runtime's own
/// `wasi:http/handler` export (see [`install_exports`]).
const HTTP_HANDLER_INTERFACE: &str = "wasi:http/handler";

/// The [`ResultShape`] of a WIT result type, resolving aliases.
fn result_shape(ty: Option<Type>) -> ResultShape {
    let Some(ty) = ty else {
        return ResultShape::None;
    };
    match dealias(ty) {
        Type::Result(r) => ResultShape::Result {
            ok: r.ok().is_some(),
            err: r.err().map(|err| Rc::new(type_shape(err))),
        },
        Type::Future(_) => ResultShape::Future,
        _ => ResultShape::Plain,
    }
}

// ===========================================================================
// The interpreter
// ===========================================================================

/// The StarlingMonkey component-model interpreter.
pub struct StarlingInterpreter;

/// The context of one component call.
///
/// Export and import calls must use this same type, because the generated
/// `wit_dylib_*` functions cast the context pointer to
/// `&mut StarlingInterpreter::CallCx`, and `call_import_sync` casts the
/// `&mut impl Call` it receives to the same type.
pub enum CallCx<'a> {
    /// The stack of an export call.
    Owned(CallStack),
    /// The stack of an import call, owned by the caller.
    Borrowed(&'a mut CallStack),
}

impl CallCx<'_> {
    pub(crate) fn stack(&mut self) -> &mut CallStack {
        match self {
            CallCx::Owned(stack) => stack,
            CallCx::Borrowed(stack) => stack,
        }
    }

    /// Like `stack`, but by shared reference.
    fn stack_ref(&self) -> &CallStack {
        match self {
            CallCx::Owned(stack) => stack,
            CallCx::Borrowed(stack) => stack,
        }
    }
}

impl Interpreter for StarlingInterpreter {
    type CallCx<'a> = CallCx<'a>;

    /// Called by the bindings library's constructors, possibly before the
    /// runtime's constructors have run, so this must not use SpiderMonkey. The
    /// runtime is initialized later, by the `init` export.
    fn initialize(wit: Wit) {
        // Building the description reads `wit` through `wit()`, so it is stored
        // first.
        WIT.with(|cell| *cell.borrow_mut() = Some(wit));
        resources::set_imported_drop(drop_imported);
        let world = build_world_desc(wit);
        WORLD.with(|cell| *cell.borrow_mut() = Some(world));
    }

    fn export_start<'a>(_wit: Wit, _func: ExportFunction) -> Box<CallCx<'a>> {
        Box::new(CallCx::Owned(CallStack::new()))
    }

    /// Call a synchronous export with the lifted arguments on the stack, and
    /// push the result for lowering.
    ///
    /// The export runs without an event loop, so timers and async imports throw
    /// a `TypeError` naming the export. Microtasks queued by the export run
    /// before its result is lowered.
    fn export_call(_wit: Wit, func: ExportFunction, cx: &mut CallCx<'_>) {
        init::before_export();
        let index = func.index();
        let result = crate::exports::with_export_table(|table| table.result(index));
        let reason = || {
            format!(
                "the synchronous export `{}` runs without one. Declare the export `async` in \
                 WIT to use timers and async imports",
                wit_function_name(func.interface(), func.name())
            )
        };

        init::with_scope(|scope| {
            // Pop the arguments and call the export. The result is rooted on
            // `scope`.
            let outcome = core_runtime::event_loop::without_event_loop(reason, || {
                let outcome = dispatch_export(scope, index, func.name(), cx.stack(), &result);
                core_runtime::event_loop::run_microtasks(scope);
                outcome
            });

            // Push the result in the form expected by the generated lowering
            // code: nothing if there is no result, the value for a plain result,
            // and a `{tag, val}` object for a `result`.
            push_export_result(cx.stack(), scope, &result, outcome);
            keep_lowering_source(cx.stack(), &result, index);

            // Borrows are released here, before `export_finish`, because dropping
            // one calls an import, which post-return forbids.
            if resources::release_borrows(cx.stack(), scope, &drop_exported_handle).is_err() {
                trap(scope, "releasing borrows at export end failed")
            }
        });
    }

    /// Call an async export: create an event loop for the call, call the export
    /// with the lifted arguments on `cx`'s stack, run the loop until the
    /// returned promise settles, and lower the result with `task-return`.
    ///
    /// Each call has its own
    /// [`OwnedInvocation`](core_runtime::invocation::OwnedInvocation), so that
    /// the timers, `fetch` calls and async imports of concurrent calls are kept
    /// apart. The returned promise is kept on the call's stack, because no
    /// [`Scope`] can be held across an `await`.
    #[allow(clippy::manual_async_fn)]
    fn export_call_async(
        _wit: Wit,
        func: ExportFunction,
        cx: Box<CallCx<'static>>,
    ) -> impl std::future::Future<Output = ()> {
        async move {
            init::before_export();
            let mut cx = cx;
            let index = func.index();
            let result = crate::exports::with_export_table(|table| table.result(index));

            let invocation =
                core_runtime::invocation::OwnedInvocation::new(init::runtime(), Default::default());

            // Phase 1: call the export, and keep its result on the call stack,
            // which is traced during the awaits below.
            //
            // The result isn't necessarily a promise: a plain function returns a
            // value, and an exception converted into an `err` value is returned
            // as `Outcome::Threw`. The event loop isn't run for either.
            let name = func.name();
            let pending = init::with_scope(|scope| {
                core_runtime::event_loop::with_event_loop(invocation.state().event_loop(), |_| {
                    let outcome = dispatch_export(scope, index, name, cx.stack(), &result);
                    core_runtime::event_loop::run_microtasks(scope);
                    match outcome {
                        // For a `future` result, the returned promise is
                        // lowered as the future, without waiting for it.
                        crate::exports::Outcome::Returned(v)
                            if result == crate::exports::ResultShape::Future =>
                        {
                            cx.stack().push_value(v.get());
                            StackTop::Future
                        }
                        crate::exports::Outcome::Returned(v) => {
                            let pending = !crate::exports::promise_is_settled(scope, v);
                            cx.stack().push_value(v.get());
                            // The event loop only runs if the value is a
                            // pending promise.
                            StackTop::Returned { pending }
                        }
                        crate::exports::Outcome::Threw(v) => {
                            // An exception thrown synchronously and converted
                            // into an `err` value.
                            cx.stack().push_value(v.get());
                            StackTop::Threw
                        }
                    }
                })
            });

            // Phase 2: run this call's event loop until the promise on top of
            // the stack settles.
            if matches!(pending, StackTop::Returned { pending: true }) {
                let stack = cx.stack_ref();
                crate::run::drive_until_settled(init::raw_cx(), &invocation, |scope| {
                    scope.root_value(stack.last())
                })
                .await;
            }

            // Phase 3: convert the value on top of the stack into an `Outcome`,
            // and push the result in the form expected by the generated lowering
            // code.
            init::with_scope(|scope| {
                let top = scope.root_value(cx.stack().pop_value());
                let outcome = match pending {
                    StackTop::Threw => crate::exports::Outcome::Threw(top),
                    StackTop::Future => crate::exports::Outcome::Returned(top),
                    StackTop::Returned { .. } => settled_outcome(scope, top, &result, func),
                };
                push_export_result(cx.stack(), scope, &result, outcome);
                keep_lowering_source(cx.stack(), &result, index);
                // Dropping a borrow calls the `[resource-drop]` import, which is
                // legal here but would trap after `task-return`.
                if resources::release_borrows(cx.stack(), scope, &drop_exported_handle).is_err() {
                    trap(scope, "releasing borrows at async export end failed")
                }
            });

            // Lowering a stream or future result starts the pump or write that
            // fills it on this call's event loop, so the loop is active during
            // the lowering. Microtasks queued by the lowering, such as the start
            // of a pump, run before the loop does.
            core_runtime::event_loop::with_event_loop(invocation.state().event_loop(), |_| {
                func.call_task_return(&mut *cx);
                init::with_scope(core_runtime::event_loop::run_microtasks);
            });

            // The values of a stream or future result are transferred after
            // `task-return`, on this call's event loop, which runs until the
            // transfers are done.
            crate::run::drain_async_work(init::raw_cx(), &invocation).await;

            // Pending timers are dropped with the event loop.
            drop(invocation);
        }
    }

    /// Drop the call context, which frees its deferred allocations. This runs in
    /// `cabi_post`, where calling imports isn't allowed, so borrows are
    /// released earlier, in [`export_call`](Self::export_call).
    fn export_finish(cx: Box<CallCx<'_>>, _func: ExportFunction) {
        drop(cx);
    }

    /// Drop the exported resource with the rep `handle`, whose type is `ty` (see
    /// [`resources::drop_exported`]).
    fn resource_dtor(ty: Resource, handle: usize) {
        let rep = handle as u32;
        let expected = ty.index() as u32;
        init::with_scope(|scope| {
            if resources::drop_exported(scope, rep, expected).is_err() {
                trap(scope, "dropping an exported resource failed")
            }
        });
    }
}

// ===========================================================================
// The `Call` implementation
// ===========================================================================

impl Call for CallCx<'_> {
    unsafe fn defer_deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged from the generated caller, which owns the
        // allocation.
        unsafe { self.stack().defer_deallocate(ptr, layout) }
    }

    // --- Lower (pop_*): JS value off the stack -> WIT value ---

    fn pop_u8(&mut self) -> u8 {
        self.pop(value::pop_u8, "pop_u8")
    }
    fn pop_u16(&mut self) -> u16 {
        self.pop(value::pop_u16, "pop_u16")
    }
    fn pop_u32(&mut self) -> u32 {
        self.pop(value::pop_u32, "pop_u32")
    }
    fn pop_u64(&mut self) -> u64 {
        self.pop(value::pop_u64, "pop_u64")
    }
    fn pop_s8(&mut self) -> i8 {
        self.pop(value::pop_s8, "pop_s8")
    }
    fn pop_s16(&mut self) -> i16 {
        self.pop(value::pop_s16, "pop_s16")
    }
    fn pop_s32(&mut self) -> i32 {
        self.pop(value::pop_s32, "pop_s32")
    }
    fn pop_s64(&mut self) -> i64 {
        self.pop(value::pop_s64, "pop_s64")
    }
    fn pop_bool(&mut self) -> bool {
        self.pop(value::pop_bool, "pop_bool")
    }
    fn pop_char(&mut self) -> char {
        self.pop(value::pop_char, "pop_char")
    }
    fn pop_f32(&mut self) -> f32 {
        self.pop(value::pop_f32, "pop_f32")
    }
    fn pop_f64(&mut self) -> f64 {
        self.pop(value::pop_f64, "pop_f64")
    }

    fn pop_string(&mut self) -> &str {
        // The string is stored on the stack, so the returned `&str` stays valid
        // until the call ends.
        let owned = self.pop(value::pop_string, "pop_string");
        self.stack().store_string(owned)
    }

    fn pop_borrow(&mut self, ty: Resource) -> u32 {
        with_resource_desc(ty, |desc| {
            self.pop(|s, sc| resources::pop_borrow(s, sc, desc), "pop_borrow")
        })
    }
    fn pop_own(&mut self, ty: Resource) -> u32 {
        // For an imported resource type with a registered adapter, values
        // converted by the adapter are also accepted.
        let adapter = match ty.new() {
            None => resources::own_adapter(ty.interface(), ty.name()).map(|(adapter, _)| adapter),
            Some(_) => None,
        };
        if let Some(adapter) = adapter {
            let adapted = self.pop(
                |s, sc| {
                    let value = sc.root_value(s.last());
                    if resources::imported_wrapper(sc, value).is_some() {
                        return Ok(None);
                    }
                    let handle = adapter(sc, value)?;
                    if handle.is_some() {
                        s.pop_value();
                    }
                    Ok(handle)
                },
                "pop_own",
            );
            if let Some(handle) = adapted {
                return handle;
            }
        }
        with_resource_desc(ty, |desc| {
            self.pop(|s, sc| resources::pop_own(s, sc, desc), "pop_own")
        })
    }
    fn pop_enum(&mut self, ty: Enum) -> u32 {
        let cases = ty.names().len() as u32;
        self.pop(|s, sc| value::pop_enum(s, sc, cases), "pop_enum")
    }
    fn pop_flags(&mut self, ty: Flags) -> u32 {
        let flags = ty.names().len() as u32;
        self.pop(|s, sc| value::pop_flags(s, sc, flags), "pop_flags")
    }
    fn pop_future(&mut self, ty: Future) -> u32 {
        // Lowering a `future` passes its readable end to the host.
        let index = ty.index() as u32;
        self.pop(
            |s, sc| crate::promises::future_to_handle(sc, sc.root_value(s.pop_value()), index),
            "pop_future",
        )
    }
    fn pop_stream(&mut self, ty: Stream) -> u32 {
        // Lowering a `stream` passes its readable end to the host.
        let index = ty.index() as u32;
        self.pop(
            |s, sc| crate::readable::stream_to_handle(sc, sc.root_value(s.pop_value()), index),
            "pop_stream",
        )
    }
    fn pop_option(&mut self, ty: WitOption) -> u32 {
        let nested = inner_is_option(ty);
        let is_some = self.pop(|s, sc| value::pop_option(s, sc, nested), "pop_option");
        is_some as u32
    }
    fn pop_result(&mut self, ty: WitResult) -> u32 {
        let (ok, err) = (ty.ok().is_some(), ty.err().is_some());
        self.pop(|s, sc| value::pop_result(s, sc, ok, err), "pop_result")
    }
    fn pop_variant(&mut self, ty: Variant) -> u32 {
        let cases = variant_cases(ty);
        self.pop(|s, sc| value::pop_variant(s, sc, &cases), "pop_variant")
    }
    fn pop_record(&mut self, ty: Record) {
        let names = record_field_names(ty);
        self.pop(|s, sc| value::pop_record(s, sc, &names), "pop_record")
    }
    fn pop_tuple(&mut self, ty: Tuple) {
        let len = ty.types().count();
        self.pop(|s, sc| value::pop_tuple(s, sc, len), "pop_tuple")
    }

    /// Lower a list of numbers given as a typed array, or as a
    /// [`TypeShapeValue::NumericList`], into a buffer. Returns `None` for other
    /// element types and other values, which are lowered with
    /// [`pop_list`](Self::pop_list) instead.
    ///
    /// The returned pointer isn't null, even for an empty list, because a null
    /// pointer selects `pop_list`. The buffer is freed when the [`CallStack`] is
    /// dropped.
    unsafe fn maybe_pop_list(&mut self, ty: List) -> Option<(*const u8, usize)> {
        let elem = numeric_scalar_shape(ty.ty())?;
        let stack = self.stack();
        let is_view = match stack.last_shaped() {
            Some(shaped) => matches!(shaped, TypeShapeValue::NumericList(_)),
            None => init::with_scope(|scope| {
                js::ArrayBufferView::from_jsval(scope, scope.root_value(stack.last()), ()).is_ok()
            }),
        };
        if !is_view {
            return None;
        }
        let (dst, layout, count) = self.pop(
            |s, sc| value::pop_numeric_list(s, sc, &elem),
            "maybe_pop_list",
        );
        if count > 0 {
            // SAFETY: `pop_numeric_list` transferred ownership of an allocation
            // made with the global allocator and exactly this layout.
            unsafe { self.stack().defer_deallocate(dst, layout) };
        }
        Some((dst as *const u8, count))
    }

    fn pop_list(&mut self, _ty: List) -> usize {
        self.pop(|s, sc| s.lower_list_begin(sc), "pop_list")
    }
    fn pop_list_iter_next(&mut self, _ty: List) {
        self.pop(|s, sc| s.lower_list_next(sc), "pop_list_iter_next")
    }
    fn pop_list_iter(&mut self, _ty: List) {
        self.stack().lower_list_end();
    }

    fn pop_map(&mut self, _ty: Map) -> usize {
        unimplemented!("maps are not yet supported")
    }
    fn pop_map_iter_next(&mut self, _ty: Map) {
        unimplemented!("maps are not yet supported")
    }
    fn pop_map_iter(&mut self, _ty: Map) {
        unimplemented!("maps are not yet supported")
    }

    // --- Lift (push_*): WIT value -> JS value onto the stack ---

    fn push_bool(&mut self, val: bool) {
        value::push_bool(self.stack(), val);
    }
    fn push_char(&mut self, val: char) {
        self.push(|s, sc| value::push_char(s, sc, val), "push_char");
    }
    fn push_u8(&mut self, val: u8) {
        value::push_u8(self.stack(), val);
    }
    fn push_s8(&mut self, val: i8) {
        value::push_s8(self.stack(), val);
    }
    fn push_u16(&mut self, val: u16) {
        value::push_u16(self.stack(), val);
    }
    fn push_s16(&mut self, val: i16) {
        value::push_s16(self.stack(), val);
    }
    fn push_u32(&mut self, val: u32) {
        value::push_u32(self.stack(), val);
    }
    fn push_s32(&mut self, val: i32) {
        value::push_s32(self.stack(), val);
    }
    fn push_u64(&mut self, val: u64) {
        self.push(|s, sc| value::push_u64(s, sc, val), "push_u64");
    }
    fn push_s64(&mut self, val: i64) {
        self.push(|s, sc| value::push_s64(s, sc, val), "push_s64");
    }
    fn push_f32(&mut self, val: f32) {
        value::push_f32(self.stack(), val);
    }
    fn push_f64(&mut self, val: f64) {
        value::push_f64(self.stack(), val);
    }
    fn push_string(&mut self, val: String) {
        self.push(|s, sc| value::push_string(s, sc, &val), "push_string");
    }
    fn push_record(&mut self, ty: Record) {
        let names = record_field_names(ty);
        self.push(|s, sc| value::push_record(s, sc, &names), "push_record");
    }
    fn push_tuple(&mut self, ty: Tuple) {
        let len = ty.types().count();
        self.push(|s, sc| value::push_tuple(s, sc, len), "push_tuple");
    }
    fn push_flags(&mut self, _ty: Flags, bits: u32) {
        self.push(
            |s, _sc| {
                value::push_flags(s, bits);
                Ok(())
            },
            "push_flags",
        );
    }
    fn push_enum(&mut self, ty: Enum, discr: u32) {
        let cases = ty.names().len() as u32;
        self.push(|s, sc| value::push_enum(s, sc, cases, discr), "push_enum");
    }
    fn push_borrow(&mut self, ty: Resource, handle: u32) {
        with_resource_desc(ty, |desc| {
            self.push(
                |s, sc| resources::push_borrow(s, sc, desc, handle),
                "push_borrow",
            )
        });
    }
    fn push_own(&mut self, ty: Resource, handle: u32) {
        with_resource_desc(ty, |desc| {
            self.push(|s, sc| resources::push_own(s, sc, desc, handle), "push_own")
        });
    }
    fn push_future(&mut self, ty: Future, handle: u32) {
        let index = ty.index() as u32;
        self.push(
            |s, sc| {
                s.push_value(crate::promises::future_from_handle(sc, index, handle)?);
                Ok(())
            },
            "push_future",
        );
    }
    fn push_stream(&mut self, ty: Stream, handle: u32) {
        let index = ty.index() as u32;
        self.push(
            |s, sc| {
                s.push_value(crate::readable::stream_from_handle(sc, index, handle)?);
                Ok(())
            },
            "push_stream",
        );
    }
    fn push_variant(&mut self, ty: Variant, discr: u32) {
        let cases = variant_cases(ty);
        self.push(
            |s, sc| value::push_variant(s, sc, &cases, discr),
            "push_variant",
        );
    }
    fn push_option(&mut self, ty: WitOption, is_some: bool) {
        let nested = inner_is_option(ty);
        self.push(
            |s, sc| value::push_option(s, sc, nested, is_some),
            "push_option",
        );
    }
    fn push_result(&mut self, ty: WitResult, is_err: bool) {
        let (ok, err) = (ty.ok().is_some(), ty.err().is_some());
        self.push(
            |s, sc| value::push_result(s, sc, ok, err, is_err),
            "push_result",
        );
    }

    /// Lift a list of numbers as a typed array that takes over the list's
    /// buffer. Returns `false` for other element types, which are lifted with
    /// [`push_list`](Self::push_list) and [`list_append`](Self::list_append)
    /// instead.
    unsafe fn push_raw_list(&mut self, ty: List, ptr: *mut u8, len: usize) -> bool {
        let Some(elem) = numeric_scalar_shape(ty.ty()) else {
            return false;
        };
        let Some(layout) = elem.numeric_list_layout(len) else {
            self.push(
                |_, sc| {
                    Err(js::error::throw_range_error(
                        sc,
                        c"the list is longer than memory can hold",
                    ))
                },
                "push_raw_list",
            );
            return true;
        };
        // Returning `true` takes ownership of the allocation: the generated code
        // skips both its element loop and its trailing `dealloc_bytes`.
        self.push(
            // SAFETY: `cabi_realloc`, which made a non-empty list's allocation, and
            // the global allocator are the same allocator, and `layout` matches
            // what `dealloc_bytes` would have reconstructed. It holds `len` valid
            // contiguous elements.
            |s, sc| unsafe { value::push_numeric_list(s, sc, &elem, ptr, layout, len) },
            "push_raw_list",
        );
        true
    }

    fn push_list(&mut self, _ty: List, capacity: usize) {
        self.push(|s, sc| value::push_list_begin(s, sc, capacity), "push_list");
    }
    fn list_append(&mut self, _ty: List) {
        self.push(value::list_append, "list_append");
    }

    fn push_map(&mut self, _ty: Map, _capacity: usize) {
        unimplemented!("maps are not yet supported")
    }
    fn map_append(&mut self, _ty: Map) {
        unimplemented!("maps are not yet supported")
    }
}

impl CallCx<'_> {
    /// Run the lowering function `f` in a new scope, and trap if it fails. `what`
    /// names the operation in the trap message.
    fn pop<T>(
        &mut self,
        f: impl FnOnce(&mut CallStack, &Scope<'_>) -> Result<T, ExnThrown>,
        what: &str,
    ) -> T {
        let stack = self.stack();
        init::with_scope(|scope| match f(stack, scope) {
            Ok(v) => v,
            Err(_) => lowering_failed(stack, scope, what),
        })
    }

    /// Run the lifting function `f` in a new scope, and trap if it fails. `what`
    /// names the operation in the trap message.
    fn push(
        &mut self,
        f: impl FnOnce(&mut CallStack, &Scope<'_>) -> Result<(), ExnThrown>,
        what: &str,
    ) {
        let stack = self.stack();
        init::with_scope(|scope| {
            if f(stack, scope).is_err() {
                trap(scope, what)
            }
        });
    }
}

// ===========================================================================
// Import calls
// ===========================================================================

/// The [`ImportInvoker`] that calls imports with the code generated by
/// `wit-dylib`.
pub struct WasmImportInvoker;

impl ImportInvoker for WasmImportInvoker {
    fn invoke(
        &self,
        _scope: &Scope<'_>,
        stack: &mut CallStack,
        func: &ImportFuncDesc,
    ) -> Result<(), ExnThrown> {
        let w = wit();
        let import = w.import_func(func.index as usize);
        let mut cx = CallCx::Borrowed(stack);
        import.call_import_sync(&mut cx);
        Ok(())
    }

    fn supports_async(&self) -> bool {
        true
    }

    /// Call an async import on the wit-bindgen executor. The future captures
    /// only the import's index, so it doesn't borrow `self`.
    fn invoke_async(
        &self,
        stack: CallStack,
        func: std::rc::Rc<ImportFuncDesc>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CallStack> + 'static>> {
        let index = func.index;
        Box::pin(async move {
            let mut cx = CallCx::Owned(stack);
            let import = wit().import_func(index as usize);
            import.call_import_async(&mut cx).await;
            match cx {
                CallCx::Owned(stack) => stack,
                CallCx::Borrowed(_) => unreachable!("async import cx is always owned"),
            }
        })
    }

    fn drop_exported_handle(&self, type_idx: u32, handle: u32) {
        drop_exported_handle(type_idx, handle);
    }
}

/// Create the world's import modules (see
/// [`imports::synthesize_import_modules`]). This is the `synthesize` argument
/// of [`init::initialize_runtime`].
pub fn synthesize(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    // For a world without exports, `StarlingInterpreter::initialize` isn't
    // called, so there is no world description.
    let Some(world) = world_desc_opt() else {
        return Ok(());
    };
    imports::synthesize_import_modules(scope, &world, Rc::new(WasmImportInvoker))
}

// ===========================================================================
// Dropping imported resources
// ===========================================================================

/// Drop the canonical handle `handle` of the imported resource type `type_idx`
/// through the type's wit-dylib `drop`. Throws a `TypeError` if `type_idx` is out
/// of range.
fn drop_imported(scope: &Scope<'_>, type_idx: u32, handle: u32) -> Result<(), ExnThrown> {
    let res = wit_resource_checked(type_idx as usize)
        .ok_or_else(|| throw_type_error(scope, c"resource type index is out of range"))?;
    // SAFETY: `resource.drop` traps on a handle that is not in the instance's
    // table, so any `handle` is sound.
    unsafe { res.drop()(handle) };
    Ok(())
}

// ===========================================================================
// `wit_dylib_ffi::Type` -> `TypeShape` and `Wit` -> `WorldDesc`
// ===========================================================================

/// Convert `ty` into a [`TypeShape`], with field, case and flag names converted
/// to JS names.
fn type_shape(ty: Type) -> TypeShape {
    match ty {
        Type::Bool => TypeShape::Bool,
        Type::U8 => TypeShape::U8,
        Type::U16 => TypeShape::U16,
        Type::U32 => TypeShape::U32,
        Type::U64 => TypeShape::U64,
        Type::S8 => TypeShape::S8,
        Type::S16 => TypeShape::S16,
        Type::S32 => TypeShape::S32,
        Type::S64 => TypeShape::S64,
        Type::F32 => TypeShape::F32,
        Type::F64 => TypeShape::F64,
        Type::Char => TypeShape::Char,
        Type::String => TypeShape::String,
        Type::List(l) => TypeShape::List(Box::new(type_shape(l.ty()))),
        Type::Record(r) => TypeShape::Record(
            r.fields()
                .map(|(name, ty)| (mangle_name(name), type_shape(ty)))
                .collect(),
        ),
        Type::Tuple(t) => TypeShape::Tuple(t.types().map(type_shape).collect()),
        Type::Variant(v) => TypeShape::Variant(
            v.cases()
                .map(|(name, ty)| (mangle_name(name), ty.map(type_shape)))
                .collect(),
        ),
        Type::Enum(e) => TypeShape::Enum(e.names().map(mangle_name).collect()),
        Type::Option(o) => TypeShape::Option(Box::new(type_shape(o.ty()))),
        Type::Result(r) => TypeShape::Result(
            r.ok().map(|t| Box::new(type_shape(t))),
            r.err().map(|t| Box::new(type_shape(t))),
        ),
        Type::Flags(f) => TypeShape::Flags(f.names().map(mangle_name).collect()),
        Type::Own(res) => TypeShape::OwnResource {
            index: res.index() as u32,
            imported: res.new().is_none(),
        },
        Type::Borrow(res) => TypeShape::BorrowResource {
            index: res.index() as u32,
            imported: res.new().is_none(),
        },
        Type::Alias(a) => type_shape(a.ty()),
        Type::FixedLengthList(_) => TypeShape::Unsupported("fixed-length list"),
        Type::Map(_) => TypeShape::Unsupported("map"),
        Type::Stream(s) => TypeShape::Stream(s.index() as u32),
        Type::Future(f) => TypeShape::Future(f.index() as u32),
        Type::ErrorContext => TypeShape::Unsupported("error-context"),
    }
}

/// Return the JS names of `r`'s fields in declaration order. The names are
/// cached per record type.
fn record_field_names(r: Record) -> Rc<[&'static CStr]> {
    RECORD_NAMES.with(|cell| {
        cached(&mut cell.borrow_mut(), r.index(), || {
            r.fields()
                .map(|(name, _)| js::class::static_property_name(&mangle_name(name)))
                .collect()
        })
    })
}

/// Return the JS names of `v`'s cases in declaration order, each with whether
/// the case has a value. The names are cached per variant type.
fn variant_cases(v: Variant) -> Rc<[(&'static CStr, bool)]> {
    VARIANT_CASES.with(|cell| {
        cached(&mut cell.borrow_mut(), v.index(), || {
            v.cases()
                .map(|(name, ty)| {
                    (
                        js::class::static_property_name(&mangle_name(name)),
                        ty.is_some(),
                    )
                })
                .collect()
        })
    })
}

/// Return the entry at `index` of a cache indexed by type index, created with
/// `build` if it doesn't exist.
fn cached<T: ?Sized>(
    entries: &mut Vec<Option<Rc<T>>>,
    index: usize,
    build: impl FnOnce() -> Rc<T>,
) -> Rc<T> {
    if entries.len() <= index {
        entries.resize(index + 1, None);
    }
    Rc::clone(entries[index].get_or_insert_with(build))
}

/// Return the [`TypeShape`] of `ty` if it is a number type, following aliases,
/// or `None` otherwise. Unlike [`type_shape`], this doesn't build the shapes of
/// other types.
fn numeric_scalar_shape(ty: Type) -> Option<TypeShape> {
    let shape = match dealias(ty) {
        Type::U8 => TypeShape::U8,
        Type::U16 => TypeShape::U16,
        Type::U32 => TypeShape::U32,
        Type::U64 => TypeShape::U64,
        Type::S8 => TypeShape::S8,
        Type::S16 => TypeShape::S16,
        Type::S32 => TypeShape::S32,
        Type::S64 => TypeShape::S64,
        Type::F32 => TypeShape::F32,
        Type::F64 => TypeShape::F64,
        _ => return None,
    };
    debug_assert!(shape.view_kind().is_some());
    Some(shape)
}

/// Whether the payload type of `o` is an option, following aliases (see
/// [`value::push_option`]).
fn inner_is_option(o: WitOption) -> bool {
    matches!(dealias(o.ty()), Type::Option(_))
}

/// Return the type that `ty` refers to, following aliases.
fn dealias(mut ty: Type) -> Type {
    while let Type::Alias(alias) = ty {
        ty = alias.ty();
    }
    ty
}

/// Run `f` with the [`ResourceDesc`] of the resource type `ty`. A type with
/// `new` and `rep` functions is exported. Any other type is imported.
fn with_resource_desc<R>(ty: Resource, f: impl FnOnce(&ResourceDesc<'_>) -> R) -> R {
    let type_idx = ty.index() as u32;
    match (ty.new(), ty.rep()) {
        (Some(new), Some(rep)) => {
            let drop = ty.drop();
            // SAFETY: `new`, `rep` and `drop` are this type's canonical
            // `resource.new`, `resource.rep` and `resource.drop`. `resource.new`
            // takes any rep, and the other two trap on a handle that is not in
            // the instance's table, so any argument is sound.
            let new_fn = move |rep_index: u32| unsafe { new(rep_index as usize) };
            let rep_fn = move |handle: u32| unsafe { rep(handle) as u32 };
            let drop_fn = move |handle: u32| unsafe { drop(handle) };
            f(&ResourceDesc::Exported {
                type_idx,
                new: &new_fn,
                rep: &rep_fn,
                drop: &drop_fn,
            })
        }
        _ => f(&ResourceDesc::Imported { type_idx }),
    }
}

/// Drop `handle`, a handle of the exported resource type `type_idx` that was
/// created to lend a resource to the host (see [`resources::release_borrows`]).
fn drop_exported_handle(type_idx: u32, handle: u32) {
    let res = wit().resource(type_idx as usize);
    // SAFETY: as in `drop_imported`.
    unsafe { res.drop()(handle) };
}

/// Build the description of the world's imports from `wit`, without using
/// SpiderMonkey.
///
/// Imports are grouped by interface, with a separate group for the imports
/// defined on the world. Each interface's functions are sorted into functions
/// that don't belong to a resource, and the constructors, methods and static
/// functions of each resource. Only imported resources get a class.
fn build_world_desc(wit: Wit) -> WorldDesc {
    use std::collections::{BTreeMap, BTreeSet};

    // The imports of one interface, keyed by the interface's WIT name, or by
    // `None` for the imports defined on the world.
    #[derive(Default)]
    struct Acc {
        functions: Vec<ImportFuncDesc>,
        resources: BTreeMap<String, ResAcc>,
        error_classes: Vec<ErrorClass>,
        enums: Vec<EnumDesc>,
    }
    #[derive(Default)]
    struct ResAcc {
        type_idx: Option<u32>,
        constructor: Option<ImportFuncDesc>,
        methods: Vec<(String, ImportFuncDesc)>,
        statics: Vec<(String, ImportFuncDesc)>,
    }

    // The interfaces in the order in which they are found, so that the modules
    // are created in a stable order.
    let mut order: Vec<Option<String>> = Vec::new();
    let mut ifaces: BTreeMap<Option<String>, Acc> = BTreeMap::new();

    for import in wit.iter_import_funcs() {
        let iface_key = import.interface().map(str::to_string);
        if !ifaces.contains_key(&iface_key) {
            order.push(iface_key.clone());
        }
        let acc = ifaces.entry(iface_key).or_default();

        let func = ImportFuncDesc {
            index: import.index() as u32,
            name: import.name().to_string(),
            params: import.params().map(type_shape).collect(),
            result: import.result().map(type_shape),
            // `AsyncFilterSet::default()` binds an `async` WIT import as async,
            // so its `async_impl` function is present.
            is_async: import.is_async(),
            err_class: err_class_of(import.result()),
        };
        match FunctionName::parse(import.name()) {
            FunctionName::Constructor { resource } => {
                let res = acc.resources.entry(resource.to_string()).or_default();
                // A constructor's result is `own<R>`, giving the resource type index.
                res.type_idx = res.type_idx.or_else(|| own_type_idx(func.result.as_ref()));
                res.constructor = Some(func);
            }
            FunctionName::Method { resource, name } => {
                let res = acc.resources.entry(resource.to_string()).or_default();
                // A method's first parameter is `borrow<R>` or `own<R>`, giving the
                // type.
                res.type_idx = res
                    .type_idx
                    .or_else(|| handle_type_idx(func.params.first()));
                res.methods.push((mangle_name(name), func));
            }
            FunctionName::Static { resource, name } => {
                let res = acc.resources.entry(resource.to_string()).or_default();
                res.statics.push((mangle_name(name), func));
            }
            FunctionName::Plain(_) => acc.functions.push(func),
        }
    }
    // An imported resource without a constructor, method or static function
    // also gets a class, so that its wrappers have `[Symbol.dispose]()` and its
    // module exports the class. Only exported resources have a `new` function.
    for resource in wit
        .iter_resources()
        .filter(|resource| resource.new().is_none())
    {
        let iface_key = resource.interface().map(str::to_string);
        if !ifaces.contains_key(&iface_key) {
            order.push(iface_key.clone());
        }
        let res = ifaces
            .entry(iface_key)
            .or_default()
            .resources
            .entry(resource.name().to_string())
            .or_default();
        res.type_idx = res.type_idx.or(Some(resource.index() as u32));
    }

    // Each error class is exported from the module of the interface defining
    // its type.
    let mut error_classes = Vec::new();
    for import in wit.iter_import_funcs() {
        for param in import.params() {
            collect_err_classes(param, false, &mut error_classes);
        }
        if let Some(result) = import.result() {
            collect_err_classes(result, true, &mut error_classes);
        }
    }
    for export in wit.iter_export_funcs() {
        for param in export.params() {
            collect_err_classes(param, false, &mut error_classes);
        }
        if let Some(result) = export.result() {
            collect_err_classes(result, true, &mut error_classes);
        }
    }
    // A class whose type is defined by an exported interface without a module
    // isn't exported. Every other class is exported from the module of its
    // interface, or from the module for the world, which are created even if
    // they have no functions.
    let exported: BTreeSet<&str> = wit
        .iter_export_funcs()
        .filter_map(|export| export.interface())
        .collect();
    let mut unexported_error_classes = Vec::new();
    for class in error_classes {
        let key = class.interface.clone();
        if key.as_deref().is_some_and(|name| exported.contains(name)) && !ifaces.contains_key(&key)
        {
            unexported_error_classes.push(class);
            continue;
        }
        if !ifaces.contains_key(&key) {
            order.push(key.clone());
        }
        ifaces.entry(key).or_default().error_classes.push(class);
    }

    // Each enum and flags type gets an object in the module of the interface
    // that defines it, and in the module of each interface that `use`s it, under
    // its name in that interface. Interfaces that the world only exports are
    // skipped. If an error class has the same name, only the error class is
    // exported.
    let mut enums: Vec<(Option<&str>, EnumDesc)> = Vec::new();
    for e in wit.iter_enums() {
        enums.extend(enum_desc(e.name(), Type::Enum(e)).map(|desc| (e.interface(), desc)));
    }
    for flags in wit.iter_flags() {
        enums.extend(
            enum_desc(flags.name(), Type::Flags(flags)).map(|desc| (flags.interface(), desc)),
        );
    }
    for alias in wit.iter_aliases() {
        enums.extend(enum_desc(alias.name(), alias.ty()).map(|desc| (alias.interface(), desc)));
    }
    for (interface, desc) in enums {
        if interface.is_some_and(is_export_only) {
            continue;
        }
        let key = interface.map(str::to_string);
        if !ifaces.contains_key(&key) {
            order.push(key.clone());
        }
        let acc = ifaces.entry(key).or_default();
        let taken = |name: &str| {
            acc.error_classes
                .iter()
                .any(|class| class.js_name() == name)
                || acc.enums.iter().any(|e| e.js_name == name)
        };
        if !taken(&desc.js_name) {
            acc.enums.push(desc);
        }
    }

    let mut interfaces = Vec::new();
    for key in order {
        let acc = ifaces.remove(&key).expect("key from discovery order");
        let resources = acc
            .resources
            .into_iter()
            .map(|(name, res)| ResourceTypeDesc {
                // A resource missing from the world's resource list gets the
                // type index `u32::MAX`, which means that it has none.
                type_idx: res.type_idx.unwrap_or(u32::MAX),
                name,
                constructor: res.constructor,
                methods: res.methods,
                statics: res.statics,
            })
            .collect();
        interfaces.push(InterfaceDesc {
            wit_name: key,
            functions: acc.functions,
            resources,
            error_classes: acc.error_classes,
            enums: acc.enums,
        });
    }

    WorldDesc {
        interfaces,
        unexported_error_classes,
    }
}

/// Return the [`EnumDesc`] named `name` for `ty`, following aliases, or `None`
/// if `ty` is neither an enum nor a flags type.
fn enum_desc(name: &str, ty: Type) -> Option<EnumDesc> {
    let members = match dealias(ty) {
        Type::Enum(e) => e
            .names()
            .enumerate()
            .map(|(index, case)| (mangle_name(case), index as i32))
            .collect(),
        // Flags are lifted as an int32, so bit 31 is negative.
        Type::Flags(flags) => flags
            .names()
            .enumerate()
            .map(|(bit, flag)| (mangle_name(flag), (1u32 << bit) as i32))
            .collect(),
        _ => return None,
    };
    Some(EnumDesc {
        js_name: mangle_resource_name(name),
        members,
    })
}

/// The check of the own adapter that converts values to the imported resource
/// type `index`, if one is registered (see
/// [`resources::register_own_adapter`]).
pub(crate) fn own_adapter_check(index: u32) -> Option<resources::OwnAdapterCheck> {
    let resource = wit().resource(index as usize);
    resources::own_adapter(resource.interface(), resource.name()).map(|(_, check)| check)
}

/// The resource type index of an `own<R>` result, if `ty` is one.
fn own_type_idx(ty: Option<&TypeShape>) -> Option<u32> {
    match ty {
        Some(TypeShape::OwnResource { index, .. }) => Some(*index),
        _ => None,
    }
}

/// The resource type index of an `own<R>`/`borrow<R>` handle parameter.
fn handle_type_idx(ty: Option<&TypeShape>) -> Option<u32> {
    match ty {
        Some(TypeShape::OwnResource { index, .. })
        | Some(TypeShape::BorrowResource { index, .. }) => Some(*index),
        _ => None,
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Trap with an error message made from `what` and the pending exception.
pub(crate) fn trap(scope: &Scope<'_>, what: &str) -> ! {
    trap_with(what, ExnThrown::capture(scope))
}

/// Like [`trap`], with the captured exception `captured`.
fn trap_with(what: &str, captured: CapturedError) -> ! {
    match captured.message {
        Some(_) => trap_message(&format!("{what}: {captured}")),
        None => trap_message(what),
    }
}

/// Print `message` on stderr and trap the component call.
pub(crate) fn trap_message(message: &str) -> ! {
    eprintln!("component trapped: {}", message.trim_end());
    std::arch::wasm32::unreachable()
}

/// Trap for the failed lowering function `what`, with the pending exception.
///
/// If an export's result is being lowered, the trap message instead describes
/// the part of the result that can't be lowered (see
/// [`keep_lowering_source`]).
fn lowering_failed(stack: &CallStack, scope: &Scope<'_>, what: &str) -> ! {
    let captured = ExnThrown::capture(scope);
    if let Some((value, index)) = stack.lowering_source(scope) {
        let func = wit().export_func(index);
        if let Some(ty) = func.result() {
            if let Err(mismatch) = crate::validate::check(scope, value, &type_shape(ty)) {
                let mut message = mismatch.describe(&format!(
                    "the result of export `{}`",
                    wit_function_name(func.interface(), func.name())
                ));
                // An `async` export's promise is awaited before its result is
                // lowered, so a promise here is the result of a sync export.
                if mismatch.path.is_empty() && js::Promise::from_jsval(scope, value, ()).is_ok() {
                    message.push_str(
                        ". The function is not `async` in WIT, so it cannot return a promise. \
                         Declare it `async`, or return the value itself.",
                    );
                }
                trap_message(&message);
            }
        }
    }
    trap_with(what, captured)
}

/// Keep the export result on top of `stack` for [`lowering_failed`], unless
/// the export has no result.
fn keep_lowering_source(stack: &mut CallStack, result: &ResultShape, index: usize) {
    if *result != ResultShape::None {
        stack.set_lowering_source(stack.last(), index);
    }
}

/// Return a WIT function's name for messages: `interface#name`, or `name` for a
/// function defined on the world. The functions of the runtime's own
/// `wasi:http/handler` export are named `wasi:http/handler#name`.
fn wit_function_name(interface: Option<&str>, name: &str) -> String {
    let raw = interface.is_some_and(|interface| {
        RAW_HTTP_HANDLER.with(|cell| cell.borrow().as_deref() == Some(interface))
    });
    let interface = if raw {
        Some(HTTP_HANDLER_INTERFACE)
    } else {
        interface
    };
    wit_name_mapping::function_wit_name(interface, name)
}

/// Return `ty` as it is written in WIT, such as `future<list<string>>`. Named
/// types are written as their name, without their interface.
pub(crate) fn wit_type_name(ty: Type) -> String {
    let named = |name: Option<&str>, spelled: &dyn Fn() -> String| match name {
        Some(name) => name.to_string(),
        None => spelled(),
    };
    let optional = |base: &str, ty: Option<Type>| match ty {
        Some(ty) => format!("{base}<{}>", wit_type_name(ty)),
        None => base.to_string(),
    };
    match ty {
        Type::U8 => "u8".to_string(),
        Type::U16 => "u16".to_string(),
        Type::U32 => "u32".to_string(),
        Type::U64 => "u64".to_string(),
        Type::S8 => "s8".to_string(),
        Type::S16 => "s16".to_string(),
        Type::S32 => "s32".to_string(),
        Type::S64 => "s64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::Char => "char".to_string(),
        Type::F32 => "f32".to_string(),
        Type::F64 => "f64".to_string(),
        Type::String => "string".to_string(),
        Type::ErrorContext => "error-context".to_string(),
        Type::Record(record) => record.name().to_string(),
        Type::Own(resource) => resource.name().to_string(),
        Type::Borrow(resource) => format!("borrow<{}>", resource.name()),
        Type::Flags(flags) => flags.name().to_string(),
        Type::Enum(e) => e.name().to_string(),
        Type::Variant(variant) => variant.name().to_string(),
        Type::Alias(alias) => alias.name().to_string(),
        Type::Tuple(tuple) => named(tuple.name(), &|| {
            let types: Vec<String> = tuple.types().map(wit_type_name).collect();
            format!("tuple<{}>", types.join(", "))
        }),
        Type::Option(option) => named(option.name(), &|| {
            format!("option<{}>", wit_type_name(option.ty()))
        }),
        Type::Result(result) => named(result.name(), &|| match (result.ok(), result.err()) {
            (None, None) => "result".to_string(),
            (Some(ok), None) => format!("result<{}>", wit_type_name(ok)),
            (None, Some(err)) => format!("result<_, {}>", wit_type_name(err)),
            (Some(ok), Some(err)) => {
                format!("result<{}, {}>", wit_type_name(ok), wit_type_name(err))
            }
        }),
        Type::List(list) => named(list.name(), &|| {
            format!("list<{}>", wit_type_name(list.ty()))
        }),
        Type::Map(map) => named(map.name(), &|| {
            format!(
                "map<{}, {}>",
                wit_type_name(map.key_type()),
                wit_type_name(map.value_type())
            )
        }),
        Type::FixedLengthList(list) => named(list.name(), &|| {
            format!("list<{}, {}>", wit_type_name(list.ty()), list.len())
        }),
        Type::Future(future) => named(future.name(), &|| optional("future", future.ty())),
        Type::Stream(stream) => named(stream.name(), &|| optional("stream", stream.ty())),
    }
}

js::instance_local! {
    /// The name of the exported interface that has the runtime's own
    /// declaration of `wasi:http/handler` (see [`install_exports`]).
    static RAW_HTTP_HANDLER: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The kind of value left on top of the call stack by phase 1 of an async
/// export call.
enum StackTop {
    /// The value returned by the export. `pending` is set if it is a pending
    /// promise.
    Returned { pending: bool },
    /// The `err` value converted from an exception thrown synchronously.
    Threw,
    /// The value returned for a `future` result, which is lowered as the future
    /// whether or not it has settled.
    Future,
}

/// Pop the lifted arguments off `stack`, and call the export `index` (see
/// [`crate::exports::call_export`]). Traps if the export throws an exception
/// that isn't converted into an `err` value. `name` is the export's WIT name,
/// for the trap message.
fn dispatch_export<'s>(
    scope: &'s Scope<'_>,
    index: usize,
    name: &str,
    stack: &mut CallStack,
    result: &ResultShape,
) -> crate::exports::Outcome<'s> {
    // Each argument is rooted when it is popped. The arguments were pushed in
    // order, so they are popped starting with the last one.
    let mut args: smallvec::SmallVec<[HandleValue<'_>; 8]> =
        smallvec::smallvec![HandleValue::undefined(); stack.len()];
    for arg in args.iter_mut().rev() {
        *arg = scope.root_value(stack.pop_value());
    }

    let resolved = crate::exports::with_export_table(|table| table.resolved(scope, index));
    crate::exports::call_export(scope, resolved, &args, result).unwrap_or_else(|err| {
        let func = wit().export_func(index);
        trap_with(
            &format!(
                "export `{}` threw",
                wit_function_name(func.interface(), name)
            ),
            err,
        )
    })
}

/// Convert the value returned by the async export `func` into an
/// [`Outcome`](crate::exports::Outcome). Traps if the value is a pending
/// promise, or a promise rejected with a reason that can't be converted into an
/// `err` value.
///
/// The event loop also stops when it has no work left, so the promise can
/// still be pending, such as for `await new Promise(() => {})`.
fn settled_outcome<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'s>,
    result: &ResultShape,
    func: ExportFunction,
) -> crate::exports::Outcome<'s> {
    let name = || wit_function_name(func.interface(), func.name());
    match js::Promise::from_jsval(scope, value, ()) {
        Ok(promise) => {
            if promise.is_pending() {
                trap(
                    scope,
                    &format!(
                        "the async export `{}` did not settle: the event loop went idle while \
                         the promise it returned was still pending",
                        name()
                    ),
                )
            }
            crate::exports::settled_promise_outcome(scope, &promise, result).unwrap_or_else(
                |reason| trap(scope, &format!("the async export `{}`: {reason}", name())),
            )
        }
        Err(_) => crate::exports::Outcome::Returned(value),
    }
}

/// Push the result of an export in the form expected by the generated lowering
/// code: nothing if there is no result, the value for a plain result, and a
/// `{tag, val}` object for a `result`.
fn push_export_result(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    result: &ResultShape,
    outcome: crate::exports::Outcome<'_>,
) {
    match result {
        ResultShape::None => {}
        ResultShape::Result { ok, err } => {
            let err = err.is_some();
            let (value, is_err) = match outcome {
                crate::exports::Outcome::Returned(v) => (v, false),
                crate::exports::Outcome::Threw(v) => (v, true),
            };
            let has_payload = if is_err { err } else { *ok };
            if has_payload {
                stack.push_value(value.get());
            }
            if value::push_result(stack, scope, *ok, err, is_err).is_err() {
                trap(scope, "lifting an export result wrapper")
            }
        }
        ResultShape::Plain | ResultShape::Future => match outcome {
            crate::exports::Outcome::Returned(v) => stack.push_value(v.get()),
            // Unreachable: `call_export` already returns `Err` for this.
            crate::exports::Outcome::Threw(_) => trap(scope, "non-result export threw"),
        },
    }
}

mod exports_glue {
    // `export!` emits `wit_dylib_*` intrinsics that take the opaque call-context
    // pointer without an `unsafe` marker, so the raw-pointer-deref lint is allowed
    // here.
    #![allow(clippy::not_unsafe_ptr_arg_deref)]
    use super::StarlingInterpreter;
    wit_dylib_ffi::export!(StarlingInterpreter);
}
