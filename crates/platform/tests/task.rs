// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The WASIp2 executor: what [`block_on`] waits for, and futures that outlive the call they were
//! created in, as a `fetch` body kept in a global does across export calls.

#![cfg(target_env = "p2")]

use platform::task::{block_on, spawn, spawn_background, wait_for};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

#[test]
fn block_on_returns_the_futures_output() {
    assert_eq!(block_on(async { 7 }), 7);
}

#[test]
fn block_on_waits_for_spawned_tasks() {
    let done = Rc::new(Cell::new(false));
    let task_done = Rc::clone(&done);
    block_on(async move {
        spawn(async move {
            platform::clock::sleep(Duration::from_millis(5)).await;
            task_done.set(true);
        });
    });
    assert!(
        done.get(),
        "`block_on` returned before the spawned task finished"
    );
}

#[test]
fn background_tasks_progress_during_later_calls() {
    let ticks = Rc::new(Cell::new(0));
    let task_ticks = Rc::clone(&ticks);
    block_on(async move {
        spawn_background(async move {
            for _ in 0..3 {
                platform::clock::sleep(Duration::from_millis(1)).await;
                task_ticks.set(task_ticks.get() + 1);
            }
        });
    });
    assert_eq!(ticks.get(), 0, "`block_on` waited for the background task");
    while ticks.get() < 3 {
        block_on(platform::clock::sleep(Duration::from_millis(2)));
    }
}

#[test]
fn a_wait_started_in_one_call_finishes_in_a_later_one() {
    let mut wait = Box::pin(wait_for(
        wasip2::clocks::monotonic_clock::subscribe_duration(5_000_000),
    ));
    // Polled, so it registers with the reactor, and abandoned when the shorter timer fires.
    let timer_won = block_on(async {
        futures_lite::future::or(
            async {
                (&mut wait).await;
                false
            },
            async {
                platform::clock::sleep(Duration::from_millis(1)).await;
                true
            },
        )
        .await
    });
    assert!(timer_won);
    block_on(wait);
}
