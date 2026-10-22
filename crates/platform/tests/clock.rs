// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The monotonic clock's snapshot offset.
//!
//! These run natively, where the clock never restarts, so they cover the offset arithmetic rather
//! than a real resume: [`record_snapshot_reading`] and [`resume_from_snapshot`] move the clock
//! forward by the recorded reading exactly once.

use platform::clock::{monotonic_ns, record_snapshot_reading, resume_from_snapshot, Instant};
use std::time::Duration;

#[test]
fn resuming_advances_the_clock_by_the_recorded_reading() {
    let before = monotonic_ns();
    record_snapshot_reading();
    let added = resume_from_snapshot();
    assert!(
        added > 0,
        "the recorded reading should be past the clock's origin"
    );
    assert!(
        monotonic_ns() >= before + added,
        "readings after the resume should sit past the snapshot's"
    );

    // The reading is consumed by the first resume, so a second resume does nothing. Every later
    // call in a resumed instance takes this path.
    assert_eq!(resume_from_snapshot(), 0);
}

#[test]
fn instant_arithmetic_saturates() {
    let now = Instant::now();
    assert_eq!(
        now.saturating_duration_since(now + Duration::from_secs(1)),
        Duration::ZERO,
        "a later reading is not in the past"
    );
    assert_eq!(
        (now + Duration::from_millis(250)).saturating_duration_since(now),
        Duration::from_millis(250)
    );
    assert!(
        now + Duration::MAX > now,
        "adding saturates rather than wrapping"
    );
}
