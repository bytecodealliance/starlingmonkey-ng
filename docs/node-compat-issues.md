# Node.js compatibility: known issues

This document lists the known deficiencies of the Node.js compatibility work and its supporting
setup, as a list to work through. Each item states what is wrong, and what is known about the
cause. Items were collected from the work up to 2026-10-06 and from the reviews in the sections at
the end. Check that an item still applies before working on it, and remove it once it is fixed.

## Priorities for missing and incorrect APIs

Ordered by the use real applications make of them, not by the number of Node.js tests they
unblock:

1. `node:tls`, and `https` on the runtime's sockets. Database drivers (`pg`, `mysql2`, `mongodb`)
   connect with `tls.connect`, and `https` requests go through reqwest and `wasi:http` today.
   `platform::tls` runs rustls client sessions over the runtime's sockets natively, which
   `WebSocket` uses for `wss:`. On wasm, the pure-Rust rustls providers need `getrandom` 0.2, which
   makes the component import WASIp2 interfaces (see `WebSocket` below).
2. Asymmetric cryptography: `generateKeyPair`, `createPublicKey`/`createPrivateKey`, `KeyObject`,
   `sign`/`verify`, `publicEncrypt`/`privateDecrypt` and `diffieHellman` for RSA, ECDSA, Ed25519
   and X25519, and `crypto.subtle` (WebCrypto). JWT libraries need them: `jsonwebtoken` for RS256
   and ES256, `jose` through WebCrypto. RustCrypto's pure-Rust crates build for wasm.
3. Unix domain sockets for `net`, and `socketPath` for `http`, which local databases and the Docker
   API use. Natively only, since WASI has no Unix sockets.

Node.js internals that only Node's own tests load (`internal/repl`, `internal/webstreams/*`,
`internal/dgram` and others) and `repl`, `cluster` and `worker_threads` come after these.

## Correctness

### Regressions and unexplained failures

- **`js::Unbox` crashes on scripted proxies.** A guard in `unbox` avoids the segmentation fault,
  but the root cause in the binding is unknown. A cross-compartment wrapper around a proxy may
  still reach it.
- **The Express `data` app differed on one request occasionally.** It passed on rerun. The cause
  is unknown. Its scenario waited 100 ms for the server to subscribe to new posts, and the app
  waited 50 ms for its log file, where both now wait for a readiness signal. On 2026-10-10 neither
  version failed: each passed 30 runs natively and 30 on wasm, 10 of each with 12 busy processes
  on the 10 CPUs.

### Behavior that differs from Node.js

- `url.resolveObject` and `Url.prototype.resolveObject` give the result the fields they copy from
  the relative URL object as strings or `null`, where Node.js copies the values: a `port` of `80`
  becomes `'80'`, a `hash` of `0` becomes `null`, and a missing `hash` becomes `null` instead of
  `undefined`. The legacy URL code holds each field as a Rust `Option<String>`.
- The DNS resolver differs from c-ares in these ways: `reverse` doesn't consult `/etc/hosts`,
  timeouts don't adapt to the measured latency of a server, `setLocalAddress` doesn't apply to
  TCP queries. The Node.js tests that replace
  `internalBinding('cares_wrap')` functions (`test-dns-default-order-*`, `test-dns-lookup`,
  `test-dns-lookup-promises`, `test-dns-lookupService`, `test-dns-resolve-promises` and
  `test-dns-set-default-order`) and `test-dns-get-server`, which replaces
  `resolver._handle.getServers`, fail, since the implementation doesn't go through those objects.
- An unreferenced child process that a served request spawned and that outlives the request's
  event loop is never reaped, and stays a zombie until the server exits: waiting for its exit is
  cancelled with the request's other pending work, and `platform::child::Child` doesn't reap on
  drop. Its `exit` and `close` aren't emitted. A referenced child keeps the loop running instead.
- `net.Server.prototype.listen` ignores `backlog`, `ipv6Only`, `reusePort` and `exclusive`:
  `platform::net::listen_stream` takes no socket options, and natively listens with a backlog of
  511.
  `exclusive` has no effect in Node.js either without cluster workers.
- The `ADDRCONFIG` hint of `dns.lookup` and of a connect's lookup has no effect, since
  `platform::net` can't list the addresses the host's interfaces have. Node.js's `getaddrinfo`
  then leaves out the addresses of a family the host has no address of.
- `eos`, `pipeline` and `addAbortSignal` reject web streams. Node.js accepts them. The stream
  predicates misreport web streams too (see the review findings).
- Assigning a non-array to `_readableState.buffer` or `_readableState.pipes`, or any value to
  `_writableState.bufferedIndex`, is ignored. Node.js throws a `TypeError` later.
- The callback of a socket write that `destroy` cancels is never called. Node.js calls it with an
  `ECANCELED` error.
- `resetAndDestroy` on a connected `net.Socket`, or one that is connecting, closes the connection
  normally instead of sending a reset, since `platform::net` can't reset a connection.
- A socket `end` that waits for the connect gets `ERR_SOCKET_CLOSED_BEFORE_CONNECTION` if the
  socket is destroyed first. Node.js never calls that callback.
- An `OutgoingMessage` or `ServerResponse` whose `highWaterMark` option isn't a number has the
  default high water mark. Node.js keeps the value, which `writableHighWaterMark` returns.
- A file `ReadStream` whose `pos` is set to a value that converts to a number but that `fs.read`
  rejects as a position, such as `"3"` or `true`, is destroyed with that error. Node.js destroys
  it without emitting `error` or `close`.
- The `fd`, `path`, `flags`, `mode`, `start`, `pos`, `end`, `bytesRead` and `bytesWritten` of file
  streams are enumerable accessors on `ReadStream.prototype` and `WriteStream.prototype`, where
  Node.js has own data properties, so `Object.keys` and `util.inspect` of a stream don't list them.
  WriteStreams also lack Node's `flush` property.
- Object shapes:
  - Readables have their own `_eventsCount` and `shapeMode: false`. Node.js pre-shapes `_events`
    and sets `shapeMode: true`.
  - `_events` has a null prototype.
  - `_readableState` is defined before `_events`.
  - `_readableState` has no own keys. Node.js has six.
  - `joinDuplicateHeaders` is `false` where Node.js has `null`.
  - `_onPendingData` has a different function name.
  - Emitters have Node's `shapeMode` and `kCapture` symbol properties, but the other internal
    symbol properties are absent, such as `kOutHeaders` and `kSocket` on `OutgoingMessage` and
    `ClientRequest`, and `kCallback` and `kError` on zlib streams.
- `structuredClone` throws for instances that have Rust state. Node.js clones them as plain
  objects.
- Reassigning a constructor's `prototype` after its first use isn't observed by native instance
  creation and instance checks.
- Objects the runtime creates natively, such as served requests and responses, get their own
  properties defined, so setters on the module's own prototypes aren't called.
- `process.stdout` and `process.stderr` are `EventEmitter`s with a `write` method, not `Writable`
  streams. Their `write` also ignores `encoding` and calls its callback synchronously (see the
  review findings).
- `new Console(stream)` routes `group`, `count`, `time` and `table` to the global console, not to
  the given stream.
- Messaging (`MessageChannel`, `MessagePort`, `BroadcastChannel`):
  - Open `BroadcastChannel`s stay in a per-global registry until `close()`, including channels
    without listeners, which the spec lets the garbage collector free. A server that creates
    channels and never closes them grows without bound.
  - Disentangled ports emit no `close` event, keep-alive isn't recomputed when a listener is
    removed through an `AbortSignal`, and ports lack Node's `EventEmitter` methods (`port.on`).
  - Within one event-loop step, messages run before timers. Node.js runs timers first.
  - `CompressionStream` and `DecompressionStream` reject string chunks, which Node.js accepts.
- `node:perf_hooks`:
  - The timeline classes are the web platform's, whose `PerformanceObserver` callbacks get a
    third argument.
  - No `resource`, `http`, `http2`, `net` or `dns` entries are produced, and
    `markResourceTiming`, `setResourceTimingBufferSize` and `clearResourceTimings` are missing.
  - Histograms can't be transferred through a `MessagePort` or cloned, and `samplePerIteration`
    samples with a timer. `nodeTiming.uvMetricsInfo` reports zeros.
- `node:zlib` lacks the `rejectGarbageAfterEnd` option.
- The errors of the web classes (see
  [Errors of the web classes](node-compat.md#errors-of-the-web-classes)) differ from Node's in
  these cases:
  - Node.js validates values that WebIDL converts: `AbortSignal.timeout` takes any number, where
    Node.js requires an integer from 0 to 2³²−1, `TextEncoder.prototype.encodeInto` converts its
    source to a string, `addEventListener` takes any non-object `options` as `capture`,
    `performance.measure` converts a number to a mark name, and `URLPattern` and its `exec` and
    `test` methods convert a number to a string. Node.js throws `ERR_INVALID_ARG_TYPE` or
    `ERR_OUT_OF_RANGE` for these.
  - Options that are WebIDL dictionaries, such as those of the `Event` constructors,
    `performance.mark` and `PerformanceObserver.prototype.observe`, take arrays and functions,
    which Node's `validateObject` rejects with `ERR_INVALID_ARG_TYPE`. `new File(undefined, name)`
    throws, where Node.js creates an empty file.
  - A `postMessage` transfer list element that isn't an object throws a `TypeError`, where
    Node.js throws a `DataCloneError` `DOMException`.
  - `this` of `undefined` is the global object for the `EventTarget` methods, as WebIDL's
    `[Global]` semantics define, where Node.js throws `ERR_INVALID_THIS`.
  - Errors that V8 or undici create have V8's or undici's messages in Node.js, and the web
    messages here: converting a symbol to a string, `ReadableStream.from` with a value that
    isn't iterable (`ERR_ARG_NOT_ITERABLE`), the controllers of `ReadableStream` called on
    another object, `MessageChannel`'s getters, `Headers` and `Request` conversions, the
    `MessageEvent` constructor's dictionary, and invalid header names and methods.
  - The message of a call of a value that isn't a function names the callee as SpiderMonkey's
    expression decompiler prints it, where V8 quotes the source text. In CommonJS modules,
    functions and `vm` scripts, an array literal prints as `[...]` (`[...].nope is not a
    function` for `[1].nope()`), and `obj?.nope()` and `(0, obj.nope)()` print as `obj.nope`.
    Builtins that call a method of an argument that has none name only the method (`on is not a
    function`), where Node.js names its own variable (`dest.on is not a function`).
  - The `DOMException`s of `atob`, `btoa`, `crypto.getRandomValues`, `structuredClone`,
    `BroadcastChannel.prototype.postMessage` and `WebSocket`'s constructor, `send` and `close`
    have the right names and codes, but the web messages. `crypto.getRandomValues` of a value
    that isn't a typed array throws a `TypeError` where Node.js throws a `TypeMismatchError`
    `DOMException`.
  - Of the stream errors, the `ERR_OUT_OF_RANGE` and `ERR_INVALID_ARG_VALUE` errors of a BYOB
    reader's `min` option and of `respond`'s `bytesWritten`, and the `RangeError` of a size
    function's result, have the web messages without codes. `ERR_INVALID_STATE` errors without
    a Node.js counterpart in `errors/web.rs` have the web message as their detail.
  - `TextDecoderStream` rejects a chunk that isn't a buffer source with the web error, where
    Node.js throws `ERR_INVALID_ARG_TYPE`.
- `console.log` truncates strings at a NUL character, in the C++ console.
- `Buffer.from` rejects a `SharedArrayBuffer`.
- `Buffer.allocUnsafe` and `Buffer.from` of a string don't allocate from a pool, so their
  `byteOffset` is 0 and two of them never share an `ArrayBuffer`.
- `StringDecoder.prototype.lastChar` returns a copy of the buffered bytes. In Node.js it views
  the decoder's state, so writing to it changes what the decoder decodes next.
- `util.inspect` shows no entries for Map and Set iterators, labels `map.entries()` as
  `[Map Iterator]` where Node.js writes `[Map Entries]`, and doesn't recognize module namespace
  objects. It detects generator and async functions unless their `Symbol.toStringTag` was changed.
- Recursion limits differ from Node's. Natively, they follow from SpiderMonkey's stack quota for
  untrusted script, which mozjs's `Runtime::new` derives from the thread's stack size. On a macOS
  main thread, release builds format 2675 levels with `util.inspect` and `depth: null` (Node.js
  903), compare 9993 levels deeply (Node.js 1110), expand 9368 levels of brace globs (Node.js
  3774) and 8326 of extglobs (Node.js 757), and recurse through 47860 JavaScript frames (Node.js
  10382). On wasm, a depth of 350 nested recursion checks bounds them,
  which `RecursionGuard` levels and the engine's own checks count together: inspection stops at
  345 levels, deep comparison and brace globs at 345 and extglobs at 172. `--recursion-limit`
  raises the depth for hosts with a larger native stack (see `README.md`). Beyond the limits,
  inspection prints Node's "Inspection interrupted prematurely" marker and the others throw a
  `RangeError`.
- `node:domain`:
  - The callback hooks enter and exit a callback's domain without calling its `enter` and `exit`
    methods, so overrides of those methods don't run for implicitly bound callbacks.
  - Promises created in a domain have no `domain` property.
  - Instances of `Domain` lack Node's `kWeak` symbol property.
  - `domain._stack` is a new array on each read, so changing it doesn't change the stack.
  - While a domain stays entered after the code that entered it returned, Node.js also enters it
    around each run of its timer and immediate queues, through `MakeCallback` on `process`, and
    emits `DEP0097`. The runtime doesn't.
- An uncaught exception from `-e` code is printed without the source line and caret Node.js shows.
  `parseArgs` doesn't apply Node's default `args` for `-e`.
- `assert.ok(value)` and `assert(value)` without a message fail with `The expression evaluated to
  a falsy value` alone. Node.js appends the source text of the call, such as `assert(1 === 2)`,
  which it finds by tokenizing the calling line that V8 keeps for the captured frame. No runtime
  API reads the source text of the calling script.

### The HTTP client

Requests over the runtime's sockets, which `http:` requests in a script the CLI runs as a command
are (see `docs/node-compat.md`):

- `socketPath` fails, since `net` has no IPC sockets.
- Requests an agent queues until it has a socket don't keep the async context they were created
  in, so `AsyncLocalStorage` reads in their `socket` and `response` listeners see the context of
  the code that freed the socket.
- A listener that throws while the parser handles a response's head or body leaves the parser
  failed and reports the exception, as in Node.js, but the socket's other `data` listeners still
  get the chunk, which Node.js's exception skips.
- `https.Agent` has `http.Agent`'s `getName` and `createConnection`, and lacks the TLS session
  cache methods.
- `proxyEnv`, `NODE_USE_ENV_PROXY` and `--use-env-proxy` have no effect.
- The `http.client.*` diagnostics channels get no messages, and `perf_hooks` gets no `http`
  entries.
- A client socket can read the end of the connection before its response's `end` closes it,
  where Node.js destroys the socket first, so the socket emits `end` where Node.js's doesn't.
  The order depends on when the peer's FIN arrives.
- `net` doesn't emit `lookup` without the `lookup` option, or `connectionAttemptFailed`.

Requests through the runtime's HTTP client, reqwest natively and `wasi:http` on wasm, which
`https:` requests and every request while the runtime serves requests are:

- It adds `accept: */*`, lowercases header names, normalizes paths, strips hop-by-hop headers and
  reports the standard reason phrase.
- It exposes no 1xx responses or `information` events.
- Upgrades and `CONNECT` aren't supported, so `ws` as a client can't connect in serve mode, in
  components or to `wss:` URLs.
- Agents only keep the sockets of finished requests for reuse. A keep-alive agent's
  `freeSockets` ignore `maxFreeSockets`, and empty per-host arrays are never deleted.

### Missing APIs

- `node:test`: `TestContext.tags`, `MockTracker.property`.
- `fs`: `FileHandle` `readv`, `writev`, `readLines` and `readableWebStream`.
- `crypto`:
  - `KeyObject.toCryptoKey`. `KeyObject.from` always throws, because `CryptoKey` isn't
    implemented.
  - Asymmetric keys, signatures and ciphers. There is no OpenSSL.
- `zlib`:
  - zstd compresses at one level only, and its output appears only when the input ends.
  - Some corrupt-input error codes of the Brotli and zstd decoders differ from Node's.
  - The Brotli and zstd stream classes share the `Zlib` base class.
  - Zlib streams lack `_outBuffer` and `_writeState`.
- `Hash` and `Hmac` aren't `LazyTransform` streams.
- Fetch and files: `Body.blob()` throws, and `Blob.textStream` is missing.
- `stream`: `Readable.prototype.compose`.
- `net.Socket` lacks Node's `on`/`addListener` overrides.
- `WebSocket`:
  - `wss:` on wasm. The rustls providers that build for wasm without a C toolchain need
    `getrandom` 0.2, whose WASI backend makes the component import WASIp2 interfaces. A provider
    whose key generation and `SecureRandom` use `platform::random` would remove that dependency.
  - `permessage-deflate`, which tungstenite doesn't implement, so `extensions` is always empty.
  - A timeout for the server to close the connection after the closing handshake.
  - tungstenite accepts a handshake response's `Connection` header only if it is exactly
    `Upgrade`, where RFC 6455 accepts a list of tokens that contains it.
  - Node's `dispatcher` option.
- Unimplemented modules: `repl`, `worker_threads`, `cluster`, `inspector`.
- `child_process`:
  - `serialization: 'advanced'` and sending handles over IPC (`send(message, socket)`).
  - `stdio` entries that are TCP `net.Socket`s and other streams without a descriptor, which
    Node.js passes to the child as handles.
  - `internal/child_process` can't be loaded, so the tests that replace its functions fail.
  - The child-process tests also fail for gaps elsewhere: `net.Socket` doesn't take `fd`,
    `--stack-size` is rejected, `process.argv0` is `starlingmonkey` rather than the `argv[0]` the
    process got, and `process.openStdin` and `process.getgroups` are stubs.
- `vm`:
  - `vm.Module`, `vm.SourceTextModule` and `vm.SyntheticModule`.
  - The top-level `this` of a script in a context is a proxy of the contextified object, not the
    context's global, and reflection on a context's global doesn't see the contextified object's
    properties.
  - `timeout` isn't enforced on wasm. `breakOnSigint` and `microtaskMode` have no effect.
  - `import()` with `importModuleDynamically: vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER` in a
    script that runs in a context fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`, where
    Node.js loads the module with the main context's loader. A context is a global of its own,
    and the engine's module loading creates the imported module in the importing global, where
    the Node.js classes that builtin modules such as `fs` and CommonJS loading use aren't
    registered. Loading the module in the main global and resolving the context's `import()`
    with that namespace needs another import path in `core_runtime::module`.
  - No code cache is produced or used, and a `vm.Script` is compiled again each time it runs.
    The stencil APIs that would compile a script once for every context return
    `already_AddRefed` by value, which the mozjs bindings can't call. A glue function in the
    mozjs fork's engine patch would make them usable.

### Serve mode and components

- Synchronous `fs` is refused while serving. EJS calls `fs.existsSync` on every render, so the
  `ssr-site` app needs `--allow-blocking-host-io`.
- Timers and `http.request`s still running after a served response ends are abandoned. Open
  response bodies and pending `fs` operations keep a request alive, but timers and outgoing
  requests don't.
- WebSocket upgrades can't work in serve mode, because served requests have no raw socket.
  Client requests there go through the runtime's HTTP client, which can't upgrade either.
- Under Spin, `wasi:http` provides no client address, so `req.ip` is undefined and
  `express-rate-limit` logs `ERR_ERL_UNDEFINED_IP_ADDRESS`.
- Spin's default instance reuse (128 requests per instance, 1 s idle) drops in-memory state such
  as sessions between instances.
- A component's snapshot holds the timers its top level leaves pending, such as the unreferenced
  `setInterval` with which `express-rate-limit`'s `MemoryStore` rotates its windows, and the
  resumed instance runs them. A snapshot still can't hold other pending work, such as a `fetch` in
  flight or a listening `MessagePort`. A component that exports neither `wasi:http/handler` nor
  `wasi:cli/run` doesn't run the timers it holds.
- wasmtime's HTTP client strips `Transfer-Encoding` and `Connection` from proxied responses.
- `fetch` to a blocked port reports a different error than Node.js. `fetch` doesn't check the
  port with `web_fetch::algorithms::is_bad_port`, which `WebSocket` uses.
- A `WebSocket` still connecting or open when its request's response is sent is abandoned, as
  timers are.

### wasm platform limits

These tests fail only on wasm because of the platform:

- Absolute symlink targets give `EPERM`.
- `process.execPath` is `starling.wasm`.
- `::1` is refused.
- The stack is shallower: recursion stops at a depth of 350 nested recursion checks, which fits
  wasmtime's default 512 KiB native stack. Deep values throw a `RangeError` or print the
  inspection marker, as natively, instead of trapping.
- The wasm build has no `WebAssembly` global.

## Performance

- **CPU per request is about twice Node's.** On rest-api `/health` at c=32 natively it is about
  45 µs per request, against Node's 23 µs. The latest profile is in `docs/express-benchmarks.md`.
- **Adding properties to new objects.** The messages, `Url`s, timers, `Stats` and `Dirent`s the
  builtins create get all their properties in one engine call, and fixed slots for them, up to
  SpiderMonkey's 16 fixed slots, which include the state slot of a class's instances. Most of the remaining time is SpiderMonkey's `NativeObject::addProperty`, one shape
  transition per property, and growing the dynamic slots of a `ServerResponse` (33 properties) and
  an `IncomingMessage` (23). Creating them with a shape cached per template and
  their dynamic slots allocated up front needs more glue. Objects that constructors called from
  JavaScript create, such as the `new Url()` of Express's `parseurl` and each `net.Socket`, still
  get their properties one by one, by assignment as in Node.
- **Memory under load grew.** Peak RSS under load rose by 17 to 43% on six of the seven apps with
  the state refactor, more than throughput rose. The cause is unknown. One candidate is Rust state
  that stays alive until its wrapper is finalized.
- **Memory of components.** Components reach 0.9 to 1.6 GiB RSS under wasmtime and 1.9 to 2.6 GiB
  under Spin. Spin's process holds about 1.3 GiB after start. The causes are unknown.
- **Concurrency scaling.** Throughput gains less from concurrency than Node's does. The cause is
  unknown.
- **`setTimeout(1)` takes about 2.3 ms**, against Node's 1.2 ms. The native event loop waits with
  `tokio::time::sleep`, which has millisecond resolution.
- **No JIT in wasm.** JavaScript runs about 4.6 times slower in components than natively.
- **`fs.watch` polling.** Each watcher walks its directory tree synchronously on the JS thread
  every 100 ms.
- **CommonJS.** An ambiguous `.js` file with ES module syntax is compiled twice: the compile as a
  CommonJS function body fails, and the file is compiled again as a module. Its `Module` object is
  dropped and removed from the parent's `children`, where Node.js keeps it with the namespace as its
  `exports`. Ambiguous CommonJS files compile once, except an entry script: `is_commonjs_entry`
  compiles it as a function body to choose the CommonJS shim, the shim's load reads the file again,
  and `run_wrapped` compiles it again. The prefetch cache keeps every required source and resolution
  for the instance's whole lifetime: `PREFETCHED` and `RESOLVED` in `commonjs.rs` are never pruned,
  and `read_source` clones the whole source on every hit. For components, the sources are also part
  of the Wizer snapshot's linear memory. Removing an entry on its first load fixes both.
- **`querystring.parse`** reads back the result object it builds, where a Rust accumulator would
  write it once.

## Setup and tooling

- **Crown can't run in a nested worktree.** `check-crown.sh` fails in agent worktrees. Running crown
  there needs a temporary empty `[workspace]` table in `crown/Cargo.toml`, or the main checkout's
  prebuilt binary.
- **Flaky tests:**
  - `test-http-server-headers-timeout-keepalive`, `test-http-server-request-timeout-keepalive`,
    `test-string-decoder-fuzz`, `test-net-socket-reset-send`, `test-net-connect-reset-after-destroy`
    and `test-http-parser-free` (marked `FLAKY`)
  - `test-http-agent-maxsockets` on wasm (marked `FLAKY` there): in about half of the runs one of
    the agent's two sockets isn't free yet when the test counts the free sockets. The cause is
    unknown.
  - WPT `hr-time/basic.any.js`
  - The Express `realtime` app, and `auth` timing out at startup on wasm. The `realtime` scenario
    waited 100 ms for a long poll to start and for the server to remove closed WebSockets, where
    it now waits until `/stats` shows both. On 2026-10-10 neither version failed: each passed 30
    runs natively and 30 on wasm, 10 of each with 12 busy processes on the 10 CPUs. So the cause
    of the earlier failures is unknown.
  - The Express `rest-api` app on wasm once printed that it was listening on its port, but no
    connection to `127.0.0.1` on that port succeeded for 30 seconds (2026-10-08). It passed on the
    next 10 runs. The cause is unknown.
- **Fuzzer signatures not on the accepted list.** `http/raw:log-differs["bytes"]` appears in about 1
  in 15,000 operations because of microtask ordering. It isn't on the accepted list, because that
  key would hide real bugs. `http/raw:log-differs["clientError socket"]`, which has the same
  cause, didn't occur in 20,000 programs on 2026-10-10 and was taken off the list.
  `zlib/fixture`, which decompresses corrupted input, has a long tail of error code pairs: three
  runs of 10,000 `zlib/fixture` programs found 4, 2 and 0 pairs that weren't listed yet. A new
  pair fails the run until it is added.
- **Spin.** The bundle needs `--allow-transient-write`, and Spin's `--direct-mounts` resolves mount
  paths against the current directory, which makes it unusable for the bundle.
- **`node-builtins` is always linked** into every runtime and component, even without
  `--node-compat`.
- **Failures that predate this work:**
  - Clippy fails in `componentize/tests/lowering.rs`.
  - Crown reports 20 errors in `component-model`.
  - One `test-wizer.sh` case expects a stale message.
- **The Node.js shape reference (`shapes-node26.txt`) came from Node 26.** The oracle is v24.21.0,
  so shape claims need checking against the oracle. The fuzzer runs its generators under the
  oracle.
- **The first wasm harness runs after a rebuild time out.** wasmtime compiles a new
  `starling.wasm` in each of the parallel processes until its cache holds the module, which can
  exceed the 30 s timeout. The wasm runs also time out when anything else loads the machine. The
  harness warns when the runtime binary is older than the last commit, since `just test-wasm`
  doesn't build `target/wasm32-wasip3/release/starling.wasm`.
- **Agents stall on long foreground commands and very large edits.** Run long jobs in the
  background and make edits in small steps (see `docs/builtin-development.md`).

## Accepted exceptions to the rules

These deviate from the rules in `docs/builtin-development.md` on purpose:

- `EventEmitter`'s `_events`, `_eventsCount` and `_maxListeners` stay own data properties, because
  libraries read and change them in place.
- An ES5 constructor called on a foreign object (`Base.call(obj)`) links a state handle from the
  object under one private name symbol, `STATE_HANDLE_KEY`. JavaScript can't read, list, copy or
  delete the property, as for `#private` fields.
- `Props` remains for reading caller options and Node-public properties that Node.js itself reads
  dynamically. That includes the `aborted` and `reason` of a signal, which `readline` and other
  modules read, and its `addEventListener`, which they call, by name from any object
  `validateAbortSignal` accepts, as Node.js does.
- An `http.Agent`'s `requests`, `sockets`, `freeSockets` and `totalSocketCount` stay own data
  properties, which the agent reads and changes as Node.js's does, because libraries such as
  `agent-base` change them in place.
- The setup of the Node.js globals wraps the methods of the global `console` object, which the
  C++ builtins create without a Rust handle. It reads the object from the global when the global
  is set up, before any content code runs.
- `Readable.toWeb` defines Node's `kErrorSentinelAttached` symbol property on the streams it
  adds its `error` listener to, as Node.js does. It reads the property back only for a stream
  without Rust state, and reads a flag in the state otherwise.
- `node:domain` keeps a domain's `members` array and the `domain` property of emitters, timers,
  async resources and bound functions as own data properties, which it reads back as Node.js's
  does, because libraries read and assign them. The errors a domain handles get the same own
  properties as in Node.js.

## Review findings (2026-10-06)

An independent review of `crates/builtins/node`, the branch's changes to `crates/js`,
`crates/starling-macro`, `crates/core-runtime`, `crates/libstarling` and the web builtins, and the
test tooling. Line numbers refer to 453be54f. Each entry gives its severity:

- High: a crash, a GC hazard or data corruption reachable from JavaScript, or a semantic bug that
  real applications hit.
- Medium: a rule violation, a leak, a semantic deviation in an edge case, or a moderate cost on a
  hot path.
- Low: a minor deviation or cost, a small duplicate, or a comment defect.

"Prebuilt binary" means a release build of a working state near 453be54f. The oracle is
`deps/node-bin` (v24.21.0). `net/blocklist.rs` was only partly reviewed (lines 60 to 170 and 270
to 430, and a search for lookups, panics and conversions). `exports_table.rs` was only checked
for its structure.

### Rule compliance

- **Medium: emitter state in symbol-keyed properties.** `events.rs` keeps `kCapture` and
  `shapeMode` in own symbol-keyed properties and reads them back as state: `kCapture` in
  `captures_rejections` (events.rs:88) for every listener that returns a value, and `shapeMode` in
  `removeListener` (events.rs:509). `emit_rejection` saves, sets and restores `kCapture` around an
  emit (events.rs:732-745). The EventTarget listener limit (events.rs:1095, events.rs:1134) uses
  the same pattern. The accepted exceptions cover only `_events`, `_eventsCount` and
  `_maxListeners`. Either add these to the accepted exceptions, since Node.js defines the same own
  properties and any object can be an emitter, or keep the values in Rust state and write the
  properties without reading them back.
- **Low: error values made by throwing and taking the exception.** buffer.rs:2575-2585 (transcode,
  which also ignores a failed `set_property` of `errno`), util.rs:236-246, crypto.rs:1102-1103,
  events.rs:593-607, process.rs:798-803, and core-runtime module.rs:489-505, which also duplicates
  the node crate's `blocking_not_allowed` (errors.rs:62) without its `toString`. console.rs:160-162
  discards an exception with `take_pending_or_undefined`. Use `NodeError::to_value` or
  `js::error::new_error`, one shared helper for the blocking refusal, and `exception::clear`.
- **Low: `then` with an unused derived promise, and next-tick work through bound functions.**
  readable.rs:1858 and readable.rs:1883-1887, adapters.rs:1099, 1511, 1530 and 1568, and
  fs/streams.rs:238-242 call `then` and discard its result. `emit_in_tick` (watch.rs:770-783) calls
  `emit.bind` by name and queues the bound function. Use `Promise::add_reactions` and
  `js::jobs::queue_native_tick` with a payload.
- **Low: duplicate helpers in the node crate.** The inline argument copy in events.rs,
  `rest_args` in timers.rs and `args_from` in async_hooks.rs duplicate `CallbackArgs::rest`, and
  `events::with_method` duplicates `support::has_method`.
- **Low: `.map_err(|_| ExnThrown)` on `FromJSVal` conversions.** state.rs:878, adapters.rs:257,
  605 and 1316, operators.rs:720 and 1300, six sites in inspect.rs, readline.rs:955 and 1027,
  errors.rs:191 and 252, os.rs:36, util.rs:974 and punycode.rs:389-430. The `f64` and `String`
  conversions only return `ExnPending`, so these are correct today, but the rule prescribes
  `from_jsval_throwing`. At adapters.rs:257 Node.js passes `writableHighWaterMark` through
  unconverted, and a foreign writable without it gets `NaN` here instead of the default.
- **Low: a runtime object reached by name.** process.rs:1439 reads `process` back from the
  global right after `functions::add_to_global` defines it. Use the registration's return value.
- **Low: implemented functions without a Node.js documentation link.** All `*Sync`, callback and
  `create*` functions in zlib.rs:1725-2016, the `Stats` and `Dirent` `is*` methods
  (fs/classes.rs:116-143 and 253-280), the operations in fs/ops.rs, and `_write`, `_undestroy`,
  `_destroy`, `pipe`, `_final`, `_transform` and `_read` in writable.rs:997-1047 and
  duplex.rs:374-501.
- **Low: misplaced and wrong doc comments.**
  - The doc of `register_classes` is attached to `register_templates` in lib.rs:137-140 and in
    http.rs:443-444, so neither `register_classes` has one.
  - errors.rs:353-356 merges two doc comments, so `inspect_short` has none, and the doc of
    `validate_function` sits on `validate_boolean` (errors.rs:623-626). events.rs:576-579 repeats a
    sentence.
  - stub.rs:186-188, internal_modules.rs:188 and internals/mod.rs:261 have safety prose on safe
    code.
  - The SAFETY comment of `JSString::write_utf8` (crates/js string.rs) says that it makes no call
    into SpiderMonkey, but `with_chars` flattens a rope, which allocates. Callers flatten first.
- **Low: `docs/node-compat.md` statements that don't match the code.** Starting `fs.watchFile`
  doesn't throw `ERR_BLOCKING_NOT_ALLOWED` ("Synchronous host I/O"). A served socket's `destroy` destroys the response (lines 629-630), and
  a client disconnect during a streamed body emits `close` (line 645, `cancel_body` in
  outgoing.rs:1597-1618).
- **Low: `AsyncIteratorRecord` is registered twice.** Both web-streams (lib.rs:46) and
  operators.rs:2279 register it. Unverified whether the second registration replaces the
  prototype.

### Correctness

- **Medium: exceptions left pending after a swallowed error.** `from_jsval(..).unwrap_or(..)`,
  `.ok()` and `unwrap_or_else` return success with an exception pending at process.rs:202-203,
  internal_modules.rs:95 and 106, assert.rs:1549-1554 (`CallTracker.calls` with a throwing `name`
  getter), destroy.rs:630-633, operators.rs:1039-1044 and watch.rs:621.
  `process.hrtime(time)` doesn't validate its argument. Propagate
  with `from_jsval_throwing(..)?`, or clear the exception explicitly.
- **Medium: `ExnThrown` without a pending exception.** `property` and `set` in parse_args.rs:50
  and 63 convert option names with `CString::new(name).map_err(|_| ExnThrown)`. With the prebuilt
  binary, `util.parseArgs` with an option name that contains a NUL character ends the script with
  `Uncaught undefined`, where Node.js returns the parsed values. `emit_in_tick` (watch.rs:771) does
  the same when `watcher.emit` isn't an object. Look the names up by id, and throw a `TypeError`.
- **Medium: the immediate-id sets grow without bound.** Every `setImmediate` that runs adds its id
  to `RUN_IMMEDIATE_IDS` (timers.rs:43-47, 560-575), and only `process.getActiveResourcesInfo()`
  prunes it. `clearImmediate` doesn't remove the id from `IMMEDIATE_IDS`. Both are instance-wide,
  so a reused instance accumulates one entry per immediate. Keep the state in the `Immediate`'s
  Rust state.
- **Medium: `process.exit(undefined)` keeps an earlier `process.exitCode`.** process.rs:675-696
  redefines the `exitCode` accessor with two new functions but leaves `ProcessState.exit_code`
  unchanged. With the prebuilt binary, `process.exit(undefined)` after `process.exitCode = 3` exits with
  status 3, where Node.js exits with 0. Clear the state as the setter does.
- **Medium: `process.stdout.write` ignores `encoding` and calls back synchronously.**
  process.rs:1369-1406 writes a string's UTF-8 bytes whatever the encoding and calls the callback
  before returning. With the prebuilt binary, `write('68690a', 'hex')` prints `68690a`. Use
  `support::chunk_bytes` and `support::next_tick_call`.
- **Medium: `console.assert` formats differently and fails when detached.** console.rs:206-228
  stringifies a non-string first argument (`Assertion failed: [object Object]` where Node.js prints
  `Assertion failed { a: 1 }`), and a destructured `assert(false)` from `console` throws "Value isn't
  an object". Port Node's `typeof args[0] === 'string'` branch and use `console_object`.
- **Medium: the Linux `os.constants.errno` table lacks 22 names.** The table that wasm and Linux
  use (os.rs:203-261) has 57 of Node's 79 names, among them `EPROTO`, `ENOTSOCK` and `EMSGSIZE`,
  and is sorted by number where Node's keys are alphabetical. `util.getSystemErrorName(-71)`
  (util.rs:1228) returns an unknown-error string. The signals lack `SIGSTKFLT`. The Linux output
  wasn't run.
- **Medium: an aborted `stream/promises` pipeline settles before its streams close.** `abort`
  calls `finish_impl(.., true)` (pipeline.rs:189-203), so the promise rejects in the next tick
  while streams with an asynchronous `_destroy` are still closing. Node.js waits for their
  `finished` callbacks. The same applies to the already-aborted signal at pipeline.rs:912-914.
  Pass `false`, as Node.js does.
- **Medium: the readable async iterator's `return()` and `throw()` don't wait for a pending
  `next()`.** readable.rs:1870-1889 and 1933-1949 destroy the stream one microtask later while an
  earlier `next()` waits, which then settles as `{ done: true }` and loses the data. With the
  prebuilt binary the order of the two results is reversed compared with Node.js. Chain them on
  `state.pending`, as `iterator_next` does.
- **Medium: the stream predicates misreport web streams.** `isDisturbed`, `isErrored`, `isReadable`
  and `isWritable` (stream/mod.rs:342-373, util.rs:351-392) don't check the web streams. With the
  prebuilt binary, `isDisturbed` is `false` for a read or consumed `Response.body`, `isErrored` is
  `false` for an errored stream, and `isReadable` is `null`. Read the web streams' Rust state.
- **Medium: served responses emit `finish` after `destroy()`.** `write` and `end` with a chunk
  report `ERR_STREAM_DESTROYED` after `destroy`, as in Node.js, but `end` then still emits
  `finish` and calls its callback. Node.js emits only `close`.
- **Medium: `net.Server` stops accepting after most accept errors.** net/server.rs:86-101 accepts
  again only after `ECONNABORTED`, `ECONNRESET` and `ENOTCONN`. After `EMFILE` the server stays
  `listening` but never accepts again. Node.js keeps accepting. Keep accepting while listening,
  with a backoff for `EMFILE` and `ENFILE`. Not reproduced.
- **Medium: the resolver's cache is never cleared.** `oxc_resolver` caches file and `package.json`
  lookups for the runtime's lifetime (module.rs:714-738, 1116-1138). With the prebuilt binary, a
  `require` that failed keeps failing after the file is created, where Node.js finds it. The cache
  also grows with every probed path on a reused instance. Clear the cache and retry on a failed
  resolution. The name of the clearing method wasn't checked.
- **Medium: `require()` of an ES module with top-level `await` runs it before throwing.**
  module.rs:513-526 evaluates the module and throws a `TypeError` without a `code` when the
  evaluation promise is pending. Node.js throws `ERR_REQUIRE_ASYNC_MODULE` before evaluating. Code
  that falls back to `import()` runs the module's side effects twice. Detect top-level `await`
  after linking, which may need mozjs glue.
- **Medium: `require()` of an ES module that is still loading evaluates it early.** When `a.mjs`
  imports `b.cjs` and `b.cjs` requires `a.mjs`, `require_module` (module.rs:486-535) loads and
  evaluates `a.mjs` inside the outer load, and registers `b.cjs`'s facade with its partial exports
  (module.rs:391-444). With the prebuilt binary, `a.mjs` sees `{}` as `b`'s default. Node.js
  throws `ERR_REQUIRE_CYCLE_MODULE`. Track the graphs whose load is in progress and throw that
  error.
- **Medium: the equality assertions don't throw `ERR_MISSING_ARGS`.** assert.rs:484-537 treats a
  missing argument as `undefined`, so `assert.deepEqual(undefined)` passes and
  `assert.strictEqual(1)` throws an `AssertionError`. Check the argument count.
- **Medium: readline's async iterator has no backpressure and leaks listeners.**
  readline.rs:436-466 creates a new iterator with three listeners on every call and queues lines
  without limit (readline.rs:312-334). Node.js returns one cached iterator, pauses the input at
  1024 queued lines, and removes its listeners on `return()`. A slow `for await` over a large file
  buffers the whole file. Cache the iterator in `InterfaceState`, pause at the high water mark, and
  remove the listeners.
- **Medium: `URL`, `URLSearchParams` and `Headers` are inspected as `{}`.** Their state lives in
  Rust and prototype accessors, and their prototypes have no `util.inspect.custom`, so
  `console.log(new URL(..))` prints `URL {}`. Define `util.inspect.custom` on them at global setup
  in node-compat mode.
- **Low: `events` deviations.** `eventNames()` returns integer-like names as numbers and
  `removeAllListeners()` emits `removeListener` with a number (events.rs:976-980, 870-876).
  `newListener` gets a `null` `listener.listener` instead of the listener (events.rs:313-324).
  An `Object.create(Error.prototype)` error is wrapped in `ERR_UNHANDLED_ERROR`, and an empty
  `_events.error` array throws (events.rs:560-585). Checked against the oracle.
- **Low: `clearTimeout(number)` cancels timeouts never converted to a number.** timers.rs:716-720
  cancels any timer by id. Node.js only resolves ids that `Symbol.toPrimitive` registered, and sets
  `_idleTimeout` to -1.
- **Low: CommonJS surface gaps.** `require.extensions`, `require.resolve.paths` and
  `process.mainModule` are missing (commonjs.rs:446-456). The main module's `parent` is
  `undefined`, not `null`. JSON parse errors lack the filename prefix (commonjs.rs:304-307). The
  `MODULE_NOT_FOUND` message repeats the specifier three times and has no `requireStack`
  (commonjs.rs:52-58).
- **Low: the `AbortError` constructor takes `options` first.** errors.rs:597-610 ignores
  `new AbortError('m', { cause })`'s message and cause. Node's signature is `(message, options)`.
- **Low: `--abort-on-uncaught-exception` splits frames at the last `@`.** process.rs:459 breaks
  frames under `node_modules/@scope`. Use `Frame::parse`. Unverified by a run.
- **Low: `crypto` argument handling.** `digest()` ignores a non-string encoding instead of
  stringifying it (crypto.rs:163), `hash.copy(options)` drops `options` (crypto.rs:315-323), and
  `randomUUID(1)` doesn't throw (crypto.rs:1522-1525).
- **Low: `util.inherits` is stricter than Node's.** util.rs:945-960 rejects a non-function `ctor`
  and a `null` `superCtor.prototype`, which Node.js accepts.
- **Low: `path.format` uses non-empty strings instead of truthiness.** path.rs:452-480 gives
  `"0/x"` for `{ dir: 0, base: 'x' }`, where Node.js gives `"x"`, and reads the fields in another
  order.
- **Low: `parseArgs` splits `--=foo` at the `=`.** parse_args.rs:316-324. Node.js treats it as the
  long option `=foo`.
- **Low: `Buffer.from(object)` reads `data` before `type`, and `type` twice.** buffer.rs:1245-1248.
- **Low: `fs` details.** `fs.write(fd, string, cb)` passes a UTF-8 round trip of the string to the
  callback, which replaces lone surrogates (ops.rs:1317-1323). `Buffer` paths and `encoding:
  'buffer'` directory entries are converted lossily to UTF-8 (fs/mod.rs:458-459, ops.rs:1430,
  internals/fs.rs:145). `POLL_TIMERS` (watch.rs:47-50) keeps entries of timers whose event loop
  ended, and a stale entry can hide a later timer with the same id from
  `getActiveResourcesInfo()`.
- **Low: `FileHandle` gaps.** Garbage collection of an open `FileHandle` closes its descriptor
  but emits no DEP0137 warning, because a finalizer can't run JavaScript and core-runtime has no
  GC-safe way to queue work. Streams from a handle use its descriptor directly, where Node.js goes
  through `handle.read` and `handle.write` and refuses an `fs` option. Operations other than
  streams don't take a reference on the handle while they run.
- **Low: `promisify` details.** `internal/util`'s `customPromisifyArgs` is a stub symbol, not
  `util::CUSTOM_PROMISIFY_ARGS`. `readv`, `writev`, `stream.pipeline` and
  `stream.finished` define no `customPromisifyArgs` or `promisify.custom`. Native functions have no
  `prototype` property, which Node's JavaScript functions have.
- **Low: wasm blocking checks run before the work.** On wasm, `refuse_blocking` runs when an
  operation starts (fs/mod.rs:1028-1035), but `platform::fs::offload` runs the work when the future
  is first polled, which can be after serving began. `fs.cp` with a `filter` checks the policy once
  and blocks in each later step (cp.rs:779-813). `fs.watchFile` doesn't check at start at all
  (watch.rs:1504-1535).
- **Low: served-response edge cases.** A served response with an `Expect` header throws
  `undefined is not a function`, because `store_header` flushes through the `HostSocket`, which has
  no `write` (wire.rs:369-371, 500-506). The served `finish` handling calls `_dump` without Node's
  `resumeScheduled` check and emits `close` synchronously (outgoing.rs:506-529).
- **Low: `stream` details.** `endReadableNT` reads `allowHalfOpen` before `writable`
  (readable.rs:869-874). `prepend_listener` appends for emitters without `prependListener`
  (readable.rs:1213-1224). `readable[Symbol.asyncIterator]()` returns a plain object with own
  methods, not an async generator (readable.rs:1608-1618), which `docs/node-compat.md` doesn't
  list.
- **Low: zlib details.** A zlib stream doesn't `push(null)` when data follows the end of the
  compressed stream, which Node.js does. `complete()` in zlib.rs builds error values by throwing
  and taking the exception.
- **Low: `support::try_filled_vec` is in the node crate.** It allocates a `Vec` fallibly, which no
  shared crate offers. It belongs in `crates/js` once another builtin needs it.
- **Low: `util.inspect` details.** Custom inspect functions don't get the caller's extra options
  or `stylize`, and non-enumerable options are read (inspect.rs:106-130, 200-231). Buffers ignore
  the enclosing call's options (inspect.rs:2749-2752). Inspecting a prototype chain without
  constructors takes quadratic time, as in Node.js.
- **Low: `assert.match`, `doesNotMatch`, `throws` and `rejects` call the RegExp's own `exec`.**
  assert.rs:70-77. Node.js calls the original `RegExp.prototype.exec`.
- **Low: `queue_js_timer_with_id` doesn't check its precondition.** event_loop.rs:727-750
  overwrites a live timer's entry, which orphans its task. The one caller cancels first.
- **Low: `import()` blocks while serving where `require()` is refused.** `resolve_file_module`
  (module.rs:914-915) reads uncached files without `blocking_host_io_refusal`.
- **Low: `import.meta.resolve` and `builtin_namespace` ignore the `node_compat` flag.**
  module.rs:833 and 468 consult the builtin modules unconditionally, unlike the load hook
  (module.rs:598-603). This affects embedders that create a runtime without `--node-compat` after
  registering the builtins.
- **Low: a `SharedArrayBuffer` blob part is stringified.** web-files blob.rs:28-35 gives
  `new Blob([new SharedArrayBuffer(3)]).size` 26. Node.js gives 3.
- **Low: macro and job-queue details in the shared crates.**
  - `class_syntax` constructors get a writable `prototype` (starling-macro lib.rs:5203).
  - `#[jsclass]` accessors became non-enumerable with this branch (class.rs:1752-1764,
    starling-macro lib.rs:1450). This also changed `ComponentError` in `component-model`, and
    `ClassConfig::method_flags` doesn't document it.
  - Jobs held during debugger hooks lose their async context, because their tags stay at the old
    queue positions (jobs.rs:312-358).
  - `stop_draining` has no callers, and `run_jobs` runs ticks before checking `INTERRUPTED`
    (jobs.rs:269-276, 370-373).
  - `run_next_tick` drops the popped `Tick` while the `data_mut()` guard is live (jobs.rs:207). A
    GC during the drop reads the private data through the trace hook under the guard.

### Efficiency

- **Medium: `emit` copies its listeners on every emit.** events.rs:538-572 compares the event name
  with `"error"` through a string conversion and builds a `Vec` of the listeners, with a generic
  element get per listener for arrays (events.rs:290-304). This is the hottest path of the crate.
  Node.js iterates the live array and copies on write in `removeListener`. Call a single listener
  directly, compare the name with the `error` atom first, and copy only arrays.
- **Medium: `listenerCount(name)` copies the listeners on a per-request path.**
  events.rs:955-969 materializes the listener array and reads `listener` from every listener. The
  HTTP server calls it several times per request (http/connection.rs, http/server.rs:129,
  http/incoming.rs:842). Without a `listener` argument, return 1 or the array length. With one,
  use Node's comparison, which also matches a raw `once` wrapper and treats `null` as absent.
- **Low: own properties of new objects are set one by one.** readline's `Interface`
  (readline.rs:269-281) and assertion errors (assert.rs:1172-1181) set their properties with
  `set_property`, which also calls setters on the prototypes. Add `PropertyTemplate`s in Node's
  key order. readline's `history`, `historyIndex`, `historySize` and `isFlushing` are data
  properties where Node has accessors, and `historyManager` is missing.
- **Low: each relative `path.resolve` calls `getcwd`.** Node.js caches the working directory
  until `process.chdir`, and the cache needs `process.rs` to clear it.
- **Low: fixed event names created per call.** `on` (events.rs:370-371) and `removeListener`
  (events.rs:524, 878-879) create a new `JSString` for `newListener` or `removeListener`, as does
  async_hooks.rs:481. Use `js::class::get_or_init_atom`.
- **Low: functions created per call for stateless callbacks.** A `toString` per `SystemError`
  (errors.rs:30), the `enable` and `disable` of `createHook` (async_hooks.rs:691-694) and
  `AsyncLocalStorage.snapshot` (async_hooks.rs:247). Use `js::class::get_or_init_shared_function`.
- **Low: the first `req.headers` read groups the header lines by a linear search.** It is
  quadratic in the number of lines, which `maxHeadersCount` bounds.
- **Low: per-chunk lookups in the readable byte path.** With `setEncoding()`, every pushed chunk
  calls the decoder's `write` by name, as Node.js's `state.decoder.write(chunk)` does, since code
  can reach the decoder through `_readableState`. `chunk_length` checks each typed array chunk
  for an own `length`, which Node.js reads by name.
- **Low: listing the keys of an array or typed array that aren't indices is linear in its
  length.** `Object::own_non_index_keys` gets every own property id from `GetPropertyKeys`, which
  has no flag to leave out elements, and skips the index ids. `util.inspect` of a 5,000,000-element
  `Uint8Array` takes about 120 ms in a debug build. Node.js's `getOwnNonIndexProperties` doesn't
  depend on the length.
- **Low: `new_error` deletes the `cause` that `CreateError` adds.** The mozjs glue's
  `CreateError` passes the cause as `mozilla::ToMaybe(&cause)`, which is never `Nothing`, so
  every error it creates gets a `cause`. A glue variant without a cause avoids the lookup and the
  delete.

### Tooling

- **Medium: CI runs none of the Node.js compatibility suites.** `just node-test`,
  `node-test-wasm`, `node-fuzz` and `express-test` run them locally, but no CI job does, so
  `expectations.json` and `known-diffs.json` are enforced only by hand. The harness expectations
  were recorded on macOS, and some results depend on the platform (`os` constants, error codes), so
  a CI job needs either a macOS runner or separately recorded Linux results.
