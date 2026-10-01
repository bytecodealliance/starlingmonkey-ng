// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The wasm-only glue between `wit-dylib-ffi` and the conversion core.
//!
//! This module is the only place that names `wit_dylib_ffi` types. It implements
//! [`wit_dylib_ffi::Interpreter`] and [`wit_dylib_ffi::Call`] as thin delegations
//! to `value::*`, converting the `wit_dylib_ffi::Type` metadata it is handed into
//! the crate-local [`TypeShape`] the rest of the crate uses. Keeping those types
//! out of the rest of the crate lets it be tested natively.
//!
//! It compiles only for `wasm32`: `wit_dylib_ffi`'s metadata types wrap
//! `&'static` FFI structs that only the generated bindings library constructs.
//!
//! The `Call` methods receive no [`Scope`], so each opens a throwaway one on the
//! process's entered realm ([`init::with_scope`]) for its single conversion.
//! Values outlive those scopes because the [`CallStack`] is GC-rooted by its own
//! tracer, which also lets `pop_list` → `pop_list_iter_next`×N → `pop_list_iter` span
//! several FFI calls.
//!
//! The `Call` methods also return bare values while the conversions return
//! `Result<_, ExnThrown>`. The generated code calls the next `pop_*` or `push_*`
//! regardless of failure, so there is no sound way to unwind partway through and
//! a conversion error becomes a trap. Import arguments are checked against their
//! types before the call and throw a `TypeError` instead (see
//! [`crate::validate`]). A trap while lowering an export result prints which part
//! of the result failed to lower. The guest-visible error path is
//! `result<_, err>` lowered to `ComponentError`, handled elsewhere.

#![cfg(target_arch = "wasm32")]

use std::alloc::Layout;
use std::cell::RefCell;
use std::ffi::CStr;
use std::rc::Rc;

use crate::component_error::ErrorClass;
use crate::exports::{ExportSpec, ResultShape};

use js::error::{throw_type_error, CapturedError, ExnThrown};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use wit_dylib_ffi::{
    Call, Enum, ExportFunction, Flags, Future, Interpreter, List, Map, Record, Resource, Stream,
    Tuple, Type, Variant, Wit, WitOption, WitResult,
};

use crate::imports::{
    self, EnumDesc, ImportFuncDesc, ImportInvoker, InterfaceDesc, ResourceTypeDesc, WorldDesc,
};
use crate::init;
use crate::naming::InterfaceNames;
use crate::resources::{self, ResourceDesc, HANDLE_FIELD, TYPE_FIELD};
use crate::stack::CallStack;
use crate::value::{self, mangle_name, mangle_resource_name, TypeShape};

// ===========================================================================
// Process-global WIT metadata, stashed by `initialize`
// ===========================================================================

js::instance_local! {
    /// Set by [`StarlingInterpreter::initialize`]. `Wit` is a `Copy` wrapper over
    /// a `&'static` FFI struct, so storing it by value is sound.
    static WIT: RefCell<Option<Wit>> = const { RefCell::new(None) };

    /// The world's imported surface, built from [`WIT`] by
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

/// Panics if the bindings library's ctor has not run.
fn wit() -> Wit {
    WIT.with(|cell| cell.borrow().expect("wit_dylib_initialize has not run yet"))
}

pub fn wit_stream(index: usize) -> Stream {
    wit().stream(index)
}

pub fn wit_future(index: usize) -> Future {
    wit().future(index)
}

/// The resource metadata for `index`, or `None` when `index` is past the end of
/// the world's resource table. Used where the index came from JS, since indexing
/// the table directly would panic, and a panic in a JS callback aborts the
/// instance.
pub fn wit_resource_checked(index: usize) -> Option<Resource> {
    let wit = wit();
    let count = wit.iter_resources().len();
    (index < count).then(|| wit.resource(index))
}

/// Take the handle out of `value` if it is a wrapper of the imported resource
/// `resource` of the interface `interface`, such as `wasi:http/types@0.3.0`'s
/// `request`, or of an interface [`crate::naming::same_interface`] matches with
/// it, and `None` for any other value. The handle leaves the wrapper as
/// when the wrapper is passed to the host as an `own`, so the wrapper can no
/// longer be used.
pub fn take_imported_handle(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    interface: &str,
    resource: &str,
) -> Result<Option<u32>, ExnThrown> {
    let Ok(object) = js::Object::from_value(scope, value) else {
        return Ok(None);
    };
    let Some(type_idx) = resources::read_u32_field(scope, &object, TYPE_FIELD)? else {
        return Ok(None);
    };
    let Some(ty) = wit_resource_checked(type_idx as usize) else {
        return Ok(None);
    };
    let matches = ty
        .interface()
        .is_some_and(|name| crate::naming::same_interface(name, interface));
    if ty.new().is_some() || !matches || ty.name() != resource {
        return Ok(None);
    }
    resources::unwrap_to_canon(scope, value, type_idx, true).map(Some)
}

/// The value a `future<T>` type holds.
pub struct FuturePayload {
    /// The type of `T`, or `None` for a `future` without one.
    pub shape: Option<TypeShape>,
    /// The error class of `T`'s `err` arm, if `T` is a `result` whose `err` arm
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
    /// The reason a synchronous export runs without an event loop, per export index, built on
    /// first use.
    static NO_EVENT_LOOP_REASONS: RefCell<Vec<Option<Rc<str>>>> = const { RefCell::new(Vec::new()) };

    /// The [`FuturePayload`] of each `future` type index, built on first use.
    static FUTURE_PAYLOADS: RefCell<Vec<Option<Rc<FuturePayload>>>> =
        const { RefCell::new(Vec::new()) };

    /// The [`StreamElement`] of each `stream` type index, built on first use.
    static STREAM_ELEMENTS: RefCell<Vec<Option<Rc<StreamElement>>>> =
        const { RefCell::new(Vec::new()) };
}

/// The reason the synchronous export `func`, whose index is `index`, runs without an event loop.
fn no_event_loop_reason(index: usize, func: ExportFunction) -> Rc<str> {
    NO_EVENT_LOOP_REASONS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() <= index {
            cache.resize(index + 1, None);
        }
        cache[index]
            .get_or_insert_with(|| {
                format!(
                    "the synchronous export `{}` runs without one. Declare the export `async` in \
                     WIT to use timers and async imports",
                    wit_function_name(func.interface(), func.name())
                )
                .into()
            })
            .clone()
    })
}

/// The element type of the `stream<T>` with type index `index`.
pub fn stream_element(index: u32) -> Rc<StreamElement> {
    let index = index as usize;
    STREAM_ELEMENTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() <= index {
            cache.resize(index + 1, None);
        }
        cache[index]
            .get_or_insert_with(|| {
                let shape = wit_stream(index).ty().map(type_shape);
                let owns_resources = shape.as_ref().is_some_and(TypeShape::contains_own_resource);
                Rc::new(StreamElement {
                    shape,
                    owns_resources,
                })
            })
            .clone()
    })
}

/// The value a `future<T>` with type index `index` holds.
pub fn future_payload(index: u32) -> Rc<FuturePayload> {
    let index = index as usize;
    FUTURE_PAYLOADS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() <= index {
            cache.resize(index + 1, None);
        }
        cache[index]
            .get_or_insert_with(|| {
                let ty = wit_future(index).ty();
                let shape = ty.map(type_shape);
                let owns_resources = shape.as_ref().is_some_and(TypeShape::contains_own_resource);
                Rc::new(FuturePayload {
                    shape,
                    err_class: err_class_of(ty),
                    owns_resources,
                })
            })
            .clone()
    })
}

/// Add to `classes` the error class of the `err` arm of every `result` whose
/// payloads cross into JS as errors: the one a `future` in `ty` holds, and, with
/// `direct`, `ty` itself, as a function's result.
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

/// The error class of a `result` type's `err` arm, if `ty` is a `result` whose
/// `err` arm has one.
fn err_class_of(ty: Option<Type>) -> Option<ErrorClass> {
    let mut ty = ty?;
    while let Type::Alias(alias) = ty {
        ty = alias.ty();
    }
    let Type::Result(result) = ty else {
        return None;
    };
    let err = result.err()?;
    let (interface, name) = match err {
        Type::Alias(alias) => {
            let mut target = alias.ty();
            while let Type::Alias(inner) = target {
                target = inner.ty();
            }
            if matches!(target, Type::Own(_) | Type::Borrow(_)) {
                return None;
            }
            // An interface the world only exports has no module for the guest
            // to import the class from, so an alias there takes the class of
            // the type it names instead.
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

/// The error class of the named `err` type `err`, or of the type an alias names,
/// following aliases in interfaces the world only exports.
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
    let mut target = err;
    while let Type::Alias(alias) = target {
        target = alias.ty();
    }
    let enum_cases = match target {
        Type::Enum(e) => e.names().map(mangle_name).collect(),
        _ => Vec::new(),
    };
    ErrorClass {
        interface: interface.map(str::to_string),
        name: name.to_string(),
        enum_cases,
    }
}

/// The world's imported surface, or `None` for a world with no exports, whose
/// bindings ctor never runs. Callers treat `None` as the empty world.
pub fn world_desc_opt() -> Option<WorldDesc> {
    WORLD.with(|cell| cell.borrow().clone())
}

/// Resolve every export of the world against `ns`, the main module's namespace,
/// and install the table the export paths dispatch through. Called from
/// [`init::initialize_runtime`]'s `after_evaluate` hook, so an export the main
/// module does not provide fails `init` with a message naming it.
///
/// Each export is looked up in the shapes [`crate::naming::plan`] allows. With
/// `cli`, for a component that exports `wasi:cli/run`, the main module must
/// provide a `run` function, and no other export takes the name `run`.
/// `raw_http_handler` names the interface export with the runtime's
/// `wasi:http/handler` declaration, which the application may implement itself. It is named
/// like `wasi:http/handler` and the main module may leave it out. Returns how
/// the main module provides it, or `None` when `raw_http_handler` is `None`.
///
/// A world with no exports has no bindings ctor and so no metadata, so its table
/// is empty.
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
                Object::from_value(scope, function).is_ok_and(|run| run.is_callable())
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
    let items: Vec<crate::naming::Item> = wit
        .iter_export_funcs()
        .map(|func| {
            let names = func.interface().map(|interface| {
                if Some(interface) == raw_http_handler {
                    InterfaceNames::of(HTTP_HANDLER_INTERFACE)
                } else {
                    InterfaceNames::of(interface)
                }
            });
            (names, crate::exports::export_item(func.name()))
        })
        .collect();
    let fixed: &[&str] = if cli { &["run"] } else { &[] };
    let shapes = crate::naming::plan(&items, fixed)?;
    let mut raw_paths = Vec::new();
    let specs: Vec<ExportSpec> = wit
        .iter_export_funcs()
        .zip(items.into_iter().zip(shapes))
        .map(|(func, ((names, _), shapes))| {
            let raw = raw_http_handler.is_some() && func.interface() == raw_http_handler;
            if raw {
                let item = crate::exports::export_item(func.name());
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

/// How the main module provides the raw `wasi:http/handler` export (see
/// [`install_exports`]).
pub struct RawHttpHandlerExport {
    /// Whether the main module provides it.
    pub provided: bool,
    /// The JS paths it may be provided under, such as `handler.handle`.
    pub paths: Vec<String>,
}

/// The interface whose shapes the raw `wasi:http/handler` export is looked up
/// in. See [`install_exports`].
const HTTP_HANDLER_INTERFACE: &str = "wasi:http/handler";

/// The [`ResultShape`] of a WIT result type, resolving aliases.
fn result_shape(ty: Option<Type>) -> ResultShape {
    let Some(mut ty) = ty else {
        return ResultShape::None;
    };
    while let Type::Alias(a) = ty {
        ty = a.ty();
    }
    match ty {
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

/// The per-call context driving one component call.
///
/// Both paths must use this one type: `export!` emits `wit_dylib_*` symbols that
/// cast the opaque context pointer to `&mut StarlingInterpreter::CallCx`, and
/// `call_import_sync` casts the `&mut impl Call` it is given to that same
/// pointer.
pub enum CallCx<'a> {
    /// The export path owns its stack for the call's duration.
    Owned(CallStack),
    /// The import bridge borrows the stack `call_import_from_js` owns.
    Borrowed(&'a mut CallStack),
}

impl CallCx<'_> {
    fn stack(&mut self) -> &mut CallStack {
        match self {
            CallCx::Owned(stack) => stack,
            CallCx::Borrowed(stack) => stack,
        }
    }

    /// A shared view, for `run_until`'s `should_stop` closure: it peeks the
    /// awaited promise while the surrounding future holds the stack mutably.
    fn stack_ref(&self) -> &CallStack {
        match self {
            CallCx::Owned(stack) => stack,
            CallCx::Borrowed(stack) => stack,
        }
    }
}

impl Interpreter for StarlingInterpreter {
    type CallCx<'a> = CallCx<'a>;

    /// Runs from the bindings library's ctors, before the runtime dylib's ctors
    /// have necessarily run, so SpiderMonkey does not exist yet and this must
    /// stay pure Rust. The JS bootstrap happens later, from the dylib's
    /// `init`-world export.
    fn initialize(wit: Wit) {
        // Stored first, since building the description reads it through `wit()`.
        WIT.with(|cell| *cell.borrow_mut() = Some(wit));
        let world = build_world_desc(wit);
        WORLD.with(|cell| *cell.borrow_mut() = Some(world));
    }

    fn export_start<'a>(_wit: Wit, _func: ExportFunction) -> Box<CallCx<'a>> {
        Box::new(CallCx::Owned(CallStack::new()))
    }

    /// Dispatch one exported call: take the lifted JS arguments off the stack,
    /// invoke the resolved JS export, and push the JS result back for the
    /// generated lowering to consume.
    ///
    /// The export runs with no event loop, so timers and async imports throw a
    /// `TypeError` naming the export. The microtasks it queues run before its
    /// result is lowered.
    fn export_call(_wit: Wit, func: ExportFunction, cx: &mut CallCx<'_>) {
        init::before_export();
        let index = func.index();
        let result = crate::exports::with_export_table(|table| table.result(index));
        let reason = no_event_loop_reason(index, func);

        init::with_scope(|scope| {
            // Drain the arguments and dispatch to the JS export, leaving the JS
            // return value rooted on `scope`.
            let outcome = core_runtime::event_loop::without_event_loop(reason, || {
                let outcome = dispatch_export(scope, index, func.name(), cx.stack(), &result);
                core_runtime::event_loop::run_microtasks(scope);
                outcome
            });

            // Leave the JS result on the stack in the shape the generated
            // lowering expects: nothing for a `()` result, the bare value for a
            // plain type, and a `{tag, val?}` wrapper for a `result<_, _>`, with
            // its payload first and then `push_result`.
            push_export_result(cx.stack(), scope, &result, outcome);
            keep_lowering_source(cx.stack(), &result, index);

            // Release the per-call imported-resource borrows recorded during the
            // call. This MUST happen here, inside `export_call`, not in
            // `export_finish`: dropping an imported borrow invokes its drop
            // callback, which calls the `[resource-drop]` import. The
            // canonical ABI forbids calling imports during post-return, and
            // `export_finish` runs from `cabi_post`, so doing it there traps with
            // "cannot leave component instance". The borrow stack is separate
            // from the value stack, so the just-pushed result is undisturbed, and
            // that result is traced on the stack, so the drop callback's GC
            // cannot lose it.
            if resources::release_borrows(cx.stack(), scope, &drop_exported_handle).is_err() {
                trap(scope, "releasing borrows at export end failed")
            }
        });
    }

    /// Dispatch one async-declared exported call: create a per-call event loop,
    /// call the JS `async function`, drive the loop until the promise it returns
    /// settles, then lower the result via `task-return`.
    ///
    /// There is no `export_start`/`export_finish` pair here. The boxed [`CallCx`]
    /// arrives with the lifted arguments already on its stack, and this future
    /// owns it for the call's whole duration. It runs as a sibling task on the
    /// wit-bindgen executor, so it can `await` the WASIp3 clock while driving.
    ///
    /// Each call gets its own [`OwnedInvocation`], so timers, `fetch` and async
    /// imports are tagged with this loop's id and concurrent calls stay isolated.
    ///
    /// Before any JS runs, the debugger starts a session in an instance resumed
    /// from a snapshot, or connects a `connect` session's client (see
    /// `debugger::before_export`). [`Self::export_call`] does neither, since a
    /// synchronous export cannot block.
    ///
    /// No [`Scope`] can be held across an `await`, so the returned promise stays
    /// alive on the owned [`CallStack`] instead, whose slots a compacting GC
    /// updates in place.
    ///
    /// This returns `-> impl Future` rather than being an `async fn`, to match
    /// the trait's declared return type and avoid the `async_fn_in_trait`
    /// auto-trait-leakage warning.
    #[allow(clippy::manual_async_fn)]
    fn export_call_async(
        wit: Wit,
        func: ExportFunction,
        cx: Box<CallCx<'static>>,
    ) -> impl std::future::Future<Output = ()> {
        async move {
            init::before_export();
            // SAFETY: `initialize_runtime` left the default global's realm entered for the
            // process, and keeps its runtime and context alive for the process.
            unsafe { debugger::before_export(&init::runtime(), init::raw_cx()) };
            let mut cx = cx;
            let _ = wit;
            let index = func.index();
            let result = crate::exports::with_export_table(|table| table.result(index));

            let invocation =
                core_runtime::invocation::OwnedInvocation::new(init::runtime(), Default::default());

            // Phase 1: call the JS export, leaving its value on the GC-traced
            // call stack so it survives the awaits below.
            //
            // The value need not be a promise: a plain `function` exported for an
            // async WIT signature returns a settled value, and a synchronous
            // throw a `result<_, _>` absorbs arrives as `Outcome::Threw`. Both
            // skip the drive loop.
            let name = func.name();
            let pending = init::with_scope(|scope| {
                core_runtime::event_loop::with_event_loop(invocation.state().event_loop(), |_| {
                    let outcome = dispatch_export(scope, index, name, cx.stack(), &result);
                    core_runtime::event_loop::run_microtasks(scope);
                    match outcome {
                        // A returned promise for a `future<T>` result is the
                        // future itself, which is lowered without waiting for it.
                        crate::exports::Outcome::Returned(v)
                            if result == crate::exports::ResultShape::Future =>
                        {
                            cx.stack().push_value(v.get());
                            StackTop::Future
                        }
                        crate::exports::Outcome::Returned(v) => {
                            let pending = !crate::exports::promise_is_settled(scope, v.get());
                            cx.stack().push_value(v.get());
                            // Drive the loop only if the value is a
                            // still-pending promise. A settled promise or a
                            // plain value needs no turn.
                            StackTop::Returned { pending }
                        }
                        crate::exports::Outcome::Threw(v) => {
                            // A synchronous throw the export's `result<_, _>`
                            // absorbs, which is only possible for a plain
                            // non-`async` function. Already settled.
                            cx.stack().push_value(v.get());
                            StackTop::Threw
                        }
                    }
                })
            });

            // Phase 2: drive this call's loop until the promise on top of the
            // stack settles. An already-settled top skips the loop.
            if matches!(pending, StackTop::Returned { pending: true }) {
                crate::run::drive_until_top_settled(init::raw_cx(), &invocation, cx.stack_ref())
                    .await;
            }

            // Phase 3: reduce the settled top of the stack to an `Outcome` per
            // how phase 1 classified it, then leave it in the shape the generated
            // lowering expects.
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
            // fills it, which this call's loop drives, so the loop is active for
            // the lowering. The microtasks the lowering queues, such as the start
            // of a stream the pump reads, are drained before the loop steps.
            core_runtime::event_loop::with_event_loop(invocation.state().event_loop(), |_| {
                func.call_task_return(&mut *cx);
                init::with_scope(core_runtime::event_loop::run_microtasks);
            });

            // A stream or future export returns as soon as it has the rx, while
            // the transfer runs afterwards as `__spawn_promise` futures this loop
            // owns, so the loop must keep stepping until those drain.
            crate::run::drain_async_work(init::raw_cx(), &invocation).await;

            // Any abandoned timer task drops cleanly with the loop.
            drop(invocation);
        }
    }

    /// Drop the owned call context, running its deferred deallocations.
    ///
    /// This runs from `cabi_post`, where calling imports is illegal, so it must
    /// stay import-free. Borrows are released earlier, in
    /// [`export_call`](Self::export_call).
    fn export_finish(cx: Box<CallCx<'_>>, _func: ExportFunction) {
        drop(cx);
    }

    /// Drop an exported resource: remove its backing object from the slab and
    /// run the guest's drop hook if it set one.
    ///
    /// `handle` is the resource's rep, which is the slab key, and `ty` gives the
    /// type index the removed entry must record. [`resources::drop_exported`]
    /// also calls the object's `[Symbol.dispose]()`, if it has one.
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
// The `Call` impl: thin delegation to `value::*`
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
        // The owned `String` goes on the stack's arena so the borrowed `&str`
        // stays valid until the call ends. This takes two steps because the
        // conversion needs a scope and the store a separate `&mut` borrow.
        let owned = self.pop(value::pop_string, "pop_string");
        self.stack().store_string(owned)
    }

    fn pop_borrow(&mut self, ty: Resource) -> u32 {
        with_resource_desc(ty, |desc| {
            self.pop(|s, sc| value::pop_borrow(s, sc, desc), "pop_borrow")
        })
    }
    fn pop_own(&mut self, ty: Resource) -> u32 {
        // An imported resource type with a registered adapter also takes the
        // values the adapter converts.
        let adapter = match ty.new() {
            None => resources::own_adapter(ty.interface(), ty.name()).map(|(adapter, _)| adapter),
            Some(_) => None,
        };
        if let Some(adapter) = adapter {
            let adapted = self.pop(
                |s, sc| {
                    let value = sc.root_value(s.last());
                    if resources::is_resource_wrapper(sc, value)? {
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
            self.pop(|s, sc| value::pop_own(s, sc, desc), "pop_own")
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
        // A `future<T>` lowered out of the guest transfers its readable end.
        let index = ty.index() as u32;
        self.pop(
            |s, sc| crate::promises::future_to_handle(sc, sc.root_value(s.pop_value()), index),
            "pop_future",
        )
    }
    fn pop_stream(&mut self, ty: Stream) -> u32 {
        // A `stream<T>` lowered out of the guest transfers its readable end.
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

    /// Byte fast path: lower a numeric-element list straight from its typed array
    /// into a freshly allocated canonical-ABI buffer. Returns `None` for
    /// non-numeric element types, which fall back to
    /// [`pop_list`](Self::pop_list).
    ///
    /// The returned pointer must be non-null, including for an empty list: the
    /// generated lowering reads a null pointer as "take the iterator path", which
    /// would then panic on the empty `iters` stack this path leaves behind.
    ///
    /// The generated code stores the pointer as the lowered list and never frees
    /// it, so freeing is deferred to the [`CallStack`]'s drop: post-return for an
    /// export, the end of the call for an import. Either way the canonical ABI is
    /// done reading the buffer by then.
    unsafe fn maybe_pop_list(&mut self, ty: List) -> Option<(*const u8, usize)> {
        let elem = numeric_scalar_shape(ty.ty())?;
        // A value other than a typed array, such as a plain `Array`, is lowered
        // element by element through `pop_list`.
        let top = self.stack().last();
        let is_view = init::with_scope(|scope| {
            js::Object::from_value(scope, top)
                .ok()
                .and_then(js::ArrayBufferView::from_object)
                .is_some()
        });
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
                |s, sc| {
                    let dispose = drop_callback_value(sc);
                    value::push_borrow(s, sc, desc, handle, dispose)
                },
                "push_borrow",
            )
        });
    }
    fn push_own(&mut self, ty: Resource, handle: u32) {
        with_resource_desc(ty, |desc| {
            self.push(
                |s, sc| {
                    let dispose = drop_callback_value(sc);
                    value::push_own(s, sc, desc, handle, dispose)
                },
                "push_own",
            )
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

    /// Byte fast path: lift a numeric-element list straight into the matching
    /// typed array. Returns `false` for non-numeric element types, falling back
    /// to [`push_list`](Self::push_list) and
    /// [`list_append`](Self::list_append).
    unsafe fn push_raw_list(&mut self, ty: List, ptr: *mut u8, len: usize) -> bool {
        let Some(elem) = numeric_scalar_shape(ty.ty()) else {
            return false;
        };
        let layout = elem
            .numeric_list_layout(len)
            .expect("a numeric scalar shape has a list layout");
        // Returning `true` takes ownership of the allocation: the generated code
        // skips both its element loop and its trailing `dealloc_bytes`.
        match std::ptr::NonNull::new(ptr).filter(|_| len > 0) {
            Some(data) => self.push(
                // SAFETY: `cabi_realloc`, which made the allocation, and the global
                // allocator are the same allocator, and `layout` matches what
                // `dealloc_bytes` would have reconstructed. It holds `len` valid
                // contiguous elements.
                |s, sc| unsafe { value::push_owned_numeric_list(s, sc, &elem, data, layout, len) },
                "push_raw_list",
            ),
            None => self.push(
                // SAFETY: an empty list reads no elements.
                |s, sc| unsafe { value::push_numeric_list(s, sc, &elem, ptr, 0) },
                "push_raw_list",
            ),
        }
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
    /// Run a fallible lowering conversion under a throwaway scope, trapping on
    /// error.
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

    /// Run a fallible lifting conversion under a throwaway scope, trapping on
    /// error.
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
// The import bridge
// ===========================================================================

/// Drives an import through `wit-dylib`'s generated code, which pops the lowered
/// arguments off the borrowed stack, calls the host, and pushes the lifted result
/// back, leaving exactly one JS value for the caller to take.
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

    fn dispose_callback<'s>(
        &self,
        scope: &'s Scope<'_>,
    ) -> Result<Option<HandleValue<'s>>, ExnThrown> {
        Ok(Some(drop_callback_value(scope)))
    }

    fn supports_async(&self) -> bool {
        true
    }

    /// Drive an `async` import on the wit-bindgen executor.
    ///
    /// The owned `stack` moves into a [`CallCx::Owned`] so it stays GC-traced
    /// across the suspension, and comes back with the lifted result on top.
    ///
    /// The future captures only the `Copy` import index and rebuilds the
    /// `ImportFunction` inside the `async` block, so it borrows nothing from
    /// `&self`. The resulting `'static` bound lets the caller spawn it.
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

/// Synthesize this world's import modules. Passed to
/// [`init::initialize_runtime`] as its `synthesize` closure.
///
/// # Safety
///
/// `core_runtime::module::init_module_loader` must have run first, which
/// `Runtime::init` guarantees.
pub fn synthesize(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    // A world with no exports has no imported surface: its bindings ctor never
    // ran.
    let Some(world) = world_desc_opt() else {
        return Ok(());
    };
    // SAFETY: the module loader is initialized by `Runtime::init`.
    unsafe { imports::synthesize_import_modules(scope, &world, Rc::new(WasmImportInvoker)) }
}

// ===========================================================================
// The shared drop callback
// ===========================================================================

js::instance_local! {
    /// The one shared imported-resource drop callback, built on first use. It is
    /// traced by [`trace_drop_callback`] rather than kept reachable through a
    /// wrapper, since every wrapper holding it may have been collected between
    /// calls.
    static DROP_CALLBACK: RefCell<Option<js::gc::handle::Heap<Value>>> =
        const { RefCell::new(None) };
}

/// The shared drop-callback value, built on first use. It derives the resource
/// type from `this` rather than closing over one, so a single function serves
/// every imported-resource wrapper's
/// [`DISPOSE_FIELD`](crate::resources::DISPOSE_FIELD).
fn drop_callback_value<'s>(scope: &'s Scope<'_>) -> HandleValue<'s> {
    let existing = DROP_CALLBACK.with(|cell| cell.borrow().as_ref().map(|f| f.get(scope)));
    if let Some(v) = existing {
        return v;
    }
    let f = js::Function::new_callback(scope, c"_componentizeJsDrop", 0, drop_callback, ())
        .expect("building the drop callback failed");
    let val = f.as_value();
    DROP_CALLBACK.with(|cell| *cell.borrow_mut() = Some(js::gc::handle::Heap::from(val)));
    scope.root_value(val)
}

/// Drop the cached drop callback. See [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    DROP_CALLBACK.with(|cell| *cell.borrow_mut() = None);
}

/// Keep the shared drop callback alive.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_drop_callback(trc: *mut js::native::JSTracer) {
    use js::heap::Trace;
    DROP_CALLBACK.with(|cell| {
        if let Some(heap) = &*cell.as_ptr() {
            heap.trace(trc);
        }
    });
}

/// `this[Symbol.dispose]()`, and the finalizer's `this[DISPOSE_FIELD]()`: drop
/// the host-owned resource `this` wraps.
///
/// Unregisters `this` from finalization, so a later collection cannot
/// double-drop, then calls the wit-dylib resource `drop`. A `this` whose handle
/// was already stripped is a no-op, so a second dispose does nothing. A `this`
/// with a handle that is not a registered wrapper throws a `TypeError`. A `this`
/// lent to an import call that has not returned is dropped once the last such
/// call returns (see [`resources::LENDS_FIELD`]).
fn drop_callback(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    if let (Some(type_idx), Some(handle)) = (
        resources::read_u32_field(scope, &this, TYPE_FIELD)?,
        resources::read_u32_field(scope, &this, HANDLE_FIELD)?,
    ) {
        if resources::read_u32_field(scope, &this, resources::LENDS_FIELD)?.unwrap_or(0) > 0 {
            this.define_property(
                scope,
                resources::DISPOSE_DEFERRED_FIELD,
                true,
                resources::HIDDEN_FIELD_ATTRS,
            )?;
            return Ok(js::value::undefined());
        }
        // `this` can be any object with the hidden fields, so the type index is
        // checked, and only an object the finalization registry holds, which
        // every wrapper with a handle is, has its handle dropped.
        let res = wit_resource_checked(type_idx as usize)
            .ok_or_else(|| throw_type_error(scope, c"resource type index is out of range"))?;
        if !resources::unregister_resource(scope, &this)? {
            return Err(throw_type_error(
                scope,
                c"dispose was called on an object that is not a resource wrapper",
            ));
        }
        // SAFETY: `handle` is the live canonical handle read off `this`.
        unsafe { res.drop()(handle) };
        // A second dispose, and a method call on the stashed wrapper, now find no
        // handle rather than passing the released one back to the host.
        this.delete_property(scope, HANDLE_FIELD)?;
    }
    Ok(js::value::undefined())
}

// ===========================================================================
// `wit_dylib_ffi::Type` -> `TypeShape` and `Wit` -> `WorldDesc`
// ===========================================================================

/// Convert a `wit_dylib_ffi::Type` into the crate-local [`TypeShape`], mangling
/// field, case and flag names so the conversion core uses them verbatim.
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

/// A record's mangled field names, in declaration order, built once per record
/// type and shared by every later call. The value intrinsics need the names
/// alone, not the field types.
fn record_field_names(r: Record) -> Rc<[&'static CStr]> {
    RECORD_NAMES.with(|cell| {
        cached(&mut cell.borrow_mut(), r.index(), || {
            r.fields()
                .map(|(name, _)| static_name(&mangle_name(name)))
                .collect()
        })
    })
}

/// A variant's mangled case names paired with whether each case has a payload,
/// in declaration order, built once per variant type.
fn variant_cases(v: Variant) -> Rc<[(&'static CStr, bool)]> {
    VARIANT_CASES.with(|cell| {
        cached(&mut cell.borrow_mut(), v.index(), || {
            v.cases()
                .map(|(name, ty)| (static_name(&mangle_name(name)), ty.is_some()))
                .collect()
        })
    })
}

/// `name` as a NUL-terminated string that lives for the rest of the process. The per-type caches
/// hold their names for the rest of the instance anyway, and the value intrinsics key their
/// property-key cache on the address of a `'static` name.
fn static_name(name: &str) -> &'static CStr {
    Box::leak(value::to_cstring(name).into_boxed_c_str())
}

/// The entry at `index` of a per-type-index cache, built with `build` on the
/// first request.
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

/// The [`TypeShape`] of `ty` if it is a numeric scalar after resolving aliases,
/// so a list's typed-array path is chosen without building the shape of a
/// composite element.
fn numeric_scalar_shape(mut ty: Type) -> Option<TypeShape> {
    while let Type::Alias(a) = ty {
        ty = a.ty();
    }
    let shape = match ty {
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
    debug_assert!(shape.numeric_layout().is_some());
    Some(shape)
}

/// Whether an option's payload type is itself an option, after resolving
/// aliases. Nested options take the `{val}` wrapper that tells `some(none)` apart
/// from `none`, and that is all the option intrinsics need to know.
fn inner_is_option(o: WitOption) -> bool {
    let mut ty = o.ty();
    while let Type::Alias(a) = ty {
        ty = a.ty();
    }
    matches!(ty, Type::Option(_))
}

/// Run `f` with the crate-local [`ResourceDesc`] for a single lift/lower of a
/// `wit_dylib_ffi::Resource`.
///
/// A resource with `new` and `rep` operations is exported, meaning guest-owned
/// and slab-backed. Any other resource is imported, meaning host-owned.
///
/// The `new` and `rep` wrappers over the wit-dylib externs are built as locals
/// here and borrowed into the `Exported` descriptor, so they live exactly as
/// long as `f` runs, with no per-operation allocation or leak.
fn with_resource_desc<R>(ty: Resource, f: impl FnOnce(&ResourceDesc<'_>) -> R) -> R {
    let type_idx = ty.index() as u32;
    match (ty.new(), ty.rep()) {
        (Some(new), Some(rep)) => {
            let drop = ty.drop();
            // SAFETY: `new`, `rep` and `drop` are the wit-dylib resource-table
            // externs for this type, taking and returning the canonical handle
            // and rep.
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

/// Release a canonical handle the interpreter minted for a guest-owned resource
/// borrowed to the host, as [`resources::release_borrows`] calls for.
fn drop_exported_handle(type_idx: u32, handle: u32) {
    let res = wit().resource(type_idx as usize);
    // SAFETY: `handle` is the handle this call minted through the same type's
    // `[resource-new]`, and the caller has already removed its slab entry.
    unsafe { res.drop()(handle) };
}

/// Build the world's imported surface from `Wit` metadata. This is pure Rust and
/// safe to call from `initialize` before SpiderMonkey exists.
///
/// Imports are grouped by interface, plus a world-level group for imports defined
/// on the world directly, then each interface's functions are classified into
/// freestanding functions and per-resource constructor, method and static lists.
/// Only imported resources get a synthesized class. A resource named by an
/// import function's `[constructor]`, `[method]` or `[static]` prefix is
/// host-owned.
fn build_world_desc(wit: Wit) -> WorldDesc {
    use std::collections::{BTreeMap, BTreeSet};

    // Per-interface accumulator, keyed by the interface's WIT specifier, with
    // `None` for world-level imports.
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

    // Preserve interface discovery order so the synthesized modules are built in
    // a stable order. The key is the raw specifier string.
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
            // `AsyncFilterSet::default()` binds an `async`-declared WIT import
            // as async, so its `async_impl` thunk is present. The synthesized
            // callback then returns a promise and spawns a loop-driven future for
            // it.
            is_async: import.is_async(),
            err_class: err_class_of(import.result()),
        };
        let name = import.name();

        if let Some(ty) = name.strip_prefix("[constructor]") {
            let res = acc.resources.entry(ty.to_string()).or_default();
            // A constructor's result is `own<R>`, giving the resource type index.
            res.type_idx = res.type_idx.or_else(|| own_type_idx(func.result.as_ref()));
            res.constructor = Some(func);
        } else if let Some(rest) = name.strip_prefix("[method]") {
            let (ty, member) = rest
                .split_once('.')
                .expect("[method] name is `type.member`");
            let res = acc.resources.entry(ty.to_string()).or_default();
            // A method's first parameter is `borrow<R>` or `own<R>`, giving the
            // type.
            res.type_idx = res
                .type_idx
                .or_else(|| handle_type_idx(func.params.first()));
            res.methods.push((mangle_name(member), func));
        } else if let Some(rest) = name.strip_prefix("[static]") {
            let (ty, member) = rest
                .split_once('.')
                .expect("[static] name is `type.member`");
            let res = acc.resources.entry(ty.to_string()).or_default();
            res.statics.push((mangle_name(member), func));
        } else {
            acc.functions.push(func);
        }
    }
    // An imported resource that no constructor, method or static names gets a
    // class too, so its wrappers have `[Symbol.dispose]()` and its module exports
    // the class. Only an exported resource has a `new` intrinsic.
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
    // A class whose type an exported interface defines has no import module to
    // be exported from. Every other class is exported from the module of its
    // interface, or the world-level module, which exist even when they have no
    // import functions.
    let exported: BTreeSet<&str> = wit
        .iter_export_funcs()
        .filter_map(|export| export.interface())
        .collect();
    for class in error_classes {
        let key = class.interface.clone();
        if key.as_deref().is_some_and(|name| exported.contains(name)) && !ifaces.contains_key(&key)
        {
            continue;
        }
        if !ifaces.contains_key(&key) {
            order.push(key.clone());
        }
        ifaces.entry(key).or_default().error_classes.push(class);
    }

    // Each enum and flags type gets an object in the module of the interface
    // defining it, and in that of each interface `use`ing it, under the name the
    // interface gives it. An interface whose functions the world only exports
    // has no module, and an error class of the same name takes the name.
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
                // Every imported resource has its type index from the world's
                // resource list. A resource missing from it gets the `u32::MAX`
                // sentinel, which is never a real type index, so synthesis skips
                // registering a prototype for it. Falling back to `0` would
                // clobber the prototype of whatever real resource occupies
                // index 0.
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

    WorldDesc { interfaces }
}

/// The [`EnumDesc`] of `ty`, following aliases, named `name`, or `None` if `ty`
/// is neither an enum nor a flags type.
fn enum_desc(name: &str, mut ty: Type) -> Option<EnumDesc> {
    while let Type::Alias(alias) = ty {
        ty = alias.ty();
    }
    let members = match ty {
        Type::Enum(e) => e
            .names()
            .enumerate()
            .map(|(index, case)| (mangle_name(case), index as i32))
            .collect(),
        // Flags cross as an int32, so bit 31 is negative.
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

/// Capture the pending exception and trap the component call.
///
/// A conversion error inside a `Call` method cannot unwind cleanly, because the
/// generated code calls the next intrinsic regardless, so it becomes a trap with
/// a human-readable message.
pub(crate) fn trap(scope: &Scope<'_>, what: &str) -> ! {
    trap_with(what, ExnThrown::capture(scope))
}

/// Trap with `captured`, an exception already taken off the context. See [`trap`].
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

/// Trap for a lowering conversion `what` that failed, with the exception it left
/// pending.
///
/// While the result of an export is lowered, the stack keeps the result (see
/// [`keep_lowering_source`]), which is checked against the export's result type
/// to name the part that failed to lower and why.
fn lowering_failed(stack: &CallStack, scope: &Scope<'_>, what: &str) -> ! {
    let captured = ExnThrown::capture(scope);
    if let Some((value, index)) = stack.lowering_source() {
        let func = wit().export_func(index);
        let value = scope.root_value(value);
        if let Some(ty) = func.result() {
            if let Err(mismatch) = crate::validate::check(scope, value, &type_shape(ty)) {
                let mut message = mismatch.describe(&format!(
                    "the result of export `{}`",
                    wit_function_name(func.interface(), func.name())
                ));
                // An `async` export's promise is awaited before its result is
                // lowered, so a promise here is the result of a sync export.
                if mismatch.path.is_empty()
                    && crate::exports::as_promise(scope, value.get()).is_some()
                {
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
        let top = stack.last();
        stack.set_lowering_source(top, index);
    }
}

/// A WIT function's name for a message: `interface#name`, or the bare name for
/// a function the world declares.
fn wit_function_name(interface: Option<&str>, name: &str) -> String {
    match interface {
        Some(interface)
            if RAW_HTTP_HANDLER.with(|cell| cell.borrow().as_deref() == Some(interface)) =>
        {
            format!("wasi:http/handler#{name}")
        }
        Some(interface) => format!("{interface}#{name}"),
        None => name.to_string(),
    }
}

js::instance_local! {
    /// The name of the export the application implements `wasi:http/handler`
    /// through, which messages name `wasi:http/handler` (see [`install_exports`]).
    static RAW_HTTP_HANDLER: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// How the async export driver's phase 1 left the top of the call stack, so
/// phase 3 can reduce it to an [`Outcome`](crate::exports::Outcome).
enum StackTop {
    /// A value the JS call returned. `pending` is `true` only for a promise the
    /// drive loop must still settle.
    Returned { pending: bool },
    /// The `err` payload of a synchronous throw the export's `result<_, _>`
    /// absorbed, reachable only from a plain non-`async` function.
    Threw,
    /// The value returned for a `future<T>` result, lowered as the future
    /// whether or not it has settled.
    Future,
}

/// Drain the lifted arguments off `stack` and call export `index` through the
/// table [`install_exports`] built, returning the JS result rooted on `scope`.
/// For an async export that result is the pending `Promise`, which the caller
/// awaits. `name` is the WIT name, for the trap message.
fn dispatch_export<'s>(
    scope: &'s Scope<'_>,
    index: usize,
    name: &str,
    stack: &mut CallStack,
    result: &ResultShape,
) -> crate::exports::Outcome<'s> {
    // Root each argument as it comes off, since a later allocation could
    // invalidate it. They were pushed left-to-right, so they are popped into
    // the slots from the last one down.
    // `call_export` does the `[method]` `this = args[0]` split itself.
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

/// Reduce the settled value the async export `func` returned to an [`Outcome`],
/// trapping on an unabsorbable rejection.
///
/// `run_until` also returns when the loop goes idle, so a guest that `await`s a
/// never-resolving promise (`await new Promise(() => {})`) reaches here still
/// `Pending`. Trapping on that establishes
/// [`settled_promise_outcome`](crate::exports::settled_promise_outcome)'s
/// settled-promise precondition.
fn settled_outcome<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'s>,
    result: &ResultShape,
    func: ExportFunction,
) -> crate::exports::Outcome<'s> {
    let name = wit_function_name(func.interface(), func.name());
    match crate::exports::as_promise(scope, value) {
        Some(promise) => {
            if promise.is_pending() {
                trap(
                    scope,
                    &format!(
                        "the async export `{name}` did not settle: the event loop went idle \
                         while the promise it returned was still pending"
                    ),
                )
            }
            crate::exports::settled_promise_outcome(scope, &promise, result).unwrap_or_else(
                |reason| trap(scope, &format!("the async export `{name}`: {reason}")),
            )
        }
        None => crate::exports::Outcome::Returned(value),
    }
}

/// Leave the JS export result on the stack in the shape the generated lowering
/// expects: nothing for `()`, the bare value for a plain type, and for
/// `result<ok, err>` the payload of the matched arm followed by a `{tag, val?}`
/// wrapper.
fn push_export_result(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    result: &ResultShape,
    outcome: crate::exports::Outcome<'_>,
) {
    match result {
        // A `()` result lowers nothing.
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

// `export!` emits `wit_dylib_*` intrinsics that take the opaque call-context
// pointer without an `unsafe` marker, so the raw-pointer-deref lint is allowed
// here. `#[no_mangle]` keeps the symbols linkable from inside this submodule.
mod exports_glue {
    #![allow(clippy::not_unsafe_ptr_arg_deref)]
    use super::StarlingInterpreter;
    wit_dylib_ffi::export!(StarlingInterpreter);
}
