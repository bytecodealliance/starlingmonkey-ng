// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Queue-with-sizes support, shared by the default readable/writable stream
//! controllers.
//!
//! <https://streams.spec.whatwg.org/#queue-with-sizes>

use std::collections::VecDeque;

use core_runtime::Traceable;
use js::gc::handle::Heap;
use js::native::Value;

/// A single `value-with-size` entry in a queue-with-sizes.
///
/// <https://streams.spec.whatwg.org/#value-with-size>
#[js::must_root]
#[derive(Traceable)]
pub enum ValueWithSize {
    /// An enqueued chunk.
    Chunk {
        value: Heap<Value>,
        /// The chunk's size, as computed by the stream's size algorithm.
        #[no_trace]
        size: f64,
    },
    /// The writable stream's `close sentinel`, enqueued by
    /// `WritableStreamDefaultControllerClose` with size 0. Never enqueued by the readable
    /// controllers.
    ///
    /// <https://streams.spec.whatwg.org/#writablestreamdefaultcontroller-close-sentinel>
    CloseSentinel,
}

impl ValueWithSize {
    /// The entry's `size`.
    pub fn size(&self) -> f64 {
        match self {
            Self::Chunk { size, .. } => *size,
            Self::CloseSentinel => 0.0,
        }
    }
}

/// A container with `[[queue]]` and `[[queueTotalSize]]` internal slots, as
/// required by the § 8.1 queue-with-sizes operations.
pub trait QueueWithSizes {
    fn queue(&self) -> &VecDeque<ValueWithSize>;
    fn queue_mut(&mut self) -> &mut VecDeque<ValueWithSize>;
    fn queue_total_size(&self) -> f64;
    fn set_queue_total_size(&mut self, size: f64);
}
