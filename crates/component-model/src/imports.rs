// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The JavaScript→host import path and native synthesis of import modules.
//!
//! A componentized application imports WIT functions and resources by importing
//! them from ES modules named after their WIT interface (e.g.
//! `import { f } from "test:iface/x"`).
//!
//! Those modules are synthesized natively at init time
//! ([`synthesize_import_modules`]): each export is a [`Function::new_callback`]
//! whose callback runs [`dispatch_import`], which routes to the sync or async
//! host-call path by the import's declared mode. A synthesized module holds a
//! function per freestanding import and a class per resource, with constructor,
//! methods, statics and a hidden drop callback.
//!
//! Lowering the JS arguments, calling the host and lifting the result back is
//! driven by `wit-dylib`'s generated code, which exists only in the dylib. To
//! keep the path natively testable it goes through a pluggable [`ImportInvoker`],
//! and the world's structure is described by a crate-local [`WorldDesc`], so this
//! module never names a wit-dylib type. [`call_import_from_js`] and
//! [`call_import_from_js_async`] only push the raw JS arguments onto a
//! [`CallStack`] and pop the single lifted result off it. Everything in between
//! happens inside the invoker.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js::error::{throw_type_error, ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::component_error::ErrorClass;
use crate::stack::CallStack;
use crate::value::{mangle_name, mangle_resource_name, to_cstring, TypeShape};

/// A crate-local description of a component's imported surface.
///
/// Built from `Wit` metadata by the wasm interpreter glue and by hand in native
/// tests. Interfaces and functions are named in their WIT form, and
/// [`synthesize_import_modules`] mangles them to JS names.
#[derive(Clone, Debug, Default)]
pub struct WorldDesc {
    /// One entry per imported interface, plus at most one world-level entry
    /// (whose [`InterfaceDesc::wit_name`] is `None`) for imports defined
    /// directly on the world.
    pub interfaces: Vec<InterfaceDesc>,
    /// The error classes of types that interfaces the world only exports
    /// define. No module exports them, and [`synthesize_import_modules`] creates
    /// them with the others.
    pub unexported_error_classes: Vec<ErrorClass>,
}

/// One imported interface (or the world-level import group).
#[derive(Clone, Debug, Default)]
pub struct InterfaceDesc {
    /// The interface's WIT specifier (`"test:iface/x@0.2.0"`), used verbatim as
    /// the synthesized module's name, or `None` for world-level imports, which go
    /// in the `external` module.
    pub wit_name: Option<String>,
    /// Freestanding imported functions on this interface.
    pub functions: Vec<ImportFuncDesc>,
    /// Imported resource types on this interface.
    pub resources: Vec<ResourceTypeDesc>,
    /// The error classes this interface's module exports: those whose types the
    /// interface defines.
    pub error_classes: Vec<ErrorClass>,
    /// The enum and flags types this interface's module exports objects for:
    /// those the interface defines or `use`s.
    pub enums: Vec<EnumDesc>,
}

/// An enum or flags type, which its interface's module exports as a frozen
/// object. The object maps each case's JS name to its value, and each value
/// back to the name, as a TypeScript numeric enum does.
#[derive(Clone, Debug)]
pub struct EnumDesc {
    /// The name the module exports the object under: the type's name in the
    /// interface, upper-camel-cased.
    pub js_name: String,
    /// Each case's JS name and value. An enum case's value is its index, and a
    /// flag's is its bit, as an int32.
    pub members: Vec<(String, i32)>,
}

/// One imported function: its global import index plus its WIT signature.
#[derive(Clone, Debug)]
pub struct ImportFuncDesc {
    /// The function's index among all of the component's import functions, stored
    /// in the synthesized callback's payload slot and used to look the function
    /// back up in the registry.
    pub index: u32,
    /// The WIT-level name, possibly with a `[constructor]`, `[method]` or
    /// `[static]` prefix. Used only to classify the function during synthesis.
    pub name: String,
    /// Parameter types, in declaration order. For a `[method]`, the first
    /// parameter is the resource's `self` (a `borrow`).
    pub params: Vec<TypeShape>,
    /// The result type, or `None` for a function returning nothing.
    pub result: Option<TypeShape>,
    /// For a `result` whose `err` arm has a class of its own, that class.
    pub err_class: Option<ErrorClass>,
    /// Whether this import is `async`-declared in WIT, selecting
    /// [`call_import_from_js_async`] over [`call_import_from_js`].
    pub is_async: bool,
}

/// One imported resource type and the import functions implementing its
/// constructor, methods, and statics.
#[derive(Clone, Debug)]
pub struct ResourceTypeDesc {
    /// The resource type index, stored on the wrappers this synthesis creates.
    pub type_idx: u32,
    /// The WIT resource name (`"my-resource"`), upper-camel-cased to the JS class
    /// name during synthesis.
    pub name: String,
    /// The import implementing `[constructor]name`, if any.
    pub constructor: Option<ImportFuncDesc>,
    /// The imports implementing `[method]name.m`, each paired with the method's
    /// JS name, which is the already-split part after the `.`.
    pub methods: Vec<(String, ImportFuncDesc)>,
    /// The imports implementing `[static]name.m`, paired with the method's JS
    /// name.
    pub statics: Vec<(String, ImportFuncDesc)>,
}

/// The host side of an import call: pop the lowered arguments off the stack,
/// call the host, and push the lifted result back.
///
/// The implementor receives the stack with the raw JS arguments already pushed,
/// first argument on top, and must leave exactly one lifted result value on top
/// when it returns.
pub trait ImportInvoker {
    /// Drive one synchronous import call.
    fn invoke(
        &self,
        scope: &Scope<'_>,
        stack: &mut CallStack,
        func: &ImportFuncDesc,
    ) -> Result<(), ExnThrown>;

    /// Whether this invoker implements [`invoke_async`](Self::invoke_async).
    /// [`call_import_from_js_async`] checks this before dispatching, so a
    /// sync-only invoker paired with an async descriptor throws rather than
    /// reaching the default `invoke_async`'s panic.
    fn supports_async(&self) -> bool {
        false
    }

    /// Drive one async-declared import call.
    ///
    /// The host call may suspend, so this owns the [`CallStack`] across the await,
    /// with the live-stacks registry keeping its values GC-traced throughout, and
    /// hands it back with exactly one lifted result value on top.
    ///
    /// The returned future must be `'static`, so it must not borrow `&self`, and
    /// [`call_import_from_js_async`] can spawn it onto the active event loop where
    /// it outlives the synthesized callback that started it.
    ///
    /// The default panics. It is unreachable from guest dispatch, which rejects on
    /// [`supports_async`](Self::supports_async), and is a backstop against a
    /// direct mis-call.
    fn invoke_async(
        &self,
        stack: CallStack,
        func: Rc<ImportFuncDesc>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CallStack> + 'static>> {
        let _ = (stack, func);
        panic!("invoke_async called on an invoker whose supports_async() is false");
    }

    /// Release a canonical handle minted for a guest-owned resource that this
    /// call borrowed to the host, given the resource's type index.
    ///
    /// [`resources::release_borrows`] calls this once per exported borrow when
    /// the call ends. The default does nothing, for an invoker whose world has no
    /// exported resources.
    fn drop_exported_handle(&self, type_idx: u32, handle: u32) {
        let _ = (type_idx, handle);
    }
}

/// One synthesized world's import state: every import function by index, plus
/// the invoker that drives them.
struct ImportRegistry {
    /// Indexed by import-function index (the callback payload). `None` slots are
    /// never dispatched to. Each descriptor is behind an `Rc`, so dispatching a
    /// call hands out a handle rather than deep-cloning the name, the parameter
    /// shapes and the result shape.
    funcs: Vec<Option<Rc<ImportFuncDesc>>>,
    /// The invoker shared by every import in this world.
    invoker: Rc<dyn ImportInvoker>,
}

js::instance_local! {
    /// The process's single synthesized-world import registry, set by
    /// [`synthesize_import_modules`].
    static IMPORTS: RefCell<Option<ImportRegistry>> = const { RefCell::new(None) };

    /// Whether calling an import throws, set by [`set_imports_unavailable`].
    static IMPORTS_UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
}

/// Make every import call throw a `TypeError` while `unavailable`, as it is
/// while the application initializes before the component is wizened.
pub fn set_imports_unavailable(unavailable: bool) {
    IMPORTS_UNAVAILABLE.with(|cell| cell.set(unavailable));
}

/// Throw if imports are unavailable (see [`set_imports_unavailable`]).
fn check_imports_available(scope: &Scope<'_>, func: &ImportFuncDesc) -> Result<(), ExnThrown> {
    if !IMPORTS_UNAVAILABLE.with(Cell::get) {
        return Ok(());
    }
    Err(js::error::TypeError(format!(
        "the import `{}` was called while the application initializes, before the component \
         is wizened, where imports are unavailable. Call it from an export instead",
        js_import_name(&func.name)
    ))
    .throw(scope))
}

/// Look up an import descriptor and the shared invoker by index. Both come back
/// as `Rc` handles, so neither is copied.
fn lookup_import(index: u32) -> Option<(Rc<ImportFuncDesc>, Rc<dyn ImportInvoker>)> {
    IMPORTS.with(|cell| {
        let borrow = cell.borrow();
        let reg = borrow.as_ref()?;
        let func = reg.funcs.get(index as usize)?.clone()?;
        Some((func, Rc::clone(&reg.invoker)))
    })
}

/// [`lookup_import`], throwing a `TypeError` for an unregistered index.
fn lookup_import_or_throw(
    scope: &Scope<'_>,
    index: u32,
) -> Result<(Rc<ImportFuncDesc>, Rc<dyn ImportInvoker>), ExnThrown> {
    lookup_import(index)
        .ok_or_else(|| throw_type_error(scope, c"import function index is not registered"))
}

/// Install the import registry for the synthesized world. Replaces any previous
/// registry (a fresh process bootstraps exactly once).
fn set_import_registry(funcs: Vec<Option<Rc<ImportFuncDesc>>>, invoker: Rc<dyn ImportInvoker>) {
    IMPORTS.with(|cell| *cell.borrow_mut() = Some(ImportRegistry { funcs, invoker }));
}

/// Invoke an imported function from guest JavaScript.
///
/// `index` is the import-function index, read from the calling callback's
/// payload. `args` are the JS argument values the guest passed. For a method the
/// synthesized method callback has already prepended the receiver as `args[0]`.
/// The returned [`Value`] is the lifted JS result and is not rooted: the callback
/// trampoline hands it straight back to SpiderMonkey as the function's return
/// value, so it is consumed before any further allocation.
pub fn call_import_from_js(
    scope: &Scope<'_>,
    index: u32,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let (func, invoker) = lookup_import_or_throw(scope, index)?;
    call_import(scope, &func, invoker.as_ref(), args)
}

/// [`call_import_from_js`] for an import already looked up.
fn call_import(
    scope: &Scope<'_>,
    func: &ImportFuncDesc,
    invoker: &dyn ImportInvoker,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    check_imports_available(scope, func)?;
    let mut stack = push_import_args(scope, func, args)?;

    // The result is rooted while the borrows are released, which runs JS.
    let result = invoker
        .invoke(scope, &mut stack, func)
        .and_then(|()| {
            handle_import_result(
                scope,
                &mut stack,
                func.result.as_ref(),
                func.err_class.as_ref(),
            )
        })
        .map(|value| scope.root_value(value));
    release_call_borrows(&mut stack, scope, invoker);
    result.map(|value| value.get())
}

/// Check `args` against `func`'s parameters and push them onto a new call stack,
/// in reverse so the first argument is on top, matching the order the lowering
/// `pop_*` conversions consume them.
///
/// A wrong number of arguments, or an argument that does not lower to its
/// parameter's type, throws a `TypeError` naming the import and the argument,
/// before anything is lowered.
fn push_import_args(
    scope: &Scope<'_>,
    func: &ImportFuncDesc,
    args: &[HandleValue<'_>],
) -> Result<CallStack, ExnThrown> {
    let is_method = func.name.starts_with("[method]");
    if args.len() != func.params.len() {
        let expected = func.params.len().saturating_sub(usize::from(is_method));
        return Err(js::error::TypeError(format!(
            "`{}` takes {expected} argument{}, got {}",
            js_import_name(&func.name),
            if expected == 1 { "" } else { "s" },
            args.len().saturating_sub(usize::from(is_method)),
        ))
        .throw(scope));
    }
    // Each argument is lowered from a copy, so a getter that returns something
    // else on a second read cannot make the lowering trap. The resource wrappers
    // the copies hold are checked after all arguments were read, so a getter
    // cannot dispose of one between its check and the lowering.
    let copies = crate::validate::snapshot_arguments(scope, args, &func.params).map_err(
        |(index, mismatch)| {
            let what = match (is_method, index) {
                (true, 0) => format!("the receiver of `{}`", js_import_name(&func.name)),
                (true, _) => format!("argument {index} of `{}`", js_import_name(&func.name)),
                (false, _) => {
                    format!("argument {} of `{}`", index + 1, js_import_name(&func.name))
                }
            };
            js::error::TypeError(mismatch.describe(&what)).throw(scope)
        },
    )?;
    let mut stack = CallStack::new();
    for copy in copies.iter().rev() {
        stack.push_value(copy.get());
    }
    Ok(stack)
}

/// An import's WIT function name as the guest calls it: `thing.method` for a
/// resource's method or static function, `new thing` for its constructor, and
/// the name itself otherwise.
pub(crate) fn js_import_name(wit_name: &str) -> String {
    if let Some(ty) = wit_name.strip_prefix("[constructor]") {
        format!("new {}", crate::value::mangle_resource_name(ty))
    } else if let Some(rest) = wit_name
        .strip_prefix("[method]")
        .or_else(|| wit_name.strip_prefix("[static]"))
    {
        match rest.split_once('.') {
            Some((ty, member)) => format!(
                "{}.{}",
                crate::value::mangle_resource_name(ty),
                crate::value::mangle_name(member)
            ),
            None => rest.to_string(),
        }
    } else {
        crate::value::mangle_name(wit_name)
    }
}

/// Release the borrows an import call recorded, once the host call has returned
/// and its result has been lifted.
///
/// A guest-owned resource lowered as a `borrow` argument holds a canonical handle
/// and a slab pin only for the duration of the call, so both are given up here.
///
/// This runs on the failing path too, where an exception is already pending. That
/// exception is set aside while the drops run, since they call back into JS, and
/// restored afterwards. A failure in a drop is reported and dropped, since the
/// call's own outcome outranks it.
fn release_call_borrows(stack: &mut CallStack, scope: &Scope<'_>, invoker: &dyn ImportInvoker) {
    let pending = js::exception::take_pending(scope);

    if crate::resources::release_borrows(stack, scope, &|type_idx, handle| {
        invoker.drop_exported_handle(type_idx, handle)
    })
    .is_err()
    {
        js::exception::report_and_clear(scope, "releasing an import call's borrows");
    }

    if let Some(exn) = pending {
        js::exception::set_pending(scope, exn, js::native::ExceptionStackBehavior::Capture);
    }
}

/// Invoke an async-declared imported function from guest JavaScript.
///
/// Returns a pending JS promise immediately and spawns a Rust future onto the
/// active event loop that settles it once the host subtask completes. The future
/// is attributed to whichever loop is active when the guest calls the import, so
/// an async export calling an async import gets the export's own `run_until`
/// drive. A call with no loop active, one while imports are unavailable, and one
/// with an argument that does not lower return a promise rejected with a
/// `TypeError` explaining why.
///
/// The arguments are lowered before this returns, so JavaScript that runs after
/// the call cannot dispose of, move or change what the host receives.
///
/// An `err` arm rejects the promise with the same `ComponentError` the sync path
/// throws.
pub fn call_import_from_js_async(
    scope: &Scope<'_>,
    index: u32,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let (func, invoker) = lookup_import_or_throw(scope, index)?;
    call_import_async(scope, func, invoker, args)
}

/// [`call_import_from_js_async`] for an import already looked up.
fn call_import_async(
    scope: &Scope<'_>,
    func: Rc<ImportFuncDesc>,
    invoker: Rc<dyn ImportInvoker>,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    use js::promise::{PromiseFuture, PromiseOutcome};

    // The mismatch is rejected here rather than spawning a future that would hit
    // `invoke_async`'s default panic when polled, since a panic unwinding across
    // the FFI boundary is undefined behaviour.
    if !invoker.supports_async() {
        return Err(throw_type_error(
            scope,
            c"async import dispatched to an invoker that does not support async imports",
        ));
    }

    let promise = js::Promise::new_pending(scope)?;
    // A call that cannot start rejects the promise with the `TypeError`
    // explaining why, rather than throwing, so a `.catch` on the call's result
    // handles it.
    let stack = (|| {
        check_imports_available(scope, &func)?;
        // Only an event loop drives the call's future.
        if core_runtime::event_loop::with_active_event_loop(|_| ()).is_none() {
            let what = format!("the async import `{}`", js_import_name(&func.name));
            return Err(core_runtime::event_loop::throw_no_event_loop(scope, &what));
        }
        push_import_args(scope, &func, args)
    })();
    let stack = match stack {
        Ok(stack) => stack,
        Err(ExnThrown) => {
            promise.reject_with_pending(scope)?;
            return Ok(promise.as_value());
        }
    };

    let result_ty = func.result.clone();
    let err_class = func.err_class.clone();
    // The settle closure outlives the future, so it gets its own handle on the
    // invoker to release the call's borrows through.
    let releasing_invoker = Rc::clone(&invoker);
    let future = async move {
        // The owned stack stays GC-traced across this await through the
        // live-stacks registry.
        let mut stack = invoker.invoke_async(stack, func).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let result =
                settle_import_result(scope, &mut stack, result_ty.as_ref(), err_class.as_ref());
            release_call_borrows(&mut stack, scope, releasing_invoker.as_ref());
            result
        }))
    };

    // The future's first poll lowers the arguments and starts the subtask, so it
    // runs now, while the arguments are in the state `push_import_args` checked.
    // `spawn_polled` roots the promise for the future's lifetime and attributes it
    // to the active event loop.
    promise.spawn_polled(PromiseFuture::new(future));

    Ok(promise.as_value())
}

/// Settle an async import's promise from the lifted result.
///
/// The lifted [`Value`] is rooted in the expression that produces it, so no GC
/// can run between [`handle_import_result`] returning it and `root_value` taking
/// it.
#[js::allow_unrooted]
fn settle_import_result<'s>(
    scope: &'s Scope<'_>,
    stack: &mut CallStack,
    result: Option<&TypeShape>,
    err_class: Option<&ErrorClass>,
) -> Result<js::prelude::HandleValue<'s>, ExnThrown> {
    // `Err` leaves the `err` arm's error, or a `TypeError` for a malformed result,
    // as the pending exception, which the settle path rejects the promise with.
    handle_import_result(scope, stack, result, err_class).map(|value| scope.root_value(value))
}

/// Pop and interpret an import call's lifted result.
///
/// For a non-`result` type the lifted value is returned as-is, or `undefined`
/// when the function returns nothing. For a `result<_, _>` the lifted value is a
/// `{tag, val?}` object: `ok` returns the `val`, or `undefined` if the arm has no
/// payload, and `err` throws an error holding the `err` payload, or `undefined`
/// if that arm has none. The error is an instance of `err_class`'s class, or a
/// `ComponentError` without one.
pub(crate) fn handle_import_result(
    scope: &Scope<'_>,
    stack: &mut CallStack,
    result: Option<&TypeShape>,
    err_class: Option<&ErrorClass>,
) -> Result<Value, ExnThrown> {
    match result {
        None => Ok(js::value::undefined()),
        Some(TypeShape::Result(ok, err)) => {
            let wrapper = pop_result_wrapper(scope, stack)?;
            match crate::value::tag_index(&wrapper, scope, [c"ok", c"err"].into_iter())? {
                Some(0) => {
                    if ok.is_some() {
                        Ok(crate::value::get_field(&wrapper, scope, c"val")?.get())
                    } else {
                        Ok(js::value::undefined())
                    }
                }
                Some(_) => {
                    let payload = if err.is_some() {
                        crate::value::get_field(&wrapper, scope, c"val")?
                    } else {
                        HandleValue::undefined()
                    };
                    Err(crate::component_error::throw_err(scope, payload, err_class))
                }
                _ => Err(throw_type_error(
                    scope,
                    c"imported result has a tag that is neither \"ok\" nor \"err\"",
                )),
            }
        }
        Some(_) => Ok(stack.pop_value()),
    }
}

/// Pop the top value, rooting it as the `result` wrapper object.
fn pop_result_wrapper<'s>(
    scope: &'s Scope<'_>,
    stack: &mut CallStack,
) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, stack.pop_value())
        .map_err(|_| throw_type_error(scope, c"imported result is not a {tag, val} object"))
}

/// Synthesize one ES module per imported interface and register them all for
/// `import` resolution, plus the world-level imports module.
///
/// For each [`InterfaceDesc`]:
///
/// - Every freestanding [`ImportFuncDesc`] becomes an exported
///   [`Function::new_callback`] whose payload is the import index and whose
///   callback runs [`dispatch_import`] (sync or async per the import's mode).
/// - Every [`ResourceTypeDesc`] becomes an exported class
///   ([`synthesize_resource_class`]): a constructor function whose prototype
///   holds the method callbacks and `[Symbol.dispose]`, and the static callbacks
///   on the constructor object.
/// - Every [`ErrorClass`] becomes an exported class extending `ComponentError`
///   (see [`error_class`](crate::component_error::error_class)).
/// - Every [`EnumDesc`] becomes an exported frozen object ([`enum_object`]).
///
/// The module's specifier is the interface's WIT name verbatim
/// ([`InterfaceDesc::wit_name`]), and the world-level group is registered as
/// `external`.
///
/// The whole world's imports are registered in a lookup table keyed by index,
/// with the given `invoker`, before any module is built.
///
/// # Safety
///
/// `core_runtime::module::init_module_loader` must have been called first, since
/// the synthetic modules go through the same registry as every other module.
pub unsafe fn synthesize_import_modules(
    scope: &Scope<'_>,
    world: &WorldDesc,
    invoker: Rc<dyn ImportInvoker>,
) -> Result<(), ExnThrown> {
    // The table covers every import in the world, both freestanding and every
    // resource constructor, method and static.
    let mut funcs: Vec<Option<Rc<ImportFuncDesc>>> = Vec::new();
    let mut record = |func: &ImportFuncDesc| {
        let idx = func.index as usize;
        if funcs.len() <= idx {
            funcs.resize(idx + 1, None);
        }
        funcs[idx] = Some(Rc::new(func.clone()));
    };
    for iface in &world.interfaces {
        for func in &iface.functions {
            record(func);
        }
        for res in &iface.resources {
            if let Some(ctor) = &res.constructor {
                record(ctor);
            }
            for (_, m) in &res.methods {
                record(m);
            }
            for (_, s) in &res.statics {
                record(s);
            }
        }
    }
    set_import_registry(funcs, invoker);

    for iface in &world.interfaces {
        synthesize_interface_module(scope, iface)?;
    }
    for class in &world.unexported_error_classes {
        crate::component_error::error_class(scope, class)?;
    }
    Ok(())
}

/// The module specifier for the world-level imports group.
const WORLD_MODULE_NAME: &str = "external";

/// Build and register the synthetic module for one interface, or for the
/// world-level import group.
unsafe fn synthesize_interface_module(
    scope: &Scope<'_>,
    iface: &InterfaceDesc,
) -> Result<(), ExnThrown> {
    let mut names: Vec<String> = Vec::new();
    let mut values: Vec<HandleValue<'_>> = Vec::new();

    for func in &iface.functions {
        let name = mangle_name(&func.name);
        let f = synthesize_import_function(scope, &name, func)?;
        names.push(name);
        values.push(scope.root_value(f.as_value()));
    }

    for res in &iface.resources {
        let class = synthesize_resource_class(scope, res)?;
        names.push(mangle_resource_name(&res.name));
        values.push(scope.root_value(class.as_value()));
    }

    for class in &iface.error_classes {
        let created = crate::component_error::error_class(scope, class)?;
        names.push(class.js_name());
        values.push(scope.root_value(created.as_value()));
    }

    for desc in &iface.enums {
        let object = enum_object(scope, desc)?;
        names.push(desc.js_name.clone());
        values.push(scope.root_value(object.as_value()));
    }

    let module_name = iface.wit_name.as_deref().unwrap_or(WORLD_MODULE_NAME);
    let exports: Vec<(&str, HandleValue<'_>)> = names
        .iter()
        .map(String::as_str)
        .zip(values.iter().copied())
        .collect();

    core_runtime::module::register_synthetic_module(scope, module_name, &exports)
}

/// The frozen object a module exports for the enum or flags type `desc`: a
/// property per case, holding its value, and a property per value, holding the
/// case's name.
fn enum_object<'s>(scope: &'s Scope<'_>, desc: &EnumDesc) -> Result<Object<'s>, ExnThrown> {
    const ENUMERATE: std::ffi::c_uint = js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;
    let object = Object::new_plain(scope)?;
    for (name, value) in &desc.members {
        object.define_property_by_name(scope, &to_cstring(name), *value, ENUMERATE)?;
        object.define_property_by_name(
            scope,
            &to_cstring(&value.to_string()),
            name.as_str(),
            ENUMERATE,
        )?;
    }
    object.freeze(scope)?;
    Ok(object)
}

/// Build the callback function for one freestanding import. Its payload slot
/// holds the import index, which [`import_callback`] reads back to dispatch.
fn synthesize_import_function<'s>(
    scope: &'s Scope<'_>,
    js_name: &str,
    func: &ImportFuncDesc,
) -> Result<Function<'s>, ExnThrown> {
    let c_name = to_cstring(js_name);
    Function::new_callback(
        scope,
        &c_name,
        func.params.len() as u32,
        import_callback,
        func.index,
    )
}

/// Build a resource class: a constructor function with its method callbacks and
/// `[Symbol.dispose]` on its prototype, and its static callbacks on itself.
///
/// The constructor callback returns the wrapper object `push_own` lifts, which
/// `new R(...)` adopts as the instance. Methods forward the receiver as the first
/// lowered argument, and statics forward none.
fn synthesize_resource_class<'s>(
    scope: &'s Scope<'_>,
    res: &ResourceTypeDesc,
) -> Result<Object<'s>, ExnThrown> {
    let class_name = mangle_resource_name(&res.name);
    let c_class_name = to_cstring(&class_name);

    // A resource with no WIT constructor gets a stub that throws, since a guest
    // must not `new` an unconstructable imported resource.
    let ctor = match &res.constructor {
        Some(ctor) => Function::new_constructor_callback(
            scope,
            &c_class_name,
            ctor.params.len() as u32,
            import_callback,
            ctor.index,
        )?,
        None => Function::new_constructor_callback(
            scope,
            &c_class_name,
            0,
            unconstructable_callback,
            (),
        )?,
    };

    let proto = Object::new_plain(scope)?;
    for (method_name, m) in &res.methods {
        let c_method = to_cstring(method_name);
        let nargs = m.params.len().saturating_sub(1) as u32;
        let f = Function::new_callback(scope, &c_method, nargs, method_callback, m.index)?;
        proto.set_property_by_name(scope, &c_method, f)?;
    }

    let dispose = crate::resources::dispose_function(scope)?;
    crate::resources::define_dispose(scope, &proto, dispose)?;

    let ctor_obj = *ctor;

    for (static_name, s) in &res.statics {
        let c_static = to_cstring(static_name);
        let f = Function::new_callback(
            scope,
            &c_static,
            s.params.len() as u32,
            import_callback,
            s.index,
        )?;
        ctor_obj.set_property_by_name(scope, &c_static, f)?;
    }

    // `R.prototype === proto` and `proto.constructor === R`.
    js::class::link_constructor_and_prototype(scope, ctor_obj.handle(), proto.handle())?;

    // The prototype is recorded so the wrappers `resources::push_own` and
    // `resources::push_borrow` create are re-parented to it. A resource with the
    // `u32::MAX` sentinel has no type index (see `build_world_desc`), and
    // registering under that stand-in index would clobber a real resource's
    // prototype.
    if res.type_idx != u32::MAX {
        crate::resources::register_resource_prototype(res.type_idx, &proto);
    }

    Ok(ctor_obj)
}

/// Dispatch a synthesized import callback by whether the import is async. A sync
/// import returns its lifted result directly, and an async import returns a
/// pending promise the spawned future settles.
fn dispatch_import(
    scope: &Scope<'_>,
    index: u32,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let (func, invoker) = lookup_import_or_throw(scope, index)?;
    if func.is_async {
        call_import_async(scope, func, invoker, args)
    } else {
        call_import(scope, &func, invoker.as_ref(), args)
    }
}

/// The body of a freestanding import function and of a resource constructor or
/// static method: dispatch with the JS arguments as-is.
fn import_callback(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let index = payload_index(scope, payload)?;
    // `CallbackArgs::get` yields handles the engine keeps rooted for the callback's duration.
    let collected: smallvec::SmallVec<[HandleValue<'_>; 8]> =
        (0..args.len()).map(|i| args.get(i)).collect();
    dispatch_import(scope, index, &collected)
}

/// The body of a resource instance method: prepend `this` as argument 0, matching
/// the WIT `[method]` convention.
fn method_callback(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let index = payload_index(scope, payload)?;
    // `this` is the only value the engine does not already hand over rooted.
    let mut collected: smallvec::SmallVec<[HandleValue<'_>; 8]> =
        smallvec::SmallVec::with_capacity(args.len() as usize + 1);
    collected.push(scope.root_value(args.this()));
    collected.extend((0..args.len()).map(|i| args.get(i)));
    dispatch_import(scope, index, &collected)
}

/// The construct trap for a resource type with no WIT constructor: always throws.
fn unconstructable_callback(
    scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    Err(throw_type_error(
        scope,
        c"this imported resource type has no constructor",
    ))
}

/// Read the import index out of a callback payload value.
fn payload_index(scope: &Scope<'_>, payload: HandleValue<'_>) -> Result<u32, ExnThrown> {
    // `from_u32` boxes values past `i32::MAX` as a double, so the index is read
    // back through the number path for the full `u32` range to round-trip.
    if payload.get().is_number() {
        Ok(payload.get().to_number() as u32)
    } else {
        Err(throw_type_error(
            scope,
            c"import callback payload is not an index",
        ))
    }
}
