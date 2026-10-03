# Node.js compatibility

This document specifies StarlingMonkey's Node.js compatibility layer: which Node APIs exist, how
they are enabled, and how they behave where WASI cannot provide what Node does. It is the
specification for the implementation in `crates/builtins/node` and the module-system support in
`crates/core-runtime/src/module.rs`, and it is kept in sync with both.

## Enabling Node compatibility

Node compatibility is opt-in:

- The CLI takes `--node-compat`.
- `starling-componentize` takes `--node-compat`, which it passes to the runtime's `init` export
  (`starling:componentize/init@0.2.0`).

Without the flag the runtime is web-only: no Node globals, no `node:` modules, no CommonJS, and
module resolution uses the web condition set (see [Module resolution](#module-resolution)).

With the flag:

- Every Node.js builtin module can be imported, with and without the `node:` prefix (except the
  modules Node only has with the prefix, such as `node:test`), and `require`d.
- `process`, `global`, `Buffer`, `setImmediate` and `clearImmediate` are installed on every
  global, and `setTimeout` and `setInterval` return `Timeout` objects (see
  [`node:timers`](#nodetimers-nodetimerspromises-and-the-timer-globals)).
- CommonJS modules can be `require`d and imported from ES modules, and an entry script can be one.
- Module resolution adds the `node` condition.
- `console.log`, `info`, `debug`, `warn`, `error` and `trace` format their arguments with
  `util.format`, `console.dir` uses `util.inspect` with its options, and `warn` and `error` print
  to stderr without the runtime's `Warn: ` and `Error: ` prefixes, as Node prints them.

## Implementation rules

- Every Node module is implemented natively in Rust with the proc macros (`#[jsmodule]`,
  `#[jsclass]`/`#[jsmethods]`, `#[jsfunction_class]`, `#[jsnamespace]`). No JavaScript from
  Node's `lib/` is vendored.
- Classes that Node code subclasses ES5-style (`EventEmitter.call(this)` with `util.inherits`) or
  whose prototype methods it copies onto other objects are `#[jsfunction_class]`es: constructor
  functions callable without `new`, whose state lives in JS properties as in Node, so every object
  can be an instance. Rust state lives in a `#[jsclass]` object held in one of those properties.
  Their methods and static methods are enumerable, as the members Node assigns to its
  function-style classes are, except for the classes Node writes with `class` syntax, which use
  `#[jsfunction_class(class_syntax)]`. The classes that return a new instance when called without
  `new`, such as the stream classes, `net.Socket` and the zlib classes, use
  `#[jsfunction_class(self_construct)]`.
- Node APIs have no step-structured specification. Each implemented function's doc comment links
  to the section of the Node.js API documentation it implements, and states where the
  implementation deviates from it.
- Every builtin module can be imported. A module or export that isn't implemented is a stub with
  Node's export names (`crates/builtins/node/src/exports_table.rs`, generated from Node.js by
  `scripts/node-builtin-exports.mjs`): functions throw `ERR_METHOD_NOT_IMPLEMENTED` when called,
  classes when constructed (and can be extended), and primitive constants have Node's values.
- Errors that Node gives a `code` have the same `code` and constructor (`Error`, `TypeError`,
  `RangeError`, `URIError`). Those with an `ERR_` code have Node's `toString`, which returns
  `` `${name} [${code}]: ${message}` ``, as a non-enumerable own property.
- Where Node's behavior comes from its JavaScript or C++ implementation rather than from its
  documentation, such as argument validation order or event order, the implementation ports that
  code. [Testing](#testing) describes how the behavior is compared with Node's.

## Module resolution

Specifiers resolve with `oxc_resolver` using these `package.json` `exports`/`imports` conditions,
in addition to `default`:

| Mode | ES module `import` | CommonJS `require` |
|---|---|---|
| web (default) | `starlingmonkey`, `import`, `module-sync` | n/a |
| `--node-compat` | `starlingmonkey`, `node`, `import`, `module-sync` | `starlingmonkey`, `node`, `require`, `module-sync` |

Object order in the package's `exports` map decides between matching conditions, as in Node.

Recognized extensions are `.js`, `.mjs`, `.cjs` and `.json` for imports, and `.js`, `.cjs`,
`.json` and `.node` for `require`. Builtin module names take precedence over packages of the same
name.

A file's format follows Node: `.mjs` is an ES module, `.cjs` is CommonJS, and `.js` follows the
`type` of the nearest `package.json`. A `.js` file with no `type` in reach is CommonJS if it
compiles as a CommonJS module body, and an ES module otherwise. Without `--node-compat`, every
JavaScript file is an ES module. `.node` native addons are refused with `ERR_DLOPEN_FAILED`.

## CommonJS

- A CommonJS module's source is the body of a function with the parameters `exports`, `require`,
  `module`, `__filename` and `__dirname`, called with `this` its `module.exports`. A byte order
  mark is removed and a leading `#!` line ignored.
- Modules are cached in `require.cache` (`Module._cache`) under their canonical path before they
  run, so a cycle gets the partial `module.exports`. Deleting an entry makes the next `require`
  load the module again. A module that throws is removed from the cache.
- `require` loads builtin modules (their `default` export), CommonJS modules, JSON files, and ES
  modules. An ES module is loaded, linked and evaluated synchronously, and `require` returns its
  namespace, or its export named `"module.exports"` if it has one. An ES module graph whose
  top-level `await` doesn't settle synchronously can't be `require`d.
- `require.resolve(request)` returns the resolved path, or the builtin module name.
- `module.createRequire(filename)` takes an absolute path or a `file:` URL.
- An ES module importing a CommonJS module gets its `module.exports` as the `default` export and
  as the `"module.exports"` export, and each own enumerable string-keyed property of
  `module.exports` as a named export.
- A componentized program's main module can be a CommonJS module too, by the same rules as the
  CLI's entry script. The component exports the properties of its `module.exports`.
- Loading a module file blocks, which isn't allowed once the runtime serves requests (see
  [Synchronous host I/O](#synchronous-host-io)). So in a script that serves requests, loading a
  CommonJS module also resolves and reads the CommonJS modules and JSON files its source
  `require`s with a string literal, transitively, while the top level runs, or before the Wizer
  snapshot. A later `require` of one of them loads it from memory. A `require` whose specifier
  isn't a literal, or that names an ES module, still reads the file, and throws
  `ERR_BLOCKING_NOT_ALLOWED` while serving.

Deviation: Node determines a CommonJS module's named exports by analyzing its source, and runs it
when the importing graph evaluates. StarlingMonkey runs it when the importing graph loads, before
any ES module in that graph evaluates, and exports the properties `module.exports` has then.

## `import.meta`

Every file-backed ES module has:

- `import.meta.url`: the module's `file:` URL.
- `import.meta.filename` and `import.meta.dirname`: the module's absolute path and its directory.
- `import.meta.resolve(specifier)`: the `file:` URL `specifier` resolves to from this module, or
  the builtin module's `node:` name, resolved synchronously without loading anything.
- `import.meta.main`: `true` for the entry module.

These exist with and without `--node-compat`. Modules with no file (`-e` scripts, native modules)
have `resolve` and `main` only.

## Stack traces

`Error.captureStackTrace` and `Error.stackTraceLimit` are SpiderMonkey's. V8's
`Error.prepareStackTrace` hook is supported: while it is a function, the `stack` of an error, and
of an object `captureStackTrace` was called on, is the hook's result for the error and an array of
`CallSite` objects, one per frame of SpiderMonkey's stack.

- `getFunctionName`, `getFileName`, `getLineNumber`, `getColumnNumber`, `isEval`, `getEvalOrigin`,
  `isAsync` and `toString` come from the stack. `getThis`, `getFunction`, `getTypeName` and
  `getMethodName` return `undefined` or `null`, and `isNative`, `isConstructor` and
  `isPromiseAll` return `false`. `isToplevel` returns `true` unless the function's name is a
  property path such as `obj.method`.
- V8 calls the hook when `stack` is first read. StarlingMonkey calls it then for errors, but for
  `captureStackTrace` during the call, so a hook removed before reading `stack` still applies.
- The hook's result replaces `stack` as an own data property, so the hook runs once per object.
  `stack` reads while the hook runs get SpiderMonkey's string.
- Without the hook, `stack` is SpiderMonkey's format, `name@file:line:column` per line, without
  V8's `Error: message` first line.

## Synchronous host I/O

Synchronous Node APIs that touch the host, such as `require()` of a file on disk or
`fs.readFileSync`, block the current task while the host does the I/O. That is allowed in
`wasi:cli/run`, during Wizer initialization, and in a served script's top level. Once the runtime
serves requests, they throw `ERR_BLOCKING_NOT_ALLOWED` instead, because a blocked task holds the
component instance's lock and stalls every request the instance serves concurrently.
`--allow-blocking-host-io` allows blocking while serving. With `--serve-isolated`, which serves
one request at a time and evaluates the script for each, blocking is always allowed. Inside
synchronous component exports, which must not block at all, they always throw.
`core_runtime::event_loop::blocking_host_io_refusal` implements the policy. On wasm, the callback
and promise forms of most `node:fs` operations suspend instead of blocking, so they work while
serving (see [`node:fs`](#nodefs-and-nodefspromises)). The others block and follow the same
policy.

## Runtime constants

- `process.version` is `v24.21.0`, the Node.js version whose API surface is targeted and whose
  tests and behavior the implementation is compared with, and `process.versions.node` is
  `24.21.0`. `process.versions.starlingmonkey` is the runtime's version.
- `process.platform` is `'linux'` on wasm, since WASI paths are POSIX paths, and the host's
  platform natively (`darwin`, `linux`, `win32`).
- `process.arch` is `'wasm32'` on wasm, and the host's architecture in Node's naming natively.

## Modules

### `node:path`

The POSIX implementation, which `path.posix` and `node:path/posix` also are. `path.win32` is not
implemented. `path.matchesGlob` is a stub.

### `node:crypto`

Hashes, HMACs and random values are implemented: `createHash`, `createHmac`, `hash`, `getHashes`,
`randomBytes`, `randomFill`, `randomFillSync`, `randomInt`, `randomUUID`, `getRandomValues` and
`timingSafeEqual`. `webcrypto` is the global `crypto`, and `subtle` is its `subtle` if it has one.
The rest of the module is stubs, including the key derivation functions `pbkdf2`, `scrypt` and
`hkdf`.

- The digests are MD5, RIPEMD-160, SHA-1, SHA-224, SHA-256, SHA-384, SHA-512, SHA-512/224,
  SHA-512/256 and SHA3-224, SHA3-256, SHA3-384 and SHA3-512, under their names with or without an
  `RSA-` prefix. The SHAKE extendable-output functions are missing.
- `Hash` and `Hmac` are the classes `createHash` and `createHmac` instantiate. Their objects
  have `update`, `digest` and, for `Hash`, `copy`, but aren't streams.
  `digest` returns a `Buffer` for an encoding it doesn't know, as Node does.
- Random bytes come from the operating system natively, and from `wasi:random/random` on wasm.
- The callbacks of `randomBytes` and `randomFill` run in a later turn of the event loop, and
  `randomInt`'s in a tick.

### `node:diagnostics_channel`

`channel`, `hasSubscribers`, `subscribe`, `unsubscribe`, `tracingChannel` and `Channel` are
implemented, including `bindStore`, `unbindStore` and `runStores`, and `TracingChannel`'s
`traceSync`, `tracePromise` and `traceCallback`.

- A realm keeps its channels by name for as long as it exists. Node collects a channel that has
  no subscribers and is no longer referenced.
- An exception a subscriber or a store's transform throws is uncaught, so it reaches the
  `uncaughtException` listeners, and the remaining subscribers still run.

### `node:events`

`EventEmitter`, the module's `default` export, keeps an emitter's listeners in `_events`,
`_eventsCount` and `_maxListeners`, as Node does. Its static members (`defaultMaxListeners`,
`errorMonitor`, `captureRejectionSymbol`, `captureRejections`, `usingDomains`) are also named
exports. `getEventListeners`, `getMaxListeners`, `setMaxListeners` and `listenerCount` are
implemented, as are `once` and `on`, for emitters and `EventTarget`s, except that
`getEventListeners` can't list an `EventTarget`'s listeners. `on`'s `highWaterMark` and
`lowWaterMark` options call the emitter's `pause` and `resume` if it has them.
`addAbortListener` and `EventEmitterAsyncResource` are stubs.

- Exceeding the listener limit emits a `MaxListenersExceededWarning`.
- With the `captureRejections` option, a promise a listener returns that rejects emits `error`
  in a later tick, or calls the emitter's `Symbol.for('nodejs.rejection')` method if it has one.
  The static `EventEmitter.captureRejections` is a constant `false`.
- An emitter has the own symbol-keyed properties `kCapture` and `shapeMode`, as in Node.

### `node:process` and `process`

`process` is an `EventEmitter`, and `node:process`'s `default` export, with its properties as named
exports.

- `env` and `argv` read the host's environment and arguments on first use after the runtime starts
  or resumes from a Wizer snapshot. On wasm they come from `wasi:cli/environment`. Assigning
  either replaces it until the next resume. `env` converts the values assigned to it to strings.
  `argv` is the program, followed by the entry module's path if it has one.
- `nextTick(callback, ...args)` queues the callback in a tick queue, which runs before the
  promise reactions and other microtasks each time they run. Ticks queued by microtasks run once
  the microtasks are done. As in Node, an ES module entry runs its first promise reactions before
  its ticks. A tick runs with the async context current when it was queued.
- When a script runs as a command, the process emits `beforeExit` once the event loop has no work
  left, runs the loop again if a listener queued work, and emits `exit` with the exit code at the
  end. An exception nothing caught, including one the entry script's top level throws, or a
  rejection nothing handled, goes to the `uncaughtException` or `unhandledRejection` listeners,
  and without one ends the process with exit code 1 after printing it as Node does: the
  `util.inspect` of an error, followed by the Node.js version. An `uncaughtException`
  listener that throws ends it with exit code 7. `setUncaughtExceptionCaptureCallback` is
  implemented.
- `config` holds the `target_defaults` and `variables` fields test suites probe.
- `exit(code)` emits `exit`, flushes stdout and stderr, and exits with `code`, `process.exitCode`,
  or 0. `exitCode` accepts the values Node accepts.
- `emitWarning` follows Node: it emits `warning` in a tick, and prints the warning to stderr, the
  first one followed by Node's hint about `--trace-warnings` or `--trace-deprecation`.
- `getBuiltinModule` returns a builtin module's namespace.
- `stdout` and `stderr` are `EventEmitter`s with a synchronous `write(chunk[, encoding][,
  callback])`. Their `isTTY` is `true` for a terminal and `undefined` otherwise.
- `hrtime`, `hrtime.bigint` and `uptime` read the monotonic clock.
- `getActiveResourcesInfo()` lists `Timeout` for each referenced `setTimeout` and `setInterval`
  timer and `Immediate` for each referenced `setImmediate` whose callback hasn't started. Sockets,
  servers and pending fs requests aren't listed.
- `cwd()` and `chdir()` use the process's working directory, which WASI emulates in the guest.
- `pid` is 1 on wasm. `memoryUsage()` and `cpuUsage()` report 0. `umask()` is `0o022`, and
  `getuid()`, `geteuid()`, `getgid()` and `getegid()` are 0. `kill()` throws
  `ERR_METHOD_NOT_IMPLEMENTED`.

### `node:module`

`Module` is the `default` export. `createRequire`, `isBuiltin`, `builtinModules`,
`syncBuiltinESMExports` (which does nothing), `_cache`, `globalPaths` and `constants` are
implemented, as are `module.require`, `module.load` and `module._compile`. `Module._extensions`
has loaders for `.js`, `.json` and `.node` files, which `require` doesn't consult. The rest is
stubs.

The legacy internal modules `_stream_duplex`, `_stream_passthrough`, `_stream_readable`,
`_stream_transform` and `_stream_writable` are the `stream` classes. `_http_agent`,
`_http_client`, `_http_common`, `_http_incoming`, `_http_outgoing` and `_http_server` hold the
`http` classes and helpers Node.js exports from them. Their other exports, and the other internal
modules, are stubs.

### `node:async_hooks`

`AsyncLocalStorage` and `AsyncResource` keep their stores in an async context: a `Map` from each
`AsyncLocalStorage` to its store, as in Node's `AsyncContextFrame` implementation. Setting a store
replaces the context with a copy that holds it. Promise reactions, `process.nextTick` and
`queueMicrotask` callbacks, timers and other event-loop tasks capture the context current when
they are created, and run with it. Event listeners run with the context current when the event is
emitted.

- `run`, `exit`, `getStore`, `enterWith`, `disable`, `AsyncLocalStorage.bind`,
  `AsyncLocalStorage.snapshot`, and the `defaultValue` and `name` options are implemented.
- `AsyncResource`'s `runInAsyncScope`, `bind`, `asyncId`, `triggerAsyncId` and `AsyncResource.bind`
  are implemented. `emitDestroy` does nothing.
- `executionAsyncId()` returns 1, `triggerAsyncId()` returns 0, and `executionAsyncResource()`
  returns the same empty object on every call. `createHook` is a stub.
- Natively, jobs that off-thread work queues, such as the reactions to `WebAssembly.compile`'s
  promise, run without a context.
- Reactions to settled promises that are created while a debugger has paused the program can run
  with another reaction's context.

### `node:buffer` and `Buffer`

`Buffer` is installed on every global. A `Buffer` is a `Uint8Array` whose prototype is
`Buffer.prototype`, which inherits from `Uint8Array.prototype`, and `Buffer`'s methods accept plain
`Uint8Array`s as `this`. `Buffer(...)` and `new Buffer(...)` behave as `Buffer.from` and
`Buffer.alloc`. The encodings are `utf8`, `utf16le`, `latin1`, `ascii`, `base64`, `base64url`
and `hex`, with Node's aliases. Decoding is lenient as in Node: base64 skips characters outside
both alphabets and stops at `=`, hex stops at the first pair that isn't hex, and both read only
the low byte of each UTF-16 code unit.

- The methods validate and convert their arguments as Node's `lib/buffer.js` does, including the
  order of the checks and which error codes they throw.
- `Buffer.copyBytesFrom`, the `parent` and `offset` getters, and the `utf8Slice`-style and
  `utf8Write`-style methods exist, and the `Uint` spellings of the read and write methods are the
  same functions as the `UInt` ones.
- `kMaxLength` and `constants.MAX_LENGTH` are 2147483647, the largest `Buffer` the runtime
  creates.
- No pool backs small buffers: `Buffer.allocUnsafe` and `Buffer.from` of a string create a
  buffer with an `ArrayBuffer` of its own, whose `byteOffset` is 0. `Buffer.poolSize` is 65536,
  as in Node.js, and has no effect.

`atob` and `btoa` are the globals. `isUtf8`, `isAscii`, `transcode`, `constants`, `kMaxLength`,
`kStringMaxLength` and `INSPECT_MAX_BYTES` are implemented. `transcode` converts between UTF-8,
UTF-16LE, Latin-1 and ASCII as Node.js's ICU-based implementation does. `Blob`, `File` and
`resolveObjectURL` are stubs.

### `node:http` and `node:https`

Servers serve the runtime's incoming requests, and clients send requests through the runtime's
HTTP client, which also serves `fetch`. `Server`, `createServer`, `IncomingMessage`,
`OutgoingMessage`, `ServerResponse`, `ClientRequest`, `request`, `get`, `Agent`, `globalAgent`,
`METHODS`, `STATUS_CODES`, `validateHeaderName` and `validateHeaderValue` are implemented. The
rest of the modules are stubs.

- `listen()` adds a `fetch` event listener: with `--serve` natively, and in a `wasi:http`
  component. A request goes to the listening server whose port is the request URL's, or else to
  the first server that listened. When the CLI runs a script as a command, nothing serves
  requests, and `listen()` emits an `ERR_SERVE_MODE_REQUIRED` error. In a component without a
  `wasi:http/handler` export, `listen()` throws that error, which fails the build when the top
  level listens. A server that listens while the component is built emits `listening` then.
- A served request with an `Upgrade` header, or a `CONNECT` request, gets a 501 response when the
  server has `upgrade` or `connect` listeners, since served requests have no socket to give them.
  Without such listeners it is a `request` event.
- `https.createServer` ignores the TLS options, since the host terminates TLS.
- The `rawHeaders` of served requests and of client responses have the lowercase names the host
  reports, and `httpVersion` is `1.1`. A served request's `url` is the path and query of the URL
  the host parsed, not the raw request target. Its `socket` has the client's `remoteAddress`,
  `remotePort` and `remoteFamily` natively, and no remote address on wasm, since `wasi:http`
  doesn't report it. A
  `wasi:http` host passes the host as the request's authority, which becomes its `host` header.
  `writeContinue`, `writeProcessing` and `writeEarlyHints` have no effect.
- `setHeaders` sets the headers of a `Headers` or `Map`, and `writeHeader` is `writeHead`. The first
  `write` or `end` calls `writeHead` through the response, so a patched `writeHead` sees implicit
  headers too.
- The host frames responses and requests, so `Connection`, `Keep-Alive`, `Transfer-Encoding` and
  `Content-Length` headers set on them are left out. On wasm, so are the request headers the host
  refuses. A response's `write` returns `false` once 16 KiB are buffered, and the response emits
  `drain` once they're sent. A response's status must be from 200 to 599, since the host's
  responses can't have other statuses.
- When a client disconnects, the response doesn't emit `close`.
- A client request is sent once `end()` is called, with everything written as its body. Agents
  hold their options and have no effect on connections, which the host manages. The TLS options,
  `createConnection` and the `socket` event aren't supported, and `signal` is. `setTimeout` emits
  `timeout` when no response arrives in time. Transport errors have `code` `ECONNREFUSED`,
  `ECONNRESET`, `ENOTFOUND` or `ETIMEDOUT` where the host's message names the cause.

### `node:net` and `node:dns`

TCP sockets and servers, over tokio natively and `wasi:sockets` on wasm: `Socket`, `Stream`,
`Server`, `connect`, `createConnection`, `createServer`, `isIP`, `isIPv4` and `isIPv6`, and
`dns.lookup`, `dns.promises.lookup` and `node:dns/promises`' `lookup`. The rest of the modules,
including the DNS queries other than `lookup`, are stubs.

- IPC paths are not supported. A server listens on every IPv4 address unless given an address or
  `localhost`, and its `address()` reports that address.
- `setNoDelay`, `setKeepAlive`, `ref` and `unref` have no effect, and so do the `noDelay` and
  `keepAlive` options. `connect` supports the `family` and `signal` options, and ignores `lookup`,
  `localAddress` and `localPort`, so it emits no `lookup` event. Servers support `allowHalfOpen`,
  `pauseOnConnect` and `maxConnections`. `setTimeout` emits `timeout` once the socket was idle for
  the given time.
- Errors have Node's message, `errno`, `code` and `syscall`, plus `address` and `port` for
  `connect` and `listen`, and `hostname` for lookups.
- On wasm, `localhost` resolves to `127.0.0.1` and `::1` without asking the host, since
  `wasi:sockets` hosts may not resolve it. The wasm module must be run with network access, such
  as `wasmtime run -Sinherit-network=y,tcp=y`.

### `node:tty` and `process.stdin`

`isatty` is `true` only for a standard stream connected to a terminal. `ReadStream` is the class
of `process.stdin`, a `Readable` of standard input, and only reads descriptor 0. The rest of the
module is stubs.

- `process.stdin` reads only while it flows or has `readable` listeners, and pausing it stops the
  read in progress, so a paused `process.stdin` doesn't keep the process running natively. On
  wasm, the process waits until standard input delivers more bytes or ends.
- `setRawMode` records the mode in `isRaw` and has no other effect.

### `node:readline` and `node:readline/promises`

Line mode only: `Interface`, `createInterface`, `cursorTo`, `moveCursor`, `clearLine`,
`clearScreenDown` and `emitKeypressEvents`, and the promise `Interface` and `createInterface`.
`Readline` is a stub.

- An interface emits a `line` event for each line of its input, which ends at `\n`, `\r\n` or
  `\r`, and decodes `Buffer` input as UTF-8. A line read while a `question` is pending goes to
  the question instead. `for await` gives the lines emitted after its iterator is created, and
  rejects with an error the input emits.
- A `question`'s `signal` option cancels the question when it aborts, and rejects the promise
  `node:readline/promises` returns with an `AbortError`.
- An error the input emits is emitted as the interface's `error`.
- `terminal` is always `false`: keypresses, history, completion and cursor editing are not
  supported, and `emitKeypressEvents` has no effect.
- `close()` pauses the interface, which emits `pause`, before it emits `close`.

### `node:constants`

The deprecated module of the `os.constants` `dlopen`, `errno`, `priority` and `signals` values and
the `fs.constants` values, merged in that order as Node does.

### `node:os`

WASI exposes no information about the host machine, so on wasm `os` describes a single processor
with unknown speed and load, no network interfaces, and 4 GiB of memory. Natively it describes the
host where `std` can, and uses the same placeholders elsewhere. `hostname()` returns the host's
name natively, and `HOSTNAME` or `localhost` on wasm. `homedir()`, `tmpdir()` and `userInfo()`
read `HOME`, `TMPDIR`, `USER` and `SHELL`. `setPriority()` validates its arguments and does
nothing, and `getPriority()` returns 0. `constants.dlopen` has the values of macOS on macOS and of Linux
elsewhere.

### `node:stream` and `node:stream/promises`

The implementation follows Node.js v24's `lib/internal/streams` function by function. `Stream`,
`Readable`, `Writable`, `Duplex`, `Transform` and `PassThrough` are implemented, with
`Readable.from`, `pipeline`, `finished`, `addAbortSignal`, `destroy`, `isReadable`, `isWritable`,
`isErrored`, `isDisturbed`, `isDestroyed`, `getDefaultHighWaterMark` and
`setDefaultHighWaterMark`, and `node:stream/promises`' `pipeline` and `finished`.

- A stream's state is in its `_readableState` and `_writableState`, objects with a property per
  field under the names Node's states expose, such as `objectMode`, `flowing`, `ended` and
  `needDrain`.
- Work Node schedules with `process.nextTick` goes in the tick queue `process.nextTick` uses.
- In a `pipeline`, a function stage's result stays an iterable unless a Node stream consumes it.
  Web streams aren't accepted in a `pipeline`, and `finished` doesn't accept them.
- `duplexPair` returns two `Duplex` streams, each reading what the other writes.
- `compose` and `Duplex.from` are stubs. The web stream conversions (`fromWeb`,
  `toWeb`), the static `Readable.wrap` and the experimental `Readable` iterator helpers, such as
  `map`, `filter` and `toArray`, don't exist. `node:stream/consumers` is a stub.

### `node:string_decoder`

`StringDecoder` holds back the bytes that end a chunk in the middle of a UTF-8 or UTF-16
character, or of a base64 group, until a later chunk or `end()` completes them. It follows
Node.js's `src/string_decoder.cc`, including where that differs from decoding the chunks'
concatenation, and has the legacy `lastChar`, `lastNeed`, `lastTotal` and `text`. Like Node's, it
can be called on any object, as constructors that inherit from it do.

### `node:punycode`

Punycode.js 2.1.0's API: `encode`, `decode`, `toASCII`, `toUnicode` and `ucs2`. `toASCII` and
`toUnicode` convert each label without case mapping or normalization, as Punycode.js does.

### `node:querystring`

`parse`/`decode`, `stringify`/`encode`, `escape`, `unescape` and `unescapeBuffer` are implemented,
including the `maxKeys`, `decodeURIComponent` and `encodeURIComponent` options. They follow
Node.js's `lib/querystring.js` on UTF-16 code units, so lone surrogates pass through `parse` and
`unescape`, and `escape` throws `ERR_INVALID_URI` for a surrogate that ends its input.

### `node:url`

`URL` and `URLSearchParams` are the globals. `fileURLToPath`, `pathToFileURL`,
`urlToHttpOptions`, `domainToASCII` and `domainToUnicode` are implemented, with POSIX paths only.
The legacy API, `Url`, `parse`, `format`, `resolve` and `resolveObject`, follows Node's
`lib/url.js`, including its escaping and IDNA hostnames, except that lone surrogates become
U+FFFD, and that `xn--` labels the `idna` crate rejects, which Node accepts, are errors.

### `node:util` and `node:util/types`

`util.inspect` follows Node's `lib/internal/util/inspect.js`, including its line breaking and the
grouping of long arrays into columns. Error stacks are rewritten into V8's frame format. It
doesn't show the entries of `Map` and `Set` iterators or the contents of `WeakMap` and `WeakSet`,
and a class instance without a prototype is shown without its class name. Colors are not
supported, though `inspect.colors` and `inspect.styles` hold Node's tables. `format` and `formatWithOptions` use `inspect` for `%o`, `%O` and non-string
arguments.

`isDeepStrictEqual`, `inherits`, `promisify` (with `promisify.custom`), `callbackify`,
`deprecate`, `debuglog`, `debug`, `getCallSites`, `stripVTControlCharacters`, `styleText`,
`toUSVString`, `getSystemErrorName`, `isArray` and `_extend` are implemented. `styleText` styles
its text whatever the output stream. `types` is `node:util/types`, and `TextEncoder` and
`TextDecoder` are the globals. The rest is stubs.

`node:util/types` implements the predicates with brand checks. `isProxy`, `isExternal`,
`isKeyObject` and `isCryptoKey` always return `false`.

### `node:assert` and `node:assert/strict`

The assertion functions follow Node's `lib/assert.js`, and a failed assertion throws an
`AssertionError` with `code` `ERR_ASSERTION`, the assertion's `actual`, `expected` and `operator`,
and Node's generated message, including its diff. Deep equality follows Node's
`lib/internal/util/comparisons.js`. The deprecated `CallTracker` follows Node's
`lib/internal/assert/calltracker.js`, and emits DEP0173 when the first one is created.
`partialDeepStrictEqual` and the `Assert` class are stubs.

### `node:timers`, `node:timers/promises` and the timer globals

`setTimeout` and `setInterval` return `Timeout` objects instead of numbers, and `setImmediate`
returns an `Immediate`. Both convert to the web timer's ID, so `clearTimeout` and `clearInterval`
accept them as well as IDs. `node:timers` exports the globals.

- `setImmediate` callbacks run as zero-delay timers.
- `ref()`, `unref()`, `hasRef()`, `refresh()` and `close()` are implemented. An unreferenced timer
  runs while other work keeps the event loop running, and doesn't keep it running by itself.
  An unreferenced timer still pending when its event loop finishes is dropped with the loop. A
  request handler's timers belong to the request's event loop, so they never run during a later
  request.
- Timer callbacks get the timer as `this`, delays follow Node's rules, including its warnings for
  negative and too large delays, and nested timers aren't clamped.
- `node:timers/promises` implements `setTimeout`, `setImmediate`, `scheduler.wait` and
  `scheduler.yield`, with the `signal` and `ref` options. `setInterval` is a stub.

### `node:fs` and `node:fs/promises`

The host filesystem: the host's natively, and the component's preopened directories on wasm.
The synchronous, callback and promise forms of `access`, `appendFile`, `chmod`, `chown`, `close`,
`copyFile`, `cp`, `exists`, `fchmod`, `fchown`, `fdatasync`, `fstat`, `fsync`, `ftruncate`, `futimes`,
`link`, `lstat`, `mkdir`, `mkdtemp`, `open`, `read`, `readFile`, `readdir`, `readlink`,
`realpath`, `rename`, `rm`, `rmdir`, `stat`, `symlink`, `truncate`, `unlink`, `utimes`, `write`
and `writeFile` are implemented, as are `createReadStream`, `createWriteStream`, `Stats`,
`Dirent`, `constants`, and `node:fs/promises` with `FileHandle`. `fs.promises` holds the same
functions as `node:fs/promises`. The rest of the modules are stubs.

- Natively, the callback and promise forms run on four worker threads. Where
  [blocking isn't allowed](#synchronous-host-io), the synchronous forms throw
  `ERR_BLOCKING_NOT_ALLOWED`.
- On wasm, the callback and promise forms of `access`, `appendFile`, `close`, `exists`,
  `fdatasync`, `fstat`, `fsync`, `ftruncate`, `futimes`, `lstat`, `open`, `read`, `readFile`,
  `readdir`, `stat`, `truncate`, `utimes`, `write` and `writeFile` use `wasi:filesystem`'s
  asynchronous API and never block, so these operations, `createReadStream`, `createWriteStream`
  and the matching `FileHandle` methods work while the runtime serves requests. Their paths
  resolve the way wasi-libc resolves the synchronous forms' paths: a relative path is made
  absolute against the working directory, the preopened directory whose name is the longest
  prefix of whole path components wins, and a preopen named `.` contains every path. Their errors
  are the ones the synchronous forms report.
- On wasm, the other callback and promise forms block the instance like the synchronous forms,
  and throw `ERR_BLOCKING_NOT_ALLOWED` where blocking isn't allowed. So do the descriptor
  operations on descriptors 0, 1 and 2 and on descriptors `openSync` opened. The synchronous
  forms work on descriptors from the callback and promise forms of `open`, and block on them.
- File descriptors index a table of open files, starting at 3. Descriptors 0, 1 and 2 read from
  stdin and write to stdout and stderr. On wasm, a write without a position to a descriptor that
  the callback or promise form of `open` opened for appending leaves its position unchanged.
- Errors have Node's message, `errno`, `code`, `syscall`, `path` and `dest`. `errno` is the host's
  natively and Linux's on wasm. `realpath.native` is `realpath` itself, so its errors name the
  `lstat` and `stat` system calls, and it resolves the empty path to the working directory.
- `rm` retries a removal that fails with `EBUSY`, `EMFILE`, `ENFILE`, `ENOTEMPTY` or `EPERM` up
  to `maxRetries` times, waiting `retryDelay` milliseconds longer before each retry.
- `cp` follows Node's two implementations: `cpSync` the C++ one, with its messages and its handling
  of `dereference`, and the callback and promise forms the JS one. As in Node, `cpSync` without a
  `filter` creates directories with default permissions, skips sockets and FIFOs inside them, and
  points copied symlinks at the canonical path of their target unless `verbatimSymlinks` is set.
  On wasm, where there are no inode numbers, the asynchronous forms don't detect a copy of a path
  onto itself, and copies keep the default permissions, since WASI has none to set. The
  asynchronous forms call `filter` on the JS thread and copy on the worker threads between calls.
- On wasm, `Stats` has no owners, device or inode numbers, and `mode` holds `0o644` for files,
  `0o755` for directories and `0o777` for symlinks. `chown` does nothing, and `access` doesn't
  check permissions.
- The `bigint` and `signal` options are ignored, and so are the file streams' `fs` and `flush`
  options.

### `node:zlib`

The deflate, zlib and gzip formats are implemented: the synchronous functions, such as
`gzipSync`, the callback functions, such as `gzip`, the stream classes and their `create`
functions, `crc32`, `constants` and `codes`. Brotli, zstd and the zip archive functions are stubs.

- The callback functions compress or decompress synchronously, and call back in a tick.
- Errors have zlib's message, `code` and `errno`, such as `Z_DATA_ERROR` and -3.
- Every option is validated as Node validates it. The `level` option is used, and so are
  `finishFlush` and `maxOutputLength` in the synchronous and callback functions. `windowBits`,
  `memLevel`, `strategy`, `dictionary`, `chunkSize` and `flush` have no effect.
- The legacy `Z_*` constants on the module object are missing. They're in `constants`.
- `flush(Z_FINISH)` runs as a full flush, without ending the compressed stream. A flush before the
  first input produces no output.
- `params` ignores the strategy, and leaves the level unchanged when the stream has already
  compressed input and the new level uses a different zlib strategy function.

## Testing

Three kinds of tests check the implementation.

- `crates/builtins/node/tests/js` has a script per module, each run by
  `cargo test -p node-builtins --test modules`, and the scripts that use host I/O also run on wasm
  in `scripts/test-runtime.sh`.
- `tests/node-harness/run-node-tests.mjs` runs Node.js's own tests from `test/parallel`, at the
  tag `scripts/clone-node-tests.sh` pins, with Node's unmodified `test/common`. Each test runs as a
  process of its own and passes if it exits with status 0. `tests.txt` selects the tests by file
  name, and tests whose source needs runtime flags, child processes, workers or Node.js internals
  are skipped without running. `expectations.json` records each test's status, natively and, where
  it differs, on wasm. The run fails if a test expected to pass fails or skips.
  `--update-expectations` records the current results.
- `tests/node-diff/fuzz.mjs` compares the runtime with Node.js on generated programs. The
  generators in `generators.mjs` draw operations on the implemented modules, such as `Buffer`
  conversions with edge-case arguments, `StringDecoder` chunk sequences, event orders of streams
  and emitters, and file system sequences. Each program runs twice under Node.js, to drop
  operations whose result varies, and once under the runtime. The fuzzer reruns each difference as
  a program of its one operation, groups the differences by signature, and writes a report and
  reproductions to `target/node-diff`. Calls to unimplemented functions share one signature per
  function. `known-diffs.json` lists the accepted signatures. The oracle is the Node.js release
  `scripts/fetch-node-oracle.sh` downloads, the same version as the Node.js tests.

```sh
./scripts/clone-node-tests.sh
node tests/node-harness/run-node-tests.mjs -j 8            # native, target/debug/starlingmonkey
node tests/node-harness/run-node-tests.mjs --target=wasm   # the release starling.wasm
./scripts/fetch-node-oracle.sh
cargo build --release -p starlingmonkey
node tests/node-diff/fuzz.mjs --programs=200 --only=buffer,stream
node tests/node-diff/fuzz.mjs --repro=target/node-diff/repro/<signature>.js
```
