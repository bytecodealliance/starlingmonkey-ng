# Developing builtins

This guide is an attempt to capture all the key considerations that must go into adding or making
changes to StarlingMonkey builtins. It collects the practices that keep builtins consistent, fast
and correct. GC rooting has a dedicated guide, [`rooting.md`](rooting.md).

## Use what exists

Before writing a helper, cache, wrapper, macro or any other plumbing, check whether the capability
already exists. Most of what builtins share lives in these places:

- `crates/js/src`: the safe JSAPI layer.
- `crates/core-runtime/src`: the event loop, timers and the module loader.
- `crates/starling-macro`: the class and module macros, and the code they generate.
- `crates/builtins/web-*`: the existing web builtins.

If something close exists, use it, or extend it in place with a parameter or variant. If nothing
exists, add the new primitive to the shared crate where similar primitives live, not to the
builtin that first needs it.

What to use for capabilities that builtins often need:

| Capability | Use |
|---|---|
| Interned property names | `&'static CStr` names with `get_property`, `set_property`, `define_property`, which intern through `js::class::get_or_init_property_id`. `js::class::static_property_name` makes such a name from a runtime string of a bounded set. The `*_by_name` variants atomize other runtime names on each call. |
| Symbols | `js::symbol::get_well_known` for well-known symbols, and `new_symbol` rather than `get_symbol_for` unless the spec registers the symbol |
| Intrinsics (`Object`, `Error`, `Promise`, `RegExp`, …) | `js::class::get_class_object`, never a lookup on the global object |
| The runtime's own builtins | Direct Rust calls, per-global shared objects (`js::class::get_or_init_shared_object`), or a class's prototype (`js::class::get_prototype_object_for`), never a lookup by name |
| Per-global Rust state | `js::class::global_state` |
| Callback state | The value itself as the payload, or a `#[jsclass(hidden)]` instance read with `js::function::cast_payload` |
| The arguments of a native function as a list | A `RestArgs<T>` parameter |
| Timers | The `EventLoop` timer methods, such as `queue_timer`, not the global `setTimeout` |
| Promise reactions whose derived promise is unused | `Promise::add_reactions`, not `then` |
| Brand checks | Engine predicates (`cast`, `Array::is_array`), not `Symbol.toStringTag` or probe calls |
| Property descriptors | `Object::get_own_property_descriptor`, not a JS call to `Object.getOwnPropertyDescriptor` |
| Array contents | `js::Array::{with_contents, length, set_length}` |
| Iterators | `js::iteration` |
| UTF-8 and UTF-16 conversion | The `JSString` helpers (`from_str`, `from_latin1`, `from_utf16`, `to_utf8`) |

Do an audit for duplicated functionality before opening a PR, and remove any duplication you find.

## Instance state lives in Rust

All of an instance's state is one Rust struct in slot 0, the private data that `#[jsclass]`
classes use. Fields are typed, and fields that reference JS values are `Heap<T>`. Two reasons make
this a hard rule:

- **Performance.** Rust code reaches JS properties through SpiderMonkey's generic lookup path,
  which has no inline caches.
- **Safety.** Accessors on a prototype chain, monkeypatched prototypes and re-entrant author code
  can intercept or corrupt state kept in properties.

That rules out each of these:

- state in JS properties, symbol-keyed properties or JS arrays;
- hidden objects stored in properties to hold state;
- WeakMaps or other side tables keyed by the instance;
- separate reserved slots per kind of state.

## Register classes eagerly

Every class, including internal and hidden ones, is registered when a global is set up. Each
builtin crate has an `add_to_global` function, which is registered with
`core_runtime::runtime::register_global_initializer` and runs for each new global. It calls the
generated `add_to_global` of each of the builtin's classes. For a
`#[jsclass(hidden)]` class, the generated function registers the class with
`js::class::register_hidden_class`, which defines no global property.

Registration on first use would run after content code has started. It could observe or overwrite
globals that content code defined. It would also run on the first request rather than during
snapshot creation, so its cost wouldn't be covered by the snapshot.

## Borrows and re-entrancy

Never hold a borrow of a class's private data (`data()`, `data_mut()`, `get()`) across a call into
JavaScript. A call into content code, such as a stream's underlying source or sink algorithm or an
event listener, can call into the same instance again and panic on the outstanding borrow. Copy the
values you need into locals, make the call, then borrow again.

A borrow taken in the scrutinee of a `match`, `if let` or `while let` lives until the end of the
statement, including all of its arms. Bind the value to a local first. Debug builds check borrows
at runtime: run the affected tests against a debug build after changing code that calls into
JavaScript.

## Untrusted input

Builtins process input from content code and from the network. Input that makes a builtin abort
the process takes down every request an instance serves, and no JavaScript `try` can catch it.

- **Bound recursion.** Native code that recurses over a structure from content code, such as
  nested objects or nested data in a parser, checks its depth or the remaining native stack at
  every level, and throws an exception once it exceeds the limit. Iterating with an explicit stack
  is an alternative where the algorithm allows it.
- **Allocate fallibly.** A buffer whose size comes from JavaScript, from a header, or from decoded
  data is allocated with `Vec::try_reserve_exact` or another fallible allocation. If the
  allocation fails, the builtin throws an exception or reports out of memory, either with
  `js::exception::report_out_of_memory` or by returning `js::error::OutOfMemory` as a method's
  error. `vec![0; n]`, `Vec::with_capacity`, `resize` and `extend` abort the process when the
  allocation fails.
- **Compute sizes with checked arithmetic.** `usize` is 32 bits on wasm32, so a product of two
  sizes from JavaScript can wrap there and pass a limit check that catches it natively. Use
  `checked_mul` and `checked_add`, and compare against the limit before allocating.
- **Apply limits while the work happens.** A limit on output size, such as a stream's chunk size,
  stops the work as soon as it's reached, not after the whole output exists.
- **Check a value's type before converting it.** `Value::to_number`, `to_int32`, `to_double`,
  `to_boolean`, `to_string` and `to_object` assert the value's type and abort on any other. Values
  from content code go through `from_jsval_throwing` or a type check first, including values
  returned from content callbacks and read from properties that content code can assign.

## WebAssembly and instance reuse

- **Thread-locals reset on WASIp3.** `thread_local!` storage is reset on every export call while
  linear memory persists. Builtins use `instance_local!` for per-instance state, never
  `thread_local!`.
- **Instances are reused.** A wasm instance serves many requests, concurrently when requests wait
  on I/O. Nothing may assume a fresh instance per request, and per-instance caches must not grow
  without bound.
- **Top-level code runs at snapshot time.** Applications snapshotted with Wizer evaluate their top
  level during snapshotting, with no lazy deferral. State that can't be snapshotted, such as a host
  resource, is created when a request needs it.

## Writing tests

- **Don't repeat WPT.** Behavior checked by a WPT test with a passing expectation needs no
  in-tree test. In-tree tests cover what WPT doesn't reach: Starling-specific APIs, error paths, GC
  and rooting, and regressions for specific bugs.
- **Count wasm test binaries.** In each wasm CI job, every test binary that links the engine takes
  about 10 seconds to compile, however many tests it holds. Gate test files that are pure Rust
  or only meaningful natively with `#![cfg(not(target_arch = "wasm32"))]`. Add new ungated engine
  tests to a binary that already runs on wasm. A crate without unit tests worth running on wasm
  sets `[lib] test = false` and keeps its tests in `tests/`.
- **Wait for outcomes, not for fixed times.** Poll for an outcome (`serve_test_support::eventually`) instead
  of sleeping for a fixed time. Serve timeouts accept fractions of a second, so timeout tests
  configure a few hundred milliseconds rather than whole seconds.

## Verifying changes

Run targeted tests while iterating, and the slow suites once per coarse batch of changes:

- `cargo test --features debugmozjs -p <crate>` for the crates you changed, for example
  `-p core-runtime` or `-p libstarling --test serve`, and `just test` for the whole workspace.
- WPT (`just wpt-test`, `just wpt-test-wasm`) when web builtins or the `js` crate change. Record
  new results with `just wpt-update` and `just wpt-update-wasm`, not by editing expectation files
  by hand. A subtest whose result varies between runs is marked `FLAKY` in its expectation file,
  and updates keep that marker. Before calling a failure a regression, run it again and against
  the previous binary.
- The componentization suites (`just test-componentize`) when the runtime's component exports,
  the WIT bridge (`wit-bridge`) or the componentizer change.
- Crown (`just check-gc`, see [rooting.md](rooting.md#enforcing-proper-rooting-at-the-type-system-level-the-crown-static-analysis-lint))
  and the GC zeal tests (`just gc-zeal`).
- `just clippy`, which fails on any warning.

`just check-all` runs the formatting check, clippy, the examples' tests, the componentizer's unit
tests, crown and the quick GC zeal modes over `js`, `core-runtime`, `wit-bridge` and the
builtin crates. The other crates' tests need `just test`.

Run heavy jobs one at a time. Wasm builds, componentize runs, harness runs and benchmarks running
at once exhaust memory, and their timings are meaningless.

For performance work, measure server CPU time per request rather than requests per second alone,
alternate the old and new binaries, and repeat each measurement. Profile before optimizing.

## Comments and commits

Comments are plain declarative statements about the code they describe. State what the code does
and what a caller must know, enumerate the cases completely and leave out everything else.

Commits are coarse and grouped by theme, and each one compiles. A commit that fixes shared code
comes before the commits that build on it.
