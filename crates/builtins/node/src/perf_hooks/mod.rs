// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:perf_hooks`. See <https://nodejs.org/api/perf_hooks.html>.
//!
//! The performance timeline is the web `performance` object's, in `web_globals::performance`:
//! `performance` and the `Performance*` classes are the globals. The module registers a
//! [`TimelineExtension`] for Node.js's entry types and `nodeTiming` milestones, and adds Node.js's
//! members to `Performance.prototype`: `eventLoopUtilization`, `nodeTiming` and `timerify`, and
//! Node.js's `toJSON`. The histograms are in [`histogram`], and the `gc` entries in [`gc`].

mod gc;
mod hdr;
pub(crate) mod histogram;

use std::cell::Cell;
use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};
use web_globals::performance::entry::{PerformanceEntry, PerformanceEntryImpl};
use web_globals::performance::{PerformanceImpl, TimelineExtension};

use crate::errors::{validate_function, validate_plain_object, NodeError, Thrown};

/// `NODE_PERFORMANCE_GC_MINOR`, a nursery collection.
pub(crate) const GC_MINOR: u32 = 1;
/// `NODE_PERFORMANCE_GC_MAJOR`, a major collection or its last slice.
pub(crate) const GC_MAJOR: u32 = 4;
/// `NODE_PERFORMANCE_GC_INCREMENTAL`, a slice of an incremental major collection.
pub(crate) const GC_INCREMENTAL: u32 = 8;
/// `NODE_PERFORMANCE_GC_FLAGS_NO`.
pub(crate) const GC_FLAGS_NO: u32 = 0;
/// `NODE_PERFORMANCE_GC_FLAGS_FORCED`, a collection the embedding or `gc()` requested.
pub(crate) const GC_FLAGS_FORCED: u32 = 4;

/// The `perf_hooks.constants`, in Node.js's order.
const CONSTANTS: &[(&CStr, u32)] = &[
    (c"NODE_PERFORMANCE_GC_MAJOR", GC_MAJOR),
    (c"NODE_PERFORMANCE_GC_MINOR", GC_MINOR),
    (c"NODE_PERFORMANCE_GC_MINOR_MARK_SWEEP", 2),
    (c"NODE_PERFORMANCE_GC_INCREMENTAL", GC_INCREMENTAL),
    (c"NODE_PERFORMANCE_GC_WEAKCB", 16),
    (c"NODE_PERFORMANCE_GC_FLAGS_NO", GC_FLAGS_NO),
    (c"NODE_PERFORMANCE_GC_FLAGS_CONSTRUCT_RETAINED", 2),
    (c"NODE_PERFORMANCE_GC_FLAGS_FORCED", GC_FLAGS_FORCED),
    (
        c"NODE_PERFORMANCE_GC_FLAGS_SYNCHRONOUS_PHANTOM_PROCESSING",
        8,
    ),
    (c"NODE_PERFORMANCE_GC_FLAGS_ALL_AVAILABLE_GARBAGE", 16),
    (c"NODE_PERFORMANCE_GC_FLAGS_ALL_EXTERNAL_MEMORY", 32),
    (c"NODE_PERFORMANCE_GC_FLAGS_SCHEDULE_IDLE", 64),
    (c"NODE_PERFORMANCE_ENTRY_TYPE_GC", 0),
    (c"NODE_PERFORMANCE_ENTRY_TYPE_HTTP", 1),
    (c"NODE_PERFORMANCE_ENTRY_TYPE_HTTP2", 2),
    (c"NODE_PERFORMANCE_ENTRY_TYPE_NET", 3),
    (c"NODE_PERFORMANCE_ENTRY_TYPE_DNS", 4),
    (c"NODE_PERFORMANCE_ENTRY_TYPE_QUIC", 5),
    (c"NODE_PERFORMANCE_MILESTONE_TIME_ORIGIN_TIMESTAMP", 0),
    (c"NODE_PERFORMANCE_MILESTONE_TIME_ORIGIN", 1),
    (c"NODE_PERFORMANCE_MILESTONE_ENVIRONMENT", 2),
    (c"NODE_PERFORMANCE_MILESTONE_NODE_START", 3),
    (c"NODE_PERFORMANCE_MILESTONE_V8_START", 4),
    (c"NODE_PERFORMANCE_MILESTONE_LOOP_START", 5),
    (c"NODE_PERFORMANCE_MILESTONE_LOOP_EXIT", 6),
    (c"NODE_PERFORMANCE_MILESTONE_BOOTSTRAP_COMPLETE", 7),
];

/// The entry types `PerformanceObserver.supportedEntryTypes` lists, as in Node.js. Of the types
/// Node.js produces itself, only `function` and `gc` entries are produced.
const SUPPORTED_ENTRY_TYPES: &[&str] = &[
    "dns", "function", "gc", "http", "http2", "mark", "measure", "net", "quic", "resource",
];

/// The entry types `observerCounts` counts the observers of, in the order of its elements and
/// of the `NODE_PERFORMANCE_ENTRY_TYPE_*` constants.
const COUNTED_ENTRY_TYPES: [&str; 6] = ["gc", "http", "http2", "net", "dns", "quic"];

/// The timeline extension `register` registers.
static NODE_TIMELINE: TimelineExtension = TimelineExtension {
    supported_entry_types: SUPPORTED_ENTRY_TYPES,
    reserved_timestamp,
    observed,
    measure_options_without_start_or_end: true,
};

/// The `platform::clock::monotonic_ns` readings of the `nodeTiming` milestones the runtime
/// records itself.
#[derive(Clone, Copy, Default)]
struct Milestones {
    /// When the Node.js setup of the global started, `nodeStart` and `v8Start`.
    start: Option<u64>,
    /// When the classes of the Node.js builtins were registered, `environment`.
    environment: Option<u64>,
    /// When the Node.js globals were installed, `bootstrapComplete`.
    bootstrap_complete: Option<u64>,
}

js::instance_local! {
    static MILESTONES: Cell<Milestones> = const {
        Cell::new(Milestones { start: None, environment: None, bootstrap_complete: None })
    };
    /// The number of observers of each of [`COUNTED_ENTRY_TYPES`].
    static OBSERVER_COUNTS: Cell<[u32; 6]> = const { Cell::new([0; 6]) };
}

/// Register the timeline extension.
pub(crate) fn register() {
    web_globals::performance::register_timeline_extension(&NODE_TIMELINE);
}

/// Record the `nodeStart` and `v8Start` milestone, as the Node.js setup of a global starts.
pub(crate) fn mark_start() {
    MILESTONES.with(|milestones| {
        milestones.set(Milestones {
            start: Some(platform::clock::monotonic_ns()),
            ..milestones.get()
        })
    });
}

/// Record the `environment` milestone, once the classes of the Node.js builtins are registered.
pub(crate) fn mark_environment() {
    MILESTONES.with(|milestones| {
        milestones.set(Milestones {
            environment: Some(platform::clock::monotonic_ns()),
            ..milestones.get()
        })
    });
}

/// Record the `bootstrapComplete` milestone, as the setup of a global finishes.
pub(crate) fn mark_bootstrap_complete() {
    MILESTONES.with(|milestones| {
        milestones.set(Milestones {
            bootstrap_complete: Some(platform::clock::monotonic_ns()),
            ..milestones.get()
        })
    });
}

/// The milliseconds from the time origin to the reading `ns`, or -1 without one, as Node.js's
/// `getMilestoneTimestamp` reports a milestone.
fn milestone_ms(ns: Option<u64>) -> f64 {
    ns.map_or(-1.0, web_globals::performance::ms_since_origin)
}

/// The `platform::clock::monotonic_ns` reading at the first step of the active event loop, and
/// the nanoseconds it spent waiting, or `None` before its first step.
fn loop_timing() -> Option<(u64, u64)> {
    core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop
            .started_ns()
            .map(|started| (started, event_loop.idle_ns()))
    })
    .flatten()
}

/// The `nodeTiming` milestone `name` in milliseconds from the time origin, or `None` for a name
/// that isn't one.
fn milestone(name: &str) -> Option<f64> {
    let milestones = MILESTONES.with(Cell::get);
    Some(match name {
        "nodeStart" | "v8Start" => milestone_ms(milestones.start),
        "environment" => milestone_ms(milestones.environment),
        "loopStart" => milestone_ms(loop_timing().map(|(started, _)| started)),
        "loopExit" => -1.0,
        "bootstrapComplete" => milestone_ms(milestones.bootstrap_complete),
        _ => return None,
    })
}

/// The [`TimelineExtension::reserved_timestamp`] of Node.js: the `nodeTiming` milestones.
fn reserved_timestamp(_scope: &Scope<'_>, name: &str) -> Option<f64> {
    milestone(name)
}

/// The [`TimelineExtension::observed`] of Node.js: count the observers of the types in
/// `observerCounts`, and install the collection callbacks while `gc` entries are observed.
fn observed(scope: &Scope<'_>, entry_type: &'static str, observing: bool) {
    let Some(index) = COUNTED_ENTRY_TYPES.iter().position(|t| *t == entry_type) else {
        return;
    };
    let mut counts = OBSERVER_COUNTS.with(Cell::get);
    let before = counts[index];
    counts[index] = if observing {
        before.saturating_add(1)
    } else {
        before.saturating_sub(1)
    };
    OBSERVER_COUNTS.with(|cell| cell.set(counts));
    if entry_type == "gc" {
        match (before, counts[index]) {
            (0, 1..) => gc::install(scope),
            (1.., 0) => gc::uninstall(scope),
            _ => {}
        }
    }
    let key = observer_counts_array as *const () as usize;
    if let Some(array) = js::class::get_shared_object(scope, key) {
        let _ = array.set_element(scope, index as u32, counts[index]);
    }
}

/// The `observerCounts` of `internalBinding('performance')`: a `Uint32Array` of the number of
/// observers of each of [`COUNTED_ENTRY_TYPES`], which the counts update.
pub(crate) fn observer_counts_array<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let key = observer_counts_array as *const () as usize;
    js::class::get_or_init_shared_object(scope, key, |scope| {
        let counts = OBSERVER_COUNTS.with(Cell::get);
        let array = js::Uint32Array::with_data(scope, &counts)?;
        let array = array.to_jsval_throwing(scope)?;
        Object::from_value(scope, *array).map_err(|e| e.throw(scope))
    })
}

/// A new object with `perf_hooks.constants`.
pub(crate) fn constants<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let constants = Object::new_plain(scope)?;
    for &(name, value) in CONSTANTS {
        constants.set_property(scope, name, value)?;
    }
    Ok(constants)
}

/// The keys of `performance.eventLoopUtilization()` results.
static ELU_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"idle"),
    TemplateKey::Name(c"active"),
    TemplateKey::Name(c"utilization"),
]);

/// The keys of the objects Node.js's `performance.toJSON()` returns.
static PERFORMANCE_JSON_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"nodeTiming"),
    TemplateKey::Name(c"timeOrigin"),
    TemplateKey::Name(c"eventLoopUtilization"),
]);

/// The keys of the object a `Performance`'s custom inspection shows.
static PERFORMANCE_INSPECT_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"nodeTiming"),
    TemplateKey::Name(c"timeOrigin"),
]);

/// The keys of the objects `performance.nodeTiming.toJSON()` returns, in Node.js's order.
static NODE_TIMING_JSON_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"name"),
    TemplateKey::Name(c"entryType"),
    TemplateKey::Name(c"startTime"),
    TemplateKey::Name(c"duration"),
    TemplateKey::Name(c"nodeStart"),
    TemplateKey::Name(c"v8Start"),
    TemplateKey::Name(c"bootstrapComplete"),
    TemplateKey::Name(c"environment"),
    TemplateKey::Name(c"loopStart"),
    TemplateKey::Name(c"loopExit"),
    TemplateKey::Name(c"idleTime"),
]);

/// The keys of `performance.nodeTiming.uvMetricsInfo`.
static UV_METRICS_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"loopCount"),
    TemplateKey::Name(c"events"),
    TemplateKey::Name(c"eventsWaiting"),
]);

/// The keys of the `detail` of `gc` entries.
static GC_DETAIL_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"kind"), TemplateKey::Name(c"flags")]);

/// A performance entry of a type Node.js produces, with a `detail`, as Node.js's
/// `PerformanceNodeEntry`. See <https://nodejs.org/api/perf_hooks.html#class-performanceentry>.
#[core_runtime::jsclass(
    name = "PerformanceNodeEntry",
    hidden,
    extends = PerformanceEntry,
    invalid_this = "crate::errors::invalid_this"
)]
pub struct PerformanceNodeEntry {
    parent: PerformanceEntryImpl,
    detail: Heap<Value>,
    /// The values of the entry's deprecated accessors, by name.
    deprecated: Vec<(String, Heap<Value>)>,
}

#[core_runtime::jsmethods(node_args)]
impl PerformanceNodeEntry {
    /// <https://nodejs.org/api/perf_hooks.html#performanceentrydetail>
    #[getter]
    pub fn detail<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        self.data().detail.get(scope)
    }

    /// The entry's `name`, `entryType`, `startTime`, `duration` and `detail`.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    pub fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        let detail = self.data().detail.get(scope);
        self.json(scope, Some(detail))
    }
}

/// A new entry named `name` of `entry_type` with `detail`, as Node.js's
/// `createPerformanceNodeEntry` creates it.
fn new_node_entry<'s>(
    scope: &'s Scope<'_>,
    name: String,
    entry_type: &'static str,
    start_time: f64,
    duration: f64,
    detail: HandleValue<'_>,
) -> Result<PerformanceNodeEntry<'s>, ExnThrown> {
    js::class::create_instance_with::<PerformanceNodeEntryImpl>(scope, |_| {
        PerformanceNodeEntryImpl {
            parent: PerformanceEntryImpl::with_timing(name, entry_type, start_time, duration),
            detail: Heap::from(detail.get()),
            deprecated: Vec::new(),
        }
    })
}

/// Queue a `gc` entry of a collection of `kind` with `flags`, with the deprecated `kind` and
/// `flags` accessors Node.js gives it (DEP0152).
fn queue_gc_entry(
    scope: &Scope<'_>,
    start_time: f64,
    duration: f64,
    kind: u32,
    flags: u32,
) -> Result<(), ExnThrown> {
    let values = [
        scope.root_value(js::value::from_f64(f64::from(kind))),
        scope.root_value(js::value::from_f64(f64::from(flags))),
    ];
    let detail = Object::new_plain_with_template(scope, &GC_DETAIL_PROPERTIES, &values)?;
    let detail = detail.to_jsval_throwing(scope)?;
    let entry = new_node_entry(scope, "gc".into(), "gc", start_time, duration, detail)?;
    entry.data_mut().deprecated = vec![
        ("kind".into(), Heap::from(values[0].get())),
        ("flags".into(), Heap::from(values[1].get())),
    ];
    let object = entry.as_object();
    for name in [c"kind", c"flags"] {
        let key = name.to_str().map_err(|_| ExnThrown)?;
        let getter = Function::new_callback(scope, c"deprecated", 0, get_deprecated, key)?;
        let setter = Function::new_callback(scope, c"deprecated", 1, set_deprecated, key)?;
        let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
        object.define_accessor(scope, name, Some(*getter), Some(*setter), enumerable)?;
    }
    web_globals::performance::queue_entry(scope, *entry)
}

/// The message of the deprecation warning `DEP0152`.
const DEPRECATED_ACCESSOR: &str =
    "Custom PerformanceEntry accessors are deprecated. Please use the detail property.";

/// The deprecated accessor of `this` named by `args`' payload.
fn deprecated_entry<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<PerformanceNodeEntry<'s>, ExnThrown> {
    Object::from_value(scope, args.this())
        .ok()
        .and_then(|object| object.cast::<PerformanceNodeEntry>().ok())
        .ok_or_else(|| crate::errors::invalid_this(scope, "PerformanceNodeEntry"))
}

/// A deprecated accessor's getter, whose payload is its name: emit `DEP0152` and return the
/// value.
fn get_deprecated(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    crate::process::deprecate_once(scope, DEPRECATED_ACCESSOR, "DEP0152")?;
    let name = String::from_jsval_throwing(scope, payload, ())?;
    let entry = deprecated_entry(scope, &args)?;
    let data = entry.data();
    Ok(data
        .deprecated
        .iter()
        .find(|(key, _)| *key == name)
        .map_or(js::value::undefined(), |(_, value)| value.get(scope).get()))
}

/// A deprecated accessor's setter, whose payload is its name: emit `DEP0152` and replace the
/// value.
fn set_deprecated(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    crate::process::deprecate_once(scope, DEPRECATED_ACCESSOR, "DEP0152")?;
    let name = String::from_jsval_throwing(scope, payload, ())?;
    let value = args.get(0);
    let entry = deprecated_entry(scope, &args)?;
    if let Some((_, slot)) = entry
        .data_mut()
        .deprecated
        .iter_mut()
        .find(|(key, _)| *key == name)
    {
        slot.set(value.get());
    }
    Ok(js::value::undefined())
}

/// The object `performance.nodeTiming` is, as Node.js's `PerformanceNodeTiming`. See
/// <https://nodejs.org/api/perf_hooks.html#class-performancenodetiming>.
#[core_runtime::jsclass(
    name = "PerformanceNodeTiming",
    hidden,
    extends = PerformanceEntry,
    invalid_this = "crate::errors::invalid_this"
)]
pub struct PerformanceNodeTiming {
    parent: PerformanceEntryImpl,
}

#[core_runtime::jsmethods(node_args)]
impl PerformanceNodeTiming {
    /// The timing as a plain object, read from the timing's properties.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    pub fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        let this = self.as_object();
        let mut values = vec![
            "node".to_jsval_throwing(scope)?,
            "node".to_jsval_throwing(scope)?,
        ];
        for name in [
            c"startTime",
            c"duration",
            c"nodeStart",
            c"v8Start",
            c"bootstrapComplete",
            c"environment",
            c"loopStart",
            c"loopExit",
            c"idleTime",
        ] {
            values.push(this.get_property(scope, name)?);
        }
        Object::new_plain_with_template(scope, &NODE_TIMING_JSON_PROPERTIES, &values)
    }
}

/// The getters of `performance.nodeTiming`'s accessors, in Node.js's order.
const NODE_TIMING_ACCESSORS: &[(&CStr, js::function::Callback)] = &[
    (c"duration", timing_duration),
    (c"nodeStart", timing_milestone),
    (c"v8Start", timing_milestone),
    (c"environment", timing_milestone),
    (c"loopStart", timing_milestone),
    (c"loopExit", timing_milestone),
    (c"bootstrapComplete", timing_milestone),
    (c"idleTime", timing_idle_time),
    (c"uvMetricsInfo", timing_uv_metrics),
];

/// The `duration` getter of `performance.nodeTiming`: `performance.now()`.
fn timing_duration(_: &Scope<'_>, _: CallbackArgs<'_>, _: HandleValue) -> Result<Value, ExnThrown> {
    Ok(js::value::from_f64(web_globals::performance::now()))
}

/// A milestone getter of `performance.nodeTiming`, whose payload is the milestone's name.
fn timing_milestone(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let name = String::from_jsval_throwing(scope, payload, ())?;
    Ok(js::value::from_f64(milestone(&name).unwrap_or(-1.0)))
}

/// The `idleTime` getter of `performance.nodeTiming`: the milliseconds the event loop spent
/// waiting.
fn timing_idle_time(
    _: &Scope<'_>,
    _: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let idle_ns = loop_timing().map_or(0, |(_, idle)| idle);
    Ok(js::value::from_f64(idle_ns as f64 / 1e6))
}

/// The `uvMetricsInfo` getter of `performance.nodeTiming`. The event loop doesn't count its
/// iterations or events, so each count is 0.
fn timing_uv_metrics(
    scope: &Scope<'_>,
    _: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let zero = scope.root_value(js::value::from_f64(0.0));
    let metrics =
        Object::new_plain_with_template(scope, &UV_METRICS_PROPERTIES, &[zero, zero, zero])?;
    metrics.to_jsval_raw_throwing(scope)
}

/// A new `PerformanceNodeTiming`, with Node.js's own properties.
fn new_node_timing<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let timing = js::class::create_instance_with::<PerformanceNodeTimingImpl>(scope, |_| {
        PerformanceNodeTimingImpl {
            parent: PerformanceEntryImpl::with_timing("node".into(), "node", 0.0, 0.0),
        }
    })?;
    let object = timing.as_object();
    let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
    let readonly = (js::class_spec::JSPROP_ENUMERATE | js::class_spec::JSPROP_READONLY) as u32;
    object.define_property(scope, c"name", "node", readonly)?;
    object.define_property(scope, c"entryType", "node", readonly)?;
    object.define_property(scope, c"startTime", 0, readonly)?;
    for &(name, getter) in NODE_TIMING_ACCESSORS {
        let key = name.to_str().map_err(|_| ExnThrown)?;
        let getter = Function::new_callback(scope, name, 0, getter, key)?;
        object.define_accessor(scope, name, Some(*getter), None, attrs)?;
    }
    Ok(object)
}

/// <https://nodejs.org/api/perf_hooks.html#performanceeventlooputilizationutilization1-utilization2>:
/// the event loop's idle and active time, and their ratio, since its first step, or between the
/// results `util1` and `util2` of earlier calls. Before the event loop's first step, all three are
/// 0.
fn event_loop_utilization(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let util1 = args.get(0);
    let util2 = args.get(1);
    let result = |idle: f64, active: f64, utilization: f64| -> Result<Value, ExnThrown> {
        let values =
            [idle, active, utilization].map(|value| scope.root_value(js::value::from_f64(value)));
        Object::new_plain_with_template(scope, &ELU_PROPERTIES, &values)?
            .to_jsval_raw_throwing(scope)
    };
    let Some((started, idle_ns)) = loop_timing() else {
        return result(0.0, 0.0, 0.0);
    };
    let field = |value: HandleValue<'_>, name: &'static CStr| -> Result<f64, ExnThrown> {
        if value.is_null_or_undefined() {
            return Err(js::error::throw_type_error(
                scope,
                c"Cannot read properties of an undefined utilization",
            ));
        }
        match Object::from_value(scope, *value) {
            Ok(object) => f64::from_jsval_throwing(scope, object.get_property(scope, name)?, ()),
            Err(_) => Ok(f64::NAN),
        }
    };
    if js::conversion::to_boolean(util2) {
        let idle = field(util1, c"idle")? - field(util2, c"idle")?;
        let active = field(util1, c"active")? - field(util2, c"active")?;
        return result(idle, active, active / (idle + active));
    }
    let now = platform::clock::monotonic_ns();
    let idle = idle_ns as f64 / 1e6;
    let active = (now as f64 - started as f64) / 1e6 - idle;
    if !js::conversion::to_boolean(util1) {
        return result(idle, active, active / (idle + active));
    }
    let idle_delta = idle - field(util1, c"idle")?;
    let active_delta = active - field(util1, c"active")?;
    result(
        idle_delta,
        active_delta,
        active_delta / (idle_delta + active_delta),
    )
}

/// The state of a function `timerify` returns.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Timerified {
    function: Heap<js::object::Object>,
    histogram: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl Timerified {}

/// The state of the `finally` callback of a timerified function's promise.
#[core_runtime::jsclass(hidden)]
pub(crate) struct TimerifyCompletion {
    function: Heap<js::object::Object>,
    histogram: Option<Heap<js::object::Object>>,
    args: Heap<js::object::Object>,
    #[no_trace]
    start: f64,
}

#[core_runtime::jsmethods]
impl TimerifyCompletion {}

/// <https://nodejs.org/api/perf_hooks.html#performancetimerifyfn-options>: a function that
/// calls or constructs `fn`, and queues a `function` entry when the call returns, or when the
/// promise a call returns settles. With `options.histogram`, it records the call's duration in
/// nanoseconds there.
fn timerify(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let result = (|| -> Result<Function<'_>, Thrown> {
        let function = validate_function(scope, args.get(0), "fn")?;
        let options = args.get(1);
        let histogram = if options.is_undefined() {
            HandleValue::undefined()
        } else {
            validate_plain_object(scope, options, "options", false)?
                .get_property(scope, c"histogram")?
        };
        let histogram = if histogram.is_undefined() {
            None
        } else {
            let record = match Object::from_value(scope, *histogram) {
                Ok(object) if histogram::is_histogram(scope, histogram) => {
                    Some(object.get_property(scope, c"record")?)
                }
                _ => None,
            };
            if !record.is_some_and(|record| {
                Object::from_value(scope, *record).is_ok_and(|record| record.is_callable())
            }) {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "options.histogram",
                    "an instance of RecordableHistogram",
                    histogram,
                )
                .into());
            }
            Some(Object::from_value(scope, *histogram).map_err(|e| e.throw(scope))?)
        };
        let state = js::class::create_instance_with::<TimerifiedImpl>(scope, |_| TimerifiedImpl {
            function: Heap::from(function),
            histogram: histogram.map(Heap::from),
        })?;
        let length = function.get_property(scope, c"length")?;
        let name = function.get_property(scope, c"name")?;
        let name = format!("timerified {}", crate::errors::js_string(scope, name)?);
        let timerified = Function::new_constructor_callback(scope, c"", 0, call_timerified, state)?;
        let attrs = (js::class_spec::JSPROP_ENUMERATE
            | js::class_spec::JSPROP_PERMANENT
            | js::class_spec::JSPROP_READONLY) as u32;
        timerified.define_property(scope, c"length", length, attrs)?;
        timerified.define_property(scope, c"name", name.as_str(), attrs)?;
        Ok(timerified)
    })();
    match result {
        Ok(timerified) => (*timerified).to_jsval_raw_throwing(scope),
        Err(error) => Err(error.throw(scope)),
    }
}

/// The body of a timerified function, whose payload is its [`Timerified`] state.
fn call_timerified(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<Timerified>(scope, payload);
    let (function, histogram) = {
        let data = state.data();
        (
            data.function.get(scope),
            data.histogram
                .as_ref()
                .map(|histogram| histogram.get(scope)),
        )
    };
    let call_args = args.rest(0);
    let constructing = args.is_constructing();
    let start = web_globals::performance::now();
    let result = if constructing {
        let constructed =
            Function::construct_with_new_target(scope, function, function.handle(), &call_args)?;
        constructed.to_jsval_throwing(scope)?
    } else {
        let this = scope.root_value(args.this());
        Function::call(scope, this, function, &call_args)?
    };
    let args_array = js::Array::with_contents(scope, &call_args)?;
    if !constructing {
        if let Ok(promise) = Object::from_value(scope, *result) {
            let finally = promise.get_property(scope, c"finally")?;
            if Object::from_value(scope, *finally).is_ok_and(|finally| finally.is_callable()) {
                let completion =
                    js::class::create_instance_with::<TimerifyCompletionImpl>(scope, |_| {
                        TimerifyCompletionImpl {
                            function: Heap::from(function),
                            histogram: histogram.map(Heap::from),
                            args: Heap::from(*args_array),
                            start,
                        }
                    })?;
                let callback = Function::new_callback(
                    scope,
                    c"bound processComplete",
                    0,
                    complete_timerified,
                    completion,
                )?;
                let callback = (*callback).to_jsval_throwing(scope)?;
                return Ok(Function::call(scope, promise, finally, &[callback])?.get());
            }
        }
    }
    process_complete(scope, function, start, *args_array, histogram)?;
    Ok(result.get())
}

/// The `finally` callback of a timerified function's promise, whose payload is its
/// [`TimerifyCompletion`] state.
fn complete_timerified(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TimerifyCompletion>(scope, payload);
    let (function, histogram, args, start) = {
        let data = state.data();
        (
            data.function.get(scope),
            data.histogram
                .as_ref()
                .map(|histogram| histogram.get(scope)),
            data.args.get(scope),
            data.start,
        )
    };
    process_complete(scope, function, start, args, histogram)?;
    Ok(js::value::undefined())
}

/// Node.js's `processComplete`: record the duration of a call of `function` that started at
/// `start` in `histogram`, and queue a `function` entry with the call's arguments `args` as its
/// `detail` and its indexed properties.
fn process_complete(
    scope: &Scope<'_>,
    function: Object<'_>,
    start: f64,
    args: Object<'_>,
    histogram: Option<Object<'_>>,
) -> Result<(), ExnThrown> {
    let duration = web_globals::performance::now() - start;
    if let Some(histogram) = histogram {
        let nanoseconds = scope.root_value(js::value::from_f64((duration * 1e6).ceil()));
        Function::call_by_name(scope, histogram.handle(), c"record", &[nanoseconds])?;
    }
    let name = function.get_property(scope, c"name")?;
    let name = crate::errors::js_string(scope, name)?;
    let detail = args.to_jsval_throwing(scope)?;
    let entry = new_node_entry(scope, name, "function", start, duration, detail)?;
    let length = args
        .cast::<js::Array>()
        .map_or(Ok(0), |args| args.length(scope))?;
    let object = entry.as_object();
    for index in 0..length {
        let value = args.get_element(scope, index)?;
        object.set_element(scope, index, value)?;
    }
    web_globals::performance::queue_entry(scope, *entry)
}

/// The `Performance` object `this`, or `ERR_INVALID_THIS`.
fn this_performance<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, args.this())
        .ok()
        .filter(|object| {
            object
                .cast::<web_globals::performance::Performance>()
                .is_ok()
        })
        .ok_or_else(|| crate::errors::invalid_this(scope, "Performance"))
}

/// <https://nodejs.org/api/perf_hooks.html#performancetojson>: `nodeTiming`, `timeOrigin` and
/// the result of `eventLoopUtilization()`, read from `this`.
fn performance_to_json(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = this_performance(scope, &args)?;
    let node_timing = this.get_property(scope, c"nodeTiming")?;
    let time_origin = this.get_property(scope, c"timeOrigin")?;
    let elu = Function::call_by_name(
        scope,
        this.handle(),
        c"eventLoopUtilization",
        &[] as &[HandleValue],
    )?;
    let json = Object::new_plain_with_template(
        scope,
        &PERFORMANCE_JSON_PROPERTIES,
        &[node_timing, time_origin, elu],
    )?;
    json.to_jsval_raw_throwing(scope)
}

/// The inspect options of a custom inspection's `options` argument, one level deeper.
fn nested_inspect_options(
    scope: &Scope<'_>,
    options: HandleValue<'_>,
) -> Result<crate::inspect::InspectOptions, ExnThrown> {
    let mut inspect_options = crate::inspect::InspectOptions::current(scope)?;
    if let Ok(given) = Object::from_value(scope, *options) {
        inspect_options = inspect_options.read(scope, given)?;
    }
    inspect_options.depth = inspect_options.depth.map(|depth| depth - 1.0);
    Ok(inspect_options)
}

/// Whether a custom inspection's `depth` argument is negative, which makes it return `this`.
fn below_depth(depth: HandleValue<'_>) -> bool {
    depth.is_number() && depth.to_number() < 0.0
}

/// `performance[util.inspect.custom](depth, options)`: `Performance` followed by `nodeTiming`
/// and `timeOrigin`, as Node.js shows the `performance` object.
fn inspect_performance(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if below_depth(args.get(0)) {
        return Ok(args.this());
    }
    let this = this_performance(scope, &args)?;
    let options = nested_inspect_options(scope, args.get(1))?;
    let values = [
        this.get_property(scope, c"nodeTiming")?,
        this.get_property(scope, c"timeOrigin")?,
    ];
    let shown = Object::new_plain_with_template(scope, &PERFORMANCE_INSPECT_PROPERTIES, &values)?;
    let shown = shown.to_jsval_throwing(scope)?;
    let text = format!(
        "Performance {}",
        crate::inspect::inspect(scope, shown, options)?
    );
    js::JSString::from_str(scope, &text)?.to_jsval_raw_throwing(scope)
}

/// `entry[util.inspect.custom](depth, options)` of performance entries and `nodeTiming`: the name
/// of the entry's constructor followed by the inspected result of its `toJSON()`, as Node.js
/// shows them.
fn inspect_entry(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if below_depth(args.get(0)) {
        return Ok(args.this());
    }
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    let options = nested_inspect_options(scope, args.get(1))?;
    let constructor = this.get_property(scope, c"constructor")?;
    let name = match Object::from_value(scope, *constructor) {
        Ok(constructor) => {
            crate::errors::js_string(scope, constructor.get_property(scope, c"name")?)?
        }
        Err(_) => String::new(),
    };
    let json = Function::call_by_name(scope, this.handle(), c"toJSON", &[] as &[HandleValue])?;
    let text = format!("{name} {}", crate::inspect::inspect(scope, json, options)?);
    js::JSString::from_str(scope, &text)?.to_jsval_raw_throwing(scope)
}

/// Define `body` as the `[util.inspect.custom]` method of `proto`.
fn define_inspect(
    scope: &Scope<'_>,
    proto: Object<'_>,
    body: js::function::Callback,
) -> Result<(), ExnThrown> {
    let inspect = Function::new_callback(scope, c"[nodejs.util.inspect.custom]", 2, body, ())?;
    proto.define_value_by_id(
        scope,
        crate::inspect::INSPECT_CUSTOM.id(scope)?,
        *inspect,
        js::class_spec::JSPROP_READONLY as u32,
    )
}

/// Markers keying the realm's `eventLoopUtilization` and `timerify` functions, which
/// `node:perf_hooks` exports.
struct EventLoopUtilization;
struct Timerify;

/// Register the classes of `node:perf_hooks` and their templates.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    ELU_PROPERTIES.register(scope)?;
    PERFORMANCE_JSON_PROPERTIES.register(scope)?;
    PERFORMANCE_INSPECT_PROPERTIES.register(scope)?;
    NODE_TIMING_JSON_PROPERTIES.register(scope)?;
    UV_METRICS_PROPERTIES.register(scope)?;
    GC_DETAIL_PROPERTIES.register(scope)?;
    PerformanceNodeEntry::add_to_global(scope, global);
    PerformanceNodeTiming::add_to_global(scope, global);
    Timerified::add_to_global(scope, global);
    TimerifyCompletion::add_to_global(scope, global);
    histogram::register_classes(scope, global)
}

/// Add Node.js's members to `Performance.prototype`, and its custom inspection to the
/// prototypes of `Performance` and `PerformanceEntry`.
pub(crate) fn install(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let Some(proto) = js::class::get_prototype_object_for::<PerformanceImpl>(scope) else {
        return Ok(());
    };
    let elu = Function::new_callback(
        scope,
        c"eventLoopUtilization",
        2,
        event_loop_utilization,
        (),
    )?;
    let timerify = Function::new_callback(scope, c"timerify", 1, timerify, ())?;
    js::class::register_realm_object::<EventLoopUtilization>(scope, *elu);
    js::class::register_realm_object::<Timerify>(scope, *timerify);
    let to_json = Function::new_callback(scope, c"toJSON", 0, performance_to_json, ())?;
    let enumerable = js::class_spec::JSPROP_ENUMERATE as u32;
    proto.define_property(scope, c"toJSON", *to_json, enumerable)?;
    proto.define_property(scope, c"eventLoopUtilization", *elu, 0)?;
    proto.define_property(scope, c"nodeTiming", new_node_timing(scope)?, 0)?;
    proto.define_property(scope, c"timerify", *timerify, 0)?;
    define_inspect(scope, proto, inspect_performance)?;
    if let Some(entry_proto) = js::class::get_prototype_object_for::<PerformanceEntryImpl>(scope) {
        define_inspect(scope, entry_proto, inspect_entry)?;
    }
    Ok(())
}

/// Register `node:perf_hooks` as builtin module `name`, with the globals as its classes and its
/// `performance`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        use web_globals::performance::entry::{PerformanceMarkImpl, PerformanceMeasureImpl};
        use web_globals::performance::observer::{
            PerformanceObserverEntryListImpl, PerformanceObserverImpl,
        };
        let classes = [
            (
                "Performance",
                js::class::class_constructor::<PerformanceImpl>(scope)?,
            ),
            (
                "PerformanceEntry",
                js::class::class_constructor::<PerformanceEntryImpl>(scope)?,
            ),
            (
                "PerformanceMark",
                js::class::class_constructor::<PerformanceMarkImpl>(scope)?,
            ),
            (
                "PerformanceMeasure",
                js::class::class_constructor::<PerformanceMeasureImpl>(scope)?,
            ),
            (
                "PerformanceObserver",
                js::class::class_constructor::<PerformanceObserverImpl>(scope)?,
            ),
            (
                "PerformanceObserverEntryList",
                js::class::class_constructor::<PerformanceObserverEntryListImpl>(scope)?,
            ),
        ];
        let mut exports = Vec::new();
        for (export, class) in classes {
            exports.push((export, class.to_jsval_throwing(scope)?));
        }
        let performance = web_globals::performance::performance_object(scope).ok_or(ExnThrown)?;
        exports.push((
            "performance",
            performance.as_object().to_jsval_throwing(scope)?,
        ));
        let elu = js::class::realm_object::<EventLoopUtilization>(scope).ok_or(ExnThrown)?;
        exports.push(("eventLoopUtilization", elu.to_jsval_throwing(scope)?));
        let timerify = js::class::realm_object::<Timerify>(scope).ok_or(ExnThrown)?;
        exports.push(("timerify", timerify.to_jsval_throwing(scope)?));
        exports.push(("constants", constants(scope)?.to_jsval_throwing(scope)?));
        Ok(exports)
    };
    let Ok(exports) = exports() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &exports)
}

#[core_runtime::jsmodule(node_args, name = "node:perf_hooks", default_namespace)]
pub mod module {
    use super::*;

    /// <https://nodejs.org/api/perf_hooks.html#perf_hookscreatehistogramoptions>
    pub fn create_histogram<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        histogram::create_histogram(scope, options.unwrap_or(HandleValue::undefined()))
    }

    /// <https://nodejs.org/api/perf_hooks.html#perf_hooksmonitoreventloopdelayoptions>
    pub fn monitor_event_loop_delay<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        histogram::monitor_event_loop_delay(scope, options.unwrap_or(HandleValue::undefined()))
    }
}
