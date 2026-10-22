// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `NativePump` behavior that its users' tests can't observe: the size of the batches it writes,
//! what happens to chunks that arrive after [`PumpBatch::check`] rejected one, and when unsent
//! chunks are released.

#![cfg(not(target_arch = "wasm32"))]

use std::cell::RefCell;
use std::rc::Rc;

use core_runtime::config::RuntimeConfig;
use core_runtime::event_loop::{run_microtasks, run_to_completion, with_event_loop, EventLoop};
use core_runtime::runtime::{clear_global_initializers, register_global_initializer, Runtime};
use js::conversion::FromJSVal;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use web_streams::readable::native_pump::{
    append_bytes, NativePump, PumpBatch, PumpEnd, PumpSink, PUMP_BATCH_BYTES,
};
use web_streams::readable::native_read::acquire_native_reader;
use web_streams::readable::ReadableStream;

#[derive(Default)]
struct Log {
    writes: Vec<Vec<u8>>,
    ends: Vec<PumpEnd>,
    /// The contents of each non-empty batch that was dropped instead of written.
    dropped: Vec<Vec<u8>>,
}

struct Bytes(Vec<u8>, Rc<RefCell<Log>>);

impl Drop for Bytes {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            self.1
                .borrow_mut()
                .dropped
                .push(std::mem::take(&mut self.0));
        }
    }
}

impl PumpBatch for Bytes {
    fn append(&mut self, scope: &Scope<'_>, chunk: HandleValue<'_>) -> Result<(), String> {
        let array = js::Uint8Array::from_jsval(scope, chunk, ())
            .map_err(|_| "not a Uint8Array".to_string())?;
        array
            .with_slice(|bytes| append_bytes(&mut self.0, bytes))
            .map_err(|_| "out of memory".to_string())
    }

    /// Reject the byte 255.
    fn check(&mut self, _scope: &Scope<'_>) -> Result<(), String> {
        match self.0.iter().position(|&byte| byte == 255) {
            Some(rejected) => {
                self.0.truncate(rejected);
                Err("rejected 255".to_string())
            }
            None => Ok(()),
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn is_full(&self) -> bool {
        self.0.len() >= PUMP_BATCH_BYTES
    }

    fn take(&mut self) -> Self {
        Bytes(std::mem::take(&mut self.0), Rc::clone(&self.1))
    }
}

/// A sink that records each batch it writes and the end it finishes with. If `gone` is set, its
/// receiver went away, and every write drops its batch.
struct TestSink {
    log: Rc<RefCell<Log>>,
    gone: bool,
}

impl PumpSink for TestSink {
    type Batch = Bytes;

    async fn write(self, mut batch: Bytes) -> Option<Self> {
        if self.gone {
            return None;
        }
        self.log
            .borrow_mut()
            .writes
            .push(std::mem::take(&mut batch.0));
        Some(self)
    }

    fn describe_source_error(_scope: &Scope<'_>, _error: HandleValue<'_>) -> String {
        "the source errored".to_string()
    }

    fn finish(self, _scope: &Scope<'_>, end: &PumpEnd) {
        self.log.borrow_mut().ends.push(end.clone());
    }
}

/// Evaluate `source`, whose completion value is a `ReadableStream`, pump that stream into a
/// [`TestSink`] with `gone`, and run the event loop until it has no work left. A `cancel` function
/// that records its reason in `globalThis.__out` is in scope for `source`. The pump is kept alive
/// until the function returns, so that collecting it doesn't release its batch. Returns the sink's
/// log and `String(globalThis.__out)`.
fn pump(source: &str, gone: bool) -> (Log, String) {
    clear_global_initializers();
    register_global_initializer(web_globals::add_to_global);
    register_global_initializer(web_streams::add_to_global);
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let el = EventLoop::new();
    let log = Rc::new(RefCell::new(Log::default()));
    let code = format!(
        r#"globalThis.__out = "not cancelled";
        function cancel(reason) {{
            globalThis.__out = reason instanceof TypeError
                ? "TypeError: " + reason.message
                : String(reason);
        }}
        {source}"#
    );
    with_event_loop(&el, |_| {
        let value = js::compile::evaluate_with_filename(&scope, &code, "test.js", 1)
            .expect("evaluating the source");
        let stream = ReadableStream::from_jsval(&scope, value, ()).expect("a ReadableStream");
        let reader = acquire_native_reader(&scope, &stream).expect("a reader");
        let sink = TestSink {
            log: Rc::clone(&log),
            gone,
        };
        let batch = Bytes(Vec::new(), Rc::clone(&log));
        let pump = NativePump::create(&scope, reader, sink, batch).expect("a pump");
        scope
            .global()
            .set_property(&scope, c"__pump", pump.as_object())
            .expect("keeping the pump alive");
        pump.start(&scope);
        run_microtasks(&scope);
    });
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a tokio runtime");
    tokio.block_on(async {
        let raw_cx = unsafe { scope.raw_cx_no_gc() };
        unsafe { run_to_completion(raw_cx, &el, tokio::time::sleep).await }
    });
    run_microtasks(&scope);
    let out = js::compile::evaluate_with_filename(&scope, "String(globalThis.__out)", "out.js", 1)
        .expect("reading __out");
    let out = String::from_jsval(&scope, out, ()).expect("a string");
    let log = std::mem::take(&mut *log.borrow_mut());
    (log, out)
}

#[test]
fn a_batch_fills_up_and_the_rest_follows() {
    let (log, out) = pump(
        &format!(
            r#"new ReadableStream({{
            start(c) {{
                for (let i = 0; i < 3; i++) c.enqueue(new Uint8Array({half}));
                c.close();
            }},
            cancel,
        }})"#,
            half = PUMP_BATCH_BYTES / 2,
        ),
        false,
    );
    let lengths: Vec<usize> = log.writes.iter().map(Vec::len).collect();
    assert_eq!(lengths, [PUMP_BATCH_BYTES, PUMP_BATCH_BYTES / 2]);
    assert_eq!(log.ends, [PumpEnd::Closed]);
    assert_eq!(out, "not cancelled");
}

#[test]
fn nothing_after_a_chunk_rejected_by_check_is_written() {
    // The third read waits, so the batch is checked and written while it is pending. Its chunk
    // arrives during the write.
    let (log, out) = pump(
        r#"let pulled = false;
        new ReadableStream({
            start(c) {
                c.enqueue(new Uint8Array([1]));
                c.enqueue(new Uint8Array([255]));
            },
            pull(c) {
                if (pulled) return;
                pulled = true;
                return Promise.resolve().then(() => c.enqueue(new Uint8Array([2])));
            },
            cancel,
        })"#,
        false,
    );
    assert_eq!(log.writes, [vec![1]]);
    assert_eq!(log.ends, [PumpEnd::Failed("rejected 255".to_string())]);
    assert_eq!(out, "TypeError: rejected 255");
}

#[test]
fn a_finished_pump_releases_unsent_chunks() {
    // The second chunk arrives while the first is being written. The write then finds that the
    // receiver went away, which finishes the pump with the second chunk unsent.
    let (log, out) = pump(
        r#"let pulled = false;
        new ReadableStream({
            start(c) { c.enqueue(new Uint8Array([1])); },
            pull(c) {
                if (pulled) return;
                pulled = true;
                return Promise.resolve().then(() => c.enqueue(new Uint8Array([2])));
            },
            cancel,
        })"#,
        true,
    );
    assert!(log.writes.is_empty());
    assert_eq!(log.dropped, [vec![1], vec![2]]);
    assert_eq!(out, "TypeError: the stream's reader was dropped");
}
