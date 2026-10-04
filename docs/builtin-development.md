# Developing builtins

This guide is for anyone changing StarlingMonkey's builtins, human contributors and coding agents
alike. It collects the practices that keep builtins consistent, fast and correct, and the mistakes
that past changes had to undo. [`rooting.md`](rooting.md) covers GC rooting, and the
[implementation rules](node-compat.md#implementation-rules) in `node-compat.md` cover the Node.js
builtins specifically. This guide doesn't repeat them.

## Use what exists

A second implementation of an existing capability is a defect, even when it works. Duplicates
diverge, miss fixes made to the original (rooting, tracing, per-global lifetimes) and make every
later audit and performance change larger. Past examples include a second property-name cache next
to `js::class::get_or_init_property_id`, four copies of a no-op function, three copies of
`decodeURIComponent` with different bugs, and closures carrying state in JS arrays while the
web-streams builtins already passed typed state as callback payloads.

Before writing a helper, cache, wrapper, macro or any other plumbing:

1. Name the capability, for example "intern a property name", "per-global cached object", "class
   with private Rust state", "throw a `TypeError` with a code", "queue a task" or "convert a JS
   value to Rust".
2. Search for it by the words that describe it, not by one guessed function name, in
   `crates/js/src` (the safe JSAPI layer), `crates/core-runtime/src` (event loop, timers, module
   loader), `crates/starling-macro` (what the class and module macros generate) and the existing
   web builtins in `crates/builtins/web-*`.
3. If something close exists, use it, or extend it in place with a parameter or variant.
4. If nothing exists, add the new primitive to the shared crate where its siblings live, not to the
   builtin that first needs it.

These are capabilities that were reimplemented in the past, and what to use instead:

| Capability | Use |
|---|---|
| Interned property names | `&'static CStr` names with `get_property`, `set_property`, `define_property`, which intern through `js::class::get_or_init_property_id`. The `*_by_name` variants are for names built at runtime. |
| The public own properties an object is created with | A `js::object::PropertyTemplate` static, registered at global setup, and `Object::property_writer`. `PropertyInit::Define` is for objects the builtin created itself, `PropertyInit::Assign` for objects a constructor was given |
| Interned strings, such as event names | `js::class::get_or_init_atom` |
| Symbols | `js::symbol::CachedSymbol` statics, unregistered unless the spec registers them |
| Intrinsics (`Object`, `Error`, `Promise`, `RegExp`, …) | `js::class::get_class_object`, never a lookup on the global object |
| The runtime's own builtins | Direct Rust calls, realm objects, or a class's `prototype_object`, never a lookup by name |
| Callback state | The value itself as the payload, or a `#[jsclass(hidden)]` instance read with `js::function::cast_payload` |
| Work for the next tick | `js::jobs::queue_native_tick` with a payload and arguments |
| Timers | `core_runtime::event_loop::timer::start_timer` and the `EventLoop` timer methods, not the global `setTimeout` |
| Promise reactions whose derived promise is unused | `Promise::add_reactions`, not `then` |
| Error values | `NodeError::to_value` or `js::error::new_error`, not throwing and taking the pending exception |
| Brand checks | Engine predicates (`builtin_class`, `cast`, `is_proxy`, `IsSharedArrayBufferObject`), not `Symbol.toStringTag` or probe calls |
| Property descriptors | `Object::own_property_descriptor`, not a JS call to `Object.getOwnPropertyDescriptor` |
| Array contents | `js::Array::{length, push, to_vec}` and the splice helpers |
| Iterators | `js::iteration` |
| UTF-8 and UTF-16 conversion | The `JSString` helpers (`from_utf8_lossy`, `utf8_length`, `to_utf16_vec`, `from_latin1`) |

When an audit or review finds a second implementation, removing it is part of the fix.

## Instance state lives in Rust

All of an instance's state is one Rust struct in slot 0, the private data that `#[jsclass]`
classes use. Fields are typed, and fields that reference JS values are `Heap<T>`. Two reasons make
this a hard rule:

- **Performance.** Rust code reaches JS properties through SpiderMonkey's generic lookup path,
  which has no inline caches. Before the state of the Node.js builtins moved into Rust, generic
  property lookups and atomization were the largest native cost of serving a request, and a
  streamed response needed about 34 named lookups for every chunk written.
- **Safety.** Accessors on a prototype chain, monkeypatched prototypes and re-entrant author code
  can intercept or corrupt state kept in properties.

That rules out each of these:

- state in JS properties, symbol-keyed properties or JS arrays;
- hidden objects held in properties as state carriers;
- WeakMaps or other side tables keyed by the instance;
- separate reserved slots per kind of state.

The one exception is the handle that `#[jsfunction_class(state = "…")]` generates for ES5
subclassing, described in the implementation rules. A new exception needs a strong reason and an
explicit decision.

A property that the JavaScript ecosystem reads and writes directly is part of the API, whatever
the original implementation intended. `EventEmitter`'s `_events` is one: libraries change it in
place, so it stays a plain own data property. Decide such cases from what real code does with the
property, and document them.

## Register classes eagerly

Every class, including internal and hidden ones, is registered when a global is set up, through
the same mechanism the web builtins use. Registration triggered on first use runs after content
code has started. It can observe or clobber globals that content code defined, and it runs on the
first request rather than during snapshot creation, so its cost isn't part of the snapshot.

## Borrows and re-entrancy

Never hold a borrow of a class's private data (`data()`, `data_mut()`, `get()`) across a call into
JavaScript. Stream methods such as `_write`, `_read` and `emit` call back into content code, which
can call into the same instance again and panic on the outstanding borrow. Copy the values you
need into locals, make the call, then borrow again.

A borrow taken in the scrutinee of a `match`, `if let` or `while let` lives until the end of the
statement, including all of its arms. Bind the value to a local first. Debug builds check borrows
at runtime: run the affected Node.js tests against a debug build after changing code that calls
into JavaScript.

## Untrusted input

Builtins process input from content code and from the network. Input that makes a builtin abort
the process takes down every request an instance serves, and no JavaScript `try` can catch it.

- **Bound recursion.** Native code that recurses over a structure from content code, such as
  nested objects in `util.inspect` or deep comparison, nested groups in a glob pattern, or nested
  data in a parser, checks its depth or the remaining native stack at every level. Exceeding the
  limit throws the error Node.js throws, usually a `RangeError` for a call stack overflow.
  Iterating with an explicit stack is an alternative where the algorithm allows it.
- **Allocate fallibly.** A buffer whose size comes from JavaScript, from a header, or from
  decompressed or decoded data is allocated with `Vec::try_reserve_exact` or another fallible
  allocation. Failure throws Node's error for the operation, or reports out of memory with
  `js::exception::report_out_of_memory`. `vec![0; n]`, `Vec::with_capacity`, `resize` and
  `extend` abort the process when the allocation fails.
- **Compute sizes with checked arithmetic.** `usize` is 32 bits on wasm32, so a product of two
  sizes from JavaScript can wrap there and pass a limit check that catches it natively. Use
  `checked_mul` and `checked_add`, and compare against the limit before allocating.
- **Apply limits while the work happens.** A limit on output size, such as zlib's
  `maxOutputLength` or a stream's chunk size, stops the work as soon as it's reached, not after the
  whole output exists.
- **Check a value's type before converting it.** `Value::to_number`, `to_int32`, `to_double`,
  `to_boolean`, `to_string` and `to_object` assert the value's type and abort on any other. Values
  from content code go through
  `from_jsval_throwing` or a type check first, including values returned from content callbacks
  and read from properties that content code can assign.

## WebAssembly and instance reuse

- **Thread-locals reset on WASIp3.** `thread_local!` storage is reset on every export call while
  linear memory persists. Builtins use `instance_local!` for per-instance state, never
  `thread_local!`.
- **Instances are reused.** A wasm instance serves many requests, concurrently when requests wait
  on I/O. Nothing may assume a fresh instance per request, and per-instance caches must not grow
  without bound.
- **Synchronous host I/O blocks only in the CLI.** In serve mode and in components it is refused
  unless explicitly allowed.
- **Top-level code runs at snapshot time.** Componentized applications evaluate their top level
  during Wizer snapshotting, with no lazy deferral. State that can't be snapshotted, such as open
  files or sockets, is created when a request needs it.

## Matching Node.js

- **Compare with the pinned oracle.** `deps/node-bin` holds the Node.js release the tests come
  from. Check claims about Node's behavior or object shapes against that release, not against
  memory or a newer version.
- **Match the shape that code can observe.** That means whether a member is an own data property
  or a prototype accessor, whether it is enumerable, and whether the constructor is a `class` or an
  ES5 function. These differences affect `Object.keys`, `util.inspect`, `hasOwnProperty` and
  subclassing.
- **Port the behavior that Node's documentation doesn't describe.** Argument validation order,
  event order and error codes often come only from Node's implementation. Port that code's
  behavior and use the differential fuzzer to confirm it.
- **Use real APIs, not public globals.** Building a value through a public JS API can bring along
  restrictions meant for content code. Node's HTTP responses once went through the global
  `Response` constructor, and its header guard dropped `Set-Cookie` under
  `--web-security-restrictions`. Native builtins call each other's Rust APIs.

## Verifying changes

Run targeted tests while iterating, and the slow suites once per coarse batch of changes:

- `cargo test -p node-builtins --test modules`, `cargo test -p core-runtime` and
  `cargo test -p libstarling --test serve`.
- The Node.js test suite (`tests/node-harness/run-node-tests.mjs`). Record new results only with
  `--update-expectations`, never by editing `expectations.json` by hand or with another tool,
  because the harness sorts it in a specific order. Mark tests whose results vary between runs as
  `FLAKY` with `--mark-flaky`, once for each target they vary on. Before calling a failure a
  regression, run it again and against the previous binary.
- The differential fuzzer (`tests/node-diff/fuzz.mjs`) for every module you change, with 0 new
  signatures. Add generator variants for new behavior.
- The Express and Koa applications (`tests/express-apps/run.mjs`) in native, serve, wasm and Spin
  modes. WASI can't follow symbolic links out of a preopened directory, so the wasm runs preopen a
  linked `deps/node` or `node_modules` at the guest path where it's expected. Other tools that run
  `starling.wasm` need the same mapping.
- WPT when shared web builtins or the `js` crate change.
- Crown on every touched crate.
- `cargo clippy -- -D warnings` on every touched crate.

Run heavy jobs one at a time. Wasm builds, componentize runs, harness runs and benchmarks running
at once exhaust memory, and their timings are meaningless.

For performance work, measure server CPU time per request rather than requests per second alone,
alternate the old and new binaries, and repeat each measurement. Profile before optimizing: past
profiles showed that the time went to property lookups and allocation in the Rust layer, not to
the JavaScript of the applications.

## Comments and commits

Comments are plain declarative statements about the code they describe. State what the code does
and what a caller must know, enumerate the cases completely and leave out everything else.

- Don't attribute intent to code ("says", "wants", "decides", "knows").
- Avoid metaphors ("carries", "seam") and the "X is what/how/where Y" pattern.
- Don't use em dashes or join two sentences with a semicolon.
- Never state a mechanism you haven't verified.
- Match the comment density of the surrounding code.

Commits are coarse and grouped by theme, each one compiles, and a commit that fixes the shared
code comes before the commits that build on it.

## Working with coding agents

These practices apply to agents working in the repository, and to anyone delegating work to them:

- **Give rules explicitly.** Every prompt for an agent that writes runtime code states the rules
  above: use what exists, keep state in Rust, register eagerly, and keep borrows away from
  JavaScript calls. An agent left to infer them tends to rebuild existing infrastructure or carry
  over patterns the codebase has moved away from.
- **Audit before refactoring.** An audit with file and line references, the Node.js shapes
  involved and a list of the duplicated helpers gives a refactor a concrete specification. Line
  numbers go stale as soon as other work lands, so the refactor locates each item again.
- **Split parallel work by file ownership.** Tell each agent which files belong to another agent.
  Shared crates still overlap, so plan the merge order, and have the agent that wrote a branch
  rebase it, since that agent knows which of two overlapping helpers to keep.
- **Verify before reporting.** Run the gates on the final commit, report flaky results as flaky
  with the evidence, and report what wasn't run.
- **Keep long jobs in the background.** Long builds and test runs go in the background, and agents
  check their output occasionally rather than waiting on them in the foreground. An agent that
  waits on a long command can stall and be stopped, losing the work in progress.
