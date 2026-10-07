// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:timers`, `node:timers/promises`, and Node's timer globals. See
//! <https://nodejs.org/api/timers.html>.
//!
//! With Node.js compatibility, the global `setTimeout` and `setInterval` return `Timeout` objects
//! instead of numbers, and `setImmediate` returns an `Immediate`. Both run on the event loop's
//! timers, and a `Timeout` converts to its timer's ID, so the global `clearTimeout` and
//! `clearInterval` accept the ID as well.
//!
//! A `Timeout` or `Immediate` keeps its state in Rust. Its own data properties are the ones Node's
//! objects have, such as `_idleTimeout` and `_onTimeout`, and are written when the state changes.
//! A timer reads only the callback and arguments from them when it fires, as Node's do.
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
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::{CallArgs, Value};
use js::object::{PropertyInit, PropertyTemplate, PropertyWriter, TemplateKey};
use js::prelude::HandleValue;
use js::symbol::CachedSymbol;
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

/// Node's `Symbol('refed')`, under which a timer holds whether it is referenced.
static REFED: CachedSymbol = CachedSymbol::new(c"refed");
/// Node's `Symbol('kHasPrimitive')`, under which a `Timeout` holds whether it was converted to its
/// ID.
static HAS_PRIMITIVE: CachedSymbol = CachedSymbol::new(c"kHasPrimitive");
/// Node's `Symbol('asyncId')`, under which a timer holds its ID.
static ASYNC_ID: CachedSymbol = CachedSymbol::new(c"asyncId");
/// Node's `Symbol('triggerId')`, under which a timer holds the ID of the resource that created it.
static TRIGGER_ID: CachedSymbol = CachedSymbol::new(c"triggerId");
/// Node's `Symbol('kAsyncContextFrame')`, under which a timer holds the async context current when
/// it was created.
static ASYNC_CONTEXT_FRAME: CachedSymbol = CachedSymbol::new(c"kAsyncContextFrame");

/// The longest timer delay, in milliseconds.
const TIMEOUT_MAX: f64 = 2147483647.0;

/// The duration `value`, named `name`, as Node.js's `getTimerDuration` checks it: a non-negative
/// finite number, or `ERR_INVALID_ARG_TYPE` or `ERR_OUT_OF_RANGE`. A duration above
/// [`TIMEOUT_MAX`] emits a `TimeoutOverflowWarning` and becomes [`TIMEOUT_MAX`].
pub(crate) fn get_timer_duration(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<f64, crate::errors::Thrown> {
    if !value.is_number() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type number", value).into());
    }
    let msecs = value.to_number();
    if !(msecs >= 0.0 && msecs.is_finite()) {
        let received = crate::errors::out_of_range_received(msecs);
        return Err(
            NodeError::out_of_range(name, "a non-negative finite number", &received).into(),
        );
    }
    if msecs > TIMEOUT_MAX {
        let shown = crate::errors::number_to_string(msecs);
        crate::process::emit_warning(
            scope,
            &format!(
                "{shown} does not fit into a 32-bit signed integer.\nTimer duration was truncated to {}.",
                crate::errors::number_to_string(TIMEOUT_MAX)
            ),
            "TimeoutOverflowWarning",
        )?;
        return Ok(TIMEOUT_MAX);
    }
    Ok(msecs)
}

/// The delay of a timer created with the delay argument `delay`, as Node's `Timeout` constructor
/// computes it: `delay` converted to a number, or 1 if it is `undefined`, below 1, above
/// [`TIMEOUT_MAX`] or `NaN`. A delay above [`TIMEOUT_MAX`] emits a `TimeoutOverflowWarning`, and
/// the first negative delay and the first `NaN` delay emit a `TimeoutNegativeWarning` and a
/// `TimeoutNaNWarning`.
pub(crate) fn timer_delay(scope: &Scope<'_>, delay: HandleValue<'_>) -> Result<f64, ExnThrown> {
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

/// The own properties the `Timeout` constructor creates before those of
/// [`ASYNC_RESOURCE_PROPERTIES`], in Node's order.
pub(crate) static TIMEOUT_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"_idleTimeout"),
    TemplateKey::Name(c"_idlePrev"),
    TemplateKey::Name(c"_idleNext"),
    TemplateKey::Name(c"_idleStart"),
    TemplateKey::Name(c"_onTimeout"),
    TemplateKey::Name(c"_timerArgs"),
    TemplateKey::Name(c"_repeat"),
    TemplateKey::Name(c"_destroyed"),
    TemplateKey::Symbol(&REFED),
    TemplateKey::Symbol(&HAS_PRIMITIVE),
]);

/// The own properties the `Immediate` constructor creates before those of
/// [`ASYNC_RESOURCE_PROPERTIES`], in Node's order.
pub(crate) static IMMEDIATE_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"_idleNext"),
    TemplateKey::Name(c"_idlePrev"),
    TemplateKey::Name(c"_onImmediate"),
    TemplateKey::Name(c"_argv"),
    TemplateKey::Name(c"_destroyed"),
    TemplateKey::Symbol(&REFED),
]);

/// The async resource properties of Node's `initAsyncResource`, in its order.
pub(crate) static ASYNC_RESOURCE_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Symbol(&ASYNC_ID),
    TemplateKey::Symbol(&TRIGGER_ID),
    TemplateKey::Symbol(&ASYNC_CONTEXT_FRAME),
]);

/// Write the async resource properties of Node's `initAsyncResource` with `properties`: the
/// timer's ID under `Symbol(asyncId)`, the ID of the resource that created it under
/// `Symbol(triggerId)`, which is always 1, and the current async context under
/// `Symbol(kAsyncContextFrame)`.
fn write_async_resource<'w>(
    scope: &'w Scope<'_>,
    properties: &mut PropertyWriter<'w>,
    id: u64,
) -> Result<(), ExnThrown> {
    properties.continue_with(scope, &ASYNC_RESOURCE_PROPERTIES)?;
    properties.write(scope, &ASYNC_ID, id as f64)?;
    properties.write(scope, &TRIGGER_ID, 1)?;
    let frame = match js::async_context::current(scope) {
        Some(frame) => frame.to_jsval_throwing(scope)?,
        None => HandleValue::undefined(),
    };
    properties.write(scope, &ASYNC_CONTEXT_FRAME, frame)
}

/// Set whether the event loop timer `id` keeps the event loop running.
fn set_timer_ref(id: u64, referenced: bool) {
    core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop.set_js_timer_ref(id, referenced)
    });
}

/// Make the queued event loop timer `id` due `delay` from now instead. Returns `false` if the
/// timer isn't queued.
fn restart_timer(id: u64, delay: Duration) -> bool {
    core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop.restart_js_timer(id, delay)
    })
    .unwrap_or(false)
}

/// Cancel the event loop timer `id`.
fn cancel_timer(id: u64) {
    core_runtime::event_loop::with_active_event_loop(|event_loop| event_loop.clear_js_timer(id));
}

/// Call `callback` with `this` and the elements of `args`, as `Reflect.apply` does, or with no
/// arguments if `args` is `undefined`.
fn apply(
    scope: &Scope<'_>,
    this: Object<'_>,
    callback: HandleValue<'_>,
    args: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if args.is_undefined() {
        Function::call(scope, this, callback, js::function::EmptyArgs)?;
        return Ok(());
    }
    let args = crate::support::list_from_array_like(scope, args)?;
    Function::call(scope, this, callback, &args)?;
    Ok(())
}

/// A `Timeout`, which `setTimeout` and `setInterval` return, as Node's `class Timeout`. See
/// <https://nodejs.org/api/timers.html#class-timeout>.
#[core_runtime::jsclass(
    name = "Timeout",
    hidden,
    templates = "TIMEOUT_PROPERTIES, ASYNC_RESOURCE_PROPERTIES"
)]
pub struct Timeout {
    /// The ID of the event loop timer the timeout runs on, and its `Symbol(asyncId)`.
    #[no_trace]
    id: u64,
    /// Whether the event loop timer is queued.
    #[no_trace]
    scheduled: bool,
    /// The delay in milliseconds, `_idleTimeout`, which is -1 once the timeout is cleared.
    #[no_trace]
    delay: f64,
    #[no_trace]
    repeat: bool,
    #[no_trace]
    refed: bool,
    /// Whether the timeout fired without repeating or was cleared, `_destroyed`.
    #[no_trace]
    destroyed: bool,
    /// Whether `clearTimeout` cleared the timeout.
    #[no_trace]
    cleared: bool,
}

#[core_runtime::jsmethods(node_args)]
impl Timeout {
    /// Node's `new Timeout(callback, after, args, isRepeat, isRefed)`: a timeout running
    /// `callback` with the array `args` after the delay `after`, again every `after` milliseconds
    /// if `isRepeat`, that keeps the event loop running if `isRefed`. The timeout doesn't start.
    #[constructor]
    pub fn new(
        &self,
        scope: &Scope<'_>,
        callback: HandleValue<'_>,
        after: HandleValue<'_>,
        timer_args: HandleValue<'_>,
        is_repeat: HandleValue<'_>,
        is_refed: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let delay = timer_delay(scope, after)?;
        let repeat = js::conversion::to_boolean(is_repeat);
        let id = core_runtime::event_loop::reserve_js_timer_id();
        {
            let mut data = self.data_mut();
            data.id = id;
            data.delay = delay;
            data.repeat = repeat;
            data.refed = js::conversion::to_boolean(is_refed);
        }
        let this = self.as_object();
        // SAFETY: the constructor's `this` is a new instance, and no JavaScript runs before the
        // writer finishes.
        let mut properties =
            unsafe { this.property_writer(scope, &TIMEOUT_PROPERTIES, PropertyInit::Define) }?;
        properties.write(scope, c"_idleTimeout", delay)?;
        properties.write(scope, c"_idlePrev", this)?;
        properties.write(scope, c"_idleNext", this)?;
        properties.write(scope, c"_idleStart", HandleValue::null())?;
        properties.write(scope, c"_onTimeout", callback)?;
        properties.write(scope, c"_timerArgs", timer_args)?;
        if repeat {
            properties.write(scope, c"_repeat", delay)?;
        } else {
            properties.write(scope, c"_repeat", HandleValue::null())?;
        }
        properties.write(scope, c"_destroyed", false)?;
        properties.write(scope, &REFED, is_refed)?;
        properties.write(scope, &HAS_PRIMITIVE, false)?;
        write_async_resource(scope, &mut properties, id)?;
        properties.finish(scope)?;
        crate::domain::init_resource(scope, this)
    }

    /// <https://nodejs.org/api/timers.html#timeoutrefresh>: restart the timeout with its delay,
    /// keeping its ID, including after it fired. Does nothing for a cleared timeout.
    #[method]
    pub fn refresh(&self, scope: &Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        let (id, delay, scheduled, destroyed) = {
            let data = self.data();
            (data.id, data.delay, data.scheduled, data.destroyed)
        };
        if delay < 0.0 {
            return Ok(self.as_object());
        }
        if scheduled {
            // Node.js's `insert` truncates the delay to whole milliseconds.
            let delay = Duration::from_millis(delay.trunc() as u64);
            if restart_timer(id, delay) {
                self.as_object()
                    .set_property(scope, c"_idleStart", crate::process::uptime_ms())?;
                return Ok(self.as_object());
            }
            cancel_timer(id);
        }
        // A timeout that fired or was cleared gets a new ID, its new `Symbol(asyncId)`, as Node's
        // `insert` initializes the async resource again.
        let id = if destroyed {
            let id = core_runtime::event_loop::reserve_js_timer_id();
            self.data_mut().id = id;
            id
        } else {
            id
        };
        self.insert(scope)?;
        if destroyed {
            self.data_mut().destroyed = false;
            let this = self.as_object();
            this.set_property(scope, c"_destroyed", false)?;
            // SAFETY: `Assign` has no requirements.
            let mut properties = unsafe {
                this.property_writer(scope, &ASYNC_RESOURCE_PROPERTIES, PropertyInit::Assign)
            }?;
            write_async_resource(scope, &mut properties, id)?;
            properties.finish(scope)?;
        }
        Ok(self.as_object())
    }

    /// <https://nodejs.org/api/timers.html#timeoutunref>
    #[method(name = "unref")]
    pub fn unref_method(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        set_refed_any(scope, args, false)
    }

    /// <https://nodejs.org/api/timers.html#timeoutref>
    #[method(name = "ref")]
    pub fn ref_method(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        set_refed_any(scope, args, true)
    }

    /// <https://nodejs.org/api/timers.html#timeouthasref>: whether the timeout keeps the event
    /// loop running, and `this[Symbol(refed)]` for a `this` that isn't a `Timeout`, as Node's
    /// method reads it.
    #[method(name = "hasRef")]
    pub fn has_ref_method(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        let this = scope.root_value(args.thisv().get());
        let result = match Object::from_value(scope, this) {
            Ok(object) => match object.cast::<Timeout>() {
                Ok(timer) => scope.root_value(js::value::from_bool(timer.has_ref())),
                Err(_) => object.get_property_by_id(scope, REFED.id(scope)?)?,
            },
            Err(_) => HandleValue::undefined(),
        };
        args.rval().set(result.get());
        Ok(())
    }

    /// <https://nodejs.org/api/timers.html#timeoutclose>
    #[method]
    pub fn close(&self, scope: &Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        self.clear(scope)?;
        Ok(self.as_object())
    }

    /// Queue the timeout's event loop timer with its delay, as Node's `insert` does, and record
    /// the start time in `_idleStart`.
    fn insert(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let (id, delay, repeat, refed) = {
            let data = self.data();
            (data.id, data.delay, data.repeat, data.refed)
        };
        let fire = js::class::get_or_init_shared_function(
            scope,
            fire_timeout as *const () as usize,
            |scope| Function::new_callback(scope, c"", 0, fire_timeout, ()),
        )?;
        // Node.js's `insert` truncates the delay to whole milliseconds.
        let delay = Duration::from_millis(delay.trunc() as u64);
        let what = if repeat { "setInterval" } else { "setTimeout" };
        let this = self.as_object().to_jsval_throwing(scope)?;
        core_runtime::event_loop::timer::start_timer(
            scope,
            fire.as_callable(),
            this,
            &[],
            delay,
            repeat,
            Some(id),
        )
        .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, what))?;
        self.data_mut().scheduled = true;
        if !refed {
            set_timer_ref(id, false);
        }
        self.as_object()
            .set_property(scope, c"_idleStart", crate::process::uptime_ms())
    }

    /// Whether the timeout keeps the event loop running.
    pub(crate) fn has_ref(&self) -> bool {
        self.data().refed
    }

    /// Set whether the timeout keeps the event loop running, as `ref()` and `unref()` do.
    fn set_refed(&self, scope: &Scope<'_>, referenced: bool) -> Result<Object<'s>, ExnThrown> {
        let (id, scheduled, changed) = {
            let mut data = self.data_mut();
            let changed = data.refed != referenced;
            data.refed = referenced;
            (data.id, data.scheduled, changed)
        };
        if changed {
            if scheduled {
                set_timer_ref(id, referenced);
            }
            self.as_object()
                .set_property_by_id(scope, REFED.id(scope)?, referenced)?;
        }
        Ok(self.as_object())
    }

    /// Node's `clearTimeout(timer)` for a `Timeout`: clear `_onTimeout`, and unless the timeout
    /// is destroyed, cancel its timer and set `_idleTimeout` to -1, so that `refresh()` doesn't
    /// start it again. Does nothing if the timeout was cleared already.
    fn clear(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let (id, destroyed) = {
            let mut data = self.data_mut();
            if data.cleared {
                return Ok(());
            }
            data.cleared = true;
            (data.id, data.destroyed)
        };
        let this = self.as_object();
        this.set_property(scope, c"_onTimeout", HandleValue::undefined())?;
        if destroyed {
            return Ok(());
        }
        {
            let mut data = self.data_mut();
            data.destroyed = true;
            data.scheduled = false;
            data.delay = -1.0;
        }
        // A timeout whose callback is running is cancelled too, so it no longer counts as active.
        cancel_timer(id);
        this.set_property(scope, c"_destroyed", true)?;
        this.set_property(scope, c"_idleTimeout", -1)?;
        this.set_property(scope, c"_idlePrev", HandleValue::null())?;
        this.set_property(scope, c"_idleNext", HandleValue::null())
    }
}

/// The `ref()` and `unref()` of timeouts and immediates: set whether `this`, a `Timeout` or an
/// `Immediate`, keeps the event loop running, and return `this`. Any other `this` is returned
/// unchanged.
fn set_refed_any(scope: &Scope<'_>, args: &CallArgs, referenced: bool) -> Result<(), ExnThrown> {
    let this = scope.root_value(args.thisv().get());
    if let Ok(object) = Object::from_value(scope, this) {
        if let Ok(timer) = object.cast::<Timeout>() {
            timer.set_refed(scope, referenced)?;
        } else if let Ok(immediate) = object.cast::<Immediate>() {
            immediate.set_refed(scope, referenced)?;
        }
    }
    args.rval().set(this.get());
    Ok(())
}

/// The callback of a `Timeout`'s event loop timer, whose `this` is the `Timeout`: call
/// `_onTimeout` with `this` and the elements of `_timerArgs`, as Node's `listOnTimeout` does. A
/// timeout that doesn't repeat is destroyed, whether or not the callback throws.
fn fire_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let timer = Object::from_value(scope, args.this())
        .ok()
        .and_then(|timer| timer.cast::<Timeout>().ok())
        .ok_or_else(|| js::error::TypeError("not a Timeout".into()).throw(scope))?;
    let repeat = timer.data().repeat;
    let this = timer.as_object();
    let start = crate::process::uptime_ms();
    if !repeat {
        timer.data_mut().scheduled = false;
        this.set_property(scope, c"_idlePrev", HandleValue::null())?;
        this.set_property(scope, c"_idleNext", HandleValue::null())?;
    }
    let callback = this.get_property(scope, c"_onTimeout")?;
    let timer_args = this.get_property(scope, c"_timerArgs")?;
    let result = apply(scope, this, callback, timer_args);
    // The exception the callback threw stays pending while the timeout's state is written.
    let thrown = result
        .is_err()
        .then(|| js::exception::take_pending(scope))
        .flatten();
    let (repeating, destroyed) = {
        let data = timer.data();
        (data.repeat && data.delay >= 0.0, data.destroyed)
    };
    if repeating {
        this.set_property(scope, c"_idleStart", start)?;
    } else if !destroyed {
        timer.data_mut().destroyed = true;
        this.set_property(scope, c"_destroyed", true)?;
    }
    if let Some(thrown) = thrown {
        js::exception::throw_value(scope, thrown);
    }
    result?;
    Ok(js::value::undefined())
}

/// A new `Timeout` running `callback` after `delay` milliseconds, every `delay` milliseconds if
/// `repeat`, with `args`, that keeps the event loop running if `refed`, as `setTimeout` and
/// `setInterval` create it.
pub(crate) fn create_timeout<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
    delay: HandleValue<'_>,
    args: &[HandleValue<'_>],
    repeat: bool,
    refed: bool,
) -> Result<Timeout<'s>, ExnThrown> {
    let args = if args.is_empty() {
        HandleValue::undefined()
    } else {
        js::Array::with_contents(scope, args)?.to_jsval_throwing(scope)?
    };
    let timer = Timeout::new(
        scope,
        callback,
        delay,
        args,
        scope.root_value(js::value::from_bool(repeat)),
        scope.root_value(js::value::from_bool(refed)),
    )?;
    timer.insert(scope)?;
    Ok(timer)
}

/// An `Immediate`, which `setImmediate` returns, as Node's `class Immediate`. See
/// <https://nodejs.org/api/timers.html#class-immediate>.
#[core_runtime::jsclass(
    name = "Immediate",
    hidden,
    templates = "IMMEDIATE_PROPERTIES, ASYNC_RESOURCE_PROPERTIES"
)]
pub struct Immediate {
    /// The ID of the event loop timer the immediate runs on, and its `Symbol(asyncId)`.
    #[no_trace]
    id: u64,
    #[no_trace]
    refed: bool,
    /// Whether the immediate's callback started or it was cleared, `_destroyed`.
    #[no_trace]
    destroyed: bool,
}

#[core_runtime::jsmethods(node_args)]
impl Immediate {
    /// Node's `new Immediate(callback, args)`: an immediate running `callback` with the array
    /// `args`, which starts right away and keeps the event loop running.
    #[constructor]
    pub fn new(
        &self,
        scope: &Scope<'_>,
        callback: HandleValue<'_>,
        argv: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let this = self.as_object();
        // SAFETY: the constructor's `this` is a new instance, and no JavaScript runs before the
        // writer finishes.
        let mut properties =
            unsafe { this.property_writer(scope, &IMMEDIATE_PROPERTIES, PropertyInit::Define) }?;
        properties.write(scope, c"_idleNext", HandleValue::null())?;
        properties.write(scope, c"_idlePrev", HandleValue::null())?;
        properties.write(scope, c"_onImmediate", callback)?;
        properties.write(scope, c"_argv", argv)?;
        properties.write(scope, c"_destroyed", false)?;
        properties.write(scope, &REFED, true)?;
        let fire = js::class::get_or_init_shared_function(
            scope,
            fire_immediate as *const () as usize,
            |scope| Function::new_callback(scope, c"", 0, fire_immediate, ()),
        )?;
        let id = core_runtime::event_loop::timer::start_timer(
            scope,
            fire.as_callable(),
            this.to_jsval_throwing(scope)?,
            &[],
            Duration::ZERO,
            false,
            None,
        )
        .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, "setImmediate"))?;
        {
            let mut data = self.data_mut();
            data.id = id;
            data.refed = true;
        }
        IMMEDIATE_IDS.with_borrow_mut(|ids| ids.insert(id));
        write_async_resource(scope, &mut properties, id)?;
        properties.finish(scope)?;
        crate::domain::init_resource(scope, this)
    }

    /// <https://nodejs.org/api/timers.html#immediateref>
    #[method(name = "ref")]
    pub fn ref_method(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        set_refed_any(scope, args, true)
    }

    /// <https://nodejs.org/api/timers.html#immediateunref>
    #[method(name = "unref")]
    pub fn unref_method(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        set_refed_any(scope, args, false)
    }

    /// <https://nodejs.org/api/timers.html#immediatehasref>: whether the immediate keeps the
    /// event loop running, and whether `this[Symbol(refed)]` is truthy for a `this` that isn't an
    /// `Immediate`, as Node's method reads it.
    #[method(name = "hasRef")]
    pub fn has_ref_method(scope: &Scope<'_>, args: &CallArgs) -> Result<(), ExnThrown> {
        let this = scope.root_value(args.thisv().get());
        let result = match Object::from_value(scope, this) {
            Ok(object) => match object.cast::<Immediate>() {
                Ok(immediate) => immediate.data().refed,
                Err(_) => {
                    js::conversion::to_boolean(object.get_property_by_id(scope, REFED.id(scope)?)?)
                }
            },
            Err(_) => false,
        };
        args.rval().set(js::value::from_bool(result));
        Ok(())
    }

    /// Set whether the immediate keeps the event loop running, as `ref()` and `unref()` do. A
    /// destroyed immediate keeps its setting.
    fn set_refed(&self, scope: &Scope<'_>, referenced: bool) -> Result<Object<'s>, ExnThrown> {
        let (id, changed) = {
            let mut data = self.data_mut();
            let changed = !data.destroyed && data.refed != referenced;
            if changed {
                data.refed = referenced;
            }
            (data.id, changed)
        };
        if changed {
            set_timer_ref(id, referenced);
            self.as_object()
                .set_property_by_id(scope, REFED.id(scope)?, referenced)?;
        }
        Ok(self.as_object())
    }

    /// Node's `clearImmediate(immediate)`: cancel its timer, and mark it destroyed. Does nothing
    /// if it is destroyed already.
    fn clear(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let id = {
            let mut data = self.data_mut();
            if data.destroyed {
                return Ok(());
            }
            data.destroyed = true;
            data.refed = false;
            data.id
        };
        cancel_timer(id);
        let this = self.as_object();
        this.set_property(scope, c"_destroyed", true)?;
        this.set_property_by_id(scope, REFED.id(scope)?, HandleValue::null())?;
        this.set_property(scope, c"_onImmediate", HandleValue::undefined())
    }
}

/// The callback of an `Immediate`'s event loop timer, whose `this` is the `Immediate`: mark it
/// destroyed and unreferenced, call `_onImmediate` with `this` and the elements of `_argv`, and
/// then clear `_onImmediate`, as Node's `processImmediate` does.
fn fire_immediate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let immediate = Object::from_value(scope, args.this())
        .ok()
        .and_then(|immediate| immediate.cast::<Immediate>().ok())
        .ok_or_else(|| js::error::TypeError("not an Immediate".into()).throw(scope))?;
    let id = {
        let mut data = immediate.data_mut();
        data.destroyed = true;
        data.refed = false;
        data.id
    };
    IMMEDIATE_IDS.with_borrow_mut(|ids| ids.remove(&id));
    RUN_IMMEDIATE_IDS.with_borrow_mut(|ids| ids.insert(id));
    let this = immediate.as_object();
    this.set_property(scope, c"_destroyed", true)?;
    this.set_property_by_id(scope, REFED.id(scope)?, HandleValue::null())?;
    let callback = this.get_property(scope, c"_onImmediate")?;
    let argv = this.get_property(scope, c"_argv")?;
    let argv = if js::conversion::to_boolean(argv) {
        argv
    } else {
        HandleValue::undefined()
    };
    let result = apply(scope, this, callback, argv);
    let pending = js::exception::take_pending(scope);
    this.set_property(scope, c"_onImmediate", HandleValue::undefined())?;
    if let Some(pending) = pending {
        js::exception::throw_value(scope, pending);
    }
    result?;
    Ok(js::value::undefined())
}

/// A new `Immediate` running `callback` with `args`, as `setImmediate` creates it.
pub(crate) fn create_immediate<'s>(
    scope: &'s Scope<'_>,
    callback: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Immediate<'s>, ExnThrown> {
    let args = if args.is_empty() {
        HandleValue::undefined()
    } else {
        js::Array::with_contents(scope, args)?.to_jsval_throwing(scope)?
    };
    Immediate::new(scope, callback, args)
}

/// The numbers of referenced `Timeout`s and `Immediate`s whose timers are queued or running, for
/// `process.getActiveResourcesInfo`. An `Immediate` stops counting when its callback starts.
/// Watchers' poll timers don't count.
pub(crate) fn active_timers() -> (usize, usize) {
    let (referenced, live) = core_runtime::event_loop::with_active_event_loop(|event_loop| {
        let referenced: BTreeSet<u64> = event_loop
            .referenced_js_timers()
            .into_iter()
            .filter(|id| !crate::fs::is_poll_timer(*id))
            .collect();
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

/// The arguments after the first `skip` of a native call.
fn rest_args<'a>(args: &'a CallbackArgs<'_>, skip: u32) -> Vec<HandleValue<'a>> {
    (skip..args.len()).map(|index| args.get(index)).collect()
}

/// The global `setTimeout(callback, delay, ...args)`.
fn set_timeout_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    validate_function(scope, args.get(0), "callback").map_err(|e| e.throw(scope))?;
    create_timeout(
        scope,
        args.get(0),
        args.get(1),
        &rest_args(&args, 2),
        false,
        true,
    )?
    .to_jsval_raw_throwing(scope)
}

/// The global `setInterval(callback, delay, ...args)`.
fn set_interval_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    validate_function(scope, args.get(0), "callback").map_err(|e| e.throw(scope))?;
    create_timeout(
        scope,
        args.get(0),
        args.get(1),
        &rest_args(&args, 2),
        true,
        true,
    )?
    .to_jsval_raw_throwing(scope)
}

/// The global `setImmediate(callback, ...args)`.
fn set_immediate_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    validate_function(scope, args.get(0), "callback").map_err(|e| e.throw(scope))?;
    create_immediate(scope, args.get(0), &rest_args(&args, 1))?.to_jsval_raw_throwing(scope)
}

/// The global `clearTimeout(timer)` and `clearInterval(timer)`: clear a `Timeout`, or cancel the
/// timer with the ID `timer`, a number or a string, as the web `clearTimeout` does.
fn clear_timeout_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let timer = args.get(0);
    if let Ok(timer) = Object::from_value(scope, timer) {
        if let Ok(timer) = timer.cast::<Timeout>() {
            timer.clear(scope)?;
        }
    } else if timer.is_number() || timer.is_string() {
        let id = f64::from_jsval(scope, timer, ()).map_err(|e| e.throw(scope))?;
        if id.is_finite() && id >= 1.0 && id.fract() == 0.0 {
            cancel_timer(id as u64);
        }
    }
    Ok(js::value::undefined())
}

/// The global `clearImmediate(immediate)`.
fn clear_immediate_global(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Ok(immediate) = Object::from_value(scope, args.get(0)) {
        if let Ok(immediate) = immediate.cast::<Immediate>() {
            immediate.clear(scope)?;
        }
    }
    Ok(js::value::undefined())
}

/// `timeout[Symbol.toPrimitive]()`: the timeout's ID, which marks it as converted under
/// `Symbol(kHasPrimitive)`.
fn to_primitive(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let timer = Object::from_value(scope, args.this())
        .ok()
        .and_then(|timer| timer.cast::<Timeout>().ok());
    let Some(timer) = timer else {
        return Ok(js::value::undefined());
    };
    timer
        .as_object()
        .set_property_by_id(scope, HAS_PRIMITIVE.id(scope)?, true)?;
    let id = timer.data().id;
    Ok(js::value::from_f64(id as f64))
}

/// `timeout[Symbol.dispose]()`: clear the timeout.
fn dispose_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(timer) = Object::from_value(scope, args.this())
        .ok()
        .and_then(|timer| timer.cast::<Timeout>().ok())
    {
        timer.clear(scope)?;
    }
    Ok(js::value::undefined())
}

/// `immediate[Symbol.dispose]()`: clear the immediate.
fn dispose_immediate(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(immediate) = Object::from_value(scope, args.this())
        .ok()
        .and_then(|immediate| immediate.cast::<Immediate>().ok())
    {
        immediate.clear(scope)?;
    }
    Ok(js::value::undefined())
}

/// `timeout[util.inspect.custom](depth, options)`: the timeout as `util.inspect` shows it with
/// `options`, a depth of 0 and without custom inspection, as Node's `Timeout` shows itself.
fn inspect_timeout(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let mut options = crate::inspect::InspectOptions::current(scope)?;
    if let Ok(given) = Object::from_value(scope, args.get(1)) {
        options = options.read(scope, given)?;
    }
    options.depth = Some(0.0);
    options.custom_inspect = false;
    let this = scope.root_value(args.this());
    let text = crate::inspect::inspect(scope, this, options)?;
    js::JSString::from_str(scope, &text)?.to_jsval_raw_throwing(scope)
}

/// Define a method named `name` that runs `body` on `proto`, under the well-known symbol `which`.
fn define_symbol_method(
    scope: &Scope<'_>,
    proto: Object<'_>,
    which: js::native::SymbolCode,
    name: &'static std::ffi::CStr,
    body: js::function::Callback,
) -> Result<(), ExnThrown> {
    let function = Function::new_callback(scope, name, 0, body, ())?;
    js::class::define_well_known_symbol_property(scope, proto, which, *function)
}

/// Register the timer classes on `global`, and define their symbol-keyed methods.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    Timeout::add_to_global(scope, global);
    Immediate::add_to_global(scope, global);
    Wait::add_to_global(scope, global);
    let proto = js::class::get_prototype_object_for::<TimeoutImpl>(scope)
        .expect("Timeout was registered above");
    use js::native::SymbolCode;
    let inspect = Function::new_callback(
        scope,
        c"[nodejs.util.inspect.custom]",
        2,
        inspect_timeout,
        (),
    )?;
    proto.define_value_by_id(
        scope,
        crate::inspect::INSPECT_CUSTOM.id(scope)?,
        *inspect,
        js::class_spec::JSPROP_READONLY as u32,
    )?;
    define_symbol_method(
        scope,
        proto,
        SymbolCode::dispose,
        c"[Symbol.dispose]",
        dispose_timeout,
    )?;
    define_symbol_method(
        scope,
        proto,
        SymbolCode::toPrimitive,
        c"[Symbol.toPrimitive]",
        to_primitive,
    )?;
    let proto = js::class::get_prototype_object_for::<ImmediateImpl>(scope)
        .expect("Immediate was registered above");
    define_symbol_method(
        scope,
        proto,
        SymbolCode::dispose,
        c"[Symbol.dispose]",
        dispose_immediate,
    )
}

/// Markers keying the realm's timer functions, which `node:timers` exports.
struct SetTimeout;
struct ClearTimeout;
struct SetInterval;
struct ClearInterval;
struct SetImmediate;
struct ClearImmediate;

/// The getter of `setTimeout[util.promisify.custom]` and `setImmediate[util.promisify.custom]`,
/// whose payload is `true` for `setTimeout`: `node:timers/promises`' `setTimeout` or
/// `setImmediate`.
fn promisified_timer(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let namespace = core_runtime::module::builtin_namespace(scope, "node:timers/promises")?
        .ok_or_else(|| {
            js::error::TypeError("node:timers/promises is missing".into()).throw(scope)
        })?;
    let name = if payload.to_boolean() {
        c"setTimeout"
    } else {
        c"setImmediate"
    };
    Ok(namespace.get_property(scope, name)?.get())
}

/// Replace the global `setTimeout`, `setInterval`, `clearTimeout` and `clearInterval` with the
/// ones for `Timeout`s, and add `setImmediate` and `clearImmediate`. `setTimeout` and
/// `setImmediate` have Node's enumerable, non-configurable `util.promisify.custom` getter.
pub fn install(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    fn define<'s, T: 'static>(
        scope: &'s Scope<'_>,
        global: Object<'_>,
        name: &'static std::ffi::CStr,
        nargs: u32,
        body: js::function::Callback,
    ) -> Result<Function<'s>, ExnThrown> {
        let function = Function::new_callback(scope, name, nargs, body, ())?;
        js::class::register_realm_object::<T>(scope, *function);
        global.set_property(scope, name, *function)?;
        Ok(function)
    }
    fn define_custom(
        scope: &Scope<'_>,
        function: Function<'_>,
        timeout: bool,
    ) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"get", 0, promisified_timer, timeout)?;
        let id = crate::util::PROMISIFY_CUSTOM.id(scope)?;
        let attrs = js::class_spec::JSPROP_ENUMERATE | js::class_spec::JSPROP_PERMANENT;
        function.define_accessor_by_id(scope, id, Some(*getter), None, attrs as u32)
    }
    let set_timeout = define::<SetTimeout>(scope, global, c"setTimeout", 2, set_timeout_global)?;
    define_custom(scope, set_timeout, true)?;
    define::<SetInterval>(scope, global, c"setInterval", 2, set_interval_global)?;
    define::<ClearTimeout>(scope, global, c"clearTimeout", 1, clear_timeout_global)?;
    define::<ClearInterval>(scope, global, c"clearInterval", 1, clear_timeout_global)?;
    let set_immediate =
        define::<SetImmediate>(scope, global, c"setImmediate", 1, set_immediate_global)?;
    define_custom(scope, set_immediate, false)?;
    define::<ClearImmediate>(scope, global, c"clearImmediate", 1, clear_immediate_global)?;
    Ok(())
}

/// The state of a `timers/promises` wait: the promise it settles, the value it resolves the
/// promise with, and the timer and abort listener once they exist.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Wait {
    promise: Heap<js::promise::Promise>,
    value: Heap<Value>,
    /// The `Timeout` or `Immediate` the wait runs on.
    timer: Option<Heap<js::object::Object>>,
    signal: Option<Heap<js::object::Object>>,
    listener: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl Wait {}

/// The callback of the timer a `timers/promises` promise waits on, whose payload is the [`Wait`]:
/// remove the abort listener, if there is one, and resolve the promise with the value.
fn resolve_later(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let wait = cast_payload::<Wait>(scope, payload);
    let (signal, listener) = {
        let data = wait.data();
        (
            data.signal.as_ref().map(|signal| signal.get(scope)),
            data.listener.as_ref().map(|listener| listener.get(scope)),
        )
    };
    if let (Some(signal), Some(listener)) = (signal, listener) {
        let abort = js::class::get_or_init_atom(scope, c"abort")?;
        Function::call_by_name(
            scope,
            signal.handle(),
            c"removeEventListener",
            &[abort, listener.to_jsval_throwing(scope)?],
        )?;
    }
    let (promise, value) = {
        let data = wait.data();
        (data.promise.get(scope), data.value.get(scope))
    };
    promise.resolve(scope, value)?;
    Ok(js::value::undefined())
}

/// The abort listener of a `timers/promises` wait, whose payload is the [`Wait`]: if the promise
/// is pending, clear the timer and reject the promise with an `AbortError` whose cause is the
/// signal's reason.
fn abort_later(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let wait = cast_payload::<Wait>(scope, payload);
    let (promise, timer, signal) = {
        let data = wait.data();
        (
            data.promise.get(scope),
            data.timer.as_ref().map(|timer| timer.get(scope)),
            data.signal.as_ref().map(|signal| signal.get(scope)),
        )
    };
    if !promise.is_pending() {
        return Ok(js::value::undefined());
    }
    if let Some(timer) = timer {
        if let Ok(timeout) = timer.cast::<Timeout>() {
            timeout.clear(scope)?;
        } else if let Ok(immediate) = timer.cast::<Immediate>() {
            immediate.clear(scope)?;
        }
    }
    let reason = match signal {
        Some(signal) => signal.get_property(scope, c"reason")?,
        None => HandleValue::undefined(),
    };
    let error = abort_error(scope, Some(reason))?;
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
            if js::conversion::to_boolean(signal.get_property(scope, c"aborted")?) {
                let error = abort_error(scope, Some(signal.get_property(scope, c"reason")?))?;
                promise.reject(scope, error)?;
                return Ok(());
            }
        }
        let value = value.unwrap_or(HandleValue::undefined());
        let state = js::class::create_instance_with::<WaitImpl>(scope, |_| WaitImpl {
            promise: Heap::from(promise),
            value: Heap::from(value.get()),
            timer: None,
            signal: None,
            listener: None,
        })?;
        let resolver = Function::new_callback(scope, c"", 0, resolve_later, state)?;
        let resolver = (*resolver).to_jsval_throwing(scope)?;
        let timer = match delay {
            Some(delay) => {
                create_timeout(scope, resolver, delay, &[], false, referenced)?.as_object()
            }
            None => {
                let immediate = create_immediate(scope, resolver, &[])?;
                if !referenced {
                    immediate.set_refed(scope, false)?;
                }
                immediate.as_object()
            }
        };
        if let Some(signal) = signal {
            let listener = Function::new_callback(scope, c"", 0, abort_later, state)?;
            {
                let mut data = state.data_mut();
                data.timer = Some(Heap::from(timer));
                data.signal = Some(Heap::from(signal));
                data.listener = Some(Heap::from(*listener));
            }
            let abort = js::class::get_or_init_atom(scope, c"abort")?;
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
    #[function(length = 2)]
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
    #[function(length = 1)]
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

/// The realm's timer function [`install`] registered for the marker `T`.
fn timer_function<'s, T: 'static>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    js::class::realm_object::<T>(scope)
        .ok_or_else(|| js::error::TypeError("the timers are not installed".into()).throw(scope))?
        .to_jsval_throwing(scope)
}

/// Register `node:timers`, whose functions are the ones [`install`] defined on the global and
/// whose `promises` is `node:timers/promises`' `default` export.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let mut values = vec![
            ("setTimeout", timer_function::<SetTimeout>(scope)?),
            ("clearTimeout", timer_function::<ClearTimeout>(scope)?),
            ("setInterval", timer_function::<SetInterval>(scope)?),
            ("clearInterval", timer_function::<ClearInterval>(scope)?),
            ("setImmediate", timer_function::<SetImmediate>(scope)?),
            ("clearImmediate", timer_function::<ClearImmediate>(scope)?),
        ];
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
