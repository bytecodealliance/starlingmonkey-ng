# StarlingMonkey

A JavaScript runtime for WASI and native platforms, built on
[SpiderMonkey](https://spidermonkey.dev/).

StarlingMonkey is designed to be extensible and provides safe high-level
abstractions for defining additional builtins as JS classes, WebIDL interfaces,
JS modules, and functions and properties on the global object.

[`starling-componentize`](#componentizing-javascript) builds WebAssembly components from a
JavaScript application and a WIT world.

---

## Contents

- [Running JavaScript](#running-javascript)
- [Built-in APIs](#built-in-apis)
- [CLI Reference](#cli-reference)
- [Componentizing JavaScript](#componentizing-javascript)
  - [Installing](#installing)
  - [Command line](#command-line)
  - [Export names](#export-names)
  - [HTTP servers](#http-servers)
  - [CLI tools](#cli-tools-wasiclirun)
  - [Components that serve and run](#components-that-serve-and-run)
  - [Initialization](#initialization)
  - [Refused worlds](#refused-worlds)
  - [Disabling WASI features](#disabling-wasi-features)
  - [The interfaces the runtime imports](#the-interfaces-the-runtime-imports)
  - [WIT values in JavaScript](#wit-values-in-javascript)
  - [Bundling and build caching](#bundling-and-build-caching)
  - [TypeScript declarations for a world](#typescript-declarations-for-a-world)
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
  - [The runtime component](#the-runtime-component)
  - [The componentizer](#the-componentizer)
- [Web Platform Tests (WPT)](#web-platform-tests-wpt)
- [GC Rooting Checks](#gc-rooting-checks)

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

### C++ built-ins

[crates/builtins/cpp-builtins](crates/builtins/cpp-builtins) runs builtins written for
StarlingMonkey's earlier C++ implementation through a compatibility layer. It provides `console`.

Builtins are tested against the
[Web Platform Tests](https://github.com/web-platform-tests/wpt) suite running
on both native and wasm targets.

---

## CLI Reference

```
starlingmonkey [OPTIONS] [SCRIPT_PATH]

Arguments:
  [SCRIPT_PATH]     Path to the entry JS/MJS file (default: ./index.js)

Options:
  -e, --eval <SCRIPT>                  Evaluate inline script instead of a file
  -i, --initializer-script <PATH>      Run an init script (classic, synchronous) before the content script
      --async-stacks                   Show async parent frames in Error.stack (always on under a debugger)
      --legacy-script                  Run as a classic script instead of an ES module
      --wpt-mode                       Enable WPT (Web Platform Tests) mode
      --web-security-restrictions[=<BOOL>]  Enforce the browser-security Fetch constraints and coarsen performance.now() and Event.timeStamp (default: only with --wpt-mode)
      --init-location <URL>            Override the location URL for initialization
      --serve <PORT>                   Serve HTTP on this port, dispatching fetch events
      --serve-isolated                 Handle each served request in its own global
      --dispatch-timeout <SECONDS>     Give up on a request whose respondWith hasn't settled
      --response-body-timeout <SECONDS>  Truncate a response body not fully sent by then
      --waituntil-timeout <SECONDS>    Stop driving a served request's leftover waitUntil work
      --end-to-end-timeout <SECONDS>   Wall-clock ceiling over all of a request's phases together
      --max-connection-buffer-size <BYTES>  Bytes read from a connection at once (default 408KiB, minimum 8KiB)
      --max-request-headers <COUNT>    Cap the number of header fields (default 128)
      --max-request-body-bytes <BYTES> Cap a request body (default 512MiB)
      --max-body-drain-bytes <BYTES>   Read at most this much of a body the handler ignored (default 256KiB)
      --max-connections <COUNT>        Serve at most this many connections at once (default 1024)
      --request-read-timeout <SECONDS> Give up on a client that stopped sending (default 30)
      --keepalive-timeout <SECONDS>    Close an idle kept-alive connection (default 30)
  -v, --verbose                        No effect
  -d, --debug                          No effect
      --strip-path-prefix <PREFIX>     No effect
  -h, --help                           Print help
```

`starlingmonkey --help` describes each option in full. The serve timeouts take whole or
fractional seconds, and `0` means no limit. Without a value, `--dispatch-timeout`,
`--response-body-timeout` and `--waituntil-timeout` have no limit, or 120 seconds under
`--wpt-mode`. A phase timeout longer than `--end-to-end-timeout` is rejected. The `--max-…`
limits, `--request-read-timeout`, `--keepalive-timeout` and `--serve-isolated` exist in native
builds only.

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
request whose evaluation fails is answered with a 500. The native server prints
`serving on http://127.0.0.1:<PORT>` on stderr once it listens. With `--serve 0`, it listens on
a port the OS assigns, which that line names.

---

## Componentizing JavaScript

`starling-componentize` builds a WebAssembly component from a JavaScript module and a WIT world.
The component implements the world's exports in JavaScript, and the application calls the world's
imports as ordinary JS functions. The component contains the whole runtime, SpiderMonkey
included, and runs under `wasmtime` or composes with other components through `wasm-tools`.

```bash
starling-componentize -w <wit-dir> -n <world> componentize <app.js> -o out.wasm
```

### Installing

- The npm package `@bytecodealliance/starling-componentize` installs a prebuilt binary for the
  platform from an optional dependency, one package per platform, and runs as
  `npx @bytecodealliance/starling-componentize`. Its `binaryPath()` export returns the binary's
  path. There are packages for macOS (arm64 and x64), Linux (arm64 and x64) and Windows (x64).
  The Linux binaries are statically linked, so they run with any C library.
- Each GitHub release has `starling.wasm`, a `starling-componentize` binary per platform and a
  `SHA256SUMS` file of their digests. The runtime and `starling-componentize` are
  released together, under the same version.
- In a checkout, `just install-componentize` builds the release runtime and installs
  `starling-componentize` with it embedded, so the installed binary needs no other file.
  [The componentizer](#the-componentizer) describes the other recipes.

### Command line

`starling-componentize [OPTIONS] <COMMAND>` takes one of three subcommands:
`componentize <input>` builds a component, [`types`](#typescript-declarations-for-a-world)
prints TypeScript declarations for the world, and [`imports`](#bundling-and-build-caching) lists
the modules the application may import. These options apply to all three, and may come before or
after the subcommand:

| Option | Effect |
|--------|--------|
| `-w`, `--wit <PATH>` | A `.wit` file or a directory of them. Repeatable. |
| `-n`, `--world-name <NAME>` | The world to target. Repeatable. Requires `-w`. |
| `--export-cli` | Export `wasi:cli/run` ([CLI tools](#cli-tools-wasiclirun)). |
| `--export-http` | Export `wasi:http/handler` ([HTTP servers](#http-servers)). |
| `--disable <FEATURES>` | Leave WASI features out of the component ([Disabling WASI features](#disabling-wasi-features)). |
| `--unstable-features <FEATURES>` | Include the WIT items annotated `@unstable(feature = …)` with these features. Repeat, or separate them with commas or whitespace. |
| `--all-unstable-features` | Include every WIT item annotated `@unstable`. |
| `-q`, `--quiet` | Print only errors. Without it, what the application prints while it initializes is shown. |

`componentize` also takes these:

| Option | Effect |
|--------|--------|
| `-o`, `--output <FILE>` | The component to write. Defaults to `js.wasm`. |
| `-p`, `--base-directory <DIR>` | The directory the application's relative `import`s resolve within. Defaults to the input file's directory. |
| `--runtime <PATH>` | The runtime build to link against (also for `types` and `imports`). |
| `--init-location <URL>` | The URL `globalThis.location` reflects while the application's top level runs. Without it, reading `location` there throws a `TypeError`, which fails componentization unless the application catches it. |
| `--deterministic-insecure-random-seed <SEED>` | Leave out `wasi:random/insecure-seed` ([Disabling WASI features](#disabling-wasi-features)). |
| `--no-cache` | Keep the compiled component out of wasmtime's compilation cache, which otherwise speeds up componentizing the same world again. |

`-w`, `--export-cli` or `--export-http` is required. A directory passed to `-w` brings the packages
in its `deps` directory along. Repeated `-w`s are read in order, so a package comes after the
packages it uses. A package several of them define is merged into one, with the interfaces, types
and functions of every definition, so libraries can each ship the part of a package they use. The
definitions must agree on the functions they have in common.

`-n` names a world in the last package loaded, or any world by its qualified name,
`namespace:package/world@version`. Without `-n`, the last package's only world is targeted.
Repeated `-n`s target several worlds, merged into one, `starling:componentize/merged`, which
imports and exports everything they do.

The runtime is the one `--runtime` names, or else the one the `STARLING_RUNTIME` environment
variable names, or else the one embedded in `starling-componentize`. It must be a `starling.wasm`
that `scripts/build-runtime.sh` built ([The runtime component](#the-runtime-component)). The
componentizer refuses one that a plain `cargo build` wrote, since it lacks the metadata the script
adds. The
componentizer also refuses a runtime whose version of the `starling:componentize/init` interface,
which `starling/wit/init.wit` declares, is not
[compatible](#the-interfaces-the-runtime-imports) with its own.

### Export names

The component's type is exactly the world's: it exports what the world declares and nothing else.
The runtime's own `wasi:cli/run` and `wasi:http/handler` are exported only when the world
declares them, which the worlds of `--export-cli` and `--export-http` do. The `init` interface the
componentizer calls is not exported.

The main module provides each function and resource class of an exported interface in exactly one
of the shapes below. For `greet` in the interface `greeter` of the package `test:app@1.0.0`:

```js
// The package layer: `test-app` in lowerCamelCase, then the interface's name.
// The version is not part of the name.
export const testApp = { greeter: { greet } };
// The interface layer.
export const greeter = { greet };
// A bare export.
export function greet(name) { /* … */ }
```

The interface layer is allowed only when no other exported interface has the same name, and a bare
export only when no other exported item or world-level function has the same name. Where the names
of two shapes coincide, the package layer's name takes precedence over an interface-layer and a
bare name, and an interface-layer name over a bare name. A world-level exported function is always
a bare export, and takes precedence over an interface-layer name. An interface the world exports
under a plain name (`export local: interface { … }`) has no package layer. Functions are
lowerCamelCase and resource classes UpperCamelCase. A reserved word such as `default` or `new` is
exported with an export clause (`export { make as new }`).

Two versions of one interface have the same package-layer and interface-layer names, so each is
provided only by the export named by its full WIT name. A TypeScript build needs
`"module": "es2022"` or later for such a name:

```js
export { v1 as "local:hello/hello@1.0.0", v2 as "local:hello/hello@2.0.0" };
```

Every export is resolved when the snapshot is taken. An item the main module provides in more than
one shape, or in none, fails componentization with a message listing the shapes it may take.

### HTTP servers

For an HTTP server, pass `--export-http`, or declare `export wasi:http/handler@0.3.0;` in your own
world, and the component exports `wasi:http/handler`. With `-w`, `--export-http` merges the world
`-n` selects with a world exporting the interface, so the application can import the interfaces its
WIT declares as well. The application serves requests in one of two ways:

- Register a listener with `addEventListener('fetch', …)`. The runtime dispatches each request as
  a `fetch` event.
- Export its own `handle`, in one of the shapes of `wasi:http/handler`'s exports:
  `wasiHttp.handler.handle`, `handler.handle` or a bare `handle`. `handle` takes a
  `wasi:http/types` request and returns a response: a `Response` object, such as the one `fetch`
  resolved to, whose body is then passed on without being read, or a `wasi:http/types` one.
  `fetch` takes the `wasi:http/types` request as its input, so `return fetch(request)` forwards a
  request unchanged. The request can't be used after `fetch` took it.

```js
export async function handle(request) {
  if (request.getPathWithQuery() === "/proxy") {
    return fetch(request);
  }
  return new Response(`hello from ${request.getPathWithQuery()}`);
}
```

Which of the two the application uses is determined when the snapshot is taken. An application
that does both, or neither, fails componentization. A world that declares the `wasi:http`
interfaces itself must agree with the runtime on every item both declare, or leave
`wasi:http/handler` empty (`interface handler {}`).

The `wasi:http/types` resources a `handle` receives have the methods the runtime's builtins use,
since the component imports nothing beyond them. A world that imports `wasi:http/types` itself,
with `-w`/`-n`, adds the functions it declares, for example `fields`' `get` and `has`.

### CLI tools (`wasi:cli/run`)

A command-line tool needs no WIT of its own. Pass `--export-cli`, which with `-w` merges the
selected world with one exporting `wasi:cli/run`, and have the application `export` a `run`
function:

```bash
starling-componentize --export-cli componentize <app.js> -o cli.wasm
wasmtime run -Sinherit-env=y,http=y cli.wasm
```

```js
// app.js
export async function run() {
  console.log("hello from a componentized CLI tool");
}
```

Invoking the component's `wasi:cli/run` export calls the JavaScript `run` export, which runs as
[an `async` export](#asynchronous-calls) does. Running the module as a plain script evaluates its
top level without calling `run`. Componentizing a module without a `run` function as a CLI tool
fails, naming the export to add.

### Components that serve and run

`--export-cli` and `--export-http` combine, alone or with `-w`, into one component exporting both
`wasi:cli/run` and `wasi:http/handler`. The application then exports `run`, and registers a `fetch`
listener or exports `handle`. The host picks the export it calls, so `wasmtime run` calls `run` and
`wasmtime serve` serves requests, each in instances of its own resumed from the same snapshot:

```bash
starling-componentize --export-cli --export-http componentize <app.js> -o app.wasm
```

### Initialization

The application's top level runs to completion under Wizer, and the component starts from the
snapshot taken after it. Its relative and JSON imports are read from the base directory, and a
top-level `await` may wait on timers and dynamic `import()`s. A module that imports the input file
gets the application's main module itself. Exports are resolved once the top level has finished,
so an export declared after a top-level `await` is found. A top level that throws, rejects, or
awaits a promise that never settles fails componentization with the reason. A dynamic
`import()` of a module the top level did not load reads the file when an export calls it, through
the component's own filesystem imports and relative to the working directory the host gives it, so
it fails with `--disable filesystem`.

A snapshot holds memory, not a running event loop, so the top level must leave no asynchronous
work behind when it finishes: no timer or interval still pending, no `fetch` it did not await.
Componentization fails otherwise, listing each piece of work with the stack that created it:

```text
Error: the application's top level finished evaluating with asynchronous work still pending, [...]
  - a pending `interval` timer, created at:
      startPolling@app.js:2:14
      @app.js:4:1
  - a host operation in flight, created at:
      @app.js:6:20
```

The component opens its own standard streams when it first writes after resuming. Calling an
import during initialization throws a `TypeError` naming it, since the host's imports are not
linked while the snapshot is taken, and fails componentization unless the application catches it.
An `async` import returns a promise rejected with that error instead, which fails
componentization when the top level awaits it.

### Refused worlds

Componentizing these worlds fails with an error naming the function or export:

- a world with a function that uses a `map`, a fixed-length `list` or `error-context`;
- a world with a synchronous export that takes or returns a `stream` or `future`, since no event
  loop drives their transfer (declare it `async`);
- a world with a world-level import function that takes a `borrow` anywhere in its parameters,
  such as a method of a resource declared directly in the world rather than in an interface,
  since the composition step cannot encode a `borrow` outside an interface (`types` still
  describes such a world);
- a world that exports `wizer-initialize`, `wasi-http-handler` or the
  `starling:componentize/init` interface, whose names the componentizer's own exports use;
- a world that exports `wasi:cli/run` or `wasi:http/handler` in a version not
  [compatible](#the-interfaces-the-runtime-imports) with the one the runtime provides;
- a world that imports an interface at several compatible versions, from its own WIT or the
  runtime's, where a version lacks a type or function that a lower one declares.

### Disabling WASI features

`--disable` takes a comma-separated list of `stdio`, `random`, `clocks`, `http` and `filesystem`,
and may be repeated. The interfaces of the named WASI features are left out of the component's
imports, and stubs that trap when called take their place. With all five disabled, a world that
imports no WASI produces a component that imports nothing beyond `wasi:cli/environment`,
`wasi:cli/exit` and `wasi:random/insecure-seed`. The application's top level still runs with every feature during
componentization.

A disabled clock's `now` returns zero instead of trapping, since the runtime reads it whatever the
application does. `Date.now()` and `performance.now()` then return constants, and only waiting on
the clock traps. `random` leaves out `wasi:random/random` only. A trap in a stub names the disabled
function in the backtrace, as in
`disabled-features!wasi:random/random@0.3.0#get-random-bytes (disabled)`.

`wasi:io` is left out once no remaining feature uses it. A feature that a remaining feature uses
cannot be disabled by itself: `filesystem` uses the clocks' types, so disabling `clocks` alone is
refused with a message listing every such use and the features to disable with it.

SpiderMonkey seeds its hash tables from `wasi:random/insecure-seed`, so the component imports it
unless `--deterministic-insecure-random-seed <SEED>` is given. SEED is a 128-bit number, in decimal
or with a `0x` prefix in hex, and `get-insecure-seed` then always returns its high and low 64 bits.
With SEED, an attacker can craft input that collides in those hash tables and degrades the
application's performance, so use it only with trusted input.

### The interfaces the runtime imports

The targeted world also imports every interface the runtime imports, whether it comes from `-w`,
`--export-cli` or `--export-http`: `wasi:cli`'s environment, exit, standard streams and terminals,
`wasi:clocks`, `wasi:filesystem`, `wasi:random`, `wasi:http/types` and `wasi:http/client`, except
those of the features `--disable` names. Each has the functions the runtime itself imports, so the
application can import them without adding to what the component imports:

```js
import { getEnvironment } from "wasi:cli/environment@0.3";
```

Two versions of an interface are compatible if they have the same canonical version, as the
component model defines it for interface names:

- the major number, for a version from `1.0.0` on, such as `1` for `1.2.3`;
- `0.` followed by the minor number, for a `0.x.y` version whose minor number isn't 0, such as
  `0.3` for `0.3.1`;
- the whole version without its build metadata, for a `0.0.x` version or a pre-release, such as
  `0.0.4` or `0.3.0-rc.1`. Such a version is compatible only with itself.

A world imports each interface once, even if its WIT and the runtime declare several compatible
versions of it: at the lowest of them if they all declare the same types and functions, and at the
highest otherwise. Its types then have their plain names. A version that lacks a type or function
of a lower compatible version is [refused](#refused-worlds).

A module specifier names an interface by its canonical version, as in `wasi:cli/environment@0.3`,
and `starling-componentize imports` and the
[TypeScript declarations](#typescript-declarations-for-a-world) name it that way. A specifier with
any compatible full version, such as `wasi:cli/environment@0.3.0`, resolves to the same module.

### WIT values in JavaScript

These WIT types work: all scalars (`u64` and `s64` are always BigInts, like the elements of a
`BigUint64Array`), `string`, `list` (numeric lists as typed arrays, `list<u64>` and `list<s64>` as
`BigUint64Array` and `BigInt64Array`, and a numeric list also takes a plain `Array`), `record`,
`tuple`, `variant`, `enum`, `option`, `result`, `flags`, `resource`s with constructors, methods,
static methods and borrows, and [`stream<T>` and `future<T>`](#streams-and-futures). Worlds that
use a `map`, a fixed-length `list` or `error-context` are [refused](#refused-worlds).

Imports are surfaced as ES modules the application can `import` (see
[`imports`](#bundling-and-build-caching)). An interface the world only exports has no module, so
the application cannot import the objects or error classes of its types.

Every imported resource has a class, including one that no constructor, method or static function
names. An imported resource wrapper has a `[Symbol.dispose]()` method that releases the host handle
early (so `using` works, and a second call does nothing). Disposing of a wrapper an `async` import
call still borrows releases the handle once that call returns. A guest class implementing an
exported resource may define `[Symbol.dispose]()`, which the runtime calls when the host drops the
resource. Its methods are looked up on the instance, so a subclass's override runs.

A variant's tag and a record's fields are their WIT names in lowerCamelCase (`db-null` is
`dbNull`). An `enum` crosses as its case index, and a `flags` set as an int32 of its bits, the bit
of the n-th flag being `1 << n`. The module of the interface defining an enum or flags type, and of
each interface that `use`s it, exports a frozen object under the type's name in that interface, in
UpperCamelCase, as a TypeScript enum's object would be: it maps each case's lowerCamelCase name to
its value and each value back to the name (`Qos.atLeastOnce` is `1`, `Qos[1]` is `"atLeastOnce"`).
An error class of the same name takes the name instead.

An import argument that does not match its WIT type throws a `TypeError` naming the argument, the
part of it that failed and its value, such as ``argument 1 of `bigArgument` `.a1`: expected a
string, got 5``. The arguments of one call are also checked together: a resource wrapper passed as
an `own` must not appear anywhere else in them, a wrapper the guest received as a `borrow`, or one
an `async` import call still borrows, cannot be passed as an `own`, and a `ReadableStream` must be
unlocked and passed only once. An export result that does not match traps the call, and the
message printed on stderr names the export and the part of the result the same way.

#### Synchronous exports

A synchronous export runs without an event loop. The microtasks it queues run before it returns. A
timer it starts throws a `TypeError` naming the export. A `fetch`, an `async` import, or a read of
a response body or of a stream from the host it starts returns a promise rejected with one.
Passing a stream or a future to an import throws one too, since nothing would transfer it. A value
it writes to a stream or future that an earlier asynchronous call returned, such as by settling a
promise lowered to a future, is written in the next asynchronous call.

#### Asynchronous calls

`async` exported functions return a JS `Promise` that the component lifts into a WASIp3 async
return. `async` imports are called as ordinary `async` JS functions and `await`ed. Their arguments
are checked and lowered when the call starts, so a change to an argument after the call, such as to
a typed array's elements, has no effect on what the host receives.

Each call to an `async` export runs its own event loop, which drives guest promises,
imported-function calls, and the builtins' work (`fetch`, timers, web streams), so one call can
`await` a `fetch` and run `setTimeout` callbacks. The call finishes once its promise has settled
and its imports, streams and futures are done. Timers still pending then are dropped without
running. An `async` export whose promise is still pending once its call's event loop has nothing
left to run traps, even if a later call could settle the promise.

#### Streams and futures

Streams and futures are web streams and promises:

- A `stream<T>` the guest receives is a `ReadableStream`. A `stream<u8>` is a readable byte stream
  of `Uint8Array` chunks, so BYOB readers work, and any other `stream<T>` has one chunk per
  element. The stream reads from the host only when something reads it, and cancelling it drops
  it.
- A `stream<T>` the guest hands over may be a `ReadableStream` or any async or sync iterable, such
  as an async generator or an array of chunks. A `stream<u8>` takes `ArrayBufferView` and
  `ArrayBuffer` chunks. A source that errors, or produces a chunk or element that does not match
  the stream's type, ends the stream early and logs why on stderr. After the host stops reading,
  the source is cancelled when it produces its next chunk. It is also cancelled when the call's
  event loop runs out of other work while the source produces nothing more. When the host doesn't
  read a stream element that is an imported resource, its handle is restored to the guest's
  wrapper. For one nested in a record, tuple, option or list, the wrapper holds no handle
  afterwards. A stream
  the guest received and returns unread is passed to the host unchanged.
- A `stream` without a payload type has one `undefined` chunk per element in both directions. A
  chunk the guest hands over that is not `undefined` ends the stream early, as a chunk that does
  not match a `stream<T>`'s type does.
- A `future<T>` the guest receives is a `Promise`. Its value is read whether or not the guest uses
  the promise. A `future<T>` the guest hands over may be a promise or any other value. A promise
  that never settles does not keep the call running, and its value is still written if it settles
  later. An `async` import the call started and did not await keeps the call running until the
  import returns, so if the import's host side waits on such a future, the call never finishes.
  An `async` export returning a `future<T>` returns it as the promise it produces, so the call
  completes before the future does.
- A `future` without a payload type is a `Promise` that fulfills with `undefined`. A promise the
  guest hands over as one must fulfill with `undefined`. Fulfilling with any other value or
  rejecting traps.

Streams work with `fetch` in both directions: a received `stream<u8>` can be a request body
(`fetch(url, { method: "POST", body, duplex: "half" })`), and a response's `body` can be returned
as a `stream<u8>`. A stream or body that JavaScript hasn't read from is handed over directly, so
its bytes don't pass through JavaScript. This also applies to a `stream<u8>` passed on as another
`stream<u8>` type, and to one piped through an identity `TransformStream`.

#### Errors

The `err` arm of a `result` crosses as an error. An import's `err`, or a rejected
`future<result<T, E>>`, throws or rejects with an instance of `E`'s class when `E` is a named type
other than a resource, and a `ComponentError` otherwise. `E`'s class is named after the type,
extends `ComponentError`, and holds the payload as `payload`. It is exported from the module of the
interface whose function names the type, under the name that interface uses for it
(`import { ErrorCode } from "wasi:http/types@0.3"`), so a `use`d or renamed type's class is
exported by the interface that `use`s it. The class of a type the world names is exported from
`external`. A type that an interface the world only exports `use`s from another interface has that
interface's class.

An export failing, or a promise the guest hands over as a `future<result<T, E>>` rejecting,
produces an `err` from:

- a `ComponentError` or an instance of a class extending it: its `payload`,
- any other `Error`: its `message`, if `E` is a `string`, or an `enum` or `variant` with a
  payload-less case whose JS (camelCase) name it is,
- any other value: the value itself.

Anything else traps, as does a rejection where the WIT type has no `err` arm. An `err` arm without
a payload takes any thrown or rejected value, and logs it to stderr.

### Bundling and build caching

`imports` prints the module specifiers the guest may import for the selected world, one per line:
each imported interface's WIT name with its canonical version (see
[The interfaces the runtime imports](#the-interfaces-the-runtime-imports)), and `external` for the
world's own imports. A module is listed only if the runtime registers it, which it does if the
module exports a function, a resource class, an error class, or an enum or flags object. A bundler
has to leave these unresolved. A world exporting `wasi:http/handler` includes the `wasi:http`
interfaces the runtime's handler uses:

```bash
starling-componentize -w wit -n app --export-http imports
```

`--version` (`-V`) prints the componentizer's version and the version of the `init` interface it
calls, and for the runtime in `$STARLING_RUNTIME`, or else the embedded one, the SHA-256 digest of
its module and the version of the interface it implements. A build tool can tell from it when a
rebuild would produce a different component.

### TypeScript declarations for a world

To scaffold (or type-check) the guest module a world expects, generate a TypeScript declaration
(`.d.ts`) from the WIT:

```bash
starling-componentize -w <wit-dir> -n <world> types -o world.d.ts
starling-componentize --export-cli types    # the built-in wasi:cli/run world
starling-componentize --export-http types   # the built-in serve world
```

Without `-o`, `types` prints the declarations to stdout. Add the generated file to the TypeScript
program (a `files` or `include` entry in `tsconfig.json`, or a `/// <reference path=…/>`). It
has no file-level declarations, which makes each `declare module` in it an ambient module
declaration the guest can import from:

- `starling:types/<world>`, where `<world>` is the world's qualified name (for example
  `starling:types/wasi:http/proxy@0.3.0`), declares every WIT type the world uses, a class per
  resource, and an error class per `err` type that has one. It is named after the world so
  declarations generated for different worlds can be part of one program. They live in a module
  rather than at file level so a WIT type named `permissions` or `response` cannot collide with the
  standard library. A name that several types share is qualified there with the interface's name,
  then also with the package's (`AThing`, `WasiHttpTypesErrorCode`). An interface the world both
  imports and exports has two classes per resource there, the host's and the guest's (`Item` and
  `ExportedItem`).
- One module per WIT interface the world imports, named by its WIT name with its canonical
  version, plus `external` for the world-level imports. Each declares the interface's
  functions and re-exports the types, resource classes and error classes the interface defines or
  `use`s under their WIT names, so `import { ErrorCode } from 'wasi:filesystem/types@0.3'` is the
  filesystem one. The runtime registers no module for an interface without functions, resources,
  error classes, enums or flags, so only the types of such a module are usable.
- `starling:guest` declares what the guest module exports. Reference it to have the compiler check
  the guest:

  ```ts
  import type * as Guest from 'starling:guest';
  export const greeter: typeof Guest.greeter = { /* … */ };
  ```

`starling:guest` declares every shape the world allows for each item (see
[Export names](#export-names)). `types` fails for a world whose exports cannot all be named: a
world-level function with a package layer's name.

The WIT→TypeScript types match the runtime's own value mapping. Numeric `list`s are typed arrays
(`list<u8>` is `Uint8Array`, `list<u64>` is `BigUint64Array`), `u64`/`s64` are `bigint`, and a
record's `option` field is an optional key. `result<T, E>` on a return type is unwrapped to its
`ok` payload (errors are thrown), while a `result` parameter is the `{ tag, val }` union. `enum` is
an `enum` numbered from zero, and `flags` an `enum` of its bits plus a `number` alias for a set of
them. A set crosses as an int32, matching the result of `|`, so the member for bit 31 is
`1 << 31`. Each is imported from the module that exports its object, so builds that compile files
one at a time (`isolatedModules`, as esbuild, swc, tsx and Bun do) can use them. A type of an
interface the world only exports is a `const enum`, whose members `tsc` inlines.

For the two command worlds, `wasi:cli/run` is `export function run(): void | Promise<void>`. For
`wasi:http/handler`, the guest either registers an `addEventListener('fetch', …)` listener, whose
`FetchEvent` the file declares, or implements `handle` in one of the shapes
[HTTP servers](#http-servers) lists. `handle` receives the WIT `request` and returns a `Response`,
or the WIT `response` resource when the world declares one, or a promise of either. The file
declares an overload of `fetch` taking the WIT `request` when the world imports
`wasi:http/types@0.3.x`. `types` takes the interface's declaration from the runtime the
[command line](#command-line) selects. Without a runtime, the request in the built-in serve world,
which declares no `wasi:http/types`, is `unknown`.

A resource renders as a class with the constructor, methods and statics its WIT declares. An
imported resource's class declares `[Symbol.dispose](): void`, has a private member so that no
other object type-checks as one of its handles, and has a private constructor when the WIT
declares none. An exported resource's class declares `[Symbol.dispose]` as optional, and is also a
member of its interface object (`Counter: typeof Counter`), since the runtime looks it up there.

Where the guest receives them, `stream<T>` renders as the DOM library's `ReadableStream<T>`
(`ReadableStream<Uint8Array>` for `stream<u8>`) and `future<T>` as `Promise<T>`. Where the guest
hands them over, they render as what it may pass. A `stream` without a payload type renders with
`undefined` chunks (`ReadableStream<undefined>`), and a `future` without one with `void`
(`Promise<void>`). The error class of an `err` type extends the global `ComponentError`, which
the file always declares, and is exported from the module [Errors](#errors) names. That module
does not re-export the payload type under the class's name. Name it as `ErrorCode['payload']`
instead.

---

## Extending with Custom Builtins

StarlingMonkey provides proc macros for exposing Rust code to JavaScript. All
builtins in the `web-globals` crate are implemented using these macros.

[docs/builtin-development.md](docs/builtin-development.md) describes the practices builtins
follow, and [docs/rooting.md](docs/rooting.md) describes GC rooting.

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

A borrow from `self.data()` or `self.data_mut()` must not be held across a call into JavaScript
(see [Borrows and re-entrancy](docs/builtin-development.md#borrows-and-re-entrancy)).

The `#[jsclass]` macro generates two types from the annotated struct, allowing
the type to be used from JS and Rust while ensuring proper GC rooting:

| Generated type | Purpose |
|----------------|---------|
| `CounterImpl` | Inner data struct implementing `ClassDef` (`#[doc(hidden)]`). |
| `Counter<'s>` | Stack newtype wrapping `Stack<'s, CounterImpl>`, for use within a GC scope. |

A long-lived reference to an instance is a `Heap<CounterImpl>` in a traced struct (see
[docs/rooting.md](docs/rooting.md)).

**`#[jsclass]` options:**

```rust
#[jsclass(name = "MyCounter")]         // override the JS class name
#[jsclass(extends = Parent)]           // set up a prototype chain
#[jsclass(js_proto = "Error")]         // inherit from a built-in JS prototype
#[jsclass(to_string_tag = "MyClass")]  // set Symbol.toStringTag
#[jsclass(hidden)]                     // register without defining the constructor on the global
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
| `T: ToJSVal` | Value returned to JS. |
| `Result<T, E>` where `E: ThrowException` | `Ok` → value; `Err` → typed JS exception. |
| `Self` (from `#[static_method]` / `#[method]`) | New JS instance of the same class. |
| `PromiseFuture` | JS `Promise` resolved to the result of a Rust future. |
| `Ref<'_, T>` where `T: ToJSVal + ?Sized` | Value returned to JS, converted from data the class still owns. |

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
math_utils::register(&scope);
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

Same options as `#[jsclass]`: `name`, `extends`, `js_proto`, `to_string_tag`, `hidden`.

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

[docs/rooting.md](docs/rooting.md) describes when and how to store JS references.

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
        PromiseFuture::from_value(async move {
            // ... async work ...
            Ok::<_, String>("response body".to_string())
        })
    }
}
```

The method returns the `Promise` to JS immediately, and the future is queued on
the event loop. `ScriptEvaluation::run_to_completion`, which
`libstarling::run` drives for you, polls it and settles the `Promise` with the
future's `Ok`/`Err`. `PromiseFuture::new_void` takes a future resolving to
`Result<(), E>`, and `PromiseFuture::new` one resolving to a `PromiseOutcome`, for
settling that needs the scope.

---

## Building and Testing

**Prerequisites:**

- [Rust toolchain](./rust-toolchain.toml), and a beta toolchain for wasm32-wasip3 builds
- [just](https://github.com/casey/just)
- [WASI-SDK 34](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34)
- [wasmtime](https://github.com/bytecodealliance/wasmtime), to run the wasm tests and the wasm WPT
  harness
- [wasm-tools](https://github.com/bytecodealliance/wasm-tools), to build the runtime component
- [Node.js](https://nodejs.org/), to run the WPT harness, the componentizer's TypeScript test and
  `just npm-componentize`

[`.github/tool-versions.env`](.github/tool-versions.env) lists the versions CI uses.

Checking, building, and testing is done using a [`justfile`](justfile).
Commands include:

```bash
just build             # debug build
just test              # all Rust tests, use `-p` for specific packages
just wpt-test          # all Web Platform Tests, optionally filtered by a pattern
just fmt               # format code
just clippy            # run clippy
just check             # fmt-check + clippy + the examples' tests + the componentizer's unit tests
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

`WASM_TARGET` selects the wasm target of the recipes above, the wasm WPT recipes and
`just test-wizer`, as `p2`, `p3` or a full triple. It defaults to `p2`:

```bash
WASM_TARGET=p3 just test-wasm
WASM_TARGET=p3 just wpt-test-wasm
```

`rust-toolchain.toml` pins a toolchain that ships no wasm32-wasip3 std, so `p3` builds through
`cargo +beta`, or the toolchain `STARLING_BETA` names, and `p3` needs wasi-sdk 34, the first
with a wasm32-wasip3 sysroot.

`cargo build` produces the native binary `target/debug/starlingmonkey`. A wasm build produces the
component `target/wasm32-wasip2/debug/starling.wasm` from the crate's `cdylib` target.

### The runtime component

`just build-runtime` builds the runtime component releases ship,
`target/wasm32-wasip3/release/starling.wasm`, with SpiderMonkey, libc and libc++ statically linked
and dead-code eliminated. `just build-runtime-debug` builds it with `debugmozjs`, at
`target/wasm32-wasip3/debug/starling.wasm`. `RUNTIME_TARGET` selects their target as `WASM_TARGET`
does, and defaults to `p3`. `RUNTIME_TARGET=p2` builds for wasm32-wasip2 from the stable toolchain
and an older SDK.

The component runs scripts under `wasmtime run`, serves under `wasmtime serve`, and is the runtime
`starling-componentize` links against. `just test-runtime` builds the debug component and runs two
scripts with it under wasmtime: a hello-world that logs before and after a timer, and a top level
that rejects after a timer, which must fail the run.

Running a script calls no `run` export: the script's top level and the event loop it starts are
the whole program. A main module's `run` export is called only in an instance pre-initialized with
Wizer, such as one [componentized as a CLI tool](#cli-tools-wasiclirun) or one wizened directly
through the runtime's `wizer-initialize` export:

```bash
wasmtime wizer --keep-init-func=true --dir=. -Scli=y,http=y \
    --env STARLINGMONKEY_CONFIG=app.js target/wasm32-wasip3/release/starling.wasm -o tool.wasm
wasmtime run -Shttp=y tool.wasm
```

A snapshot whose main module registers no `fetch` listener and exports no `run` function is
refused, since it can neither serve nor run. As when [componentizing](#initialization), the top
level has to settle and leave no work behind, such as a timer or a `fetch` in flight. A host
resource the script keeps without pending work, such as the body of a `fetch` response it did not
read, is not detected. Its handle is not valid in an instance resumed from the snapshot.

### The componentizer

`componentize` is a Cargo workspace of its own, so its recipes run cargo from its directory.

`just componentize <args>` builds `starling-componentize` from the checkout and runs it with
`<args>` in the directory you run it from. It embeds `target/wasm32-wasip3/release/starling.wasm`,
the runtime `just build-runtime` writes, or no runtime if there is none. `STARLING_EMBED_RUNTIME`
names another runtime to embed at build time, and the build fails if that file can't be read.
`just install-componentize` and `just npm-componentize [VERSION]` build the release runtime for
`RUNTIME_TARGET` and embed it. `npm-componentize` assembles the npm packages for the current
platform in `target/npm`, and their tarballs in `target/npm/tarballs`.
`just ts-bindings <wit-path> [<world>]` prints the TypeScript declarations for a world.

```bash
just test-componentize        # build the debug runtime, run every suite against it
just test-componentize-only   # run the suites against a prebuilt runtime, without building one
just test-componentize-only large_byte_stream   # only the tests whose name matches
just test-componentize-unit   # the componentizer's unit tests, which need no runtime build
```

The suites componentize JS applications and run them under wasmtime. `test-componentize-only`
reads the runtime from `STARLING_RUNTIME`, by default the one `just build-runtime-debug` writes.
It runs several suites at a time, one per 8 GiB of memory by default, and prints a suite's output
only if it fails. `COMPONENTIZE_JOBS` sets the number of suites, and
[`scripts/test-componentize.sh`](scripts/test-componentize.sh) describes the other settings.
The `wit_to_ts` suite type-checks the generated declarations with the TypeScript compiler that
`npm ci` at the repository root installs, and fails without it.

A change to `starling/wit/init.wit` needs a version of its package that is not
[compatible](#the-interfaces-the-runtime-imports) with the latest release's, which CI checks with
`scripts/check-init-wit.sh`. The `release` workflow
publishes the runtime, the componentizer binaries and the npm packages, and
[its header](.github/workflows/release.yml) describes each kind of run.

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

Use `just wpt-test-wasm` to run under WebAssembly instead of native, and
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

The `js` crate's GC-safety guarantees rest on annotations that a custom linter,
[crown](./crown) (adapted from [Servo's lint of the same name][crown]), checks
statically, and on SpiderMonkey's dynamic GC checks. `just check-gc` runs crown
and `just gc-zeal` the dynamic checks, and `just check-all` runs both.
[docs/rooting.md](docs/rooting.md) describes them.

[crown]: https://github.com/servo/servo/tree/main/support/crown
