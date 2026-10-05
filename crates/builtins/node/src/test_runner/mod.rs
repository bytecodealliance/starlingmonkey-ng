// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:test`: <https://nodejs.org/api/test.html>.
//!
//! Tests, suites and hooks are [`TestRecord`]s holding their state in Rust, and the realm's
//! [`Harness`] holds the root record and the counts of the run. Tests run one at a time in the
//! order they were started, whatever their `concurrency` option. Tests registered at the top
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

use std::collections::VecDeque;

use js::async_context::Snapshot;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use web_globals::events::algorithms::ScriptStackState;
use web_globals::signals::abort_controller::{AbortController, AbortControllerImpl};

use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::support::rust_string;

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
    /// The next hook in the record's hook queue runs. Once there are none left, the record's
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
    /// The hooks of a test or suite its parent cancelled finished.
    pub const CANCELLED: u8 = 10;
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

/// The types of hooks.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum HookType {
    #[default]
    Before,
    After,
    BeforeEach,
    AfterEach,
}

impl HookType {
    /// The hook type a hook function of `node:test` registers, by its name.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "before" => Some(HookType::Before),
            "after" => Some(HookType::After),
            "beforeEach" => Some(HookType::BeforeEach),
            "afterEach" => Some(HookType::AfterEach),
            _ => None,
        }
    }

    /// The name of the hook type.
    fn name(self) -> &'static str {
        match self {
            HookType::Before => "before",
            HookType::After => "after",
            HookType::BeforeEach => "beforeEach",
            HookType::AfterEach => "afterEach",
        }
    }
}

/// A hook a record runs, with the context it receives.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct QueuedHook {
    hook: Heap<TestRecordImpl>,
    ctx: Heap<Value>,
}

/// A test, a suite, a hook, or the root of the realm's tests.
#[core_runtime::jsclass(hidden)]
pub(crate) struct TestRecord {
    #[no_trace]
    kind: u8,
    #[no_trace]
    name: String,
    /// The depth below the root, which indents the record's report.
    #[no_trace]
    nesting: u32,
    parent: Option<Heap<TestRecordImpl>>,
    subtests: Vec<Heap<TestRecordImpl>>,
    /// The started subtests waiting to run.
    queue: VecDeque<Heap<TestRecordImpl>>,
    /// The subtest running.
    running: Option<Heap<TestRecordImpl>>,
    diagnostics: Vec<Heap<Value>>,
    before: Vec<Heap<TestRecordImpl>>,
    after: Vec<Heap<TestRecordImpl>>,
    before_each: Vec<Heap<TestRecordImpl>>,
    after_each: Vec<Heap<TestRecordImpl>>,
    /// The number of started subtests that haven't finished.
    #[no_trace]
    pending: u32,
    #[no_trace]
    phase: u8,
    /// The number of the current await. Settlements of earlier awaits are ignored.
    #[no_trace]
    token: u64,
    #[no_trace]
    timeout: f64,
    #[no_trace]
    deadline: Option<f64>,
    /// The event loop timer of the deadline.
    #[no_trace]
    timer: Option<u64>,
    /// The promise that settles once the record finished.
    done: Option<Heap<js::promise::Promise>>,
    /// The promise that settles once the record's subtests finished.
    waiting: Option<Heap<js::promise::Promise>>,
    /// The promise of a suite's function that is building it.
    build_promise: Option<Heap<js::object::Object>>,
    callee: Heap<Value>,
    /// The message of `skip` or `todo`.
    message: Heap<Value>,
    error: Heap<Value>,
    /// The `failureType` of Node.js's `ERR_TEST_FAILURE`.
    #[no_trace]
    failure_type: &'static str,
    /// The `only` option, if it is a boolean.
    #[no_trace]
    only: Option<bool>,
    #[no_trace]
    plan_option: Option<f64>,
    #[no_trace]
    plan: Option<f64>,
    #[no_trace]
    plan_actual: f64,
    ctx: Option<Heap<TestContextImpl>>,
    suite_ctx: Option<Heap<SuiteContextImpl>>,
    controller: Option<Heap<AbortControllerImpl>>,
    mock: Option<Heap<mock::MockTrackerImpl>>,
    assert: Option<Heap<js::object::Object>>,
    #[no_trace]
    start_time: Option<f64>,
    #[no_trace]
    end_time: Option<f64>,
    /// The test or suite a hook belongs to.
    owner: Option<Heap<TestRecordImpl>>,
    #[no_trace]
    hook_type: HookType,
    /// Whether a hook didn't run (0), runs (1) or ran (2).
    #[no_trace]
    hook_state: u8,
    /// The context a hook's function receives.
    hook_ctx: Heap<Value>,
    hook_queue: Vec<QueuedHook>,
    #[no_trace]
    hook_index: usize,
    /// The phase that follows the hook queue.
    #[no_trace]
    next: u8,
    current_hook: Option<Heap<TestRecordImpl>>,
    #[no_trace]
    started: bool,
    #[no_trace]
    finished: bool,
    #[no_trace]
    failed: bool,
    #[no_trace]
    cancelled: bool,
    #[no_trace]
    skipped: bool,
    #[no_trace]
    todo: bool,
    #[no_trace]
    expect_failure: bool,
    #[no_trace]
    built: bool,
    #[no_trace]
    building: bool,
    #[no_trace]
    queued: bool,
    /// Whether the `only` options left the record out.
    #[no_trace]
    filtered: bool,
    #[no_trace]
    passed: bool,
    /// Whether the test was skipped before it ran.
    #[no_trace]
    was_skipped: bool,
    /// Whether the record runs its hooks after a failure.
    #[no_trace]
    cleanup: bool,
    #[no_trace]
    skip_after_each: bool,
    /// Whether no subtest was pending for the first time.
    #[no_trace]
    subtests_settled: bool,
    #[no_trace]
    run_only_subtests: bool,
    #[no_trace]
    has_only_tests: bool,
    #[no_trace]
    after_started: bool,
    #[no_trace]
    after_done: bool,
    /// Whether the line of a test with subtests was written.
    #[no_trace]
    announced: bool,
}

#[core_runtime::jsmethods]
impl TestRecord {}

/// The realm's state of the test runner.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Harness {
    /// The root record, once a test or hook was registered.
    root: Option<Heap<TestRecordImpl>>,
    /// The promises of the suites that build.
    builds: Vec<Heap<js::object::Object>>,
    /// The failed records the summary lists.
    failures: Vec<Heap<TestRecordImpl>>,
    /// The counts of finished tests, in the order of [`report::COUNTER_LABELS`].
    #[no_trace]
    counters: [u32; 7],
    #[no_trace]
    success: bool,
    /// Whether the root's subtests haven't started.
    #[no_trace]
    building: bool,
    #[no_trace]
    begun: bool,
    #[no_trace]
    reported: bool,
    #[no_trace]
    exiting: bool,
    /// The number of `beforeExit` events.
    #[no_trace]
    exits: u32,
    /// The functions `t.assert` wraps, by name.
    assertions: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl Harness {}

/// The marker keying the realm's [`Harness`].
struct HarnessObject;

/// The realm's harness.
fn harness<'s>(scope: &'s Scope<'_>) -> Result<Harness<'s>, ExnThrown> {
    js::class::realm_object::<HarnessObject>(scope)
        .and_then(|harness| harness.cast::<Harness>().ok())
        .ok_or_else(|| js::error::TypeError("node:test is not registered".into()).throw(scope))
}

/// The root record, created on first use with the `process` listeners that write the summary and
/// handle the uncaught errors of tests.
fn root<'s>(scope: &'s Scope<'_>) -> Result<TestRecord<'s>, ExnThrown> {
    let harness = harness(scope)?;
    if let Some(root) = harness.data().root.as_ref().map(|root| root.get(scope)) {
        return Ok(root);
    }
    let root = new_record(scope, ROOT, "<root>".into(), None)?;
    {
        let mut data = root.data_mut();
        data.started = true;
        data.start_time = Some(now_ms());
    }
    harness.data_mut().root = Some(Heap::from(root));
    let process = crate::process::process_value(scope)?;
    if let Ok(process) = Object::from_value(scope, process) {
        let listener = Function::new_callback(scope, c"", 0, on_before_exit, ())?;
        let event = js::class::get_or_init_atom(scope, c"beforeExit")?;
        Function::call_by_name(
            scope,
            process.handle(),
            c"on",
            &[event, (*listener).to_jsval_throwing(scope)?],
        )?;
        for (name, event) in [
            ("uncaughtException", c"uncaughtException"),
            ("unhandledRejection", c"unhandledRejection"),
        ] {
            let listener = Function::new_callback(scope, c"", 1, on_uncaught, name)?;
            let event = js::class::get_or_init_atom(scope, event)?;
            Function::call_by_name(
                scope,
                process.handle(),
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
    let record = frame_record(scope)?;
    if let Some(record) = record {
        let (kind, finished, current) = {
            let data = record.data();
            (data.kind, data.finished, data.phase)
        };
        if kind != HOOK && kind != ROOT && !finished {
            fail(scope, record, error, failure_type(&event))?;
            let controller = record
                .data()
                .controller
                .as_ref()
                .map(|controller| controller.get(scope));
            if let Some(controller) = controller {
                controller.abort(scope, HandleValue::undefined(), ScriptStackState::NonEmpty)?;
            }
            if current == phase::SETTLED || current == phase::SUBTESTS {
                record.data_mut().token += 1;
                clear_timer(record);
                advance(scope, record, Outcome::Error(error))?;
            }
            return Ok(js::value::undefined());
        }
    }
    let shown = report::display(scope, error);
    let message = match record {
        Some(record) if record.data().kind != ROOT => {
            let subject = {
                let data = record.data();
                if data.kind == HOOK {
                    format!("Test hook \"{}\"", data.hook_type.name())
                } else {
                    format!("Test \"{}\"", data.name)
                }
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
    let message = js::JSString::from_str(scope, &message)?;
    root.data_mut()
        .diagnostics
        .push(Heap::from(message.as_value()));
    harness(scope)?.data_mut().success = false;
    set_failed_exit_code(scope)
}

/// The failure type of an error the uncaught event `event` reported.
fn failure_type(event: &str) -> &'static str {
    if event == "unhandledRejection" {
        "unhandledRejection"
    } else {
        "uncaughtException"
    }
}

/// Set `process.exitCode` to 1.
fn set_failed_exit_code(scope: &Scope<'_>) -> Result<Value, ExnThrown> {
    let process = crate::process::process_value(scope)?;
    if let Ok(process) = Object::from_value(scope, process) {
        process.set_property(scope, c"exitCode", 1)?;
    }
    Ok(js::value::undefined())
}

/// The monotonic clock, in milliseconds.
pub(crate) fn now_ms() -> f64 {
    platform::clock::monotonic_ns() as f64 / 1e6
}

/// A new record of `kind` named `name`, the child of `parent`. It is not added to `parent`'s
/// subtests.
fn new_record<'s>(
    scope: &'s Scope<'_>,
    kind: u8,
    name: String,
    parent: Option<TestRecord<'_>>,
) -> Result<TestRecord<'s>, ExnThrown> {
    let nesting = match parent {
        Some(parent) => {
            let data = parent.data();
            if data.kind == ROOT {
                0
            } else {
                data.nesting + 1
            }
        }
        None => 0,
    };
    let done = js::Promise::new_pending(scope)?;
    js::class::create_instance_with::<TestRecordImpl>(scope, |_| TestRecordImpl {
        kind,
        name,
        nesting,
        parent: parent.map(Heap::from),
        phase: phase::START,
        timeout: f64::INFINITY,
        done: Some(Heap::from(done)),
        ..Default::default()
    })
}

/// The parent of `record`, if it has one.
fn parent_of<'s>(scope: &'s Scope<'_>, record: TestRecord<'_>) -> Option<TestRecord<'s>> {
    record
        .data()
        .parent
        .as_ref()
        .map(|parent| parent.get(scope))
}

/// Mark `record` failed with `error`, the value the reporter shows, unless it failed already.
/// `failure_type` is the `failureType` of Node.js's `ERR_TEST_FAILURE`.
fn fail(
    _scope: &Scope<'_>,
    record: TestRecord<'_>,
    error: HandleValue<'_>,
    failure_type: &'static str,
) -> Result<(), ExnThrown> {
    let mut data = record.data_mut();
    if data.failed {
        return Ok(());
    }
    data.failed = true;
    data.error.set(error.get());
    data.failure_type = failure_type;
    Ok(())
}

/// [`fail`] with the message `message` as the error.
fn fail_with(
    scope: &Scope<'_>,
    record: TestRecord<'_>,
    message: &str,
    failure_type: &'static str,
) -> Result<(), ExnThrown> {
    let message = js::conversion::ToJSVal::to_jsval_throwing(message, scope)?;
    fail(scope, record, message, failure_type)
}

/// Cancel `record` with the message `message`: mark it failed and cancelled, unless it failed
/// already.
fn cancel(
    scope: &Scope<'_>,
    record: TestRecord<'_>,
    message: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if record.data().failed {
        return Ok(());
    }
    fail(scope, record, message, "cancelledByParent")?;
    record.data_mut().cancelled = true;
    Ok(())
}

/// The message of a test that ran out of time.
fn timeout_message(record: TestRecord<'_>) -> String {
    format!(
        "test timed out after {}ms",
        crate::errors::number_to_string(record.data().timeout)
    )
}

/// The `ERR_TEST_FAILURE` error with `message`.
fn test_failure(message: &str) -> NodeError {
    NodeError::new(ErrorKind::Error, "ERR_TEST_FAILURE", message)
}

// ---------------------------------------------------------------------------
// The async context
// ---------------------------------------------------------------------------

/// The record of the current async context, if it has one. The harness keys it in the frame.
fn frame_record<'s>(scope: &'s Scope<'_>) -> Result<Option<TestRecord<'s>>, ExnThrown> {
    let key = harness(scope)?.as_object();
    Ok(crate::async_hooks::current_store(scope, key)
        .and_then(|record| Object::from_value(scope, record).ok())
        .and_then(|record| record.cast::<TestRecord>().ok()))
}

/// The test or suite whose function, or whose hook's function, is running in the current async
/// context.
fn current_record<'s>(scope: &'s Scope<'_>) -> Result<Option<TestRecord<'s>>, ExnThrown> {
    let Some(record) = frame_record(scope)? else {
        return Ok(None);
    };
    if record.data().kind == HOOK {
        return Ok(record.data().owner.as_ref().map(|owner| owner.get(scope)));
    }
    Ok(Some(record))
}

/// Call `callee` with `this` and `args`, with `record` as the current record of the async context.
fn call_with_record<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
    callee: HandleValue<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<HandleValue<'s>, ExnThrown> {
    let key = harness(scope)?.as_object();
    let value = record.to_jsval_throwing(scope)?;
    let frame = crate::async_hooks::derived_frame(scope, key, Some(value))?;
    let snapshot = Snapshot::of(Some(frame.as_object()));
    crate::async_hooks::call_in(scope, snapshot, callee, this, args)
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// The `TestContext` of `record`, created on first use.
fn context_of<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
) -> Result<TestContext<'s>, ExnThrown> {
    if let Some(ctx) = record.data().ctx.as_ref().map(|ctx| ctx.get(scope)) {
        return Ok(ctx);
    }
    let ctx = js::class::create_instance_with::<TestContextImpl>(scope, |_| TestContextImpl {
        record: Some(Heap::from(record)),
    })?;
    record.data_mut().ctx = Some(Heap::from(ctx));
    Ok(ctx)
}

/// The context the hooks of `record` receive: a `SuiteContext` for a suite, and its `TestContext`
/// otherwise.
fn hook_context<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    if record.data().kind != SUITE {
        return context_of(scope, record)?.to_jsval_throwing(scope);
    }
    if let Some(ctx) = record.data().suite_ctx.as_ref().map(|ctx| ctx.get(scope)) {
        return ctx.to_jsval_throwing(scope);
    }
    let ctx = js::class::create_instance_with::<SuiteContextImpl>(scope, |_| SuiteContextImpl {
        record: Some(Heap::from(record)),
    })?;
    record.data_mut().suite_ctx = Some(Heap::from(ctx));
    ctx.to_jsval_throwing(scope)
}

/// The names of `record` and its ancestors below the root, joined with ` > `.
fn full_name(scope: &Scope<'_>, record: TestRecord<'_>) -> String {
    let mut names = Vec::new();
    let mut current = Some(record);
    while let Some(record) = current {
        if record.data().kind == ROOT {
            break;
        }
        names.push(record.data().name.clone());
        current = parent_of(scope, record);
    }
    names.reverse();
    names.join(" > ")
}

/// `process.argv[1]`, the entry module's path, or `undefined`.
fn entry_file<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let process = crate::process::process_value(scope)?;
    let Ok(process) = Object::from_value(scope, process) else {
        return Ok(HandleValue::undefined());
    };
    let argv = process.get_property(scope, c"argv")?;
    match Object::from_value(scope, argv) {
        Ok(argv) => argv.get_element(scope, 1),
        Err(_) => Ok(HandleValue::undefined()),
    }
}

/// The `AbortSignal` of `record`, created on first use, and aborted once `record` finished.
fn signal_of<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let existing = record
        .data()
        .controller
        .as_ref()
        .map(|controller| controller.get(scope));
    let controller = match existing {
        Some(controller) => controller,
        None => {
            let controller = AbortController::new(scope)?;
            record.data_mut().controller = Some(Heap::from(controller));
            if record.data().finished {
                controller.abort(scope, HandleValue::undefined(), ScriptStackState::NonEmpty)?;
            }
            controller
        }
    };
    controller.signal(scope).to_jsval_throwing(scope)
}

/// The `MockTracker` of `record`, created on first use, and reset once `record` finished.
fn mock_of<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
) -> Result<mock::MockTracker<'s>, ExnThrown> {
    if let Some(tracker) = record
        .data()
        .mock
        .as_ref()
        .map(|tracker| tracker.get(scope))
    {
        return Ok(tracker);
    }
    let tracker = mock::new_tracker(scope)?;
    record.data_mut().mock = Some(Heap::from(tracker));
    Ok(tracker)
}

/// The assertion functions `t.assert` wraps: the realm's map from names to functions, holding the
/// [`ASSERT_METHODS`] of `node:assert` and the functions `assert.register` added.
fn assertions<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let harness = harness(scope)?;
    if let Some(map) = harness.data().assertions.as_ref().map(|map| map.get(scope)) {
        return Ok(map);
    }
    let map = Object::new_with_null_prototype(scope)?;
    if let Some(assert) = core_runtime::module::builtin_namespace(scope, "node:assert")? {
        for &name in ASSERT_METHODS {
            let c_name = std::ffi::CString::new(name).map_err(|_| ExnThrown)?;
            map.set_property_by_name(scope, &c_name, assert.get_property_by_name(scope, &c_name)?)?;
        }
    }
    harness.data_mut().assertions = Some(Heap::from(map));
    Ok(map)
}

/// The payload of [`counted_assertion`]: the test record and the assertion function.
#[core_runtime::jsclass(hidden)]
pub(crate) struct CountedAssertion {
    record: Heap<TestRecordImpl>,
    callee: Heap<Value>,
}

#[core_runtime::jsmethods]
impl CountedAssertion {}

/// The body of a `t.assert` function, whose payload is a [`CountedAssertion`]: count the
/// assertion for the record's plan, then call the function.
fn counted_assertion(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let assertion = cast_payload::<CountedAssertion>(scope, payload);
    let (record, callee) = {
        let data = assertion.data();
        (data.record.get(scope), data.callee.get(scope))
    };
    count_for_plan(record);
    let ctx = context_of(scope, record)?.to_jsval_throwing(scope)?;
    let call_args: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    Ok(Function::call(scope, ctx, callee, &call_args)?.get())
}

/// Count an assertion or a subtest of `record` for its plan, if it has one.
fn count_for_plan(record: TestRecord<'_>) {
    let mut data = record.data_mut();
    if data.plan.is_some() {
        data.plan_actual += 1.0;
    }
}

// ---------------------------------------------------------------------------
// Registering tests and hooks
// ---------------------------------------------------------------------------

/// Whether an option value turns its option on: anything but `undefined` and `false`.
fn option_set(value: HandleValue<'_>) -> bool {
    !value.is_undefined() && !(value.is_boolean() && !value.to_boolean())
}

/// Create the test or suite of `kind` from the arguments `name`, `options` and `callee` of
/// `test()`, `t.test()` or `describe()`, as a subtest of `parent`. `modifier` is the option
/// `test.skip`, `test.todo`, `test.only` or `test.expectFailure` sets, if any.
fn create_test<'s>(
    scope: &'s Scope<'_>,
    parent: TestRecord<'_>,
    kind: u8,
    args: [HandleValue<'_>; 3],
    modifier: Option<&str>,
) -> Result<TestRecord<'s>, Thrown> {
    let [mut name, mut options, mut callee] = args;
    if js::function::is_callable(name) {
        callee = name;
        name = HandleValue::undefined();
        options = HandleValue::undefined();
    } else if name.is_object() {
        callee = options;
        options = name;
        name = HandleValue::undefined();
    } else if js::function::is_callable(options) {
        callee = options;
        options = HandleValue::undefined();
    }
    let options = Object::from_value(scope, options).ok();
    let option = |name: &'static std::ffi::CStr| -> Result<HandleValue<'_>, ExnThrown> {
        match options {
            Some(options) => options.get_property(scope, name),
            None => Ok(HandleValue::undefined()),
        }
    };
    let callee = if js::function::is_callable(callee) {
        callee
    } else {
        HandleValue::undefined()
    };
    let name = match rust_string(scope, name) {
        Some(name) if !name.is_empty() => name,
        _ => {
            let fn_name = match Object::from_value(scope, callee).ok() {
                Some(callee) => callee.get_property(scope, c"name")?,
                None => HandleValue::undefined(),
            };
            match rust_string(scope, fn_name) {
                Some(name) if !name.is_empty() => name,
                _ => "<anonymous>".into(),
            }
        }
    };

    let concurrency = option(c"concurrency")?;
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
    let timeout = option(c"timeout")?;
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
    let signal = option(c"signal")?;
    crate::errors::validate_abort_signal(scope, signal, "options.signal")?;

    let finished = {
        let data = parent.data();
        data.finished || (data.kind == SUITE && data.built)
    };
    let parent = if finished { root(scope)? } else { parent };
    let record = new_record(scope, kind, name, Some(parent))?;

    let skip = match modifier {
        Some("skip") => scope.root_value(js::value::from_bool(true)),
        _ => option(c"skip")?,
    };
    let todo = match modifier {
        Some("todo") => scope.root_value(js::value::from_bool(true)),
        _ => option(c"todo")?,
    };
    let expect_failure = match modifier {
        Some("expectFailure") => true,
        _ => option_set(option(c"expectFailure")?),
    };
    let only = match modifier {
        Some("only") => scope.root_value(js::value::from_bool(true)),
        _ => option(c"only")?,
    };
    let only = only.is_boolean().then(|| only.to_boolean());
    let skipped = option_set(skip);
    let plan = option(c"plan")?;
    let (parent_todo, parent_expect_failure, parent_kind, parent_timeout, parent_started) = {
        let data = parent.data();
        (
            data.todo,
            data.expect_failure,
            data.kind,
            data.timeout,
            data.started,
        )
    };
    {
        let mut data = record.data_mut();
        if !skipped && only != Some(false) {
            data.callee.set(callee.get());
        }
        data.skipped = skipped;
        data.todo = option_set(todo) || parent_todo;
        if skip.is_string() {
            data.message.set(skip.get());
        } else if todo.is_string() {
            data.message.set(todo.get());
        }
        data.expect_failure = expect_failure || parent_expect_failure;
        data.only = only;
        let inherited = if kind == SUITE {
            f64::INFINITY
        } else {
            parent_timeout
        };
        data.timeout = timeout.unwrap_or(inherited);
        data.plan_option = plan.is_number().then(|| plan.to_number());
    }
    if only == Some(true) {
        parent.data_mut().run_only_subtests = true;
        if parent_kind == ROOT || !parent_started {
            let mut current = Some(parent);
            while let Some(ancestor) = current {
                if ancestor.data().has_only_tests {
                    break;
                }
                ancestor.data_mut().has_only_tests = true;
                current = parent_of(scope, ancestor);
            }
        }
    }
    parent.data_mut().subtests.push(Heap::from(record));
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

/// Call the function of the suite `record` with its `SuiteContext`. If it returns a promise, the
/// suite is building until the promise settles. A function that throws or rejects fails the suite.
fn build_suite(scope: &Scope<'_>, record: TestRecord<'_>) -> Result<(), ExnThrown> {
    let callee = record.data().callee.get(scope);
    if !js::function::is_callable(callee) {
        record.data_mut().built = true;
        return Ok(());
    }
    let ctx = hook_context(scope, record)?;
    match call_with_record(scope, record, callee, ctx, &[ctx]) {
        Err(ExnThrown) => {
            let error = js::exception::take_pending_or_undefined(scope);
            fail(scope, record, error, "testCodeFailure")?;
            record.data_mut().built = true;
            Ok(())
        }
        Ok(result) if is_thenable(scope, result)? => {
            record.data_mut().building = true;
            let promise = js::Promise::call_original_resolve(scope, result)?;
            let on_built = Function::new_callback(scope, c"", 1, suite_built, record)?;
            let on_failed = Function::new_callback(scope, c"", 1, suite_build_failed, record)?;
            let built = promise.then(scope, Some(*on_built), Some(*on_failed))?;
            record.data_mut().build_promise = Some(Heap::from(*built));
            harness(scope)?.data_mut().builds.push(Heap::from(*built));
            Ok(())
        }
        Ok(_) => {
            record.data_mut().built = true;
            Ok(())
        }
    }
}

/// Whether `value` is an object with a `then` method.
fn is_thenable(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, value).ok() {
        Some(object) => Ok(js::function::is_callable(
            object.get_property(scope, c"then")?,
        )),
        None => Ok(false),
    }
}

/// The fulfillment reaction to the promise a suite's function returned, whose payload is the
/// suite: finish its building.
fn suite_built(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    finish_build(cast_payload::<TestRecord>(scope, payload));
    Ok(js::value::undefined())
}

/// Mark the suite `record` built.
fn finish_build(record: TestRecord<'_>) {
    let mut data = record.data_mut();
    data.building = false;
    data.built = true;
}

/// The rejection reaction to the promise a suite's function returned, whose payload is the suite:
/// finish its building, and fail it with the reason.
fn suite_build_failed(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let record = cast_payload::<TestRecord>(scope, payload);
    finish_build(record);
    fail(scope, record, args.get(0), "testCodeFailure")?;
    Ok(js::value::undefined())
}

/// The promise that settles once `record` finished.
fn done_of<'s>(scope: &'s Scope<'_>, record: TestRecord<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let done = record.data().done.as_ref().map(|done| done.get(scope));
    match done {
        Some(done) => done.to_jsval_throwing(scope),
        None => Ok(HandleValue::undefined()),
    }
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
    let parent = parent_of(scope, record).ok_or(ExnThrown)?;
    let parent_kind = parent.data().kind;
    match parent_kind {
        SUITE => Ok(
            js::Promise::call_original_resolve(scope, HandleValue::undefined())?
                .to_jsval_throwing(scope)?,
        ),
        ROOT if harness(scope)?.data().building => {
            begin(scope)?;
            Ok(done_of(scope, record)?)
        }
        _ => Ok(start_subtest(scope, record)?),
    }
}

/// Start running the root's subtests in a later job, once the suites registered by then were
/// built. Does nothing after the first call.
fn begin(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let harness = harness(scope)?;
    if std::mem::replace(&mut harness.data_mut().begun, true) {
        return Ok(());
    }
    let ready = js::Promise::call_original_resolve(scope, HandleValue::undefined())?;
    let on_ready = Function::new_callback(scope, c"", 0, wait_for_builds, ())?;
    ready.add_reactions(scope, Some(*on_ready), None)
}

/// Wait for the suites the harness's builds hold to be built, then start the root's subtests.
fn wait_for_builds(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let builds: Vec<Object<'_>> = std::mem::take(&mut harness(scope)?.data_mut().builds)
        .iter()
        .map(|build| build.get(scope))
        .collect();
    let on_built = *Function::new_callback(scope, c"", 0, start_root_tests, ())?;
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

/// The subtests of `record`.
fn subtests_of<'s>(scope: &'s Scope<'_>, record: TestRecord<'_>) -> Vec<TestRecord<'s>> {
    record
        .data()
        .subtests
        .iter()
        .map(|subtest| subtest.get(scope))
        .collect()
}

/// End the harness's building phase, and start the root's subtests that haven't started.
fn start_root_tests(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let harness = harness(scope)?;
    if !std::mem::replace(&mut harness.data_mut().building, false) {
        return Ok(js::value::undefined());
    }
    let root = root(scope)?;
    for record in subtests_of(scope, root) {
        let (queued, finished) = {
            let data = record.data();
            (data.queued, data.finished)
        };
        if !queued && !finished {
            start_subtest(scope, record)?;
        }
    }
    root_check(scope)?;
    Ok(js::value::undefined())
}

/// Whether the `only` options of `record` and its siblings leave it out.
fn filtered_out(record: TestRecord<'_>, parent: TestRecord<'_>) -> bool {
    let data = record.data();
    if data.failed {
        return false;
    }
    if data.only == Some(true) || data.has_only_tests {
        return false;
    }
    let parent = parent.data();
    parent.run_only_subtests || parent.has_only_tests || data.only == Some(false)
}

/// Start `record`: queue it to run once its parent runs no other subtest. Returns the promise that
/// settles once it finished. A test the `only` options leave out finishes without running and
/// without being reported.
fn start_subtest<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let done = done_of(scope, record)?;
    let parent = parent_of(scope, record).ok_or(ExnThrown)?;
    if filtered_out(record, parent) {
        {
            let mut data = record.data_mut();
            data.filtered = true;
            data.finished = true;
            data.passed = true;
        }
        resolve_done(scope, record)?;
        return Ok(done);
    }
    record.data_mut().queued = true;
    {
        let mut data = parent.data_mut();
        data.pending += 1;
        data.queue.push_back(Heap::from(record));
    }
    process_queue(scope, parent)?;
    Ok(done)
}

/// Run the next subtest in the queue of `parent`, if it runs none.
fn process_queue(scope: &Scope<'_>, parent: TestRecord<'_>) -> Result<(), ExnThrown> {
    loop {
        let next = {
            let mut data = parent.data_mut();
            if data.running.is_some() {
                break;
            }
            match pop_front_rooted(scope, &mut data.queue) {
                Some(next) => next,
                None => break,
            }
        };
        if next.data().finished {
            continue;
        }
        parent.data_mut().running = Some(Heap::from(next));
        next.data_mut().phase = phase::START;
        advance(scope, next, Outcome::Value)?;
    }
    Ok(())
}

/// Resolve the promise that settles once `record` finished.
fn resolve_done(scope: &Scope<'_>, record: TestRecord<'_>) -> Result<(), ExnThrown> {
    let done = record.data().done.as_ref().map(|done| done.get(scope));
    if let Some(done) = done {
        done.resolve(scope, HandleValue::undefined())?;
    }
    Ok(())
}

/// Register the hook of `hook_type` that `before()`, `t.after()` or the like was called with, as a
/// hook of `owner`. A `before` hook of a test or suite that started runs right away, and fails the
/// test if it fails.
fn create_hook(
    scope: &Scope<'_>,
    owner: TestRecord<'_>,
    hook_type: HookType,
    callee: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<(), Thrown> {
    let mut timeout = None;
    if let Ok(options) = Object::from_value(scope, options) {
        let value = options.get_property(scope, c"timeout")?;
        if value.is_number() {
            let number = value.to_number();
            if number != f64::INFINITY && !(0.0..=TIMEOUT_MAX).contains(&number) {
                return Err(NodeError::out_of_range(
                    "options.timeout",
                    ">= 0 && <= 2147483647",
                    &crate::errors::number_to_string(number),
                )
                .into());
            }
            timeout = Some(number);
        } else if !value.is_null() && !value.is_undefined() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.timeout",
                "of type number",
                value,
            )
            .into());
        }
    }
    let hook = new_record(scope, HOOK, "<anonymous>".into(), None)?;
    {
        let mut data = hook.data_mut();
        data.owner = Some(Heap::from(owner));
        data.hook_type = hook_type;
        if js::function::is_callable(callee) {
            data.callee.set(callee.get());
        }
        if let Some(timeout) = timeout {
            data.timeout = timeout;
        }
    }
    {
        let mut data = owner.data_mut();
        let hooks = match hook_type {
            HookType::Before => &mut data.before,
            HookType::After => &mut data.after,
            HookType::BeforeEach => &mut data.before_each,
            HookType::AfterEach => &mut data.after_each,
        };
        hooks.push(Heap::from(hook));
    }
    let (started, kind) = {
        let data = owner.data();
        (data.started, data.kind)
    };
    if hook_type == HookType::Before && started {
        let ctx = hook_context(scope, owner)?;
        let done = start_hook(scope, hook, ctx)?;
        if kind != ROOT {
            let early =
                js::class::create_instance_with::<EarlyHookImpl>(scope, |_| EarlyHookImpl {
                    owner: Heap::from(owner),
                    hook: Heap::from(hook),
                })?;
            let on_done = Function::new_callback(scope, c"", 0, early_hook_done, early)?;
            done.add_reactions(scope, Some(*on_done), None)?;
        }
    }
    Ok(())
}

/// The payload of [`early_hook_done`]: a `before` hook that ran as it was registered, and its
/// owner.
#[core_runtime::jsclass(hidden)]
pub(crate) struct EarlyHook {
    owner: Heap<TestRecordImpl>,
    hook: Heap<TestRecordImpl>,
}

#[core_runtime::jsmethods]
impl EarlyHook {}

/// The reaction to a `before` hook that ran as it was registered, whose payload is an
/// [`EarlyHook`]: fail the owner if the hook failed.
fn early_hook_done(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let early = cast_payload::<EarlyHook>(scope, payload);
    let (owner, hook) = {
        let data = early.data();
        (data.owner.get(scope), data.hook.get(scope))
    };
    let error = {
        let data = hook.data();
        data.failed.then(|| data.error.get(scope))
    };
    if let Some(error) = error {
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
    record: TestRecord<'_>,
    value: HandleValue<'_>,
    next: u8,
    timed: bool,
) -> Result<(), ExnThrown> {
    let (token, deadline) = {
        let mut data = record.data_mut();
        data.token += 1;
        data.phase = next;
        (data.token, data.deadline)
    };
    let promise = js::Promise::call_original_resolve(scope, value)?;
    let awaited = js::class::create_instance_with::<AwaitImpl>(scope, |_| AwaitImpl {
        record: Heap::from(record),
        token,
    })?;
    let on_fulfilled = Function::new_callback(scope, c"", 1, resume_fulfilled, awaited)?;
    let on_rejected = Function::new_callback(scope, c"", 1, resume_rejected, awaited)?;
    promise.add_reactions(scope, Some(*on_fulfilled), Some(*on_rejected))?;
    if let Some(deadline) = deadline.filter(|deadline| timed && deadline.is_finite()) {
        let delay = (deadline - now_ms()).max(0.0);
        let on_timeout = Function::new_callback(scope, c"", 0, resume_timeout, awaited)?;
        let id = core_runtime::event_loop::timer::start_timer(
            scope,
            on_timeout.as_callable(),
            HandleValue::undefined(),
            &[],
            std::time::Duration::from_micros((delay * 1000.0) as u64),
            false,
            None,
        );
        record.data_mut().timer = id;
    }
    Ok(())
}

/// Cancel the timer of `record`'s deadline, if it has one.
fn clear_timer(record: TestRecord<'_>) {
    if let Some(id) = record.data_mut().timer.take() {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
    }
}

/// The payload of the reactions and the timer callback of an await: the record, and the await's
/// token.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Await {
    record: Heap<TestRecordImpl>,
    #[no_trace]
    token: u64,
}

#[core_runtime::jsmethods]
impl Await {}

/// If the await whose payload is `payload` is its record's current await, consume it, cancel its
/// timer, and [`advance`] the record with `outcome`. Otherwise, do nothing.
fn resume(
    scope: &Scope<'_>,
    payload: HandleValue<'_>,
    outcome: Outcome<'_>,
) -> Result<Value, ExnThrown> {
    let awaited = cast_payload::<Await>(scope, payload);
    let (record, token) = {
        let data = awaited.data();
        (data.record.get(scope), data.token)
    };
    {
        let mut data = record.data_mut();
        if data.token != token {
            return Ok(js::value::undefined());
        }
        data.token += 1;
    }
    clear_timer(record);
    if advance(scope, record, outcome).is_err() {
        core_runtime::event_loop::handle_and_clear_exception(scope);
    }
    Ok(js::value::undefined())
}

/// The fulfillment reaction to an awaited value, whose payload is an [`Await`].
fn resume_fulfilled(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    resume(scope, payload, Outcome::Value)
}

/// The rejection reaction to an awaited value, whose payload is an [`Await`].
fn resume_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    resume(scope, payload, Outcome::Error(args.get(0)))
}

/// The timer callback of a record's deadline, whose payload is an [`Await`].
fn resume_timeout(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    resume(scope, payload, Outcome::Timeout)
}

/// Set the deadline of `record` from its timeout.
fn set_deadline(record: TestRecord<'_>) {
    let mut data = record.data_mut();
    if data.timeout.is_finite() {
        data.deadline = Some(now_ms() + data.timeout);
    }
}

/// Mark `record` started.
fn mark_started(record: TestRecord<'_>) {
    let mut data = record.data_mut();
    data.started = true;
    data.start_time = Some(now_ms());
}

/// The hooks to run in turn, with the context each receives.
type HookList<'s> = Vec<(TestRecord<'s>, HandleValue<'s>)>;

/// Append the hooks of `hook_type` of `owner` to `list`, each receiving `ctx`.
fn add_hooks<'s>(
    scope: &'s Scope<'_>,
    list: &mut HookList<'s>,
    owner: TestRecord<'_>,
    hook_type: HookType,
    ctx: HandleValue<'s>,
) {
    let data = owner.data();
    let hooks = match hook_type {
        HookType::Before => &data.before,
        HookType::After => &data.after,
        HookType::BeforeEach => &data.before_each,
        HookType::AfterEach => &data.after_each,
    };
    list.extend(hooks.iter().map(|hook| (hook.get(scope), ctx)));
}

/// The ancestors of `record`, starting with its parent.
fn ancestors<'s>(scope: &'s Scope<'_>, record: TestRecord<'_>) -> Vec<TestRecord<'s>> {
    let mut list = Vec::new();
    let mut current = parent_of(scope, record);
    while let Some(ancestor) = current {
        current = parent_of(scope, ancestor);
        list.push(ancestor);
    }
    list
}

/// The hooks that run after the function of the test `record`, or after it failed: the
/// `afterEach` hooks of its ancestors, innermost first, unless it was skipped before it ran, then
/// its own `after` hooks.
fn after_hooks<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
) -> Result<HookList<'s>, ExnThrown> {
    let mut list = Vec::new();
    let ctx = context_of(scope, record)?.to_jsval_throwing(scope)?;
    if !record.data().was_skipped {
        for ancestor in ancestors(scope, record) {
            add_hooks(scope, &mut list, ancestor, HookType::AfterEach, ctx);
        }
    }
    add_hooks(scope, &mut list, record, HookType::After, ctx);
    Ok(list)
}

/// The `after` hooks of the suite or root `record`.
fn own_after_hooks<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
) -> Result<HookList<'s>, ExnThrown> {
    let mut list = Vec::new();
    let ctx = hook_context(scope, record)?;
    add_hooks(scope, &mut list, record, HookType::After, ctx);
    Ok(list)
}

/// Make `list` the hooks `record` runs next, followed by the phase `next`.
fn queue_hooks(record: TestRecord<'_>, list: HookList<'_>, next: u8) {
    let mut data = record.data_mut();
    data.hook_queue = list
        .into_iter()
        .map(|(hook, ctx)| QueuedHook {
            hook: Heap::from(hook),
            ctx: Heap::from(ctx.get()),
        })
        .collect();
    data.hook_index = 0;
    data.next = next;
    data.phase = phase::HOOKS;
}

/// Run the hooks that follow the failure of the test `record`, ignoring their failures, then
/// finish it.
fn clean_up(scope: &Scope<'_>, record: TestRecord<'_>) -> Result<(), ExnThrown> {
    record.data_mut().cleanup = true;
    let list = after_hooks(scope, record)?;
    queue_hooks(record, list, phase::POST);
    Ok(())
}

/// Run `hook` with `ctx`, unless it is a `before` or `after` hook that ran or runs already.
/// Returns the promise that resolves once it finished.
fn start_hook<'s>(
    scope: &'s Scope<'_>,
    hook: TestRecord<'_>,
    ctx: HandleValue<'_>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let existing = {
        let data = hook.data();
        let once = matches!(data.hook_type, HookType::Before | HookType::After);
        if once && data.hook_state != 0 {
            data.done.as_ref().map(|done| done.get(scope))
        } else {
            None
        }
    };
    if let Some(done) = existing {
        return Ok(done);
    }
    let done = js::Promise::new_pending(scope)?;
    {
        let mut data = hook.data_mut();
        data.hook_state = 1;
        data.failed = false;
        data.error.set(js::value::undefined());
        data.hook_ctx.set(ctx.get());
        data.done = Some(Heap::from(done));
        data.phase = phase::CALL;
    }
    advance(scope, hook, Outcome::Value)?;
    Ok(done)
}

/// Handle the failure of `hook`, which `record` ran. Before the record's function, it fails the
/// record, whose function doesn't run. After it, it fails the record, and its remaining
/// `afterEach` hooks don't run. Hooks that fail while the record cleans up after a failure are
/// ignored.
fn hook_failed(
    scope: &Scope<'_>,
    record: TestRecord<'_>,
    hook: TestRecord<'_>,
) -> Result<(), ExnThrown> {
    let error = hook.data().error.get(scope);
    let (kind, cleanup, next) = {
        let data = record.data();
        (data.kind, data.cleanup, data.next)
    };
    if kind == ROOT {
        return report::report_root_hook_failure(scope, error);
    }
    if cleanup {
        return Ok(());
    }
    fail(scope, record, error, "hookFailed")?;
    record.data_mut().cleanup = true;
    if next == phase::CALL {
        let list = after_hooks(scope, record)?;
        queue_hooks(record, list, phase::POST);
    } else if next == phase::SUITE_RUN {
        let list = own_after_hooks(scope, record)?;
        queue_hooks(record, list, phase::POST);
    } else {
        record.data_mut().skip_after_each = true;
    }
    Ok(())
}

/// Call the function of the test or hook `record` with `ctx`, and with a callback as a second
/// argument if the function declares two parameters. Returns the value to await: the function's
/// return value, a promise the callback settles, or a promise rejected with the error the
/// function threw.
fn call_body<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
    ctx: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let callee = record.data().callee.get(scope);
    let Some(function) = Object::from_value(scope, callee)
        .ok()
        .filter(|callee| callee.is_callable())
    else {
        return Ok(HandleValue::undefined());
    };
    let declared = function.get_property(scope, c"length")?;
    if declared.is_number() && declared.to_number() == 2.0 {
        let settled = js::Promise::new_pending(scope)?;
        let callback =
            js::class::create_instance_with::<DoneCallbackImpl>(scope, |_| DoneCallbackImpl {
                promise: Heap::from(settled),
                called: false,
            })?;
        let done = Function::new_callback(scope, c"", 1, callback_done, callback)?;
        let done = done.to_jsval_throwing(scope)?;
        return match call_with_record(scope, record, callee, ctx, &[ctx, done]) {
            Err(ExnThrown) => rejected_with_exception(scope),
            Ok(result) => {
                let is_promise = Object::from_value(scope, result)
                    .is_ok_and(|object| js::Promise::is_promise(object.handle()));
                if is_promise {
                    fail_with(
                        scope,
                        record,
                        "passed a callback but also returned a Promise",
                        "callbackAndPromisePresent",
                    )?;
                    Ok(result)
                } else {
                    settled.to_jsval_throwing(scope)
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
    let error = js::exception::take_pending_or_undefined(scope);
    js::Promise::call_original_reject(scope, error)?.to_jsval_throwing(scope)
}

/// The payload of [`callback_done`]: the promise it settles, and whether it was called.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DoneCallback {
    promise: Heap<js::promise::Promise>,
    #[no_trace]
    called: bool,
}

#[core_runtime::jsmethods]
impl DoneCallback {}

/// The callback a test function that declares two parameters receives, whose payload is a
/// [`DoneCallback`]: reject the promise with a truthy first argument, and resolve it otherwise.
fn callback_done(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let callback = cast_payload::<DoneCallback>(scope, payload);
    if std::mem::replace(&mut callback.data_mut().called, true) {
        return Err(test_failure("callback invoked multiple times").throw(scope));
    }
    let promise = callback.data().promise.get(scope);
    let error = args.get(0);
    if js::conversion::to_boolean(error) {
        promise.reject(scope, error)?;
    } else {
        promise.resolve(scope, HandleValue::undefined())?;
    }
    Ok(js::value::undefined())
}

/// Take the steps of running `record` from its `phase`, until it awaits a value or finishes.
/// `outcome` describes how the value the record awaited settled.
fn advance(
    scope: &Scope<'_>,
    record: TestRecord<'_>,
    outcome: Outcome<'_>,
) -> Result<(), ExnThrown> {
    let mut outcome = Some(outcome);
    let kind = record.data().kind;
    loop {
        let outcome = outcome.take().unwrap_or(Outcome::Value);
        let current = record.data().phase;
        match current {
            phase::START if kind == SUITE => {
                mark_started(record);
                let build = {
                    let data = record.data();
                    if data.building {
                        data.build_promise.as_ref().map(|build| build.get(scope))
                    } else {
                        None
                    }
                };
                if let Some(build) = build {
                    let build = build.to_jsval_throwing(scope)?;
                    return await_value(scope, record, build, phase::BUILT, false);
                }
                record.data_mut().phase = phase::BUILT;
            }
            phase::START if kind == TEST => {
                mark_started(record);
                let skipped = {
                    let mut data = record.data_mut();
                    if data.plan.is_none() {
                        if let Some(plan) = data.plan_option {
                            data.plan = Some(plan);
                            data.plan_actual = 0.0;
                        }
                    }
                    data.was_skipped = data.skipped;
                    data.skipped
                };
                let mut list = Vec::new();
                if let Some(parent) = parent_of(scope, record) {
                    let parent_ctx = hook_context(scope, parent)?;
                    add_hooks(scope, &mut list, parent, HookType::Before, parent_ctx);
                    if !skipped {
                        let ctx = context_of(scope, record)?.to_jsval_throwing(scope)?;
                        let mut chain = ancestors(scope, record);
                        chain.reverse();
                        for ancestor in chain {
                            add_hooks(scope, &mut list, ancestor, HookType::BeforeEach, ctx);
                        }
                    }
                }
                queue_hooks(record, list, phase::CALL);
            }
            phase::HOOKS => {
                let finished_hook = record
                    .data_mut()
                    .current_hook
                    .take()
                    .map(|hook| hook.get(scope));
                if let Some(hook) = finished_hook {
                    if hook.data().failed {
                        hook_failed(scope, record, hook)?;
                        continue;
                    }
                }
                let queued = {
                    let mut data = record.data_mut();
                    match data.hook_queue.get(data.hook_index) {
                        Some(queued) => {
                            let entry = (queued.hook.get(scope), queued.ctx.get(scope));
                            data.hook_index += 1;
                            Some(entry)
                        }
                        None => {
                            data.phase = data.next;
                            None
                        }
                    }
                };
                let Some((hook, ctx)) = queued else {
                    continue;
                };
                let skip =
                    record.data().skip_after_each && hook.data().hook_type == HookType::AfterEach;
                if skip {
                    continue;
                }
                let done = start_hook(scope, hook, ctx)?;
                record.data_mut().current_hook = Some(Heap::from(hook));
                return await_value(
                    scope,
                    record,
                    done.to_jsval_throwing(scope)?,
                    phase::HOOKS,
                    false,
                );
            }
            phase::CALL => {
                set_deadline(record);
                if kind == TEST && record.data().skipped {
                    return await_value(
                        scope,
                        record,
                        HandleValue::undefined(),
                        phase::SETTLED,
                        false,
                    );
                }
                let ctx = if kind == HOOK {
                    record.data().hook_ctx.get(scope)
                } else {
                    context_of(scope, record)?.to_jsval_throwing(scope)?
                };
                let result = call_body(scope, record, ctx)?;
                return await_value(scope, record, result, phase::SETTLED, true);
            }
            phase::SETTLED if kind == HOOK => {
                match outcome {
                    Outcome::Error(error) => {
                        let mut data = record.data_mut();
                        data.failed = true;
                        data.error.set(error.get());
                    }
                    Outcome::Timeout => {
                        let message = timeout_message(record);
                        let message = js::JSString::from_str(scope, &message)?;
                        let mut data = record.data_mut();
                        data.failed = true;
                        data.error.set(message.as_value());
                    }
                    Outcome::Value => {}
                }
                record.data_mut().hook_state = 2;
                return resolve_done(scope, record);
            }
            phase::SETTLED => match outcome {
                Outcome::Error(error) => {
                    fail(scope, record, error, "testCodeFailure")?;
                    clean_up(scope, record)?;
                }
                Outcome::Timeout => {
                    let message = js::conversion::ToJSVal::to_jsval_throwing(
                        &timeout_message(record),
                        scope,
                    )?;
                    cancel(scope, record, message)?;
                    clean_up(scope, record)?;
                }
                Outcome::Value => {
                    // As in Node.js, a test waits for its subtests only until none of them is
                    // pending for the first time. Subtests started after that are cancelled if
                    // they haven't finished when the test does.
                    let wait = {
                        let data = record.data();
                        data.pending > 0 && !data.subtests_settled
                    };
                    if wait {
                        return wait_for_subtests(scope, record, phase::SUBTESTS);
                    }
                    record.data_mut().phase = phase::SUBTESTS;
                }
            },
            phase::SUBTESTS => {
                match outcome {
                    Outcome::Timeout => {
                        let message = js::conversion::ToJSVal::to_jsval_throwing(
                            &timeout_message(record),
                            scope,
                        )?;
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
                let (plan, actual) = {
                    let data = record.data();
                    (data.plan, data.plan_actual)
                };
                if let Some(plan) = plan.filter(|plan| *plan != actual) {
                    let message = format!(
                        "plan expected {} assertions but received {}",
                        crate::errors::number_to_string(plan),
                        crate::errors::number_to_string(actual)
                    );
                    fail_with(scope, record, &message, "testCodeFailure")?;
                    clean_up(scope, record)?;
                    continue;
                }
                let list = after_hooks(scope, record)?;
                queue_hooks(record, list, phase::POST);
            }
            phase::BUILT => {
                let mut list = Vec::new();
                if let Some(parent) = parent_of(scope, record) {
                    let parent_ctx = hook_context(scope, parent)?;
                    add_hooks(scope, &mut list, parent, HookType::Before, parent_ctx);
                }
                let ctx = hook_context(scope, record)?;
                add_hooks(scope, &mut list, record, HookType::Before, ctx);
                queue_hooks(record, list, phase::SUITE_RUN);
            }
            phase::SUITE_RUN => {
                set_deadline(record);
                let (skipped, failed) = {
                    let data = record.data();
                    (data.skipped, data.failed)
                };
                if !skipped && !failed {
                    for subtest in subtests_of(scope, record) {
                        let (queued, finished) = {
                            let data = subtest.data();
                            (data.queued, data.finished)
                        };
                        if !queued && !finished {
                            start_subtest(scope, subtest)?;
                        }
                    }
                }
                if record.data().pending > 0 {
                    return wait_for_subtests(scope, record, phase::SUITE_DONE);
                }
                record.data_mut().phase = phase::SUITE_DONE;
            }
            phase::SUITE_DONE => {
                if let Outcome::Timeout = outcome {
                    let message = js::conversion::ToJSVal::to_jsval_throwing(
                        &timeout_message(record),
                        scope,
                    )?;
                    cancel(scope, record, message)?;
                }
                {
                    let mut data = record.data_mut();
                    data.cleanup = data.failed;
                }
                let list = own_after_hooks(scope, record)?;
                queue_hooks(record, list, phase::POST);
            }
            phase::POST => return post_run(scope, record, None),
            phase::ROOT_DONE => {
                record.data_mut().after_done = true;
                if harness(scope)?.data().exiting {
                    return summarize(scope);
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

/// Run the hooks that follow the test or suite `record` after its parent cancelled and finished
/// it while it ran, as `clean_up` runs them after a failure, in a later job. A record that was
/// running these hooks already runs the rest of them. `record` isn't finished or reported again.
fn run_hooks_after_cancel(scope: &Scope<'_>, record: TestRecord<'_>) -> Result<(), ExnThrown> {
    let (kind, current, next) = {
        let data = record.data();
        (data.kind, data.phase, data.next)
    };
    if kind != TEST && kind != SUITE {
        return Ok(());
    }
    record.data_mut().cleanup = true;
    if current == phase::HOOKS && next == phase::POST {
        record.data_mut().next = phase::CANCELLED;
        let running = record.data().current_hook.as_ref().and_then(|hook| {
            hook.get(scope)
                .data()
                .done
                .as_ref()
                .map(|done| done.get(scope))
        });
        let running = match running {
            Some(done) => done.to_jsval_throwing(scope)?,
            None => HandleValue::undefined(),
        };
        return await_value(scope, record, running, phase::HOOKS, false);
    }
    let list = if kind == TEST {
        after_hooks(scope, record)?
    } else {
        own_after_hooks(scope, record)?
    };
    queue_hooks(record, list, phase::CANCELLED);
    await_value(scope, record, HandleValue::undefined(), phase::HOOKS, false)
}

/// Await the end of the subtests of `record`, then advance it from the phase `next`.
fn wait_for_subtests(scope: &Scope<'_>, record: TestRecord<'_>, next: u8) -> Result<(), ExnThrown> {
    let waiting = js::Promise::new_pending(scope)?;
    record.data_mut().waiting = Some(Heap::from(waiting));
    await_value(scope, record, waiting.to_jsval_throwing(scope)?, next, true)
}

/// Finish `record`: cancel its subtests that haven't finished, with `cancel_error` or the message
/// that they didn't finish before their parent, fail it if a subtest failed, report it, and run
/// the next subtest of its parent.
fn post_run(
    scope: &Scope<'_>,
    record: TestRecord<'_>,
    cancel_error: Option<HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    record.data_mut().token += 1;
    clear_timer(record);
    {
        let now = now_ms();
        let mut data = record.data_mut();
        data.end_time.get_or_insert(now);
        data.start_time.get_or_insert(now);
        data.queue.clear();
    }
    let message = match cancel_error {
        Some(error) => error,
        None => js::class::get_or_init_atom(
            scope,
            c"test did not finish before its parent and was cancelled",
        )?,
    };
    let mut failed = 0;
    for subtest in subtests_of(scope, record) {
        if !subtest.data().finished {
            cancel(scope, subtest, message)?;
            let started = subtest.data().started;
            post_run(scope, subtest, cancel_error)?;
            if started {
                run_hooks_after_cancel(scope, subtest)?;
            }
        }
        let data = subtest.data();
        if !data.passed && !data.todo {
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
    let (expect_failure, failed, skipped) = {
        let data = record.data();
        (data.expect_failure, data.failed, data.skipped)
    };
    if !failed && expect_failure && !skipped {
        fail_with(
            scope,
            record,
            "test was expected to fail but passed",
            "expectedFailure",
        )?;
        record.data_mut().passed = false;
    } else {
        record.data_mut().passed = !failed || expect_failure;
    }
    let tracker = record
        .data()
        .mock
        .as_ref()
        .map(|tracker| tracker.get(scope));
    if let Some(tracker) = tracker {
        tracker.reset(scope)?;
    }
    record.data_mut().finished = true;
    let controller = record
        .data()
        .controller
        .as_ref()
        .map(|controller| controller.get(scope));
    if let Some(controller) = controller {
        controller.abort(scope, HandleValue::undefined(), ScriptStackState::NonEmpty)?;
    }
    report::report_test(scope, record)?;
    resolve_done(scope, record)?;

    let Some(parent) = parent_of(scope, record) else {
        return Ok(());
    };
    if record.data().queued {
        let waiting = {
            let mut data = parent.data_mut();
            data.pending = data.pending.saturating_sub(1);
            if data.pending == 0 {
                data.subtests_settled = true;
                data.waiting.take_rooted(scope)
            } else {
                None
            }
        };
        if let Some(waiting) = waiting {
            waiting.resolve(scope, HandleValue::undefined())?;
        }
    }
    {
        let mut data = parent.data_mut();
        let running = data.running.as_ref().is_some_and(|running| {
            running.get(scope).as_object().as_raw() == record.as_object().as_raw()
        });
        if running {
            data.running = None;
        }
    }
    process_queue(scope, parent)?;
    if parent.data().kind == ROOT {
        root_check(scope)?;
    }
    Ok(())
}

/// Run the root's `after` hooks once its subtests all finished, unless they ran already.
fn root_check(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let building = harness(scope)?.data().building;
    let root = root(scope)?;
    {
        let mut data = root.data_mut();
        if building || data.after_started || data.pending > 0 {
            return Ok(());
        }
        data.after_started = true;
    }
    let list = own_after_hooks(scope, root)?;
    queue_hooks(root, list, phase::ROOT_DONE);
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
    let exits = {
        let mut data = harness.data_mut();
        if data.reported {
            return Ok(js::value::undefined());
        }
        data.exits += 1;
        data.building = false;
        data.exits
    };
    let root = root(scope)?;
    root.data_mut().queue.clear();
    let message = js::class::get_or_init_atom(
        scope,
        c"Promise resolution is still pending but the event loop has already resolved",
    )?;
    for record in subtests_of(scope, root) {
        if !record.data().finished {
            cancel(scope, record, message)?;
            post_run(scope, record, Some(message))?;
        }
    }
    root_check(scope)?;
    if root.data().after_done || exits > 1 {
        summarize(scope)?;
    } else {
        harness.data_mut().exiting = true;
    }
    Ok(js::value::undefined())
}

/// Write the summary, once, and set `process.exitCode` to 1 if a test failed.
fn summarize(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let harness = harness(scope)?;
    if std::mem::replace(&mut harness.data_mut().reported, true) {
        return Ok(());
    }
    let root = root(scope)?;
    root.data_mut().end_time = Some(now_ms());
    report::summary(scope, harness, root)?;
    if !harness.data().success {
        set_failed_exit_code(scope)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

/// The payload of [`register_entry`]: the kind of record, and the variant's name, if any.
#[core_runtime::jsclass(hidden)]
pub(crate) struct EntryVariant {
    #[no_trace]
    kind: u8,
    #[no_trace]
    modifier: Option<&'static str>,
}

#[core_runtime::jsmethods]
impl EntryVariant {}

/// The body of `test()`, `describe()` and their `skip`, `todo`, `only` and `expectFailure`
/// variants, whose payload is an [`EntryVariant`].
fn register_entry(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let variant = cast_payload::<EntryVariant>(scope, payload);
    let (kind, modifier) = {
        let data = variant.data();
        (data.kind, data.modifier)
    };
    let result = register(
        scope,
        kind,
        [args.get(0), args.get(1), args.get(2)],
        modifier,
    )
    .map_err(|error| error.throw(scope))?;
    Ok(result.get())
}

/// The body of `before()`, `after()`, `beforeEach()` and `afterEach()`, whose payload is the hook
/// type's name.
fn register_hook(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let hook_type = String::from_jsval(scope, payload, ()).map_err(|e| e.throw(scope))?;
    let hook_type = HookType::from_name(&hook_type).ok_or(ExnThrown)?;
    let owner = match current_record(scope)? {
        Some(owner) => owner,
        None => root(scope)?,
    };
    create_hook(scope, owner, hook_type, args.get(0), args.get(1))
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
        Some(record) if record.data().kind != ROOT => {
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
    let entry_variant = |modifier| {
        js::class::create_instance_with::<EntryVariantImpl>(scope, |_| EntryVariantImpl {
            kind,
            modifier,
        })
    };
    let function = *Function::new_callback(scope, name, 3, register_entry, entry_variant(None)?)?;
    for (variant, c_variant) in [
        ("skip", c"skip"),
        ("todo", c"todo"),
        ("only", c"only"),
        ("expectFailure", c"expectFailure"),
    ] {
        let payload = entry_variant(Some(variant))?;
        let variant_function =
            Function::new_callback(scope, c_variant, 3, register_entry, payload)?;
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
            let kind = crate::stub::ExportKind::Function(1);
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

/// The record of a context, or the `TypeError` of a context created with `new`, which has none.
fn record_of<'s>(
    scope: &'s Scope<'_>,
    record: &Option<Heap<TestRecordImpl>>,
) -> Result<TestRecord<'s>, ExnThrown> {
    record
        .as_ref()
        .map(|record| record.get(scope))
        .ok_or_else(|| {
            js::error::TypeError("Cannot read properties of undefined".into()).throw(scope)
        })
}

/// A [`counted_assertion`] function of `record` that calls `callee`.
fn counted_assertion_function<'s>(
    scope: &'s Scope<'_>,
    record: TestRecord<'_>,
    callee: HandleValue<'_>,
) -> Result<Function<'s>, ExnThrown> {
    let assertion =
        js::class::create_instance_with::<CountedAssertionImpl>(scope, |_| CountedAssertionImpl {
            record: Heap::from(record),
            callee: Heap::from(callee.get()),
        })?;
    Function::new_callback(scope, c"", 0, counted_assertion, assertion)
}

/// `t.assert` of the test `record`, created on first use: the functions [`assertions`] holds, and
/// `node:assert`'s `ok` unless it holds one, each counting towards the plan.
fn assert_of<'s>(scope: &'s Scope<'_>, record: TestRecord<'_>) -> Result<Object<'s>, ExnThrown> {
    if let Some(assert) = record
        .data()
        .assert
        .as_ref()
        .map(|assert| assert.get(scope))
    {
        return Ok(assert);
    }
    let assert = Object::new_with_null_prototype(scope)?;
    let map = assertions(scope)?;
    let mut has_ok = false;
    for key in map.own_keys(scope)? {
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        let callee = map.get_property_by_id(scope, id)?;
        let wrapper = counted_assertion_function(scope, record, callee)?;
        assert.set_property_by_id(scope, id, *wrapper)?;
        has_ok |= crate::support::is_str(scope, key, c"ok")?;
    }
    if !has_ok {
        if let Some(node_assert) = core_runtime::module::builtin_namespace(scope, "node:assert")? {
            let ok = node_assert.get_property(scope, c"ok")?;
            let wrapper = counted_assertion_function(scope, record, ok)?;
            assert.set_property(scope, c"ok", *wrapper)?;
        }
    }
    record.data_mut().assert = Some(Heap::from(assert));
    Ok(assert)
}

/// Add `message` to the diagnostics of `record`.
fn add_diagnostic(record: TestRecord<'_>, message: Option<HandleValue<'_>>) {
    let message = message.unwrap_or(HandleValue::undefined());
    record
        .data_mut()
        .diagnostics
        .push(Heap::from(message.get()));
}

/// The rooted front element of `queue`, which is removed from it.
#[js::allow_unrooted]
fn pop_front_rooted<'s>(
    scope: &'s Scope<'_>,
    queue: &mut VecDeque<Heap<TestRecordImpl>>,
) -> Option<TestRecord<'s>> {
    queue.pop_front().map(|record| record.get(scope))
}

/// The context of a test, its function's first argument:
/// <https://nodejs.org/api/test.html#class-testcontext>.
#[core_runtime::jsclass(name = "TestContext", hidden)]
pub struct TestContext {
    record: Option<Heap<TestRecordImpl>>,
}

#[core_runtime::jsmethods(node_args)]
impl TestContext {
    /// A context without a test, whose members throw, as `new TestContext()` creates it in
    /// Node.js.
    #[constructor]
    pub fn create() -> Self {
        TestContextImpl { record: None }
    }

    /// <https://nodejs.org/api/test.html#contextsignal>
    #[getter]
    pub fn signal<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        signal_of(scope, record_of(scope, &self.data().record)?)
    }

    /// <https://nodejs.org/api/test.html#contextname>
    #[getter]
    pub fn name(&self, scope: &Scope<'_>) -> Result<String, ExnThrown> {
        Ok(record_of(scope, &self.data().record)?.data().name.clone())
    }

    /// <https://nodejs.org/api/test.html#contextfilepath>
    #[getter]
    pub fn file_path<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        record_of(scope, &self.data().record)?;
        entry_file(scope)
    }

    /// <https://nodejs.org/api/test.html#contextfullname>
    #[getter]
    pub fn full_name(&self, scope: &Scope<'_>) -> Result<String, ExnThrown> {
        Ok(full_name(scope, record_of(scope, &self.data().record)?))
    }

    /// <https://nodejs.org/api/test.html#contexterror>: the error the test failed with, or `null`.
    #[getter]
    pub fn error<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let record = record_of(scope, &self.data().record)?;
        let data = record.data();
        Ok(if data.failed {
            data.error.get(scope)
        } else {
            HandleValue::null()
        })
    }

    /// <https://nodejs.org/api/test.html#contextpassed>
    #[getter]
    pub fn passed(&self, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        Ok(record_of(scope, &self.data().record)?.data().passed)
    }

    /// <https://nodejs.org/api/test.html#contextattempt>: always 0.
    #[getter]
    pub fn attempt(&self, scope: &Scope<'_>) -> Result<f64, ExnThrown> {
        record_of(scope, &self.data().record)?;
        Ok(0.0)
    }

    /// <https://nodejs.org/api/test.html#contextworkerid>: tests don't run in workers.
    #[getter]
    pub fn worker_id<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        record_of(scope, &self.data().record)?;
        Ok(HandleValue::undefined())
    }

    /// <https://nodejs.org/api/test.html#contextassert>
    #[getter]
    pub fn assert<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        assert_of(scope, record_of(scope, &self.data().record)?)
    }

    /// <https://nodejs.org/api/test.html#contextmock>
    #[getter]
    pub fn mock<'r>(&self, scope: &'r Scope<'_>) -> Result<mock::MockTracker<'r>, ExnThrown> {
        mock_of(scope, record_of(scope, &self.data().record)?)
    }

    /// <https://nodejs.org/api/test.html#contextdiagnosticmessage>
    #[method]
    pub fn diagnostic(
        &self,
        scope: &Scope<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        add_diagnostic(record_of(scope, &self.data().record)?, message);
        Ok(())
    }

    /// `t.log()`: not implemented.
    #[method]
    pub fn log(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        Err(NodeError::not_implemented("t.log").throw(scope))
    }

    /// <https://nodejs.org/api/test.html#contextplancount-options>: the `wait` option is ignored.
    #[method]
    pub fn plan(&self, scope: &Scope<'_>, count: HandleValue<'_>) -> Result<(), Thrown> {
        let record = record_of(scope, &self.data().record)?;
        if record.data().plan.is_some() {
            return Err(test_failure("cannot set plan more than once").into());
        }
        let count = crate::errors::validate_integer(scope, count, "count", 0.0, 4294967295.0)?;
        let mut data = record.data_mut();
        data.plan = Some(count);
        data.plan_actual = 0.0;
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#contextrunonlyshouldrunonlytests>
    #[method]
    pub fn run_only(
        &self,
        scope: &Scope<'_>,
        value: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let record = record_of(scope, &self.data().record)?;
        record.data_mut().run_only_subtests = value.is_some_and(js::conversion::to_boolean);
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#contextskipmessage>
    #[method]
    pub fn skip(
        &self,
        scope: &Scope<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let record = record_of(scope, &self.data().record)?;
        let mut data = record.data_mut();
        data.skipped = true;
        data.message
            .set(message.map_or(js::value::undefined(), |message| message.get()));
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#contexttodomessage>
    #[method]
    pub fn todo(
        &self,
        scope: &Scope<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let record = record_of(scope, &self.data().record)?;
        let mut data = record.data_mut();
        data.todo = true;
        data.message
            .set(message.map_or(js::value::undefined(), |message| message.get()));
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#contexttestname-options-fn>: start a subtest, which
    /// counts towards the plan.
    #[method]
    pub fn test<'r>(
        &self,
        scope: &'r Scope<'_>,
        name: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
        callee: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, Thrown> {
        let record = record_of(scope, &self.data().record)?;
        count_for_plan(record);
        let undefined = HandleValue::undefined();
        let args = [
            name.unwrap_or(undefined),
            options.unwrap_or(undefined),
            callee.unwrap_or(undefined),
        ];
        let subtest = create_test(scope, record, TEST, args, None)?;
        Ok(start_subtest(scope, subtest)?)
    }

    /// <https://nodejs.org/api/test.html#contextbeforefn-options>
    #[method]
    pub fn before(
        &self,
        scope: &Scope<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        self.add_hook(scope, HookType::Before, callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextafterfn-options>
    #[method]
    pub fn after(
        &self,
        scope: &Scope<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        self.add_hook(scope, HookType::After, callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextbeforeeachfn-options>
    #[method]
    pub fn before_each(
        &self,
        scope: &Scope<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        self.add_hook(scope, HookType::BeforeEach, callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextaftereachfn-options>
    #[method]
    pub fn after_each(
        &self,
        scope: &Scope<'_>,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        self.add_hook(scope, HookType::AfterEach, callee, options)
    }

    /// <https://nodejs.org/api/test.html#contextwaitforcondition-options>: not implemented.
    #[method]
    pub fn wait_for(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        Err(NodeError::not_implemented("t.waitFor").throw(scope))
    }
}

impl<'s> TestContext<'s> {
    /// Register the hook of `hook_type` of the context's record.
    fn add_hook(
        &self,
        scope: &Scope<'_>,
        hook_type: HookType,
        callee: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let record = record_of(scope, &self.data().record)?;
        create_hook(
            scope,
            record,
            hook_type,
            callee.unwrap_or(HandleValue::undefined()),
            options.unwrap_or(HandleValue::undefined()),
        )
    }
}

/// The context of a suite, its function's argument:
/// <https://nodejs.org/api/test.html#class-suitecontext>.
#[core_runtime::jsclass(name = "SuiteContext", hidden)]
pub struct SuiteContext {
    record: Option<Heap<TestRecordImpl>>,
}

#[core_runtime::jsmethods(node_args)]
impl SuiteContext {
    /// A context without a suite, whose members throw, as `new SuiteContext()` creates it in
    /// Node.js.
    #[constructor]
    pub fn create() -> Self {
        SuiteContextImpl { record: None }
    }

    /// <https://nodejs.org/api/test.html#contextsignal-1>
    #[getter]
    pub fn signal<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        signal_of(scope, record_of(scope, &self.data().record)?)
    }

    /// <https://nodejs.org/api/test.html#contextname-1>
    #[getter]
    pub fn name(&self, scope: &Scope<'_>) -> Result<String, ExnThrown> {
        Ok(record_of(scope, &self.data().record)?.data().name.clone())
    }

    /// <https://nodejs.org/api/test.html#contextfilepath-1>
    #[getter]
    pub fn file_path<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        record_of(scope, &self.data().record)?;
        entry_file(scope)
    }

    /// <https://nodejs.org/api/test.html#contextfullname-1>
    #[getter]
    pub fn full_name(&self, scope: &Scope<'_>) -> Result<String, ExnThrown> {
        Ok(full_name(scope, record_of(scope, &self.data().record)?))
    }

    /// Whether the suite passed.
    #[getter]
    pub fn passed(&self, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        Ok(record_of(scope, &self.data().record)?.data().passed)
    }

    /// Always 0.
    #[getter]
    pub fn attempt(&self, scope: &Scope<'_>) -> Result<f64, ExnThrown> {
        record_of(scope, &self.data().record)?;
        Ok(0.0)
    }

    /// <https://nodejs.org/api/test.html#contextdiagnosticmessage-1>
    #[method]
    pub fn diagnostic(
        &self,
        scope: &Scope<'_>,
        message: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        add_diagnostic(record_of(scope, &self.data().record)?, message);
        Ok(())
    }

    /// `s.log()`: not implemented.
    #[method]
    pub fn log(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        Err(NodeError::not_implemented("s.log").throw(scope))
    }
}

/// Register the test runner's classes on `global`, and create the realm's [`Harness`].
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    TestRecord::add_to_global(scope, global);
    Harness::add_to_global(scope, global);
    TestContext::add_to_global(scope, global);
    SuiteContext::add_to_global(scope, global);
    CountedAssertion::add_to_global(scope, global);
    EarlyHook::add_to_global(scope, global);
    Await::add_to_global(scope, global);
    DoneCallback::add_to_global(scope, global);
    EntryVariant::add_to_global(scope, global);
    mock::register_classes(scope, global)?;
    let harness = js::class::create_instance_with::<HarnessImpl>(scope, |_| HarnessImpl {
        success: true,
        building: true,
        ..Default::default()
    })?;
    js::class::register_realm_object::<HarnessObject>(scope, harness.as_object());
    Ok(())
}
