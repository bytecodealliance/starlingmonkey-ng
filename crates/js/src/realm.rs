// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Globals for code that runs apart from the global that created it, and contextified globals,
//! whose global object forwards the properties it lacks to a sandbox object.
//!
//! [`new_global`] creates a global of [`GLOBAL_CLASS`] in a new realm of the current compartment.
//! Objects pass between the new realm and the current one without wrappers, and each realm has
//! its own standard classes.
//!
//! [`contextify`] makes a global forward to a sandbox:
//!
//! - The global's prototype is a proxy whose target is the sandbox. A property the global lacks
//!   is read from the sandbox if the sandbox has it, and from the realm's `Object.prototype`
//!   otherwise. Assignments to properties the global lacks set them on the sandbox. A value read
//!   from the sandbox that is the sandbox itself reads as the global.
//! - A property added to the global, such as by an assignment to an undeclared variable, is
//!   added to the sandbox instead, and removed from the global unless it is non-configurable. A
//!   data property that is writable, enumerable and configurable is assigned to the sandbox, and
//!   other properties are defined on it with their descriptor. If the sandbox refuses it, the
//!   property stays on the global.
//!
//! Scripts run with the global's [`sandbox_environment`] on their environment chain (see
//! [`compile::execute_script_with_env`](crate::compile::execute_script_with_env)) find the
//! sandbox's properties first. Their `var` and function declarations define properties of the
//! sandbox. A declaration the sandbox refuses, such as one in a non-extensible sandbox that lacks
//! the property, defines the property on the global instead.
//!
//! [`set_code_generation_from_strings`] makes `eval` and `Function` throw in a realm, with the
//! security callback [`install`] sets.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use mozjs::gc::Handle;
use mozjs::glue::{
    CreateWrapperProxyHandler, GetProxyPrivate, GetProxyReservedSlot, IsWrapper, ProxyTraps,
    SetProxyReservedSlot,
};
use mozjs::jsapi::JS::CompartmentSpecifier;
use mozjs::jsapi::{
    jsid, JSClass, JSClassOps, JSContext as RawJSContext, JSObject, JSProtoKey, ObjectOpResult,
    PropertyDescriptor, JSCLASS_FOREGROUND_FINALIZE, JSCLASS_GLOBAL_FLAGS, JSCLASS_IS_GLOBAL,
};
use mozjs::jsval::{ObjectValue, UndefinedValue};
use mozjs::rust::wrappers2;

use crate::error::ExnThrown;
use crate::gc::scope::{OnNewGlobalHookOption, RootScope, Scope};
use crate::Object;

/// The class of the globals [`new_global`] creates: [`STARLING_GLOBAL_CLASS`]'s ops, with an
/// `addProperty` hook that moves properties added to a contextified global to its sandbox.
///
/// [`STARLING_GLOBAL_CLASS`]: crate::class::STARLING_GLOBAL_CLASS
pub static GLOBAL_CLASS: JSClass = JSClass {
    name: c"global".as_ptr(),
    flags: JSCLASS_IS_GLOBAL | JSCLASS_FOREGROUND_FINALIZE | JSCLASS_GLOBAL_FLAGS,
    cOps: &GLOBAL_OPS as *const JSClassOps,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

static GLOBAL_OPS: JSClassOps = crate::class::starling_global_ops(Some(add_property));

/// The native stack [`new_global`] requires. Defining the standard classes of a new global with
/// less can exhaust the stack in the new realm before its `Error` prototype exists, which fails a
/// SpiderMonkey assertion when the over-recursion is reported.
const NEW_GLOBAL_STACK: usize = 256 * 1024;

/// Create a global of [`GLOBAL_CLASS`] in a new realm of the current compartment, with
/// `options`, which this sets to use the current compartment. Its standard classes are all
/// defined before it is returned, and the `onNewGlobalObject` hook of every `Debugger` is fired
/// for it. Throws SpiderMonkey's `InternalError` "too much recursion" if less than
/// [`NEW_GLOBAL_STACK`] of native stack is left.
pub fn new_global<'s>(
    scope: &'s Scope<'_>,
    mut options: crate::engine::RealmOptions,
) -> Result<Object<'s>, ExnThrown> {
    if crate::context::stack_available(scope) < NEW_GLOBAL_STACK {
        return Err(crate::error::throw_internal_error(
            scope,
            c"too much recursion",
        ));
    }
    // SAFETY: a realm is entered, so the current realm is a live realm, whose first field is its
    // compartment, as `JS::shadow::Realm` describes it.
    let compartment = unsafe {
        let realm = mozjs::jsapi::GetCurrentRealmOrNull(scope.raw_cx_no_gc());
        (*realm.cast::<mozjs::jsapi::JS::shadow::Realm>()).compartment_
    };
    options.creationOptions_.compSpec_ = CompartmentSpecifier::ExistingCompartment;
    options.creationOptions_.__bindgen_anon_1.comp_ = compartment;
    // SAFETY: the class is a global class, and the options are valid.
    let global = unsafe {
        wrappers2::JS_NewGlobalObject(
            scope.cx_mut(),
            &GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::DontFireOnNewGlobalHook,
            &*options,
        )
    };
    let global = NonNull::new(global).ok_or(ExnThrown)?;
    let global = scope.root_object(global);
    {
        let realm = RootScope::new_with_realm(
            scope.cx_mut(),
            NonNull::new(global.get()).expect("the global is not null"),
        );
        crate::class::init_standard_classes(&realm)?;
        crate::class::fire_on_new_global_object(&realm, global);
    }
    Ok(Object::from_handle(global).expect("the global is not null"))
}

/// The realm private value of a realm whose code can't compile strings, which
/// [`set_code_generation_from_strings`] sets.
static STRINGS_DISALLOWED: u8 = 0;

/// The security callbacks [`install`] sets.
static SECURITY_CALLBACKS: mozjs::jsapi::JSSecurityCallbacks = mozjs::jsapi::JSSecurityCallbacks {
    contentSecurityPolicyAllows: Some(code_generation_allowed),
    codeForEvalGets: None,
    subsumes: None,
};

/// Set the security callbacks of `cx` that [`set_code_generation_from_strings`] relies on. Call
/// once per context.
pub fn install(cx: &mozjs::context::JSContext) {
    // SAFETY: the callbacks are a static.
    unsafe { wrappers2::JS_SetSecurityCallbacks(cx, &SECURITY_CALLBACKS) };
}

/// Make `eval` and the `Function` constructors of the realm of `global` throw an `EvalError`
/// unless `allowed`. Sets the private value of the realm, which nothing else uses.
pub fn set_code_generation_from_strings(global: Object<'_>, allowed: bool) {
    let private = if allowed {
        ptr::null_mut()
    } else {
        ptr::addr_of!(STRINGS_DISALLOWED)
            .cast_mut()
            .cast::<c_void>()
    };
    // SAFETY: `global` is a live object, whose realm is live.
    unsafe {
        let realm = mozjs::jsapi::GetObjectRealmOrNull(global.as_raw());
        mozjs::jsapi::SetRealmPrivate(realm, private);
    }
}

/// The `contentSecurityPolicyAllows` callback: strings can be compiled unless the current realm
/// is one [`set_code_generation_from_strings`] disallowed it for, where it throws an `EvalError`
/// with V8's message, which Node.js's `vm` reports.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn code_generation_allowed(
    cx: *mut RawJSContext,
    _kind: mozjs::jsapi::JS::RuntimeCode,
    _code: mozjs::jsapi::HandleString,
    _compilation_type: mozjs::jsapi::JS::CompilationType,
    _parameter_strings: mozjs::jsapi::JS::Handle<
        mozjs::jsapi::JS::StackGCVector<
            *mut mozjs::jsapi::JSString,
            mozjs::jsapi::js::TempAllocPolicy,
        >,
    >,
    _body_string: mozjs::jsapi::HandleString,
    _parameter_args: mozjs::jsapi::JS::Handle<
        mozjs::jsapi::JS::StackGCVector<mozjs::jsapi::Value, mozjs::jsapi::js::TempAllocPolicy>,
    >,
    _body_arg: mozjs::jsapi::HandleValue,
    can_compile_strings: *mut bool,
) -> bool {
    // SAFETY: SpiderMonkey passes a valid context and result location.
    unsafe {
        let realm = mozjs::jsapi::GetCurrentRealmOrNull(cx);
        let allowed = realm.is_null()
            || !ptr::eq(
                mozjs::jsapi::GetRealmPrivate(realm).cast::<u8>(),
                &STRINGS_DISALLOWED,
            );
        if !allowed {
            let message = c"Code generation from strings disallowed for this context";
            crate::error::report_eval_error(cx, message);
            return false;
        }
        *can_compile_strings = true;
    }
    true
}

/// Make `global`, which [`new_global`] created, forward the properties it lacks to `sandbox`, as
/// the [module documentation](self) describes. `sandbox` must be in the current compartment.
pub fn contextify(
    scope: &Scope<'_>,
    global: Object<'_>,
    sandbox: Object<'_>,
) -> Result<(), ExnThrown> {
    let realm = RootScope::new_with_realm(
        scope.cx_mut(),
        global.as_non_null().expect("a rooted object is not null"),
    );
    let object_prototype = crate::class::get_class_prototype(&realm, JSProtoKey::JSProto_Object)?;
    let target = realm.root_value(ObjectValue(sandbox.as_raw()));
    // SAFETY: the handler is a proxy handler that lives as long as the process, and the target
    // and prototype are rooted objects of this compartment.
    let proxy = unsafe {
        mozjs::glue::NewProxyObject(
            realm.raw_cx_no_gc(),
            sandbox_handler(),
            target.into(),
            object_prototype.get(),
            ptr::null(),
            false,
        )
    };
    let proxy = realm.root_object(NonNull::new(proxy).ok_or(ExnThrown)?);
    // SAFETY: as above, with `environment_handler`. The environment proxy's prototype is the
    // sandbox's.
    let environment = unsafe {
        mozjs::glue::NewProxyObject(
            realm.raw_cx_no_gc(),
            environment_handler(),
            target.into(),
            ptr::null_mut(),
            ptr::null(),
            true,
        )
    };
    let environment = ObjectValue(NonNull::new(environment).ok_or(ExnThrown)?.as_ptr());
    // SAFETY: `proxy` is a live proxy, whose class has a reserved slot `ENVIRONMENT_SLOT` that its
    // handler doesn't use.
    unsafe { SetProxyReservedSlot(proxy.get(), ENVIRONMENT_SLOT, &environment) };
    global.set_prototype(&realm, proxy)
}

/// The reserved slot of a sandbox proxy that holds its [`sandbox_environment`]. The slot is
/// `undefined` in the other wrappers.
const ENVIRONMENT_SLOT: u32 = 0;

/// The object that scripts run in the contextified global `global` have on their environment
/// chain, and `None` if `global` isn't contextified: a proxy of the realm of `global` that
/// forwards to the sandbox, created once by [`contextify`]. It is the top-level `this` of those
/// scripts. It differs from the sandbox in these operations:
///
/// - A property read from the sandbox whose value is the sandbox itself reads as the proxy.
///   Getters and setters of the sandbox are called with the sandbox as `this`.
/// - A property definition that the sandbox refuses defines the property on `global` instead,
///   unless the sandbox has an own property of that key. A `var` declaration, a data property
///   whose value is `undefined`, does nothing if `global` already has an own property of that
///   key.
pub fn sandbox_environment<'s>(scope: &'s Scope<'_>, global: Object<'_>) -> Option<Object<'s>> {
    // SAFETY: `global` is a live object, its prototype is a live sandbox proxy if it is
    // contextified, and the environment is rooted before anything can GC.
    unsafe {
        sandbox_raw(global.as_raw())?;
        let proxy = mozjs::jsapi::js::GetStaticPrototype(global.as_raw());
        let mut environment = UndefinedValue();
        GetProxyReservedSlot(proxy, ENVIRONMENT_SLOT, &mut environment);
        Object::from_raw(scope, environment.to_object())
    }
}

/// The sandbox `global` forwards to: the target of its prototype if `global` is a global of
/// [`GLOBAL_CLASS`] and its prototype is the sandbox proxy [`contextify`] creates, and `None`
/// otherwise.
pub fn sandbox<'s>(scope: &'s Scope<'_>, global: Object<'_>) -> Option<Object<'s>> {
    // SAFETY: `global` is a live object, and the target is rooted before anything can GC.
    unsafe { sandbox_raw(global.as_raw()).and_then(|sandbox| Object::from_raw(scope, sandbox)) }
}

/// `object`, or a proxy that forwards to it if it is a global, which can't be on an environment
/// chain.
pub fn environment_object<'s>(
    scope: &'s Scope<'_>,
    object: Object<'s>,
) -> Result<Object<'s>, ExnThrown> {
    // SAFETY: `object` is a live object.
    if !unsafe { mozjs::jsapi::JS_IsGlobalObject(object.as_raw()) } {
        return Ok(object);
    }
    let target = scope.root_value(ObjectValue(object.as_raw()));
    // SAFETY: the handler lives as long as the process, and the target is a rooted object of this
    // compartment.
    let proxy = unsafe {
        mozjs::glue::NewProxyObject(
            scope.raw_cx_no_gc(),
            forwarding_handler(),
            target.into(),
            ptr::null_mut(),
            ptr::null(),
            true,
        )
    };
    let proxy = scope.root_object(NonNull::new(proxy).ok_or(ExnThrown)?);
    Ok(Object::from_handle(proxy).expect("the proxy is not null"))
}

/// The sandbox of `global`, as [`sandbox`] describes it.
///
/// # Safety
///
/// `global` must be a live object. The result must be rooted before anything can GC.
unsafe fn sandbox_raw(global: *mut JSObject) -> Option<*mut JSObject> {
    unsafe {
        if !ptr::eq(crate::object::get_object_class(global), &GLOBAL_CLASS) {
            return None;
        }
        let proto = mozjs::jsapi::js::GetStaticPrototype(global);
        if proto.is_null() || !IsWrapper(proto) {
            return None;
        }
        // The sandbox proxies are the wrappers whose `ENVIRONMENT_SLOT` holds an object.
        let mut environment = UndefinedValue();
        GetProxyReservedSlot(proto, ENVIRONMENT_SLOT, &mut environment);
        environment.is_object().then(|| target_of(proto))
    }
}

/// The target of the sandbox proxy or environment proxy `proxy`.
///
/// # Safety
///
/// `proxy` must be a live sandbox proxy or environment proxy. The result must be rooted before
/// anything can GC.
unsafe fn target_of(proxy: *mut JSObject) -> *mut JSObject {
    let mut target = UndefinedValue();
    unsafe { GetProxyPrivate(proxy, &mut target) };
    target.to_object()
}

/// The address of a proxy handler: of the sandbox proxies, a `js::Wrapper` whose `has`, `get`
/// and `set` traps are [`proxy_has`], [`proxy_get`] and [`proxy_set`], of the
/// [`sandbox_environment`] proxies, a `js::Wrapper` whose `get`, `set` and `defineProperty` traps
/// are [`environment_get`], [`proxy_set`] and [`environment_define`], or of the proxies
/// [`environment_object`] creates, a `js::Wrapper` that forwards every operation to its target.
struct HandlerAddress(*const c_void);

// SAFETY: the handler is immutable once created, and only used on the thread that runs JS.
unsafe impl Send for HandlerAddress {}
// SAFETY: as above.
unsafe impl Sync for HandlerAddress {}

static SANDBOX_HANDLER: OnceLock<HandlerAddress> = OnceLock::new();

static FORWARDING_HANDLER: OnceLock<HandlerAddress> = OnceLock::new();

static ENVIRONMENT_HANDLER: OnceLock<HandlerAddress> = OnceLock::new();

fn forwarding_handler() -> *const c_void {
    FORWARDING_HANDLER
        .get_or_init(|| {
            // SAFETY: `ProxyTraps` holds only optional function pointers, for which all-zero is
            // `None`, which leaves each trap to `js::Wrapper`.
            let traps: ProxyTraps = unsafe { std::mem::zeroed() };
            // SAFETY: the handler copies the traps.
            HandlerAddress(unsafe { CreateWrapperProxyHandler(&traps) })
        })
        .0
}

fn environment_handler() -> *const c_void {
    ENVIRONMENT_HANDLER
        .get_or_init(|| {
            // SAFETY: `ProxyTraps` holds only optional function pointers, for which all-zero is
            // `None`.
            let mut traps: ProxyTraps = unsafe { std::mem::zeroed() };
            traps.get = Some(environment_get);
            traps.set = Some(proxy_set);
            traps.defineProperty = Some(environment_define);
            // SAFETY: the traps are valid for the life of the process, which the handler lives
            // as long as.
            HandlerAddress(unsafe { CreateWrapperProxyHandler(&traps) })
        })
        .0
}

fn sandbox_handler() -> *const c_void {
    SANDBOX_HANDLER
        .get_or_init(|| {
            // SAFETY: `ProxyTraps` holds only optional function pointers, for which all-zero is
            // `None`.
            let mut traps: ProxyTraps = unsafe { std::mem::zeroed() };
            traps.has = Some(proxy_has);
            traps.get = Some(proxy_get);
            traps.set = Some(proxy_set);
            // SAFETY: the traps are valid for the life of the process, which the handler lives
            // as long as.
            HandlerAddress(unsafe { CreateWrapperProxyHandler(&traps) })
        })
        .0
}

/// The sandbox proxy `has` trap: whether the sandbox or the proxy's prototype has `id`.
unsafe extern "C" fn proxy_has(
    cx: *mut RawJSContext,
    proxy: mozjs::jsapi::HandleObject,
    id: mozjs::jsapi::HandleId,
    bp: *mut bool,
) -> bool {
    // SAFETY: SpiderMonkey calls traps with a realm entered and the handles rooted.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let (proxy, id) = unsafe { (Handle::from_raw(proxy), Handle::from_raw(id)) };
    let found = (|| {
        // SAFETY: `proxy` is a live sandbox proxy, and its target is rooted at once.
        let sandbox = scope.root_object(unsafe { NonNull::new_unchecked(target_of(proxy.get())) });
        let mut found = false;
        // SAFETY: the handles are rooted.
        let ok = unsafe { wrappers2::JS_HasPropertyById(scope.cx_mut(), sandbox, id, &mut found) };
        ExnThrown::check(ok)?;
        if found {
            return Ok(true);
        }
        // SAFETY: `proxy` is live, and the prototype is rooted at once.
        let Some(proto) =
            NonNull::new(unsafe { mozjs::jsapi::js::GetStaticPrototype(proxy.get()) })
        else {
            return Ok(false);
        };
        let proto = scope.root_object(proto);
        // SAFETY: the handles are rooted.
        let ok = unsafe { wrappers2::JS_HasPropertyById(scope.cx_mut(), proto, id, &mut found) };
        ExnThrown::check(ok).map(|()| found)
    })();
    match found {
        Ok(found) => {
            // SAFETY: SpiderMonkey passes a valid `bp`.
            unsafe { *bp = found };
            true
        }
        Err(ExnThrown) => false,
    }
}

/// The sandbox proxy `get` trap: the sandbox's property `id` if the sandbox has it, read with the
/// sandbox as the receiver, and the property of the proxy's prototype otherwise. A sandbox
/// property whose value is the sandbox reads as the proxy realm's global.
unsafe extern "C" fn proxy_get(
    cx: *mut RawJSContext,
    proxy: mozjs::jsapi::HandleObject,
    receiver: mozjs::jsapi::HandleValue,
    id: mozjs::jsapi::HandleId,
    vp: mozjs::jsapi::MutableHandleValue,
) -> bool {
    // SAFETY: SpiderMonkey calls traps with a realm entered and the handles rooted.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let (proxy, receiver, id) = unsafe {
        (
            Handle::from_raw(proxy),
            Handle::from_raw(receiver),
            Handle::from_raw(id),
        )
    };
    let mut vp = unsafe { mozjs::gc::MutableHandle::from_raw(vp) };
    // SAFETY: `proxy` is a live sandbox proxy, and its target is rooted at once.
    let sandbox = scope.root_object(unsafe { NonNull::new_unchecked(target_of(proxy.get())) });
    let mut found = false;
    // SAFETY: the handles are rooted.
    if !unsafe { wrappers2::JS_HasPropertyById(scope.cx_mut(), sandbox, id, &mut found) } {
        return false;
    }
    if found {
        let sandbox_value = scope.root_value(ObjectValue(sandbox.get()));
        // SAFETY: the handles are rooted.
        let ok = unsafe {
            wrappers2::JS_ForwardGetPropertyTo(
                scope.cx_mut(),
                sandbox,
                id,
                sandbox_value,
                vp.reborrow(),
            )
        };
        if ok && vp.get().is_object() && vp.get().to_object() == sandbox.get() {
            // SAFETY: `proxy` is a live object of a realm, which has a global.
            let global = unsafe { mozjs::jsapi::GetNonCCWObjectGlobal(proxy.get()) };
            vp.set(ObjectValue(global));
        }
        return ok;
    }
    // SAFETY: `proxy` is live, and the prototype is rooted at once.
    let Some(proto) = NonNull::new(unsafe { mozjs::jsapi::js::GetStaticPrototype(proxy.get()) })
    else {
        vp.set(UndefinedValue());
        return true;
    };
    let proto = scope.root_object(proto);
    // SAFETY: the handles are rooted.
    unsafe { wrappers2::JS_ForwardGetPropertyTo(scope.cx_mut(), proto, id, receiver, vp) }
}

/// The `set` trap of the sandbox proxies and environment proxies: assign `v` to the sandbox's
/// property `id`, with the sandbox as the receiver.
unsafe extern "C" fn proxy_set(
    cx: *mut RawJSContext,
    proxy: mozjs::jsapi::HandleObject,
    id: mozjs::jsapi::HandleId,
    v: mozjs::jsapi::HandleValue,
    _receiver: mozjs::jsapi::HandleValue,
    result: *mut ObjectOpResult,
) -> bool {
    // SAFETY: SpiderMonkey calls traps with a realm entered and the handles rooted.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let (proxy, id, v) = unsafe {
        (
            Handle::from_raw(proxy),
            Handle::from_raw(id),
            Handle::from_raw(v),
        )
    };
    // SAFETY: `proxy` is a live sandbox proxy, and its target is rooted at once.
    let sandbox = scope.root_object(unsafe { NonNull::new_unchecked(target_of(proxy.get())) });
    let sandbox_value = scope.root_value(ObjectValue(sandbox.get()));
    // SAFETY: the handles are rooted, and SpiderMonkey passes a valid `result`.
    unsafe {
        wrappers2::JS_ForwardSetPropertyTo(scope.cx_mut(), sandbox, id, v, sandbox_value, result)
    }
}

/// The environment proxy `get` trap: the sandbox's property `id`, read with the sandbox as the
/// receiver. A value that is the sandbox reads as the proxy.
unsafe extern "C" fn environment_get(
    cx: *mut RawJSContext,
    proxy: mozjs::jsapi::HandleObject,
    _receiver: mozjs::jsapi::HandleValue,
    id: mozjs::jsapi::HandleId,
    vp: mozjs::jsapi::MutableHandleValue,
) -> bool {
    // SAFETY: SpiderMonkey calls traps with a realm entered and the handles rooted.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let (proxy, id) = unsafe { (Handle::from_raw(proxy), Handle::from_raw(id)) };
    let mut vp = unsafe { mozjs::gc::MutableHandle::from_raw(vp) };
    // SAFETY: `proxy` is a live environment proxy, and its target is rooted at once.
    let sandbox = scope.root_object(unsafe { NonNull::new_unchecked(target_of(proxy.get())) });
    let sandbox_value = scope.root_value(ObjectValue(sandbox.get()));
    // SAFETY: the handles are rooted.
    let ok = unsafe {
        wrappers2::JS_ForwardGetPropertyTo(
            scope.cx_mut(),
            sandbox,
            id,
            sandbox_value,
            vp.reborrow(),
        )
    };
    if ok && vp.get().is_object() && vp.get().to_object() == sandbox.get() {
        vp.set(ObjectValue(proxy.get()));
    }
    ok
}

/// The environment proxy `defineProperty` trap: define the property `id` on the sandbox, and on
/// the global of the proxy's realm if the sandbox refuses it, as [`sandbox_environment`]
/// describes.
unsafe extern "C" fn environment_define(
    cx: *mut RawJSContext,
    proxy: mozjs::jsapi::HandleObject,
    id: mozjs::jsapi::HandleId,
    desc: mozjs::jsapi::Handle<PropertyDescriptor>,
    result: *mut ObjectOpResult,
) -> bool {
    // SAFETY: SpiderMonkey calls traps with a realm entered and the handles rooted, and passes a
    // valid `result`.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let (proxy, id, desc) = unsafe {
        (
            Handle::from_raw(proxy),
            Handle::from_raw(id),
            Handle::from_raw(desc),
        )
    };
    let result = unsafe { &mut *result };
    // SAFETY: `proxy` is a live environment proxy, and its target is rooted at once.
    let sandbox = scope.root_object(unsafe { NonNull::new_unchecked(target_of(proxy.get())) });
    // SAFETY: the handles are rooted.
    if !unsafe { wrappers2::JS_DefinePropertyById(scope.cx_mut(), sandbox, id, desc, result) } {
        return false;
    }
    if result.ok() {
        return true;
    }
    let mut found = false;
    // SAFETY: the handles are rooted.
    if !unsafe { wrappers2::JS_HasOwnPropertyById(scope.cx_mut(), sandbox, id, &mut found) } {
        return false;
    }
    if found {
        return true;
    }
    // SAFETY: `proxy` is a live object of a realm, which has a global, rooted at once.
    let global = scope.root_object(unsafe {
        NonNull::new_unchecked(mozjs::jsapi::GetNonCCWObjectGlobal(proxy.get()))
    });
    let declaration =
        desc.hasValue_() && desc.value_.is_undefined() && !desc.hasGetter_() && !desc.hasSetter_();
    if declaration {
        // SAFETY: the handles are rooted.
        if !unsafe { wrappers2::JS_HasOwnPropertyById(scope.cx_mut(), global, id, &mut found) } {
            return false;
        }
        if found {
            return result.succeed();
        }
    }
    // SAFETY: the handles are rooted.
    unsafe { wrappers2::JS_DefinePropertyById(scope.cx_mut(), global, id, desc, result) }
}

/// The `addProperty` hook of [`GLOBAL_CLASS`]: move the property `id`, just added to the
/// contextified global `obj`, to its sandbox, as the [module documentation](self) describes. Does
/// nothing for a global that isn't contextified.
unsafe extern "C" fn add_property(
    cx: *mut RawJSContext,
    obj: mozjs::jsapi::HandleObject,
    id: mozjs::jsapi::HandleId,
    _v: mozjs::jsapi::HandleValue,
) -> bool {
    // SAFETY: `obj` is the rooted global the property was added to.
    let Some(sandbox) = (unsafe { sandbox_raw(obj.get()) }) else {
        return true;
    };
    // SAFETY: SpiderMonkey calls the hook with a realm entered and the handles rooted.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let (global, id) = unsafe { (Handle::from_raw(obj), Handle::from_raw(id)) };
    // SAFETY: the sandbox is live, and rooted at once.
    let sandbox = scope.root_object(unsafe { NonNull::new_unchecked(sandbox) });
    move_to_sandbox(&scope, global, sandbox, id).is_ok()
}

fn move_to_sandbox(
    scope: &Scope<'_>,
    global: Handle<'_, *mut JSObject>,
    sandbox: Handle<'_, *mut JSObject>,
    id: Handle<'_, jsid>,
) -> Result<(), ExnThrown> {
    mozjs::rooted!(in(unsafe { scope.raw_cx_no_gc() }) let mut desc = PropertyDescriptor::default());
    let mut is_none = true;
    // SAFETY: the handles are rooted.
    let ok = unsafe {
        wrappers2::JS_GetOwnPropertyDescriptorById(
            scope.cx_mut(),
            global,
            id,
            desc.handle_mut(),
            &mut is_none,
        )
    };
    ExnThrown::check(ok)?;
    if is_none {
        return Ok(());
    }
    let plain = !desc.hasGetter_()
        && !desc.hasSetter_()
        && desc.writable_()
        && desc.enumerable_()
        && desc.configurable_();
    let configurable = desc.configurable_();
    // The `Uninitialized` code has all bits set.
    let mut result = ObjectOpResult { code_: usize::MAX };
    let ok = if plain {
        let value = scope.root_value(desc.value_);
        let receiver = scope.root_value(ObjectValue(sandbox.get()));
        // SAFETY: the handles are rooted, and `result` is a valid location.
        unsafe {
            wrappers2::JS_ForwardSetPropertyTo(
                scope.cx_mut(),
                sandbox,
                id,
                value,
                receiver,
                &mut result,
            )
        }
    } else {
        // SAFETY: the handles are rooted, and `result` is a valid location.
        unsafe {
            wrappers2::JS_DefinePropertyById(
                scope.cx_mut(),
                sandbox,
                id,
                desc.handle(),
                &mut result,
            )
        }
    };
    ExnThrown::check(ok)?;
    let moved = result.code_ == mozjs::jsapi::ObjectOpResult_SpecialCodes::OkCode as usize;
    if moved && configurable {
        let mut deleted = ObjectOpResult { code_: usize::MAX };
        // SAFETY: the handles are rooted, and `deleted` is a valid location.
        let ok =
            unsafe { wrappers2::JS_DeletePropertyById(scope.cx_mut(), global, id, &mut deleted) };
        ExnThrown::check(ok)?;
    }
    Ok(())
}
