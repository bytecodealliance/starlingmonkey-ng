// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `gc` performance entries of `node:perf_hooks`, from SpiderMonkey's collection callbacks.
//!
//! While an observer observes `gc` entries, the slice and nursery collection callbacks record
//! each collection in [`PENDING`], and request an interrupt. The interrupt callback, which runs
//! where JavaScript can run, starts an unreferenced zero-delay timer that turns the records into
//! entries and queues them. The entries map SpiderMonkey's collections onto Node.js's kinds:
//!
//! - a nursery collection is `NODE_PERFORMANCE_GC_MINOR`,
//! - a slice of an incremental major collection other than its last is
//!   `NODE_PERFORMANCE_GC_INCREMENTAL`,
//! - the last slice of a major collection, and a non-incremental major collection, is
//!   `NODE_PERFORMANCE_GC_MAJOR`.
//!
//! A collection that `gc()` or the embedding requested, with the reason `API`, has the flags
//! `NODE_PERFORMANCE_GC_FLAGS_FORCED`, and every other collection `NODE_PERFORMANCE_GC_FLAGS_NO`.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::gc::{GCDescription, GCNurseryProgress, GCProgress, GCReason};
use js::native::{RawJSContext, Value};
use js::prelude::HandleValue;

use super::{GC_FLAGS_FORCED, GC_FLAGS_NO, GC_INCREMENTAL, GC_MAJOR, GC_MINOR};

/// A collection, as the callbacks record it.
#[derive(Clone, Copy)]
struct Collection {
    /// One of the `NODE_PERFORMANCE_GC_*` kinds.
    kind: u32,
    /// One of the `NODE_PERFORMANCE_GC_FLAGS_*` flags.
    flags: u32,
    /// The [`platform::clock::monotonic_ns`] readings at its start and end.
    start_ns: u64,
    end_ns: u64,
}

js::instance_local! {
    /// The collections recorded since the entries were last queued.
    static PENDING: RefCell<Vec<Collection>> = const { RefCell::new(Vec::new()) };
    /// The start of the major collection slice that is running.
    static SLICE_START_NS: Cell<u64> = const { Cell::new(0) };
    /// The start of the nursery collection that is running.
    static NURSERY_START_NS: Cell<u64> = const { Cell::new(0) };
    /// Whether an interrupt was requested to queue the recorded collections.
    static INTERRUPT_REQUESTED: Cell<bool> = const { Cell::new(false) };
    /// Whether the interrupt callback was added to the context.
    static INTERRUPT_CALLBACK_ADDED: Cell<bool> = const { Cell::new(false) };
}

/// The flags of a collection with `reason`.
fn flags_for(reason: GCReason) -> u32 {
    if reason == GCReason::API {
        GC_FLAGS_FORCED
    } else {
        GC_FLAGS_NO
    }
}

/// Record `collection`, and request the interrupt that queues it unless one was requested.
///
/// # Safety
///
/// `cx` must be the context the collection ran on.
unsafe fn record(cx: *mut RawJSContext, collection: Collection) {
    PENDING.with_borrow_mut(|pending| pending.push(collection));
    if !INTERRUPT_REQUESTED.replace(true) {
        // SAFETY: guaranteed by the caller.
        unsafe { js::context::request_interrupt_callback_raw(cx) };
    }
}

/// The slice callback: record each slice of a major collection, and mark the last slice of each
/// collection as the major collection.
unsafe extern "C" fn on_slice(
    cx: *mut RawJSContext,
    progress: GCProgress,
    desc: *const GCDescription,
) {
    let now = platform::clock::monotonic_ns();
    match progress {
        GCProgress::GC_SLICE_BEGIN => SLICE_START_NS.set(now),
        GCProgress::GC_SLICE_END => {
            // SAFETY: the engine passes a valid description for the duration of the call.
            let reason = unsafe { (*desc).reason_ };
            let collection = Collection {
                kind: GC_INCREMENTAL,
                flags: flags_for(reason),
                start_ns: SLICE_START_NS.get(),
                end_ns: now,
            };
            // SAFETY: the engine passes the context the collection runs on.
            unsafe { record(cx, collection) };
        }
        GCProgress::GC_CYCLE_END => PENDING.with_borrow_mut(|pending| {
            if let Some(last) = pending
                .last_mut()
                .filter(|last| last.kind == GC_INCREMENTAL)
            {
                last.kind = GC_MAJOR;
            }
        }),
        GCProgress::GC_CYCLE_BEGIN => {}
    }
}

/// The nursery collection callback: record each nursery collection.
unsafe extern "C" fn on_nursery(
    cx: *mut RawJSContext,
    progress: GCNurseryProgress,
    reason: GCReason,
    _data: *mut std::os::raw::c_void,
) {
    let now = platform::clock::monotonic_ns();
    match progress {
        GCNurseryProgress::GC_NURSERY_COLLECTION_START => NURSERY_START_NS.set(now),
        GCNurseryProgress::GC_NURSERY_COLLECTION_END => {
            let collection = Collection {
                kind: GC_MINOR,
                flags: flags_for(reason),
                start_ns: NURSERY_START_NS.get(),
                end_ns: now,
            };
            // SAFETY: the engine passes the context the collection runs on.
            unsafe { record(cx, collection) };
        }
    }
}

/// The interrupt callback: start the unreferenced timer that queues the recorded collections.
/// Returns `true`, which lets the interrupted JavaScript continue.
unsafe extern "C" fn on_interrupt(cx: *mut RawJSContext) -> bool {
    if !INTERRUPT_REQUESTED.get() {
        return true;
    }
    // SAFETY: interrupt callbacks run with the interrupted JavaScript's realm entered.
    let scope = unsafe { js::gc::scope::RootScope::from_current_realm(cx) };
    // A later collection requests the interrupt again if the timer didn't start.
    if start_delivery(&scope).is_err() {
        js::exception::clear(&scope);
    }
    INTERRUPT_REQUESTED.set(false);
    true
}

/// Start an unreferenced zero-delay timer that queues the recorded collections. Returns whether
/// the timer started, which it can't without an event loop.
fn start_delivery(scope: &Scope<'_>) -> Result<bool, ExnThrown> {
    let deliver =
        js::class::get_or_init_shared_function(scope, deliver as *const () as usize, |scope| {
            js::Function::new_callback(scope, c"", 0, deliver, ())
        })?;
    let Some(id) = core_runtime::event_loop::timer::start_timer(
        scope,
        deliver.as_callable(),
        HandleValue::undefined(),
        &[],
        Duration::ZERO,
        false,
        None,
    ) else {
        return Ok(false);
    };
    core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop.set_js_timer_ref(id, false)
    });
    Ok(true)
}

/// The timer callback: queue a `gc` entry for each recorded collection.
fn deliver(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let collections = PENDING.with_borrow_mut(std::mem::take);
    for collection in collections {
        let start = web_globals::performance::ms_since_origin(collection.start_ns);
        let duration = collection.end_ns.saturating_sub(collection.start_ns) as f64 / 1e6;
        super::queue_gc_entry(scope, start, duration, collection.kind, collection.flags)?;
    }
    Ok(js::value::undefined())
}

/// Install the collection callbacks, and the interrupt callback if this context has none.
pub(crate) fn install(scope: &Scope<'_>) {
    if !INTERRUPT_CALLBACK_ADDED.replace(true) {
        // SAFETY: the callback is a valid function for the life of the process.
        let added = unsafe { js::context::add_interrupt_callback(scope, Some(on_interrupt)) };
        if !added {
            INTERRUPT_CALLBACK_ADDED.set(false);
        }
    }
    // SAFETY: both callbacks are valid functions for the life of the process, and neither
    // reads `data`.
    unsafe {
        js::gc::set_gc_slice_callback(scope, Some(on_slice));
        js::gc::add_gc_nursery_collection_callback(scope, Some(on_nursery), std::ptr::null_mut());
    }
}

/// Remove the collection callbacks, and drop the collections recorded but not yet queued.
pub(crate) fn uninstall(scope: &Scope<'_>) {
    // SAFETY: the arguments match the registration in `install`.
    unsafe {
        js::gc::set_gc_slice_callback(scope, None);
        js::gc::remove_gc_nursery_collection_callback(
            scope,
            Some(on_nursery),
            std::ptr::null_mut(),
        );
    }
    PENDING.with_borrow_mut(Vec::clear);
}
