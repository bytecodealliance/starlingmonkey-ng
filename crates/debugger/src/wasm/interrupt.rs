// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! How a debugger outside the instance requests the session's interrupt callback.
//!
//! A wasm instance has no thread that could request the callback while JavaScript runs. A
//! debugger that can stop the instance and write its memory, such as a debugger component run
//! with `wasmtime -Ddebugger=`, requests it instead: it stops the instance, sets the bit
//! [`InterruptRequest::callback_bit`] in the word at [`InterruptRequest::bits`], and resumes the
//! instance. The interpreter then runs the callback at the next backward branch or return from
//! a call, as it does after `JS_RequestInterruptCallback`.
//!
//! A callback that runs while the debug adapter is the interrupted code leaves the messages that
//! have arrived queued, and sets [`InterruptRequest::deferred`]. A debugger that stops the
//! instance again a few milliseconds after a request, and requests the interrupt again if the
//! field is set, has the messages passed on once content code is interrupted.
//!
//! The module exports [`STARLING_DEBUG_INTERRUPT`] as the global `starling_debug_interrupt`,
//! whose value is the address of the [`InterruptRequest`] in the module's memory.

use std::sync::atomic::{AtomicU32, Ordering};

use js::gc::scope::Scope;

/// Where and what a debugger outside the instance writes to request the interrupt callback. The
/// fields are little-endian `u32`s: `bits` at offset 0, `callback_bit` at offset 4 and `deferred`
/// at offset 8.
#[repr(C)]
pub struct InterruptRequest {
    /// The address of the word that holds the context's pending interrupt requests. 0 while no
    /// session is active, when nothing may be written.
    bits: AtomicU32,
    /// The bit of that word that requests the interrupt callback.
    callback_bit: AtomicU32,
    /// 1 after an interrupt callback that left messages queued because the adapter was running,
    /// and 0 after one that passed on the messages that had arrived.
    deferred: AtomicU32,
}

/// The session's [`InterruptRequest`].
#[unsafe(export_name = "starling_debug_interrupt")]
#[used]
pub static STARLING_DEBUG_INTERRUPT: InterruptRequest = InterruptRequest {
    bits: AtomicU32::new(0),
    callback_bit: AtomicU32::new(0),
    deferred: AtomicU32::new(0),
};

/// Publish the interrupt word of the context of `scope`.
pub(crate) fn publish(scope: &Scope<'_>) {
    let bits = js::context::interrupt_bits_address(scope);
    STARLING_DEBUG_INTERRUPT
        .callback_bit
        .store(js::context::interrupt_callback_bit(), Ordering::Relaxed);
    // Addresses are 32 bits wide on wasm32.
    STARLING_DEBUG_INTERRUPT
        .bits
        .store(bits as u32, Ordering::Relaxed);
}

/// Publish whether the last interrupt callback left messages queued because the adapter was
/// running.
pub(crate) fn set_deferred(deferred: bool) {
    STARLING_DEBUG_INTERRUPT
        .deferred
        .store(deferred.into(), Ordering::Relaxed);
}

/// Publish that no interrupt word may be written.
pub(crate) fn unpublish() {
    STARLING_DEBUG_INTERRUPT.bits.store(0, Ordering::Relaxed);
}
