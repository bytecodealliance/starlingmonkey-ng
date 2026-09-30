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
  - [Componentizing JavaScript](#componentizing-javascript)
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
build (`build-runtime`) and the componentize suites follow it too, so a componentized guest
belongs to the target it was built for.

The componentize side defaults to `p3` rather than `p2`: `just build-runtime`, `just test-runtime`
and the componentize suites build and read `target/wasm32-wasip3/release/starling.wasm` unless
`WASM_TARGET` selects another target.

The package builds two targets. `cargo build` produces the native binary
`target/debug/starlingmonkey`. A wasm build produces the component
`target/wasm32-wasip2/debug/starling.wasm` from the crate's `cdylib` target. The binary is
not the wasm entry point, because a binary links `crt1-command.o`, which exports
`wasi:cli/run` itself and collides with the component's own export on wasm32-wasip3. It is
still built on wasm targets, where it does nothing.

### Runtime Builds for Component Linking

`starling-componentize` (below) links the full StarlingMonkey runtime, SpiderMonkey
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
in a pre-initialized instance, one componentized as a CLI tool (below) or
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

### Componentizing JavaScript

`componentize` (`starling-componentize` binary) assembles a JavaScript
source file and a custom WIT world into a self-contained WebAssembly component
on top of one of the runtime builds above. The component implements the world's
exports in JS and calls its imports as ordinary JS functions. No host runtime is
embedded beyond the component itself.

```bash
starling-componentize -d <wit-dir> -w <world> componentize <app.js> -o out.wasm
```

`-d`/`--wit-path` takes a `.wit` file or a directory of them (repeatable) and
`-w`/`--world` names the world to target, defaulting to the WIT's own default
world. A directory brings the packages in its `deps` directory along, so
`-d ./wit` is usually all that is needed. Repeated `-d`s are read in order, so a
package comes after the packages it uses. A package several of them define is
merged into one, with the interfaces, types and functions of every definition,
so libraries can each ship the part of a package they use. The definitions must
agree on the functions they have in common. `-w` names a world in the last
package loaded, or any world by its qualified name,
`namespace:package/world@version`. Repeat it to target several worlds: they are
merged into one world, `starling:componentize/merged`, which imports and exports
everything they do. These flags, and the others shown before the subcommand, may
also follow it. `-p`/`--base-directory` is the directory the application's relative
`import`s resolve within, and defaults to the input file's directory. A module
that imports the input file gets the application's main module itself. The
output (`-o`, default `js.wasm`) is a component you can run directly under
`wasmtime` or compose with others via `wasm-tools`.

`just componentize <args>` runs `starling-componentize` built from the checkout,
with the runtime `just build-runtime` last built embedded, in the directory you
run it from. `just install-componentize` builds the static runtime and installs
`starling-componentize` with the runtime embedded, so the installed binary needs
no other file. It is also packaged for npm as
`@bytecodealliance/starling-componentize`, which installs a prebuilt binary for
the platform from an optional dependency, one package per platform, and runs it
as `npx @bytecodealliance/starling-componentize`. Its `binaryPath()` export
returns the binary's path. `just npm-componentize [VERSION]` assembles the
packages for the current platform in `target/npm`, and their tarballs in
`target/npm/tarballs`.

The runtime and `starling-componentize` are released together, under the
workspace's version in `Cargo.toml`, which `componentize/Cargo.toml` repeats (a
test checks they agree). The `release` workflow builds `starling.wasm` and
`starling-componentize` for every platform in `componentize/npm/platforms.json`.
A `v<version>` tag publishes the npm packages and creates a GitHub release with
`starling.wasm` and each platform's binary. A manual run publishes nothing
unless asked to, and takes a version for the npm packages, such as
`0.3.0-preview.1` for the Spin JS SDK's preview kits. Every run uploads the
packages' tarballs as the `npm-tarballs` artifact. The Linux binaries link
against glibc 2.28, through `cargo zigbuild`, so they run on distributions as
old as that.

The componentizer calls the runtime's `init` export, the interface
`starling:componentize/init` that `starling/wit/init.wit` declares, and refuses
a runtime whose version of that interface differs from its own in more than the
patch number. Any change to the interface needs a new major or minor version of
its package, which CI checks against the latest release with
`scripts/check-init-wit.sh`.

A componentizer built without a runtime at
`target/wasm32-wasip3/release/starling.wasm`, or at the path
`STARLING_EMBED_RUNTIME` names at build time, embeds none. `--runtime`, or the
`STARLING_RUNTIME` environment variable, points at another static runtime build,
relative to the working directory. To link the dynamic library instead, pass
`--runtime-lib target/dylib/wasm32-wasip2/libstarling_rt.so` (built by
`just build-dylib`), which takes precedence over `STARLING_RUNTIME`. The wasi-sdk shared sysroot libraries it needs default to
`$WASI_SDK_PATH/share/wasi-sysroot/lib/wasm32-wasip2`, overridable with
`--sysroot-libs`.

`--init-location <url>` sets the URL `globalThis.location` reflects while the
application's top level runs. Without it, reading `location` there throws a
`TypeError`.

What the application prints while it initializes is shown, unless `-q` is
given. The compiled component is kept in wasmtime's compilation cache, which
speeds up componentizing the same world again, unless `--no-cache` is given.

The component's type is exactly the world's: it exports what the world declares
and nothing else. The runtime's own `wasi:cli/run` and `wasi:http/handler` are
exported only when the world declares them (or with `--cli` and `--serve`, whose
built-in worlds do), and the `init` interface the componentizer runs under
Wizer never survives. Every export is resolved when the snapshot is taken, so a
guest module missing one fails componentization with a message naming the JS
export to add, rather than trapping on the first call.

The application's top level runs to completion before the snapshot is taken. Its
relative and JSON imports are read from the base directory, and a top-level
`await` may wait on timers and dynamic `import()`s. Exports are resolved once it
has finished, so an export declared after a top-level `await` is found. A top
level that throws, rejects, or awaits a promise that never settles fails
componentization with the reason. A dynamic `import()` of a module the top level
did not load reads the file when an export calls it, through the component's own
filesystem imports and relative to the working directory the host gives it, so it
fails with `--disable filesystem`.

A snapshot holds memory, not a running event loop, so the top level must leave no
asynchronous work behind when it finishes: no timer or interval still pending, no
`fetch` it did not await. Componentization fails otherwise, listing each piece of
work with the stack that created it:

```text
Error: the application's top level finished evaluating with asynchronous work still pending, [...]
  - a pending `interval` timer, created at:
      startPolling@app.js:2:14
      @app.js:4:1
  - a host operation in flight, created at:
      @app.js:6:20
```

The component opens its own standard streams when it first writes after
resuming. Calling an import during initialization throws a `TypeError` naming
it, since the host's imports are not linked while the snapshot is taken, and
fails componentization unless the application catches it. An `async` import
returns a promise rejected with that error instead, which fails componentization
when the top level awaits it.

Some worlds are refused, each with an error naming the function or export:

- a function that uses a `map`, a fixed-length `list` or `error-context`, which the
  runtime cannot lift or lower yet;
- a synchronous export that takes or returns a `stream` or `future`, since no event
  loop drives their transfer (declare it `async`);
- a world-level import function that takes a `borrow` anywhere in its parameters,
  such as a method of a resource declared directly in the world rather than in an
  interface, since the composition step cannot encode a `borrow` outside an
  interface. `types` still describes such a world;
- a world that exports `wizer-initialize`, `wasi-http-handler` or the
  `starling:componentize/init` interface, names the componentizer's own exports
  take;
- a world that exports `wasi:cli/run` or `wasi:http/handler` in a version whose
  major and minor numbers or pre-release suffix differ from the one the runtime
  provides.

`--disable stdio,random,clocks,http,filesystem` (comma-separated, repeatable)
leaves the named WASI features out of the component: their interfaces disappear
from its imports, satisfied instead by stubs that trap when reached, so a world
that imports no WASI produces a component that imports none beyond
`wasi:cli/environment`, `wasi:cli/exit` and `wasi:random/insecure-seed`. The
application's top level still runs with every feature during componentization.
A disabled clock's `now` is the exception to trapping, since the runtime reads
it whatever the application does: it reads as zero, so `Date.now()` and
`performance.now()` return constants, and only waiting on the clock traps.
`random` leaves out `wasi:random/random` only. SpiderMonkey seeds its hash
tables from `wasi:random/insecure-seed`, so the component imports it unless
`--deterministic-insecure-random-seed <SEED>` is given, which makes
`get-insecure-seed` always return SEED's high and low 64 bits. With the seed,
an attacker can craft input that collides in those hash tables and slows the
application down drastically, so use it only with trusted input. `wasi:io` goes with
the last feature that uses it, and a feature another remaining one uses cannot be
disabled by itself: `filesystem` uses the clocks' types, so disabling `clocks`
alone is refused with a message listing every such use and the features to
disable with it. A trap in a stub names the disabled
function in the backtrace, as in
`disabled-features!wasi:random/random@0.3.0#get-random-bytes (disabled)`.

In both modes the world's bindings are generated with `wit-dylib`, a small module
that lowers and lifts every WIT function through the runtime's `wit_dylib_*`
intrinsics. With the static runtime, the componentizer takes the core module out
of `starling.wasm`, reserves room for the bindings' data in its memory and table,
and links the bindings as a library of it (`componentize/src/static_link.rs`). With the
dynamic library, every module is a position-independent library resolved by the
wasm dynamic-linking convention. The componentized snapshot behaves the same
either way.

#### Export names

The main module provides each function and resource class of an exported
interface in exactly one of the shapes below. For `greet` in the interface `greeter`
of the package `test:app@1.0.0`:

```js
// The package layer: `test-app` in lowerCamelCase, then the interface's name.
// The version is not part of the name.
export const testApp = { greeter: { greet } };
// The interface layer, the shape jco uses.
export const greeter = { greet };
// A bare export.
export function greet(name) { /* … */ }
```

The interface layer is allowed only when no other exported interface has the
same name, and a bare export only when no other exported item or world-level
function has the same name. Where the names of two shapes coincide, the package
layer's name takes precedence over an interface-layer and a bare name, and an
interface-layer name over a bare name. A world-level exported function is always
a bare export, and takes precedence over an interface-layer name. An interface
the world exports under a plain name (`export local: interface { … }`) has no
package layer. Functions are lowerCamelCase and resource classes UpperCamelCase.
A reserved word such as `default` or `new` is exported with an export clause
(`export { make as new }`).

Two versions of one interface have the same package-layer and interface-layer
names, so each is provided only by the export named by its full WIT name. A
TypeScript build needs `"module": "es2022"` or later for such a name:

```js
export { v1 as "local:hello/hello@1.0.0", v2 as "local:hello/hello@2.0.0" };
```

An item the main module provides in more than one shape, or in none, fails
componentization with a message listing the shapes it may take.

#### HTTP servers

For an HTTP server, pass `--serve` (or declare
`export wasi:http/handler@0.3.0;` in your own world), and the component exports
`wasi:http/handler`. With `-d`, `--serve` merges the world `-w` selects with a
world exporting the interface, so the application can import the interfaces its
WIT declares as well. The application serves it in one of two ways:

- Register a listener with `addEventListener('fetch', …)`. The runtime dispatches
  each request as a `fetch` event.
- Export its own `handle`, in the shapes of `wasi:http/handler`'s exports:
  `wasiHttp.handler.handle`, `handler.handle` or a bare `handle`. `handle`
  takes a `wasi:http/types` request and returns a response: a `Response`
  object, such as the one `fetch` resolved to, whose body is then passed on
  without being read, or a `wasi:http/types` one. `fetch` takes the
  `wasi:http/types` request as its input, so `return fetch(request)` forwards a
  request unchanged. The request can't be used after `fetch` took it.

```js
export async function handle(request) {
  if (request.getPathWithQuery() === "/proxy") {
    return fetch(request);
  }
  return new Response(`hello from ${request.getPathWithQuery()}`);
}
```

Which one the application uses is decided when the snapshot is taken. An
application that does both, or neither, fails componentization with a message
saying so. A world that declares the `wasi:http` interfaces itself must agree
with the runtime on every item both declare, or leave `wasi:http/handler` empty
(`interface handler {}`).

The `wasi:http/types` resources a `handle` receives have the methods the
runtime's builtins use, since the component imports nothing beyond them. A world
that imports `wasi:http/types` itself, with `-d`/`-w`, adds the functions it
declares, for example `fields`' `get` and `has`.

#### CLI tools (`wasi:cli/run`)

A command-line tool needs no WIT of its own. Pass `--cli`, which with `-d` merges
the selected world with one exporting `wasi:cli/run`, and have the application
`export` a `run` function:

```bash
starling-componentize --cli componentize <app.js> -o cli.wasm
wasmtime run -Sinherit-env=y,http=y,p3=y -Wcomponent-model-async=y cli.wasm
```

```js
// app.js
export async function run() {
  console.log("hello from a componentized CLI tool");
}
```

The resulting component exports `wasi:cli/run`, and invoking it calls the
JavaScript `run` export. An `async` `run` may `await` timers, `fetch` and
imports, which the call's event loop drives. Timers still pending once `run`'s
promise has settled are dropped without running. Running the module as a plain script
evaluates its top level without calling `run`. Componentizing a module without a
`run` function as a CLI tool fails, naming the export to add.

#### Components that serve and run

`--cli` and `--serve` combine, alone or with `-d`, into one component exporting
both `wasi:cli/run` and `wasi:http/handler`. The application then has to do what
each asks of it: export `run`, and register a `fetch` listener or export
`handle`. The host picks the export it calls, so `wasmtime run` calls `run` and
`wasmtime serve` serves requests, each in instances of its own resumed from the
same snapshot:

```bash
starling-componentize --cli --serve componentize <app.js> -o app.wasm
```

#### The interfaces the runtime imports

The targeted world also imports every interface the runtime imports, whether
it comes from `-d`, `--cli` or `--serve`: `wasi:cli`'s environment, exit,
standard streams and terminals, `wasi:clocks`, `wasi:filesystem`,
`wasi:random`, `wasi:http/types` and `wasi:http/client`. Each has the functions
the runtime itself imports, so the application can import them without adding
to what the component imports:

```js
import { getEnvironment } from "wasi:cli/environment@0.3.0";
```

`--disable` leaves out the interfaces of the features it names. An interface of
a disabled feature that a remaining one uses stays, and componentization then
refuses it with a message naming both.

#### WIT values in JavaScript

Every synchronous WIT type works today: all scalars (`u64` and `s64` are always
BigInts, like the elements of a `BigUint64Array`), `string`, `list` (numeric
lists as typed arrays, `list<u64>` and `list<s64>` as `BigUint64Array` and
`BigInt64Array`, and a numeric list also takes a plain `Array`), `record`,
`tuple`, `variant`, `enum`, `option`, `result`, `flags`, and `resource`s, with
constructors, methods, static methods, and borrows. Imports are surfaced as ES
modules the guest can `import`. Every imported resource has a class, including
one that no constructor, method or static function names. An imported resource
wrapper has a `[Symbol.dispose]()` method that releases the host handle early (so
`using` works, and a second call does nothing). Disposing of a wrapper an `async`
import call still borrows releases the handle once that call returns. A guest
class implementing an exported resource may define `[Symbol.dispose]()`, which
the runtime calls when the host drops the resource. Its methods are looked up on
the instance, so a subclass's override runs. This is validated
end-to-end by a 43-test round-trip suite over a WIT world exercising the whole
type system, with imported and guest-owned resources each covered end-to-end by
a suite of their own.

A variant's tag and a record's fields are their WIT names in lowerCamelCase
(`db-null` is `dbNull`). An `enum` crosses as its case index, and a `flags` set
as an int32 of its bits, the bit of the n-th flag being `1 << n`. The module of
the interface defining an enum or flags type, and of each interface that `use`s
it, exports a frozen object under the type's name in that interface, in
UpperCamelCase, as a TypeScript enum's object would be: it maps each case's
lowerCamelCase name to its value and each value back to the name
(`Qos.atLeastOnce` is `1`, `Qos[1]` is `"atLeastOnce"`). An error class of the
same name takes the name instead. An interface whose functions the world only
exports has no module, so the guest cannot import objects for its types.

An import argument that does not match its WIT type throws a `TypeError` naming
the argument, the part of it that failed and its value, such as
``argument 1 of `bigArgument` `.a1`: expected a string, got 5``. The arguments
of one call are also checked together: a resource wrapper passed as an `own`
must not appear anywhere else in them, a wrapper the guest received as a
`borrow`, or one an `async` import call still borrows, cannot be passed as an
`own`, and a `ReadableStream` must be unlocked and passed only once. An export result that does not match traps the call, and
the message printed on stderr names the export and the part of the result the
same way.

A synchronous export runs without an event loop. The microtasks it queues run
before it returns. A timer it starts throws a `TypeError` naming the export.
A `fetch`, an `async` import, or a read of a response body or of a stream from
the host it starts returns a promise rejected with one. Passing a stream or a
future to an import throws one too, since nothing would transfer it. A value it
writes to a stream or future that an earlier asynchronous call returned, such as
by settling a promise lowered to a future, is written in the next asynchronous
call.

Asynchronous WIT works too. `async` exported functions return a JS `Promise`
that the component lifts into a WASIp3 async return. `async` imports are called
as ordinary `async` JS functions and `await`ed. Their arguments are checked and
lowered when the call starts, so a change to an argument after the call, such as
to a typed array's elements, has no effect on what the host receives. An `async`
export whose promise is still pending once its call's event loop has nothing left
to run traps, even if a later call could settle the promise.

Streams and futures are web streams and promises:

- A `stream<T>` the guest receives is a `ReadableStream`. A `stream<u8>` is a
  readable byte stream of `Uint8Array` chunks, so BYOB readers work, and any
  other `stream<T>` has one chunk per element. The stream reads from the host
  only when something reads it, and cancelling it drops it.
- A `stream<T>` the guest hands over may be a `ReadableStream` or any async or
  sync iterable, such as an async generator or an array of chunks. A
  `stream<u8>` takes `ArrayBufferView` and `ArrayBuffer` chunks. A source that
  errors, or produces a chunk or element that does not match the stream's type,
  ends the stream early and logs why on stderr. After the host stops reading,
  the source is cancelled when it produces its next chunk. It is also cancelled
  when the call's event loop runs out of other work while the source produces
  nothing more. An imported resource the host does not read as a stream
  element goes back to the wrapper the guest holds. One nested in a record,
  tuple, option or list does not, and the guest's wrapper holds no handle
  afterwards. A stream the guest received and returns unread goes back to the
  host as it is.
- A `future<T>` the guest receives is a `Promise`. Its value is read whether or
  not the guest uses the promise. A `future<T>` the guest hands over may be a promise or any other value.
  A promise that never settles does not keep the call running, and its value
  is still written if it settles later. An `async` import the call started and
  did not await does keep it running until the import returns, so one whose
  host side waits on such a future never lets the call finish.
  An `async` export returning a `future<T>` returns it as the promise it
  produces, so the call completes before the future does.

Streams work with `fetch` in both directions: a received `stream<u8>` can be a
request body (`fetch(url, { method: "POST", body, duplex: "half" })`), and a
response's `body` can be returned as a `stream<u8>`.

The `err` arm of a `result` crosses as an error. An import's `err`, or a rejected
`future<result<T, E>>`, throws or rejects with an instance of `E`'s class when
`E` is a named type other than a resource, and a `ComponentError` otherwise.
`E`'s class is named after the type, extends `ComponentError`, and holds the
payload as `payload`. It is exported from the module of the interface whose
function names the type, under the name that interface uses for it, or from
`external` for a type the world names (`import { ErrorCode } from
"wasi:http/types@0.3.0"`). An interface the world only exports has no module, so
a type it `use`s from another interface has that interface's class. An export
failing, or a promise the guest hands over as a `future<result<T, E>>`
rejecting, produces an `err` from:

- a `ComponentError` or an instance of a class extending it: its `payload`,
- any other `Error`: its `message`, if `E` is a `string`, or an `enum` or
  `variant` with a payload-less case whose JS (camelCase) name it is,
- any other value: the value itself.

Anything else traps, as does a rejection where the WIT type has no `err` arm. An
`err` arm without a payload takes any thrown or rejected value, and logs it to
stderr. A synchronous export cannot take or return a `stream` or `future`, since
nothing would drive them after it returns, so componentizing one fails.

Each call to an `async` export runs its own event loop, which drives guest
promises, imported-function calls, and the builtins' work (`fetch`, timers, web
streams). A guest can `await fetch(...)` and a `setTimeout` callback in the same
call. The call finishes once its promise has settled and its imports, streams
and futures are done. Timers still pending then are dropped without running.

In the dynamic link mode, a world-level export named after a libc symbol, such
as `random`, collides with that symbol: the dynamic link puts `libc.so`'s exports
and the world's in one symbol namespace, so `wit-component`'s linker rejects the
duplicate.

#### Bundling and build caching

`imports` prints the module specifiers the guest may import for the selected
world, one per line: each imported interface's WIT name, and `external` for the
world's own imports. A module is listed only if the runtime registers it, which
it does if the module exports a function, a resource class, an error class, or
an enum or flags object. A bundler leaves these unresolved.
It takes the same WIT flags as `componentize`, and a world exporting
`wasi:http/handler` includes the `wasi:http` interfaces the runtime's handler
uses:

```bash
starling-componentize -d wit -w app --serve imports
```

`--version` (`-V`) prints the componentizer's version and the version of the
`init` interface it calls, and for the runtime in `$STARLING_RUNTIME`, or else
the embedded one, the SHA-256 digest of its module and the version of the
interface it implements. A build tool can tell from it when a rebuild would
produce a different component.

#### TypeScript declarations for a world

To scaffold (or type-check) the guest module a world expects, generate a
TypeScript declaration (`.d.ts`) from the WIT:

```bash
just ts-bindings <wit-path> <world>     # print the .d.ts to stdout
just ts-bindings --cli ''               # the built-in wasi:cli/run world
just ts-bindings --serve ''             # the built-in serve world

# or directly, with -o to write a file:
starling-componentize -d <wit-dir> -w <world> types -o world.d.ts
```

Add the generated file to the TypeScript program (a `files` or `include` entry in
`tsconfig.json`, or a `/// <reference path=…/>`). It declares nothing at file
level, which makes each `declare module` in it an ambient module declaration the
guest can import from:

- `starling:types/<world>`, where `<world>` is the world's qualified name (for
  example `starling:types/wasi:http/proxy@0.3.0`), declares every WIT type the
  world uses, a class per resource, and an error class per `err` type that has
  one. It is named after the world so declarations generated for different
  worlds can be part of one program. They live in a module rather
  than at file level so a WIT type named `permissions` or `response` cannot
  collide with the standard library. A name that several types share is
  qualified there with the interface's name, then also with the package's
  (`AThing`, `WasiHttpTypesErrorCode`). An interface the world both imports and
  exports has two classes per resource there, the host's and the guest's
  (`Item` and `ExportedItem`).
- One module per WIT interface the world imports, named by its WIT name, plus
  `external` for the world-level imports. Each declares the interface's
  functions and re-exports the types, resource classes and error classes the
  interface defines or `use`s under their WIT names, so
  `import { ErrorCode } from 'wasi:filesystem/types@0.3.0'` is the filesystem
  one. The runtime registers no module for an interface without functions,
  resources, error classes, enums or flags, so only the types of such a module
  are usable.
- `starling:guest` declares what the guest module exports. Reference it to have
  the compiler check the guest:

  ```ts
  import type * as Guest from 'starling:guest';
  export const greeter: typeof Guest.greeter = { /* … */ };
  ```

`starling:guest` declares every shape the world allows for each item (see
[Export names](#export-names)). `types` fails for a world whose exports cannot
all be named: a world-level function with a package layer's name.

The WIT→TypeScript types match the runtime's own value mapping. Numeric `list`s
are typed arrays (`list<u8>` is `Uint8Array`, `list<u64>` is `BigUint64Array`),
`u64`/`s64` are `bigint`, and a record's `option` field is an optional key.
`result<T, E>` on a return type is unwrapped to its `ok` payload (errors are
thrown), while a `result` parameter is the `{ tag, val }` union. `enum` is an
`enum` numbered from zero, and `flags` an `enum` of its bits plus a `number`
alias for a set of them. A set crosses as an int32, matching the result of `|`,
so the member for bit 31 is `1 << 31`. Each is imported from its interface's
module, which exports the object, so builds that compile files one at a time
(`isolatedModules`, as esbuild, swc, tsx and Bun do) can use them. A type of an
interface the world only exports has no module, and is a `const enum`, whose
members `tsc` inlines.

The two command worlds get bespoke declarations. `wasi:cli/run` is
`export function run(): void | Promise<void>`. For `wasi:http/handler`, the guest
either registers an `addEventListener('fetch', …)` listener, whose `FetchEvent`
the file declares, or implements `handle` as `wasiHttp.handler.handle`,
`handler.handle` or a bare `handle`. `handle` receives the WIT `request` and
returns a `Response`, or the WIT `response` resource when the world declares
one, or a promise of either. The file declares an overload of `fetch` taking the
WIT `request` when the world imports `wasi:http/types@0.3.x`. `types` takes the
interface's declaration from the runtime, the one `--runtime` or
`STARLING_RUNTIME` names or the embedded one, as componentizing does. Without a
runtime, the request in the built-in serve world, which declares no
`wasi:http/types`, is `unknown`.

A resource renders as a class with the constructor, methods and statics its WIT
declares. An imported resource's class declares `[Symbol.dispose](): void`, has a
private member so that no other object type-checks as one of its handles, and has
a private constructor when the WIT declares none. An exported resource's class
declares `[Symbol.dispose]` as optional, and is also a member of its interface
object (`Counter: typeof Counter`), since the runtime looks it up there.

Where the guest receives them, `stream<T>` renders as the DOM library's
`ReadableStream<T>` (`ReadableStream<Uint8Array>` for `stream<u8>`) and
`future<T>` as `Promise<T>`. Where the guest hands them over, they render as what
it may pass. The error class of an `err` type extends the global
`ComponentError`, which the file always declares. It is exported from the module
of the interface whose functions name the type, under the name they use, so a
`use`d or renamed type's class is exported by the interface that `use`s it. That
module does not re-export the payload type under the class's name. Name it as
`ErrorCode['payload']` instead.

The generated declarations are snapshot-tested, and each is type-checked with
`tsc --noEmit --strict` together with a consumer module that imports from every
module it declares and with hand-written guest modules for the naming shapes,
error classes and the serve worlds. That needs a TypeScript compiler: `npm ci` at
the workspace root installs the pinned one, and the test fails without it.

The componentizer's end-to-end suites build a runtime once, then componentize JS
apps and run them under wasmtime:

```bash
just test-componentize        # build the static runtime, run every suite against it
just test-componentize-only   # run the suites against a prebuilt runtime, without building one
just test-componentize-only echo_stream_u8   # one suite (name filter forwarded to libtest)
```

`test-componentize-only` picks the mode from `STARLING_LINK_MODE` (`static`, the
default, or `dynamic`) and the runtime from `STARLING_RUNTIME` or
`STARLING_DYLIB`. Each suite componentizes its own WIT world with Wizer. Against
the static runtime that takes a few seconds per world. Against the dylib it is
about 3.5 minutes for all of them, and about 1.5 once the compilation cache is
warm, since a world's bindings are linked into the component before it is
compiled and a cached compilation is reused by a rerun of that same world against
the same runtime build. When iterating on the tests, `test-componentize-only`
with a name filter runs just the world you care about and never rebuilds the
runtime.


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
