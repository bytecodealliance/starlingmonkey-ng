# Node.js compatibility

This document specifies StarlingMonkey's Node.js compatibility layer: which Node APIs exist, how
they are enabled, and how they behave where WASI cannot provide what Node does. It is the
specification for the implementation in `crates/builtins/node` and the module-system support in
`crates/core-runtime/src/module.rs`, and it is kept in sync with both.

## Enabling Node compatibility

Node compatibility is opt-in:

- The CLI takes `--node-compat`. With `--allow-node-runtime-flags` as well, it also takes Node.js
  runtime flags, such as `--expose-gc` (see [Runtime flags](#runtime-flags)).
- `starling-componentize` takes `--node-compat`, which it passes to the runtime's `init` export
  (`starling:componentize/init@0.2.0`). Components don't take Node.js runtime flags.

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

The CLI also takes `--node-http-listen`, which makes `http.Server.listen()` in a script run as a
command accept HTTP/1.1 connections itself, through `net.Server` (see
[`node:http`](#nodehttp-and-nodehttps)). Without it, `listen()` emits `ERR_SERVE_MODE_REQUIRED`
there. It has no effect with `--serve` or in a component.

## Runtime flags

`--allow-node-runtime-flags`, which requires `--node-compat`, makes the CLI accept Node.js runtime
flags before the script path:

```sh
starlingmonkey --node-compat --allow-node-runtime-flags --expose-internals --no-warnings test.js
```

Every argument before the script path that starts with `-` and isn't an option of the runtime is
a Node.js flag. The runtime's options keep their meaning where Node.js has an option of the same
name, such as `-e` and `-v`. A flag is written as `--name` or `--name=value`. A value in the next
argument isn't supported, since that argument is taken for the script path. Dashes and underscores
in a name are interchangeable, as in Node.js. The flags are `process.execArgv`, as given.

A flag takes effect, is accepted without effect, or is rejected. A rejected or unknown flag ends
the process with status 1 and an error naming it, before the script runs. Without
`--allow-node-runtime-flags`, every Node.js flag is an unknown option of the runtime.

These flags take effect:

- `--expose-internals`: `require` and `import` resolve Node.js's internal modules (see
  [Internal modules](#internal-modules)).
- `--expose-gc`: a global `gc(options)` runs a full garbage collection. With
  `options.execution` `'async'` it returns a promise resolved after the collection. `options.type`
  is ignored.
- `--no-warnings` and `--warnings`: with `--no-warnings`, process warnings aren't printed, and
  are still emitted as `warning` events. `process.noProcessWarnings` is then `true`.
- `--pending-deprecation`: the pending deprecations emit their warnings. `Buffer()` (DEP0005) and
  loading `punycode` (DEP0040) warn when called from a module in `node_modules` as well, assigning
  a value other than a string, number or boolean to a `process.env` property warns (DEP0104), and
  so does `process.binding()` (DEP0111).
- `--no-deprecation`, `--deprecation` and `--throw-deprecation`: the initial
  `process.noDeprecation` and `process.throwDeprecation`.
- `--abort-on-uncaught-exception`: an uncaught exception, unless
  `process.setUncaughtExceptionCaptureCallback` set a callback, prints `Uncaught` with the error
  and its stack frames, as Node.js does, and aborts the process with `SIGABRT` natively and traps
  on wasm, without emitting `uncaughtException`. A rejection nothing handles is printed as without
  the flag, and then aborts the process.
- `--title=title`: the initial `process.title`.
- `--network-family-autoselection`, `--no-network-family-autoselection` and
  `--network-family-autoselection-attempt-timeout=ms`: the initial
  `net.getDefaultAutoSelectFamily()` and `net.getDefaultAutoSelectFamilyAttemptTimeout()`.
  Connecting tries the addresses a host resolves to in turn either way.
- `--experimental-stream-iter`: `stream/iter` and `zlib/iter` can be imported. Their exports are
  stubs.
- `--test-udp-no-try-send`: `dgram` sockets queue every send instead of sending it at once.
- `--insecure-http-parser`: HTTP servers and client requests without the `insecureHTTPParser` and
  `httpValidation` options parse messages leniently (see [`node:http`](#nodehttp-and-nodehttps)),
  and the first one to do so emits a warning.

These flags are accepted without effect:

- `--zero-fill-buffers`: every new `Buffer` is zero-filled, including those of `allocUnsafe`.
- `--max-old-space-size`, `--max-semi-space-size` and `--no-concurrent-array-buffer-sweeping`:
  V8 heap settings.
- `--experimental-vm-modules`: `vm.Module`, `vm.SourceTextModule` and `vm.SyntheticModule`
  aren't implemented.

These flags are rejected:

- `--allow-natives-syntax`: V8's `%` intrinsics have no SpiderMonkey equivalent.
- `--expose-externalize-string`: V8's external strings have no SpiderMonkey equivalent.
- `--permission`, `--allow-fs-read`, `--allow-fs-write`, `--allow-child-process`,
  `--allow-worker`, `--allow-addons` and `--allow-wasi`: the permission model isn't implemented,
  and a script given them would otherwise run without the restrictions they ask for.

### Internal modules

With `--expose-internals`, the internal modules Node.js's tests load can be `require`d and
imported, with and without the `node:` prefix: `internal/assert/myers_diff`,
`internal/async_context_frame`, `internal/async_hooks`, `internal/blocklist`, `internal/buffer`,
`internal/errors`, `internal/event_target`, `internal/fs/glob`, `internal/fs/promises`,
`internal/fs/recursive_watch`, `internal/fs/utils`, `internal/fs/watchers`, `internal/http`,
`internal/js_stream_socket`, `internal/linkedlist`, `internal/net`, `internal/options`,
`internal/readline/utils`, `internal/socketaddress`, `internal/streams/add-abort-signal`,
`internal/streams/end-of-stream`, `internal/streams/state`, `internal/test/binding`,
`internal/timers`, `internal/url`, `internal/util` and `internal/util/inspect`. Unlike Node.js,
`module.builtinModules` doesn't list them. Each has Node.js's export names. The exports below are
implemented, and the others are stubs, as in a stub module, with a new symbol for each symbol
export. Loading `internal/test/binding` emits Node.js's warning about it.

- `internal/assert/myers_diff`: `myersDiff`.
- `internal/blocklist` and `internal/socketaddress`: `BlockList` and `SocketAddress`, which are
  `node:net`'s.
- `internal/buffer`: `FastBuffer`, which is `Buffer[Symbol.species]`.
- `internal/event_target`: `Event`, `EventTarget` and `CustomEvent`, which are the globals.
- `internal/fs/utils`: `Dirent`, `Stats`, `getDirent`, `getDirents`, `stringToFlags`,
  `validateOffsetLengthRead` and `validateOffsetLengthWrite`.
- `internal/linkedlist`: `append`, `init`, `isEmpty`, `peek` and `remove`.
- `internal/net`: `isIP`, `isIPv4`, `isIPv6`, and `normalizedArgsSymbol`, which marks the arrays
  `net._normalizeArgs` returns.
- `internal/options`: `getOptionValue`, for the options the flags above set and
  `--max-http-header-size`. Other options are `undefined`.
- `internal/readline/utils`: `CSI`.
- `internal/streams/add-abort-signal`: `addAbortSignal` and `addAbortSignalNoValidate`.
- `internal/streams/end-of-stream`: `eos` and `finished`, which are `stream.finished`.
- `internal/streams/state`: `getDefaultHighWaterMark` and `setDefaultHighWaterMark`.
- `internal/timers`: `setUnrefTimeout`.
- `internal/url`: `isURL`, and `node:url`'s `URL`, `URLSearchParams`, `domainToASCII`,
  `domainToUnicode`, `fileURLToPath`, `pathToFileURL` and `urlToHttpOptions`.
- `internal/util`: `deprecate`, `emitExperimentalWarning`, `pendingDeprecate`, `promisify` and
  `sleep`.
- `internal/util/inspect`: `format`, `formatWithOptions`, `getStringWidth`, `inspect`,
  `inspectDefaultOptions` and `stripVTControlCharacters`.

`internal/test/binding`'s `internalBinding(name)` returns an object for each of Node.js's binding
names, and throws an `Error` with the code `ERR_INVALID_MODULE` for other names. The objects hold
these members, and nothing else:

- `buffer`: `fill`.
- `constants`: `fs` and `os`, which are `fs.constants` and `os.constants`.
- `performance`: `constants`, which are `perf_hooks.constants`, and `observerCounts`, a
  `Uint32Array` of the number of observers of each `NODE_PERFORMANCE_ENTRY_TYPE_*` type.
- `timers`: `getLibuvNow`, which is `process.uptime()` in milliseconds, as an integer.
- `uv`: a `UV_` constant for each libuv error code, and `errname`.

The runtime's modules don't use the binding objects, so replacing a member of one changes no
module's behavior. `process.binding(name)` returns the same objects for the names Node.js allows it, and throws an
`Error` otherwise.

## Implementation rules

These rules add to the practices for all builtins in
[builtin-development.md](builtin-development.md).

- Every Node module is implemented natively in Rust with the proc macros (`#[jsmodule]`,
  `#[jsclass]`/`#[jsmethods]`, `#[jsfunction_class]`, `#[jsnamespace]`). No JavaScript from
  Node's `lib/` is vendored.
- Internal state lives in Rust: in the private data of a `#[jsclass]` instance, with `Heap<T>`
  fields for JS values. It is not kept in JS properties, symbol-keyed properties or JS arrays.
  Where Node exposes part of the state publicly, it is an accessor over the Rust state, or an own
  data property where Node defines one, which the implementation never reads back as state.
- Classes that Node code subclasses ES5-style (`EventEmitter.call(this)` with `util.inherits`) or
  whose prototype methods it copies onto other objects are `#[jsfunction_class]`es: constructor
  functions callable without `new`. With `state = "Type"`, all of an instance's Rust state is the
  private data of an instance of `Type`'s class:
  - `new` and `super()` create instances of that class, with the prototype of `new.target`.
  - `Base.call(obj)` on an object the constructor didn't create gives `obj` a handle: an instance
    of that class, which `obj` holds in a property under a private name symbol, the kind of key
    the engine uses for `#private` fields. JavaScript can't read, list, copy or delete it. For a
    `Proxy`, its innermost target holds the handle.
  - Methods reach the state with the generated `state_of`, which reads the private data of the
    instance, of its handle, of a `Proxy`'s target or of an object on the prototype chain without
    running JavaScript, and throws `ERR_INVALID_THIS` for other objects.
  - A class whose instances combine the parts of several classes, as `Duplex` combines `Readable`
    and `Writable`, has one state type holding all parts, which the other classes' methods reach
    through a `js::class::InstanceState` implementation.
  - Note: `structuredClone` throws for instances with state, which Node clones as plain objects.
- Methods and static methods of `#[jsfunction_class]`es are enumerable, as the members Node
  assigns to its function-style classes are, except for the classes Node writes with `class`
  syntax, which use `#[jsfunction_class(class_syntax)]` and throw when called without `new`.
  Those can also be plain `#[jsclass]`es. The classes that return a new instance when called
  without `new`, such as the stream classes, `net.Socket` and the zlib classes, use
  `#[jsfunction_class(self_construct)]`.
- The state of a native callback is its payload: the value itself for one value, and a
  `#[jsclass(hidden)]` instance, read with `js::function::cast_payload`, for several. Work
  queued for the tick queue is a native callback with a payload and arguments, queued with
  `js::jobs::queue_native_tick`.
- Internal classes are `#[jsclass(hidden)]`. Every class is registered for each global when the
  global is set up, in `register_classes` in `crates/builtins/node/src/lib.rs`, and none is created
  on first use.
- Standard classes, web classes, and the objects the global setup installs, such as `process`,
  `console`, `atob` and `btoa`, are reached through `js::class::get_class_object`, the class
  registry or realm objects, and not through the global's properties, which code can replace. A
  stub module is the exception: when it is first imported, an export takes the value of the
  global of the same name, such as `stream/web`'s classes.
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
policy. So do the file watchers: `fs.watch`, `fsPromises.watch` and `fs.watchFile` read the
filesystem synchronously on the JS thread when they start and at every poll. Starting one throws
`ERR_BLOCKING_NOT_ALLOWED` where blocking isn't allowed, and a watcher started earlier skips its
polls there, so it reports nothing while the runtime serves requests unless
`--allow-blocking-host-io` is given.

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
implemented.

`path.matchesGlob` matches as the minimatch that Node.js bundles, with the options Node passes:
brace expansion including sequences, `*`, `**`, `?`, character classes with ranges, negation and
POSIX classes, and extglobs. Backslashes are path separators, a leading `!` and `#` are literal,
and `.` segments and `<segment>/..` are collapsed in both the pattern and the path. Segments with
magic match case-insensitively where `process.platform` is `darwin` or `win32`, which is natively
on macOS and Windows, and never on wasm. Segment patterns compile to minimatch's regular
expressions, which run on the JS engine's `RegExp`. Matching follows minimatch's POSIX rules on
every platform, without its Windows drive and UNC handling.

### `node:perf_hooks`

`performance`, `Performance`, `PerformanceEntry`, `PerformanceMark`, `PerformanceMeasure`,
`PerformanceObserver` and `PerformanceObserverEntryList` are the globals, the web performance
timeline of `web_globals::performance`, so marks, measures and observers are shared with code that
uses the globals. `createHistogram`, `monitorEventLoopDelay`, `eventLoopUtilization`, `timerify`
and `constants` are implemented. `PerformanceResourceTiming` is a stub, and no `resource` entries
are produced.

- The timeline is the `Performance` object's Rust state: the `mark` and `measure` buffers and the
  registered observers. An observer's callback runs in a zero-delay timer queued once for all
  observers with new entries, as Node.js's `setImmediate`, with the entries sorted by start
  time. Errors of the shared classes are the web platform's: argument conversions throw a
  `TypeError` without a `code` where Node.js throws `ERR_INVALID_ARG_TYPE` or
  `ERR_MISSING_ARGS`, and `performance.mark` throws a `SyntaxError` `DOMException` for a
  `nodeTiming` milestone name, where Node.js throws `ERR_INVALID_ARG_VALUE`.
- `Performance.prototype` gets Node.js's non-enumerable `eventLoopUtilization`, `nodeTiming` and
  `timerify`, and Node.js's `toJSON`. `markResourceTiming`, `clearResourceTimings`,
  `setResourceTimingBufferSize` and `onresourcetimingbufferfull` are missing. `measure` takes
  options with neither `start` nor `end`, as Node.js's does, and resolves the `nodeTiming`
  milestone names.
- `PerformanceObserver.supportedEntryTypes` is Node.js's list. Of the types Node.js produces itself,
  `function` entries come from `timerify` and `gc` entries from the engine's collection
  callbacks, which are installed only while an observer observes `gc`. A nursery collection is
  `NODE_PERFORMANCE_GC_MINOR`, a slice of an incremental major collection other than the last is
  `NODE_PERFORMANCE_GC_INCREMENTAL`, and the last slice of a major collection is
  `NODE_PERFORMANCE_GC_MAJOR`. Collections that `gc()` or the embedding requested have
  `NODE_PERFORMANCE_GC_FLAGS_FORCED`, and others `NODE_PERFORMANCE_GC_FLAGS_NO`. The entries are
  queued by an unreferenced timer, which doesn't keep the event loop running. `dns`, `http`,
  `http2`, `net` and `quic` entries are never produced.
- `nodeTiming`'s `nodeStart` and `v8Start` are when the Node.js setup of the global started,
  `environment` when the classes of the Node.js builtins were registered, and
  `bootstrapComplete` when the setup finished. `loopStart` is when the event loop first stepped,
  `-1` before that, and `loopExit` is always `-1`. `idleTime` is the time the event loop's driver
  spent waiting for timers, tasks and I/O. `uvMetricsInfo`'s counts are 0.
- `eventLoopUtilization` computes Node.js's idle and active times from the event loop's start and
  idle time, and returns zeros before the event loop's first step. In serve mode, it is the
  utilization of the event loop of the request that calls it.
- Histograms hold their counts in the `hdrhistogram` crate, which has HdrHistogram_c's bucket
  layout. The statistics are ports of HdrHistogram_c's and Node.js's, so percentiles, the
  `percentiles` map, means and the analysis methods match Node.js's results, apart from the last
  bits of some floating-point results. A histogram can't be transferred or cloned.
- `monitorEventLoopDelay` samples with an unreferenced repeating timer of `resolution`
  milliseconds, which records the time between its runs. With `samplePerIteration`, it samples
  with the timer as well, as the event loop has no hook that runs on each of its iterations.
- `logBuckets` with a `base` below 2 stops after the first bucket, where Node.js's iteration
  doesn't end.

### `node:crypto`

Hashes, HMACs, secret keys, key derivation and random values are implemented: `createHash`,
`createHmac`, `hash`, `getHashes`, `KeyObject`, `createSecretKey`, `pbkdf2`, `pbkdf2Sync`,
`scrypt`, `scryptSync`, `hkdf`, `hkdfSync`, `randomBytes`, `randomFill`, `randomFillSync`,
`randomInt`, `randomUUID`, `getRandomValues` and `timingSafeEqual`. `pseudoRandomBytes`, `prng`
and `rng` are non-enumerable aliases of `randomBytes`, as in Node. `webcrypto` is the global
`crypto`, and `subtle` is its `subtle` if it has one. The rest of the module is stubs, including
asymmetric keys, signatures and ciphers.

- The digests are MD5, RIPEMD-160, SHA-1, SHA-224, SHA-256, SHA-384, SHA-512, SHA-512/224,
  SHA-512/256 and SHA3-224, SHA3-256, SHA3-384 and SHA3-512, under their names with or without an
  `RSA-` prefix. The SHAKE extendable-output functions are missing.
- `Hash` and `Hmac` are the classes `createHash` and `createHmac` instantiate. Their objects
  have `_options`, `update`, `digest` and, for `Hash`, `copy`, but aren't streams. Calling the
  classes directly emits DEP0179 and DEP0181.
  `digest` returns a `Buffer` for an encoding it doesn't know, as Node does.
- `createHmac` takes a secret `KeyObject` as its key, and decodes a string key in
  `options.encoding`.
- `KeyObject`s are secret keys, which `createSecretKey` creates. They keep their material
  private, and have `type`, `symmetricKeySize`, `equals` and `export`, which returns a `Buffer` or
  a JSON Web Key. `KeyObject.from` always throws, as `CryptoKey` isn't implemented, `toCryptoKey`
  is missing, and `createPublicKey` and `createPrivateKey` are stubs.
- `pbkdf2`, `scrypt` and `hkdf` validate their arguments and options as Node does, and derive the
  keys of RFC 8018, RFC 7914 and RFC 5869 with the supported digests. scrypt's parameters are
  checked as OpenSSL checks them, including `maxmem`. The keys are derived on the JS thread,
  including in the callback forms, which call back in a later turn of the event loop.
- Random bytes come from the operating system natively, and from `wasi:random/random` on wasm.
- The callbacks of `randomBytes` and `randomFill` run in a later turn of the event loop, and
  `randomInt`'s in a tick.

### `node:diagnostics_channel`

`channel`, `hasSubscribers`, `subscribe`, `unsubscribe`, `tracingChannel` and `Channel` are
implemented, including `bindStore`, `unbindStore` and `runStores`, and `TracingChannel`'s
`traceSync`, `tracePromise` and `traceCallback`.

- A channel with subscribers or bound stores has the prototype of Node's internal
  `ActiveChannel` class, and `Channel[Symbol.hasInstance]` accepts both prototypes, as in Node.
  `_subscribers`, `_stores`, `name` and `_index` are own data properties that reflect the
  channel's state, but changing them doesn't change it.
- As in Node, a realm holds a channel that has no subscribers or bound stores weakly, so it is
  collected once nothing references it, and its name's entry is removed.
- An exception a subscriber or a store's transform throws is uncaught, so it reaches the
  `uncaughtException` listeners, and the remaining subscribers still run.

### `node:events`

`EventEmitter`, the module's `default` export, keeps an emitter's listeners in `_events`,
`_eventsCount` and `_maxListeners`, as Node does. Its static members (`defaultMaxListeners`,
`errorMonitor`, `captureRejectionSymbol`, `captureRejections`, `usingDomains`, `init`) are also
named exports. `getEventListeners`, `getMaxListeners`, `setMaxListeners` and `listenerCount` are
implemented, as are `once` and `on`, for emitters and `EventTarget`s, except that
`getEventListeners` can't list an `EventTarget`'s listeners. `on`'s `highWaterMark` and
`lowWaterMark` options call the emitter's `pause` and `resume` if it has them.
`addAbortListener` and `EventEmitterAsyncResource` are stubs.

- Exceeding the listener limit emits a `MaxListenersExceededWarning`, whose `emitter`, `type` and
  `count` are the emitter, the event and the number of listeners.
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
- `execArgv` is the Node.js runtime flags (see [Runtime flags](#runtime-flags)), and `title` is
  `starlingmonkey` unless `--title` sets it. `binding(name)` returns the binding objects of
  [Internal modules](#internal-modules).
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
`http` classes and helpers Node.js exports from them. Their other exports are stubs. The
`internal/` modules exist with `--expose-internals` (see [Internal modules](#internal-modules)).

### `node:async_hooks`

`AsyncLocalStorage` and `AsyncResource` keep their stores in an async context: a frame holding
each `AsyncLocalStorage`'s store, as in Node's `AsyncContextFrame` implementation. Setting a store
replaces the context with a copy that holds it, and `disable()` removes the store from the current
frame. Promise reactions, `process.nextTick` and
`queueMicrotask` callbacks, timers and other event-loop tasks capture the context current when
they are created, and run with it. Event listeners run with the context current when the event is
emitted.

- `run`, `exit`, `getStore`, `enterWith`, `disable`, `withScope`, `AsyncLocalStorage.bind`,
  `AsyncLocalStorage.snapshot`, and the `defaultValue` and `name` options are implemented.
  `AsyncLocalStorage.bind` and `AsyncLocalStorage.snapshot` bind through a new `AsyncResource`,
  as in Node.
- `AsyncResource`'s `runInAsyncScope`, `bind`, `asyncId`, `triggerAsyncId` and `AsyncResource.bind`
  are implemented. `emitDestroy` does nothing. A bound function has the deprecated
  `asyncResource` accessor, which emits `DEP0172`.
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
`Buffer.alloc`, and emit DEP0005 once, unless called from a module in `node_modules`.
`Buffer[Symbol.species]` is a constructor that creates `Buffer`s without the warning, which the
`Uint8Array` methods such as `map` use. A `Buffer` of a resizable `ArrayBuffer` created without a
length follows the buffer's length. The encodings are `utf8`, `utf16le`, `latin1`, `ascii`, `base64`, `base64url`
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
`kStringMaxLength`, `INSPECT_MAX_BYTES` and `SlowBuffer`, which emits DEP0030, are implemented. `transcode` converts between UTF-8,
UTF-16LE, Latin-1 and ASCII as Node.js's ICU-based implementation does. `Blob` and `File` are
the globals, which implement the File API's `Blob` and `File` without `textStream()`, with and
without `--node-compat`. `resolveObjectURL` is a stub.

### `node:http` and `node:https`

Servers serve the runtime's incoming requests, or accept HTTP/1.1 connections themselves.
Clients write HTTP/1.1 to sockets, or send requests through the runtime's HTTP client, which also
serves `fetch` (see "For clients" below). `Server`, `createServer`, `IncomingMessage`,
`OutgoingMessage`, `ServerResponse`, `ClientRequest`, `request`, `get`, `Agent`, `globalAgent`,
`METHODS`, `STATUS_CODES`, `validateHeaderName` and `validateHeaderValue` are implemented. The
rest of the modules are stubs.

- `Server` extends `net.Server`. Its options are validated and stored as Node.js stores them.
- `listen()` depends on how the runtime runs:
  - When the CLI runs a script as a command with `--node-http-listen`, `listen()` is
    `net.Server`'s, and the server handles the connections it accepts, as described below.
  - Otherwise, `listen()` adds a `fetch` event listener: with `--serve` natively, and in a
    `wasi:http` component. A request goes to the listening server whose port is the request URL's,
    or else to the first server that listened. When the CLI runs a script as a command without
    `--node-http-listen`, nothing serves requests, and `listen()` emits an
    `ERR_SERVE_MODE_REQUIRED` error. In a component without a `wasi:http/handler` export,
    `listen()` throws that error, which fails the build when the top level listens. A server that
    listens while the component is built emits `listening` then.
- `https.createServer` ignores the TLS options, and its servers don't accept TLS connections
  themselves, since the host terminates TLS.
- `setHeaders` sets the headers of a `Headers` or `Map`, and `writeHeader` is `writeHead`. The first
  `write` or `end` calls `writeHead` through the response, so a patched `writeHead` sees implicit
  headers too. As in Node.js, the headers `writeHead` is given are sent without being set, so
  `getHeader` doesn't return them, unless a header was set before.
- A `ServerResponse` that doesn't respond to a served request writes HTTP/1.1 to the socket it is
  assigned with `assignSocket`, and queues what it writes in `outputData` until then, as in
  Node.js.

The own data properties that Node's constructors give `IncomingMessage`, `OutgoingMessage`,
`ServerResponse` and `ClientRequest` objects, and those of emitters and readable streams, are
created in Node's order from `js::object::PropertyTemplate`s. The messages that the module creates
itself, for the requests a server parses or serves, their responses, and the responses of client
requests, get their properties defined with the values of the request or response head. Objects
that a constructor is given, including those of the `IncomingMessage` and `ServerResponse`
classes a server is created with, get them assigned, as in Node.js.

Each message, server and connection keeps its state in one Rust struct:

- An `OutgoingMessage`'s `OutgoingState` holds its headers with the values they were given, the
  header block `writeHead` or `_implicitHeader` stored until it is sent, its socket, the writes it
  queues until it has one, the request a response responds to, the names of `uniqueHeaders`, and
  its transport: HTTP/1.1 written to its socket, the web `Response` of a served request, or the
  body of a client request through the runtime's HTTP client. A client request's state also holds
  its agent, its `path`, the error it was destroyed with and, while its agent queues it, the
  options it waits with. `headersSent`, `socket`, `connection`, `closed`, `errored` and the
  `writable*` getters are accessors over it. `statusCode` and `statusMessage` are data properties
  of `ServerResponse.prototype`, as in Node.js.
- An `IncomingMessage`'s `IncomingState` extends the state of the stream with the source of its
  body: the socket it is parsed from, the body the host received, or the body stream of a served
  request that was read before. It also holds the header and trailer lines, from which `headers`,
  `trailers`, `headersDistinct` and `trailersDistinct` are built when they are first read, and the
  controller of `signal`.
- An `http.Server`'s `HttpServerState` extends the state of the `net.Server` with the connections
  it accepted, the timer that checks them for timeouts, the port it serves the runtime's requests
  on, and the classes of the `IncomingMessage` and `ServerResponse` options.
- A connection's `ConnectionState` holds its parser, the requests waiting for their responses, the
  responses waiting for the socket, and the bytes those responses queued. It is the payload of
  the socket's listeners.
- The `parser` of a connection or of a client request's socket is a `ParserHandle`, whose private
  data holds the parser, the message it parses, and for a client request the socket and the
  options of the response's headers. A client request's parser is reused by a later request once
  it is freed, as in Node.js.
- An agent's own data properties `requests`, `sockets`, `freeSockets` and `totalSocketCount`
  hold its pools, as in Node.js. The listeners it adds to its sockets are native functions whose
  payload, an `AgentSocket`, holds the agent, the socket and the socket's options.
- Served requests and client requests through the runtime's HTTP client get a host socket, a
  `Socket` whose private data holds the peer's address the host reported, the local port, whether
  it was destroyed, and the request it belongs to.

A server that accepts connections itself handles each connection's requests as Node.js does:

- Request heads are parsed with `httparse` and checked as `llhttp` checks them in its strict mode:
  methods of `METHODS`, URLs of visible ASCII characters, HTTP/0.9, 1.0, 1.1 and 2.0, header
  names of token characters, header values without control characters other than a tab, CRLF
  line ends, and at most `maxHeadersCount` (by default 1000) header lines in at most
  `maxHeaderSize` (by default 16 KiB) bytes. With `insecureHTTPParser`, an `httpValidation` of
  `'relaxed'` or `'insecure'`, or `--insecure-http-parser`, the parser is lenient as `llhttp` is
  with its lenient flags: header values can have control characters, lines can end with a line
  feed alone, a line that starts with a space or a tab continues the header before it, a request
  can have both `Transfer-Encoding` and `Content-Length`, and data after a request that closes
  the connection is ignored. `'relaxed'` relaxes more than `llhttp` does. Bodies are framed by `Content-Length` or chunked
  transfer coding, with trailers. A parse error has `llhttp`'s `code`, `reason` and the position it
  reports as `bytesParsed`, and `rawPacket`. It is emitted as `clientError`, and without
  listeners the server responds with a 400, 408, 413 or 431 and destroys the socket. Data after a
  request that closes the connection is an `HPE_CLOSED_CONNECTION` error.
- `rawHeaders` keep the names as received, and `headers` join duplicates as Node.js does
  (`joinDuplicateHeaders` included). `headersDistinct`, `trailersDistinct` and `trailers` are
  set.
- Connections are kept alive and requests can be pipelined: responses are written in the order of
  their requests. `requireHostHeader`, `maxRequestsPerSocket` (with `dropRequest`),
  `Expect: 100-continue` (with `checkContinue`), other expectations (with `checkExpectation`),
  and `upgrade` and `connect` with the socket and the bytes after the head (with
  `shouldUpgradeCallback`) behave as in Node.js. The socket's `setEncoding` throws
  `ERR_HTTP_SOCKET_ENCODING`.
- `headersTimeout` and `requestTimeout` are checked every `connectionsCheckingInterval`
  milliseconds, and a request that exceeds them gets a 408. Between requests, a connection times
  out after `keepAliveTimeout` plus `keepAliveTimeoutBuffer` milliseconds. `timeout` and
  `setTimeout` apply to the connections accepted later.
- Responses have Node.js's wire format: the headers set, then `Date` unless `sendDate` is false,
  `Connection` and `Keep-Alive`, and `Content-Length` when `end` is given the whole body before
  the head was stored, or chunked transfer coding otherwise. Responses to `HEAD` requests and
  responses with status 1xx, 204 or 304 have no body. `writeContinue`, `writeProcessing`,
  `writeEarlyHints` and `writeInformation` write informational responses, and `addTrailers` adds
  trailers to a chunked response. The head is encoded as Node.js encodes it, as UTF-8 when it is
  sent with a string body chunk in UTF-8 and as Latin-1 otherwise. `cork` and `uncork` cork the
  socket, and `writableLength`, `writableHighWaterMark`, `writableCorked`, `closed` and `errored`
  are Node.js's.
- `close()` destroys the connections without a request in progress, as Node.js does. A connection
  whose response finishes after `close()` is destroyed if it is still without a request 500
  milliseconds later, where Node.js waits for the client or `keepAliveTimeout`.
  `closeAllConnections` and `closeIdleConnections` destroy the connections.
- After it passes a request body chunk or trailers to the request, the server runs the tick queue,
  as Node.js does for a TCP socket. The microtasks don't run then, so their effects can come
  after events Node.js emits after them, such as a `clientError` for a later pipelined request.

For served requests:

- The `fetch` listener reads the `FetchEvent`'s `Request` natively: its method, current URL and
  header list, and the body the host received, which the `IncomingMessage` reads without creating
  the request's body stream. A request whose body stream was created reads that stream.
- The response is a web `Response` created natively, with the headers set on the
  `ServerResponse`. A body that `end` writes whole is the response's body. Otherwise the response
  is sent with the first write, and its body is a native `ReadableStream` the later writes
  enqueue chunks on. The headers don't pass through a `Headers` guard, so `Set-Cookie` is sent
  with `--enforce-fetch-restrictions` too.
- A served request with an `Upgrade` header, or a `CONNECT` request, gets a 501 response when the
  server has `upgrade` or `connect` listeners, since served requests have no socket to give them.
  Without such listeners it is a `request` event.
- The `rawHeaders` of served requests and of client responses have the lowercase names the host
  reports, and `httpVersion` is `1.1`. A served request's `url` is the path and query of the URL
  the host parsed, not the raw request target. Its `socket` has the client's `remoteAddress`,
  `remotePort` and `remoteFamily` natively, from the address the serve layer records in the
  request's `FetchEvent`, and no remote address on wasm, since `wasi:http` doesn't report it.
  The socket is an `EventEmitter` whose `setTimeout`, `setNoDelay`, `setKeepAlive`, `ref`,
  `unref` and `address` have no effect on the connection, and whose `destroy` and `end` only mark
  it destroyed and emit `close`. A `wasi:http` host passes the host as the request's authority, which becomes its `host` header.
  `writeContinue`, `writeProcessing` and `writeEarlyHints` have no effect.
- The host frames responses and requests, so `Connection`, `Keep-Alive` and `Transfer-Encoding`
  headers set on them are left out, and so are `Content-Length` headers set on requests. A
  response's `Content-Length` frames its body, and is the length of the body of a response to a
  `HEAD` request. Without one, a response whose body isn't written whole by `end` is chunked. On
  wasm, the request headers the host refuses are left out too. A response's `write` returns
  `false` once 16 KiB are buffered, and the response emits `drain` once they're sent. A
  response's status must be from 200 to 599, since the host's responses can't have other
  statuses.
- A response whose body is being written keeps its request's event loop running until it ends,
  is destroyed, or the client disconnects, so another request's handler can write to it and end
  it, as for server-sent events. Other work a handler leaves running when its response is sent,
  such as timers and client requests, is abandoned, except for pending `node:fs` operations (see
  [`node:fs`](#nodefs-and-nodefspromises)).
- When a client disconnects, the response doesn't emit `close`.

For clients, a request takes one of two paths:

- A request to an `http:` URL in a script the CLI runs as a command, natively or on wasm, writes
  HTTP/1.1 to a socket, as Node.js's `ClientRequest` does.
- Every other request goes through the runtime's HTTP client, reqwest natively and `wasi:http` on
  wasm: requests to `https:` URLs, since the runtime's sockets have no TLS, and every request while
  the runtime serves requests, with `--serve` or in a `wasi:http` component, including Spin. There
  `net` sockets work natively, but a component's host may not grant `wasi:sockets`, and a socket a
  served request's event loop reads isn't read once its response is sent, so an agent couldn't
  reuse it.

A request over a socket behaves as in Node.js:

- It gets its socket from `agent.addRequest`, from the `createConnection` option, or from
  `net.createConnection` with `agent: false` and without `createConnection`, all called by name,
  so agents and options can replace them. `onSocket` assigns the socket in the tick queue, and the
  request emits `socket`, writes its queued header block and body, and parses the response with
  the parser of the server's requests in response mode (see above). Its `rawHeaders` keep the
  names as received, and `statusMessage` is the reason phrase as received.
- The header block is Node.js's, from `_storeHeader`: the headers in the order they were set, with
  `Host` (unless `setHost` is `false`), `Authorization` for `auth`, and `Connection`, and
  `Content-Length` or chunked transfer coding as the method and `end()` allow. `headers` given as
  an array is sent as given, `uniqueHeaders` sends the values of its headers on one line, and
  `setDefaultHeaders: false` leaves out `Connection`, `Content-Length` and `Transfer-Encoding`.
  The request target is `path` as given, and nothing is added or normalized.
- A response with status 100 emits `continue`, and every informational response but 101 emits
  `information`. A response with status 101 and `Upgrade`, or any response to a `CONNECT`
  request, emits `upgrade` or `connect` with the response, the socket and the bytes after the
  head, and destroys the socket without listeners. Responses to `HEAD` requests and with status
  304 have no body, and a response without `Content-Length` or chunked transfer coding ends when
  the connection closes. `maxHeaderSize` (by default 16 KiB), `maxHeadersCount`,
  `insecureHTTPParser`, `httpValidation` and `joinDuplicateHeaders` apply to the response's head.
  A listener that throws while the response's head or body is handled throws an uncaught
  exception, and the parser fails the bytes that follow.
- Errors before a response are `error` events, such as `ECONNRESET` "socket hang up" when the
  connection closes, and the parser's errors with `llhttp`'s codes. A response whose connection
  closes before it is complete is destroyed with `ECONNRESET` "aborted". `timeout`, `setTimeout`,
  `setNoDelay`, `setSocketKeepAlive` and `signal` work on the socket, as in Node.js.
- An `Agent` keeps sockets per connection name from `getName`, creates them with
  `createConnection` up to `maxSockets` per name and `maxTotalSockets` in all, and queues the
  requests that find none in `requests`. With `keepAlive`, a socket whose response ended goes to
  `freeSockets`, up to `maxFreeSockets` per name, with the agent's `timeout` or the response's
  `Keep-Alive` timeout less `agentKeepAliveTimeoutBuffer`, and is given to the next request in
  `scheduling` order, whose `reusedSocket` is then `true`. A free socket doesn't keep the event
  loop alive, and is destroyed if it receives data. `http.globalAgent` keeps connections alive
  with a timeout of 5 seconds, as in Node.js, and assigning `http.globalAgent` replaces the
  default agent of later requests. Queued requests don't keep the async context they were
  created in. `proxyEnv` and the environment's proxy settings have no effect.
- `socketPath` is passed to `createConnection` as `path`, where `net` refuses it, since `net` has
  no IPC sockets. `localAddress`, `family` and `lookup` reach `net.connect`.

A request through the runtime's HTTP client is sent once `end()` is called, with everything
written as its body:

- The host manages its connections and TLS. The TLS options and `createConnection` are ignored,
  and so are agents but for `keepAlive` (see below). Upgrades, `CONNECT` and informational
  responses aren't supported. `signal` is. `setTimeout` emits `timeout` when no response arrived
  in time. Transport errors have `code` `ECONNREFUSED`, `ECONNRESET`, `ENOTFOUND` or `ETIMEDOUT`
  where the host's message names the cause.
- The host may add headers, such as `accept: */*` natively, lowercases header names, normalizes
  the path and sets the headers that frame the request. The response's `rawHeaders` have the
  lowercase names the host reports, and `statusMessage` is the standard reason phrase.
- The request emits `socket` in the tick queue after it is created, unless it was destroyed by
  then, and `socket` and `connection` are the socket from then on, as is its response's `socket`.
  The socket is the one of served requests (see above), with `pending` and `connecting` `false`,
  the host as its `remoteAddress` if the host is an IP address, and `encrypted` set for `https:`.
  Its `destroy` destroys the request. A `keepAlive` agent takes the socket back when the response
  ends, and gives it to its next request to the same host and port, whose `reusedSocket` is then
  `true`.
- `https.Agent` has `http.Agent`'s methods.

### `node:net` and `node:dns`

TCP sockets and servers, over tokio natively and `wasi:sockets` on wasm: `Socket`, `Stream`,
`Server`, `connect`, `createConnection`, `createServer`, `BlockList`, `SocketAddress`, `isIP`,
`isIPv4`, `isIPv6` and `_normalizeArgs`, and `dns.lookup` (see [`node:dns`](#nodedns)). The rest of
`node:net` is stubs.

- A socket's state is a `SocketState`, which extends the state of the stream with its
  connection, its byte counts, its idle timer, its addresses and the server that accepted it.
  `bytesRead`, `bytesWritten`, `readyState`, `pending`, the address getters and `_handle` are
  accessors over it. A server's `ServerState` holds its listening socket, the accept in progress
  and the number of open connections, which `_connections` mirrors. `BlockList` and
  `SocketAddress` are classes whose private data holds the rules and the address.
- IPC paths are not supported. A server listens on every IPv4 address unless given an address or
  `localhost`, and its `address()` reports that address.
- `setNoDelay` and `setKeepAlive` have no effect, and so do the `noDelay` and `keepAlive` options.
  `unref` on a socket or server keeps its connect, reads, writes and accepts from keeping the event
  loop alive, and `ref` undoes it. `connect` supports the `family`, `hints`, `signal`,
  `localAddress` and `localPort` options, and `lookup`, which it calls for a host that isn't an IP
  address, with `all: true` unless the connect has an address family or a local address or
  `autoSelectFamily` is `false`, and emits a `lookup` event for each address the callback gives.
  It tries the addresses in turn. Without `lookup` it emits no `lookup` event. Servers support
  `allowHalfOpen`, `pauseOnConnect` and `maxConnections`, and `listen` supports the `signal`
  option. The `backlog`, `ipv6Only`, `reusePort` and `exclusive` options of `listen` have no
  effect. `setTimeout` emits `timeout` once the
  socket was idle for the given time. Unless the socket allows half-open connections, a write
  after the peer ended the connection destroys the socket with `EPIPE`, as in Node.js.
- Natively, a write sends what the connection takes at once before returning, and waits for the
  rest. On wasm, every write waits. `bytesWritten` counts the bytes handed to the connection and
  the bytes the socket buffers, as in Node.js, and `destroySoon` ends the socket and destroys it
  once it finished writing.
- A socket's `blockList` option makes `connect` skip the addresses the list blocks, and fail with
  `ERR_IP_BLOCKED` if it blocks the last address tried. A server's `blockList` closes connections
  from the peers it blocks without emitting an event. A `blockList` in the options of a request
  over a socket reaches `net.connect`.
- `BlockList` and `SocketAddress` parse, format and match addresses as Node.js does, with these
  differences: `rules` lists address rules in the order they were first added, where Node.js lists
  them in hash table order. An IPv6 zone ID is accepted and ignored, so addresses that differ
  only in their zone are equal. A flow label above 0xFFFFF for IPv6, or above 2\*\*31 - 1 for
  IPv4, throws `ERR_OUT_OF_RANGE`, where Node.js aborts. Neither class can be sent with
  `postMessage`.
- Errors have Node's message, `errno`, `code` and `syscall`, plus `address` and `port` for
  `connect` and `listen`, and `hostname` for lookups.
- On wasm, `localhost` resolves to `127.0.0.1` and `::1` without asking the host, since
  `wasi:sockets` hosts may not resolve it. The wasm module must be run with network access, such
  as `wasmtime run -Sinherit-network=y,tcp=y`.

### `node:dns`

`node:dns` and `node:dns/promises`, which is `dns.promises`: `lookup` and `lookupService` over the
host's name lookup, `setDefaultResultOrder` and `getDefaultResultOrder`, and the resolver, with
`Resolver`, `resolve`, `resolve4`, `resolve6`, `resolveAny`, `resolveCaa`, `resolveCname`,
`resolveMx`, `resolveNaptr`, `resolveNs`, `resolvePtr`, `resolveSoa`, `resolveSrv`, `resolveTlsa`,
`resolveTxt`, `reverse`, `getServers` and `setServers`, and the error code constants.

- The resolver queries name servers itself, with DNS messages that `hickory-proto` encodes and
  decodes, over the UDP and TCP sockets of `platform::net`, natively and on wasm alike. A query
  goes over UDP, and over TCP after a truncated response. It goes to the servers in turn, `tries`
  times through all of them, with c-ares's timeouts: `timeout`, at least 250 ms and at most
  `maxTimeout` or 5 s, doubled for each pass after the first, reduced by a random amount of up to
  half after the first pass, and at most `maxTimeout`. Servers that failed are tried last.
  c-ares's adaptation of the timeouts to the latency it measures isn't implemented.
- Results, their key order, error codes (`ENODATA`, `ENOTFOUND`, `ESERVFAIL`, `EREFUSED`,
  `ETIMEOUT`, `ECONNREFUSED`, `EBADNAME`, `EBADRESP`, `ECANCELLED` and the others) and errors'
  `errno`, `code`, `syscall` and `hostname` are those of Node.js with c-ares. Query names go
  through UTS #46 and c-ares's checks, names in results are escaped as c-ares escapes them, and
  `.onion` names fail with `ENOTFOUND` without a query. A query without servers fails with
  `UNKNOWN_ARES_ERROR`, as in Node.js.
- A resolver's state is a `ResolverState`, which holds its servers, options, local addresses, and
  the queries in progress, which keep the event loop alive. `cancel` completes them with
  `ECANCELLED`, and `setServers` throws `ERR_DNS_SET_SERVERS_FAILED` while there are any.
  `_handle` is an own data property holding a plain object, which the methods don't use.
- `dns.setServers` and `dnsPromises.setServers` replace the default resolver and bind the
  module-level functions to the new one, as in Node.js. The ES module named exports keep the
  functions they had when the module was first imported.
- Natively, a resolver's servers are the `nameserver` lines of `/etc/resolv.conf` when it is
  created. Its options and search domains aren't used. Without servers, and on wasm, where WASI
  provides no resolver configuration, the server is `127.0.0.1`, c-ares's fallback, until
  `setServers` names others.
  A resolver whose only server is that fallback reads the configuration again after a query is
  refused, as Node.js does.
- `reverse` queries the PTR record of the address only, and doesn't consult `/etc/hosts`, which
  c-ares does first. It fails with `ENOTFOUND` for every error but `ECANCELLED`, as c-ares does.
- `setLocalAddress` sets the source address of UDP queries. TCP queries don't use it.
- Responses are bounded by the size of a datagram or of a TCP message, 64 KiB, and decoding
  rejects names whose compression pointers don't point backwards. A NAPTR record whose flags
  aren't alphanumeric makes the whole response `EBADRESP`.
- `lookup` filters the addresses of the host's name lookup by `family`. With `family: 6`, the
  `V4MAPPED` hint maps the IPv4 addresses to IPv6 if there are no IPv6 addresses, and with `ALL`
  also adds them after the IPv6 addresses. The `ADDRCONFIG` hint has no effect.
- `lookupService` uses the host's `getnameinfo` natively. On wasm, `wasi:sockets` has no such
  lookup, and it fails with `ENOTSUP`. Failed lookups are `ENOTFOUND`.
- Node.js's `--dns-result-order` flag isn't supported.

### `node:dgram`

UDP sockets, over tokio natively and `wasi:sockets` on wasm: `createSocket` and `Socket`, with
`bind`, `bindSync`, `connect`, `connectSync`, `disconnect`, `send`, `sendto`, `close`, `address`,
`remoteAddress`, `ref`, `unref`, the buffer size and send queue methods, the TTL, broadcast and
multicast methods, `Symbol.asyncDispose`, and the deprecated `_`-prefixed members.
`_createSocketHandle` is a stub.

- A socket's state is a `DgramState`, which holds its OS socket once bound, its bind and connect
  states, the operations waiting for an implicit bind, the sends the OS socket hasn't taken yet
  and the receive in progress. The OS socket is created when the socket binds, so options set
  before that throw `EBADF`, as in Node.js.
- Addresses go through the socket's `lookup` option, or through `dns.lookup`, which is read from
  `node:dns` for each lookup. An IP address of the socket's family needs no lookup.
- A send that the OS socket takes at once calls its callback in the tick queue. Others wait for
  the earlier ones, and their callbacks are called once sent. A datagram above 65535 bytes fails
  with `EMSGSIZE` without being sent. `--test-udp-no-try-send` makes every send wait.
- `unref` makes the receive in progress stop keeping the event loop alive, and it goes on
  receiving while other work keeps the loop alive. Sends and lookups keep the loop alive until
  they complete. Closing a socket can drop a datagram that arrived but wasn't emitted yet.
- `bind` doesn't take a file descriptor or a handle, and throws `ERR_METHOD_NOT_IMPLEMENTED` for
  them. The `exclusive` option has no effect, since there are no cluster workers.
- `addMembership`, `dropMembership` and the source-specific variants throw `EBADF` on an unbound
  socket, which Node.js binds instead. The source-specific variants support IPv4 groups only, and
  throw `ENOSYS` for IPv6 groups.
- `reuseAddr` sets `SO_REUSEPORT` on macOS and the BSDs and `SO_REUSEADDR` elsewhere, so
  sockets with the option can bind the same address and port, as in Node.js. `reusePort` is
  supported on Linux only. Elsewhere, binding with `reusePort` emits `ENOTSUP`.
- On wasm, `wasi:sockets` provides no broadcast and no multicast: `setBroadcast`,
  `setMulticastTTL`, `setMulticastLoopback`, `setMulticastInterface` and the membership methods
  throw `ENOTSUP` once the arguments are valid. `reuseAddr` and `ipv6Only` have no effect there,
  and binding with `reusePort` emits `ENOTSUP`. Every send waits. The wasm module must be run
  with UDP access, such as `wasmtime run -Sinherit-network=y,udp=y`.

### `node:tty` and `process.stdin`

`isatty` is `true` only for a standard stream connected to a terminal. `ReadStream` is the class
of `process.stdin`, a `Readable` of standard input, and only reads descriptor 0. The rest of the
module is stubs.

- A `ReadStream` is a `net.Socket` whose state holds standard input in place of a connection.
- `process.stdin` reads only while it flows or has `readable` listeners, and pausing it stops the
  read in progress, so a paused `process.stdin` doesn't keep the process running natively. On
  wasm, the process waits until standard input delivers more bytes or ends.
- `setRawMode` records the mode in `isRaw` and has no other effect.

### `node:readline` and `node:readline/promises`

Line mode only: `Interface`, `createInterface`, `cursorTo`, `moveCursor`, `clearLine`,
`clearScreenDown` and `emitKeypressEvents`, and the promise `Interface` and `createInterface`.
`Readline` is a stub.

- An interface emits a `line` event for each line of its input, which ends at `\n`, `\r\n`,
  `\r`, U+2028 or U+2029, and decodes `Buffer` input as UTF-8. A `\n` that starts a chunk within
  `crlfDelay` milliseconds of a chunk that ended with `\r` ends no line. A line read while a
  `question` is pending goes to the question instead, and the question's query is the prompt
  until then. An unterminated last line is emitted as a `line` event when the input ends. `[Symbol.asyncIterator]()` returns the same iterator on every call,
  Node's `events.on` iterator over the `line` events, which ends when the interface closes. It
  gives the lines emitted after it is created, rejects with an error the input emits, pauses the
  interface while more than 1024 lines wait, and leaves the interface open when a loop over it
  ends early.
- The `signal` option closes the interface when it aborts, or in a tick if it already has. A
  `question`'s `signal` option cancels the question when it aborts, and rejects the promise
  `node:readline/promises` returns with an `AbortError`.
- An error the input emits is emitted as the interface's `error`.
- `terminal` is always `false`: keypresses, history, completion and cursor editing are not
  supported, and `emitKeypressEvents` has no effect. The `history`, `historySize`, `tabSize`,
  `escapeCodeTimeout` and `completer` options are validated and stored as Node.js stores them,
  except that a `completer` is stored as it is, where Node.js wraps one that doesn't take two
  arguments.
- `close()` pauses the interface, which emits `pause`, before it emits `close`.
- `line` stays empty, as in Node's line mode, and `getCursorPos()` gives the column after the
  prompt.

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

- A stream's `_readableState` and `_writableState` are `ReadableState` and `WritableState`
  objects whose prototype accessors read and write the stream's state under the names Node's
  states expose, such as `objectMode`, `flowing`, `ended` and `needDrain`. They have no own
  properties. Assigning a value that isn't an array to `buffer` or `pipes` is ignored, as is
  assigning `bufferedIndex`.
- Work Node schedules with `process.nextTick` goes in the tick queue `process.nextTick` uses.
- In a `pipeline`, a function stage's result stays an iterable unless a Node stream consumes it.
  Web streams aren't accepted in a `pipeline`, and `finished` doesn't accept them.
- `duplexPair` returns two `Duplex` streams, each reading what the other writes.
- `Readable.toWeb`, `Readable.fromWeb`, `Writable.toWeb`, `Writable.fromWeb`, `Duplex.toWeb` and
  `Duplex.fromWeb` follow Node's `lib/internal/webstreams/adapters.js`, with the runtime's
  `ReadableStream` and `WritableStream`. `Duplex.toWeb` doesn't take the internal options Node's
  `CompressionStream` and `DecompressionStream` pass to it.
- The static `Readable.wrap(src, options)` wraps an old-style stream in a `Readable` that destroys
  `src` when destroyed.
- The experimental iterator helpers `map`, `filter`, `flatMap`, `drop`, `take`, `every`,
  `forEach`, `reduce`, `toArray`, `some` and `find` follow Node's
  `lib/internal/streams/operators.js`, including `concurrency`, `highWaterMark` and `signal`.
  Their async generators are objects whose `next`, `return` and `throw` run promise reactions,
  so a result can settle after a different number of microtasks than in Node.
- `compose` and `Duplex.from` are stubs, and `Readable.prototype.compose` doesn't exist.
  `node:stream/consumers` is a stub.

### `node:string_decoder`

`StringDecoder` holds back the bytes that end a chunk in the middle of a UTF-8 or UTF-16
character, or of a base64 group, until a later chunk or `end()` completes them. It follows
Node.js's `src/string_decoder.cc`, including where that differs from decoding the chunks'
concatenation, and has the legacy `lastChar`, `lastNeed`, `lastTotal` and `text`. Like Node's, it
can be called on any object, as constructors that inherit from it do.

### `node:punycode`

Punycode.js 2.1.0's API: `encode`, `decode`, `toASCII`, `toUnicode` and `ucs2`. Loading the module
emits DEP0040, unless it is loaded from a module in `node_modules`. `toASCII` and
`toUnicode` convert each label without case mapping or normalization, as Punycode.js does.

### `node:querystring`

`parse`/`decode`, `stringify`/`encode`, `escape`, `unescape` and `unescapeBuffer` are implemented,
including the `maxKeys`, `decodeURIComponent` and `encodeURIComponent` options. They follow
Node.js's `lib/querystring.js` on UTF-16 code units, so lone surrogates pass through `parse` and
`unescape`, and `escape` throws `ERR_INVALID_URI` for a surrogate that ends its input.

### `node:url`

`URL`, `URLSearchParams` and `URLPattern` are the globals. `fileURLToPath`, `pathToFileURL`,
`urlToHttpOptions`, `domainToASCII` and `domainToUnicode` are implemented, with POSIX paths only.
The legacy API, `Url`, `parse`, `format`, `resolve` and `resolveObject`, follows Node's
`lib/url.js`, including its escaping and IDNA hostnames, except that lone surrogates become
U+FFFD, and that `xn--` labels the `idna` crate rejects, which Node accepts, are errors.
`new Url()` assigns its `null` fields by copying them from a `Url` that each global creates when
it registers the class, with `Object.assign`'s semantics.

### `node:worker_threads`

Workers aren't implemented, and the module is a stub whose `MessageChannel`, `MessagePort` and
`BroadcastChannel` are the globals. They deliver messages within the global, with Node's
`ref()` and `unref()`, and `hasRef()` for ports:

- Adding a `message` listener with `addEventListener` starts a port, as `start()` does.
- A started port keeps the event loop alive while it is entangled, has a `message` listener and
  isn't closed, transferred or unreferenced. An open `BroadcastChannel` keeps it alive until it is
  closed or unreferenced.
- `port.on()` and the other `EventEmitter` methods of Node's ports don't exist.

### `node:util` and `node:util/types`

`util.inspect` follows Node's `lib/internal/util/inspect.js`, including its line breaking and the
grouping of long arrays into columns. Error stacks are rewritten into V8's frame format. It
doesn't show the entries of `Map` and `Set` iterators or the contents of `WeakMap` and `WeakSet`,
and a class instance without a prototype is shown without its class name. A proxy is shown as
its target without running its traps, or as its target and handler with `showProxy`. Colors are not
supported, though `inspect.colors` and `inspect.styles` hold Node's tables. `format` and `formatWithOptions` use `inspect` for `%o`, `%O` and non-string
arguments.

`isDeepStrictEqual`, `inherits`, `promisify` (with `promisify.custom`), `callbackify`,
`deprecate`, `debuglog`, `debug`, `getCallSites`, `stripVTControlCharacters`, `styleText`,
`toUSVString`, `getSystemErrorName`, `isArray` and `_extend` are implemented, and `deprecate`
takes the `modifyPrototype` option. `styleText` styles
its text whatever the output stream. `types` is `node:util/types`, and `TextEncoder` and
`TextDecoder` are the globals. The rest is stubs.

`node:util/types` implements the predicates with brand checks. `isExternal` and `isCryptoKey`
always return `false`.

### `node:assert` and `node:assert/strict`

The assertion functions follow Node's `lib/assert.js`, and a failed assertion throws an
`AssertionError` with `code` `ERR_ASSERTION`, the assertion's `actual`, `expected` and `operator`,
and Node's generated message, including its diff. Deep equality follows Node's
`lib/internal/util/comparisons.js`. `AssertionError` has an own `name` and a `toString` that
includes the code, as Node's does. The deprecated `CallTracker` follows Node's
`lib/internal/assert/calltracker.js`, and emits DEP0173 when the first one is created.
`partialDeepStrictEqual` and the `Assert` class are stubs.

### `node:test`

`test`, which is also `it` and the module's `default` export, `suite`, which is also `describe`,
their `skip`, `todo`, `only` and `expectFailure` variants, the hooks `before`, `after`,
`beforeEach` and `afterEach`, `getTestContext`, `mock` and `assert.register` are implemented.
`run` and `snapshot` are stubs, and so are `t.waitFor`, `t.log` and `mock.timers`. Module mocks
and `t.assert.snapshot` are missing.

- Tests run one at a time, in the order they start, whatever their `concurrency` option.
  Top-level tests start in a later job than the one that registered them, once the functions of
  the suites registered by then have finished. A test or suite registered while the function of
  another one or of its hook runs, including after an `await` in it, becomes its subtest.
- A test fails if its function throws or rejects, passes an error to its callback, runs out of
  time, misses its plan, or has a failed subtest. An uncaught exception or unhandled rejection in
  a running test's async context fails the test. A test waits for its subtests until none of
  them is pending for the first time, and subtests it started after that are cancelled if they
  haven't finished when it does.
- `only` leaves out the other tests, as Node.js does for a file it runs without `--test`.
- The `signal` option and `t.plan`'s `wait` option are ignored.
- The results are written with `process.stdout.write`, in the format of Node's `spec` reporter
  without colors. The list of failed tests doesn't show their locations. The summary is written
  when `process` emits `beforeExit`, and sets `process.exitCode` to 1 if a test failed. Tests that
  haven't finished by then are cancelled, where Node.js keeps waiting for them.

### `node:timers`, `node:timers/promises` and the timer globals

`setTimeout` and `setInterval` return `Timeout` objects instead of numbers, and `setImmediate`
returns an `Immediate`. A `Timeout` converts to its timer's ID, which `clearTimeout` and
`clearInterval` accept as well. `node:timers` exports the functions the globals start out as.

- `Timeout` and `Immediate` keep their state in Rust. Their own data properties are Node's, such as
  `_idleTimeout`, `_onTimeout`, `_timerArgs`, `_repeat` and `_destroyed`, and reflect the state.
  As in Node, a timer reads its callback and arguments from `_onTimeout` and `_timerArgs`, or
  `_onImmediate` and `_argv`, when it fires. Changing the other properties doesn't change the
  timer. `_idlePrev` and `_idleNext` are the timer itself while it is queued, where Node links
  its timer lists.
- `new Timeout(callback, after, args, isRepeat, isRefed)` creates a timeout without starting it,
  and `new Immediate(callback, args)` starts an immediate, as in Node.
- Clearing a timeout by its ID cancels its timer without marking the `Timeout` destroyed.

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
and `writeFile` are implemented, as are `createReadStream`, `createWriteStream`, `watch`,
`watchFile`, `unwatchFile`, `Stats`, `Dirent`, `constants`, and `node:fs/promises` with
`FileHandle` and `watch`. `fs.promises` holds the same functions as `node:fs/promises`. The rest
of the modules are stubs.

- A pending callback or promise form keeps the event loop it started in running until it settles,
  as a pending libuv request does in Node. So the operations a served request starts, such as the
  writes of a `WriteStream` that outlives the request, complete after its response is sent,
  within the serve layer's `waitUntil` time limit.
- Natively, the callback and promise forms run on four worker threads. Where
  [blocking isn't allowed](#synchronous-host-io), the synchronous forms throw
  `ERR_BLOCKING_NOT_ALLOWED`.
- On wasm, the callback and promise forms of `access`, `appendFile`, `close`, `exists`,
  `fdatasync`, `fstat`, `fsync`, `ftruncate`, `futimes`, `lstat`, `mkdir`, `open`, `read`,
  `readFile`, `readdir`, `rename`, `rmdir` without `recursive`, `stat`, `truncate`, `unlink`,
  `utimes`, `write` and `writeFile` use `wasi:filesystem`'s asynchronous API and never block, so
  these operations, `createReadStream`, `createWriteStream` and the matching `FileHandle` methods
  work while the runtime serves requests. Their paths
  resolve the way wasi-libc resolves the synchronous forms' paths: a relative path is made
  absolute against the working directory, the preopened directory whose name is the longest
  prefix of whole path components wins, and a preopen named `.` contains every path. Their errors
  are the ones the synchronous forms report.
- On wasm, the other callback and promise forms block the instance like the synchronous forms,
  and throw `ERR_BLOCKING_NOT_ALLOWED` where blocking isn't allowed. So do the descriptor
  operations on descriptors 0, 1 and 2 and on descriptors `openSync` opened. The synchronous
  forms work on descriptors from the callback and promise forms of `open`, and block on them.
- A `FileHandle` is an `EventEmitter` that emits `close` when it is closed. Its descriptor is
  the `fd` getter's, and it lacks `readv`, `writev`, `readLines`, `readableWebStream` and
  `getAsyncId`.
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
  options. `fs.watch` and `fsPromises.watch` do take `signal`.
- There is no file watching API on WASI, so the watchers poll with JS timers. `unref()` and
  `persistent: false` unreference a watcher's timer, so that it doesn't keep the event loop
  running, and `ref()` references it again. `fs.watch` and `fsPromises.watch` take a snapshot of
  the watched file or directory every 100 ms: the metadata of every entry, and with `recursive`
  of every entry below it, without following symlinks. A new, removed or
  replaced entry is a `rename`, and a file whose size, mode or times changed is a `change`.
  Directories whose entries changed aren't reported themselves. Filenames are relative to the
  watched directory, and changes of the watched path itself are named after its basename. A file
  created and removed between two polls is invisible to the snapshots, so when the watched
  directory's modification time changed without an entry appearing, disappearing or being
  replaced, the change is a `rename` with a `null` filename, as Node documents filenames may be.
  Changes from one poll are reported in path order. A watched file or directory that disappears
  is a `rename`, and the watcher keeps polling, so it reports another `rename` if the path
  reappears. A file replaced by another file is told apart from a changed one by its inode
  number, so on wasm and Windows, which report none, it is a `change`.
- `fs.watch` creates the watcher Node creates on the platform `process.platform` names. With
  `recursive` on Linux and on wasm, that is the watcher of Node's `internal/fs/recursive_watch`:
  its options are validated as Node validates them, `close()` emits `close` at once, `ref()` and
  `unref()` do nothing, it is an async iterable, the watched path's own removal is named `''`, and
  every change has a filename. Otherwise it is Node's `FSWatcher`, which emits `close` in the tick
  queue. `ignore` filters the reported changes by filename with minimatch's `matchBase` (see
  [`node:path`](#nodepath)), a `RegExp` or a function. The Linux recursive watcher also drops the
  changes below an ignored directory. `FSWatcher` and `StatWatcher` aren't exported, as in Node.
  Their Rust state holds the poll handle and the ignore matcher, which `_handle` and
  `_ignoreMatcher` mirror.
- `fs.watchFile` stats the file every `interval` milliseconds as libuv's `uv_fs_poll` does: the
  first stat runs right after the call, a failing stat reports zeroed `Stats` as the current ones
  once per error, and the previous `Stats` are those of the last successful stat.
  `process.getActiveResourcesInfo()` lists referenced `fs.watch` watchers as `FSEventWrap` and
  `watchFile` watchers as `StatWatcher`.

### `node:zlib`

The deflate, zlib, gzip, Brotli and zstd formats are implemented: the synchronous functions, such
as `gzipSync`, the callback functions, such as `gzip`, the stream classes and their `create`
functions, `crc32`, `constants` and `codes`. The zip archive functions are stubs.

- The callback functions compress or decompress synchronously, and call back in a tick.
- Errors have zlib's message, `code` and `errno`, such as `Z_DATA_ERROR` and -3.
- Every option is validated as Node validates it. `level`, `flush` and `finishFlush` are used.
  The synchronous and callback functions stop once their output exceeds `maxOutputLength`, and
  fail with `ERR_BUFFER_TOO_LARGE`. Streams push their output in buffers of at most `chunkSize`
  bytes, and stop producing output while the readable side is full, until it's read.
  `windowBits`, `memLevel`, `strategy` and `dictionary` have no effect. Streams have Node's
  `_chunkSize`, `_defaultFlushFlag`, `_finishFlushFlag`, `_maxOutputLength`, `_outOffset` and,
  for the zlib formats, `_level`, `_strategy` and `_mode` properties, and the `_closed` getter,
  but no `_outBuffer` or `_writeState`. `flush(kind)` validates `kind` against the flush
  constants of the stream's format.
- The legacy `Z_*` constants on the module object are missing. They're in `constants`.
- `flush(Z_FINISH)` runs as a full flush, without ending the compressed stream. A flush before the
  first input produces no output.
- `params` validates the level and the strategy as Node does, and flushes and records them in
  `_level` and `_strategy` unless both are unchanged. The codec ignores the strategy, and keeps
  its level when the stream has already compressed input and the new level uses a different zlib
  strategy function.
- Brotli's `params` are validated as Node validates them. `BROTLI_PARAM_MODE`,
  `BROTLI_PARAM_QUALITY`, `BROTLI_PARAM_LGWIN`, `BROTLI_PARAM_LGBLOCK`,
  `BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING`, `BROTLI_PARAM_SIZE_HINT` and
  `BROTLI_PARAM_LARGE_WINDOW` are used, and the others have no effect. A flush of a Brotli stream
  is `BROTLI_OPERATION_FLUSH`, and `flush()` defaults to it.
- zstd's `params` are validated as Node validates them, and have no effect: the compressors
  compress at one level, close to zstd's level 1. `pledgedSrcSize` and `dictionary` are ignored.
  A zstd compressor writes one frame when its input ends, and a zstd decompressor decompresses the
  first frame of its input when the input ends, so flushes don't produce output.
- Corrupt Brotli and zstd input can fail with another error than in Node.js, and some corrupt
  zstd input decompresses without an error.
- The Brotli and zstd stream classes extend the zlib stream classes' base class, where Node.js
  gives them their own.
- A stream's codec, flush constants, level, strategy and `bytesWritten` count are in its Rust
  state, which extends the state of the stream. `_handle` is an empty object until the stream is destroyed.
- Destroying a stream frees its codec and sets `_handle` to `null`, as in Node.js. `reset()` of a
  destroyed stream throws `ERR_INVALID_THIS`, where Node.js throws an `AssertionError`.

### `node:vm`

`vm.Script`, `createContext`, `isContext`, `runInContext`, `runInNewContext`, `runInThisContext`,
`createScript`, `compileFunction`, `measureMemory` and `constants` are implemented, with Node's
option validation. `vm.Module`, `vm.SourceTextModule` and `vm.SyntheticModule` aren't.

- A context's global is a new global in a new realm of the creating global's compartment, so
  objects pass between them without wrappers, and a context's objects have its own `Object`,
  `Array` and other prototypes. It has the standard classes and a `console` whose methods do
  nothing, as a new V8 context has, and none of the runtime's web or Node.js globals.
- A proxy of the contextified object is on the environment chain of the scripts that run in its
  context, so they find its properties before the global's own, and their `var` and function
  declarations define its properties. A declaration that the contextified object refuses, such as
  one in a frozen object, defines a property of the context's global instead. The global's
  prototype is a proxy that reads properties the global
  lacks from the contextified object and assigns them to it, and properties added to the global,
  such as by an assignment to an undeclared variable, move to it.
- The top-level `this` of a script that runs in a context is the proxy of the contextified object,
  where it is the context's global in Node.js. Reflection on the global (`Object.keys`, `hasOwnProperty`,
  `Object.getOwnPropertyDescriptor`, `delete`) sees the global's own properties only. `var`
  declarations are non-configurable properties of the contextified object, including those
  without an initializer, which Node.js leaves off it.
- `timeout` interrupts a script that runs past it with `ERR_SCRIPT_EXECUTION_TIMEOUT`, natively.
  On wasm, which has no threads, it isn't enforced. `breakOnSigint` and `microtaskMode` are
  validated and have no effect: microtasks run in the runtime's queue.
- `codeGeneration.strings: false` makes `eval` and `Function` throw an `EvalError`.
  `codeGeneration.wasm` has no effect.
- `import()` in a script fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`, or
  `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING_FLAG` for an `importModuleDynamically` function, which
  isn't called. With `vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER`, it loads modules as the
  runtime's loader does in a script that runs in the current global.
- No code cache is produced or used: `createCachedData` and `produceCachedData` produce an empty
  `Buffer`, and any `cachedData` is rejected.
- An error that a script created and throws gets Node's location, source line and caret
  prepended to its `stack`, unless `displayErrors` is `false`. The rest of such a stack is
  rewritten in V8's format, with the error's name and message first, as `util.inspect` shows
  SpiderMonkey's stacks.
- `measureMemory` resolves with the size of the GC heap.

## Testing

Three kinds of tests check the implementation.

- `crates/builtins/node/tests/js` has a script per module, each run by
  `cargo test -p node-builtins --test modules`, and the scripts that use host I/O also run on wasm
  in `scripts/test-runtime.sh`.
- `tests/node-harness/run-node-tests.mjs` runs Node.js's own tests from `test/parallel`, at the
  tag `scripts/clone-node-tests.sh` pins, with Node's unmodified `test/common`. Each test runs as a
  process of its own, with `--node-http-listen`, and passes if it exits with status 0.
  `tests.txt` selects the tests by file name: every test except those of Node's build tooling,
  V8's heap and trace tooling, the inspector and QUIC. The runtime gets the flags of a test's
  `// Flags:` line with `--allow-node-runtime-flags`. Tests whose flags the runtime rejects, and
  tests whose source needs child processes, workers, `cluster` or `inspector`, are skipped without
  running. `expectations.json` records each test's status, natively and, where it differs, on
  wasm. The run fails if a test expected to pass fails or skips. `--update-expectations` records
  the current results. `--target=deno` and `--target=bun` run the tests under Deno and Bun, as
  `scripts/fetch-other-runtimes.sh` downloads them, and `report.mjs --compare` lists the tests they
  pass and the runtime fails.
- `tests/node-diff/fuzz.mjs` compares the runtime with Node.js on generated programs. The
  generators in `generators.mjs` draw operations on the implemented modules, such as `Buffer`
  conversions with edge-case arguments, `StringDecoder` chunk sequences, event orders of streams
  and emitters, file system sequences, and HTTP exchanges between a listening server and
  `http.request` or raw requests over a `net` socket. Each program runs twice under Node.js, to
  drop operations whose result varies, and once under the runtime, with `--node-http-listen`. The
  fuzzer reruns each difference as a program of its one operation, groups the differences by
  signature, and writes a report and reproductions to `target/node-diff`. Calls to unimplemented
  functions share one signature per function. `known-diffs.json` lists the accepted signatures.
  The oracle is the Node.js release `scripts/fetch-node-oracle.sh` downloads, the same version as
  the Node.js tests.

```sh
./scripts/clone-node-tests.sh
node tests/node-harness/run-node-tests.mjs -j 8            # native, target/debug/starlingmonkey
node tests/node-harness/run-node-tests.mjs --target=wasm   # the release starling.wasm
./scripts/fetch-node-oracle.sh
cargo build --release -p starlingmonkey
node tests/node-diff/fuzz.mjs --programs=200 --only=buffer,stream
node tests/node-diff/fuzz.mjs --repro=target/node-diff/repro/<signature>.js
```
