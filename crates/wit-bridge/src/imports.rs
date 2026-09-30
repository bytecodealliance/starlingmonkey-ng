// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The ES modules that provide the component's imports to JavaScript, and
//! calls to the imports.
//!
//! The application imports WIT functions and resources from ES modules named
//! after their WIT interfaces, such as `import { f } from "test:iface/x"`.
//! [`synthesize_import_modules`] creates these modules from a [`WorldDesc`].
//!
//! The import calls are made by an [`ImportInvoker`], which lowers the
//! arguments, calls the host and lifts the result. [`call_import_from_js`] and
//! [`call_import_from_js_async`] check the arguments and push them onto a
//! [`CallStack`] for the invoker, and pop the lifted result off it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js::error::{throw_type_error, ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use wit_name_mapping::{mangle_name, mangle_resource_name, FunctionName, WORLD_MODULE_NAME};

use crate::component_error::ErrorClass;
use crate::stack::CallStack;
use crate::value::{to_cstring, TypeShape, ENUMERATE};

/// A description of a component's imports. Interfaces and functions have
/// their WIT names, which [`synthesize_import_modules`] converts to JS names.
#[derive(Clone, Debug, Default)]
pub struct WorldDesc {
    /// One entry per imported interface, plus at most one world-level entry
    /// (whose [`InterfaceDesc::wit_name`] is `None`) for imports defined
    /// directly on the world.
    pub interfaces: Vec<InterfaceDesc>,
    /// The error classes of types defined by interfaces that the world exports
    /// but doesn't import. They are created, but no module exports them.
    pub unexported_error_classes: Vec<ErrorClass>,
}

/// One imported interface, or the imports defined directly on the world.
#[derive(Clone, Debug, Default)]
pub struct InterfaceDesc {
    /// The interface's WIT name, such as `test:iface/x@0.2.0`, which is the
    /// specifier of its module. `None` for the imports defined on the world,
    /// whose module is [`WORLD_MODULE_NAME`].
    pub wit_name: Option<String>,
    /// The imported functions of the interface that don't belong to a
    /// resource.
    pub functions: Vec<ImportFuncDesc>,
    /// Imported resource types on this interface.
    pub resources: Vec<ResourceTypeDesc>,
    /// The error classes of the types defined by the interface. The module
    /// exports them.
    pub error_classes: Vec<ErrorClass>,
    /// The enum and flags types defined or `use`d by the interface. The module
    /// exports an object for each.
    pub enums: Vec<EnumDesc>,
}

/// An enum or flags type, which its interface's module exports as a frozen
/// object. The object maps each case's JS name to its value, and each value
/// back to the name, as a TypeScript numeric enum does.
#[derive(Clone, Debug)]
pub struct EnumDesc {
    /// The name under which the module exports the object: the type's name
    /// in the interface, upper-camel-cased.
    pub js_name: String,
    /// Each case's JS name and value. An enum case's value is its index, and a
    /// flag's is its bit, as an int32.
    pub members: Vec<(String, i32)>,
}

/// One imported function: its index and its WIT signature.
#[derive(Clone, Debug)]
pub struct ImportFuncDesc {
    /// The function's index among all of the component's imported functions.
    pub index: u32,
    /// The WIT name, which can start with `[constructor]`, `[method]` or
    /// `[static]`.
    pub name: String,
    /// Parameter types, in declaration order. For a `[method]`, the first
    /// parameter is the resource's `self` (a `borrow`).
    pub params: Vec<TypeShape>,
    /// The result type, or `None` for a function returning nothing.
    pub result: Option<TypeShape>,
    /// The error class of the `err` case, if the result type is a `result`
    /// whose `err` type has its own class.
    pub err_class: Option<ErrorClass>,
    /// Whether the function is declared `async` in WIT.
    pub is_async: bool,
}

/// One imported resource type and the import functions implementing its
/// constructor, methods, and statics.
#[derive(Clone, Debug)]
pub struct ResourceTypeDesc {
    /// The resource type's index, or `u32::MAX` if it has none.
    pub type_idx: u32,
    /// The WIT resource name (`"my-resource"`), upper-camel-cased to the JS class
    /// name during synthesis.
    pub name: String,
    /// The import implementing `[constructor]name`, if any.
    pub constructor: Option<ImportFuncDesc>,
    /// The imports implementing `[method]name.m`, each with the method's JS
    /// name.
    pub methods: Vec<(String, ImportFuncDesc)>,
    /// The imports implementing `[static]name.m`, each with the function's JS
    /// name.
    pub statics: Vec<(String, ImportFuncDesc)>,
}

/// Makes import calls: lowers the arguments, calls the host, and lifts the
/// result.
///
/// The methods receive a stack holding the JS arguments, with the first
/// argument on top. They must leave exactly one value, the lifted result, on
/// the stack.
pub trait ImportInvoker {
    /// Make a synchronous import call.
    fn invoke(
        &self,
        scope: &Scope<'_>,
        stack: &mut CallStack,
        func: &ImportFuncDesc,
    ) -> Result<(), ExnThrown>;

    /// Whether this invoker implements [`invoke_async`](Self::invoke_async). If
    /// it doesn't, calling an async import throws a `TypeError`.
    fn supports_async(&self) -> bool {
        false
    }

    /// Make a call to an async import. The returned future resolves to the
    /// stack, with the lifted result on top.
    ///
    /// The default implementation panics.
    fn invoke_async(
        &self,
        stack: CallStack,
        func: Rc<ImportFuncDesc>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CallStack> + 'static>> {
        let _ = (stack, func);
        panic!("invoke_async called on an invoker whose supports_async() is false");
    }

    /// Drop `handle`, a handle of the exported resource type `type_idx` that was
    /// created to lend a resource to the host for one import call. Called for
    /// each such handle when the call ends. The default implementation does
    /// nothing.
    fn drop_exported_handle(&self, type_idx: u32, handle: u32) {
        let _ = (type_idx, handle);
    }
}

/// The imported functions of the world, and the invoker that calls them.
struct ImportRegistry {
    /// Indexed by [`ImportFuncDesc::index`]. A `None` entry has no function.
    funcs: Vec<Option<Rc<ImportFuncDesc>>>,
    /// The invoker shared by every import in this world.
    invoker: Rc<dyn ImportInvoker>,
}

js::instance_local! {
    /// The import registry, set by [`synthesize_import_modules`].
    static IMPORTS: RefCell<Option<ImportRegistry>> = const { RefCell::new(None) };

    /// Whether calling an import throws, set by [`set_imports_unavailable`].
    static IMPORTS_UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
}

/// Make every import call throw a `TypeError` while `unavailable` is set.
/// Imports are unavailable while the application is being initialized, before
/// the component is wizened.
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

/// Look up an import and the invoker by the import's index. Throws a
/// `TypeError` if there is no import with that index.
fn lookup_import(
    scope: &Scope<'_>,
    index: u32,
) -> Result<(Rc<ImportFuncDesc>, Rc<dyn ImportInvoker>), ExnThrown> {
    IMPORTS
        .with(|cell| {
            let borrow = cell.borrow();
            let reg = borrow.as_ref()?;
            let func = reg.funcs.get(index as usize)?.clone()?;
            Some((func, Rc::clone(&reg.invoker)))
        })
        .ok_or_else(|| throw_type_error(scope, c"import function index is not registered"))
}

/// Call the imported function with the index `index`, with the JS arguments
/// `args`. For a method, `args[0]` is the receiver.
///
/// Returns the lifted result, which isn't rooted, so it must not be held across
/// an allocation.
pub fn call_import_from_js(
    scope: &Scope<'_>,
    index: u32,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let (func, invoker) = lookup_import(scope, index)?;
    call_import(scope, &func, invoker.as_ref(), args, args.len())
}

/// Like [`call_import_from_js`], for the import `func`, called with `argc`
/// arguments. `args` holds the arguments if `argc` equals the import's arity,
/// and may be empty otherwise.
fn call_import(
    scope: &Scope<'_>,
    func: &ImportFuncDesc,
    invoker: &dyn ImportInvoker,
    args: &[HandleValue<'_>],
    argc: usize,
) -> Result<Value, ExnThrown> {
    check_imports_available(scope, func)?;
    let mut stack = push_import_args(scope, func, args, argc)?;

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

/// Check `args` against `func`'s parameters, and push their snapshots (see
/// [`crate::validate::snapshot_arguments`]) onto a new stack, with the first
/// argument on top.
///
/// Throws a `TypeError` naming the import and the argument if `argc` isn't
/// `func`'s arity, or if an argument can't be lowered to its parameter's type.
fn push_import_args(
    scope: &Scope<'_>,
    func: &ImportFuncDesc,
    args: &[HandleValue<'_>],
    argc: usize,
) -> Result<CallStack, ExnThrown> {
    let is_method = matches!(FunctionName::parse(&func.name), FunctionName::Method { .. });
    if argc != func.params.len() {
        let expected = func.params.len().saturating_sub(usize::from(is_method));
        return Err(js::error::TypeError(format!(
            "`{}` takes {expected} argument{}, got {}",
            js_import_name(&func.name),
            if expected == 1 { "" } else { "s" },
            argc.saturating_sub(usize::from(is_method)),
        ))
        .throw(scope));
    }
    // Each argument is lowered from its snapshot, so a getter that returns
    // something else on a second read cannot make the lowering trap. The
    // resource wrappers the snapshots hold are checked after all arguments were
    // read, so a getter cannot dispose of one between its check and the
    // lowering.
    let snapshots = crate::validate::snapshot_arguments(scope, args, &func.params).map_err(
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
    for snapshot in snapshots.into_iter().rev() {
        stack.push_shaped(snapshot);
    }
    Ok(stack)
}

/// An import's WIT function name as the guest calls it: `thing.method` for a
/// resource's method or static function, `new thing` for its constructor, and
/// the name itself otherwise.
pub(crate) fn js_import_name(wit_name: &str) -> String {
    match FunctionName::parse(wit_name) {
        FunctionName::Constructor { resource } => format!("new {}", mangle_resource_name(resource)),
        FunctionName::Method { resource, name } | FunctionName::Static { resource, name } => {
            format!("{}.{}", mangle_resource_name(resource), mangle_name(name))
        }
        FunctionName::Plain(name) => mangle_name(name),
    }
}

/// Release the exported resources lent to the host as `borrow` arguments of an
/// import call, after the call's result is lifted.
///
/// A pending exception is set aside while the borrows are released, and
/// restored afterwards. An exception thrown while releasing them is reported
/// and cleared.
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

/// Like [`call_import_from_js`], for an async import. Returns a promise that
/// settles when the host call completes. The call runs on the event loop that
/// is active when the import is called.
///
/// The promise is rejected with a `TypeError` if no event loop is active, if
/// imports are unavailable, or if an argument can't be lowered. For the `err`
/// case of a `result`, it is rejected with the error that
/// [`call_import_from_js`] throws.
///
/// The arguments are lowered before this returns, so JavaScript that runs
/// afterwards can't change what the host receives.
pub fn call_import_from_js_async(
    scope: &Scope<'_>,
    index: u32,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let (func, invoker) = lookup_import(scope, index)?;
    call_import_async(scope, func, invoker, args, args.len())
}

/// Like [`call_import_from_js_async`], for the import `func`, called with
/// `argc` arguments as for [`call_import`].
fn call_import_async(
    scope: &Scope<'_>,
    func: Rc<ImportFuncDesc>,
    invoker: Rc<dyn ImportInvoker>,
    args: &[HandleValue<'_>],
    argc: usize,
) -> Result<Value, ExnThrown> {
    use js::promise::{PromiseFuture, PromiseOutcome};

    // The default `invoke_async` panics.
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
    if check_imports_available(scope, &func).is_err() {
        promise.reject_with_pending(scope)?;
        return Ok(promise.as_value());
    }
    // The call's future requires an event loop.
    if !core_runtime::event_loop::has_active_event_loop() {
        let what = format!("the async import `{}`", js_import_name(&func.name));
        if core_runtime::event_loop::reject_without_event_loop(scope, &promise, &what)? {
            return Ok(promise.as_value());
        }
    }
    let stack = match push_import_args(scope, &func, args, argc) {
        Ok(stack) => stack,
        Err(ExnThrown) => {
            promise.reject_with_pending(scope)?;
            return Ok(promise.as_value());
        }
    };

    let result_ty = func.result.clone();
    let err_class = func.err_class.clone();
    // The closure that settles the promise outlives the future, so it holds its
    // own reference to the invoker.
    let releasing_invoker = Rc::clone(&invoker);
    let future = async move {
        // The owned stack stays GC-traced across this await through the
        // live-stacks registry.
        let mut stack = invoker.invoke_async(stack, func).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            // `Err` leaves the `err` arm's error, or a `TypeError` for a malformed
            // result, as the pending exception, which rejects the promise.
            let result =
                handle_import_result(scope, &mut stack, result_ty.as_ref(), err_class.as_ref())
                    .map(|value| scope.root_value(value));
            release_call_borrows(&mut stack, scope, releasing_invoker.as_ref());
            result
        }))
    };

    // The first poll lowers the arguments and starts the host call. It has to
    // happen now, while the arguments are as checked by `push_import_args`.
    // `spawn_polled` roots the promise until the future completes, and runs the
    // future on the active event loop.
    promise.spawn_polled(PromiseFuture::new(future));

    Ok(promise.as_value())
}

/// Pop the lifted result of an import call off the stack, and return the value
/// that the call returns to JavaScript.
///
/// If `result` is `None`, returns `undefined`. If it is a `result`, returns the
/// `ok` value, or `undefined` if the `ok` case has no value, and throws an
/// error for the `err` case (see `component_error::throw_err`). Otherwise,
/// returns the lifted value.
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
                        Ok(wrapper.get_property(scope, c"val")?.get())
                    } else {
                        Ok(js::value::undefined())
                    }
                }
                Some(_) => {
                    let payload = if err.is_some() {
                        wrapper.get_property(scope, c"val")?
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
    Object::from_raw_value(scope, stack.pop_value())
        .map_err(|_| throw_type_error(scope, c"imported result is not a {tag, val} object"))
}

/// Create and register an ES module for each entry of
/// [`WorldDesc::interfaces`], and register `invoker` for calls to the imports.
///
/// Each module exports:
///
/// - a function for each of [`InterfaceDesc::functions`],
/// - a class for each of [`InterfaceDesc::resources`], with the resource's
///   methods and a `[Symbol.dispose]` method on its prototype, and its static
///   functions on the class,
/// - the subclass of `ComponentError` for each of
///   [`InterfaceDesc::error_classes`] (see
///   [`create_error_class`](crate::component_error::create_error_class)),
/// - a frozen object for each of [`InterfaceDesc::enums`] (see [`EnumDesc`]).
///
/// A module's specifier is [`InterfaceDesc::wit_name`], or
/// [`WORLD_MODULE_NAME`] for the imports defined on the world. The classes in
/// [`WorldDesc::unexported_error_classes`] are created, but not exported.
///
/// # Panics
///
/// Panics if no runtime is current.
pub fn synthesize_import_modules(
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
    IMPORTS.with(|cell| *cell.borrow_mut() = Some(ImportRegistry { funcs, invoker }));

    for iface in &world.interfaces {
        synthesize_interface_module(scope, iface)?;
    }
    for class in &world.unexported_error_classes {
        crate::component_error::create_error_class(scope, class)?;
    }
    Ok(())
}

/// Build and register the synthetic module for one interface, or for the
/// world-level import group.
fn synthesize_interface_module(scope: &Scope<'_>, iface: &InterfaceDesc) -> Result<(), ExnThrown> {
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
        let created = crate::component_error::create_error_class(scope, class)?;
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

/// Create the frozen object for the enum or flags type `desc`, with a property
/// for each case holding its value, and a property for each value holding the
/// case's name.
fn enum_object<'s>(scope: &'s Scope<'_>, desc: &EnumDesc) -> Result<Object<'s>, ExnThrown> {
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

/// Create the function for the import `func`, which doesn't belong to a
/// resource. The function's payload is the import's index.
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

/// Create the class for the imported resource `res`, with the methods and
/// `[Symbol.dispose]` on its prototype, and the static functions on the class.
///
/// The constructor returns the wrapper of the `own` handle returned by the
/// host, which becomes the result of `new`. A method passes `this` as the first
/// argument of the import call.
fn synthesize_resource_class<'s>(
    scope: &'s Scope<'_>,
    res: &ResourceTypeDesc,
) -> Result<Object<'s>, ExnThrown> {
    let class_name = mangle_resource_name(&res.name);
    let c_class_name = to_cstring(&class_name);

    // Constructing a resource without a WIT constructor throws.
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
        // Non-enumerable, as the methods of a JS `class` are.
        proto.define_property_by_name(scope, &c_method, f, 0)?;
    }

    let dispose = crate::resources::dispose_function(scope)?;
    js::class::define_well_known_symbol_property(
        scope,
        proto,
        js::native::SymbolCode::dispose,
        dispose,
    )?;

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
        ctor_obj.define_property_by_name(scope, &c_static, f, 0)?;
    }

    // `R.prototype === proto` and `proto.constructor === R`.
    js::class::link_constructor_and_prototype(scope, ctor_obj.handle(), proto.handle())?;

    // Wrappers created by `resources::push_own` and `resources::push_borrow` get
    // the prototype registered here. A resource without a type index isn't
    // registered.
    if res.type_idx != u32::MAX {
        crate::resources::register_resource_prototype(res.type_idx, &proto);
    }

    Ok(ctor_obj)
}

/// Call the import with the index `index` with the callback's arguments,
/// preceded by `this` if it is `Some`. Returns the result of a synchronous
/// import, or a promise for the result of an async import.
fn dispatch_import(
    scope: &Scope<'_>,
    index: u32,
    this: Option<HandleValue<'_>>,
    args: &js::function::CallbackArgs<'_>,
) -> Result<Value, ExnThrown> {
    let (func, invoker) = lookup_import(scope, index)?;
    let argc = args.len() as usize + usize::from(this.is_some());
    // `CallbackArgs::get` yields handles the engine keeps rooted for the callback's duration.
    let collected: smallvec::SmallVec<[HandleValue<'_>; 8]> = if argc == func.params.len() {
        this.into_iter()
            .chain((0..args.len()).map(|i| args.get(i)))
            .collect()
    } else {
        smallvec::SmallVec::new()
    };
    if func.is_async {
        call_import_async(scope, func, invoker, &collected, argc)
    } else {
        call_import(scope, &func, invoker.as_ref(), &collected, argc)
    }
}

/// The callback of an import function that isn't a method.
fn import_callback(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let index = payload_index(scope, payload)?;
    dispatch_import(scope, index, None, &args)
}

/// The callback of a resource method, which passes `this` as the first
/// argument.
fn method_callback(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let index = payload_index(scope, payload)?;
    // Unlike the arguments, `this` isn't rooted by the engine.
    let this = scope.root_value(args.this());
    dispatch_import(scope, index, Some(this), &args)
}

/// The constructor of a resource type without a WIT constructor, which always
/// throws.
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
