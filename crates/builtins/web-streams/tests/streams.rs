// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Stream behavior that the WPT `streams` suites don't check: the Rust-side
//! `ReadableStream::from_bytes` and `ReadableByteStreamController::enqueue_bytes`
//! APIs, WebIDL conversion order, chunk adoption by async iteration, isolation
//! of internal state from `Object.prototype`, and regressions for specific bugs.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::{clear_global_initializers, register_global_initializer, Runtime};
use core_runtime::test_util::{eval_and_read_out, eval_out_with_setup};
use js::conversion::FromJSVal;
use web_streams::readable::{ReadableByteStreamController, ReadableStream};

fn add_globals() {
    clear_global_initializers();
    register_global_initializer(web_globals::add_to_global);
    register_global_initializer(web_streams::add_to_global);
}

/// Evaluate `code`, drain microtasks, and return `String(globalThis.__out)`.
fn run(code: &str) -> String {
    eval_out_with_setup(add_globals, code)
}

/// Create a stream from `bytes`, expose it as `globalThis.__stream`, then evaluate
/// `code`, drain microtasks, and return `String(globalThis.__out)`.
fn run_with_stream(bytes: &[u8], code: &str) -> String {
    add_globals();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let global = scope.global();
    let stream = ReadableStream::from_bytes(&scope, bytes).expect("create stream from bytes");
    global
        .set_property(&scope, c"__stream", stream)
        .expect("expose __stream");
    eval_and_read_out(&scope, code)
}

/// A stream from `ReadableStream::from_bytes` delivers the bytes as a single
/// `Uint8Array` chunk, then closes.
#[test]
fn from_bytes_round_trips_through_js_reader() {
    let out = run_with_stream(
        b"hello world",
        r#"
        globalThis.__out = "pending";
        (async () => {
            const reader = globalThis.__stream.getReader();
            const bytes = [];
            let chunkCount = 0;
            for (;;) {
                const { value, done } = await reader.read();
                if (done) break;
                chunkCount++;
                if (!(value instanceof Uint8Array)) {
                    globalThis.__out = "chunk not Uint8Array: " + value;
                    return;
                }
                bytes.push(...value);
            }
            globalThis.__out = chunkCount + ":" + bytes.join(",");
        })();
        "#,
    );
    assert_eq!(out, "1:104,101,108,108,111,32,119,111,114,108,100");
}

/// A stream from `ReadableStream::from_bytes` with no bytes is closed without a chunk.
#[test]
fn from_bytes_empty_stream_closes_immediately() {
    let out = run_with_stream(
        b"",
        r#"
        globalThis.__out = "pending";
        (async () => {
            const reader = globalThis.__stream.getReader();
            const { value, done } = await reader.read();
            globalThis.__out = `done=${done},value=${value}`;
        })();
        "#,
    );
    assert_eq!(out, "done=true,value=undefined");
}

/// A non-callable callback member throws a `TypeError` at dictionary conversion,
/// before each constructor's own `RangeError` checks.
#[test]
fn non_callable_callback_throws_before_constructor_checks() {
    let out = run(r#"
        function errorName(f) {
            try { f(); return "no-throw"; } catch (e) { return e.constructor.name; }
        }
        globalThis.__out = [
            errorName(() => new ReadableStream({ pull: {}, type: "bytes" }, { size: () => 1 })),
            errorName(() => new WritableStream({ write: {}, type: "bytes" })),
            errorName(() => new TransformStream({ transform: {}, readableType: "bytes" })),
        ].join(",");
        "#);
    assert_eq!(out, "TypeError,TypeError,TypeError");
}

/// For a byte stream, a callable `size` is rejected with a `RangeError`, but a
/// non-callable `size` fails the `QueuingStrategySize` conversion first with a
/// `TypeError`.
#[test]
fn byte_stream_size_function_error_ordering() {
    let out = run(r#"
        let callableErr = "none", nonCallableErr = "none";
        try { new ReadableStream({ type: "bytes" }, { size() { return 1; } }); }
        catch (e) { callableErr = e.constructor.name; }
        try { new ReadableStream({ type: "bytes" }, { size: {} }); }
        catch (e) { nonCallableErr = e.constructor.name; }
        globalThis.__out = `${callableErr},${nonCallableErr}`;
        "#);
    assert_eq!(out, "RangeError,TypeError");
}

/// `autoAllocateChunkSize` is `[EnforceRange] unsigned long long`: a negative
/// value throws a `TypeError` rather than wrapping to a huge positive value.
#[test]
fn auto_allocate_chunk_size_enforce_range() {
    let out = run(r#"
        let r;
        try { new ReadableStream({ type: "bytes", autoAllocateChunkSize: -1 }); r = "no-throw"; }
        catch (e) { r = e.constructor.name; }
        globalThis.__out = r;
        "#);
    assert_eq!(out, "TypeError");
}

/// A byte controller's `enqueue` rejects a chunk that is not an `ArrayBufferView`.
#[test]
fn byte_enqueue_of_non_view_throws_type_error() {
    let out = run(r#"
        let threw = false;
        new ReadableStream({
            type: "bytes",
            start(c) {
                try { c.enqueue([1]); } catch (e) { threw = e instanceof TypeError; }
            },
        });
        globalThis.__out = String(threw);
        "#);
    assert_eq!(out, "true");
}

/// Bound functions are accepted as WebIDL callback-type members, and are invoked
/// with their bound `this`.
#[test]
fn bound_function_callbacks() {
    let out = run(r#"
        globalThis.__out = "pending";
        let sizeCalledWith = null;
        // `start` enqueues before any reader exists, so the chunk goes through the
        // queue and the strategy `size` is actually invoked (an enqueue that
        // satisfies a pending read bypasses `size`).
        const src = { first: 7, start(c) { c.enqueue(this.first); } };
        const strat = {
            factor: 3,
            size(chunk) { sizeCalledWith = chunk; return chunk * this.factor; },
        };
        const rs = new ReadableStream(
            { start: src.start.bind(src) },
            { size: strat.size.bind(strat), highWaterMark: 100 },
        );
        rs.getReader().read().then(r => {
            globalThis.__out = `${r.value},${r.done},size=${sizeCalledWith}`;
        });
        "#);
    assert_eq!(out, "7,false,size=7");
}

/// A `read()` result has own `value`/`done` data properties, so an accessor on
/// `Object.prototype` can neither observe nor replace them.
#[test]
fn read_result_has_own_properties_immune_to_proto_accessor() {
    let out = run(r#"
        globalThis.__out = "pending";
        let leaked = "none";
        Object.defineProperty(Object.prototype, "value", {
            configurable: true,
            get() { return "HIJACKED"; },
            set(v) { leaked = v; },
        });
        const rs = new ReadableStream({ start(c) { c.enqueue(42); } });
        rs.getReader().read().then(r => {
            globalThis.__out =
                `own=${Object.hasOwn(r, "value")},value=${r.value},leaked=${leaked}`;
        });
        "#);
    assert_eq!(out, "own=true,value=42,leaked=none");
}

/// Numeric accessors on `Object.prototype` don't reach `tee()`'s internal state.
#[test]
fn tee_internal_state_immune_to_proto_numeric_accessor() {
    // `drain` concatenates into a string rather than an array: the polluted
    // numeric accessors below would also intercept ordinary `array[0] = …` writes
    // in author code.
    let out = run(r#"
        async function drain(stream) {
            const reader = stream.getReader();
            let s = "";
            for (;;) {
                const { value, done } = await reader.read();
                if (done) break;
                s += value;
            }
            return s;
        }
        globalThis.__out = "pending";
        for (const k of ["0", "1"]) {
            Object.defineProperty(Object.prototype, k, {
                configurable: true,
                get() { return "HIJACKED"; },
                set(_v) {},
            });
        }
        const rs = new ReadableStream({ start(c) { c.enqueue("a"); c.enqueue("b"); c.close(); } });
        const [b1, b2] = rs.tee();
        Promise.all([drain(b1), drain(b2)]).then(
            ([s1, s2]) => { globalThis.__out = s1 + "|" + s2; },
            e => { globalThis.__out = "rejected:" + e; },
        );
        "#);
    assert_eq!(out, "ab|ab");
}

/// A sync iterable that yields a rejected promise: the async-from-sync wrapper
/// closes the sync iterator (running its `finally`) before the stream errors.
#[test]
fn from_sync_rejected_value_closes_iterator() {
    let out = run(r#"
        globalThis.__out = "pending";
        let cleaned = false;
        const it = (function* () {
            try { yield Promise.reject(new TypeError("boom")); }
            finally { cleaned = true; }
        })();
        ReadableStream.from(it).getReader().read().then(
            () => { globalThis.__out = "resolved"; },
            e => { globalThis.__out = `rejected:${e.constructor.name}:${e.message}|cleaned:${cleaned}`; },
        );
        "#);
    assert_eq!(out, "rejected:TypeError:boom|cleaned:true");
}

/// Async iteration adopts a promise chunk: `for await` yields `5`, not the
/// promise.
#[test]
fn for_await_adopts_thenable_chunk() {
    let out = run(r#"
        globalThis.__out = "pending";
        const rs = new ReadableStream({ start(c) { c.enqueue(Promise.resolve(5)); c.close(); } });
        (async () => {
            const vals = [];
            for await (const x of rs) vals.push(x);
            globalThis.__out = vals.join(",") + "|" + typeof vals[0];
        })();
        "#);
    assert_eq!(out, "5|number");
}

/// A rejected promise chunk rejects `next()` instead of being yielded.
#[test]
fn for_await_rejected_thenable_chunk_throws() {
    let out = run(r#"
        globalThis.__out = "pending";
        const rs = new ReadableStream({ start(c) { c.enqueue(Promise.reject(new TypeError("boom"))); c.close(); } });
        (async () => {
            try {
                for await (const x of rs) { globalThis.__out = "yielded:" + x; return; }
                globalThis.__out = "done-no-throw";
            } catch (e) { globalThis.__out = "threw:" + e.constructor.name + ":" + e.message; }
        })();
        "#);
    assert_eq!(out, "threw:TypeError:boom");
}

/// The default async iterator has no interface object on the global, and its
/// class string is `"ReadableStream AsyncIterator"` (with a space), per WebIDL
/// §3.7.10.
#[test]
fn async_iterator_has_no_global_and_spaced_string_tag() {
    let out = run(r#"
        const leaked = "ReadableStreamAsyncIterator" in globalThis;
        const tag = Object.prototype.toString.call(new ReadableStream().values());
        globalThis.__out = `leaked=${leaked},tag=${tag}`;
        "#);
    assert_eq!(
        out,
        "leaked=false,tag=[object ReadableStream AsyncIterator]"
    );
}

/// `ReadableStream.prototype[Symbol.asyncIterator]` is the `values` function.
#[test]
fn async_iterator_method_is_values() {
    let out = run(r#"
        globalThis.__out = String(
            ReadableStream.prototype[Symbol.asyncIterator] === ReadableStream.prototype.values);
        "#);
    assert_eq!(out, "true");
}

/// `next()` clears the ongoing promise when the previous call settles, so a call
/// made after that runs the next steps directly instead of chaining behind the
/// settled promise. Both calls then settle after the same number of ticks.
#[test]
fn settled_next_does_not_delay_the_following_call() {
    let out = run(r#"
        globalThis.__out = "pending";
        const rs = new ReadableStream({ start(c) { c.enqueue(1); c.enqueue(2); } });
        const it = rs.values();
        function ticksUntil(p) {
            let done = false;
            p.then(() => { done = true; });
            return new Promise(resolve => {
                let n = 0;
                (function spin() {
                    if (done) { resolve(n); } else { n++; Promise.resolve().then(spin); }
                })();
            });
        }
        (async () => {
            const first = await ticksUntil(it.next());
            const second = await ticksUntil(it.next());
            globalThis.__out = `${first},${second}`;
        })();
    "#);
    let (first, second) = out.split_once(',').expect("two tick counts");
    assert_eq!(first, second, "ticks: {out}");
}

/// pipeTo's abort handling is a DOM abort algorithm, not an `abort` event
/// listener: an earlier listener that calls `stopImmediatePropagation()` doesn't
/// suppress the pipe's shutdown.
#[test]
fn abort_signal_not_suppressed_by_stop_immediate_propagation() {
    let out = run(r#"
        globalThis.__out = "pending";
        let aborted = false;
        const ws = new WritableStream({ abort() { aborted = true; } });
        const ac = new AbortController();
        ac.signal.addEventListener("abort", (e) => { e.stopImmediatePropagation(); });
        const rs = new ReadableStream({ start(c) { c.enqueue("a"); } });
        const p = rs.pipeTo(ws, { signal: ac.signal });
        ac.abort(new Error("stop"));
        p.then(() => { globalThis.__out = "fulfilled"; },
               e => { globalThis.__out = "rejected:" + e.message + "|aborted:" + aborted; });
        "#);
    assert_eq!(out, "rejected:stop|aborted:true");
}

/// Aborting a pipe runs the destination controller's `signal` listeners
/// synchronously. A listener that enqueues into the source re-enters the pipe's
/// pending read request, which must not find the pipe state borrowed.
#[test]
fn abort_listener_enqueueing_into_source_does_not_panic() {
    let out = run(r#"
        globalThis.__out = "pending";
        let sourceController;
        const rs = new ReadableStream({ start(c) { sourceController = c; } });
        const ws = new WritableStream({
            start(c) {
                c.signal.addEventListener("abort", () => sourceController.enqueue("late"));
            },
        });
        const ac = new AbortController();
        const p = rs.pipeTo(ws, { signal: ac.signal });
        Promise.resolve().then(() => ac.abort(new Error("stop")));
        p.then(() => { globalThis.__out = "fulfilled"; },
               e => { globalThis.__out = "rejected:" + e.message; });
        "#);
    assert_eq!(out, "rejected:stop");
}

/// Three pending BYOB reads, then one `enqueue` that fills the first: filling the
/// head descriptor works while two more descriptors are still queued.
#[test]
fn byob_multiple_pending_reads_then_enqueue() {
    let out = run(r#"
        globalThis.__out = "pending";
        let controller;
        const rs = new ReadableStream({
            type: "bytes",
            start(c) { controller = c; },
        });
        const reader = rs.getReader({ mode: "byob" });
        const p1 = reader.read(new Uint8Array(10));
        reader.read(new Uint8Array(10));
        reader.read(new Uint8Array(10));
        controller.enqueue(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));
        p1.then(r => {
            globalThis.__out = `${r.value.constructor.name},len=${r.value.length},${r.value[0]}-${r.value[9]}`;
        });
        "#);
    assert_eq!(out, "Uint8Array,len=10,1-10");
}

/// The `min` option of a BYOB read into a `Float16Array` is checked against the
/// view's element length, so `min: 3` on a 2-element view rejects.
#[test]
fn byob_read_float16_view_kind() {
    let out = run(r#"
        globalThis.__out = "pending";
        if (typeof Float16Array === "undefined") {
            globalThis.__out = "no-float16";
        } else {
            const rs = new ReadableStream({
                type: "bytes",
                pull(c) {
                    const v = c.byobRequest.view;
                    c.byobRequest.respond(v.byteLength);
                },
            });
            rs.getReader({ mode: "byob" }).read(new Float16Array(2), { min: 3 }).then(
                r => { globalThis.__out = `resolved:${r.value.constructor.name}`; },
                e => { globalThis.__out = `rejected:${e.constructor.name}`; },
            );
        }
        "#);
    assert_eq!(out, "rejected:RangeError");
}

/// A BYOB `read(view, { min })` on a `Uint16Array` is fulfilled once `min` elements are
/// available. Here the source supplies exactly the requested bytes.
#[test]
fn byob_read_uint16_view_with_min() {
    let out = run(r#"
        globalThis.__out = "pending";
        const rs = new ReadableStream({
            type: "bytes",
            pull(c) {
                const v = c.byobRequest.view;
                for (let i = 0; i < v.byteLength; i++) v[i] = i + 1;
                c.byobRequest.respond(v.byteLength);
            },
        });
        rs.getReader({ mode: "byob" }).read(new Uint16Array(2), { min: 2 }).then(r => {
            globalThis.__out = `${r.value.constructor.name},len=${r.value.length},${r.value[0]}`;
        });
        "#);
    // Bytes 1,2,3,4 little-endian -> u16 [0x0201, 0x0403] = [513, 1027].
    assert_eq!(out, "Uint16Array,len=2,513");
}

/// A teed byte stream whose branch has a partially filled `Int32Array` read when
/// the source closes: the branch's close throws "insufficient bytes", which the
/// spec marks infallible. Both branches then reject with a `TypeError` instead of
/// the runtime aborting.
#[test]
fn byte_tee_branch_partial_fill_close_rejects_not_aborts() {
    let out = run(r#"
        globalThis.__out = "pending";
        let step = 0;
        const rs = new ReadableStream({
            type: "bytes",
            pull(c) {
                if (step === 0) {
                    step = 1;
                    const v = c.byobRequest.view;
                    v[0] = 1; v[1] = 2; v[2] = 3;
                    c.byobRequest.respond(3); // 3 of 4 bytes -> branch partial fill
                } else {
                    c.close();
                    c.byobRequest.respond(0);
                }
            },
        });
        const [b1, b2] = rs.tee();
        // Branch 2 receives the 3-byte clone, then its next read rejects. If the
        // runtime aborted at branch 1's close, branch 2 would never close and this
        // read would never settle.
        async function drainB2() {
            const r = b2.getReader();
            let n = 0;
            try {
                for (;;) {
                    const { done } = await r.read();
                    if (done) return "b2:done@" + n;
                    n++;
                }
            } catch (e) { return "b2:" + e.constructor.name + "@" + n; }
        }
        const r1 = b1.getReader({ mode: "byob" }).read(new Int32Array(1)).then(
            () => "b1:resolved", e => "b1:" + e.constructor.name);
        Promise.all([r1, drainB2()]).then(([a, b]) => { globalThis.__out = a + "," + b; });
        "#);
    assert_eq!(out, "b1:TypeError,b2:TypeError@1");
}

/// `respond(bytesWritten)` is `[EnforceRange] unsigned long long`: a negative
/// argument throws a `TypeError` at conversion, not a `RangeError` from the
/// length checks.
#[test]
fn respond_enforce_range_rejects_negative() {
    let out = run(r#"
        globalThis.__out = "pending";
        const rs = new ReadableStream({
            type: "bytes",
            pull(c) {
                try { c.byobRequest.respond(-1); globalThis.__out = "no-throw"; }
                catch (e) { globalThis.__out = "respond:" + e.constructor.name; }
            },
        });
        rs.getReader({ mode: "byob" }).read(new Uint8Array(4));
        "#);
    assert_eq!(out, "respond:TypeError");
}

/// `read(view, options)` converts `view` before `options`, so an invalid view
/// rejects without running a getter on `options.min`.
#[test]
fn byob_read_validates_view_before_options() {
    let out = run(r#"
        globalThis.__out = "pending";
        let getterRan = false;
        const reader = new ReadableStream({ type: "bytes" }).getReader({ mode: "byob" });
        reader.read(42, { get min() { getterRan = true; throw new Error("boom"); } }).then(
            () => { globalThis.__out = "resolved"; },
            e => { globalThis.__out = `rejected:${e.constructor.name}|getterRan:${getterRan}`; },
        );
        "#);
    assert_eq!(out, "rejected:TypeError|getterRan:false");
}

/// A promise-returning operation (`cancel`) and a promise-typed getter (`closed`) called with a
/// wrong `this` return a promise rejected with a `TypeError` instead of throwing, per WebIDL
/// §3.7.7. These are the macros' two brand-check rejection branches.
#[test]
fn promise_members_reject_a_wrong_this() {
    let out = run(r#"
        globalThis.__out = "pending";
        const closed = Object.getOwnPropertyDescriptor(
            ReadableStreamDefaultReader.prototype, "closed").get;
        const outcome = (call) => {
            let p;
            try { p = call(); } catch (e) { return Promise.resolve("threw"); }
            return p.then(() => "resolved", e => `rejected:${e.constructor.name}`);
        };
        Promise.all([
            outcome(() => ReadableStream.prototype.cancel.call(undefined)),
            outcome(() => closed.call(undefined)),
        ]).then(results => { globalThis.__out = results.join("|"); });
        "#);
    assert_eq!(out, "rejected:TypeError|rejected:TypeError");
}

/// `enqueue_bytes` hands its `Vec`'s allocation to the reader: the chunk read from the stream holds
/// the bytes at the address they had in the `Vec`.
#[test]
fn enqueue_bytes_hands_its_buffer_to_the_reader() {
    add_globals();
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();
    let controller = js::compile::evaluate_with_filename(
        &scope,
        r#"
        let controller;
        globalThis.stream = new ReadableStream({ type: "bytes", start(c) { controller = c; } });
        controller
        "#,
        "test.js",
        1,
    )
    .expect("creating the stream");
    let controller =
        ReadableByteStreamController::from_jsval(&scope, controller, ()).expect("a controller");
    let bytes = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    let address = bytes.as_ptr() as usize;
    controller
        .enqueue_bytes(&scope, bytes)
        .expect("enqueueing the bytes");
    let out = eval_and_read_out(
        &scope,
        r#"
        globalThis.__out = "pending";
        stream.getReader().read().then(({ value }) => {
            globalThis.chunk = value;
            globalThis.__out = value.join(",");
        });
        "#,
    );
    assert_eq!(out, "1,2,3,4,5,6,7,8");
    let chunk = js::compile::evaluate_with_filename(&scope, "chunk", "chunk.js", 1)
        .expect("reading the chunk");
    let chunk = js::Uint8Array::from_jsval(&scope, chunk, ()).expect("a Uint8Array");
    assert_eq!(chunk.with_slice(|bytes| bytes.as_ptr() as usize), address);
}
