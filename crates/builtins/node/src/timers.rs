// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:timers`, `node:timers/promises`, and Node's timer globals. See
//! <https://nodejs.org/api/timers.html>.
//!
//! With Node.js compatibility, the global `setTimeout` and `setInterval` return `Timeout` objects
//! instead of numbers, and `setImmediate` returns an `Immediate`. They wrap the web timers, and
//! convert to the web timer's ID, so the global `clearTimeout` and `clearInterval` accept them
//! as well as IDs.
//!
//! The timers call their callbacks with the `Timeout` or `Immediate` as `this`, without the
//! nested-timer clamp, and `setImmediate` callbacks run as zero-delay timers. An unreferenced
//! timer runs while other work keeps the event loop running, and doesn't keep it running by
//! itself.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{
    abort_error, validate_abort_signal, validate_function, validate_plain_object, NodeError, Thrown,
};

js::instance_local! {
    /// Set once a negative timer delay emitted a warning.
    static WARNED_NEGATIVE: Cell<bool> = const { Cell::new(false) };
    /// Set once a timer delay that isn't a number emitted a warning.
    static WARNED_NAN: Cell<bool> = const { Cell::new(false) };
    /// The timer ids of the `Immediate`s whose callbacks haven't started.
    static IMMEDIATE_IDS: RefCell<BTreeSet<u64>> = const { RefCell::new(BTreeSet::new()) };
    /// The timer ids of the `Immediate`s whose callbacks started, until [`active_timers`] finds
    /// their timers gone.
    static RUN_IMMEDIATE_IDS: RefCell<BTreeSet<u64>> = const { RefCell::new(BTreeSet::new()) };
}

/// The longest timer delay, in milliseconds.
const TIMEOUT_MAX: f64 = 2147483647.0;

/// The delay of a timer created with the delay argument `delay`, as Node's `Timeout` constructor
/// computes it: `delay` converted to a number, or 1 if it is `undefined`, below 1, above
/// [`TIMEOUT_MAX`] or `NaN`. A delay above [`TIMEOUT_MAX`] emits a `TimeoutOverflowWarning`, and
/// the first negative delay and the first `NaN` delay emit a `TimeoutNegativeWarning` and a
/// `TimeoutNaNWarning`.
fn timer_delay(scope: &Scope<'_>, delay: HandleValue<'_>) -> Result<f64, ExnThrown> {
    if delay.is_undefined() {
        return Ok(1.0);
    }
    let after = f64::from_jsval(scope, delay, ()).map_err(|e| e.throw(scope))?;
    if (1.0..=TIMEOUT_MAX).contains(&after) {
        return Ok(after);
    }
    let shown = crate::errors::number_to_string(after);
    if after > TIMEOUT_MAX {
        crate::process::emit_warning(
            scope,
            &format!(
                "{shown} does not fit into a 32-bit signed integer.\nTimeout duration was set to 1."
            ),
            "TimeoutOverflowWarning",
        )?;
    } else if after < 0.0 && !WARNED_NEGATIVE.with(|warned| warned.replace(true)) {
        crate::process::emit_warning(
            scope,
            &format!("{shown} is a negative number.\nTimeout duration was set to 1."),
            "TimeoutNegativeWarning",
        )?;
    } else if after.is_nan() && !WARNED_NAN.with(|warned| warned.replace(true)) {
        crate::process::emit_warning(
            scope,
            &format!("{shown} is not a number.\nTimeout duration was set to 1."),
            "TimeoutNaNWarning",
        )?;
    }
    Ok(1.0)
}

/// Start the timer of `timer`, a `Timeout`, from its callback, delay and arguments, and store its
/// ID. A `Timeout` that had an ID keeps it.
fn schedule(scope: &Scope<'_>, timer: Object<'_>) -> Result<(), ExnThrown> {
    let repeat = timer.get_property(scope, c"_repeat")?;
    let repeat = !(repeat.is_null() || repeat.is_undefined());
    let callback = timer.get_property(scope, c"_onTimeout")?;
    let callback = js::Callable::from_jsval(scope, callback, ()).map_err(|e| e.throw(scope))?;
    let delay = number_or_nan(timer.get_property(scope, c"_idleTimeout")?);
    let extra = Object::from_value(scope, timer.get_property(scope, c"_timerArgs")?)
        .map_err(|e| e.throw(scope))?;
    let mut args = Vec::new();
    for index in 0..length_of(scope, extra)? {
        args.push(extra.get_element(scope, index)?);
    }
    let id = timer_id(scope, timer)?;
    let this = timer.to_jsval_throwing(scope)?;
    // Node.js's `insert` truncates the delay to whole milliseconds.
    let delay = Duration::from_millis(delay.trunc() as u64);
    let what = if repeat { "setInterval" } else { "setTimeout" };
    let id = core_runtime::event_loop::timer::start_timer(
        scope, callback, this, &args, delay, repeat, id,
    )
    .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, what))?;
    timer.set_property(scope, c"_id", id as f64)?;
    if !has_ref(scope, timer)? {
        set_ref(scope, timer, false)?;
    }
    Ok(())
}

/// The `length` of the array `array`, or 0 if it isn't a number.
fn length_of(scope: &Scope<'_>, array: Object<'_>) -> Result<u32, ExnThrown> {
    let length = array.get_property(scope, c"length")?;
    Ok(if length.is_number() {
        length.to_number() as u32
    } else {
        0
    })
}

/// `value` if it is a number, and `NaN` otherwise.
fn number_or_nan(value: HandleValue<'_>) -> f64 {
    if value.is_number() {
        value.to_number()
    } else {
        f64::NAN
    }
}

/// Whether `value` is truthy, as JS's `ToBoolean` converts it.
fn truthy(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    bool::from_jsval(scope, value, ()).unwrap_or(false)
}

/// The ID of the timer of `timer`, a `Timeout` or `Immediate`, if it has one.
fn timer_id(scope: &Scope<'_>, timer: Object<'_>) -> Result<Option<u64>, ExnThrown> {
    let id = timer.get_property(scope, c"_id")?;
    Ok(id.is_number().then(|| id.to_number() as u64))
}

/// Whether `timer`, a `Timeout` or `Immediate`, is referenced.
fn has_ref(scope: &Scope<'_>, timer: Object<'_>) -> Result<bool, ExnThrown> {
    let referenced = timer.get_property(scope, c"_ref")?;
    Ok(referenced.is_boolean() && referenced.to_boolean())
}

/// Set whether `timer`, a `Timeout` or `Immediate`, keeps the event loop running.
fn set_ref<'s>(
    scope: &Scope<'_>,
    timer: Object<'s>,
    referenced: bool,
) -> Result<Object<'s>, ExnThrown> {
    timer.set_property(scope, c"_ref", referenced)?;
    if let Some(id) = timer_id(scope, timer)? {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.set_js_timer_ref(id, referenced)
        });
    }
    Ok(timer)
}

/// Cancel the timer of `timer`, a `Timeout` or `Immediate`, if it has one.
fn cancel(scope: &Scope<'_>, timer: Object<'_>) -> Result<(), ExnThrown> {
    if let Some(id) = timer_id(scope, timer)? {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
    }
    Ok(())
}

/// Node's `clearTimeout(timer)` for a `Timeout`: cancel its timer, and clear `_onTimeout` and
/// `_idleTimeout` so that `refresh()` doesn't start it again. Does nothing if `_onTimeout` was
/// cleared already.
fn clear_timeout(scope: &Scope<'_>, timer: Object<'_>) -> Result<(), ExnThrown> {
    if !truthy(scope, timer.get_property(scope, c"_onTimeout")?) {
        return Ok(());
    }
    timer.set_property(scope, c"_onTimeout", js::value::null())?;
    timer.set_property(scope, c"_idleTimeout", -1)?;
    cancel(scope, timer)
}

/// Node's `clearImmediate(immediate)`: cancel its timer, and mark it destroyed. Does nothing if
/// it has no `_onImmediate` or is destroyed already.
fn clear_immediate(scope: &Scope<'_>, immediate: Object<'_>) -> Result<(), ExnThrown> {
    if !truthy(scope, immediate.get_property(scope, c"_onImmediate")?)
        || truthy(scope, immediate.get_property(scope, c"_destroyed")?)
    {
        return Ok(());
    }
    immediate.set_property(scope, c"_destroyed", true)?;
    cancel(scope, immediate)
}

/// A new `Timeout` running `callback` after `delay` milliseconds, every `delay` milliseconds if
/// `repeat`, with `args`.
fn create_timeout<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
    delay: HandleValue<'_>,
    args: &[HandleValue<'_>],
    repeat: bool,
) -> Result<Object<'s>, Thrown> {
    let ctor = timeout::class_constructor(scope)?;
    let proto = Object::from_value(scope, ctor.get_property(scope, c"prototype")?)?;
    let timer = Object::new_plain(scope)?;
    timer.set_prototype(scope, proto.handle())?;
    let delay = timer_delay(scope, delay)?;
    timer.set_property(scope, c"_idleTimeout", delay)?;
    timer.set_property(scope, c"_onTimeout", callback)?;
    timer.set_property(scope, c"_timerArgs", js::Array::with_contents(scope, args)?)?;
    if repeat {
        timer.set_property(scope, c"_repeat", delay)?;
    } else {
        timer.set_property(scope, c"_repeat", js::value::null())?;
    }
    timer.set_property(scope, c"_ref", true)?;
    schedule(scope, timer)?;
    Ok(timer)
}

/// A new `Immediate` running `callback` with `args` as a zero-delay timer.
fn create_immediate<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Object<'s>, ExnThrown> {
    let ctor = immediate::class_constructor(scope)?;
    let proto = Object::from_value(scope, ctor.get_property(scope, c"prototype")?)
        .map_err(|e| e.throw(scope))?;
    let timer = Object::new_plain(scope)?;
    timer.set_prototype(scope, proto.handle())?;
    timer.set_property(scope, c"_onImmediate", callback)?;
    timer.set_property(scope, c"_destroyed", false)?;
    timer.set_property(scope, c"_ref", true)?;
    js::Callable::from_jsval(scope, callback, ()).map_err(|e| e.throw(scope))?;
    let run = crate::stream::closure(scope, 0, run_immediate, &[callback])?;
    let callable = js::Callable::from_jsval(scope, run.to_jsval_throwing(scope)?, ())
        .map_err(|e| e.throw(scope))?;
    let this = timer.to_jsval_throwing(scope)?;
    let id = core_runtime::event_loop::timer::start_timer(
        scope,
        callable,
        this,
        args,
        Duration::ZERO,
        false,
        None,
    )
    .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, "setImmediate"))?;
    timer.set_property(scope, c"_id", id as f64)?;
    IMMEDIATE_IDS.with_borrow_mut(|ids| ids.insert(id));
    Ok(timer)
}

/// The body of an `Immediate`'s timer, whose captures are `[callback]` and whose `this` is the
/// `Immediate`: record that its callback started, then call it with the timer's arguments.
fn run_immediate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    if let Some(id) = Object::from_value(scope, *this)
        .ok()
        .and_then(|immediate| timer_id(scope, immediate).ok().flatten())
    {
        IMMEDIATE_IDS.with_borrow_mut(|ids| ids.remove(&id));
        RUN_IMMEDIATE_IDS.with_borrow_mut(|ids| ids.insert(id));
    }
    let callback = crate::stream::Captures::of(scope, payload)?.get(0)?;
    let arguments = crate::callback_args(&args);
    Function::call(scope, this, callback, &arguments)?;
    Ok(js::value::undefined())
}

/// The numbers of referenced `Timeout`s and `Immediate`s whose timers are queued or running, for
/// `process.getActiveResourcesInfo`. An `Immediate` stops counting when its callback starts.
pub(crate) fn active_timers() -> (usize, usize) {
    let (referenced, live) = core_runtime::event_loop::with_active_event_loop(|event_loop| {
        let referenced: BTreeSet<u64> = event_loop.referenced_js_timers().into_iter().collect();
        let live = |ids: &BTreeSet<u64>| -> BTreeSet<u64> {
            ids.iter()
                .copied()
                .filter(|id| event_loop.has_js_timer(*id))
                .collect()
        };
        let pending = IMMEDIATE_IDS.with_borrow(live);
        let running = RUN_IMMEDIATE_IDS.with_borrow(live);
        (referenced, (pending, running))
    })
    .unwrap_or_default();
    let (pending, running) = live;
    let immediates = pending.intersection(&referenced).count();
    let started = running.intersection(&referenced).count();
    IMMEDIATE_IDS.with_borrow_mut(|ids| *ids = pending);
    RUN_IMMEDIATE_IDS.with_borrow_mut(|ids| *ids = running);
    (referenced.len() - immediates - started, immediates)
}

/// The global `setTimeout(callback, delay, ...args)`.
fn set_timeout_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let all = crate::callback_args(&args);
    let run = || -> Result<Value, Thrown> {
        validate_function(scope, args.get(0), "callback")?;
        let rest = all.get(2..).unwrap_or_default();
        Ok(
            create_timeout(scope, args.get(0), args.get(1), rest, false)?
                .to_jsval_raw_throwing(scope)?,
        )
    };
    run().map_err(|error| error.throw(scope))
}

/// The global `setInterval(callback, delay, ...args)`.
fn set_interval_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let all = crate::callback_args(&args);
    let run = || -> Result<Value, Thrown> {
        validate_function(scope, args.get(0), "callback")?;
        let rest = all.get(2..).unwrap_or_default();
        Ok(create_timeout(scope, args.get(0), args.get(1), rest, true)?
            .to_jsval_raw_throwing(scope)?)
    };
    run().map_err(|error| error.throw(scope))
}

/// The global `setImmediate(callback, ...args)`.
fn set_immediate_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let all = crate::callback_args(&args);
    let run = || -> Result<Value, Thrown> {
        validate_function(scope, args.get(0), "callback")?;
        let rest = all.get(1..).unwrap_or_default();
        Ok(create_immediate(scope, args.get(0), rest)?.to_jsval_raw_throwing(scope)?)
    };
    run().map_err(|error| error.throw(scope))
}

/// The global `clearTimeout(timer)` and `clearInterval(timer)`: [`clear_timeout`] for a
/// `Timeout`, and the web `clearTimeout` for an ID.
fn clear_timeout_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let timer = args.get(0);
    if let Ok(timer) = Object::from_value(scope, timer) {
        clear_timeout(scope, timer)?;
    } else {
        Function::call(
            scope,
            js::value::undefined(),
            web_timer(scope, c"kClearTimeout")?,
            &[timer],
        )?;
    }
    Ok(js::value::undefined())
}

/// The global `clearImmediate(immediate)`.
fn clear_immediate_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Ok(timer) = Object::from_value(scope, args.get(0)) {
        clear_immediate(scope, timer)?;
    }
    Ok(js::value::undefined())
}

/// The web timer function the `Timeout` constructor holds under `name`.
fn web_timer<'s>(
    scope: &'s Scope<'_>,
    name: &'static std::ffi::CStr,
) -> Result<HandleValue<'s>, ExnThrown> {
    timeout::class_constructor(scope)?.get_property(scope, name)
}

/// `timeout[Symbol.toPrimitive]()` and `immediate[Symbol.toPrimitive]()`: the timer's ID.
fn to_primitive(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let timer = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    Ok(timer.get_property(scope, c"_id")?.get())
}

/// `timeout[Symbol.dispose]()`: [`clear_timeout`].
fn dispose_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let timer = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    clear_timeout(scope, timer)?;
    Ok(js::value::undefined())
}

/// `immediate[Symbol.dispose]()`: [`clear_immediate`].
fn dispose_immediate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let timer = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    clear_immediate(scope, timer)?;
    Ok(js::value::undefined())
}

/// Define `Symbol.toPrimitive`, and `Symbol.dispose` as `dispose`, on `proto`.
fn define_symbol_methods(
    scope: &Scope<'_>,
    proto: Object<'_>,
    dispose: js::function::Callback,
) -> Result<(), ExnThrown> {
    let key = js::symbol::get_well_known_key(scope, js::native::SymbolCode::toPrimitive);
    let function = Function::new_callback(scope, c"[Symbol.toPrimitive]", 0, to_primitive, ())?;
    proto.set_property_by_id(scope, scope.root_id(key), *function)?;
    let key = js::symbol::get_well_known_key(scope, js::native::SymbolCode::dispose);
    let function = Function::new_callback(scope, c"[Symbol.dispose]", 0, dispose, ())?;
    proto.set_property_by_id(scope, scope.root_id(key), *function)
}

/// The error constructing a `Timeout` or `Immediate` directly throws.
fn illegal_constructor() -> crate::errors::NodeError {
    crate::errors::NodeError::new(
        crate::errors::ErrorKind::TypeError,
        "ERR_ILLEGAL_CONSTRUCTOR",
        "Illegal constructor",
    )
}

#[core_runtime::jsfunction_class(name = "Timeout", class_syntax)]
pub mod timeout {
    use super::*;

    /// `Timeout`s are created by `setTimeout` and `setInterval`, not by the constructor.
    #[constructor]
    pub fn create() -> Result<(), crate::errors::NodeError> {
        Err(illegal_constructor())
    }

    /// <https://nodejs.org/api/timers.html#timeoutref>
    pub fn r#ref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        set_ref(scope, this, true)
    }

    /// <https://nodejs.org/api/timers.html#timeoutunref>
    pub fn unref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        set_ref(scope, this, false)
    }

    /// <https://nodejs.org/api/timers.html#timeouthasref>
    pub fn has_ref(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        super::has_ref(scope, this)
    }

    /// <https://nodejs.org/api/timers.html#timeoutrefresh>: restart the timer with its delay,
    /// keeping its ID. Does nothing for a cleared timer.
    pub fn refresh<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        if number_or_nan(this.get_property(scope, c"_idleTimeout")?) < 0.0 {
            return Ok(this);
        }
        cancel(scope, this)?;
        schedule(scope, this)?;
        Ok(this)
    }

    /// <https://nodejs.org/api/timers.html#timeoutclose>
    pub fn close<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        clear_timeout(scope, this)?;
        Ok(this)
    }

    /// Define `Symbol.toPrimitive` and `Symbol.dispose`, and keep the web `clearTimeout`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        define_symbol_methods(scope, proto, dispose_timeout)?;
        ctor.define_property(
            scope,
            c"kClearTimeout",
            scope.global().get_property(scope, c"clearTimeout")?,
            0,
        )
    }
}

#[core_runtime::jsfunction_class(name = "Immediate", class_syntax)]
pub mod immediate {
    use super::*;

    /// `Immediate`s are created by `setImmediate`, not by the constructor.
    #[constructor]
    pub fn create() -> Result<(), crate::errors::NodeError> {
        Err(illegal_constructor())
    }

    /// <https://nodejs.org/api/timers.html#immediateref>
    pub fn r#ref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        set_ref(scope, this, true)
    }

    /// <https://nodejs.org/api/timers.html#immediateunref>
    pub fn unref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        set_ref(scope, this, false)
    }

    /// <https://nodejs.org/api/timers.html#immediatehasref>
    pub fn has_ref(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        super::has_ref(scope, this)
    }

    /// Define `Symbol.toPrimitive` and `Symbol.dispose`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        define_symbol_methods(scope, proto, dispose_immediate)
    }
}

/// Replace the global `setTimeout`, `setInterval`, `clearTimeout` and `clearInterval` with the
/// ones for `Timeout`s, and add `setImmediate` and `clearImmediate`.
pub fn install(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    // Creating the class keeps the web `clearTimeout` before it is replaced.
    timeout::class_constructor(scope)?;
    let set_timeout = Function::new_callback(scope, c"setTimeout", 2, set_timeout_global, ())?;
    let set_interval = Function::new_callback(scope, c"setInterval", 2, set_interval_global, ())?;
    let clear_timeout =
        Function::new_callback(scope, c"clearTimeout", 1, clear_timeout_global, ())?;
    let clear_interval =
        Function::new_callback(scope, c"clearInterval", 1, clear_timeout_global, ())?;
    let set_immediate =
        Function::new_callback(scope, c"setImmediate", 1, set_immediate_global, ())?;
    let clear_immediate =
        Function::new_callback(scope, c"clearImmediate", 1, clear_immediate_global, ())?;
    global.set_property(scope, c"setTimeout", *set_timeout)?;
    global.set_property(scope, c"setInterval", *set_interval)?;
    global.set_property(scope, c"clearTimeout", *clear_timeout)?;
    global.set_property(scope, c"clearInterval", *clear_interval)?;
    global.set_property(scope, c"setImmediate", *set_immediate)?;
    global.set_property(scope, c"clearImmediate", *clear_immediate)
}

/// The promise in `state`, the state of a `timers/promises` wait.
fn wait_promise_of<'s>(
    scope: &'s Scope<'_>,
    state: Object<'_>,
) -> Result<js::Promise<'s>, ExnThrown> {
    Object::from_value(scope, state.get_property(scope, c"promise")?)
        .map_err(|e| e.throw(scope))?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))
}

/// The callback of the timer a `timers/promises` promise waits on, whose payload is the wait's
/// state: remove the abort listener, if there is one, and resolve the promise with the value.
fn resolve_later(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    if let Ok(signal) = Object::from_value(scope, state.get_property(scope, c"signal")?) {
        let abort = js::JSString::from_str(scope, "abort")?.to_jsval_throwing(scope)?;
        let listener = state.get_property(scope, c"listener")?;
        Function::call_by_name(
            scope,
            signal.handle(),
            c"removeEventListener",
            &[abort, listener],
        )?;
    }
    wait_promise_of(scope, state)?.resolve(scope, state.get_property(scope, c"value")?)?;
    Ok(js::value::undefined())
}

/// The abort listener of a `timers/promises` wait, whose payload is the wait's state: if the
/// promise is pending, clear the timer and reject the promise with an `AbortError` whose cause is
/// the signal's reason.
fn abort_later(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let promise = wait_promise_of(scope, state)?;
    if !promise.is_pending() {
        return Ok(js::value::undefined());
    }
    let timer = Object::from_value(scope, state.get_property(scope, c"timer")?)
        .map_err(|e| e.throw(scope))?;
    if truthy(scope, state.get_property(scope, c"immediate")?) {
        clear_immediate(scope, timer)?;
    } else {
        clear_timeout(scope, timer)?;
    }
    let signal = Object::from_value(scope, state.get_property(scope, c"signal")?)
        .map_err(|e| e.throw(scope))?;
    let error = abort_error(scope, Some(signal.get_property(scope, c"reason")?))?;
    promise.reject(scope, error)?;
    Ok(js::value::undefined())
}

/// The `options` of a `timers/promises` wait, validated: its `signal`, and its `ref`, which
/// defaults to `true`.
fn wait_options<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(Option<Object<'s>>, bool), Thrown> {
    let Some(options) = options.filter(|options| !options.is_undefined()) else {
        return Ok((None, true));
    };
    let options = validate_plain_object(scope, options, "options", false)?;
    let signal = options.get_property(scope, c"signal")?;
    validate_abort_signal(scope, signal, "options.signal")?;
    let referenced = options.get_property(scope, c"ref")?;
    if !referenced.is_undefined() && !referenced.is_boolean() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "options.ref",
            "of type boolean",
            referenced,
        )
        .into());
    }
    let signal = Object::from_value(scope, signal).ok();
    Ok((signal, referenced.is_undefined() || referenced.to_boolean()))
}

/// A promise resolved with `value` by a new `Timeout` with the delay argument `delay`, or by a new
/// `Immediate` if `delay` is `None`, as `timers/promises`' `setTimeout` and `setImmediate` return
/// it. Invalid arguments and an abort of `options.signal` reject the promise.
fn wait<'s>(
    scope: &'s Scope<'_>,
    delay: Option<HandleValue<'_>>,
    value: Option<HandleValue<'_>>,
    options: Option<HandleValue<'_>>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let started = (|| -> Result<(), Thrown> {
        if let Some(delay) = delay.filter(|delay| !delay.is_undefined() && !delay.is_number()) {
            return Err(
                NodeError::invalid_arg_type(scope, "delay", "of type number", delay).into(),
            );
        }
        let (signal, referenced) = wait_options(scope, options)?;
        if let Some(signal) = signal {
            if truthy(scope, signal.get_property(scope, c"aborted")?) {
                let error = abort_error(scope, Some(signal.get_property(scope, c"reason")?))?;
                promise.reject(scope, error)?;
                return Ok(());
            }
        }
        let state = Object::new_plain(scope)?;
        state.set_property(scope, c"promise", *promise)?;
        let value = value.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        state.set_property(scope, c"value", value)?;
        let resolver = Function::new_callback(scope, c"", 0, resolve_later, state)?;
        let resolver = (*resolver).to_jsval_throwing(scope)?;
        let timer = match delay {
            Some(delay) => create_timeout(scope, resolver, delay, &[], false)?,
            None => create_immediate(scope, resolver, &[])?,
        };
        if !referenced {
            set_ref(scope, timer, false)?;
        }
        if let Some(signal) = signal {
            state.set_property(scope, c"timer", timer)?;
            state.set_property(scope, c"immediate", delay.is_none())?;
            state.set_property(scope, c"signal", signal)?;
            let listener = Function::new_callback(scope, c"", 0, abort_later, state)?;
            state.set_property(scope, c"listener", *listener)?;
            let abort = js::JSString::from_str(scope, "abort")?.to_jsval_throwing(scope)?;
            let listener = (*listener).to_jsval_throwing(scope)?;
            Function::call_by_name(
                scope,
                signal.handle(),
                c"addEventListener",
                &[abort, listener],
            )?;
        }
        Ok(())
    })();
    if let Err(error) = started {
        error.throw(scope);
        promise.reject_with_pending(scope)?;
    }
    Ok(promise)
}

#[core_runtime::jsmodule(node_args, name = "node:timers/promises", default_namespace)]
pub mod promises {
    use super::*;

    /// <https://nodejs.org/api/timers.html#timerspromisessettimeoutdelay-value-options>
    pub fn set_timeout<'s>(
        scope: &'s Scope<'_>,
        delay: Option<HandleValue<'_>>,
        value: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        let delay = delay.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        wait(scope, Some(delay), value, options)
    }

    /// <https://nodejs.org/api/timers.html#timerspromisessetimmediatevalue-options>
    pub fn set_immediate<'s>(
        scope: &'s Scope<'_>,
        value: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        wait(scope, None, value, options)
    }
}

/// Register `node:timers/promises`, whose `scheduler` has `wait(delay)` and `yield()`.
pub(crate) fn register_promises(scope: &Scope<'_>, name: &'static str) -> bool {
    let scheduler = || -> Result<HandleValue<'_>, ExnThrown> {
        let scheduler = Object::new_plain(scope)?;
        let wait = Function::new_callback(scope, c"wait", 1, scheduler_wait, ())?;
        let yield_ = Function::new_callback(scope, c"yield", 0, scheduler_yield, ())?;
        scheduler.set_property(scope, c"wait", *wait)?;
        scheduler.set_property(scope, c"yield", *yield_)?;
        scheduler.to_jsval_throwing(scope)
    };
    let Ok(scheduler) = scheduler() else {
        return false;
    };
    crate::stub::register_implemented_with::<promises::js_module>(
        scope,
        name,
        &[("scheduler", scheduler)],
    )
}

/// `scheduler.wait(delay, options)`: `setTimeout(delay, undefined, options)`.
fn scheduler_wait(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    wait(scope, Some(args.get(0)), None, Some(args.get(1)))?.to_jsval_raw_throwing(scope)
}

/// `scheduler.yield()`: `setImmediate()`.
fn scheduler_yield(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    wait(scope, None, None, None)?.to_jsval_raw_throwing(scope)
}

/// Register `node:timers`, whose functions are the global's and whose `promises` is
/// `node:timers/promises`' `default` export.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let global = scope.global();
        let mut values = Vec::new();
        for (export, key) in [
            ("setTimeout", c"setTimeout"),
            ("clearTimeout", c"clearTimeout"),
            ("setInterval", c"setInterval"),
            ("clearInterval", c"clearInterval"),
            ("setImmediate", c"setImmediate"),
            ("clearImmediate", c"clearImmediate"),
        ] {
            values.push((export, global.get_property(scope, key)?));
        }
        let promises = core_runtime::module::builtin_namespace(scope, "node:timers/promises")?
            .ok_or(ExnThrown)?
            .get_property(scope, c"default")?;
        values.push(("promises", promises));
        let namespace = Object::new_plain(scope)?;
        for (export, value) in &values {
            let key = std::ffi::CString::new(*export).expect("no NUL");
            namespace.set_property_by_name(scope, &key, *value)?;
        }
        values.push(("default", namespace.to_jsval_throwing(scope)?));
        Ok(values)
    };
    let Ok(values) = exports() else {
        return false;
    };
    core_runtime::module::register_synthetic_module(scope, name, &values).is_ok()
}
