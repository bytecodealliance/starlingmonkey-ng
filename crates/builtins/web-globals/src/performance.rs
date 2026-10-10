// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://www.w3.org/TR/hr-time-3/#sec-performance/>
//!
//! Implements [Performance] API from the High Resolution Time specification
//! The time origin is the moment the runtime was initialized with the Performance Global
//!
//! The `Performance` object also holds the global's performance timeline: the `mark` and
//! `measure` entries of [User Timing], and the registered observers of [Performance Timeline].
//!
//! [User Timing]: https://w3c.github.io/user-timing/
//! [Performance Timeline]: https://w3c.github.io/performance-timeline/

pub mod entry;
pub mod observer;

use std::cell::Cell;
use std::rc::{Rc, Weak};

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods, webidl_union};
use js::conversion::{Finite, FromJSVal};
use js::error::{BindingSite, ExnThrown, JSExnType, OperationError, ThrowException};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::RootedTraceableBox;
use js::prelude::HandleValue;
use js::Object;

use crate::dom_exception::throw_dom_exception;
use crate::events::event_target::{EventTarget, EventTargetImpl};
use entry::{PerformanceEntry, PerformanceEntryImpl, PerformanceMark, PerformanceMarkImpl};
use entry::{PerformanceMarkOptions, PerformanceMeasure, PerformanceMeasureImpl};
use observer::{PerformanceObserver, PerformanceObserverImpl};

#[derive(Clone, Copy)]
struct TimeOriginSnapshot {
    /// The monotonic clock reading, in nanoseconds, when the time origin was captured.
    monotonic_ns: u64,
    /// ECMA-262 timestamp (ms since Unix epoch) for the same moment. Used to compute `timeOrigin`
    /// that is stable against system clock changes.
    epoch_ms: f64,
}

/// Held behind a lock rather than in a `OnceLock` because it has to be replaceable: an origin
/// captured before a Wizer snapshot holds the wall-clock time of the machine that took the
/// snapshot, which bears no relation to when the resumed instance runs. [`reset_time_origin`]
/// re-establishes that half on resume. The monotonic half stays as captured, since
/// [`platform::clock`](platform::clock) already puts a resumed instance's readings past it.
static TIME_ORIGIN: std::sync::RwLock<Option<TimeOriginSnapshot>> = std::sync::RwLock::new(None);

/// The current wall-clock time as an ECMA-262 timestamp (ms since Unix epoch).
fn epoch_ms_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs_f64()
        * 1_000.
}

fn capture_time_origin() -> TimeOriginSnapshot {
    TimeOriginSnapshot {
        monotonic_ns: platform::clock::monotonic_ns(),
        epoch_ms: epoch_ms_now(),
    }
}

/// Milliseconds elapsed since `origin` was captured.
fn elapsed_ms(origin: &TimeOriginSnapshot) -> f64 {
    platform::clock::monotonic_ns().saturating_sub(origin.monotonic_ns) as f64 / 1_000_000.
}

/// The resolution, in nanoseconds, that [`relative_ms`] coarsens timestamps to: 100
/// microseconds, the resolution of <https://w3c.github.io/hr-time/#dfn-coarsen-time> without
/// cross-origin isolated capability.
const COARSENED_RESOLUTION_NS: i128 = 100_000;

/// The timestamp `ns` nanoseconds after the time origin, in milliseconds. While the web security
/// restrictions apply (see [`core_runtime::config::web_security_restrictions`]), it is coarsened
/// to [`COARSENED_RESOLUTION_NS`], rounding down.
fn relative_ms(ns: i128) -> f64 {
    let ns = if core_runtime::config::web_security_restrictions() {
        ns.div_euclid(COARSENED_RESOLUTION_NS) * COARSENED_RESOLUTION_NS
    } else {
        ns
    };
    ns as f64 / 1_000_000.
}

fn init_time_origin() {
    let _ = time_origin();
}

/// Re-anchor the time origin's wall clock to the instance running now.
///
/// A pre-initialized (Wizer) snapshot includes an origin captured while the snapshot was taken, so
/// `timeOrigin` would report the wall-clock time of whoever took it. The origin's monotonic half
/// stays valid, since a resumed instance reads the clock through an offset past everything the
/// snapshot holds. Only the wall clock is re-read, from the elapsed time that monotonic half
/// gives, so `timeOrigin + performance.now()` is the current time and `now()` counts on across the
/// snapshot rather than restarting from zero. A no-op for anyone who never snapshots.
pub fn reset_time_origin() {
    let mut origin = TIME_ORIGIN.write().expect("time origin lock poisoned");
    match origin.as_mut() {
        Some(snapshot) => snapshot.epoch_ms = epoch_ms_now() - elapsed_ms(snapshot),
        None => *origin = Some(capture_time_origin()),
    }
}

fn time_origin() -> TimeOriginSnapshot {
    if let Some(origin) = *TIME_ORIGIN.read().expect("time origin lock poisoned") {
        return origin;
    }
    let mut origin = TIME_ORIGIN.write().expect("time origin lock poisoned");
    *origin.get_or_insert_with(capture_time_origin)
}

/// The `Performance` interface.
///
/// <https://www.w3.org/TR/hr-time-3/#sec-performance>
#[webidl_interface(extends = EventTarget)]
pub struct Performance {
    parent: EventTargetImpl,
    /// The `mark` entries of the performance entry buffer, in the order they were added.
    marks: Vec<Heap<PerformanceEntryImpl>>,
    /// The `measure` entries of the performance entry buffer, in the order they were added.
    measures: Vec<Heap<PerformanceEntryImpl>>,
    /// <https://w3c.github.io/performance-timeline/#list-of-registered-performance-observers>
    observers: Vec<Heap<PerformanceObserverImpl>>,
    /// <https://w3c.github.io/performance-timeline/#dfn-performance-observer-task-queued-flag>:
    /// set while the [`ObserverTask`] holding this token is queued. A task that is dropped
    /// without running, with the event loop that held it, unsets it too.
    #[no_trace]
    observer_task_queued: Weak<()>,
}

/// <https://w3c.github.io/hr-time/#dfn-current-high-resolution-time>: the milliseconds since the
/// time origin, coarsened as [`relative_ms`] describes.
pub fn now() -> f64 {
    let origin = time_origin().monotonic_ns;
    relative_ms(i128::from(
        platform::clock::monotonic_ns().saturating_sub(origin),
    ))
}

/// The milliseconds from the time origin to the [`platform::clock::monotonic_ns`] reading
/// `monotonic_ns`, negative for a reading before it, coarsened as [`relative_ms`] describes.
pub fn ms_since_origin(monotonic_ns: u64) -> f64 {
    let origin = time_origin().monotonic_ns;
    relative_ms(i128::from(monotonic_ns) - i128::from(origin))
}

/// The time origin as an ECMA-262 timestamp, `performance.timeOrigin`.
pub fn time_origin_ms() -> f64 {
    time_origin().epoch_ms
}

/// What an embedding adds to the performance timeline of every global, which
/// [`register_timeline_extension`] registers.
pub struct TimelineExtension {
    /// The entry types `PerformanceObserver.supportedEntryTypes` lists, in that order. Observers
    /// ignore other types.
    pub supported_entry_types: &'static [&'static str],
    /// The timestamp a mark name stands for in `performance.measure`, or `None` for the names
    /// of marks. `PerformanceMark` rejects the names that have a timestamp.
    pub reserved_timestamp: fn(&Scope<'_>, &str) -> Option<f64>,
    /// Called with `true` when an observer starts observing an entry type, and with `false` when
    /// it stops.
    pub observed: fn(&Scope<'_>, &'static str, bool),
    /// Whether `measure` takes measure options with neither `start` nor `end`, which measure from
    /// 0 to now, as Node.js's does. Without the extension, such options throw a `TypeError`
    /// unless they are empty.
    pub measure_options_without_start_or_end: bool,
}

/// The entry types the timeline supports without a [`TimelineExtension`].
const SUPPORTED_ENTRY_TYPES: &[&str] = &["mark", "measure"];

js::instance_local! {
    /// The extension [`register_timeline_extension`] registered.
    static EXTENSION: Cell<Option<&'static TimelineExtension>> = const { Cell::new(None) };
}

/// Register `extension` for the performance timelines of the globals set up after this call,
/// replacing any extension registered before.
pub fn register_timeline_extension(extension: &'static TimelineExtension) {
    EXTENSION.with(|cell| cell.set(Some(extension)));
}

/// The entry types observers can observe.
fn supported_entry_types() -> &'static [&'static str] {
    EXTENSION
        .with(Cell::get)
        .map_or(SUPPORTED_ENTRY_TYPES, |extension| {
            extension.supported_entry_types
        })
}

/// The `TypeError` with `message` of the member at `site` for the negative timestamp `timestamp`,
/// whose code is `ERR_PERFORMANCE_INVALID_TIMESTAMP`.
fn invalid_timestamp(site: BindingSite, message: &str, timestamp: f64) -> OperationError {
    OperationError {
        site,
        kind: JSExnType::JSEXN_TYPEERR,
        message: message.into(),
        code: "ERR_PERFORMANCE_INVALID_TIMESTAMP",
        args: vec![timestamp.to_string()],
    }
}

/// The timestamp the [`TimelineExtension`] reserves `name` for.
fn reserved_timestamp(scope: &Scope<'_>, name: &str) -> Option<f64> {
    EXTENSION
        .with(Cell::get)
        .and_then(|extension| (extension.reserved_timestamp)(scope, name))
}

/// Tell the [`TimelineExtension`] that an observer started or stopped observing `entry_type`.
fn observed(scope: &Scope<'_>, entry_type: &'static str, observing: bool) {
    if let Some(extension) = EXTENSION.with(Cell::get) {
        (extension.observed)(scope, entry_type, observing);
    }
}

/// The marker type the `performance` object of a global is registered as a realm object for.
struct PerformanceObject;

/// The `performance` object of the current global, if the global has one.
pub fn performance_object<'s>(scope: &'s Scope<'_>) -> Option<Performance<'s>> {
    js::class::realm_object::<PerformanceObject>(scope)?
        .cast::<Performance>()
        .ok()
}

/// <https://w3c.github.io/performance-timeline/#dfn-queue-a-performanceentry>: add `entry` to
/// the observer buffer of each registered observer of the current global that observes its type,
/// and queue the PerformanceObserver task if one did. The entry isn't added to the performance
/// entry buffer.
pub fn queue_entry(scope: &Scope<'_>, entry: PerformanceEntry<'_>) -> Result<(), ExnThrown> {
    let Some(performance) = performance_object(scope) else {
        return Ok(());
    };
    // Step 1: Let interested observers be an initially empty set of PerformanceObserver
    //         objects.
    // Step 2: Let entryType be newEntry's entryType value.
    let entry_type = entry.data().entry_type();
    // Step 3: Let relevantGlobal be newEntry's relevant global object.
    // Step 4: For each registered performance observer regObs in relevantGlobal's list of
    //         registered performance observer objects: if regObs's options list contains a
    //         PerformanceObserverInit options whose entryTypes member includes entryType or
    //         whose type member equals entryType, append regObs's observer to interested
    //         observers.
    let observers: Vec<PerformanceObserver<'_>> = performance
        .data()
        .observers
        .iter()
        .map(|observer| observer.get(scope))
        .collect();
    let mut interested = false;
    // Step 5: For each observer in interested observers: append newEntry to observer's
    //         observer buffer.
    for observer in observers {
        let mut data = observer.data_mut();
        if data.observes(entry_type) {
            data.push(entry);
            interested = true;
        }
    }
    // Step 6-7: If entryType's buffer is to be added to, add newEntry to it.
    // Note: the callers that add entries to a buffer do so themselves.
    // Step 8: Queue a PerformanceObserver task with relevantGlobal as input.
    if interested {
        queue_observer_task(scope)?;
    }
    Ok(())
}

/// Whether a registered observer of the current global observes `entry_type`.
pub fn is_observed(scope: &Scope<'_>, entry_type: &str) -> bool {
    performance_object(scope).is_some_and(|performance| {
        let observers: Vec<PerformanceObserver<'_>> = performance
            .data()
            .observers
            .iter()
            .map(|observer| observer.get(scope))
            .collect();
        observers
            .iter()
            .any(|observer| observer.data().observes(entry_type))
    })
}

/// Add `observer` to the list of registered performance observers of the current global, unless
/// it is in the list.
fn register_observer(
    scope: &Scope<'_>,
    observer: &PerformanceObserver<'_>,
) -> Result<(), ExnThrown> {
    if observer.is_registered() {
        return Ok(());
    }
    let Some(performance) = performance_object(scope) else {
        return Ok(());
    };
    performance.data_mut().observers.push(Heap::from(*observer));
    observer.set_registered(true);
    Ok(())
}

/// Remove `observer` from the list of registered performance observers of the current global.
fn unregister_observer(scope: &Scope<'_>, observer: &PerformanceObserver<'_>) {
    if !observer.is_registered() {
        return;
    }
    observer.set_registered(false);
    let Some(performance) = performance_object(scope) else {
        return;
    };
    performance.data_mut().observers.retain(|registered| {
        registered.get(scope).as_object().as_raw() != observer.as_object().as_raw()
    });
}

/// <https://w3c.github.io/performance-timeline/#dfn-filter-buffer-map-by-name-and-type>: the
/// entries of the current global's performance entry buffer with `name` and of `entry_type`,
/// each if given, sorted by `startTime`.
fn buffered_entries<'s>(
    scope: &'s Scope<'_>,
    name: Option<&str>,
    entry_type: Option<&str>,
) -> Result<Vec<PerformanceEntry<'s>>, ExnThrown> {
    let Some(performance) = performance_object(scope) else {
        return Ok(Vec::new());
    };
    Ok(performance.filter_buffer(scope, name, entry_type))
}

/// <https://w3c.github.io/performance-timeline/#dfn-queue-a-performanceobserver-task>: queue
/// the task that runs the observers' callbacks, unless it is queued. The task is due at once, in
/// the order of the timers, and keeps the event loop running until it runs.
fn queue_observer_task(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let Some(performance) = performance_object(scope) else {
        return Ok(());
    };
    // Step 1: If relevantGlobal's performance observer task queued flag is set, terminate these
    //         steps.
    if performance.data().observer_task_queued.strong_count() > 0 {
        return Ok(());
    }
    // Step 2: Set relevantGlobal's performance observer task queued flag.
    // Step 3: Queue a task that consists of running the following substeps.
    let steps = js::class::get_or_init_shared_function(
        scope,
        run_observer_task as *const () as usize,
        |scope| js::Function::new_callback(scope, c"", 0, run_observer_task, ()),
    )?;
    let queued = Rc::new(());
    let token = Rc::downgrade(&queued);
    let task = ObserverTask {
        steps: RootedTraceableBox::new(Heap::from(steps.as_callable())),
        queued,
    };
    let queued = core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop.queue_timer(Box::new(task), platform::clock::Instant::now())
    });
    if queued.is_some() {
        performance.data_mut().observer_task_queued = token;
    }
    Ok(())
}

/// The PerformanceObserver task, which calls the shared function of [`run_observer_task`] so that
/// the steps run in the function's realm.
struct ObserverTask {
    steps: RootedTraceableBox<Heap<js::function::Callable>>,
    /// The token of the performance observer task queued flag, which dropping the task unsets.
    queued: Rc<()>,
}

impl core_runtime::event_loop::Task for ObserverTask {
    fn kind(&self) -> &'static str {
        "performance observer"
    }

    fn run(
        self: Box<Self>,
        scope: &Scope<'_>,
        _id: core_runtime::event_loop::TaskId,
    ) -> Result<(), ExnThrown> {
        let ObserverTask { steps, queued } = *self;
        // Step 3.1: Unset performance observer task queued flag of relevantGlobal.
        drop(queued);
        js::Function::call(
            scope,
            HandleValue::undefined(),
            steps.get(scope),
            js::function::EmptyArgs,
        )?;
        Ok(())
    }
}

/// The substeps of the PerformanceObserver task, after its queued flag is unset: run the
/// callback of each registered observer with a non-empty observer buffer. An exception a
/// callback throws is reported, and the next observer is notified.
///
/// <https://w3c.github.io/performance-timeline/#dfn-queue-a-performanceobserver-task>
fn run_observer_task(
    scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let Some(performance) = performance_object(scope) else {
        return Ok(js::value::undefined());
    };
    // Step 3.2: Let notifyList be a copy of relevantGlobal's list of registered performance
    //           observer objects.
    let notify_list: Vec<PerformanceObserver<'_>> = performance
        .data()
        .observers
        .iter()
        .map(|observer| observer.get(scope))
        .collect();
    // Step 3.3: For each registered performance observer object registeredObserver in
    //           notifyList, run these steps.
    for observer in notify_list {
        if observer.notify(scope).is_err() {
            core_runtime::event_loop::handle_and_clear_exception(scope);
        }
    }
    Ok(js::value::undefined())
}

/// A mark name or a timestamp, as `measure` takes them.
#[webidl_union]
pub enum MarkReference {
    Timestamp(f64),
    Name(String),
}

/// <https://w3c.github.io/user-timing/#dom-performancemeasureoptions>
#[derive(Default)]
#[webidl_dictionary]
pub struct PerformanceMeasureOptions<'a> {
    pub detail: Option<HandleValue<'a>>,
    pub start: Option<MarkReference>,
    pub duration: Option<Finite<f64>>,
    pub end: Option<MarkReference>,
}

/// The `startOrMeasureOptions` argument of `measure`.
enum StartOrMeasureOptions<'a> {
    Start(String),
    Options(PerformanceMeasureOptions<'a>),
}

/// [`now`] from [`platform::clock::coarse_monotonic_ns`], for timestamps that don't need the
/// precise clock, such as `Event.timeStamp`.
pub fn coarse_now() -> f64 {
    let origin = time_origin().monotonic_ns;
    relative_ms(i128::from(
        platform::clock::coarse_monotonic_ns().saturating_sub(origin),
    ))
}

#[webidl_methods]
impl Performance {
    /// <https://www.w3.org/TR/hr-time-3/#now-method>
    ///
    /// Return the number of milliseconds since the time origin, as a `double`.
    ///
    /// The IDL defines the return as `DOMHighResTimeStamp` which is a
    /// `double` in JS (64-bit float).
    #[method]
    pub fn now(&self) -> f64 {
        now()
    }

    /// <https://www.w3.org/TR/hr-time-3/#timeorigin-attribute>
    ///
    /// Return the time origin as an ECMA-262 timestamp (ms since Unix epoch).
    /// This is a fixed value: the wall-clock time (in ms since Unix epoch)
    /// at which the runtime's time origin was established.
    #[getter]
    fn time_origin(&self) -> f64 {
        time_origin().epoch_ms
    }

    /// <https://www.w3.org/TR/hr-time-3/#tojson-method>
    ///
    /// Return a plain object with `timeOrigin` property.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    fn to_json<'a>(&self, scope: &'a Scope<'a>) -> Result<Object<'a>, ExnThrown> {
        let obj = Object::new_plain(scope)?;
        let time_origin_val = self.time_origin();
        obj.set_property(scope, c"timeOrigin", time_origin_val)?;
        Ok(obj)
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performance-getentries>
    #[method]
    fn get_entries<'r>(&self, scope: &'r Scope<'_>) -> Vec<PerformanceEntry<'r>> {
        self.filter_buffer(scope, None, None)
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performance-getentriesbytype>
    #[method]
    fn get_entries_by_type<'r>(
        &self,
        scope: &'r Scope<'_>,
        entry_type: String,
    ) -> Vec<PerformanceEntry<'r>> {
        self.filter_buffer(scope, None, Some(&entry_type))
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performance-getentriesbyname>
    #[method]
    fn get_entries_by_name<'r>(
        &self,
        scope: &'r Scope<'_>,
        name: String,
        entry_type: Option<String>,
    ) -> Vec<PerformanceEntry<'r>> {
        self.filter_buffer(scope, Some(&name), entry_type.as_deref())
    }

    /// <https://w3c.github.io/user-timing/#dom-performance-mark>
    #[method]
    fn mark<'r>(
        &self,
        scope: &'r Scope<'_>,
        mark_name: String,
        mark_options: Option<PerformanceMarkOptions<'_>>,
    ) -> Result<PerformanceMark<'r>, ExnThrown> {
        // Step 1: Run the PerformanceMark constructor and let entry be the newly created object.
        let entry = js::class::try_create_instance_with::<PerformanceMarkImpl>(scope, |_| {
            PerformanceMarkImpl::create(scope, mark_name, mark_options)
        })?;
        // Step 2: Queue entry.
        queue_entry(scope, *entry)?;
        // Step 3: Add entry to the performance entry buffer.
        self.data_mut().marks.push(Heap::from(*entry));
        // Step 4: Return entry.
        Ok(entry)
    }

    /// <https://w3c.github.io/user-timing/#dom-performance-clearmarks>
    #[method]
    fn clear_marks(&self, scope: &Scope<'_>, mark_name: Option<String>) {
        // Step 1: If markName is omitted, remove all PerformanceMark objects from the
        //         performance entry buffer.
        // Step 2: Otherwise, remove all PerformanceMark objects listed in the performance entry
        //         buffer whose name is markName.
        // Step 3: Return undefined.
        retain_unnamed(scope, &mut self.data_mut().marks, mark_name.as_deref());
    }

    /// <https://w3c.github.io/user-timing/#dom-performance-measure>
    #[method]
    fn measure<'r>(
        &self,
        scope: &'r Scope<'_>,
        measure_name: String,
        start_or_measure_options: Option<HandleValue<'_>>,
        end_mark: Option<String>,
    ) -> Result<PerformanceMeasure<'r>, ExnThrown> {
        let start_or_measure_options = match start_or_measure_options {
            Some(value) if !value.is_null_or_undefined() && !value.is_object() => {
                StartOrMeasureOptions::Start(String::from_jsval_throwing(scope, value, ())?)
            }
            Some(value) => StartOrMeasureOptions::Options(
                PerformanceMeasureOptions::from_jsval_throwing(scope, value, ())?,
            ),
            None => StartOrMeasureOptions::Options(PerformanceMeasureOptions::default()),
        };
        let options = match &start_or_measure_options {
            StartOrMeasureOptions::Options(options) => Some(options),
            StartOrMeasureOptions::Start(_) => None,
        };
        // Step 1: If startOrMeasureOptions is a PerformanceMeasureOptions object and at least one
        //         of start, end, duration, and detail exist, run the following checks:
        // Note: with a `TimelineExtension` that takes options without `start` and `end`, the
        // checks run for options with either.
        let lenient = EXTENSION
            .with(Cell::get)
            .is_some_and(|extension| extension.measure_options_without_start_or_end);
        if let Some(options) = options.filter(|options| {
            options.start.is_some()
                || options.end.is_some()
                || (!lenient && (options.duration.is_some() || options.detail.is_some()))
        }) {
            // Step 1.1: If endMark is given, throw a TypeError.
            if end_mark.is_some() {
                return Err(js::error::throw_type_error(
                    scope,
                    c"measure can't take an endMark with measure options",
                ));
            }
            // Step 1.2: If startOrMeasureOptions's start and end members are both omitted, throw
            //           a TypeError.
            if options.start.is_none() && options.end.is_none() {
                return Err(js::error::throw_type_error(
                    scope,
                    c"measure options need a start or an end",
                ));
            }
            // Step 1.3: If startOrMeasureOptions's start, duration, and end members are all
            //           present, throw a TypeError.
            if options.start.is_some() && options.duration.is_some() && options.end.is_some() {
                return Err(js::error::throw_type_error(
                    scope,
                    c"measure options can't have a start, a duration and an end",
                ));
            }
        }
        let duration = options.and_then(|options| options.duration.map(|d| d.get()));
        // Step 2: Compute end time as follows:
        let end_time = if let Some(end_mark) = &end_mark {
            // Step 2.1: If endMark is given, let end time be the value returned by running the
            //           convert a mark to a timestamp algorithm passing in endMark.
            self.convert_mark(scope, &MarkReference::Name(end_mark.clone()))?
        } else if let Some(end) = options.and_then(|options| options.end.as_ref()) {
            // Step 2.2: Otherwise, if startOrMeasureOptions is a PerformanceMeasureOptions
            //           object, and if its end member exists, let end time be the value
            //           returned by running the convert a mark to a timestamp algorithm passing
            //           in startOrMeasureOptions's end.
            self.convert_mark(scope, end)?
        } else if let (Some(start), Some(duration)) =
            (options.and_then(|options| options.start.as_ref()), duration)
        {
            // Step 2.3: Otherwise, if startOrMeasureOptions is a PerformanceMeasureOptions
            //           object, and if its start and duration members both exist:
            // Step 2.3.1: Let start be the value returned by running the convert a mark to a
            //             timestamp algorithm passing in start.
            let start = self.convert_mark(scope, start)?;
            // Step 2.3.2: Let duration be the value returned by running the convert a mark to a
            //             timestamp algorithm passing in duration.
            let duration = self.convert_mark(scope, &MarkReference::Timestamp(duration))?;
            // Step 2.3.3: Let end time be start plus duration.
            start + duration
        } else {
            // Step 2.4: Otherwise, let end time be the value that would be returned by the
            //           Performance object's now() method.
            now()
        };
        // Step 3: Compute start time as follows:
        let start_time = match &start_or_measure_options {
            // Step 3.1: If startOrMeasureOptions is a DOMString, let start time be the value
            //           returned by running the convert a mark to a timestamp algorithm passing
            //           in startOrMeasureOptions.
            StartOrMeasureOptions::Start(start) => {
                self.convert_mark(scope, &MarkReference::Name(start.clone()))?
            }
            // Step 3.2: Otherwise, if startOrMeasureOptions is a PerformanceMeasureOptions
            //           object, and if its start member exists, let start time be the value
            //           returned by running the convert a mark to a timestamp algorithm passing
            //           in startOrMeasureOptions's start.
            StartOrMeasureOptions::Options(PerformanceMeasureOptions {
                start: Some(start),
                ..
            }) => self.convert_mark(scope, start)?,
            // Step 3.3: Otherwise, if startOrMeasureOptions is a PerformanceMeasureOptions
            //           object, and if its duration and end members both exist:
            StartOrMeasureOptions::Options(PerformanceMeasureOptions {
                duration: Some(duration),
                end: Some(end),
                ..
            }) => {
                // Step 3.3.1: Let duration be the value returned by running the convert a mark to
                //             a timestamp algorithm passing in duration.
                let duration =
                    self.convert_mark(scope, &MarkReference::Timestamp(duration.get()))?;
                // Step 3.3.2: Let end be the value returned by running the convert a mark to a
                //             timestamp algorithm passing in end.
                let end = self.convert_mark(scope, end)?;
                // Step 3.3.3: Let start time be end minus duration.
                end - duration
            }
            // Step 3.4: Otherwise, let start time be 0.
            _ => 0.0,
        };
        // Step 4-8: Create a new PerformanceMeasure object (entry) with name measureName,
        //           entryType "measure", startTime start time and duration end time minus start
        //           time.
        // Step 9: Set entry's detail as follows: a structured clone of startOrMeasureOptions's
        //         detail if it is a PerformanceMeasureOptions object with a detail member, and
        //         null otherwise.
        let detail = entry::clone_detail(scope, options.and_then(|options| options.detail))?;
        let entry = js::class::create_instance_with::<PerformanceMeasureImpl>(scope, |_| {
            PerformanceMeasureImpl::with_detail(
                measure_name,
                start_time,
                end_time - start_time,
                detail,
            )
        })?;
        // Step 10: Queue entry.
        queue_entry(scope, *entry)?;
        // Step 11: Add entry to the performance entry buffer.
        self.data_mut().measures.push(Heap::from(*entry));
        // Step 12: Return entry.
        Ok(entry)
    }

    /// <https://w3c.github.io/user-timing/#dom-performance-clearmeasures>
    #[method]
    fn clear_measures(&self, scope: &Scope<'_>, measure_name: Option<String>) {
        // Step 1: If measureName is omitted, remove all PerformanceMeasure objects in the
        //         performance entry buffer.
        // Step 2: Otherwise remove all PerformanceMeasure objects listed in the performance
        //         entry buffer whose name is measureName.
        // Step 3: Return undefined.
        retain_unnamed(
            scope,
            &mut self.data_mut().measures,
            measure_name.as_deref(),
        );
    }

    /// <https://w3c.github.io/user-timing/#convert-a-mark-to-a-timestamp>
    fn convert_mark(&self, scope: &Scope<'_>, mark: &MarkReference) -> Result<f64, ExnThrown> {
        match mark {
            // Step 1: If mark is a DOMString and it has the same name as a read only attribute in
            //         the PerformanceTiming interface, let end time be the value returned by
            //         running the convert a name to a timestamp algorithm with name set to the
            //         value of mark.
            // Note: the names of a registered `TimelineExtension`'s timestamps take the place of
            // `PerformanceTiming`'s attributes.
            MarkReference::Name(name) => {
                if let Some(timestamp) = reserved_timestamp(scope, name) {
                    return Ok(timestamp);
                }
                // Step 2: Otherwise, if mark is a DOMString, let end time be the value of the
                //         startTime attribute from the most recent occurrence of a
                //         PerformanceMark object in the performance entry buffer whose name is
                //         mark. If no matching entry is found, throw a SyntaxError.
                let marks: Vec<PerformanceEntry<'_>> = self
                    .data()
                    .marks
                    .iter()
                    .map(|mark| mark.get(scope))
                    .collect();
                marks
                    .iter()
                    .rev()
                    .find(|mark| mark.data().name() == name)
                    .map(|mark| mark.data().start_time())
                    .ok_or_else(|| {
                        let message = format!("The \"{name}\" performance mark has not been set");
                        throw_dom_exception(scope, "SyntaxError", &message)
                    })
            }
            // Step 3: Otherwise, if mark is a DOMHighResTimeStamp:
            MarkReference::Timestamp(timestamp) => {
                if !timestamp.is_finite() {
                    return Err(js::error::throw_type_error(
                        scope,
                        c"value is not a finite floating-point value",
                    ));
                }
                // Step 3.1: If mark is negative, throw a TypeError.
                if *timestamp < 0.0 {
                    return Err(invalid_timestamp(
                        js::class::member_site::<PerformanceImpl>(
                            "measure",
                            js::error::MemberKind::Method,
                        ),
                        "A performance timestamp can't be negative",
                        *timestamp,
                    )
                    .throw(scope));
                }
                // Step 3.2: Otherwise, let end time be mark.
                Ok(*timestamp)
            }
        }
    }
}

impl<'s> Performance<'s> {
    /// <https://w3c.github.io/performance-timeline/#dfn-filter-buffer-map-by-name-and-type>: the
    /// entries of the performance entry buffer with `name` and of `entry_type`, each if given,
    /// sorted by `startTime`.
    fn filter_buffer<'r>(
        &self,
        scope: &'r Scope<'_>,
        name: Option<&str>,
        entry_type: Option<&str>,
    ) -> Vec<PerformanceEntry<'r>> {
        let mut entries: Vec<PerformanceEntry<'r>> = {
            let data = self.data();
            let buffers: [(&str, &Vec<Heap<PerformanceEntryImpl>>); 2] =
                [("mark", &data.marks), ("measure", &data.measures)];
            buffers
                .into_iter()
                .filter(|(buffer_type, _)| entry_type.is_none_or(|t| t == *buffer_type))
                .flat_map(|(_, buffer)| buffer.iter().map(|entry| entry.get(scope)))
                .collect()
        };
        entries.retain(|entry| observer::matches(entry, name, None));
        observer::sort_by_start_time(&mut entries);
        entries
    }
}

/// Remove the entries named `name` from `buffer`, or every entry if `name` is `None`.
fn retain_unnamed(
    scope: &Scope<'_>,
    buffer: &mut Vec<Heap<PerformanceEntryImpl>>,
    name: Option<&str>,
) {
    match name {
        Some(name) => buffer.retain(|entry| entry.get(scope).data().name() != name),
        None => buffer.clear(),
    }
}

/// Register the `Performance` class on `global` and install the singleton
/// `performance` instance as a property on it.
pub fn add_to_global<'s>(scope: &'s Scope<'_>, global: Object<'s>) {
    // An embedder that snapshots the initialized runtime (Wizer) captures a time origin belonging
    // to the snapshotting process; it calls `reset_time_origin` on resume to re-establish one.
    init_time_origin();

    Performance::add_to_global(scope, global);
    entry::PerformanceEntry::add_to_global(scope, global);
    entry::PerformanceMark::add_to_global(scope, global);
    entry::PerformanceMeasure::add_to_global(scope, global);
    PerformanceObserver::add_to_global(scope, global);
    observer::PerformanceObserverEntryList::add_to_global(scope, global);

    let install = || -> Result<(), ExnThrown> {
        entry::ENTRY_JSON_PROPERTIES.register(scope)?;
        entry::DETAILED_ENTRY_JSON_PROPERTIES.register(scope)?;
        observer::define_supported_entry_types(scope)?;
        let performance =
            js::class::create_instance_with::<PerformanceImpl>(scope, |_| PerformanceImpl {
                parent: EventTargetImpl::default(),
                marks: Vec::new(),
                measures: Vec::new(),
                observers: Vec::new(),
                observer_task_queued: Weak::new(),
            })?;
        js::class::register_realm_object::<PerformanceObject>(scope, performance.as_object());
        global.set_property(scope, c"performance", performance)
    };
    install().expect("failed to install the performance timeline");
}

#[cfg(test)]
mod tests {
    use core_runtime::{runtime, test_util::eval_with_setup};

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                runtime::register_global_initializer(super::add_to_global);
            },
            code,
        )
    }

    #[test]
    fn performance_to_string_tag() {
        assert_eq!(
            eval("Object.prototype.toString.call(performance)"),
            "[object Performance]"
        );
    }

    /// While the web security restrictions apply, `performance.now()` and the start times of marks
    /// are multiples of 100 microseconds.
    #[test]
    fn timestamps_are_coarsened_under_web_security_restrictions() {
        let result = eval_with_setup(
            || {
                runtime::register_global_initializer(|scope, global| {
                    core_runtime::config::set_web_security_restrictions(true);
                    super::add_to_global(scope, global);
                });
            },
            "(() => {
                const times = [];
                for (let i = 0; i < 1000; i++) {
                    times.push(performance.now(), performance.mark('m').startTime);
                }
                return times.every((t) => Math.abs(t * 10 - Math.round(t * 10)) < 1e-6);
            })()",
        );
        assert_eq!(result, "true");
    }

    #[test]
    fn to_json_returns_object() {
        assert!(eval(
            "(() => {
                const json = performance.toJSON();
                return typeof json === 'object' &&
                       typeof json.timeOrigin === 'number';
            })()"
        )
        .parse::<bool>()
        .unwrap());
    }
}
