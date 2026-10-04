// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:test`: <https://nodejs.org/api/test.html>.
//!
//! Tests, suites and hooks are records: plain objects holding their state. Tests run one at a time
//! in the order they were started, whatever their `concurrency` option. Tests registered at the top
//! level start in a later job than the one that registered them, once the functions of the suites
//! registered by then have finished. The functions of tests, suites and hooks run with their record
//! in the async context, so a test registered while one of them runs, including after an `await`,
//! becomes a subtest of that test or suite.
//!
//! Results are written to `process.stdout` in the format of Node.js's `spec` reporter, without
//! colors or source locations. The summary is written when `process` emits `beforeExit`, and sets
//! `process.exitCode` to 1 if a test failed. Tests that haven't finished by then are cancelled.
//! `run()`, snapshots, mock timers and module mocks are not implemented.

mod mock;
mod report;

use js::async_context::Snapshot;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::stream::{as_object, call_method, closure, is_function, is_str, rust_string, string};
use crate::stream::{take_exception, truthy};
use crate::stream::{Captures, Props};

/// The record kinds.
const ROOT: u8 = 0;
const TEST: u8 = 1;
const SUITE: u8 = 2;
const HOOK: u8 = 3;

/// The steps of running a record, which [`advance`] takes in turn. A record's `phase` is the step
/// it takes next.
mod phase {
    /// A test or suite starts.
    pub const START: u8 = 0;
    /// The next hook in the record's `hookQueue` runs. Once there are none left, the record's
    /// `next` phase follows.
    pub const HOOKS: u8 = 1;
    /// The function of a test or hook is called.
    pub const CALL: u8 = 2;
    /// The value the function of a test or hook returned settled.
    pub const SETTLED: u8 = 3;
    /// A test's subtests finished.
    pub const SUBTESTS: u8 = 4;
    /// A suite's function finished.
    pub const BUILT: u8 = 5;
    /// A suite's subtests start.
    pub const SUITE_RUN: u8 = 6;
    /// A suite's subtests finished.
    pub const SUITE_DONE: u8 = 7;
    /// A test or suite finished.
    pub const POST: u8 = 8;
    /// The root's `after` hooks finished.
    pub const ROOT_DONE: u8 = 9;
}

/// The longest timeout, in milliseconds.
const TIMEOUT_MAX: f64 = 2147483647.0;

/// The `node:assert` functions `t.assert` holds, as Node.js's `methodsToCopy` lists them.
const ASSERT_METHODS: &[&str] = &[
    "deepEqual",
    "deepStrictEqual",
    "doesNotMatch",
    "doesNotReject",
    "doesNotThrow",
    "equal",
    "fail",
    "ifError",
    "match",
    "notDeepEqual",
    "notDeepStrictEqual",
    "notEqual",
    "notStrictEqual",
    "partialDeepStrictEqual",
    "rejects",
    "strictEqual",
    "throws",
];

/// How an awaited step settled.
enum Outcome<'s> {
    /// The awaited value fulfilled.
    Value,
    /// The awaited value rejected with the error.
    Error(HandleValue<'s>),
    /// The record's timeout expired first.
    Timeout,
}

/// The key of the realm's harness object.
static HARNESS_KEY: u8 = 0;

/// The realm's harness: the object holding the root record, under `root` once a test or hook was
/// registered, the counts of finished tests, and the runner's other per-realm state.
fn harness<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::get_or_init_shared_object(scope, &HARNESS_KEY as *const u8 as usize, |scope| {
        let harness = Object::new_plain(scope)?;
        harness.set_property(scope, c"frameKey", Object::new_plain(scope)?)?;
        harness.set_property(scope, c"contexts", *js::WeakMap::new(scope)?)?;
        harness.set_property(scope, c"builds", *js::Array::new(scope, 0)?)?;
        harness.set_property(scope, c"failures", *js::Array::new(scope, 0)?)?;
        for counter in report::COUNTERS {
            harness.set_property(scope, counter, 0)?;
        }
        harness.set_property(scope, c"success", true)?;
        harness.set_property(scope, c"building", true)?;
        Ok(harness)
    })
}

/// The root record, created on first use with the `process` listeners that write the summary and
/// handle the uncaught errors of tests.
fn root<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let harness = harness(scope)?;
    if let Some(root) = harness.obj(scope, c"root")? {
        return Ok(root);
    }
    let root = new_record(scope, ROOT, "<root>", None)?;
    root.set_property(scope, c"started", true)?;
    root.set_property(scope, c"startTime", now_ms())?;
    harness.set_property(scope, c"root", root)?;
    let process = scope.global().get_property(scope, c"process")?;
    if let Some(process) = as_object(scope, process) {
        let listener = Function::new_callback(scope, c"", 0, on_before_exit, ())?;
        let event = string(scope, "beforeExit")?;
        call_method(
            scope,
            process,
            c"on",
            &[event, (*listener).to_jsval_throwing(scope)?],
        )?;
        for name in ["uncaughtException", "unhandledRejection"] {
            let listener = Function::new_callback(scope, c"", 1, on_uncaught, name)?;
            let event = string(scope, name)?;
            call_method(
                scope,
                process,
                c"on",
                &[event, (*listener).to_jsval_throwing(scope)?],
            )?;
        }
    }
    Ok(root)
}

/// The `uncaughtException` and `unhandledRejection` listeners, whose payload is the event's name.
/// An error from a running test's async context fails the test, and ends the wait for its function
/// or subtests. Other errors become a diagnostic of the root, and fail the run.
fn on_uncaught(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let error = args.get(0);
    let event = String::from_jsval(scope, payload, ()).map_err(|e| e.throw(scope))?;
    let record = match crate::async_hooks::current_frame(scope) {
        Some(frame) => {
            let key = frame_key(scope)?.to_jsval_throwing(scope)?;
            as_object(scope, frame.lookup(scope, key)?)
        }
        None => None,
    };
    if let Some(record) = record {
        let kind = kind_of(scope, record)?;
        if kind != HOOK && kind != ROOT && !record.flag(scope, c"finished")? {
            fail(scope, record, error, &event)?;
            if let Some(controller) = record.obj(scope, c"controller")? {
                call_method(scope, controller, c"abort", &[] as &[HandleValue])?;
            }
            let current = record.num(scope, c"phase")? as u8;
            if current == phase::SETTLED || current == phase::SUBTESTS {
                let token = record.num(scope, c"token")?;
                record.set_property(scope, c"token", token + 1.0)?;
                clear_timer(scope, record)?;
                advance(scope, record, Outcome::Error(error))?;
            }
            return Ok(js::value::undefined());
        }
    }
    let shown = report::display(scope, error);
    let message = match record {
        Some(record) if kind_of(scope, record)? != ROOT => {
            let subject = if kind_of(scope, record)? == HOOK {
                let hook_type = record.get_property(scope, c"hookType")?;
                format!("Test hook \"{}\"", report::display(scope, hook_type))
            } else {
                let name = record.get_property(scope, c"name")?;
                format!("Test \"{}\"", report::display(scope, name))
            };
            format!(
                "Error: {subject} generated asynchronous activity after the test ended. This \
                 activity created the error \"{shown}\" and would have caused the test to fail, \
                 but instead triggered an {event} event."
            )
        }
        _ => format!(
            "Error: A resource generated asynchronous activity after the test ended. This \
             activity created the error \"{shown}\" which triggered an {event} event, caught by \
             the test runner."
        ),
    };
    let root = root(scope)?;
    push(
        scope,
        objects_array(scope, root, c"diagnostics")?,
        message.as_str(),
    )?;
    harness(scope)?.set_property(scope, c"success", false)?;
    let process = scope.global().get_property(scope, c"process")?;
    if let Some(process) = as_object(scope, process) {
        process.set_property(scope, c"exitCode", 1)?;
    }
    Ok(js::value::undefined())
}

/// The monotonic clock, in milliseconds.
fn now_ms() -> f64 {
    platform::clock::monotonic_ns() as f64 / 1e6
}

/// The `length` of the array `array`.
fn length(scope: &Scope<'_>, array: Object<'_>) -> Result<u32, ExnThrown> {
    Ok(array.num(scope, c"length")? as u32)
}

/// Append `value` to the array `array`.
fn push<'s>(
    scope: &'s Scope<'_>,
    array: Object<'_>,
    value: impl ToJSVal<'s>,
) -> Result<(), ExnThrown> {
    let index = length(scope, array)?;
    array.set_element(scope, index, value)
}

/// The elements of the array `object[name]`.
fn elements<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let Some(array) = object.obj(scope, name)? else {
        return Ok(Vec::new());
    };
    (0..length(scope, array)?)
        .map(|index| array.get_element(scope, index))
        .collect()
}

/// The objects in the array `object[name]`.
fn objects<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Vec<Object<'s>>, ExnThrown> {
    Ok(elements(scope, object, name)?
        .into_iter()
        .filter_map(|value| as_object(scope, value))
        .collect())
}

/// Whether `a` and `b` are the same object.
fn same_object(a: Object<'_>, b: Object<'_>) -> bool {
    a.as_raw() == b.as_raw()
}

/// A record's kind.
fn kind_of(scope: &Scope<'_>, record: Object<'_>) -> Result<u8, ExnThrown> {
    Ok(record.num(scope, c"kind")? as u8)
}

/// A new record of `kind` named `name`, the child of `parent`. It is not added to `parent`'s
/// subtests.
fn new_record<'s>(
    scope: &'s Scope<'_>,
    kind: u8,
    name: &str,
    parent: Option<Object<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    let record = Object::new_plain(scope)?;
    record.set_property(scope, c"kind", f64::from(kind))?;
    record.set_property(scope, c"name", name)?;
    let nesting = match parent {
        Some(parent) if kind_of(scope, parent)? != ROOT => parent.num(scope, c"nesting")? + 1.0,
        _ => 0.0,
    };
    record.set_property(scope, c"nesting", nesting)?;
    if let Some(parent) = parent {
        record.set_property(scope, c"parent", parent)?;
    }
    record.set_property(scope, c"subtests", *js::Array::new(scope, 0)?)?;
    record.set_property(scope, c"queue", *js::Array::new(scope, 0)?)?;
    record.set_property(scope, c"diagnostics", *js::Array::new(scope, 0)?)?;
    let hooks = Object::new_plain(scope)?;
    for name in [c"before", c"after", c"beforeEach", c"afterEach"] {
        hooks.set_property(scope, name, *js::Array::new(scope, 0)?)?;
    }
    record.set_property(scope, c"hooks", hooks)?;
    record.set_property(scope, c"pending", 0)?;
    record.set_property(scope, c"phase", f64::from(phase::START))?;
    record.set_property(scope, c"token", 0)?;
    record.set_property(scope, c"timeout", f64::INFINITY)?;
    record.set_property(scope, c"done", *js::Promise::new_pending(scope)?)?;
    Ok(record)
}

/// The parent of `record`, if it has one.
fn parent_of<'s>(
    scope: &'s Scope<'_>,
    record: Object<'_>,
) -> Result<Option<Object<'s>>, ExnThrown> {
    record.obj(scope, c"parent")
}

/// Mark `record` failed with `error`, the value the reporter shows, unless it failed already.
/// `failure_type` is the `failureType` of Node.js's `ERR_TEST_FAILURE`.
fn fail(
    scope: &Scope<'_>,
    record: Object<'_>,
    error: HandleValue<'_>,
    failure_type: &str,
) -> Result<(), ExnThrown> {
    if record.flag(scope, c"failed")? {
        return Ok(());
    }
    record.set_property(scope, c"failed", true)?;
    record.set_property(scope, c"error", error)?;
    record.set_property(scope, c"failureType", failure_type)
}

/// [`fail`] with the message `message` as the error.
fn fail_with(
    scope: &Scope<'_>,
    record: Object<'_>,
    message: &str,
    failure_type: &str,
) -> Result<(), ExnThrown> {
    let message = string(scope, message)?;
    fail(scope, record, message, failure_type)
}

/// Cancel `record` with the message `message`: mark it failed and cancelled, unless it failed
/// already.
fn cancel(
    scope: &Scope<'_>,
    record: Object<'_>,
    message: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if record.flag(scope, c"failed")? {
        return Ok(());
    }
    fail(scope, record, message, "cancelledByParent")?;
    record.set_property(scope, c"cancelled", true)
}

/// The message of a test that ran out of time.
fn timeout_message(scope: &Scope<'_>, record: Object<'_>) -> Result<String, ExnThrown> {
    let timeout = record.num(scope, c"timeout")?;
    Ok(format!(
        "test timed out after {}ms",
        crate::errors::number_to_string(timeout)
    ))
}

/// The `ERR_TEST_FAILURE` error with `message`.
fn test_failure(message: &str) -> NodeError {
    NodeError::new(ErrorKind::Error, "ERR_TEST_FAILURE", message)
}

// ---------------------------------------------------------------------------
// The async context
// ---------------------------------------------------------------------------

/// The key of the current record in the async context's frame.
fn frame_key<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    harness(scope)?
        .obj(scope, c"frameKey")?
        .ok_or_else(|| js::error::TypeError("no frame key".into()).throw(scope))
}

/// The test or suite whose function, or whose hook's function, is running in the current async
/// context.
fn current_record<'s>(scope: &'s Scope<'_>) -> Result<Option<Object<'s>>, ExnThrown> {
    let Some(frame) = crate::async_hooks::current_frame(scope) else {
        return Ok(None);
    };
    let key = frame_key(scope)?.to_jsval_throwing(scope)?;
    let Some(record) = as_object(scope, frame.lookup(scope, key)?) else {
        return Ok(None);
    };
    if kind_of(scope, record)? == HOOK {
        return record.obj(scope, c"owner");
    }
    Ok(Some(record))
}

/// Call `callee` with `this` and `args`, with `record` as the current record of the async context.
fn call_with_record<'s>(
    scope: &'s Scope<'_>,
    record: Object<'_>,
    callee: HandleValue<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let key = frame_key(scope)?;
    let value = record.to_jsval_throwing(scope)?;
    let frame = crate::async_hooks::derived_frame(scope, key, Some(value))?;
    let snapshot = Snapshot::of(Some(*frame));
    Ok(scope.root_value(crate::async_hooks::call_in(
        scope, snapshot, callee, this, args,
    )?))
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// The `TestContext` of `record`, created on first use.
fn context_of<'s>(scope: &'s Scope<'_>, record: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(ctx) = record.obj(scope, c"ctx")? {
        return Ok(ctx);
    }
    let ctor = test_context::class_constructor(scope)?;
    let ctx = Function::construct(scope, ctor, &[] as &[Value])?;
    register_context(scope, ctx, record)?;
    record.set_property(scope, c"ctx", ctx)?;
    Ok(ctx)
}

/// The context the hooks of `record` receive: a `SuiteContext` for a suite, and its `TestContext`
/// otherwise.
fn hook_context<'s>(scope: &'s Scope<'_>, record: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if kind_of(scope, record)? != SUITE {
        return context_of(scope, record);
    }
    if let Some(ctx) = record.obj(scope, c"suiteCtx")? {
        return Ok(ctx);
    }
    let ctor = suite_context::class_constructor(scope)?;
    let ctx = Function::construct(scope, ctor, &[] as &[Value])?;
    register_context(scope, ctx, record)?;
    record.set_property(scope, c"suiteCtx", ctx)?;
    Ok(ctx)
}

/// Record that the context `ctx` belongs to `record`.
fn register_context(
    scope: &Scope<'_>,
    ctx: Object<'_>,
    record: Object<'_>,
) -> Result<(), ExnThrown> {
    let contexts = contexts(scope)?;
    contexts.insert(
        scope,
        ctx.to_jsval_throwing(scope)?,
        record.to_jsval_throwing(scope)?,
    )
}

/// The realm's `WeakMap` from each context to its record.
fn contexts<'s>(scope: &'s Scope<'_>) -> Result<js::WeakMap<'s>, ExnThrown> {
    harness(scope)?
        .obj(scope, c"contexts")?
        .and_then(|contexts| contexts.cast::<js::WeakMap>().ok())
        .ok_or_else(|| js::error::TypeError("no contexts".into()).throw(scope))
}

/// The record of the context `this`, or a `TypeError` if it isn't one.
fn record_of<'s>(scope: &'s Scope<'_>, this: HandleValue<'_>) -> Result<Object<'s>, ExnThrown> {
    let illegal = || js::error::TypeError("Illegal invocation".into()).throw(scope);
    if !this.is_object() {
        return Err(illegal());
    }
    let key = scope.root_value(this.get());
    as_object(scope, contexts(scope)?.lookup(scope, key)?).ok_or_else(illegal)
}

/// The names of `record` and its ancestors below the root, joined with ` > `.
fn full_name(scope: &Scope<'_>, record: Object<'_>) -> Result<String, ExnThrown> {
    let mut names = Vec::new();
    let mut current = Some(record);
    while let Some(object) = current {
        if kind_of(scope, object)? == ROOT {
            break;
        }
        let name = object.get_property(scope, c"name")?;
        names.push(String::from_jsval(scope, name, ()).unwrap_or_default());
        current = parent_of(scope, object)?;
    }
    names.reverse();
    Ok(names.join(" > "))
}

/// `process.argv[1]`, the entry module's path, or `undefined`.
fn entry_file<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let process = scope.global().get_property(scope, c"process")?;
    let Some(argv) = as_object(scope, process)
        .map(|process| process.obj(scope, c"argv"))
        .transpose()?
        .flatten()
    else {
        return Ok(scope.root_value(js::value::undefined()));
    };
    argv.get_element(scope, 1)
}

/// The `AbortSignal` of `record`, created on first use, and aborted once `record` finished.
fn signal_of<'s>(scope: &'s Scope<'_>, record: Object<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let controller = match record.obj(scope, c"controller")? {
        Some(controller) => controller,
        None => {
            let ctor = scope.global().get_property(scope, c"AbortController")?;
            let controller = Function::construct(scope, ctor, &[] as &[Value])?;
            record.set_property(scope, c"controller", controller)?;
            if record.flag(scope, c"finished")? {
                call_method(scope, controller, c"abort", &[] as &[HandleValue])?;
            }
            controller
        }
    };
    controller.get_property(scope, c"signal")
}

/// The `MockTracker` of `record`, created on first use, and reset once `record` finished.
fn mock_of<'s>(scope: &'s Scope<'_>, record: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(tracker) = record.obj(scope, c"mock")? {
        return Ok(tracker);
    }
    let tracker = mock::new_tracker(scope)?;
    record.set_property(scope, c"mock", tracker)?;
    Ok(tracker)
}

/// The assertion functions `t.assert` wraps: the realm's map from names to functions, holding the
/// [`ASSERT_METHODS`] of `node:assert` and the functions `assert.register` added.
fn assertions<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let harness = harness(scope)?;
    if let Some(map) = harness.obj(scope, c"assertions")? {
        return Ok(map);
    }
    let map = Object::new_plain(scope)?;
    if let Some(assert) = core_runtime::module::builtin_namespace(scope, "node:assert")? {
        for &name in ASSERT_METHODS {
            let c_name = std::ffi::CString::new(name).map_err(|_| ExnThrown)?;
            map.set_property_by_name(scope, &c_name, assert.get_property_by_name(scope, &c_name)?)?;
        }
    }
    harness.set_property(scope, c"assertions", map)?;
    Ok(map)
}

/// The body of a `t.assert` function, whose payload captures the record and the assertion
/// function: count the assertion for the record's plan, then call the function.
fn counted_assertion(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let record = captures.obj(0)?;
    count_for_plan(scope, record)?;
    let ctx = context_of(scope, record)?.to_jsval_throwing(scope)?;
    let callee = captures.get(1)?;
    Ok(Function::call(scope, ctx, callee, &crate::callback_args(&args))?.get())
}

/// Count an assertion or a subtest of `record` for its plan, if it has one.
fn count_for_plan(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    if record.get_property(scope, c"plan")?.is_number() {
        let actual = record.num(scope, c"planActual")?;
        record.set_property(scope, c"planActual", actual + 1.0)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Registering tests and hooks
// ---------------------------------------------------------------------------

/// Create the test or suite of `kind` from the arguments `name`, `options` and `callee` of
/// `test()`, `t.test()` or `describe()`, as a subtest of `parent`. `modifier` is the option
/// `test.skip`, `test.todo`, `test.only` or `test.expectFailure` sets, if any.
fn create_test<'s>(
    scope: &'s Scope<'_>,
    parent: Object<'_>,
    kind: u8,
    args: [HandleValue<'_>; 3],
    modifier: Option<&str>,
) -> Result<Object<'s>, Thrown> {
    let [mut name, mut options, mut callee] = args;
    if is_function(scope, name) {
        callee = name;
        name = HandleValue::undefined();
        options = HandleValue::undefined();
    } else if name.is_object() {
        callee = options;
        options = name;
        name = HandleValue::undefined();
    } else if is_function(scope, options) {
        callee = options;
        options = HandleValue::undefined();
    }
    let options = if options.is_object() {
        as_object(scope, options).ok_or(ExnThrown)?
    } else {
        Object::new_plain(scope)?
    };
    let callee = if is_function(scope, callee) {
        callee
    } else {
        HandleValue::undefined()
    };
    let name = match rust_string(scope, name) {
        Some(name) if !name.is_empty() => name,
        _ => {
            let fn_name = match as_object(scope, callee) {
                Some(callee) => callee.get_property(scope, c"name")?,
                None => HandleValue::undefined(),
            };
            match rust_string(scope, fn_name) {
                Some(name) if !name.is_empty() => name,
                _ => "<anonymous>".into(),
            }
        }
    };

    let concurrency = options.get_property(scope, c"concurrency")?;
    if concurrency.is_number() {
        let value = concurrency.to_number();
        if value.fract() != 0.0 || !(1.0..=4294967295.0).contains(&value) {
            return Err(NodeError::out_of_range(
                "options.concurrency",
                ">= 1 && <= 4294967295",
                &crate::errors::number_to_string(value),
            )
            .into());
        }
    } else if !concurrency.is_boolean() && !concurrency.is_null() && !concurrency.is_undefined() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "options.concurrency",
            "of type boolean or number",
            concurrency,
        )
        .into());
    }
    let timeout = options.get_property(scope, c"timeout")?;
    let timeout = if timeout.is_null() || timeout.is_undefined() {
        None
    } else if timeout.is_number() && timeout.to_number() == f64::INFINITY {
        Some(f64::INFINITY)
    } else {
        if !timeout.is_number() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.timeout",
                "of type number",
                timeout,
            )
            .into());
        }
        let value = timeout.to_number();
        if !(0.0..=TIMEOUT_MAX).contains(&value) {
            return Err(NodeError::out_of_range(
                "options.timeout",
                ">= 0 && <= 2147483647",
                &crate::errors::number_to_string(value),
            )
            .into());
        }
        Some(value)
    };
    let signal = options.get_property(scope, c"signal")?;
    crate::errors::validate_abort_signal(scope, signal, "options.signal")?;

    let finished = parent.flag(scope, c"finished")?
        || (kind_of(scope, parent)? == SUITE && parent.flag(scope, c"built")?);
    let parent = if finished { root(scope)? } else { parent };
    let record = new_record(scope, kind, &name, Some(parent))?;
    record.set_property(scope, c"fn", callee)?;

    let skip = match modifier {
        Some("skip") => scope.root_value(js::value::from_bool(true)),
        _ => options.get_property(scope, c"skip")?,
    };
    let todo = match modifier {
        Some("todo") => scope.root_value(js::value::from_bool(true)),
        _ => options.get_property(scope, c"todo")?,
    };
    let skipped = !skip.is_undefined() && !(skip.is_boolean() && !skip.to_boolean());
    let todo_set = !todo.is_undefined() && !(todo.is_boolean() && !todo.to_boolean());
    record.set_property(scope, c"skipped", skipped)?;
    record.set_property(scope, c"todo", todo_set || parent.flag(scope, c"todo")?)?;
    if skip.is_string() {
        record.set_property(scope, c"message", skip)?;
    } else if todo.is_string() {
        record.set_property(scope, c"message", todo)?;
    }
    let expect_failure = match modifier {
        Some("expectFailure") => true,
        _ => {
            let value = options.get_property(scope, c"expectFailure")?;
            !value.is_undefined() && !(value.is_boolean() && !value.to_boolean())
        }
    };
    record.set_property(
        scope,
        c"expectFailure",
        expect_failure || parent.flag(scope, c"expectFailure")?,
    )?;
    let only = match modifier {
        Some("only") => scope.root_value(js::value::from_bool(true)),
        _ => options.get_property(scope, c"only")?,
    };
    record.set_property(scope, c"only", only)?;
    if only.is_boolean() && only.to_boolean() {
        parent.set_property(scope, c"runOnlySubtests", true)?;
        if kind_of(scope, parent)? == ROOT || !parent.flag(scope, c"started")? {
            let mut current = Some(parent);
            while let Some(ancestor) = current {
                if ancestor.flag(scope, c"hasOnlyTests")? {
                    break;
                }
                ancestor.set_property(scope, c"hasOnlyTests", true)?;
                current = parent_of(scope, ancestor)?;
            }
        }
    }
    if skipped || (only.is_boolean() && !only.to_boolean()) {
        record.set_property(scope, c"fn", js::value::undefined())?;
    }
    let inherited = if kind == SUITE {
        f64::INFINITY
    } else {
        parent.num(scope, c"timeout")?
    };
    record.set_property(scope, c"timeout", timeout.unwrap_or(inherited))?;
    let plan = options.get_property(scope, c"plan")?;
    if plan.is_number() {
        record.set_property(scope, c"planOption", plan)?;
    }

    push(scope, objects_array(scope, parent, c"subtests")?, record)?;
    if finished {
        fail_with(
            scope,
            record,
            "test could not be started because its parent finished",
            "parentAlreadyFinished",
        )?;
    }
    if kind == SUITE {
        build_suite(scope, record)?;
    }
    Ok(record)
}

/// The array `object[name]`.
fn objects_array<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
) -> Result<Object<'s>, ExnThrown> {
    object
        .obj(scope, name)?
        .ok_or_else(|| js::error::TypeError("not an array".into()).throw(scope))
}

/// Call the function of the suite `record` with its `SuiteContext`. If it returns a promise, the
/// suite is building until the promise settles. A function that throws or rejects fails the suite.
fn build_suite(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    let callee = record.get_property(scope, c"fn")?;
    if !is_function(scope, callee) {
        return record.set_property(scope, c"built", true);
    }
    let ctx = hook_context(scope, record)?.to_jsval_throwing(scope)?;
    match call_with_record(scope, record, callee, ctx, &[ctx]) {
        Err(ExnThrown) => {
            let error = take_exception(scope);
            fail(scope, record, error, "testCodeFailure")?;
            record.set_property(scope, c"built", true)
        }
        Ok(result) if is_thenable(scope, result)? => {
            record.set_property(scope, c"building", true)?;
            let promise = js::Promise::call_original_resolve(scope, result)?;
            let record_value = record.to_jsval_throwing(scope)?;
            let fulfilled = scope.root_value(js::value::from_bool(false));
            let rejected = scope.root_value(js::value::from_bool(true));
            let on_built = closure(scope, 1, suite_built, &[record_value, fulfilled])?;
            let on_failed = closure(scope, 1, suite_built, &[record_value, rejected])?;
            let built = promise.then(scope, Some(on_built), Some(on_failed))?;
            record.set_property(scope, c"buildPromise", *built)?;
            push(
                scope,
                objects_array(scope, harness(scope)?, c"builds")?,
                *built,
            )
        }
        Ok(_) => record.set_property(scope, c"built", true),
    }
}

/// Whether `value` is an object with a `then` method.
fn is_thenable(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    match as_object(scope, value) {
        Some(object) => Ok(is_function(scope, object.get_property(scope, c"then")?)),
        None => Ok(false),
    }
}

/// The reaction to the promise a suite's function returned, whose payload captures the suite and
/// whether this is the rejection reaction: finish its building, and fail it with the reason of a
/// rejection.
fn suite_built(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let record = captures.obj(0)?;
    record.set_property(scope, c"building", false)?;
    record.set_property(scope, c"built", true)?;
    if captures.flag(1)? {
        fail(scope, record, args.get(0), "testCodeFailure")?;
    }
    Ok(js::value::undefined())
}

/// Register the test or suite of `kind` that `test()` or `describe()` was called with, as a
/// subtest of the current record or the root, and start it unless its parent is a suite. Returns
/// a promise that settles once it finished, or an already resolved one for a subtest of a suite.
fn register<'s>(
    scope: &'s Scope<'_>,
    kind: u8,
    args: [HandleValue<'_>; 3],
    modifier: Option<&str>,
) -> Result<HandleValue<'s>, Thrown> {
    let parent = match current_record(scope)? {
        Some(parent) => parent,
        None => root(scope)?,
    };
    let record = create_test(scope, parent, kind, args, modifier)?;
    let parent = parent_of(scope, record)?.ok_or(ExnThrown)?;
    match kind_of(scope, parent)? {
        SUITE => Ok(
            js::Promise::call_original_resolve(scope, HandleValue::undefined())?
                .to_jsval_throwing(scope)?,
        ),
        ROOT if harness(scope)?.flag(scope, c"building")? => {
            begin(scope)?;
            Ok(record.get_property(scope, c"done")?)
        }
        _ => Ok(start_subtest(scope, record)?),
    }
}

/// Start running the root's subtests in a later job, once the suites registered by then were
/// built. Does nothing after the first call.
fn begin(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let harness = harness(scope)?;
    if harness.flag(scope, c"begun")? {
        return Ok(());
    }
    harness.set_property(scope, c"begun", true)?;
    let ready = js::Promise::call_original_resolve(scope, HandleValue::undefined())?;
    let on_ready = closure(scope, 0, wait_for_builds, &[] as &[HandleValue])?;
    ready.add_reactions(scope, Some(on_ready), None)
}

/// Wait for the suites the harness's `builds` holds to be built, then start the root's subtests.
fn wait_for_builds(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let harness = harness(scope)?;
    let builds = objects(scope, harness, c"builds")?;
    harness.set_property(scope, c"builds", *js::Array::new(scope, 0)?)?;
    let on_built = closure(scope, 0, start_root_tests, &[] as &[HandleValue])?;
    if builds.is_empty() {
        let ready = js::Promise::call_original_resolve(scope, HandleValue::undefined())?;
        ready.add_reactions(scope, Some(on_built), None)?;
    } else {
        let handles: Vec<_> = builds.iter().map(|build| build.handle()).collect();
        let all = js::Promise::wait_for_all_from(scope, &handles)?;
        all.add_reactions(scope, Some(on_built), Some(on_built))?;
    }
    Ok(js::value::undefined())
}

/// End the harness's building phase, and start the root's subtests that haven't started.
fn start_root_tests(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let harness = harness(scope)?;
    if !harness.flag(scope, c"building")? {
        return Ok(js::value::undefined());
    }
    harness.set_property(scope, c"building", false)?;
    let root = root(scope)?;
    for record in objects(scope, root, c"subtests")? {
        if !record.flag(scope, c"queued")? && !record.flag(scope, c"finished")? {
            start_subtest(scope, record)?;
        }
    }
    root_check(scope)?;
    Ok(js::value::undefined())
}

/// Whether the `only` options of `record` and its siblings leave it out.
fn filtered_out(
    scope: &Scope<'_>,
    record: Object<'_>,
    parent: Object<'_>,
) -> Result<bool, ExnThrown> {
    if record.flag(scope, c"failed")? {
        return Ok(false);
    }
    let only = record.get_property(scope, c"only")?;
    let only_true = only.is_boolean() && only.to_boolean();
    let only_false = only.is_boolean() && !only.to_boolean();
    if only_true || record.flag(scope, c"hasOnlyTests")? {
        return Ok(false);
    }
    Ok(parent.flag(scope, c"runOnlySubtests")?
        || parent.flag(scope, c"hasOnlyTests")?
        || only_false)
}

/// Start `record`: queue it to run once its parent runs no other subtest. Returns the promise that
/// settles once it finished. A test the `only` options leave out finishes without running and
/// without being reported.
fn start_subtest<'s>(
    scope: &'s Scope<'_>,
    record: Object<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let done = record.get_property(scope, c"done")?;
    let parent = parent_of(scope, record)?.ok_or(ExnThrown)?;
    if filtered_out(scope, record, parent)? {
        record.set_property(scope, c"filtered", true)?;
        record.set_property(scope, c"finished", true)?;
        record.set_property(scope, c"passed", true)?;
        resolve_done(scope, record)?;
        return Ok(done);
    }
    record.set_property(scope, c"queued", true)?;
    let pending = parent.num(scope, c"pending")?;
    parent.set_property(scope, c"pending", pending + 1.0)?;
    push(scope, objects_array(scope, parent, c"queue")?, record)?;
    process_queue(scope, parent)?;
    Ok(done)
}

/// Run the next subtest in the queue of `parent`, if it runs none.
fn process_queue(scope: &Scope<'_>, parent: Object<'_>) -> Result<(), ExnThrown> {
    while parent.obj(scope, c"running")?.is_none() {
        let queue = objects_array(scope, parent, c"queue")?;
        if length(scope, queue)? == 0 {
            break;
        }
        let shifted = call_method(scope, queue, c"shift", &[] as &[HandleValue])?;
        let Some(record) = as_object(scope, shifted) else {
            continue;
        };
        if record.flag(scope, c"finished")? {
            continue;
        }
        parent.set_property(scope, c"running", record)?;
        record.set_property(scope, c"phase", f64::from(phase::START))?;
        advance(scope, record, Outcome::Value)?;
    }
    Ok(())
}

/// Resolve the promise that settles once `record` finished.
fn resolve_done(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    if let Some(done) = record.obj(scope, c"done")? {
        if let Ok(done) = done.cast::<js::Promise>() {
            done.resolve(scope, HandleValue::undefined())?;
        }
    }
    Ok(())
}

/// Register the hook of `hook_type` that `before()`, `t.after()` or the like was called with, as a
/// hook of `owner`. A `before` hook of a test or suite that started runs right away, and fails the
/// test if it fails.
fn create_hook(
    scope: &Scope<'_>,
    owner: Object<'_>,
    hook_type: &str,
    callee: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<(), Thrown> {
    let hook = new_record(scope, HOOK, "<anonymous>", None)?;
    hook.set_property(scope, c"owner", owner)?;
    hook.set_property(scope, c"hookType", hook_type)?;
    hook.set_property(scope, c"state", 0)?;
    if is_function(scope, callee) {
        hook.set_property(scope, c"fn", callee)?;
    }
    if let Some(options) = as_object(scope, options) {
        let timeout = options.get_property(scope, c"timeout")?;
        if timeout.is_number() {
            let value = timeout.to_number();
            if value != f64::INFINITY && !(0.0..=TIMEOUT_MAX).contains(&value) {
                return Err(NodeError::out_of_range(
                    "options.timeout",
                    ">= 0 && <= 2147483647",
                    &crate::errors::number_to_string(value),
                )
                .into());
            }
            hook.set_property(scope, c"timeout", value)?;
        } else if !timeout.is_null() && !timeout.is_undefined() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.timeout",
                "of type number",
                timeout,
            )
            .into());
        }
    }
    let hooks = owner.obj(scope, c"hooks")?.ok_or(ExnThrown)?;
    let c_type = std::ffi::CString::new(hook_type).map_err(|_| ExnThrown)?;
    let array = hooks.get_property_by_name(scope, &c_type)?;
    let array = Object::from_value(scope, array)
        .map_err(|_| js::error::TypeError("not an array".into()).throw(scope))?;
    push(scope, array, hook)?;
    if hook_type == "before" && owner.flag(scope, c"started")? {
        let ctx = hook_context(scope, owner)?.to_jsval_throwing(scope)?;
        let done = start_hook(scope, hook, ctx)?;
        if kind_of(scope, owner)? != ROOT {
            let values = [
                owner.to_jsval_throwing(scope)?,
                hook.to_jsval_throwing(scope)?,
            ];
            let on_done = closure(scope, 0, early_hook_done, &values)?;
            done.add_reactions(scope, Some(on_done), None)?;
        }
    }
    Ok(())
}

/// The reaction to a `before` hook that ran as it was registered, whose payload captures its owner
/// and the hook: fail the owner if the hook failed.
fn early_hook_done(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let owner = captures.obj(0)?;
    let hook = captures.obj(1)?;
    if hook.flag(scope, c"failed")? {
        let error = hook.get_property(scope, c"error")?;
        fail(scope, owner, error, "hookFailed")?;
    }
    Ok(js::value::undefined())
}

// ---------------------------------------------------------------------------
// Running records
// ---------------------------------------------------------------------------

/// Await `value` for `record`: once it settles, or once the record's deadline passes if `timed` is
/// set, [`advance`] the record from the phase `next`. Settlements and timeouts that a later await
/// or the record's end superseded are ignored.
fn await_value(
    scope: &Scope<'_>,
    record: Object<'_>,
    value: HandleValue<'_>,
    next: u8,
    timed: bool,
) -> Result<(), ExnThrown> {
    let token = record.num(scope, c"token")? + 1.0;
    record.set_property(scope, c"token", token)?;
    record.set_property(scope, c"phase", f64::from(next))?;
    let promise = js::Promise::call_original_resolve(scope, value)?;
    let record_value = record.to_jsval_throwing(scope)?;
    let token_value = scope.root_value(js::value::from_f64(token));
    let fulfilled = scope.root_value(js::value::from_bool(false));
    let rejected = scope.root_value(js::value::from_bool(true));
    let on_fulfilled = closure(scope, 1, resume, &[record_value, token_value, fulfilled])?;
    let on_rejected = closure(scope, 1, resume, &[record_value, token_value, rejected])?;
    promise.add_reactions(scope, Some(on_fulfilled), Some(on_rejected))?;
    let deadline = record.get_property(scope, c"deadline")?;
    if timed && deadline.is_number() && deadline.to_number().is_finite() {
        let delay = (deadline.to_number() - now_ms()).max(0.0);
        let on_timeout = closure(scope, 0, resume_timeout, &[record_value, token_value])?;
        let callback = js::Callable::from_jsval(scope, on_timeout.to_jsval_throwing(scope)?, ())
            .map_err(|e| e.throw(scope))?;
        let id = core_runtime::event_loop::timer::start_timer(
            scope,
            callback,
            HandleValue::undefined(),
            &[],
            std::time::Duration::from_micros((delay * 1000.0) as u64),
            false,
            None,
        );
        if let Some(id) = id {
            record.set_property(scope, c"timer", id as f64)?;
        }
    }
    Ok(())
}

/// Cancel the timer of `record`'s deadline, if it has one.
fn clear_timer(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    let timer = record.get_property(scope, c"timer")?;
    if timer.is_number() {
        let id = timer.to_number() as u64;
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
        record.set_property(scope, c"timer", js::value::undefined())?;
    }
    Ok(())
}

/// Whether the await `captures` was created for, which hold its record and token, is the record's
/// current await. If it is, it is consumed and its timer cancelled.
fn take_await(scope: &Scope<'_>, captures: &Captures<'_>) -> Result<bool, ExnThrown> {
    let record = captures.obj(0)?;
    let token = record.num(scope, c"token")?;
    let awaited = captures.get(1)?;
    if !awaited.is_number() || awaited.to_number() != token {
        return Ok(false);
    }
    record.set_property(scope, c"token", token + 1.0)?;
    clear_timer(scope, record)?;
    Ok(true)
}

/// The reaction to an awaited value, whose payload captures the record, the await's token, and
/// whether this is the rejection reaction.
fn resume(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if !take_await(scope, &captures)? {
        return Ok(js::value::undefined());
    }
    let record = captures.obj(0)?;
    let outcome = if captures.flag(2)? {
        Outcome::Error(args.get(0))
    } else {
        Outcome::Value
    };
    if advance(scope, record, outcome).is_err() {
        core_runtime::event_loop::handle_and_clear_exception(scope);
    }
    Ok(js::value::undefined())
}

/// The timer callback of a record's deadline, whose payload captures the record and the await's
/// token.
fn resume_timeout(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if !take_await(scope, &captures)? {
        return Ok(js::value::undefined());
    }
    let record = captures.obj(0)?;
    if advance(scope, record, Outcome::Timeout).is_err() {
        core_runtime::event_loop::handle_and_clear_exception(scope);
    }
    Ok(js::value::undefined())
}

/// Set the deadline of `record` from its timeout.
fn set_deadline(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    let timeout = record.num(scope, c"timeout")?;
    if timeout.is_finite() {
        record.set_property(scope, c"deadline", now_ms() + timeout)?;
    }
    Ok(())
}

/// Mark `record` started.
fn mark_started(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    record.set_property(scope, c"started", true)?;
    record.set_property(scope, c"startTime", now_ms())
}

/// The hooks to run in turn, with the context each receives.
type HookList<'s> = Vec<(Object<'s>, HandleValue<'s>)>;

/// Append the hooks of `hook_type` of `owner` to `list`, each receiving `ctx`.
fn add_hooks<'s>(
    scope: &'s Scope<'_>,
    list: &mut HookList<'s>,
    owner: Object<'_>,
    hook_type: &'static std::ffi::CStr,
    ctx: HandleValue<'s>,
) -> Result<(), ExnThrown> {
    let hooks = owner.obj(scope, c"hooks")?.ok_or(ExnThrown)?;
    for hook in objects(scope, hooks, hook_type)? {
        list.push((hook, ctx));
    }
    Ok(())
}

/// The ancestors of `record`, starting with its parent.
fn ancestors<'s>(scope: &'s Scope<'_>, record: Object<'_>) -> Result<Vec<Object<'s>>, ExnThrown> {
    let mut list = Vec::new();
    let mut current = parent_of(scope, record)?;
    while let Some(ancestor) = current {
        current = parent_of(scope, ancestor)?;
        list.push(ancestor);
    }
    Ok(list)
}

/// The hooks that run after the function of the test `record`, or after it failed: the
/// `afterEach` hooks of its ancestors, innermost first, unless it was skipped before it ran, then
/// its own `after` hooks.
fn after_hooks<'s>(scope: &'s Scope<'_>, record: Object<'_>) -> Result<HookList<'s>, ExnThrown> {
    let mut list = Vec::new();
    let ctx = context_of(scope, record)?.to_jsval_throwing(scope)?;
    if !record.flag(scope, c"wasSkipped")? {
        for ancestor in ancestors(scope, record)? {
            add_hooks(scope, &mut list, ancestor, c"afterEach", ctx)?;
        }
    }
    add_hooks(scope, &mut list, record, c"after", ctx)?;
    Ok(list)
}

/// The `after` hooks of the suite or root `record`.
fn own_after_hooks<'s>(
    scope: &'s Scope<'_>,
    record: Object<'_>,
) -> Result<HookList<'s>, ExnThrown> {
    let mut list = Vec::new();
    let ctx = hook_context(scope, record)?.to_jsval_throwing(scope)?;
    add_hooks(scope, &mut list, record, c"after", ctx)?;
    Ok(list)
}

/// Make `list` the hooks `record` runs next, followed by the phase `next`.
fn queue_hooks(
    scope: &Scope<'_>,
    record: Object<'_>,
    list: HookList<'_>,
    next: u8,
) -> Result<(), ExnThrown> {
    let queue = js::Array::new(scope, 0)?;
    for (index, (hook, ctx)) in list.into_iter().enumerate() {
        queue.set_element(scope, 2 * index as u32, hook)?;
        queue.set_element(scope, 2 * index as u32 + 1, ctx)?;
    }
    record.set_property(scope, c"hookQueue", *queue)?;
    record.set_property(scope, c"hookIndex", 0)?;
    record.set_property(scope, c"next", f64::from(next))?;
    record.set_property(scope, c"phase", f64::from(phase::HOOKS))
}

/// Run the hooks that follow the failure of the test `record`, ignoring their failures, then
/// finish it.
fn clean_up(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    record.set_property(scope, c"cleanup", true)?;
    let list = after_hooks(scope, record)?;
    queue_hooks(scope, record, list, phase::POST)
}

/// Run `hook` with `ctx`, unless it is a `before` or `after` hook that ran or runs already.
/// Returns the promise that resolves once it finished.
fn start_hook<'s>(
    scope: &'s Scope<'_>,
    hook: Object<'_>,
    ctx: HandleValue<'_>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let hook_type = hook.get_property(scope, c"hookType")?;
    let once = is_str(scope, hook_type, c"before")? || is_str(scope, hook_type, c"after")?;
    if once && hook.num(scope, c"state")? != 0.0 {
        if let Some(done) = hook.obj(scope, c"done")? {
            if let Ok(done) = done.cast::<js::Promise>() {
                return Ok(done);
            }
        }
    }
    let done = js::Promise::new_pending(scope)?;
    hook.set_property(scope, c"state", 1)?;
    hook.set_property(scope, c"failed", false)?;
    hook.set_property(scope, c"error", js::value::undefined())?;
    hook.set_property(scope, c"ctx", ctx)?;
    hook.set_property(scope, c"done", *done)?;
    hook.set_property(scope, c"phase", f64::from(phase::CALL))?;
    advance(scope, hook, Outcome::Value)?;
    Ok(done)
}

/// Handle the failure of `hook`, which `record` ran. Before the record's function, it fails the
/// record, whose function doesn't run. After it, it fails the record, and its remaining
/// `afterEach` hooks don't run. Hooks that fail while the record cleans up after a failure are
/// ignored.
fn hook_failed(scope: &Scope<'_>, record: Object<'_>, hook: Object<'_>) -> Result<(), ExnThrown> {
    let error = hook.get_property(scope, c"error")?;
    if kind_of(scope, record)? == ROOT {
        return report::report_root_hook_failure(scope, error);
    }
    if record.flag(scope, c"cleanup")? {
        return Ok(());
    }
    fail(scope, record, error, "hookFailed")?;
    record.set_property(scope, c"cleanup", true)?;
    let next = record.num(scope, c"next")? as u8;
    if next == phase::CALL {
        let list = after_hooks(scope, record)?;
        queue_hooks(scope, record, list, phase::POST)?;
    } else if next == phase::SUITE_RUN {
        let list = own_after_hooks(scope, record)?;
        queue_hooks(scope, record, list, phase::POST)?;
    } else {
        record.set_property(scope, c"skipAfterEach", true)?;
    }
    Ok(())
}

/// Call the function of the test or hook `record` with `ctx`, and with a callback as a second
/// argument if the function declares two parameters. Returns the value to await: the function's
/// return value, a promise the callback settles, or a promise rejected with the error the
/// function threw.
fn call_body<'s>(
    scope: &'s Scope<'_>,
    record: Object<'_>,
    ctx: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let callee = record.get_property(scope, c"fn")?;
    let Some(function) = as_object(scope, callee).filter(|callee| callee.is_callable()) else {
        return Ok(scope.root_value(js::value::undefined()));
    };
    let declared = function.num(scope, c"length")?;
    if declared == 2.0 {
        let settled = js::Promise::new_pending(scope)?;
        let called = scope.root_value(js::value::from_bool(false));
        let values = [(*settled).to_jsval_throwing(scope)?, called];
        let done = closure(scope, 1, callback_done, &values)?;
        let done = done.to_jsval_throwing(scope)?;
        return match call_with_record(scope, record, callee, ctx, &[ctx, done]) {
            Err(ExnThrown) => rejected_with_exception(scope),
            Ok(result) => {
                let is_promise = as_object(scope, result)
                    .is_some_and(|object| js::Promise::is_promise(object.handle()));
                if is_promise {
                    fail_with(
                        scope,
                        record,
                        "passed a callback but also returned a Promise",
                        "callbackAndPromisePresent",
                    )?;
                    Ok(result)
                } else {
                    (*settled).to_jsval_throwing(scope)
                }
            }
        };
    }
    match call_with_record(scope, record, callee, ctx, &[ctx]) {
        Err(ExnThrown) => rejected_with_exception(scope),
        Ok(result) => Ok(result),
    }
}

/// A promise rejected with the pending exception.
fn rejected_with_exception<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let error = take_exception(scope);
    js::Promise::call_original_reject(scope, error)?.to_jsval_throwing(scope)
}

/// The callback a test function that declares two parameters receives, whose payload captures the
/// promise it settles and whether it was called: reject the promise with a truthy first argument,
/// and resolve it otherwise.
fn callback_done(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    if captures.flag(1)? {
        return Err(test_failure("callback invoked multiple times").throw(scope));
    }
    captures.set(1, true)?;
    let promise = captures
        .obj(0)?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
    let error = args.get(0);
    if truthy(scope, error) {
        promise.reject(scope, error)?;
    } else {
        promise.resolve(scope, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

/// Take the steps of running `record` from its `phase`, until it awaits a value or finishes.
/// `outcome` describes how the value the record awaited settled.
fn advance(scope: &Scope<'_>, record: Object<'_>, outcome: Outcome<'_>) -> Result<(), ExnThrown> {
    let mut outcome = Some(outcome);
    let kind = kind_of(scope, record)?;
    loop {
        let outcome = outcome.take().unwrap_or(Outcome::Value);
        let current = record.num(scope, c"phase")? as u8;
        match current {
            phase::START if kind == SUITE => {
                mark_started(scope, record)?;
                if record.flag(scope, c"building")? {
                    let build = record.get_property(scope, c"buildPromise")?;
                    return await_value(scope, record, build, phase::BUILT, false);
                }
                record.set_property(scope, c"phase", f64::from(phase::BUILT))?;
            }
            phase::START if kind == TEST => {
                mark_started(scope, record)?;
                let plan = record.get_property(scope, c"planOption")?;
                if plan.is_number() && !record.get_property(scope, c"plan")?.is_number() {
                    record.set_property(scope, c"plan", plan)?;
                    record.set_property(scope, c"planActual", 0)?;
                }
                let skipped = record.flag(scope, c"skipped")?;
                record.set_property(scope, c"wasSkipped", skipped)?;
                let mut list = Vec::new();
                if let Some(parent) = parent_of(scope, record)? {
                    let parent_ctx = hook_context(scope, parent)?.to_jsval_throwing(scope)?;
                    add_hooks(scope, &mut list, parent, c"before", parent_ctx)?;
                    if !skipped {
                        let ctx = context_of(scope, record)?.to_jsval_throwing(scope)?;
                        let mut chain = ancestors(scope, record)?;
                        chain.reverse();
                        for ancestor in chain {
                            add_hooks(scope, &mut list, ancestor, c"beforeEach", ctx)?;
                        }
                    }
                }
                queue_hooks(scope, record, list, phase::CALL)?;
            }
            phase::HOOKS => {
                if let Some(hook) = record.obj(scope, c"currentHook")? {
                    record.set_property(scope, c"currentHook", js::value::undefined())?;
                    if hook.flag(scope, c"failed")? {
                        hook_failed(scope, record, hook)?;
                        continue;
                    }
                }
                let queue = objects_array(scope, record, c"hookQueue")?;
                let index = record.num(scope, c"hookIndex")? as u32;
                if 2 * index >= length(scope, queue)? {
                    let next = record.num(scope, c"next")? as u8;
                    record.set_property(scope, c"phase", f64::from(next))?;
                    continue;
                }
                record.set_property(scope, c"hookIndex", index + 1)?;
                let hook =
                    as_object(scope, queue.get_element(scope, 2 * index)?).ok_or(ExnThrown)?;
                let ctx = queue.get_element(scope, 2 * index + 1)?;
                if record.flag(scope, c"skipAfterEach")?
                    && is_str(scope, hook.get_property(scope, c"hookType")?, c"afterEach")?
                {
                    continue;
                }
                let done = start_hook(scope, hook, ctx)?;
                record.set_property(scope, c"currentHook", hook)?;
                return await_value(
                    scope,
                    record,
                    (*done).to_jsval_throwing(scope)?,
                    phase::HOOKS,
                    false,
                );
            }
            phase::CALL => {
                set_deadline(scope, record)?;
                if kind == TEST && record.flag(scope, c"skipped")? {
                    return await_value(
                        scope,
                        record,
                        HandleValue::undefined(),
                        phase::SETTLED,
                        false,
                    );
                }
                let ctx = if kind == HOOK {
                    record.get_property(scope, c"ctx")?
                } else {
                    context_of(scope, record)?.to_jsval_throwing(scope)?
                };
                let result = call_body(scope, record, ctx)?;
                return await_value(scope, record, result, phase::SETTLED, true);
            }
            phase::SETTLED if kind == HOOK => {
                match outcome {
                    Outcome::Error(error) => {
                        record.set_property(scope, c"failed", true)?;
                        record.set_property(scope, c"error", error)?;
                    }
                    Outcome::Timeout => {
                        let message = timeout_message(scope, record)?;
                        record.set_property(scope, c"failed", true)?;
                        record.set_property(scope, c"error", message.as_str())?;
                    }
                    Outcome::Value => {}
                }
                record.set_property(scope, c"state", 2)?;
                return resolve_done(scope, record);
            }
            phase::SETTLED => match outcome {
                Outcome::Error(error) => {
                    fail(scope, record, error, "testCodeFailure")?;
                    clean_up(scope, record)?;
                }
                Outcome::Timeout => {
                    let message = string(scope, &timeout_message(scope, record)?)?;
                    cancel(scope, record, message)?;
                    clean_up(scope, record)?;
                }
                Outcome::Value => {
                    // As in Node.js, a test waits for its subtests only until none of them is
                    // pending for the first time. Subtests started after that are cancelled if
                    // they haven't finished when the test does.
                    if record.num(scope, c"pending")? > 0.0
                        && !record.flag(scope, c"subtestsSettled")?
                    {
                        return wait_for_subtests(scope, record, phase::SUBTESTS);
                    }
                    record.set_property(scope, c"phase", f64::from(phase::SUBTESTS))?;
                }
            },
            phase::SUBTESTS => {
                match outcome {
                    Outcome::Timeout => {
                        let message = string(scope, &timeout_message(scope, record)?)?;
                        cancel(scope, record, message)?;
                        clean_up(scope, record)?;
                        continue;
                    }
                    Outcome::Error(error) => {
                        fail(scope, record, error, "testCodeFailure")?;
                        clean_up(scope, record)?;
                        continue;
                    }
                    Outcome::Value => {}
                }
                let plan = record.get_property(scope, c"plan")?;
                let actual = record.num(scope, c"planActual")?;
                if plan.is_number() && plan.to_number() != actual {
                    let message = format!(
                        "plan expected {} assertions but received {}",
                        crate::errors::number_to_string(plan.to_number()),
                        crate::errors::number_to_string(actual)
                    );
                    fail_with(scope, record, &message, "testCodeFailure")?;
                    clean_up(scope, record)?;
                    continue;
                }
                let list = after_hooks(scope, record)?;
                queue_hooks(scope, record, list, phase::POST)?;
            }
            phase::BUILT => {
                let mut list = Vec::new();
                if let Some(parent) = parent_of(scope, record)? {
                    let parent_ctx = hook_context(scope, parent)?.to_jsval_throwing(scope)?;
                    add_hooks(scope, &mut list, parent, c"before", parent_ctx)?;
                }
                let ctx = hook_context(scope, record)?.to_jsval_throwing(scope)?;
                add_hooks(scope, &mut list, record, c"before", ctx)?;
                queue_hooks(scope, record, list, phase::SUITE_RUN)?;
            }
            phase::SUITE_RUN => {
                set_deadline(scope, record)?;
                if !record.flag(scope, c"skipped")? && !record.flag(scope, c"failed")? {
                    for subtest in objects(scope, record, c"subtests")? {
                        if !subtest.flag(scope, c"queued")? && !subtest.flag(scope, c"finished")? {
                            start_subtest(scope, subtest)?;
                        }
                    }
                }
                if record.num(scope, c"pending")? > 0.0 {
                    return wait_for_subtests(scope, record, phase::SUITE_DONE);
                }
                record.set_property(scope, c"phase", f64::from(phase::SUITE_DONE))?;
            }
            phase::SUITE_DONE => {
                if let Outcome::Timeout = outcome {
                    let message = string(scope, &timeout_message(scope, record)?)?;
                    cancel(scope, record, message)?;
                }
                let failed = record.flag(scope, c"failed")?;
                record.set_property(scope, c"cleanup", failed)?;
                let list = own_after_hooks(scope, record)?;
                queue_hooks(scope, record, list, phase::POST)?;
            }
            phase::POST => return post_run(scope, record, None),
            phase::ROOT_DONE => {
                record.set_property(scope, c"afterDone", true)?;
                if harness(scope)?.flag(scope, c"exiting")? {
                    return summarize(scope);
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

/// Await the end of the subtests of `record`, then advance it from the phase `next`.
fn wait_for_subtests(scope: &Scope<'_>, record: Object<'_>, next: u8) -> Result<(), ExnThrown> {
    let waiting = js::Promise::new_pending(scope)?;
    record.set_property(scope, c"waiting", *waiting)?;
    await_value(
        scope,
        record,
        (*waiting).to_jsval_throwing(scope)?,
        next,
        true,
    )
}

/// Finish `record`: cancel its subtests that haven't finished, with `cancel_error` or the message
/// that they didn't finish before their parent, fail it if a subtest failed, report it, and run
/// the next subtest of its parent.
fn post_run(
    scope: &Scope<'_>,
    record: Object<'_>,
    cancel_error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let token = record.num(scope, c"token")?;
    record.set_property(scope, c"token", token + 1.0)?;
    clear_timer(scope, record)?;
    let now = now_ms();
    if !record.get_property(scope, c"endTime")?.is_number() {
        record.set_property(scope, c"endTime", now)?;
    }
    if !record.get_property(scope, c"startTime")?.is_number() {
        record.set_property(scope, c"startTime", now)?;
    }
    record.set_property(scope, c"queue", *js::Array::new(scope, 0)?)?;
    let message = match cancel_error {
        Some(error) => error,
        None => string(
            scope,
            "test did not finish before its parent and was cancelled",
        )?,
    };
    let mut failed = 0;
    for subtest in objects(scope, record, c"subtests")? {
        if !subtest.flag(scope, c"finished")? {
            cancel(scope, subtest, message)?;
            post_run(scope, subtest, cancel_error)?;
        }
        if !subtest.flag(scope, c"passed")? && !subtest.flag(scope, c"todo")? {
            failed += 1;
        }
    }
    if failed > 0 {
        let plural = if failed > 1 { "s" } else { "" };
        fail_with(
            scope,
            record,
            &format!("{failed} subtest{plural} failed"),
            "subtestsFailed",
        )?;
    }
    let expect_failure = record.flag(scope, c"expectFailure")?;
    if !record.flag(scope, c"failed")? && expect_failure && !record.flag(scope, c"skipped")? {
        fail_with(
            scope,
            record,
            "test was expected to fail but passed",
            "expectedFailure",
        )?;
        record.set_property(scope, c"passed", false)?;
    } else {
        let passed = !record.flag(scope, c"failed")? || expect_failure;
        record.set_property(scope, c"passed", passed)?;
    }
    if let Some(tracker) = record.obj(scope, c"mock")? {
        mock::reset_tracker(scope, tracker)?;
    }
    record.set_property(scope, c"finished", true)?;
    if let Some(controller) = record.obj(scope, c"controller")? {
        call_method(scope, controller, c"abort", &[] as &[HandleValue])?;
    }
    report::report_test(scope, record)?;
    resolve_done(scope, record)?;

    let Some(parent) = parent_of(scope, record)? else {
        return Ok(());
    };
    if record.flag(scope, c"queued")? {
        let pending = parent.num(scope, c"pending")? - 1.0;
        parent.set_property(scope, c"pending", pending)?;
        if pending <= 0.0 {
            parent.set_property(scope, c"subtestsSettled", true)?;
            if let Some(waiting) = parent.obj(scope, c"waiting")? {
                parent.set_property(scope, c"waiting", js::value::undefined())?;
                if let Ok(waiting) = waiting.cast::<js::Promise>() {
                    waiting.resolve(scope, HandleValue::undefined())?;
                }
            }
        }
    }
    if parent
        .obj(scope, c"running")?
        .is_some_and(|running| same_object(running, record))
    {
        parent.set_property(scope, c"running", js::value::undefined())?;
    }
    process_queue(scope, parent)?;
    if kind_of(scope, parent)? == ROOT {
        root_check(scope)?;
    }
    Ok(())
}

/// Run the root's `after` hooks once its subtests all finished, unless they ran already.
fn root_check(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let harness = harness(scope)?;
    let root = root(scope)?;
    if harness.flag(scope, c"building")?
        || root.flag(scope, c"afterStarted")?
        || root.num(scope, c"pending")? > 0.0
    {
        return Ok(());
    }
    root.set_property(scope, c"afterStarted", true)?;
    let list = own_after_hooks(scope, root)?;
    queue_hooks(scope, root, list, phase::ROOT_DONE)?;
    advance(scope, root, Outcome::Value)
}

/// The `beforeExit` listener: cancel the tests that haven't finished, run the root's `after` hooks
/// if they haven't run, and write the summary once they finished. A second `beforeExit` writes the
/// summary without waiting for the hooks.
fn on_before_exit(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let harness = harness(scope)?;
    if harness.flag(scope, c"reported")? {
        return Ok(js::value::undefined());
    }
    let exits = harness.num(scope, c"exits")? + 1.0;
    harness.set_property(scope, c"exits", exits)?;
    let root = root(scope)?;
    harness.set_property(scope, c"building", false)?;
    root.set_property(scope, c"queue", *js::Array::new(scope, 0)?)?;
    let message = string(
        scope,
        "Promise resolution is still pending but the event loop has already resolved",
    )?;
    for record in objects(scope, root, c"subtests")? {
        if !record.flag(scope, c"finished")? {
            cancel(scope, record, message)?;
            post_run(scope, record, Some(message))?;
        }
    }
    root_check(scope)?;
    if root.flag(scope, c"afterDone")? || exits > 1.0 {
        summarize(scope)?;
    } else {
        harness.set_property(scope, c"exiting", true)?;
    }
    Ok(js::value::undefined())
}

/// Write the summary, once, and set `process.exitCode` to 1 if a test failed.
fn summarize(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let harness = harness(scope)?;
    if harness.flag(scope, c"reported")? {
        return Ok(());
    }
    harness.set_property(scope, c"reported", true)?;
    let root = root(scope)?;
    root.set_property(scope, c"endTime", now_ms())?;
    report::summary(scope, harness, root)?;
    if !harness.flag(scope, c"success")? {
        let process = scope.global().get_property(scope, c"process")?;
        if let Some(process) = as_object(scope, process) {
            process.set_property(scope, c"exitCode", 1)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

/// The body of `test()`, `describe()` and their `skip`, `todo`, `only` and `expectFailure`
/// variants, whose payload captures the kind of record and the variant's name, if any.
fn register_entry(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let kind = captures.get(0)?.to_number() as u8;
    let modifier = rust_string(scope, captures.get(1)?);
    let result = register(
        scope,
        kind,
        [args.get(0), args.get(1), args.get(2)],
        modifier.as_deref(),
    )
    .map_err(|error| error.throw(scope))?;
    Ok(result.get())
}

/// The body of `before()`, `after()`, `beforeEach()` and `afterEach()`, whose payload is the hook
/// type.
fn register_hook(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let hook_type = String::from_jsval(scope, payload, ()).map_err(|e| e.throw(scope))?;
    let owner = match current_record(scope)? {
        Some(owner) => owner,
        None => root(scope)?,
    };
    create_hook(scope, owner, &hook_type, args.get(0), args.get(1))
        .map_err(|error| error.throw(scope))?;
    Ok(js::value::undefined())
}

/// `getTestContext()`: the context of the test or suite running in the current async context, or
/// `undefined`.
fn get_test_context(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    match current_record(scope)? {
        Some(record) if kind_of(scope, record)? != ROOT => {
            Ok(context_of(scope, record)?.to_jsval_raw_throwing(scope)?)
        }
        _ => Ok(js::value::undefined()),
    }
}

/// `assert.register(name, fn)`: add `fn` to the functions `t.assert` holds, under `name`.
fn register_assertion(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let register = || -> Result<(), Thrown> {
        let name = args.get(0);
        crate::errors::validate_string(scope, name, "name")?;
        let callee = args.get(1);
        crate::errors::validate_function(scope, callee, "fn")?;
        let id = scope.root_id(js::id::value_to_id(scope, name)?);
        assertions(scope)?.set_property_by_id(scope, id, callee)?;
        Ok(())
    };
    register().map_err(|error| error.throw(scope))?;
    Ok(js::value::undefined())
}

/// `test` or `suite` with its `skip`, `todo`, `only` and `expectFailure` variants as properties.
fn entry_function<'s>(
    scope: &'s Scope<'_>,
    name: &std::ffi::CStr,
    kind: u8,
) -> Result<Object<'s>, ExnThrown> {
    let kind_value = scope.root_value(js::value::from_f64(f64::from(kind)));
    let undefined = scope.root_value(js::value::undefined());
    let payload = js::Array::with_contents(scope, &[kind_value, undefined])?;
    let function = *Function::new_callback(scope, name, 3, register_entry, *payload)?;
    for (variant, c_variant) in [
        ("skip", c"skip"),
        ("todo", c"todo"),
        ("only", c"only"),
        ("expectFailure", c"expectFailure"),
    ] {
        let variant_value = string(scope, variant)?;
        let payload = js::Array::with_contents(scope, &[kind_value, variant_value])?;
        let variant_function =
            Function::new_callback(scope, c_variant, 3, register_entry, *payload)?;
        function.set_property(scope, c_variant, *variant_function)?;
    }
    Ok(function)
}

/// Register `node:test`, whose `default` export is `test`, with the other exports as its
/// properties. `it` is `test`, and `describe` is `suite`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let test = entry_function(scope, c"test", TEST)?;
        let suite = entry_function(scope, c"suite", SUITE)?;
        let test_value = test.to_jsval_throwing(scope)?;
        let suite_value = suite.to_jsval_throwing(scope)?;
        let mut exports = vec![
            ("test", test_value),
            ("it", test_value),
            ("suite", suite_value),
            ("describe", suite_value),
        ];
        for variant in [c"skip", c"todo", c"only", c"expectFailure"] {
            let name = variant.to_str().map_err(|_| ExnThrown)?;
            exports.push((name, test.get_property(scope, variant)?));
        }
        for (hook, c_hook) in [
            ("before", c"before"),
            ("after", c"after"),
            ("beforeEach", c"beforeEach"),
            ("afterEach", c"afterEach"),
        ] {
            let function = Function::new_callback(scope, c_hook, 2, register_hook, hook)?;
            exports.push((hook, (*function).to_jsval_throwing(scope)?));
        }
        let context = Function::new_callback(scope, c"getTestContext", 0, get_test_context, ())?;
        exports.push(("getTestContext", (*context).to_jsval_throwing(scope)?));
        exports.push(("mock", mock::new_tracker(scope)?.to_jsval_throwing(scope)?));
        let assert = Object::new_plain(scope)?;
        let register = Function::new_callback(scope, c"register", 2, register_assertion, ())?;
        assert.set_property(scope, c"register", *register)?;
        exports.push(("assert", assert.to_jsval_throwing(scope)?));
        let snapshot = Object::new_plain(scope)?;
        for (function, c_function) in [
            (
                "setDefaultSnapshotSerializers",
                c"setDefaultSnapshotSerializers",
            ),
            ("setResolveSnapshotPath", c"setResolveSnapshotPath"),
        ] {
            let qualified = format!("snapshot.{function}");
            let kind = crate::stub::ExportKind::Function;
            let stub = crate::stub::stub_value(scope, name, &qualified, kind)?;
            snapshot.set_property(scope, c_function, stub)?;
        }
        exports.push(("snapshot", snapshot.to_jsval_throwing(scope)?));
        for &(export, kind) in crate::stub::exports_of(name).unwrap_or_default() {
            if !exports.iter().any(|(name, _)| *name == export) {
                exports.push((export, crate::stub::stub_value(scope, name, export, kind)?));
            }
        }
        exports.sort_by_key(|(name, _)| *name);
        for (export, value) in &exports {
            if *export != "test" {
                let c_export = std::ffi::CString::new(*export).map_err(|_| ExnThrown)?;
                test.set_property_by_name(scope, &c_export, *value)?;
            }
        }
        test.set_property(scope, c"test", test_value)?;
        exports.push(("default", test_value));
        Ok(exports)
    };
    let Ok(exports) = exports() else {
        return false;
    };
    core_runtime::module::register_synthetic_module(scope, name, &exports).is_ok()
}

// ---------------------------------------------------------------------------
// TestContext and SuiteContext
// ---------------------------------------------------------------------------

/// The getters of `TestContext.prototype`. `SuiteContext.prototype` has the first
/// [`SUITE_GETTERS`] of them.
const GETTERS: &[&std::ffi::CStr] = &[
    c"name",
    c"fullName",
    c"filePath",
    c"signal",
    c"passed",
    c"attempt",
    c"error",
    c"assert",
    c"mock",
    c"workerId",
];

/// The number of [`GETTERS`] `SuiteContext.prototype` has.
const SUITE_GETTERS: usize = 6;

/// The body of a context getter, whose payload is its index in [`GETTERS`].
fn context_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let record = record_of(scope, scope.root_value(args.this()))?;
    match payload.to_number() as usize {
        0 => Ok(record.get_property(scope, c"name")?.get()),
        1 => full_name(scope, record)?
            .as_str()
            .to_jsval_raw_throwing(scope),
        2 => Ok(entry_file(scope)?.get()),
        3 => Ok(signal_of(scope, record)?.get()),
        4 => Ok(js::value::from_bool(record.flag(scope, c"passed")?)),
        5 => Ok(js::value::from_f64(0.0)),
        6 if record.flag(scope, c"failed")? => Ok(record.get_property(scope, c"error")?.get()),
        6 => Ok(js::value::null()),
        7 => assert_of(scope, record)?.to_jsval_raw_throwing(scope),
        8 => mock_of(scope, record)?.to_jsval_raw_throwing(scope),
        _ => Ok(js::value::undefined()),
    }
}

/// Define the first `count` [`GETTERS`] on `proto`.
fn define_getters(scope: &Scope<'_>, proto: Object<'_>, count: usize) -> Result<(), ExnThrown> {
    for (index, name) in GETTERS.iter().enumerate().take(count) {
        let getter = Function::new_callback(scope, name, 0, context_getter, index as f64)?;
        proto.define_accessor(scope, name, Some(*getter), None, 0)?;
    }
    Ok(())
}

/// `t.assert` of the test `record`, created on first use: the functions [`assertions`] holds, and
/// `node:assert`'s `ok` unless it holds one, each counting towards the plan.
fn assert_of<'s>(scope: &'s Scope<'_>, record: Object<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(assert) = record.obj(scope, c"assert")? {
        return Ok(assert);
    }
    let assert = Object::new_plain(scope)?;
    assert.set_prototype(scope, js::native::HandleObject::null())?;
    let map = assertions(scope)?;
    let record_value = record.to_jsval_throwing(scope)?;
    let mut has_ok = false;
    for key in map.own_keys(scope)? {
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        let callee = map.get_property_by_id(scope, id)?;
        let wrapper = closure(scope, 0, counted_assertion, &[record_value, callee])?;
        assert.set_property_by_id(scope, id, wrapper)?;
        has_ok |= is_str(scope, key, c"ok")?;
    }
    if !has_ok {
        if let Some(node_assert) = core_runtime::module::builtin_namespace(scope, "node:assert")? {
            let ok = node_assert.get_property(scope, c"ok")?;
            let wrapper = closure(scope, 0, counted_assertion, &[record_value, ok])?;
            assert.set_property(scope, c"ok", wrapper)?;
        }
    }
    record.set_property(scope, c"assert", assert)?;
    Ok(assert)
}

/// Add `message` to the diagnostics of the context `this`'s record.
fn add_diagnostic(
    scope: &Scope<'_>,
    this: Object<'_>,
    message: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let record = record_of(scope, this.to_jsval_throwing(scope)?)?;
    push(
        scope,
        objects_array(scope, record, c"diagnostics")?,
        message,
    )
}

/// The context of a test, its function's first argument:
/// <https://nodejs.org/api/test.html#class-testcontext>.
#[core_runtime::jsfunction_class(name = "TestContext", class_syntax)]
pub mod test_context {
    use super::*;

    /// Contexts are created for records, by [`context_of`].
    #[constructor]
    pub fn init(this: Object<'_>) -> Result<(), ExnThrown> {
        let _ = this;
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#contexttestname-options-fn>: start a subtest, which
    /// counts towards the plan.
    pub fn test<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        name: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
        callee: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let record = record_of(scope, this.to_jsval_throwing(scope)?)?;
        count_for_plan(scope, record)?;
        let undefined = HandleValue::undefined();
        let args = [
            name.unwrap_or(undefined),
            options.unwrap_or(undefined),
            callee.unwrap_or(undefined),
        ];
        let subtest = create_test(scope, record, TEST, args, None)?;
        Ok(start_subtest(scope, subtest)?)
    }

    /// <https://nodejs.org/api/test.html#contextskipmessage>
    pub fn skip(
        scope: &Scope<'_>,
        this: Object<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let record = record_of(scope, this.to_jsval_throwing(scope)?)?;
        record.set_property(scope, c"skipped", true)?;
        record.set_property(
            scope,
            c"message",
            message.unwrap_or(HandleValue::undefined()),
        )
    }

    /// <https://nodejs.org/api/test.html#contexttodomessage>
    pub fn todo(
        scope: &Scope<'_>,
        this: Object<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let record = record_of(scope, this.to_jsval_throwing(scope)?)?;
        record.set_property(scope, c"todo", true)?;
        record.set_property(
            scope,
            c"message",
            message.unwrap_or(HandleValue::undefined()),
        )
    }

    /// <https://nodejs.org/api/test.html#contextdiagnosticmessage>
    pub fn diagnostic(
        scope: &Scope<'_>,
        this: Object<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        add_diagnostic(scope, this, message.unwrap_or(HandleValue::undefined()))
    }

    /// <https://nodejs.org/api/test.html#contextplancount-options>: the `wait` option is ignored.
    pub fn plan(scope: &Scope<'_>, this: Object<'_>, count: HandleValue<'_>) -> Result<(), Thrown> {
        let record = record_of(scope, this.to_jsval_throwing(scope)?)?;
        if record.get_property(scope, c"plan")?.is_number() {
            return Err(test_failure("cannot set plan more than once").into());
        }
        crate::errors::validate_integer(scope, count, "count", 0.0, 4294967295.0)?;
        record.set_property(scope, c"plan", count)?;
        record.set_property(scope, c"planActual", 0)?;
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#contextrunonlyshouldrunonlytests>
    pub fn run_only(
        scope: &Scope<'_>,
        this: Object<'_>,
        value: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let record = record_of(scope, this.to_jsval_throwing(scope)?)?;
        let value = value.is_some_and(|value| truthy(scope, value));
        record.set_property(scope, c"runOnlySubtests", value)
    }

    /// <https://nodejs.org/api/test.html#contextbeforefn-options>
    pub fn before(
        scope: &Scope<'_>,
        this: Object<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        add_hook(scope, this, "before", callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextafterfn-options>
    pub fn after(
        scope: &Scope<'_>,
        this: Object<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        add_hook(scope, this, "after", callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextbeforeeachfn-options>
    pub fn before_each(
        scope: &Scope<'_>,
        this: Object<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        add_hook(scope, this, "beforeEach", callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextaftereachfn-options>
    pub fn after_each(
        scope: &Scope<'_>,
        this: Object<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        add_hook(scope, this, "afterEach", callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextwaitforcondition-options>: not implemented.
    pub fn wait_for(scope: &Scope<'_>) -> Result<(), ExnThrown> {
        Err(NodeError::not_implemented("t.waitFor").throw(scope))
    }

    /// `t.log()`: not implemented.
    pub fn log(scope: &Scope<'_>) -> Result<(), ExnThrown> {
        Err(NodeError::not_implemented("t.log").throw(scope))
    }

    /// Define the getters.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        define_getters(scope, proto, GETTERS.len())
    }
}

/// Register the hook of `hook_type` of the context `this`'s record.
fn add_hook(
    scope: &Scope<'_>,
    this: Object<'_>,
    hook_type: &str,
    callee: Option<HandleValue<'_>>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let record = record_of(scope, this.to_jsval_throwing(scope)?)?;
    create_hook(
        scope,
        record,
        hook_type,
        callee.unwrap_or(HandleValue::undefined()),
        options.unwrap_or(HandleValue::undefined()),
    )
}

/// The context of a suite, its function's argument:
/// <https://nodejs.org/api/test.html#class-suitecontext>.
#[core_runtime::jsfunction_class(name = "SuiteContext", class_syntax)]
pub mod suite_context {
    use super::*;

    /// Contexts are created for records, by [`hook_context`].
    #[constructor]
    pub fn init(this: Object<'_>) -> Result<(), ExnThrown> {
        let _ = this;
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#contextdiagnosticmessage-1>
    pub fn diagnostic(
        scope: &Scope<'_>,
        this: Object<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        add_diagnostic(scope, this, message.unwrap_or(HandleValue::undefined()))
    }

    /// Define the getters.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        define_getters(scope, proto, SUITE_GETTERS)
    }
}

/// Register the classes of `node:test`.
pub(crate) fn register_classes(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    mock::mock_tracker::register(scope)?;
    mock::mock_function_context::register(scope)?;
    test_context::register(scope)?;
    suite_context::register(scope)?;
    Ok(())
}
