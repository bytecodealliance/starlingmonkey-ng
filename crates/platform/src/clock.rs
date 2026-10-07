// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The monotonic clock, with offsets applied where needed, such as when resuming from a Wizer
//! snapshot.
//!
//! On wasm the monotonic clock is scoped to the instance: it starts again at zero when an instance
//! resumes from a snapshot, while readings taken before the snapshot are still in the heap, in the
//! instance's future. [`record_snapshot_reading`] captures the clock as the snapshot is made and
//! [`resume_from_snapshot`] adds that reading to every reading afterwards, so a resumed instance
//! continues the timeline the snapshot ended on instead of restarting it.
//!
//! SpiderMonkey keeps the same kind of state (GC phase timestamps, the last collection's end) and
//! reads its own copy of the same host clock, so an embedder passes the value
//! [`resume_from_snapshot`] returns on to the engine as well.

use std::sync::atomic::{AtomicU64, Ordering};

/// Added to every reading returned by [`monotonic_ns`].
static OFFSET_NS: AtomicU64 = AtomicU64::new(0);

/// The reading [`record_snapshot_reading`] took, carried into the snapshot for
/// [`resume_from_snapshot`] to apply. Zero for an instance that was never wizened.
static SNAPSHOT_NS: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "wasm32")]
fn host_monotonic_ns() -> u64 {
    wasip3::clocks::monotonic_clock::now()
}

/// WASI has no cheaper clock, so the coarse reading is the precise one.
#[cfg(target_arch = "wasm32")]
fn host_coarse_monotonic_ns() -> u64 {
    host_monotonic_ns()
}

/// The precise monotonic clock, and a coarse one on the same timeline that is cheaper to read.
/// Linux updates `CLOCK_MONOTONIC_COARSE` every scheduler tick. macOS updates
/// `CLOCK_UPTIME_RAW_APPROX` on context switches.
#[cfg(target_os = "linux")]
const CLOCKS: (libc::clockid_t, libc::clockid_t) =
    (libc::CLOCK_MONOTONIC, libc::CLOCK_MONOTONIC_COARSE);
#[cfg(target_os = "macos")]
const CLOCKS: (libc::clockid_t, libc::clockid_t) =
    (libc::CLOCK_UPTIME_RAW, libc::CLOCK_UPTIME_RAW_APPROX);
#[cfg(all(
    not(target_arch = "wasm32"),
    not(target_os = "linux"),
    not(target_os = "macos")
))]
const CLOCKS: (libc::clockid_t, libc::clockid_t) = (libc::CLOCK_MONOTONIC, libc::CLOCK_MONOTONIC);

#[cfg(not(target_arch = "wasm32"))]
fn read_clock(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid `timespec` to write to, and `clock` is a supported clock id.
    let result = unsafe { libc::clock_gettime(clock, &mut ts) };
    debug_assert_eq!(result, 0, "clock_gettime failed");
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(target_arch = "wasm32"))]
fn host_monotonic_ns() -> u64 {
    read_clock(CLOCKS.0)
}

#[cfg(not(target_arch = "wasm32"))]
fn host_coarse_monotonic_ns() -> u64 {
    read_clock(CLOCKS.1)
}

/// The current monotonic clock reading, in nanoseconds, continuous across a snapshot.
///
/// The origin is unspecified: only differences between readings are meaningful.
pub fn monotonic_ns() -> u64 {
    OFFSET_NS.load(Ordering::Relaxed) + host_monotonic_ns()
}

/// A [`monotonic_ns`] reading from a clock that is cheaper to read but less precise, both
/// depending on the platform. It can lag a [`monotonic_ns`] reading taken just before it.
pub fn coarse_monotonic_ns() -> u64 {
    OFFSET_NS.load(Ordering::Relaxed) + host_coarse_monotonic_ns()
}

/// Record where the clock stood as a snapshot was made.
///
/// Must be the last thing an instance being wizened does, since a reading taken afterwards
/// would sit past the recorded one and so still be in a resumed instance's future.
pub fn record_snapshot_reading() {
    SNAPSHOT_NS.store(host_monotonic_ns(), Ordering::Relaxed);
}

/// Advance the clock past everything the snapshot holds, and return the nanoseconds added.
///
/// Called on the first work a resumed instance does. The caller passes the return value to every
/// other clock the resumed instance has, above all SpiderMonkey's. Returns zero, and does nothing,
/// for an instance that did not resume from a snapshot, and for every call after the first.
pub fn resume_from_snapshot() -> u64 {
    let nanoseconds = SNAPSHOT_NS.swap(0, Ordering::Relaxed);
    OFFSET_NS.fetch_add(nanoseconds, Ordering::Relaxed);
    nanoseconds
}

/// Wait for `duration` on the host's monotonic clock.
#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: std::time::Duration) {
    let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
    wasip3::clocks::monotonic_clock::wait_for(nanos).await;
}

/// Wait for `duration` with tokio's timer, which must be polled inside a tokio runtime.
#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: std::time::Duration) {
    tokio::time::sleep(duration).await;
}

/// A monotonic clock reading, for deadlines that outlive a Wizer snapshot.
///
/// `std::time::Instant` reads the host clock directly, so a deadline taken before a snapshot sits
/// in the resumed instance's future by however long the wizened instance ran. This reads
/// [`monotonic_ns`] instead, whose offset makes a resumed instance continue the timeline the
/// snapshot ended on. A deadline that was `d` ahead of the reading [`record_snapshot_reading`]
/// took is `d` ahead of the origin of the resumed instance's host clock, and one that had passed
/// then has passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl Instant {
    /// The current reading.
    pub fn now() -> Self {
        Self(monotonic_ns())
    }

    /// The time from `earlier` to this reading, or zero if `earlier` is the later of the two.
    pub fn saturating_duration_since(&self, earlier: Self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

impl std::ops::Add<std::time::Duration> for Instant {
    type Output = Self;

    /// Saturates at the largest representable reading, which no clock reaches.
    fn add(self, duration: std::time::Duration) -> Self {
        Self(
            self.0
                .saturating_add(duration.as_nanos().min(u64::MAX as u128) as u64),
        )
    }
}
