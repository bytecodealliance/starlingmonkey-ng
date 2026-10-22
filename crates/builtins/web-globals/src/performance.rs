// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://www.w3.org/TR/hr-time-3/#sec-performance/>
//!
//! Implements [Performance] API from the High Resolution Time specification
//! The time origin is the moment the runtime was initialized with the Performance Global

use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;

use crate::events::event_target::{EventTarget, EventTargetImpl};

#[derive(Clone, Copy)]
struct TimeOriginSnapshot {
    /// The monotonic clock reading, in nanoseconds, when the time origin was captured.
    monotonic_ns: u64,
    /// ECMA-262 timestamp (ms since Unix epoch) for the same moment. Used to compute `timeOrigin`
    /// that is stable against system clock changes.
    epoch_ms: f64,
}

/// Held behind a lock rather than in a `OnceLock` because [`reset_time_origin`] replaces its
/// wall-clock half.
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
const COARSENED_RESOLUTION_NS: u64 = 100_000;

/// The timestamp `ns` nanoseconds after the time origin, in milliseconds. While the web security
/// restrictions apply (see [`core_runtime::config::web_security_restrictions`]), it is coarsened
/// to [`COARSENED_RESOLUTION_NS`], rounding down.
fn relative_ms(ns: u64) -> f64 {
    let ns = if core_runtime::config::web_security_restrictions() {
        ns / COARSENED_RESOLUTION_NS * COARSENED_RESOLUTION_NS
    } else {
        ns
    };
    ns as f64 / 1_000_000.
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

/// The time origin, captured on the first call.
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
}

/// <https://w3c.github.io/hr-time/#dfn-current-high-resolution-time>: the milliseconds since the
/// time origin, coarsened as [`relative_ms`] describes.
pub fn now() -> f64 {
    let origin = time_origin().monotonic_ns;
    relative_ms(platform::clock::monotonic_ns().saturating_sub(origin))
}

/// [`now`] from [`platform::clock::coarse_monotonic_ns`], for timestamps that don't need the
/// precise clock, such as `Event.timeStamp`.
pub fn coarse_now() -> f64 {
    let origin = time_origin().monotonic_ns;
    relative_ms(platform::clock::coarse_monotonic_ns().saturating_sub(origin))
}

#[webidl_methods]
impl Performance {
    /// <https://www.w3.org/TR/hr-time-3/#now-method>
    #[method]
    pub fn now(&self) -> f64 {
        now()
    }

    /// <https://www.w3.org/TR/hr-time-3/#timeorigin-attribute>
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
}

/// Register the `Performance` class on `global` and install the singleton
/// `performance` instance as a property on it.
pub fn add_to_global<'s>(scope: &'s Scope<'_>, global: Object<'s>) {
    // The time origin is captured when the first global is set up, not at the first clock read.
    time_origin();

    Performance::add_to_global(scope, global);

    let performance =
        js::class::create_instance_with::<PerformanceImpl>(scope, |_| PerformanceImpl {
            parent: EventTargetImpl::default(),
        })
        .expect("failed to allocate Performance singleton");

    global
        .set_property(scope, c"performance", performance)
        .expect("failed to define globalThis.performance");
}
