# StarlingMonkey

A JavaScript runtime for WASI and native platforms, built on
[SpiderMonkey](https://spidermonkey.dev/).

StarlingMonkey is designed to be extensible and provides safe high-level
abstractions for defining additional builtins as JS classes, WebIDL interfaces,
JS modules, and functions and properties on the global object.

---

## Contents

- [Running JavaScript](#running-javascript)
- [Built-in APIs](#built-in-apis)
- [CLI Reference](#cli-reference)
- [Extending with Custom Builtins](#extending-with-custom-builtins)
  - [`#[jsclass]` / `#[jsmethods]`](#jsclass--jsmethods)
  - [`#[jsmodule]`](#jsmodule)
  - [`#[jsglobals]`](#jsglobals)
  - [`#[jsnamespace]` / `#[webidl_namespace]`](#jsnamespace--webidl_namespace)
  - [`#[webidl_interface]`](#webidl_interface)
  - [`#[derive(Traceable)]`](#derivetraceable)
- [Error Handling](#error-handling)
- [Inheritance](#inheritance)
- [Promise / Async](#promise--async)
- [Building and Testing](#building-and-testing)
- [Web Platform Tests (WPT)](#web-platform-tests-wpt)
- [GC Rooting Checks](#gc-rooting-checks)
- [Key Design Points](#key-design-points)

---

## Running JavaScript

StarlingMonkey runs `.js` and `.mjs` files as ES modules by default:

```bash
starlingmonkey script.js
```

ES module features work out of the box — `import`/`export`, strict mode, and
multi-file projects:

```js
// greet.js
export function greet(name) {
  return `hello, ${name}`;
}
```

```js
// main.js
import { greet } from "./greet.js";
console.log(greet("world"));
```

```bash
starlingmonkey main.js
```

JSON modules are imported with an import attribute, and `import()` loads a module
dynamically:

```js
import config from "./config.json" with { type: "json" };
const { greet } = await import("./greet.js");
```

For quick one-liners, use `-e`:

```bash
starlingmonkey -e 'console.log("hello")'
```

For legacy scripts that rely on sloppy mode or a global `this`:

```bash
starlingmonkey --legacy-script old-code.js
```

---

## Built-in APIs

StarlingMonkey's suite of builtins is a work in progress for now.

### C++ built-ins

The previous incarnation of StarlingMonkey was written in C++. This one has
support for running built-ins from that version, in the
[crates/builtins/cpp-builtins](crates/builtins/cpp-builtins) crate. Only the old
`console` builtin is added right now.

Builtins are tested against the
[Web Platform Tests](https://github.com/web-platform-tests/wpt) suite running
on both native and `wasm32-wasip2` targets.

---

## CLI Reference

```
starlingmonkey [OPTIONS] [SCRIPT_PATH]

Arguments:
  [SCRIPT_PATH]   Path to the entry JS/MJS file (default: ./index.js)

Options:
  -e, --eval <SCRIPT>                Evaluate inline script instead of a file
  -i, --initializer-script <PATH>    Run an init script (classic, synchronous) before the content script
      --legacy-script                Run as a classic script instead of an ES module
  -v, --verbose                      Enable verbose logging
  -d, --debug                        Enable script debugging via socket
      --async-stacks                 Show async parent frames in Error.stack (always on under a debugger)
      --wpt-mode                     Enable WPT (Web Platform Tests) mode
      --init-location <URL>          Override the location URL for initialization
      --strip-path-prefix <PREFIX>   Strip this prefix from script paths
      --serve <PORT>                 Serve HTTP on this port, dispatching fetch events
      --serve-isolated               Handle each served request in its own global
      --dispatch-timeout <SECONDS>   Give up on a request whose respondWith hasn't settled (0 = no limit)
      --response-body-timeout <SECONDS>   Cut a response body not fully sent by then, truncating it visibly (0 = no limit)
      --waituntil-timeout <SECONDS>   Stop driving a served request's leftover waitUntil work (0 = no limit)
      --end-to-end-timeout <SECONDS>   Wall-clock ceiling over all of a request's phases together (0 = no limit)
      --request-read-timeout <SECONDS>   Give up on a client that stopped sending (default 30, 0 = no limit)
      --keepalive-timeout <SECONDS>   Close an idle kept-alive connection (default 30, 0 = no limit)
      --max-connection-buffer-size <BYTES>   The maximum number of bytes the server will read from the client's connection at once. (default ca 400KiB, minimum 8KiB)
      --max-request-headers <COUNT>   Cap the number of header fields (default 128)
      --max-request-body-bytes <BYTES>   Cap a request body (default 512MiB)
      --max-body-drain-bytes <BYTES>   Spend at most this reading a body the handler ignored (default 256KiB)
      --max-connections <COUNT>      Serve at most this many connections at once (default 1024)
  -h, --help                         Print help
```

The serve timeouts are unlimited by default, except under `--wpt-mode`, where the
per-phase ones default to 120s. The per-phase timeouts each bound their own phase,
while `--end-to-end-timeout` caps the whole request regardless of how the phases
divide it.

The `--max-…` limits, `--request-read-timeout` and `--keepalive-timeout` bound
guest controlled allocation and wait times.

All transport related limits exist in native builds only, since on wasm, the host
runtime enforces equivalent limits.

**Module mode** (default) — strict mode, `import`/`export` supported, `this`
is `undefined` at the top level.

**Legacy script mode** (`--legacy-script`) — sloppy mode, no
`import`/`export`, `this` is the global object.

**Output** — `console.log`, `console.info` and `console.debug` print to stdout,
and `console.warn` and `console.error` print to stderr with a `Warn:` or `Error:`
prefix. An exception a timer or other task throws is reported on stderr as
`Uncaught` with its message and stack, and a promise rejected with no handler as
`Uncaught (in promise)` with the reason's.

**Exit status** — `0` when the script and all the work it scheduled complete,
including when a promise rejected with no handler. `1` when the script throws,
its top-level `await` rejects or never settles, or an initializer, the event loop
or the runtime fails, with the error printed to stderr. `2` for invalid
command-line arguments.

**Serving** — a script served with `--serve` or under `wasmtime serve` that throws,
or whose top-level `await` rejects, fails startup. The native server exits with
status 1 once that happens, which for a top-level `await` can be after it began
listening. A wasm instance logs the error once and answers every request with a
500. With `--serve-isolated`, the script is evaluated for each request, and a
request whose evaluation fails is answered with a 500.

---

## Extending with Custom Builtins

StarlingMonkey provides proc macros for exposing Rust code to JavaScript. All
builtins in the `web-globals` crate are implemented using these macros.

### `#[jsclass]` / `#[jsmethods]`

Expose a Rust struct as a JS constructor with methods, getters, setters, and
static methods:

```rust
use libstarling::{jsclass, jsmethods};

#[jsclass]
struct Counter {
    value: i32,
}

#[jsmethods]
impl Counter {
    #[constructor]
    fn new(initial: i32) -> Self { Self { value: initial } }

    #[method]
    fn increment(&mut self) { self.data_mut().value += 1; }

    #[getter]
    fn value(&self) -> i32 { self.data().value }

    #[static_method]
    fn zero() -> Self { Self { value: 0 } }
}

// Register on the JS global and create an instance from Rust:
Counter::add_to_global(&scope, global);
let c: Result<Counter<'_>, ExnThrown> = Counter::new(&scope, 0);
```

Inside `#[jsmethods]`, `self` is the stack newtype, not the data struct, so
fields are reached through `self.data()` and `self.data_mut()` rather than
directly. From Rust, the generated constructor allocates a JS object and so
returns `Result<Counter<'s>, ExnThrown>`.

Note: `self.data()` and `self.data_mut()` should only ever be used ephemerally.
Otherwise there's a risk of having multiple incompatible borrows, which we
can't statically guard against. There's a dynamic check, but it results in
slightly opaque error stacks and is hence hard to debug.

The `#[jsclass]` macro generates two types from the annotated struct, allowing
the type to be used from JS and Rust while ensuring proper GC rooting:

| Generated type | Purpose |
|----------------|---------|
| `CounterImpl` | Inner data struct implementing `ClassDef` (`#[doc(hidden)]`). |
| `Counter<'s>` | Stack newtype wrapping `Stack<'s, CounterImpl>` — use within a GC scope. |

To store a reference to an instance in a long-lived struct, hold a
`Heap<CounterImpl>` inside a `#[derive(Traceable)]` struct — see
[`#[derive(Traceable)]`](#derivetraceable).

**`#[jsclass]` options:**

```rust
#[jsclass(name = "MyCounter")]         // override the JS class name
#[jsclass(extends = Parent)]           // set up a prototype chain
#[jsclass(js_proto = "Error")]         // inherit from a built-in JS prototype
#[jsclass(to_string_tag = "MyClass")]  // set Symbol.toStringTag
```

**`#[jsmethods]` attributes:**

| Attribute | Role |
|-----------|------|
| `#[constructor]` | Called when JS code runs `new Counter(...)`. |
| `#[method]` / `#[method(name = "jsName")]` | Instance method on the prototype. |
| `#[getter]` | Read accessor for a JS property (`obj.x`). |
| `#[setter]` | Write accessor; `fn set_x(&mut self, v: T)` pairs with the `x` getter. |
| `#[static_method]` | Method on the constructor (`Counter.zero()`). |
| `#[destructor]` | Runs during GC finalization, before the Rust data is dropped. Takes an optional second parameter, `*mut JSObject`, which receives the object being finalized. A subclass runs its parent's destructor after its own. |

**Return types:**

| Rust return type | JS behaviour |
|------------------|-------------|
| `()` | `undefined` |
| `T: ToJSValConvertible` | Value returned to JS. |
| `Result<T, E>` where `E: ThrowException` | `Ok` → value; `Err` → typed JS exception. |
| `Self` (from `#[static_method]` / `#[method]`) | New JS instance of the same class. |
| `PromiseFuture` | JS `Promise` resolved to the result of a Rust future. |
| `Ref<'_, T>` where `T: ToJSValConvertible + ?Sized` | Value returned to JS, converted from data the class still owns. |

**Returning borrowed data:**

A getter that returns `String` copies the stored bytes into a `String` that
exists only to be copied again into a JS string and dropped. `&str` can't be
returned in its place (it would borrow from the `data()` guard, which is a
temporary) but the guard itself can be narrowed to the field and returned:

```rust
#[getter]
pub fn client_id(&self) -> Ref<'_, str> {
    Ref::map(self.data(), |data| data.client_id.as_str())
}
```

The trampoline converts while the guard is alive and drops it after, so the JS
string is built straight from the stored bytes. `Ref::map` works for any
projection, not just strings — `Ref<'_, [u8]>` out of a `Vec<u8>`, say.

**Constants:**

`pub const` items in `#[jsmethods]` blocks become read-only properties on
the constructor:

```rust
#[jsmethods]
impl Counter {
    pub const MIN: i32 = 0;
    pub const MAX: i32 = 1000;
    // ...
}
```

**Variadic arguments:**

Use `RestArgs<T>` as the last parameter to collect the remaining arguments, each
converted with `FromJSVal`:

```rust
#[static_method]
fn sum(a: f64, rest: RestArgs<f64>) -> f64 {
    a + rest.iter().sum::<f64>()
}
```

This works on any callable that takes arguments: `#[method]`,
`#[static_method]`, `#[constructor]`, and the free functions exposed by
`#[jsmodule]`, `#[jsglobals]`, `#[jsnamespace]`, and `#[webidl_namespace]`.

The element type must implement `FromJSVal` and be GC-safe where applicable.
Use `RestArgs<HandleValue<'_>>` for untyped elements, or take the raw `&CallArgs`
for untyped access to the whole argument list.

**Promise arguments and dictionary members:**

WebIDL's [`Promise<T>`](https://webidl.spec.whatwg.org/#idl-promise) initially
accepts values without a typecheck. The input is wrapped into a promise with
`Promise.resolve(value)`, with the typecheck performed on the resolution value
once the promise settles. The promise exposed at the callsite resolves to the
result of the typecheck, or rejects with a type error..

```rust
// WebIDL `undefined waitUntil(Promise<undefined> f)` — every value converts to
// `undefined`, so there is nothing to check.
#[method]
fn wait_until(&self, scope: &Scope<'_>, f: Promise<'_>) -> Result<(), ExnThrown> { /* … */ }

// WebIDL `undefined take(Promise<Payload> p)` — the value it settles with is
// checked against `Payload`.
#[method]
fn take(&self, p: PromiseOf<'_, Payload<'_>>) { /* … */ }

// Dictionary members work the same way.
#[webidl_dictionary]
struct TakeInit<'a> {
    p: Option<PromiseOf<'a, Payload<'a>>>,
}
```

`PromiseOf<'_, T>` derefs to `Promise<'_>`. Note that a `Promise<'_>` *element*
of a `RestArgs<…>` is an ordinary `FromJSVal` brand check, not this conversion.

**Inherited dictionaries:**

`#[webidl_dictionary(extends = Parent)]` declares an inherited dictionary. It
holds its parent in a `parent` field and reaches the inherited members through
it — `Deref` makes that transparent at any depth:

```rust
#[webidl_dictionary(extends = EventInit)]
pub struct CustomEventInit<'a> {
    pub parent: EventInit,
    pub detail: Option<HandleValue<'a>>,
}

// `init.detail` and `init.bubbles` both just work.
```

The parent converts first, then the type's own members lexicographically.
That order is observable, since every member is a property get that can run an
author's getter.

**Parameters by reference:**

Any parameter can be taken as `&T` or `Option<&T>`; the trampoline converts an
owned `T` and lends it for the call. While this doesn't help with calls from JS,
where the input has to be converted to an owned value regardless, it means that
calls from Rust can pass a borrow instead of allocating:

```rust
#[constructor]
pub fn new(event_type: &str, init: Option<&ExtendableEventInit>) -> Self { /* … */ }

// From Rust: no `.to_string()`.
ExtendableEventImpl::new("fetch", Some(&ExtendableEventInit::new(true)))
```

Taking a dictionary by reference is the other reason, since a reference
deref-coerces up an inheritance chain:

```rust
#[constructor]
fn new(event_type: String, init: &FetchEventInit<'_>) -> Self {
    // `&FetchEventInit` → `&ExtendableEventInit`, which is what this wants.
    ExtendableEventImpl::new(event_type, Some(init.deref()))
}
```

### `#[jsmodule]`

Turn a Rust `mod` block into an importable ES module:

```rust
#[jsmodule]
mod math_utils {
    pub const PI: f64 = std::f64::consts::PI;

    pub fn add(a: f64, b: f64) -> f64 { a + b }

    pub fn safe_divide(a: f64, b: f64) -> Result<f64, String> {
        if b == 0.0 { Err("division by zero".into()) } else { Ok(a / b) }
    }
}

// Register before evaluating any JS that imports it:
unsafe { math_utils::register(&scope); }
```

Exported functions are renamed to camelCase; constants keep their declared name.
The import specifier is the `mod` name camelCased, so `mod math_utils` is
imported as `"mathUtils"`:

```js
import { PI, add, safeDivide } from "mathUtils";
```

Override the import specifier: `#[jsmodule(name = "my-math")]`

### `#[jsglobals]`

Install functions, constants, and class constructors directly on the global
object:

```rust
#[jsglobals]
mod app_globals {
    pub use super::Circle;   // `pub use` items register #[jsclass] classes;
    pub use super::Shape;    // any order works — parents are auto-registered first
    pub const APP_NAME: &str = "My App";

    pub fn greet(name: String) -> String {
        format!("Hello, {name}!")
    }
}

// Install on a global object:
app_globals::add_to_global(&scope, global);
```

JS sees `greet` and `APP_NAME`: as everywhere else, functions are camelCased
and constants keep their declared name.

`pub use`'d classes must be `#[jsclass]`es or `#[webidl_interface]`s.

### `#[jsnamespace]` / `#[webidl_namespace]`

Create a plain singleton object (like `console`):

```rust
#[jsnamespace(name = "console")]
mod console_ns {
    use js::gc::scope::Scope;
    use js::native::CallArgs;

    pub fn log(scope: &Scope<'_>, args: &CallArgs) { /* ... */ }
    pub fn warn(scope: &Scope<'_>, args: &CallArgs) { /* ... */ }
}

console_ns::add_to_global(&scope, global);
```

`#[webidl_namespace]` is the same but auto-sets `Symbol.toStringTag` per
[WebIDL §3.13](https://webidl.spec.whatwg.org/#es-namespaces).

### `#[webidl_interface]`

Like `#[jsclass]` but with [WebIDL §3.7](https://webidl.spec.whatwg.org/#es-interfaces) semantics:
- `Symbol.toStringTag` auto-set to the class name (overridable with `to_string_tag`)
- `pub const` items installed on **both** constructor and prototype

```rust
#[webidl_interface(js_proto = "Error")]
struct DOMException {
    name: String,
    message: String,
}

#[webidl_methods]
impl DOMException {
    pub const INDEX_SIZE_ERR: u16 = 1;
    // ... constructors, methods, getters, as with `#[jsmethods]`
}
```

Pair it with `#[webidl_methods]` rather than `#[jsmethods]`: it takes the same
member attributes, but registers methods with WebIDL's property flags (they're
enumerable, unlike JS builtins').

Same options as `#[jsclass]`: `name`, `extends`, `js_proto`, `to_string_tag`.

### `#[derive(Traceable)]`

Generate `unsafe impl Trace` so SpiderMonkey's GC can find JS references
stored in your Rust structs:

```rust
#[derive(Traceable)]
struct AppState {
    node: Heap<MyClassImpl>,    // traced automatically
    #[no_trace]
    counter: u32,               // excluded from tracing
}
```

Whenever a JS object reference outlives the GC scope it was created in, store
it as `Heap<MyClassImpl>` (naming the inner data type from `#[jsclass]`) inside
a `#[derive(Traceable)]` struct. Root it back onto the stack with
`Heap::get(&scope)`, which hands back the stack newtype (`MyClass<'s>`).

---

## Error Handling

Methods returning `Result<T, E>` where `E: ThrowException` throw typed JS
exceptions on `Err`:

```rust
use js::error::{TypeError, RangeError, SyntaxError};

#[jsmethods]
impl MyClass {
    #[method]
    fn parse(&self, input: String) -> Result<String, SyntaxError> {
        if input.is_empty() {
            return Err(SyntaxError("input must not be empty".into()));
        }
        Ok(input)
    }
}
```

**Built-in error types:**

| Type | JS Exception |
|------|-------------|
| `TypeError(String)` | `TypeError` |
| `RangeError(String)` | `RangeError` |
| `SyntaxError(String)` | `SyntaxError` |
| `String` | automatically converted to `TypeError` |
| `ExnThrown` | no-op: an exception is already pending |

The first four live in `js::error`. `web_globals::dom_exception` adds
`DOMExceptionError { name, message }`, which throws a `DOMException`.

Implement `ThrowException` for custom error types:

```rust
use js::error::{ExnThrown, ThrowException, TypeError};
use js::gc::scope::Scope;

struct MyError(String);

impl ThrowException for MyError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        TypeError(self.0).throw(scope)
    }
}
```

`ExnThrown` is a witness that a JS exception is now pending and must be percolated up
until the exception is handled.

---

## Inheritance

```rust
#[jsclass]
struct Shape { color: String }

#[jsmethods]
impl Shape {
    #[constructor]
    fn new(color: String) -> Self { Self { color } }
}

#[jsclass(extends = Shape)]
struct Circle {
    parent: ShapeImpl,  // the parent's data, embedded
    radius: f64,
}

#[jsmethods]
impl Circle {
    #[constructor]
    fn new(color: String, radius: f64) -> Self {
        Self { parent: ShapeImpl::new(color), radius }
    }

    #[method]
    fn area(&self) -> f64 {
        std::f64::consts::PI * self.data().radius * self.data().radius
    }
}
```

A class embeds its parent's data, so the `parent` field holds the parent's
`Impl` type, e.g. `ShapeImpl` for `extends = Shape`.

Cast between the two from Rust with `cast`, which checks the JS object's type
tag in both directions:

```rust
let circle: Circle<'s> = /* ... */;
let shape: Shape<'s> = circle.cast::<Shape<'_>>().unwrap();   // widening
let back: Result<Circle<'s>, _> = shape.cast::<Circle<'_>>(); // narrowing
```

---

## Promise / Async

Return `PromiseFuture` from any method to create a JS `Promise` that resolves 
to the result of a Rust future:

```rust
use js::promise::PromiseFuture;

#[jsmethods]
impl Fetcher {
    #[method]
    fn fetch(&self, url: String) -> PromiseFuture {
        PromiseFuture::new(async move {
            // ... async work ...
            Ok("response body".to_string())
        })
    }
}
```

The method returns the `Promise` to JS immediately, and the future is queued on
the event loop. `ScriptEvaluation::run_to_completion`, which
`libstarling::run` drives for you, polls it and settles the `Promise` with the
future's `Ok`/`Err`. Two other constructors cover the cases `new` doesn't:
`PromiseFuture::new_void` for futures resolving to `()`, and `PromiseFuture::from_value`
for futures resolving to a value.

---

## Building and Testing

**Prerequisites:**

- [Rust toolchain](./rust-toolchain.toml)
- [just](https://github.com/casey/just)
- [WASI-SDK 34](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34)
- [Node.js](https://nodejs.org/), to run the WPT harness

Checking, building, and testing is done using a [`justfile`](justfile).
Commands include:

```bash
just build             # debug build
just test              # all Rust tests, use `-p` for specific packages
just wpt-test          # all Web Platform Tests, optionally filtered by a pattern
just fmt               # format code
just clippy            # run clippy
just check             # fmt-check + clippy + tests
just check-all         # more extensive tests, including GC checks
```

`just test` invokes `cargo test` with the right feature set and passes
`--workspace` by default. It accepts all additional arguments to `cargo test`,
so to test specific packages, pass `-p [package name]`.

**For WebAssembly (WASIp2):**

```bash
just build-wasm        # debug build for wasm32-wasip2
just test-wasm         # all Rust tests, on wasm32-wasip2
just check-wasm        # fmt-check + clippy + wasm tests
```

`WASM_TARGET` retargets those recipes and the wasm WPT recipes. It accepts `p2`, `p3`, or a
full triple, and defaults to `p2`:

```bash
WASM_TARGET=p3 just test-wasm
WASM_TARGET=p3 just wpt-test-wasm
```

`rust-toolchain.toml` pins a toolchain that ships no wasm32-wasip3 std, so `p3` builds through
`cargo +nightly`, and `p3` needs wasi-sdk 34, the first with a wasm32-wasip3 sysroot. The runtime
build (`build-runtime`) follows it too.

`just build-runtime` and `just test-runtime` default to `p3` rather than `p2`: they build and read
`target/wasm32-wasip3/release/starling.wasm` unless `WASM_TARGET` selects another target.

The package builds two targets. `cargo build` produces the native binary
`target/debug/starlingmonkey`. A wasm build produces the component
`target/wasm32-wasip2/debug/starling.wasm` from the crate's `cdylib` target. The binary is
not the wasm entry point, because a binary links `crt1-command.o`, which exports
`wasi:cli/run` itself and collides with the component's own export on wasm32-wasip3. It is
still built on wasm targets, where it does nothing.

### Runtime Builds for Component Linking

`starling-componentize` links the full StarlingMonkey runtime, SpiderMonkey
included, together with generated bindings for a WIT world, from this build:

**Static runtime.** `just build-runtime` builds the runtime as a
`wasm32-wasip3` component, `target/wasm32-wasip3/release/starling.wasm`, with
SpiderMonkey, libc and libc++ statically linked and dead-code eliminated. It is
about 19 MB and needs nightly Rust and a wasi-sdk 34 or later. `WASM_TARGET=p2`
builds it for `wasm32-wasip2` from the stable toolchain and an older SDK instead.
The same component runs scripts under `wasmtime run`, serves under
`wasmtime serve`, and is the runtime the componentizer links against. The
script links the core module, copies its `component-type` sections under
`starling:`-prefixed names, and wraps it with `wasm-tools component new`. The
componentizer needs those copies, so it refuses a `starling.wasm` a plain
`cargo build` wrote. `just test-runtime` runs a JS hello-world (console output,
`setTimeout`) with it under wasmtime.

Running a script calls no `run` export: the script's top level and the event loop
it starts are the whole program. The `run` export of the main module is called only
in a pre-initialized instance, one componentized as a CLI tool or
wizened directly with Wizer through the runtime's `wizer-initialize` export:

```bash
wasmtime wizer --keep-init-func=true --dir=. -Scli=y,http=y,p3=y -Wcomponent-model-async=y \
    --env STARLINGMONKEY_CONFIG=app.js target/wasm32-wasip3/release/starling.wasm -o tool.wasm
wasmtime run -Shttp=y,p3=y -Wcomponent-model-async=y tool.wasm
```

A snapshot whose main module registers no `fetch` listener and exports no `run`
function is refused, since it can neither serve nor run.
A snapshot is also refused while the top level has not settled, or has left work
behind, such as a timer or a `fetch` in flight. A host resource the script keeps
without pending work, such as the body of a `fetch` response it did not read, is
not detected. Its handle is not valid in an instance resumed from the snapshot.

---

## Web Platform Tests (WPT)

The project includes a [WPT](https://web-platform-tests.org/) harness that
validates web API conformance against the official test suite.

### Setup

Running the full test suite requires a bunch of additions to `/etc/hosts`.
These can be applied with the following command:

```bash
just wpt-setup
```

Additionally, a local clone of the WPT test suite needs to be available.
To use a single clone across multiple working trees, pass the location using
the `WPT_ROOT` env var, or the `--wpt-root` when running the suite.

Use the following command to create a new clone at the right revision under
[deps/](deps/):

```bash
just clone-wpt-tests
```

### Running tests

```bash
just wpt-test              # all configured WPT tests
just wpt-test base64       # only base64 tests
just wpt-test DOMException # only DOMException tests
just wpt-update            # run and update expectation files
```

use `just wpt-test-wasm` to run under WebAssembly instead of native, and
`just wpt-update-wasm` to update wasm-specific expectations.

Tests run concurrently (defaulting to number of CPUs * 2 since many aren't
compute-bound) and results are reported strictly in test order, so the output
does not depend on which test finished first. Use `--jobs=N` to override the
default.

Test results are compared against expectation files in
`tests/wpt-harness/expectations/`. When adding new web APIs, add corresponding
WPT test paths to `tests/wpt-harness/tests.json` and run `just wpt-update`.

**Native and wasm.** The same tests run on both targets, which do not always
behave identically: the two HTTP stacks, `hyper` and `wasi:http`, differ in what
they accept and preserve.
Tests with different expectations have the additional field `"wasm_status"`:

```json
{
  "a subtest that agrees everywhere": { "status": "PASS" },
  "a subtest that does not":          { "status": "PASS", "wasm_status": "FAIL" }
}
```

A test that cannot run on a target *at all* is prefixed in `tests.json` with
`SKIP-WASM(reason)` or `SKIP-NATIVE(reason)` instead. Prefer `wasm_status`:
skipping a whole file gives up the subtests that would have passed.

---

## GC Rooting Checks

StarlingMonkey's `js` API tries hard to provide everything needed to write code that's
GC safe, i.e. doesn't run the risk of GC causing use-after-free, etc. The correctness
of these APIs depends on annotations that are statically checked by a custom linter
called [crown](./crown), adapted from [Servo's lint of the same name][crown].

`crown` uses annotations that enable tracking of GC references, and enforces that wherever a
GC reference is held or stored, it's properly rooted. All core types representing GC references
are annotated with `#[js::must_root]`, which means they must be stored in `Stack`, `Handle`,
`Heap`, or more advanced types such as `RootedTraceableBox`. Builtins created using one of the 
macros such as `#[jsclass]` or `#[jsmodule]` are automatically annotated with `#[js::must_root]`.

The analysis can be run using `just check-gc`, and is also part of `just check-all`.

[crown]: https://github.com/servo/servo/tree/main/support/crown

Usually that check should be sufficient when working on anything but the `js` and `core-runtime`
crates, but to suss out rooting issues not caught by the static analysis, StarlingMonkey also uses
SpiderMonkey's dynamic GC rooting checks:

```bash
just gc-zeal                               # quick mode, covering the `js` and `core-runtime` crates
just gc-zeal full                          # exhaustive checks for the same crates, takes a few minutes
just gc-zeal full --workspace --examples   # check all the things. Please file a bug if this finds anything!
```

If the dynamic GC checks find anything outside of the `js` and `core-runtime` crates, that indicates a bug
in either of those crates. Please file a bug report!

---

## Key Design Points

**GC-safe value ownership**

StarlingMonkey provides safe abstractions that ensure GC references are
properly rooted on both the stack and the heap:

- *Stack* — `Foo<'s>` wrapping `Stack<'s, FooImpl>` with lifetime tied to the GC scope.
- *Heap* — `Heap<FooImpl>` inside a `Trace`-implementing struct for persistent references.

These make proper rooting the default and much easier to get right. The
GC rooting linter will additionally catch almost all violations of GC rooting.

**Safe, high-level JS API**

Built on top of `mozjs`, the `js` crate provides a higher-level API designed to
make use of SpiderMonkey easier and safer.

While there are some cases where lower-level constructs leak through for now,
the goal is to eventually make `js` a full abstraction layer.

**Proc-macro code generation**

As described above, StarlingMonkey has an extensive suite of proc macros to
make implementation and use of additional builtins as easy and safe as
possible.
