// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:domain`. See <https://nodejs.org/api/domain.html>.
//!
//! Loading the module creates the realm's [`DomainState`]: the stack of entered domains,
//! `domain.active` and `process.domain`, as in Node's `lib/domain.js`. An async context frame
//! holds the domain `process.domain` was when the context was set. The
//! [`js::async_context::CallbackHooks`] enter that domain before each callback that runs with the
//! context and exit it afterwards, as the async hook of Node's module does.
//!
//! While a domain on the stack has `error` listeners, an uncaught exception goes to the
//! `_errorHandler` of `process.domain` instead of `uncaughtException`, as through the uncaught
//! exception capture callback Node's module installs. Whether a domain has listeners is checked
//! when an exception is uncaught, not when listeners change.

use std::cell::Cell;
use std::ffi::CStr;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};

js::instance_local! {
    /// Set once a realm loaded `node:domain`.
    static IN_USE: Cell<bool> = const { Cell::new(false) };
}

/// The realm's state of `node:domain`, which loading the module creates.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DomainState {
    /// The entered domains, the innermost last. Node's `domain._stack`.
    stack: Vec<Heap<js::object::Object>>,
    /// `domain.active`.
    active: Heap<Value>,
    /// `process.domain`.
    process_domain: Heap<Value>,
    /// For each callback the `before` hook ran for and the `after` hook didn't yet, the domain the
    /// hook entered, or `None` if it entered none. The innermost callback is last.
    entered: Vec<Option<Heap<js::object::Object>>>,
    /// The stack of the call that loaded the module.
    require_stack: Heap<Value>,
    /// Node's `updateExceptionCapture`, which each domain listens to `newListener` and
    /// `removeListener` with.
    update_capture: Heap<js::object::Object>,
    /// Node's `domainUncaughtExceptionClear`.
    uncaught_clear: Heap<js::object::Object>,
}

#[core_runtime::jsmethods]
impl DomainState {}

/// Marker keying the realm's [`DomainState`].
struct DomainStateObject;

/// The realm's [`DomainState`], if the realm loaded `node:domain`.
fn state<'s>(scope: &'s Scope<'_>) -> Option<DomainState<'s>> {
    if !IN_USE.get() {
        return None;
    }
    js::class::realm_object::<DomainStateObject>(scope)
        .and_then(|state| state.cast::<DomainState>().ok())
}

/// Whether some realm loaded `node:domain`. Until then, emitters and timers have no domain.
pub(crate) fn in_use() -> bool {
    IN_USE.get()
}

/// Whether the realm loaded `node:domain`, which makes
/// `process.setUncaughtExceptionCaptureCallback()` throw.
pub(crate) fn loaded(scope: &Scope<'_>) -> bool {
    state(scope).is_some()
}

/// Whether `value` is a `Domain`.
fn is_domain(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    domain_class::is_instance(scope, value)
}

/// Whether `value` is `object`.
fn is_object(value: HandleValue<'_>, object: Object<'_>) -> bool {
    value.is_object() && value.to_object() == object.as_raw()
}

/// Define `object[name]` as Node's `ObjectDefineProperty` with `value`, `writable` and
/// `configurable` does: a non-enumerable data property.
fn define_hidden(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static CStr,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    object.define_property(scope, name, value, 0)
}

/// Set `domain.active` and `process.domain` to `active`, and make the current async context hold
/// it as [`sync_context_domain`] does.
fn set_active(
    scope: &Scope<'_>,
    state: DomainState<'_>,
    active: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    {
        let data = state.data();
        data.active.set(active.get());
        data.process_domain.set(active.get());
    }
    sync_context_domain(scope, state)
}

/// The domains on the stack, the innermost last.
fn stack<'s>(scope: &'s Scope<'_>, state: DomainState<'_>) -> Vec<Object<'s>> {
    state
        .data()
        .stack
        .iter()
        .map(|domain| domain.get(scope))
        .collect()
}

/// Node's `Domain.prototype.enter`: push `domain` on the stack and make it active.
fn enter(scope: &Scope<'_>, state: DomainState<'_>, domain: Object<'_>) -> Result<(), ExnThrown> {
    state.data_mut().stack.push(Heap::from(domain));
    let value = domain.to_jsval_throwing(scope)?;
    set_active(scope, state, value)
}

/// Node's `Domain.prototype.exit`: if `domain` is on the stack, remove its innermost entry and the
/// domains entered after it, and make the domain below it active, or `undefined` if none is left.
fn exit(scope: &Scope<'_>, state: DomainState<'_>, domain: Object<'_>) -> Result<(), ExnThrown> {
    let top = {
        let mut data = state.data_mut();
        let Some(index) = data
            .stack
            .iter()
            .rposition(|entered| entered.get(scope).as_raw() == domain.as_raw())
        else {
            return Ok(());
        };
        data.stack.truncate(index);
        data.stack.last().map(|top| top.get(scope))
    };
    let active = match top {
        Some(top) => top.to_jsval_throwing(scope)?,
        None => HandleValue::undefined(),
    };
    set_active(scope, state, active)
}

/// Node's `domainUncaughtExceptionClear`: empty the stack, and set `domain.active` and
/// `process.domain` to `null`.
fn clear_stack(scope: &Scope<'_>, state: DomainState<'_>) -> Result<(), ExnThrown> {
    state.data_mut().stack.clear();
    set_active(scope, state, HandleValue::null())
}

/// Make the current async context hold `process.domain` if it is a `Domain` that isn't a proxy,
/// or no domain otherwise, without running JavaScript.
fn sync_context_domain(scope: &Scope<'_>, state: DomainState<'_>) -> Result<(), ExnThrown> {
    let process_domain = state.data().process_domain.get(scope);
    let domain = Object::from_value(scope, process_domain)
        .ok()
        .filter(|domain| !domain.is_proxy());
    let domain = match domain {
        Some(domain) if is_domain(scope, process_domain)? => Some(domain),
        _ => None,
    };
    crate::async_hooks::enter_domain_frame(scope, domain)
}

/// The `before` callback hook: enter the domain the callback's async context holds, if it holds
/// one, and make the context hold `process.domain`.
fn before_callback(scope: &Scope<'_>) {
    let Some(state) = state(scope) else {
        return;
    };
    let domain = crate::async_hooks::current_domain(scope);
    state.data_mut().entered.push(domain.map(Heap::from));
    let entered = match domain {
        Some(domain) => enter(scope, state, domain),
        None => sync_context_domain(scope, state),
    };
    if entered.is_err() {
        js::exception::report_and_clear(scope, "entering a domain");
    }
}

/// The `after` callback hook: exit the domain the `before` hook entered for the callback, if it
/// entered one, and make the restored context hold `process.domain`.
fn after_callback(scope: &Scope<'_>) {
    let Some(state) = state(scope) else {
        return;
    };
    let domain = state
        .data_mut()
        .entered
        .pop()
        .flatten()
        .map(|domain| domain.take(scope));
    let exited = match domain {
        Some(domain) => exit(scope, state, domain),
        None => sync_context_domain(scope, state),
    };
    if exited.is_err() {
        js::exception::report_and_clear(scope, "exiting a domain");
    }
}

/// Whether a domain on the stack has `error` listeners, in which case an uncaught exception goes
/// to [`handle_uncaught`].
pub(crate) fn captures_exceptions(scope: &Scope<'_>) -> Result<bool, ExnThrown> {
    let Some(state) = state(scope) else {
        return Ok(false);
    };
    for domain in stack(scope, state) {
        if error_listener_count(scope, domain)? > 0.0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `domain.listenerCount('error')`.
fn error_listener_count(scope: &Scope<'_>, domain: Object<'_>) -> Result<f64, ExnThrown> {
    let error = js::class::get_or_init_atom(scope, c"error")?;
    crate::events::listener_count(scope, domain, error)
}

/// Node's domain capture callback: `process.domain._errorHandler(error)`.
pub(crate) fn handle_uncaught(scope: &Scope<'_>, error: HandleValue<'_>) -> Result<(), ExnThrown> {
    let process_domain = match state(scope) {
        Some(state) => state.data().process_domain.get(scope),
        None => HandleValue::undefined(),
    };
    let process_domain = Object::from_value(scope, process_domain).map_err(|e| e.throw(scope))?;
    Function::call_by_name(scope, process_domain.handle(), c"_errorHandler", &[error])?;
    Ok(())
}

/// The domain a rejected promise was rejected in: the domain the current async context holds.
/// Node's `emitUnhandledRejection` emits the rejection as an `error` event on it.
pub(crate) fn rejection_domain<'s>(scope: &'s Scope<'_>) -> Option<Object<'s>> {
    state(scope)?;
    crate::async_hooks::current_domain(scope)
}

/// Node's `EventEmitter.init` of a loaded `node:domain`: define `emitter.domain`, which is
/// `domain.active` if one is set and `emitter` isn't a `Domain`, and `null` otherwise.
pub(crate) fn init_emitter(scope: &Scope<'_>, emitter: Object<'_>) -> Result<(), ExnThrown> {
    let Some(state) = state(scope) else {
        return Ok(());
    };
    define_hidden(scope, emitter, c"domain", HandleValue::null())?;
    let active = state.data().active.get(scope);
    if js::conversion::to_boolean(active) && !is_domain(scope, emitter.to_jsval_throwing(scope)?)? {
        emitter.set_property(scope, c"domain", active)?;
    }
    Ok(())
}

/// Define `resource.domain` as `process.domain` if that is set, as Node's async hook does for
/// each async resource created while a domain is active.
pub(crate) fn init_resource(scope: &Scope<'_>, resource: Object<'_>) -> Result<(), ExnThrown> {
    let Some(state) = state(scope) else {
        return Ok(());
    };
    let process_domain = state.data().process_domain.get(scope);
    if process_domain.is_null_or_undefined() {
        return Ok(());
    }
    define_hidden(scope, resource, c"domain", process_domain)
}

/// Node's domain-aware `EventEmitter.prototype.emit`: emit `name` with `args` on `emitter`. An
/// emitter with a domain emits an `error` event it has no listeners for on the domain instead, as
/// [`emit_error_on_domain`] does, and its other events with the domain entered. Returns `None` for
/// an emit that `node:domain` doesn't change: one on `process`, on an emitter without a domain, or
/// of an `error` event with listeners.
pub(crate) fn emit(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    name: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Option<bool>, Thrown> {
    let Some(state) = state(scope) else {
        return Ok(None);
    };
    let domain = emitter.get_property(scope, c"domain")?;
    let is_error = name.is_string()
        && js::JSString::from_value(scope, *name)?.equals_ascii(scope, c"error")?;
    if is_error && crate::events::listener_count(scope, emitter, name)? > 0.0 {
        return Ok(None);
    }
    if domain.is_null_or_undefined() || is_object(crate::process::process_value(scope)?, emitter) {
        return Ok(None);
    }
    let domain = Object::from_value(scope, domain).map_err(|e| e.throw(scope))?;
    if is_error {
        emit_error_on_domain(scope, state, emitter, domain, args.first().copied())?;
        return Ok(Some(false));
    }
    Function::call_by_name(scope, domain.handle(), c"enter", js::function::EmptyArgs)?;
    let emitted = crate::events::emit_event(scope, emitter, name, args)?;
    Function::call_by_name(scope, domain.handle(), c"exit", js::function::EmptyArgs)?;
    Ok(Some(emitted))
}

/// The `error` branch of Node's domain-aware `emit`: emit `error`, or `ERR_UNHANDLED_ERROR` if it
/// is missing or falsy, on `domain`, the domain of `emitter`, with the active domain and its
/// adjacent duplicates removed from the top of the stack. The stack and the active domain are
/// restored afterwards, unless the `error` listener throws.
fn emit_error_on_domain(
    scope: &Scope<'_>,
    state: DomainState<'_>,
    emitter: Object<'_>,
    domain: Object<'_>,
    error: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let error = match error.filter(|error| js::conversion::to_boolean(*error)) {
        Some(error) => error,
        None => NodeError::new(ErrorKind::Error, "ERR_UNHANDLED_ERROR", "Unhandled error.")
            .to_value(scope)?,
    };
    let object = Object::from_value(scope, error)
        .ok()
        .filter(|object| !object.is_callable());
    if let Some(object) = object {
        object.set_property(scope, c"domainEmitter", emitter)?;
        define_hidden(scope, object, c"domain", domain.to_jsval_throwing(scope)?)?;
        object.set_property(scope, c"domainThrown", false)?;
    }
    let original_stack = stack(scope, state);
    let original_active = state.data().process_domain.get(scope);
    let kept = original_stack
        .iter()
        .rposition(|entered| !is_object(original_active, *entered))
        .map_or(0, |index| index + 1);
    state.data_mut().stack.truncate(kept);
    let active = match original_stack[..kept].last() {
        Some(top) => top.to_jsval_throwing(scope)?,
        None => HandleValue::null(),
    };
    set_active(scope, state, active)?;
    crate::events::emit(scope, domain, c"error", &[error])?;
    state.data_mut().stack = original_stack
        .iter()
        .map(|domain| Heap::from(*domain))
        .collect();
    set_active(scope, state, original_active)?;
    Ok(())
}

/// The state of a function `domain.bind(callback)` or `domain.intercept(callback)` returns.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DomainCallback {
    domain: Heap<js::object::Object>,
    callback: Heap<Value>,
}

#[core_runtime::jsmethods]
impl DomainCallback {}

/// The domain and the callback of a function `bind` or `intercept` returned, whose payload is its
/// [`DomainCallback`].
fn bound_parts<'s>(
    scope: &'s Scope<'_>,
    payload: HandleValue<'_>,
) -> (Object<'s>, HandleValue<'s>) {
    let state = cast_payload::<DomainCallback>(scope, payload);
    let data = state.data();
    (data.domain.get(scope), data.callback.get(scope))
}

/// Call `callback` with `this` and `args` while `domain` is entered, as Node's `bound` does.
fn call_entered(
    scope: &Scope<'_>,
    domain: Object<'_>,
    callback: HandleValue<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    Function::call_by_name(scope, domain.handle(), c"enter", js::function::EmptyArgs)?;
    let result = Function::call(scope, this, callback, args)?;
    Function::call_by_name(scope, domain.handle(), c"exit", js::function::EmptyArgs)?;
    Ok(result.get())
}

/// The body of `runBound`, the function `domain.bind(callback)` returns, whose payload is its
/// [`DomainCallback`].
fn run_bound(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (domain, callback) = bound_parts(scope, payload);
    let this = scope.root_value(args.this());
    call_entered(scope, domain, callback, this, &args.rest(0))
}

/// The body of `runIntercepted`, the function `domain.intercept(callback)` returns, whose payload
/// is its [`DomainCallback`]: emit a first argument that is an `Error` on the domain, or call the
/// callback with the other arguments.
fn run_intercepted(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (domain, callback) = bound_parts(scope, payload);
    let first = args.get(0);
    let error_class = crate::stack_trace::error_constructor(scope)?;
    if js::conversion::to_boolean(first) && error_class.instance_of(scope, first)? {
        let error = Object::from_value(scope, first).map_err(|e| e.throw(scope))?;
        error.set_property(scope, c"domainBound", callback)?;
        error.set_property(scope, c"domainThrown", false)?;
        define_hidden(scope, error, c"domain", domain.to_jsval_throwing(scope)?)?;
        crate::events::emit(scope, domain, c"error", &[first])?;
        return Ok(js::value::undefined());
    }
    let this = scope.root_value(args.this());
    call_entered(scope, domain, callback, this, &args.rest(1))
}

/// A function named `name` running `body` with a [`DomainCallback`] of `domain` and `callback`.
fn domain_callback<'s>(
    scope: &'s Scope<'_>,
    name: &'static CStr,
    body: js::function::Callback,
    domain: Object<'_>,
    callback: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let state =
        js::class::create_instance_with::<DomainCallbackImpl>(scope, |_| DomainCallbackImpl {
            domain: Heap::from(domain),
            callback: Heap::from(callback.get()),
        })?;
    Ok(*Function::new_callback(scope, name, 0, body, state)?)
}

/// `domain.members` as an array.
fn members<'s>(scope: &'s Scope<'_>, domain: Object<'_>) -> Result<js::Array<'s>, ExnThrown> {
    let members = domain.get_property(scope, c"members")?;
    Object::from_value(scope, members)
        .ok()
        .and_then(|members| members.cast::<js::Array>().ok())
        .ok_or_else(|| js::error::throw_type_error(scope, c"domain.members is not an array"))
}

/// `value` as an object, or a `TypeError` naming `what`.
fn object_argument<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    what: &str,
) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, *value)
        .map_err(|_| js::error::TypeError(format!("{what} is not an object")).throw(scope))
}

#[core_runtime::jsfunction_class(
    name = "Domain",
    class_syntax,
    extends = "crate::events::event_emitter"
)]
pub mod domain_class {
    use super::*;

    /// <https://nodejs.org/api/domain.html#class-domain>: an `EventEmitter` with no members,
    /// listening to `newListener` and `removeListener` with Node's `updateExceptionCapture`.
    #[constructor(length = 0)]
    pub fn init<'s>(scope: &'s Scope<'_>, this: Object<'s>) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        this.set_property(scope, c"members", *js::Array::new(scope, 0)?)?;
        let Some(state) = state(scope) else {
            return Ok(());
        };
        let update = state
            .data()
            .update_capture
            .get(scope)
            .to_jsval_throwing(scope)?;
        for event in [c"removeListener", c"newListener"] {
            let event = js::class::get_or_init_atom(scope, event)?;
            crate::events::add_listener(scope, this, event, update, false)?;
        }
        Ok(())
    }

    /// Node's `Domain.prototype._errorHandler(error)`
    /// (<https://nodejs.org/api/domain.html#special-error-properties>), which handles an uncaught
    /// exception thrown while the domain was active: set `error.domain` and `error.domainThrown`,
    /// exit the domain while it is active, emit `error` on it, and clear the stack. With other
    /// domains left on the stack, an exception an `error` listener throws goes to the
    /// `_errorHandler` of the domain on top. Returns whether the domain had `error` listeners, or
    /// what that `_errorHandler` returned.
    #[method(name = "_errorHandler")]
    pub fn error_handler(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let _guard = crate::errors::check_stack(scope)?;
        let Some(state) = state(scope) else {
            return Ok(false);
        };
        if let Ok(object) = Object::from_value(scope, error) {
            define_hidden(scope, object, c"domain", this.to_jsval_throwing(scope)?)?;
            object.set_property(scope, c"domainThrown", true)?;
        }
        loop {
            let active = state.data().active.get(scope);
            let on_stack = state
                .data()
                .stack
                .iter()
                .any(|entered| entered.get(scope).as_raw() == this.as_raw());
            if !is_object(active, this) || !on_stack {
                break;
            }
            super::exit(scope, state, this)?;
        }
        let mut caught = false;
        if state.data().stack.is_empty() {
            if error_listener_count(scope, this)? > 0.0 {
                caught = crate::events::emit(scope, this, c"error", &[error])?;
            }
        } else {
            match crate::events::emit(scope, this, c"error", &[error]) {
                Ok(emitted) => caught = emitted,
                Err(ExnThrown) => {
                    let thrown = js::exception::take_pending_or_undefined(scope);
                    let top = state.data().stack.last().map(|top| top.get(scope));
                    let Some(top) = top else {
                        js::exception::throw_value(scope, thrown);
                        return Err(Thrown::Pending);
                    };
                    set_active(scope, state, top.to_jsval_throwing(scope)?)?;
                    let handled =
                        Function::call_by_name(scope, top.handle(), c"_errorHandler", &[thrown])?;
                    caught = js::conversion::to_boolean(handled);
                }
            }
        }
        clear_stack(scope, state)?;
        Ok(caught)
    }

    /// <https://nodejs.org/api/domain.html#domainenter>
    pub fn enter(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        match state(scope) {
            Some(state) => super::enter(scope, state, this),
            None => Ok(()),
        }
    }

    /// <https://nodejs.org/api/domain.html#domainexit>: does nothing if the domain isn't on the
    /// stack.
    pub fn exit(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        match state(scope) {
            Some(state) => super::exit(scope, state, this),
            None => Ok(()),
        }
    }

    /// <https://nodejs.org/api/domain.html#domainaddemitter>: does nothing if `emitter` is a
    /// `Domain` this domain is a member of, directly or through other domains. Timers get the
    /// `domain` property, and their callbacks still run in the domain they were created in.
    pub fn add(
        scope: &Scope<'_>,
        this: Object<'_>,
        emitter: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let emitter = object_argument(scope, emitter, "emitter")?;
        let current = emitter.get_property(scope, c"domain")?;
        if is_object(current, this) {
            return Ok(());
        }
        if let Ok(current) = Object::from_value(scope, current) {
            let emitter = emitter.to_jsval_throwing(scope)?;
            Function::call_by_name(scope, current.handle(), c"remove", &[emitter])?;
        }
        let own = this.get_property(scope, c"domain")?;
        if js::conversion::to_boolean(own) && is_domain(scope, emitter.to_jsval_throwing(scope)?)? {
            let mut link = own;
            while js::conversion::to_boolean(link) {
                if is_object(link, emitter) {
                    return Ok(());
                }
                link = match Object::from_value(scope, link) {
                    Ok(object) => object.get_property(scope, c"domain")?,
                    Err(_) => HandleValue::undefined(),
                };
            }
        }
        define_hidden(scope, emitter, c"domain", this.to_jsval_throwing(scope)?)?;
        members(scope, this)?.push(scope, emitter)
    }

    /// <https://nodejs.org/api/domain.html#domainremoveemitter>
    pub fn remove(
        scope: &Scope<'_>,
        this: Object<'_>,
        emitter: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let emitter = object_argument(scope, emitter, "emitter")?;
        emitter.set_property(scope, c"domain", HandleValue::null())?;
        let members = members(scope, this)?;
        let position = members
            .to_vec(scope)?
            .iter()
            .position(|member| is_object(*member, emitter));
        if let Some(position) = position {
            members.remove(scope, position as u32, 1)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/domain.html#domainrunfn-args>: call `fn` with the domain as `this`
    /// and `args` while the domain is entered. The domain stays entered if `fn` throws.
    #[method(length = 1)]
    pub fn run<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        Function::call_by_name(scope, this.handle(), c"enter", js::function::EmptyArgs)?;
        let result = Function::call(scope, this, callback, &args)?;
        let result = scope.root_value(result.get());
        Function::call_by_name(scope, this.handle(), c"exit", js::function::EmptyArgs)?;
        Ok(result)
    }

    /// <https://nodejs.org/api/domain.html#domaininterceptcallback>: a function that emits a
    /// first argument that is an `Error` on the domain, with its `domainBound` set to `callback`,
    /// and otherwise calls `callback` with the other arguments while the domain is entered.
    pub fn intercept<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        domain_callback(scope, c"runIntercepted", run_intercepted, this, callback)
    }

    /// <https://nodejs.org/api/domain.html#domainbindcallback>: a function that calls `callback`
    /// while the domain is entered, whose `domain` property is the domain.
    pub fn bind<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        let bound = domain_callback(scope, c"runBound", run_bound, this, callback)?;
        define_hidden(scope, bound, c"domain", this.to_jsval_throwing(scope)?)?;
        Ok(bound)
    }

    /// Make the prototype's members enumerable, as Node assigns them, in Node's order, with
    /// `members` as `undefined`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
        proto.define_property(scope, c"members", HandleValue::undefined(), enumerable)?;
        for name in [
            c"_errorHandler",
            c"enter",
            c"exit",
            c"add",
            c"remove",
            c"run",
            c"intercept",
            c"bind",
        ] {
            let method = proto.get_property(scope, name)?;
            proto.delete_property(scope, name)?;
            proto.define_property(scope, name, method, enumerable)?;
        }
        Ok(())
    }
}

/// The getter of `process.domain`, the active domain
/// (<https://nodejs.org/api/domain.html#implicit-binding>).
fn get_process_domain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(match state(scope) {
        Some(state) => state.data().process_domain.get(scope).get(),
        None => js::value::null(),
    })
}

/// The setter of `process.domain`, which sets the domain the async resources created from now on
/// enter, without changing the stack or `domain.active`.
fn set_process_domain(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = state(scope) {
        state.data().process_domain.set(args.get(0).get());
        sync_context_domain(scope, state)?;
    }
    Ok(js::value::undefined())
}

/// The getter of `domain.active`, the active domain
/// (<https://nodejs.org/api/domain.html#implicit-binding>).
fn get_active(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(match state(scope) {
        Some(state) => state.data().active.get(scope).get(),
        None => js::value::null(),
    })
}

/// The setter of `domain.active`, which doesn't change `process.domain`.
fn set_active_property(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = state(scope) {
        state.data().active.set(args.get(0).get());
    }
    Ok(js::value::undefined())
}

/// The getter of `domain._stack`: a new array of the entered domains, the innermost last
/// (<https://nodejs.org/api/domain.html#domainenter>).
fn get_stack(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let domains = match state(scope) {
        Some(state) => stack(scope, state),
        None => Vec::new(),
    };
    let values = domains
        .iter()
        .map(|domain| domain.to_jsval_throwing(scope))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(js::Array::with_contents(scope, &values)?.as_value())
}

/// Node's `updateExceptionCapture` listener. Whether domains capture uncaught exceptions is
/// checked when an exception is uncaught, so the listener does nothing.
fn update_exception_capture(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// Node's `domainUncaughtExceptionClear`, the first `uncaughtException` listener of `process`
/// while it has others: empty the stack, and set `domain.active` and `process.domain` to `null`.
fn domain_uncaught_exception_clear(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(state) = state(scope) {
        clear_stack(scope, state)?;
    }
    Ok(js::value::undefined())
}

/// Whether `name` is the string `uncaughtException` and `listener` isn't `clear`.
fn is_other_uncaught_listener(
    scope: &Scope<'_>,
    name: HandleValue<'_>,
    listener: HandleValue<'_>,
    clear: Object<'_>,
) -> Result<bool, ExnThrown> {
    Ok(name.is_string()
        && js::JSString::from_value(scope, *name)?.equals_ascii(scope, c"uncaughtException")?
        && !is_object(listener, clear))
}

/// The `newListener` listener of `process`, whose payload is `domainUncaughtExceptionClear`: make
/// it the first `uncaughtException` listener when another one is added.
fn on_process_new_listener(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let clear = Object::from_value(scope, *payload).map_err(|e| e.throw(scope))?;
    let (name, listener) = (args.get(0), args.get(1));
    if is_other_uncaught_listener(scope, name, listener, clear)? {
        let process = crate::process::global_process(scope)?;
        let clear = clear.to_jsval_throwing(scope)?;
        Function::call_by_name(scope, process.handle(), c"removeListener", &[name, clear])?;
        Function::call_by_name(scope, process.handle(), c"prependListener", &[name, clear])?;
    }
    Ok(js::value::undefined())
}

/// The `removeListener` listener of `process`, whose payload is `domainUncaughtExceptionClear`:
/// remove it once it is the only `uncaughtException` listener left.
fn on_process_remove_listener(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let clear = Object::from_value(scope, *payload).map_err(|e| e.throw(scope))?;
    let (name, listener) = (args.get(0), args.get(1));
    if is_other_uncaught_listener(scope, name, listener, clear)? {
        let process = crate::process::global_process(scope)?;
        let listeners = Function::call_by_name(scope, process.handle(), c"listeners", &[name])?;
        let listeners = Object::from_value(scope, listeners)
            .ok()
            .and_then(|listeners| listeners.cast::<js::Array>().ok());
        if let Some(listeners) = listeners {
            let listeners = listeners.to_vec(scope)?;
            if listeners.len() == 1 && is_object(listeners[0], clear) {
                let clear = clear.to_jsval_throwing(scope)?;
                Function::call_by_name(scope, process.handle(), c"removeListener", &[name, clear])?;
            }
        }
    }
    Ok(js::value::undefined())
}

/// <https://nodejs.org/api/domain.html#domaincreate>: `domain.create()` and
/// `domain.createDomain()` return a new `Domain`.
fn create_domain(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let constructor = domain_class::class_constructor(scope)?;
    let domain = Function::construct(scope, constructor, js::function::EmptyArgs)?;
    Ok(domain.as_value())
}

/// The `ERR_DOMAIN_CANNOT_SET_UNCAUGHT_EXCEPTION_CAPTURE` that
/// `process.setUncaughtExceptionCaptureCallback()` throws once the realm loaded `node:domain`,
/// whose stack ends with the stack of the call that loaded it.
pub(crate) fn cannot_set_capture_error(scope: &Scope<'_>) -> ExnThrown {
    let error = NodeError::new(
        ErrorKind::Error,
        "ERR_DOMAIN_CANNOT_SET_UNCAUGHT_EXCEPTION_CAPTURE",
        "The `domain` module is in use, which is mutually exclusive with calling \
         process.setUncaughtExceptionCaptureCallback()",
    );
    let decorated = (|| -> Result<HandleValue<'_>, ExnThrown> {
        let error = error.to_value(scope)?;
        let object = Object::from_value(scope, error).map_err(|e| e.throw(scope))?;
        let stack = object.get_property(scope, c"stack")?;
        let stack = crate::errors::js_string(scope, stack)?;
        let require_stack = match state(scope) {
            Some(state) => state.data().require_stack.get(scope),
            None => HandleValue::undefined(),
        };
        let require_stack = crate::errors::js_string(scope, require_stack)?;
        let dashes = "-".repeat(40);
        let combined = format!("{stack}\n{dashes}\n{require_stack}");
        object.set_property(scope, c"stack", combined.as_str())?;
        Ok(error)
    })();
    match decorated {
        Ok(error) => js::exception::throw_value(scope, error),
        Err(thrown) => thrown,
    }
}

/// Load `node:domain` in the current realm and register it as builtin module `name`. Throws
/// `ERR_DOMAIN_CALLBACK_NOT_AVAILABLE` if `process.setUncaughtExceptionCaptureCallback()` set a
/// callback, and loading again succeeds once it is cleared.
///
/// Loading replaces `process.domain` with an accessor, makes `process` keep
/// `domainUncaughtExceptionClear` as its first `uncaughtException` listener, sets
/// `EventEmitter.usingDomains`, and installs the callback hooks.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    load(scope, name).is_ok()
}

fn load(scope: &Scope<'_>, name: &'static str) -> Result<(), ExnThrown> {
    if crate::process::has_capture_callback(scope) {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_DOMAIN_CALLBACK_NOT_AVAILABLE",
            "A callback was registered through process.setUncaughtExceptionCaptureCallback(), \
             which is mutually exclusive with using the `domain` module",
        )
        .throw(scope));
    }
    let marker = js::error::new_error(
        scope,
        js::error::JSExnType::JSEXN_ERR,
        "require(`domain`) at this point",
    )?;
    let require_stack = Object::from_value(scope, marker)
        .map_err(|e| e.throw(scope))?
        .get_property(scope, c"stack")?;
    let update_capture = Function::new_callback(
        scope,
        c"updateExceptionCapture",
        0,
        update_exception_capture,
        (),
    )?;
    let uncaught_clear = Function::new_callback(
        scope,
        c"domainUncaughtExceptionClear",
        0,
        domain_uncaught_exception_clear,
        (),
    )?;
    let state = js::class::create_instance_with::<DomainStateImpl>(scope, |_| DomainStateImpl {
        stack: Vec::new(),
        active: Heap::from(js::value::null()),
        process_domain: Heap::from(js::value::null()),
        entered: Vec::new(),
        require_stack: Heap::from(require_stack.get()),
        update_capture: Heap::from(*update_capture),
        uncaught_clear: Heap::from(*uncaught_clear),
    })?;
    js::class::register_realm_object::<DomainStateObject>(scope, state.as_object());
    IN_USE.set(true);
    js::async_context::set_callback_hooks(Some(js::async_context::CallbackHooks {
        before: before_callback,
        after: after_callback,
    }));

    let process = crate::process::global_process(scope)?;
    let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
    let getter = Function::new_callback(scope, c"get", 0, get_process_domain, ())?;
    let setter = Function::new_callback(scope, c"set", 1, set_process_domain, ())?;
    process.define_accessor(scope, c"domain", Some(*getter), Some(*setter), enumerable)?;
    let clear = (*uncaught_clear).to_jsval_throwing(scope)?;
    for (event, listener) in [
        (
            c"newListener",
            on_process_new_listener as js::function::Callback,
        ),
        (c"removeListener", on_process_remove_listener),
    ] {
        let listener = Function::new_callback(scope, c"", 2, listener, clear)?;
        crate::events::listen(scope, process, c"on", event, *listener)?;
    }
    let emitter = crate::events::emitter_constructor(scope)?;
    emitter.set_property(scope, c"usingDomains", true)?;

    let exports = Object::new_plain(scope)?;
    let getter = Function::new_callback(scope, c"get", 0, get_stack, ())?;
    exports.define_accessor(scope, c"_stack", Some(*getter), None, enumerable)?;
    let domain = domain_class::class_constructor(scope)?;
    exports.set_property(scope, c"Domain", domain)?;
    let create = Function::new_callback(scope, c"createDomain", 0, create_domain, ())?;
    exports.set_property(scope, c"createDomain", *create)?;
    exports.set_property(scope, c"create", *create)?;
    let getter = Function::new_callback(scope, c"get", 0, get_active, ())?;
    let setter = Function::new_callback(scope, c"set", 1, set_active_property, ())?;
    exports.define_accessor(scope, c"active", Some(*getter), Some(*setter), enumerable)?;

    let stack = (*js::Array::new(scope, 0)?).to_jsval_throwing(scope)?;
    let values = [
        ("Domain", domain.to_jsval_throwing(scope)?),
        ("_stack", stack),
        ("active", HandleValue::null()),
        ("create", (*create).to_jsval_throwing(scope)?),
        ("createDomain", (*create).to_jsval_throwing(scope)?),
        ("default", exports.to_jsval_throwing(scope)?),
    ];
    core_runtime::module::register_synthetic_module(scope, name, &values)
}

/// Register the classes of `node:domain` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    DomainState::add_to_global(scope, global);
    DomainCallback::add_to_global(scope, global);
    domain_class::register(scope)?;
    Ok(())
}
