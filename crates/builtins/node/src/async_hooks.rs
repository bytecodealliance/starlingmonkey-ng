// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:async_hooks`. See <https://nodejs.org/api/async_context.html> and
//! <https://nodejs.org/api/async_hooks.html>.
//!
//! `AsyncLocalStorage` and `AsyncResource` keep their stores in the async context
//! ([`js::async_context`]) as Node's `AsyncContextFrame`s do: the context is an
//! [`AsyncContextFrame`] holding each `AsyncLocalStorage`'s store, and setting a store replaces
//! the context with a copy that holds it. Promise reactions, timers and other event-loop tasks run
//! with the context that was current when they were created.
//!
//! `executionAsyncId()` returns 1 and `triggerAsyncId()` returns 0, Node's values at the top level
//! of a script. `AsyncResource` IDs count up from 2. `createHook` returns a hook whose callbacks
//! are never called.

use js::async_context::Snapshot;
use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::symbol::CachedSymbol;
use js::{Function, Object};

use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};

/// The store of one `AsyncLocalStorage` in an [`AsyncContextFrame`].
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct FrameEntry {
    storage: Heap<js::object::Object>,
    store: Heap<Value>,
}

/// An async context frame, as Node's `AsyncContextFrame`: the stores of the `AsyncLocalStorage`s
/// that have one in an async context, and the `node:domain` domain that was active when the
/// context was set. Setting a store or the domain makes a new frame, and only
/// `asyncLocalStorage.disable()` changes a frame.
#[core_runtime::jsclass(hidden)]
pub(crate) struct AsyncContextFrame {
    entries: Vec<FrameEntry>,
    domain: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl AsyncContextFrame {}

/// The frame the current async context is, if there is one.
fn current_frame<'s>(scope: &'s Scope<'_>) -> Option<AsyncContextFrame<'s>> {
    js::async_context::current(scope).and_then(|context| context.cast::<AsyncContextFrame>().ok())
}

/// The domain the current async context holds, if it holds one.
pub(crate) fn current_domain<'s>(scope: &'s Scope<'_>) -> Option<Object<'s>> {
    let frame = current_frame(scope)?;
    let domain = frame.data().domain.get(scope);
    domain
}

/// Make a copy of the current async context's frame that holds `domain` current, unless the
/// current context already holds it.
pub(crate) fn enter_domain_frame(
    scope: &Scope<'_>,
    domain: Option<Object<'_>>,
) -> Result<(), ExnThrown> {
    let current = current_domain(scope);
    if current.map(|current| current.as_raw()) == domain.map(|domain| domain.as_raw()) {
        return Ok(());
    }
    let frame = new_frame(scope, current_entries(scope, None), domain)?;
    Snapshot::of(Some(frame.as_object())).enter();
    Ok(())
}

/// The stores of the current async context's frame, without the store of `except`.
fn current_entries<'s>(
    scope: &'s Scope<'_>,
    except: Option<Object<'_>>,
) -> Vec<(Object<'s>, HandleValue<'s>)> {
    let mut kept = Vec::new();
    if let Some(current) = current_frame(scope) {
        for entry in &current.data().entries {
            let storage = entry.storage.get(scope);
            if except.is_none_or(|key| storage.as_raw() != key.as_raw()) {
                kept.push((storage, entry.store.get(scope)));
            }
        }
    }
    kept
}

/// A frame holding `entries` and `domain`.
fn new_frame<'s>(
    scope: &'s Scope<'_>,
    entries: Vec<(Object<'_>, HandleValue<'_>)>,
    domain: Option<Object<'_>>,
) -> Result<AsyncContextFrame<'s>, ExnThrown> {
    js::class::create_instance_with::<AsyncContextFrameImpl>(scope, |_| AsyncContextFrameImpl {
        entries: entries
            .iter()
            .map(|(storage, store)| FrameEntry {
                storage: Heap::from(*storage),
                store: Heap::from(store.get()),
            })
            .collect(),
        domain: domain.map(Heap::from),
    })
}

/// The store `key` has in the current async context, if it has one.
pub(crate) fn current_store<'s>(scope: &'s Scope<'_>, key: Object<'_>) -> Option<HandleValue<'s>> {
    let frame = current_frame(scope)?;
    let data = frame.data();
    data.entries
        .iter()
        .find(|entry| entry.storage.get(scope).as_raw() == key.as_raw())
        .map(|entry| entry.store.get(scope))
}

/// A copy of the current async context's frame in which `key` has `store`, or no store if `store`
/// is `None`.
pub(crate) fn derived_frame<'s>(
    scope: &'s Scope<'_>,
    key: Object<'_>,
    store: Option<HandleValue<'_>>,
) -> Result<AsyncContextFrame<'s>, ExnThrown> {
    let mut entries = current_entries(scope, Some(key));
    entries.extend(store.map(|store| (key, store)));
    new_frame(scope, entries, current_domain(scope))
}

/// Call `callback` with `this` and `args` while `context` is current, then restore the context
/// that was current before, whether or not `callback` throws.
pub(crate) fn call_in<'s>(
    scope: &'s Scope<'_>,
    context: Snapshot,
    callback: HandleValue<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let previous = context.enter();
    let result =
        Function::call(scope, this, callback, args).map(|result| scope.root_value(result.get()));
    previous.enter();
    result
}

/// [`call_in`] for a callback of an async resource, which runs as a callback that captured
/// `context`, with the [`js::async_context::CallbackHooks`] called around it.
fn call_as_callback<'s>(
    scope: &'s Scope<'_>,
    context: Snapshot,
    callback: HandleValue<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let previous = context.enter_callback(scope);
    let result =
        Function::call(scope, this, callback, args).map(|result| scope.root_value(result.get()));
    previous.leave_callback(scope);
    result
}

/// The arguments of a native call from index `start` on.
fn args_from<'a>(args: &'a CallbackArgs<'_>, start: u32) -> Vec<HandleValue<'a>> {
    (start..args.len()).map(|index| args.get(index)).collect()
}

/// An `AsyncLocalStorage`. See <https://nodejs.org/api/async_context.html#class-asynclocalstorage>.
#[core_runtime::jsclass(name = "AsyncLocalStorage", hidden)]
pub struct AsyncLocalStorage {
    default_value: Heap<Value>,
    name: Heap<Value>,
}

#[core_runtime::jsmethods(node_args)]
impl AsyncLocalStorage {
    /// <https://nodejs.org/api/async_context.html#new-asynclocalstorageoptions>
    #[constructor]
    pub fn new(&self, scope: &Scope<'_>, options: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let (default_value, name) = match options.filter(|options| !options.is_undefined()) {
            Some(options) => {
                let options = Object::from_value(scope, options).map_err(|_| {
                    NodeError::invalid_arg_type(scope, "options", "of type object", options)
                })?;
                (
                    options.get_property(scope, c"defaultValue")?,
                    options.get_property(scope, c"name")?,
                )
            }
            None => (HandleValue::undefined(), HandleValue::undefined()),
        };
        let name = if name.is_undefined() {
            js::JSString::empty(scope)
        } else {
            js::JSString::from_value(scope, name)?
        };
        let data = self.data();
        data.default_value.set(default_value.get());
        data.name.set(name.as_value());
        Ok(())
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstoragename>
    #[getter]
    pub fn name<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        self.data().name.get(scope)
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstoragedisable>: removes the store
    /// from the current context, including from the work that captured it.
    #[method]
    pub fn disable(&self, scope: &Scope<'_>) {
        if let Some(frame) = current_frame(scope) {
            let key = self.as_object();
            frame
                .data_mut()
                .entries
                .retain(|entry| entry.storage.get(scope).as_raw() != key.as_raw());
        }
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstorageenterwithstore>: the store
    /// stays set for the rest of the current task or job, and for the work it creates.
    #[method]
    pub fn enter_with(&self, scope: &Scope<'_>, store: HandleValue<'_>) -> Result<(), ExnThrown> {
        let frame = derived_frame(scope, self.as_object(), Some(store))?;
        Snapshot::of(Some(frame.as_object())).enter();
        Ok(())
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstoragerunstore-callback-args>
    #[method]
    pub fn run<'r>(
        &self,
        scope: &'r Scope<'_>,
        store: HandleValue<'_>,
        callback: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let frame = derived_frame(scope, self.as_object(), Some(store))?;
        let context = Snapshot::of(Some(frame.as_object()));
        call_in(scope, context, callback, HandleValue::undefined(), &args)
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstorageexitcallback-args>: runs
    /// `callback` with the store set to `undefined`.
    #[method]
    pub fn exit<'r>(
        &self,
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let undefined = HandleValue::undefined();
        let frame = derived_frame(scope, self.as_object(), Some(undefined))?;
        let context = Snapshot::of(Some(frame.as_object()));
        call_in(scope, context, callback, HandleValue::undefined(), &args)
    }

    /// <https://nodejs.org/api/async_context.html#asynclocalstoragegetstore>: the store, or the
    /// `defaultValue` option if no store was set.
    #[method]
    pub fn get_store<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        current_store(scope, self.as_object())
            .unwrap_or_else(|| self.data().default_value.get(scope))
    }

    /// `asyncLocalStorage.withScope(store)`: a `RunScope` that sets the store until it is
    /// disposed, which restores the store it replaced.
    #[method]
    pub fn with_scope<'r>(
        &self,
        scope: &'r Scope<'_>,
        store: HandleValue<'_>,
    ) -> Result<RunScope<'r>, ExnThrown> {
        let previous = self.get_store(scope);
        self.enter_with(scope, store)?;
        js::class::create_instance_with::<RunScopeImpl>(scope, |_| RunScopeImpl {
            storage: Some(Heap::from(self.as_object())),
            previous: Heap::from(previous.get()),
        })
    }

    /// <https://nodejs.org/api/async_context.html#static-method-asynclocalstoragebindfn>: an
    /// `AsyncResource` binds `fn`, as in Node.js.
    #[static_method]
    pub fn bind<'r>(scope: &'r Scope<'_>, callback: HandleValue<'_>) -> Result<Object<'r>, Thrown> {
        bind_new_resource(
            scope,
            callback,
            HandleValue::undefined(),
            HandleValue::undefined(),
        )
    }

    /// <https://nodejs.org/api/async_context.html#static-method-asynclocalstoragesnapshot>: a
    /// function that calls its first argument with the others in the current async context,
    /// bound by an `AsyncResource` as in Node.js.
    #[static_method]
    pub fn snapshot<'r>(scope: &'r Scope<'_>) -> Result<Object<'r>, Thrown> {
        let run = Function::new_callback(scope, c"", 1, call_first_argument, ())?;
        bind_new_resource(
            scope,
            run.to_jsval_throwing(scope)?,
            HandleValue::undefined(),
            HandleValue::undefined(),
        )
    }
}

/// The function `AsyncLocalStorage.snapshot()` binds: call `args[0]` with the remaining arguments.
fn call_first_argument(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let rest = args_from(&args, 1);
    Ok(Function::call(scope, HandleValue::undefined(), args.get(0), &rest)?.get())
}

/// The scope `asyncLocalStorage.withScope(store)` returns.
#[core_runtime::jsclass(name = "RunScope", hidden)]
pub struct RunScope {
    /// The `AsyncLocalStorage` whose store the scope set, until it is disposed.
    storage: Option<Heap<js::object::Object>>,
    /// The store the scope replaced.
    previous: Heap<Value>,
}

#[core_runtime::jsmethods(node_args)]
impl RunScope {
    /// Restore the store the scope replaced. Does nothing once the scope is disposed.
    #[method]
    pub fn dispose(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let (storage, previous) = {
            let mut data = self.data_mut();
            let Some(storage) = data.storage.take_rooted(scope) else {
                return Ok(());
            };
            (storage, data.previous.get(scope))
        };
        let storage = storage
            .cast::<AsyncLocalStorage>()
            .map_err(|_| js::error::TypeError("not an AsyncLocalStorage".into()).throw(scope))?;
        storage.enter_with(scope, previous)
    }
}

/// `runScope[Symbol.dispose]()`: `runScope.dispose()`.
fn dispose_run_scope(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    Function::call_by_name(scope, this.handle(), c"dispose", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// Node's `Symbol('context_frame')`, under which an `AsyncResource` holds its async context.
static CONTEXT_FRAME: CachedSymbol = CachedSymbol::new(c"context_frame");
/// Node's `Symbol('async_id_symbol')`, under which an `AsyncResource` holds its ID.
static ASYNC_ID: CachedSymbol = CachedSymbol::new(c"async_id_symbol");
/// Node's `Symbol('trigger_async_id_symbol')`, under which an `AsyncResource` holds the ID of the
/// resource that created it.
static TRIGGER_ASYNC_ID: CachedSymbol = CachedSymbol::new(c"trigger_async_id_symbol");

/// The key the per-realm counter of async resource IDs is kept under.
static ASYNC_ID_KEY: u8 = 0;

/// An `AsyncResource`. See <https://nodejs.org/api/async_context.html#class-asyncresource>.
#[core_runtime::jsclass(name = "AsyncResource", hidden)]
pub struct AsyncResource {
    /// The async context current when the resource was created.
    frame: Option<Heap<js::object::Object>>,
    #[no_trace]
    async_id: f64,
    #[no_trace]
    trigger_async_id: f64,
}

#[core_runtime::jsmethods(node_args)]
impl AsyncResource {
    /// <https://nodejs.org/api/async_context.html#new-asyncresourcetype-options>: the
    /// `requireManualDestroy` option has no effect.
    #[constructor]
    pub fn new(
        &self,
        scope: &Scope<'_>,
        r#type: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if !r#type.is_string() {
            return Err(
                NodeError::invalid_arg_type(scope, "type", "of type string", r#type).into(),
            );
        }
        let trigger = match options {
            Some(options) if options.is_number() => options,
            Some(options) if !options.is_undefined() => {
                let options = Object::from_value_coerce(scope, options)?;
                options.get_property(scope, c"triggerAsyncId")?
            }
            _ => HandleValue::undefined(),
        };
        let trigger = if trigger.is_undefined() {
            1.0
        } else {
            let number = trigger.is_number().then(|| trigger.to_number());
            match number {
                Some(number)
                    if number.fract() == 0.0
                        && (-1.0..=crate::errors::MAX_SAFE_INTEGER).contains(&number) =>
                {
                    number
                }
                _ => {
                    let shown = crate::errors::js_string(scope, trigger)?;
                    return Err(NodeError::new(
                        ErrorKind::RangeError,
                        "ERR_INVALID_ASYNC_ID",
                        format!("Invalid triggerAsyncId value: {shown}"),
                    )
                    .into());
                }
            }
        };
        let frame = js::async_context::current(scope);
        // IDs start at 2, after `executionAsyncId()`.
        let id = js::class::next_global_counter(scope, &ASYNC_ID_KEY as *const u8 as usize) + 2;
        {
            let mut data = self.data_mut();
            data.frame = frame.map(Heap::from);
            data.async_id = id as f64;
            data.trigger_async_id = trigger;
        }
        let this = self.as_object();
        let frame = match frame {
            Some(frame) => frame.to_jsval_throwing(scope)?,
            None => HandleValue::undefined(),
        };
        this.set_property_by_id(scope, CONTEXT_FRAME.id(scope)?, frame)?;
        this.set_property_by_id(scope, ASYNC_ID.id(scope)?, id as f64)?;
        this.set_property_by_id(scope, TRIGGER_ASYNC_ID.id(scope)?, trigger)?;
        crate::domain::init_resource(scope, this)?;
        Ok(())
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourceruninasyncscopefn-thisarg-args>
    #[method(length = 2)]
    pub fn run_in_async_scope<'r>(
        &self,
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let context = self.context(scope);
        let this_arg = this_arg.unwrap_or(HandleValue::undefined());
        call_as_callback(scope, context, callback, this_arg, &args)
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourceemitdestroy>
    #[method]
    pub fn emit_destroy(&self) -> Object<'s> {
        self.as_object()
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourceasyncid>
    #[method]
    pub fn async_id(&self) -> f64 {
        self.data().async_id
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourcetriggerasyncid>
    #[method]
    pub fn trigger_async_id(&self) -> f64 {
        self.data().trigger_async_id
    }

    /// <https://nodejs.org/api/async_context.html#asyncresourcebindfn-thisarg>
    #[method(length = 2)]
    pub fn bind<'r>(
        &self,
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        this_arg: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, Thrown> {
        let callback = validate_function(scope, callback, "fn")?;
        bind_resource(
            scope,
            self.as_object(),
            callback,
            this_arg.unwrap_or(HandleValue::undefined()),
        )
    }

    /// <https://nodejs.org/api/async_context.html#static-method-asyncresourcebindfn-type-thisarg>
    #[static_method(name = "bind", length = 3)]
    pub fn bind_static<'r>(
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        r#type: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, Thrown> {
        bind_new_resource(
            scope,
            callback,
            r#type.unwrap_or(HandleValue::undefined()),
            this_arg.unwrap_or(HandleValue::undefined()),
        )
    }

    /// A snapshot of the async context the resource was created in.
    fn context(&self, scope: &Scope<'_>) -> Snapshot {
        Snapshot::of(self.data().frame.as_ref().map(|frame| frame.get(scope)))
    }
}

/// Node's static `AsyncResource.bind(fn, type, thisArg)`: a new `AsyncResource` of the type
/// `type`, or of `fn`'s name, or `bound-anonymous-fn`, binds `fn`.
fn bind_new_resource<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
    r#type: HandleValue<'_>,
    this_arg: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let callback = validate_function(scope, callback, "fn")?;
    let r#type = if js::conversion::to_boolean(r#type) {
        r#type
    } else {
        let name = callback.get_property(scope, c"name")?;
        if js::conversion::to_boolean(name) {
            name
        } else {
            js::JSString::from_str(scope, "bound-anonymous-fn")?.to_jsval_throwing(scope)?
        }
    };
    let resource = AsyncResource::new(scope, r#type, None)?;
    bind_resource(scope, resource.as_object(), callback, this_arg)
}

/// The state of a function `asyncResource.bind(fn, thisArg)` returns.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Bound {
    resource: Heap<js::object::Object>,
    callback: Heap<js::object::Object>,
    this_arg: Heap<Value>,
    /// The `runInAsyncScope` method of the resource when a `thisArg` was given, which the function
    /// calls instead of looking the method up.
    run: Option<Heap<js::object::Object>>,
    /// The value of the function's `asyncResource` property.
    exposed: Heap<Value>,
}

#[core_runtime::jsmethods]
impl Bound {}

/// Node's `asyncResource.bind(fn, thisArg)`: a function that calls `resource.runInAsyncScope` with
/// `fn`, `thisArg` or the call's `this` if `thisArg` is `undefined`, and its arguments. Its
/// `length` is `fn`'s, and its deprecated `asyncResource` property is `resource`.
fn bind_resource<'s>(
    scope: &'s Scope<'_>,
    resource: Object<'_>,
    callback: Object<'_>,
    this_arg: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let run = if this_arg.is_undefined() {
        None
    } else {
        let run = resource.get_property(scope, c"runInAsyncScope")?;
        Some(Object::from_value(scope, run).map_err(|_| {
            js::error::TypeError("runInAsyncScope is not a function".into()).throw(scope)
        })?)
    };
    let state = js::class::create_instance_with::<BoundImpl>(scope, |_| BoundImpl {
        resource: Heap::from(resource),
        callback: Heap::from(callback),
        this_arg: Heap::from(this_arg.get()),
        run: run.map(Heap::from),
        exposed: Heap::from(resource.to_jsval_raw_throwing(scope).unwrap_or_default()),
    })?;
    let name = if this_arg.is_undefined() {
        c"bound"
    } else {
        c"bound runInAsyncScope"
    };
    let bound = Function::new_callback(scope, name, 0, call_bound, state)?;
    let length = callback.get_property(scope, c"length")?;
    let readonly = js::class_spec::JSPROP_READONLY as u32;
    bound.define_property(scope, c"length", length, readonly)?;
    let getter = Function::new_callback(scope, c"deprecated", 0, get_async_resource, state)?;
    let setter = Function::new_callback(scope, c"deprecated", 1, set_async_resource, state)?;
    let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
    bound.define_accessor(
        scope,
        c"asyncResource",
        Some(*getter),
        Some(*setter),
        enumerable,
    )?;
    Ok(*bound)
}

/// The body of a function [`bind_resource`] returns, whose payload is its [`Bound`] state.
fn call_bound(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<Bound>(scope, payload);
    let (resource, callback, this_arg, run) = {
        let data = state.data();
        (
            data.resource.get(scope),
            data.callback.get(scope),
            data.this_arg.get(scope),
            data.run.as_ref().map(|run| run.get(scope)),
        )
    };
    let this_arg = if run.is_some() {
        this_arg
    } else {
        scope.root_value(args.this())
    };
    let mut call_args = vec![callback.to_jsval_throwing(scope)?, this_arg];
    call_args.extend(args_from(&args, 0));
    let result = match run {
        Some(run) => Function::call(scope, resource, run, &call_args)?,
        None => Function::call_by_name(scope, resource.handle(), c"runInAsyncScope", &call_args)?,
    };
    Ok(result.get())
}

/// The getter of a bound function's `asyncResource`, whose payload is its [`Bound`] state: emit
/// `DEP0172`, and return the value.
fn get_async_resource(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    crate::process::deprecate_once(
        scope,
        "The asyncResource property on bound functions is deprecated",
        "DEP0172",
    )?;
    Ok(cast_payload::<Bound>(scope, payload)
        .data()
        .exposed
        .get(scope)
        .get())
}

/// The setter of a bound function's `asyncResource`, whose payload is its [`Bound`] state: emit
/// `DEP0172`, and replace the value the getter returns.
fn set_async_resource(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    crate::process::deprecate_once(
        scope,
        "The asyncResource property on bound functions is deprecated",
        "DEP0172",
    )?;
    cast_payload::<Bound>(scope, payload)
        .data()
        .exposed
        .set(args.get(0).get());
    Ok(js::value::undefined())
}

/// A hook `createHook` returns, whose `enable()` and `disable()` return it.
#[core_runtime::jsclass(name = "AsyncHook", hidden)]
pub(crate) struct AsyncHook {}

#[core_runtime::jsmethods]
impl AsyncHook {}

/// Register the classes of `node:async_hooks` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    AsyncContextFrame::add_to_global(scope, global);
    AsyncLocalStorage::add_to_global(scope, global);
    RunScope::add_to_global(scope, global);
    AsyncResource::add_to_global(scope, global);
    Bound::add_to_global(scope, global);
    AsyncHook::add_to_global(scope, global);
    let hook_proto = js::class::get_prototype_object_for::<AsyncHookImpl>(scope)
        .expect("AsyncHook was registered above");
    for name in [c"enable", c"disable"] {
        let method = Function::new_callback(scope, name, 0, return_this, ())?;
        hook_proto.define_property(scope, name, *method, 0)?;
    }
    let proto = js::class::get_prototype_object_for::<RunScopeImpl>(scope)
        .expect("RunScope was registered above");
    let dispose = Function::new_callback(scope, c"[Symbol.dispose]", 0, dispose_run_scope, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        proto,
        js::native::SymbolCode::dispose,
        *dispose,
    )
}

/// The `enable()` and `disable()` of an `AsyncHook`: return `this`.
fn return_this(
    _scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(args.this())
}

/// The key of the realm's `executionAsyncResource()` object.
struct ExecutionAsyncResource;

#[core_runtime::jsmodule(node_args, name = "node:async_hooks", default_namespace)]
pub mod module {
    use super::*;

    pub use super::AsyncLocalStorage;
    pub use super::AsyncResource;

    /// <https://nodejs.org/api/async_hooks.html#async_hooksexecutionasyncid>: always 1.
    pub fn execution_async_id() -> f64 {
        1.0
    }

    /// <https://nodejs.org/api/async_hooks.html#async_hookstriggerasyncid>: always 0.
    pub fn trigger_async_id() -> f64 {
        0.0
    }

    /// <https://nodejs.org/api/async_hooks.html#async_hookscreatehookcallbacks>: a hook whose
    /// `enable()` and `disable()` return it, and whose callbacks are never called. Each callback
    /// must be a function or `undefined`.
    pub fn create_hook<'s>(
        scope: &'s Scope<'_>,
        callbacks: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let callbacks = Object::from_value(scope, callbacks).map_err(|e| e.throw(scope))?;
        for (name, qualified) in [
            (c"init", "hook.init"),
            (c"before", "hook.before"),
            (c"after", "hook.after"),
            (c"destroy", "hook.destroy"),
            (c"promiseResolve", "hook.promiseResolve"),
        ] {
            let callback = callbacks.get_property(scope, name)?;
            if !callback.is_undefined() && !js::function::is_callable(callback) {
                return Err(NodeError::new(
                    ErrorKind::TypeError,
                    "ERR_ASYNC_CALLBACK",
                    format!("{qualified} must be a function"),
                )
                .into());
            }
        }
        Ok(
            js::class::create_instance_with::<AsyncHookImpl>(scope, |_| AsyncHookImpl {})?
                .as_object(),
        )
    }

    /// <https://nodejs.org/api/async_hooks.html#async_hooksexecutionasyncresource>: an empty
    /// object, the same one on every call in a realm.
    pub fn execution_async_resource<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        if let Some(resource) = js::class::realm_object::<ExecutionAsyncResource>(scope) {
            return Ok(resource);
        }
        let resource = Object::new_plain(scope)?;
        js::class::register_realm_object::<ExecutionAsyncResource>(scope, resource);
        Ok(resource)
    }
}
