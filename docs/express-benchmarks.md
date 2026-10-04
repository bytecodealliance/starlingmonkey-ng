# Express and Koa app benchmarks

Throughput, latency, startup and memory of the apps in `tests/express-apps` under StarlingMonkey's
`--node-compat` mode, natively and as components, and under Node.js, Deno and Bun. The numbers
are from runs of `tests/express-apps/bench.mjs` on 2026-10-05, with the fixes in
[Fixes](#fixes) applied.

## Method

`bench.mjs` starts each app under each configuration, measures its startup, and loads a few of
its routes with [`oha`](https://github.com/hatoo/oha). `tests/express-apps/README.md` describes
its options.

- Each route runs for 1 s at 32 connections to warm up, then for 5 s at 1 connection and for 5 s
  at 32 connections. Connections are kept alive. The requests to the apps that compress
  responses (`koa-portal`, `rest-api` and `ssr-site`) send `Accept-Encoding: identity`, except
  for the routes marked gzip, which send `Accept-Encoding: gzip`.
- Every configuration runs the same entry: an Express app's `spin.mjs`, which mounts it at
  `/<app>`, the Koa portal's `app.mjs`, and `realtime`'s `app.js`. So every configuration does the
  same work in JavaScript, including the outer Express router of the mounted apps.
- Each of the 3 repeats starts every app under every configuration once, in turn. The tables
  give the median of the repeats, with half the range between the smallest and the largest as
  `±`.
- A response with another status than the route's is an error. The runs had 2 errors in all,
  both transport errors under Node.js and Bun, in about 50,000 requests each.
- Startup is measured in 5 separate processes per app and configuration: the time from spawning
  the server until it accepts a TCP connection, and the latency of the first request after
  that. Compiled components are in wasmtime's and Spin's caches, so the startup of components
  doesn't include compiling them.
- RSS is the sum over the server's process tree, after the first request and at the maximum of
  the samples taken every 500 ms under load. Several apps keep state that grows with the requests
  they serve (`morgan` and `pino` log lines, stored todos and feedback, sessions), so the maximum
  grows with the request rate.
- The machine is an Apple M5 MacBook with 10 cores and 32 GiB of memory, on macOS 27.0, with
  other work running: a VS Code helper process took one core throughout, and the load average
  was between 2 and 10.

The routes, with what they exercise:

| App | Route | Work |
|---|---|---|
| `koa-portal` | `GET /` | Handlebars page with partials, `koa-session` cookie, ETag |
| | `GET /api/apps` | JSON |
| | `GET /css/portal.css` | `koa-static`, 1.6 KB, and with gzip through `koa-compress` |
| | `POST /api/feedback` | `koa-body` JSON body, validation |
| `rest-api` | `GET /health` | the app's middleware stack (`helmet`, `cors`, `morgan`, `compression`) and a 2-byte body |
| | `GET /api/v1/big` | 12 KB of JSON, and with gzip through `compression` |
| | `POST /api/v1/echo` | `express.json` body |
| | `POST /api/v1/todos` | `express.json` body, zod validation, and a 1 ms `setTimeout` in the store |
| `ssr-site` | `GET /static/css/site.css` | `express.static`, 282 bytes |
| | `GET /files/blob.bin` | `express.static`, 70 KB |
| | `GET /` | EJS page with includes, `express-session` creating a session per request |
| `files` | `GET /stream/numbers?n=1000` | a `Readable` piped to the response, 1000 chunks |
| | `POST /echo-gzip` | a 4 KB request body read as a stream |
| `auth` | `POST /sign` | `jsonwebtoken` HS256 sign and verify |
| | `GET /me` | `jsonwebtoken` verify of a bearer token |
| `data` | `GET /config` | JSON |
| | `GET /posts` | `lowdb` reading its JSON file |
| | `GET /export.ndjson` | an async generator piped to the response with 3 1 ms sleeps |
| `realtime` | `GET /rooms/lobby/messages` | JSON |

## Configurations

| Config | Runs |
|---|---|
| `listen` | `target/release/starlingmonkey --node-compat --node-http-listen`: the app's `http.Server` accepts connections itself |
| `serve` | `target/release/starlingmonkey --node-compat --serve <port>`, with `--allow-blocking-host-io` for `ssr-site`: the runtime's HTTP server dispatches requests to the app |
| `wasmtime` | the app's component from `scripts/build-spin-app.sh` under `wasmtime serve` with `-Scli=y,p3=y -Wcomponent-model-async=y`, and the mounts and environment `spin.toml` gives the component |
| `spin` | the Spin application from `scripts/build-spin-app.sh` under `spin up --allow-transient-write`, with all seven components |
| `node`, `deno`, `bun` | Node.js v24.21.0, Deno 2.9.7 with `--unstable-detect-cjs`, Bun 1.4.2 |

Both `wasmtime` and `spin` use their default instance reuse for WASIp3 components: an instance
serves up to 128 requests, up to 16 at once, and new instances start for more concurrent
requests. Each instance starts from the component's snapshot, taken after the app's top level
ran. [Instance reuse](#instance-reuse) has numbers for one request per instance. `realtime` uses
WebSockets on its HTTP server, so it runs under `listen`, Node.js, Deno and Bun only.

## Summary

Requests per second at 1 and at 32 connections, and the p99 latency in ms at 32 connections, for
some of the routes:

| Route | listen | serve | wasmtime | spin | node | deno | bun |
|---|--:|--:|--:|--:|--:|--:|--:|
| `koa-portal` `GET /` | 6259 / 9112 / 6.1 | 5532 / 8030 / 6.7 | 934 / 5080 / 15.2 | 799 / 3997 / 18.9 | 9294 / 14399 / 3.7 | 7661 / 12382 / 4.3 | 10001 / 16683 / 4.1 |
| `koa-portal` `GET /css/portal.css` gzip | 2735 / 4987 / 12.8 | 2172 / 4186 / 17.2 | 1575 / 7952 / 14.2 | 1176 / 5463 / 19.3 | 5911 / 11969 / 5.9 | 5957 / 10710 / 5.8 | 6035 / 17075 / 4.8 |
| `rest-api` `GET /health` | 11584 / 16068 / 5.5 | 8975 / 15082 / 7.6 | 3021 / 17311 / 8.0 | 2640 / 12531 / 9.9 | 18002 / 40076 / 1.6 | 17081 / 37930 / 2.0 | 19354 / 61796 / 1.6 |
| `rest-api` `GET /api/v1/big` gzip | 4798 / 6097 / 11.2 | 4197 / 5652 / 10.5 | 1538 / 8785 / 10.2 | 1395 / 6286 / 14.0 | 5104 / 10675 / 6.1 | 5076 / 6904 / 8.4 | 5960 / 17723 / 5.4 |
| `rest-api` `POST /api/v1/echo` | 9223 / 12121 / 7.7 | 6717 / 9793 / 8.3 | 2049 / 10497 / 15.1 | 1835 / 7978 / 17.0 | 14812 / 31058 / 2.0 | 13760 / 29235 / 2.5 | 15507 / 40241 / 3.3 |
| `ssr-site` `GET /files/blob.bin` | 4560 / 7574 / 8.7 | 3451 / 6309 / 12.0 | 2421 / 10389 / 13.2 | 1586 / 5570 / 19.1 | 8800 / 15517 / 4.3 | 7757 / 14206 / 4.1 | 7842 / 16734 / 4.0 |
| `ssr-site` `GET /` | 9170 / 12932 / 7.2 | 7871 / 12119 / 7.4 | 2415 / 10760 / 13.1 | 1899 / 7468 / 26.2 | 13275 / 22094 / 2.6 | 12488 / 19028 / 3.7 | 15251 / 30993 / 2.5 |
| `files` `GET /stream/numbers` | 70 / 426 / 89 | 237 / 282 / 123 | 184 / 597 / 177 | 153 / 470 / 218 | 80 / 1335 / 27 | 80 / 1325 / 28 | 82 / 1396 / 27 |
| `auth` `POST /sign` | 8181 / 11798 / 5.9 | 6991 / 9478 / 6.7 | 2368 / 11049 / 15.3 | 2156 / 9017 / 16.3 | 11851 / 18674 / 3.4 | 11058 / 19444 / 2.9 | 16391 / 37278 / 1.8 |
| `data` `GET /posts` | 10113 / 17871 / 5.0 | 8180 / 18443 / 7.0 | 4603 / 20417 / 7.6 | 2791 / 14970 / 7.5 | 10602 / 30212 / 1.7 | 10687 / 32208 / 2.2 | 13862 / 53790 / 2.1 |

- Natively, StarlingMonkey serves 47 to 95% of Node.js's requests per second at 1 connection,
  and 32 to 72% at 32 connections. Its p99 latency at 32 connections is 1.6 to 4 times Node.js's.
- `serve` serves 73 to 89% of `listen`'s requests per second at 1 connection, as many on the
  routes that wait on timers, and more on the streamed route.
- A component serves 14 to 58% of `listen`'s requests per second at 1 connection on the routes
  that don't wait on timers or stream. At 32 connections, wasmtime runs several instances on
  several threads, and serves 56 to 160% of `listen`'s requests per second while using 7 to 9
  cores, where `listen` uses one.
- Spin serves 61 to 100% of wasmtime's requests per second, at higher latency.
- Natively, StarlingMonkey starts as fast as Node.js, and uses less memory after startup. Under
  load, its memory grows more than Node.js's for some apps.

## Startup and memory

The startup times and the RSS after start are the medians of 5 starts, and the maximum RSS under
load the median of the 3 repeats.

| App | Config | Listening ms | First request ms | RSS after start MiB | Max RSS under load MiB |
|---|---|--:|--:|--:|--:|
| koa-portal | listen | 108 ±40 | 2.7 ±0.4 | 54 ±0 | 626 ±1 |
| koa-portal | serve | 103 ±10 | 2.3 ±0.9 | 57 ±0 | 687 ±6 |
| koa-portal | wasmtime | 169 ±3 | 7.5 ±0.6 | 244 ±0 | 1633 ±242 |
| koa-portal | spin | 941 ±13 | 8.3 ±1.1 | 1336 ±0 | 2680 ±250 |
| koa-portal | node | 115 ±5 | 5.7 ±0.4 | 97 ±0 | 397 ±6 |
| koa-portal | deno | 99 ±76 | 6.3 ±6.4 | 76 ±0 | 439 ±9 |
| koa-portal | bun | 81 ±6 | 5.7 ±1.3 | 72 ±0 | 252 ±12 |
| rest-api | listen | 78 ±3 | 1.0 ±0.1 | 42 ±0 | 451 ±7 |
| rest-api | serve | 82 ±7 | 1.1 ±0.1 | 45 ±0 | 460 ±2 |
| rest-api | wasmtime | 153 ±4 | 4.3 ±0.3 | 216 ±0 | 1178 ±16 |
| rest-api | spin | 948 ±10 | 5.3 ±0.2 | 1356 ±0 | 2283 ±106 |
| rest-api | node | 88 ±26 | 4.0 ±1.4 | 76 ±0 | 524 ±11 |
| rest-api | deno | 82 ±1 | 3.6 ±0.1 | 60 ±0 | 705 ±3 |
| rest-api | bun | 68 ±1 | 3.3 ±1.1 | 49 ±0 | 838 ±35 |
| ssr-site | listen | 100 ±8 | 1.0 ±0.1 | 54 ±0 | 383 ±2 |
| ssr-site | serve | 103 ±4 | 1.3 ±0.1 | 58 ±0 | 389 ±4 |
| ssr-site | wasmtime | 170 ±4 | 4.9 ±0.1 | 242 ±0 | 1412 ±177 |
| ssr-site | spin | 947 ±9 | 6.6 ±0.2 | 1365 ±0 | 2272 ±203 |
| ssr-site | node | 106 ±1 | 3.7 ±0.1 | 82 ±0 | 378 ±8 |
| ssr-site | deno | 102 ±1 | 3.9 ±0.8 | 69 ±0 | 425 ±24 |
| ssr-site | bun | 87 ±2 | 4.2 ±1.8 | 64 ±1 | 315 ±4 |
| files | listen | 51 ±4 | 3.3 ±0.2 | 35 ±0 | 353 ±1 |
| files | serve | 49 ±3 | 5.5 ±0.2 | 37 ±0 | 291 ±4 |
| files | wasmtime | 139 ±6 | 8.7 ±0.1 | 195 ±0 | 1090 ±306 |
| files | spin | 940 ±7 | 11.1 ±0.1 | 1351 ±0 | 1986 ±381 |
| files | node | 63 ±3 | 5.3 ±0.1 | 67 ±0 | 226 ±5 |
| files | deno | 65 ±1 | 5.2 ±0.1 | 54 ±1 | 190 ±16 |
| files | bun | 51 ±4 | 4.1 ±0.2 | 43 ±0 | 130 ±2 |
| auth | listen | 76 ±4 | 1.2 ±0.1 | 43 ±0 | 219 ±1 |
| auth | serve | 77 ±4 | 1.2 ±0.1 | 45 ±0 | 265 ±8 |
| auth | wasmtime | 152 ±6 | 4.3 ±0.1 | 191 ±0 | 961 ±27 |
| auth | spin | 940 ±5 | 5.5 ±0.7 | 1348 ±0 | 2096 ±66 |
| auth | node | 88 ±3 | 4.5 ±0.4 | 76 ±0 | 123 ±7 |
| auth | deno | 89 ±5 | 4.2 ±0.3 | 62 ±0 | 233 ±20 |
| auth | bun | 70 ±1 | 3.3 ±0.4 | 49 ±0 | 136 ±4 |
| data | listen | 69 ±6 | 0.8 ±0.1 | 37 ±0 | 272 ±1 |
| data | serve | 71 ±4 | 1.6 ±0.6 | 39 ±0 | 326 ±2 |
| data | wasmtime | 143 ±4 | 3.8 ±0.3 | 203 ±0 | 1024 ±82 |
| data | spin | 942 ±9 | 4.5 ±0.6 | 1352 ±0 | 1999 ±105 |
| data | node | 92 ±3 | 3.6 ±0.7 | 80 ±0 | 557 ±3 |
| data | deno | 83 ±3 | 3.3 ±0.1 | 56 ±0 | 479 ±4 |
| data | bun | 65 ±2 | 3.1 ±0.9 | 53 ±0 | 469 ±23 |
| realtime | listen | 52 ±0 | 0.8 ±0.1 | 33 ±0 | 218 ±0 |
| realtime | node | 63 ±6 | 3.4 ±0.3 | 66 ±0 | 240 ±7 |
| realtime | deno | 64 ±1 | 3.2 ±0.0 | 50 ±1 | 161 ±0 |
| realtime | bun | 47 ±1 | 3.3 ±0.4 | 39 ±0 | 103 ±4 |

- StarlingMonkey listens 50 to 110 ms after it is spawned natively, as Node.js does, and a
  component under `wasmtime serve` 140 to 170 ms after. The first request of a component
  instantiates it, which adds 3 to 6 ms to the first request.
- Spin listens about 940 ms after it is spawned, for the whole application with its seven
  components, and its process holds 1.3 GiB after the first request.
- After startup, StarlingMonkey holds 33 to 58 MiB natively, against 66 to 97 MiB for Node.js.
  A `wasmtime serve` process holds 190 to 245 MiB.
- Under load, StarlingMonkey's RSS reaches 220 to 690 MiB natively, against 120 to 560 MiB for
  Node.js. Components reach 0.9 to 1.6 GiB under wasmtime and 1.9 to 2.6 GiB under Spin, with several
  instances alive at 32 connections, each with its own linear memory.

## Fixes

Profiles of `listen` under load (an unstripped release build sampled with macOS's `sample`)
showed where the native runtime spends its time beyond the apps' JavaScript. These changes are
on the branch:

| Change | Effect |
|---|---|
| By-name property access through interned keys: `Object::get_property`, `set_property`, `has_property`, `has_own_property` and `Function::call_by_name` take `&'static CStr` names and use the global's interned property key instead of atomizing the name on each call | `js::Atomize` was the hottest SpiderMonkey function, at 7% of the CPU time of `GET /health`. CPU per request of `GET /health` at 32 connections: 70.0 to 62.0 µs |
| zlib streams free their codec when they are destroyed, with Node.js's `_destroy` | the codecs stayed alive until the garbage collector finalized their handles, and the collector doesn't see their memory. RSS after 12 s of `GET /api/v1/big` with gzip at 8 connections: 2,069 to 288 MiB |
| `net.Socket` has `_writev`, so the writes a response buffers while its socket is corked go out as one write | `GET /stream/numbers`: 296 to 405 requests per second at 8 connections, 3,520 to 2,492 µs CPU per request |
| A readable stream reads a `Buffer` chunk's length from the typed array | 2,559 to 2,457 µs CPU per request of `GET /stream/numbers` |
| The Koa portal compiles its Handlebars partials and renders each view once at startup, since Handlebars compiles a template when it first renders | first request after startup: 7.9 to 2.3 ms natively, 23.7 to 7.2 ms in a new component instance. `GET /` with one request per instance: 41 to 195 requests per second |

Requests per second at 1 and at 32 connections before and after the runtime fixes, from a run of
the previous build with 2 repeats and the main run, both before the Koa portal's change:

| Route | listen before | listen after | serve before | serve after |
|---|--:|--:|--:|--:|
| `koa-portal` `GET /css/portal.css` gzip | 2481 / 4408 | 2713 / 5006 | 2063 / 3855 | 2173 / 4184 |
| `koa-portal` `POST /api/feedback` | 8822 / 13884 | 9368 / 15491 | 6962 / 10500 | 7191 / 11220 |
| `rest-api` `GET /health` | 11011 / 14412 | 11584 / 16068 | 8683 / 14343 | 8975 / 15082 |
| `rest-api` `GET /api/v1/big` gzip | 4557 / 5478 | 4798 / 6097 | 3629 / 5173 | 4197 / 5652 |
| `ssr-site` `GET /static/css/site.css` | 6138 / 10287 | 6602 / 11563 | 4727 / 8420 | 4966 / 9584 |
| `files` `GET /stream/numbers` | 66 / 223 | 70 / 426 | 207 / 240 | 237 / 282 |
| `auth` `POST /sign` | 7712 / 10708 | 8181 / 11798 | 6727 / 8933 | 6991 / 9478 |
| `data` `GET /config` | 13208 / 18023 | 13789 / 20228 | 10360 / 19946 | 10694 / 22193 |

The maximum RSS under load fell from 2,279 to 451 MiB for `rest-api` and from 962 to 631 MiB for
`koa-portal` natively, and from 1,926 to 1,178 MiB and 2,511 to 1,662 MiB under wasmtime. Both
compress responses.

## Analysis

### Native runtime against Node.js

For `GET /health` at 32 connections, `listen` spends about 62 µs of CPU per request, and Node.js
about 22 µs (76 and 32 µs at 1 connection). A profile of that run, by the function the samples are in:

| Share | Where |
|---|---|
| 41% | SpiderMonkey's runtime: generic property gets and sets (`NativeGetProperty` 7.7%, `NativeSetProperty` 4.8%, property map lookups 2.8%), nursery collections (about 8% including tracing), calls between Rust and JavaScript |
| 13% | the Node.js modules in Rust: streams, events, `http` |
| 12% | `malloc` and `free` |
| 11% | `sendto` and `recvfrom` |
| 8% | JIT-compiled JavaScript: Express, its middleware and the app |
| 15% | other |

The Node.js modules are Rust, and keep the state of streams, sockets, emitters and messages in JS
properties, as Node.js does, so that JavaScript sees the same objects. Each of the many accesses
of that state from Rust goes through `JS_GetPropertyById` or `JS_SetPropertyById`, which take
SpiderMonkey's generic lookup path without the inline caches the JIT uses. A request through
`net.Socket`, the HTTP parser, `IncomingMessage`, `ServerResponse` and the writable stream
machinery makes many of them. The interned keys removed the atomizing of the names, but not
the lookups themselves. Moving the hot state, such as the readable and writable state's flags and
counters, into Rust structs that JavaScript reads through accessors would remove most of them,
which is a larger change to the stream implementation.

The gap grows with concurrency: Node.js serves 2.2 to 2.5 times its 1-connection rate at 32
connections, and `listen` 1.3 to 1.6 times. Node.js's CPU per request falls from 32 to 22 µs from
1 to 32 connections, and `listen`'s from 76 to 62 µs. These measurements don't establish why.

### `--serve` against `--node-http-listen`

`serve` handles 11 to 27% fewer requests per second than `listen` at 1 connection. The
runtime's server dispatches a fetch event with a `Request`, and the `http` module creates an
`IncomingMessage` and a `ServerResponse` from it, copies the headers, reads the body through the `Request`'s stream, and responds with
`respondWith`. So each request creates both the fetch objects and the `http` objects. `serve`
is faster than `listen` on the streamed route at 1 connection, where `listen`, like Node.js,
takes 12 to 14 ms per request for a response of about 4 KB. These measurements don't establish
why.

### Components

A component serves 14 to 58% of `listen`'s requests per second at 1 connection. The JavaScript
engine runs inside the component without a JIT, since WebAssembly can't generate machine code at
runtime. A loop of object creation, string building and JSON runs at 6,161 iterations per second
in the component under `wasmtime run`, against 28,067 natively and 26,290 under Node.js, so
JavaScript runs about 4.6 times slower. The routes whose time is mostly JavaScript, such as the
Handlebars page (15% of `listen`), lose the most, and the routes that mostly move bytes, such as
the static files (41 to 58%), the least.

At 32 connections, wasmtime runs up to 16 requests in an instance at once, and more instances on
its thread pool: `GET /health` at 32 connections kept the `wasmtime serve` process at 700 to 860%
CPU for 11,700 requests per second, about 650 µs of CPU per request, against 62 µs natively. The
components' throughput at 32 connections comes from the extra cores, and their p99 latency is 0.9
to 2.6 times `listen`'s.

Spin runs the same components, and serves 61 to 100% of wasmtime's requests per second, with up to
twice its p99 latency at 32 connections.

### Instance reuse

With one request per instance (`--max-instance-reuse-count 1`), each request instantiates the
component from its snapshot:

| Route | wasmtime, reuse 1 | wasmtime, default reuse | spin, reuse 1 |
|---|--:|--:|--:|
| `rest-api` `GET /health` | 470 / 1339 | 3021 / 17311 | 205 / 1244 |
| `rest-api` `GET /api/v1/big` | 434 / 1310 | 2043 / 11022 | 329 / 251 |
| `koa-portal` `GET /api/apps` | 281 / 468 | 2141 / 11990 | 240 / 486 |
| `koa-portal` `GET /` | 195 / 377 | 934 / 5080 | 33 / 104 |

(Requests per second at 1 and at 32 connections. `koa-portal` `GET /` with one request per
instance is from after the portal renders its views at startup. Spin's `koa-portal` `GET /` is
from before.)

Starting a new instance adds about 2 ms to its first request.

### Timers

`setTimeout(fn, 1)` takes 2.3 ms natively, against 1.2 ms under Node.js, since the native event
loop waits with `tokio::time::sleep`, which has millisecond resolution. This sets the latency of
`POST /api/v1/todos` (2.6 ms, against 1.5 ms under Node.js) and `GET /export.ndjson` (7.4 ms
against 3.9 ms). Deno takes as long.

### Streams of many chunks

`GET /stream/numbers` pipes 1000 chunks of a few bytes each into the response. StarlingMonkey
spends about 2.5 ms of CPU on it natively, and Node.js about 0.5 ms, so the cost of a chunk
through `Readable`, `pipe` and `ServerResponse.write` is about 2.4 µs against 0.5 µs. In a profile
of this route, generic property gets and sets took more than a third of the CPU time, as described
in [Native runtime against Node.js](#native-runtime-against-nodejs).

### Remaining gaps

| Gap | Cause | Fixable |
|---|---|---|
| Native CPU per request 1.6 to 2.8 times Node.js's on most routes | stream, socket and message state in JS properties accessed from Rust through generic lookups, and object creation per request | yes, by keeping the hot state in Rust structs, a larger change to `node:stream`, `node:net` and `node:http` |
| Less gain from concurrency than Node.js | not established | unknown |
| `serve` 11 to 27% slower than `listen` | both the fetch objects and the `http` objects are created for each request | partly, by creating `IncomingMessage` from the host's request without the fetch objects |
| Components 1.7 to 7 times slower than native at 1 connection | no JIT in WebAssembly | no, apart from making the interpreter and the per-request path cheaper |
| `setTimeout` of 1 ms takes 2.3 ms natively | tokio's millisecond timer resolution | yes, with a timer of finer resolution for short waits |
| Spin's RSS of 1.3 GiB at startup | not established. The process serves all seven components | outside the runtime |

## Results by app

Requests per second, and latency in ms, at 1 and at 32 connections.

### `koa-portal`

#### GET / (Handlebars page)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 6259 ±75 | 0.15 ±0.00 | 0.19 ±0.00 | 9112 ±188 | 3.28 ±0.05 | 6.06 ±0.84 | 0 |
| serve | 5532 ±32 | 0.17 ±0.00 | 0.21 ±0.01 | 8030 ±138 | 3.92 ±0.08 | 6.66 ±0.08 | 0 |
| wasmtime | 934 ±15 | 1.04 ±0.03 | 1.35 ±0.07 | 5080 ±731 | 5.92 ±1.01 | 15.18 ±2.46 | 0 |
| spin | 799 ±6 | 1.22 ±0.02 | 1.75 ±0.13 | 3997 ±556 | 7.62 ±1.26 | 18.93 ±2.46 | 0 |
| node | 9294 ±58 | 0.11 ±0.00 | 0.14 ±0.01 | 14399 ±483 | 2.23 ±0.09 | 3.72 ±0.14 | 0 |
| deno | 7661 ±67 | 0.13 ±0.00 | 0.18 ±0.01 | 12382 ±292 | 2.65 ±0.07 | 4.27 ±0.36 | 0 |
| bun | 10001 ±132 | 0.10 ±0.00 | 0.13 ±0.02 | 16683 ±96 | 1.74 ±0.01 | 4.08 ±0.02 | 0 |

#### GET /api/apps (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 7390 ±144 | 0.13 ±0.00 | 0.16 ±0.00 | 11762 ±141 | 2.48 ±0.03 | 5.64 ±0.10 | 0 |
| serve | 6354 ±67 | 0.15 ±0.00 | 0.18 ±0.00 | 9881 ±25 | 3.12 ±0.02 | 8.31 ±0.06 | 0 |
| wasmtime | 2141 ±32 | 0.44 ±0.00 | 1.08 ±0.05 | 11990 ±1529 | 2.35 ±0.31 | 11.01 ±2.08 | 0 |
| spin | 1775 ±18 | 0.52 ±0.00 | 1.36 ±0.04 | 8621 ±1032 | 3.34 ±0.41 | 13.25 ±1.48 | 0 |
| node | 11651 ±83 | 0.08 ±0.00 | 0.10 ±0.00 | 21648 ±311 | 1.47 ±0.02 | 2.52 ±0.05 | 0 |
| deno | 9242 ±32 | 0.11 ±0.00 | 0.14 ±0.00 | 16773 ±265 | 2.04 ±0.02 | 3.06 ±0.21 | 0 |
| bun | 12582 ±53 | 0.08 ±0.00 | 0.10 ±0.00 | 27969 ±1815 | 1.05 ±0.07 | 2.56 ±0.10 | 0 |

#### GET /css/portal.css (static)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 4529 ±19 | 0.20 ±0.00 | 0.44 ±0.02 | 7187 ±101 | 3.84 ±0.05 | 10.70 ±0.10 | 0 |
| serve | 3808 ±13 | 0.24 ±0.00 | 0.49 ±0.06 | 5758 ±69 | 4.94 ±0.05 | 13.52 ±0.09 | 0 |
| wasmtime | 2248 ±59 | 0.41 ±0.00 | 1.42 ±0.04 | 10204 ±1108 | 2.83 ±0.37 | 12.99 ±1.93 | 0 |
| spin | 1708 ±22 | 0.55 ±0.01 | 1.79 ±0.01 | 6988 ±895 | 3.81 ±0.52 | 16.20 ±2.07 | 0 |
| node | 8292 ±195 | 0.10 ±0.00 | 0.60 ±0.02 | 17590 ±1474 | 1.62 ±0.13 | 4.74 ±0.32 | 0 |
| deno | 8101 ±171 | 0.11 ±0.00 | 0.17 ±0.04 | 16869 ±1329 | 1.85 ±0.15 | 3.52 ±0.58 | 0 |
| bun | 9282 ±136 | 0.10 ±0.00 | 0.17 ±0.01 | 23731 ±2147 | 1.24 ±0.10 | 2.57 ±0.17 | 0 |

#### GET /css/portal.css (static, gzip)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 2735 ±45 | 0.34 ±0.01 | 1.73 ±0.02 | 4987 ±39 | 5.71 ±0.06 | 12.76 ±0.31 | 0 |
| serve | 2172 ±13 | 0.43 ±0.00 | 1.93 ±0.05 | 4186 ±5 | 7.89 ±0.01 | 17.19 ±0.41 | 0 |
| wasmtime | 1575 ±14 | 0.59 ±0.00 | 1.69 ±0.04 | 7952 ±903 | 3.62 ±0.43 | 14.23 ±1.11 | 0 |
| spin | 1176 ±22 | 0.80 ±0.01 | 2.14 ±0.06 | 5463 ±511 | 5.01 ±0.59 | 19.26 ±1.49 | 0 |
| node | 5911 ±278 | 0.14 ±0.00 | 0.90 ±0.18 | 11969 ±1301 | 2.46 ±0.23 | 5.89 ±0.44 | 0 |
| deno | 5957 ±220 | 0.16 ±0.01 | 0.23 ±0.03 | 10710 ±503 | 2.90 ±0.12 | 5.81 ±0.41 | 0 |
| bun | 6035 ±135 | 0.16 ±0.00 | 0.26 ±0.01 | 17075 ±2285 | 1.71 ±0.21 | 4.76 ±0.39 | 0 |

#### POST /api/feedback (JSON, validated)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 9364 ±256 | 0.10 ±0.00 | 0.14 ±0.00 | 15630 ±180 | 1.99 ±0.04 | 5.99 ±0.27 | 0 |
| serve | 7249 ±37 | 0.13 ±0.00 | 0.16 ±0.00 | 11063 ±100 | 2.67 ±0.02 | 8.53 ±0.74 | 0 |
| wasmtime | 2111 ±16 | 0.43 ±0.00 | 1.20 ±0.02 | 11396 ±1369 | 2.43 ±0.33 | 16.76 ±1.63 | 0 |
| spin | 1921 ±13 | 0.47 ±0.00 | 1.36 ±0.00 | 9074 ±1004 | 2.99 ±0.35 | 18.84 ±1.74 | 0 |
| node | 17525 ±128 | 0.06 ±0.00 | 0.07 ±0.00 | 38061 ±1968 | 0.79 ±0.05 | 1.69 ±0.12 | 0 |
| deno | 14851 ±378 | 0.07 ±0.00 | 0.10 ±0.01 | 36424 ±2854 | 0.90 ±0.04 | 1.79 ±0.58 | 0 |
| bun | 16026 ±64 | 0.06 ±0.00 | 0.07 ±0.00 | 37434 ±2377 | 0.78 ±0.05 | 2.13 ±0.12 | 0 |

### `rest-api`

#### GET /health (text)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 11584 ±13 | 0.08 ±0.00 | 0.12 ±0.00 | 16068 ±128 | 1.77 ±0.01 | 5.54 ±0.04 | 0 |
| serve | 8975 ±62 | 0.10 ±0.00 | 0.12 ±0.00 | 15082 ±16 | 1.90 ±0.01 | 7.62 ±0.10 | 0 |
| wasmtime | 3021 ±26 | 0.30 ±0.00 | 0.73 ±0.01 | 17311 ±433 | 1.64 ±0.03 | 7.98 ±0.60 | 0 |
| spin | 2640 ±25 | 0.35 ±0.00 | 0.83 ±0.00 | 12531 ±292 | 2.28 ±0.05 | 9.90 ±0.35 | 0 |
| node | 18002 ±428 | 0.05 ±0.00 | 0.07 ±0.01 | 40076 ±257 | 0.75 ±0.00 | 1.59 ±0.01 | 0 |
| deno | 17081 ±45 | 0.06 ±0.00 | 0.07 ±0.00 | 37930 ±305 | 0.79 ±0.00 | 2.01 ±0.01 | 0 |
| bun | 19354 ±118 | 0.05 ±0.00 | 0.06 ±0.00 | 61796 ±700 | 0.45 ±0.00 | 1.59 ±0.03 | 0 |

#### GET /api/v1/big (JSON, 12 KB)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 8578 ±133 | 0.10 ±0.00 | 0.15 ±0.01 | 10790 ±32 | 2.68 ±0.01 | 7.78 ±0.12 | 0 |
| serve | 6881 ±86 | 0.14 ±0.00 | 0.16 ±0.01 | 10344 ±145 | 2.82 ±0.04 | 8.84 ±0.08 | 0 |
| wasmtime | 2043 ±26 | 0.46 ±0.01 | 0.89 ±0.02 | 11022 ±952 | 2.61 ±0.30 | 9.91 ±1.25 | 0 |
| spin | 1780 ±5 | 0.52 ±0.00 | 1.06 ±0.01 | 8086 ±129 | 3.57 ±0.06 | 12.64 ±0.96 | 0 |
| node | 10885 ±325 | 0.09 ±0.00 | 0.13 ±0.01 | 16184 ±137 | 1.94 ±0.01 | 3.91 ±0.01 | 0 |
| deno | 9017 ±92 | 0.11 ±0.00 | 0.13 ±0.00 | 11741 ±147 | 2.66 ±0.03 | 3.37 ±0.06 | 0 |
| bun | 12815 ±40 | 0.07 ±0.00 | 0.10 ±0.00 | 23957 ±167 | 1.20 ±0.01 | 3.83 ±0.05 | 0 |

#### GET /api/v1/big (JSON, gzip)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 4798 ±240 | 0.20 ±0.00 | 0.25 ±0.16 | 6097 ±639 | 5.07 ±0.27 | 11.18 ±3.34 | 0 |
| serve | 4197 ±16 | 0.22 ±0.00 | 0.26 ±0.01 | 5652 ±80 | 5.28 ±0.07 | 10.45 ±0.21 | 0 |
| wasmtime | 1538 ±28 | 0.61 ±0.01 | 1.29 ±0.14 | 8785 ±387 | 3.34 ±0.15 | 10.19 ±1.22 | 0 |
| spin | 1395 ±35 | 0.67 ±0.02 | 1.28 ±0.03 | 6286 ±239 | 4.75 ±0.25 | 14.00 ±0.57 | 0 |
| node | 5104 ±14 | 0.19 ±0.00 | 0.29 ±0.01 | 10675 ±96 | 2.71 ±0.06 | 6.08 ±0.05 | 0 |
| deno | 5076 ±28 | 0.19 ±0.00 | 0.23 ±0.01 | 6904 ±10 | 4.48 ±0.02 | 8.39 ±0.03 | 0 |
| bun | 5960 ±5 | 0.16 ±0.00 | 0.22 ±0.00 | 17723 ±568 | 1.84 ±0.04 | 5.36 ±0.30 | 0 |

#### POST /api/v1/echo (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 9223 ±516 | 0.10 ±0.00 | 0.12 ±0.08 | 12121 ±256 | 2.41 ±0.10 | 7.66 ±0.09 | 0 |
| serve | 6717 ±56 | 0.14 ±0.00 | 0.17 ±0.01 | 9793 ±174 | 2.98 ±0.04 | 8.33 ±0.11 | 0 |
| wasmtime | 2049 ±32 | 0.45 ±0.00 | 0.92 ±0.06 | 10497 ±300 | 2.68 ±0.05 | 15.08 ±0.80 | 0 |
| spin | 1835 ±9 | 0.49 ±0.00 | 1.02 ±0.04 | 7978 ±62 | 3.50 ±0.07 | 17.04 ±2.10 | 0 |
| node | 14812 ±24 | 0.07 ±0.00 | 0.08 ±0.00 | 31058 ±449 | 0.99 ±0.01 | 2.02 ±0.04 | 0 |
| deno | 13760 ±38 | 0.07 ±0.00 | 0.08 ±0.00 | 29235 ±581 | 0.98 ±0.03 | 2.53 ±0.02 | 0 |
| bun | 15507 ±258 | 0.06 ±0.00 | 0.07 ±0.00 | 40241 ±188 | 0.68 ±0.00 | 3.31 ±0.14 | 0 |

#### POST /api/v1/todos (JSON, zod, 1 ms timer)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 374 ±15 | 2.58 ±0.12 | 3.04 ±0.10 | 12183 ±26 | 2.22 ±0.00 | 7.91 ±0.27 | 0 |
| serve | 361 ±16 | 2.68 ±0.13 | 3.17 ±0.17 | 7175 ±93 | 4.45 ±0.16 | 9.91 ±0.31 | 0 |
| wasmtime | 263 ±14 | 3.67 ±0.16 | 5.34 ±0.53 | 8434 ±208 | 3.38 ±0.09 | 14.77 ±1.76 | 0 |
| spin | 262 ±9 | 3.66 ±0.13 | 5.33 ±0.73 | 7517 ±37 | 3.84 ±0.03 | 17.38 ±0.41 | 0 |
| node | 663 ±32 | 1.49 ±0.07 | 1.75 ±0.07 | 17020 ±1299 | 2.12 ±0.32 | 3.06 ±0.06 | 0 |
| deno | 390 ±16 | 2.52 ±0.12 | 2.82 ±0.14 | 8623 ±215 | 3.86 ±0.12 | 6.39 ±0.16 | 0 |
| bun | 729 ±32 | 1.33 ±0.07 | 1.70 ±0.24 | 26847 ±574 | 1.09 ±0.03 | 3.30 ±0.33 | 0 |

### `ssr-site`

#### GET /static/css/site.css (static)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 6602 ±7 | 0.14 ±0.00 | 0.20 ±0.00 | 11563 ±48 | 2.48 ±0.01 | 4.95 ±0.07 | 0 |
| serve | 4966 ±6 | 0.19 ±0.00 | 0.26 ±0.01 | 9584 ±211 | 3.01 ±0.09 | 6.44 ±0.21 | 0 |
| wasmtime | 2740 ±78 | 0.34 ±0.00 | 0.89 ±0.11 | 11875 ±1215 | 2.39 ±0.25 | 11.88 ±0.34 | 0 |
| spin | 2042 ±448 | 0.46 ±0.00 | 1.15 ±0.94 | 7662 ±3102 | 3.67 ±3.92 | 14.38 ±23.28 | 0 |
| node | 11840 ±1144 | 0.08 ±0.01 | 0.11 ±0.02 | 25159 ±4275 | 1.26 ±0.27 | 1.64 ±0.69 | 1 |
| deno | 9995 ±946 | 0.10 ±0.01 | 0.13 ±0.02 | 21015 ±2950 | 1.46 ±0.28 | 2.77 ±0.45 | 0 |
| bun | 10636 ±1099 | 0.09 ±0.01 | 0.12 ±0.03 | 31538 ±5262 | 0.96 ±0.21 | 1.93 ±0.36 | 0 |

#### GET /files/blob.bin (static, 70 KB)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 4560 ±218 | 0.20 ±0.01 | 0.31 ±0.03 | 7574 ±88 | 3.78 ±0.04 | 8.74 ±0.08 | 0 |
| serve | 3451 ±12 | 0.27 ±0.00 | 0.38 ±0.00 | 6309 ±188 | 4.97 ±0.13 | 11.96 ±0.43 | 0 |
| wasmtime | 2421 ±40 | 0.39 ±0.00 | 0.98 ±0.04 | 10389 ±1087 | 2.70 ±0.29 | 13.20 ±2.20 | 0 |
| spin | 1586 ±113 | 0.59 ±0.02 | 1.36 ±0.46 | 5570 ±1098 | 4.75 ±1.30 | 19.11 ±6.16 | 0 |
| node | 8800 ±1511 | 0.11 ±0.02 | 0.27 ±0.12 | 15517 ±2316 | 1.97 ±0.46 | 4.33 ±0.82 | 0 |
| deno | 7757 ±695 | 0.12 ±0.01 | 0.17 ±0.03 | 14206 ±1915 | 2.17 ±0.40 | 4.06 ±0.60 | 0 |
| bun | 7842 ±771 | 0.12 ±0.01 | 0.17 ±0.09 | 16734 ±3750 | 1.85 ±0.43 | 4.02 ±2.09 | 0 |

#### GET / (EJS page, new session)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 9170 ±19 | 0.10 ±0.00 | 0.15 ±0.00 | 12932 ±72 | 2.18 ±0.01 | 7.17 ±0.05 | 0 |
| serve | 7871 ±63 | 0.12 ±0.00 | 0.15 ±0.01 | 12119 ±77 | 2.34 ±0.02 | 7.38 ±0.51 | 0 |
| wasmtime | 2415 ±165 | 0.37 ±0.00 | 0.98 ±0.55 | 10760 ±331 | 2.43 ±0.37 | 13.05 ±3.88 | 0 |
| spin | 1899 ±105 | 0.49 ±0.01 | 1.30 ±0.12 | 7468 ±1488 | 2.77 ±0.60 | 26.18 ±11.71 | 0 |
| node | 13275 ±849 | 0.07 ±0.01 | 0.09 ±0.01 | 22094 ±3265 | 1.50 ±0.32 | 2.58 ±0.46 | 0 |
| deno | 12488 ±927 | 0.08 ±0.00 | 0.11 ±0.01 | 19028 ±2742 | 1.68 ±0.32 | 3.72 ±0.58 | 0 |
| bun | 15251 ±2815 | 0.06 ±0.01 | 0.11 ±0.13 | 30993 ±2181 | 1.03 ±0.08 | 2.53 ±0.16 | 1 |

### `files`

#### GET /stream/numbers (Readable.pipe, 1000 chunks)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 70 ±0 | 14.13 ±0.05 | 15.16 ±0.09 | 426 ±16 | 74.11 ±2.75 | 89.41 ±2.55 | 0 |
| serve | 237 ±1 | 4.24 ±0.01 | 4.55 ±0.00 | 282 ±6 | 110.22 ±2.43 | 122.80 ±0.71 | 0 |
| wasmtime | 184 ±5 | 5.37 ±0.12 | 6.35 ±0.55 | 597 ±119 | 44.51 ±7.67 | 176.99 ±3.41 | 0 |
| spin | 153 ±3 | 6.43 ±0.12 | 8.01 ±0.43 | 470 ±122 | 52.36 ±12.38 | 218.47 ±32.18 | 0 |
| node | 80 ±0 | 12.47 ±0.04 | 12.90 ±0.06 | 1335 ±17 | 23.65 ±0.15 | 27.41 ±2.00 | 0 |
| deno | 80 ±0 | 12.54 ±0.02 | 13.14 ±0.06 | 1325 ±19 | 23.94 ±0.26 | 27.76 ±1.98 | 0 |
| bun | 82 ±0 | 12.25 ±0.05 | 12.77 ±0.11 | 1396 ±15 | 22.59 ±0.12 | 26.59 ±1.28 | 0 |

#### POST /echo-gzip (request body stream)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 8323 ±35 | 0.11 ±0.00 | 0.16 ±0.00 | 14885 ±85 | 1.94 ±0.01 | 4.55 ±0.07 | 0 |
| serve | 7082 ±24 | 0.13 ±0.00 | 0.17 ±0.00 | 11549 ±193 | 2.61 ±0.02 | 4.95 ±0.23 | 0 |
| wasmtime | 3517 ±35 | 0.27 ±0.00 | 0.52 ±0.01 | 17119 ±2908 | 1.68 ±0.22 | 8.30 ±1.10 | 0 |
| spin | 2376 ±12 | 0.40 ±0.00 | 0.74 ±0.00 | 10875 ±1692 | 2.59 ±0.30 | 8.59 ±0.97 | 0 |
| node | 12609 ±218 | 0.08 ±0.00 | 0.10 ±0.01 | 31977 ±2903 | 0.98 ±0.09 | 1.98 ±0.15 | 0 |
| deno | 11161 ±158 | 0.09 ±0.00 | 0.10 ±0.00 | 25150 ±1454 | 1.24 ±0.08 | 2.47 ±0.33 | 0 |
| bun | 14528 ±67 | 0.07 ±0.00 | 0.08 ±0.01 | 41000 ±5648 | 0.66 ±0.08 | 1.98 ±0.26 | 0 |

### `auth`

#### POST /sign (JWT sign + verify)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 8181 ±3 | 0.11 ±0.00 | 0.16 ±0.00 | 11798 ±22 | 2.38 ±0.00 | 5.88 ±0.07 | 0 |
| serve | 6991 ±13 | 0.13 ±0.00 | 0.17 ±0.00 | 9478 ±143 | 3.09 ±0.04 | 6.74 ±0.31 | 0 |
| wasmtime | 2368 ±10 | 0.37 ±0.00 | 0.90 ±0.03 | 11049 ±1632 | 2.52 ±0.28 | 15.33 ±1.95 | 0 |
| spin | 2156 ±13 | 0.41 ±0.00 | 1.10 ±0.03 | 9017 ±1375 | 3.04 ±0.39 | 16.27 ±2.03 | 0 |
| node | 11851 ±53 | 0.08 ±0.00 | 0.10 ±0.01 | 18674 ±186 | 1.66 ±0.02 | 3.38 ±0.00 | 0 |
| deno | 11058 ±76 | 0.09 ±0.00 | 0.11 ±0.02 | 19444 ±466 | 1.49 ±0.03 | 2.94 ±0.35 | 0 |
| bun | 16391 ±366 | 0.06 ±0.00 | 0.08 ±0.01 | 37278 ±2124 | 0.79 ±0.06 | 1.79 ±0.09 | 0 |

#### GET /me (JWT verify)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 10374 ±10 | 0.09 ±0.00 | 0.13 ±0.00 | 15175 ±206 | 1.90 ±0.02 | 5.13 ±0.13 | 0 |
| serve | 8806 ±69 | 0.11 ±0.00 | 0.13 ±0.00 | 14783 ±239 | 1.95 ±0.03 | 6.68 ±0.20 | 0 |
| wasmtime | 3971 ±15 | 0.24 ±0.00 | 0.70 ±0.00 | 17677 ±2449 | 1.61 ±0.19 | 7.99 ±0.53 | 0 |
| spin | 3125 ±43 | 0.29 ±0.00 | 0.88 ±0.03 | 15008 ±1187 | 1.92 ±0.16 | 8.09 ±0.44 | 0 |
| node | 14768 ±447 | 0.07 ±0.00 | 0.09 ±0.04 | 27808 ±342 | 1.11 ±0.03 | 2.24 ±0.02 | 0 |
| deno | 14380 ±349 | 0.07 ±0.00 | 0.08 ±0.00 | 26817 ±789 | 1.11 ±0.03 | 2.17 ±0.10 | 0 |
| bun | 18412 ±290 | 0.05 ±0.00 | 0.07 ±0.01 | 47736 ±3016 | 0.61 ±0.04 | 1.41 ±0.10 | 0 |

### `data`

#### GET /config (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 13789 ±91 | 0.07 ±0.00 | 0.10 ±0.00 | 20228 ±81 | 1.40 ±0.00 | 4.41 ±0.11 | 0 |
| serve | 10694 ±29 | 0.09 ±0.00 | 0.11 ±0.00 | 22193 ±79 | 1.27 ±0.00 | 5.88 ±0.01 | 0 |
| wasmtime | 5494 ±10 | 0.16 ±0.00 | 0.37 ±0.01 | 24692 ±1925 | 1.14 ±0.08 | 6.85 ±0.59 | 0 |
| spin | 4592 ±46 | 0.20 ±0.00 | 0.48 ±0.01 | 21313 ±1240 | 1.31 ±0.08 | 7.01 ±0.29 | 0 |
| node | 20039 ±37 | 0.05 ±0.00 | 0.06 ±0.00 | 56459 ±721 | 0.52 ±0.01 | 1.12 ±0.01 | 0 |
| deno | 19184 ±112 | 0.05 ±0.00 | 0.06 ±0.00 | 51734 ±929 | 0.57 ±0.00 | 1.86 ±0.07 | 0 |
| bun | 22058 ±29 | 0.04 | 0.06 ±0.00 | 78855 ±528 | 0.35 ±0.00 | 1.35 ±0.03 | 0 |

#### GET /posts (lowdb file read)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 10113 ±405 | 0.09 ±0.00 | 0.14 ±0.00 | 17871 ±411 | 1.58 ±0.04 | 4.99 ±0.12 | 0 |
| serve | 8180 ±81 | 0.12 ±0.00 | 0.14 ±0.00 | 18443 ±27 | 1.57 ±0.00 | 7.02 ±0.35 | 0 |
| wasmtime | 4603 ±16 | 0.20 ±0.00 | 0.56 ±0.01 | 20417 ±768 | 1.35 ±0.06 | 7.59 ±0.29 | 0 |
| spin | 2791 ±14 | 0.34 ±0.00 | 0.84 ±0.01 | 14970 ±509 | 1.88 ±0.04 | 7.54 ±0.38 | 0 |
| node | 10602 ±34 | 0.09 ±0.00 | 0.11 ±0.00 | 30212 ±379 | 1.02 ±0.01 | 1.68 ±0.07 | 0 |
| deno | 10687 ±20 | 0.09 ±0.00 | 0.11 ±0.00 | 32208 ±677 | 0.91 ±0.01 | 2.16 ±0.22 | 0 |
| bun | 13862 ±610 | 0.07 ±0.00 | 0.09 ±0.05 | 53790 ±624 | 0.46 ±0.00 | 2.07 ±0.07 | 0 |

#### GET /export.ndjson (stream, 3 × 1 ms timer)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 133 ±1 | 7.40 ±0.03 | 8.50 ±0.56 | 3142 ±25 | 10.16 ±0.07 | 13.96 ±0.03 | 0 |
| serve | 131 ±1 | 7.53 ±0.03 | 9.02 ±1.58 | 3808 ±9 | 7.89 ±0.02 | 11.90 ±0.15 | 0 |
| wasmtime | 121 ±1 | 8.15 ±0.11 | 10.38 ±0.68 | 4442 ±19 | 7.04 ±0.03 | 9.82 ±0.13 | 0 |
| spin | 118 ±6 | 8.30 ±0.45 | 12.04 ±1.23 | 4240 ±77 | 7.41 ±0.14 | 10.27 ±0.38 | 0 |
| node | 260 ±11 | 3.85 ±0.19 | 4.45 ±0.21 | 5660 ±225 | 5.83 ±0.21 | 9.73 ±0.42 | 0 |
| deno | 138 ±6 | 7.34 ±0.36 | 7.86 ±0.43 | 3607 ±153 | 8.87 ±0.35 | 12.97 ±0.50 | 0 |
| bun | 263 ±12 | 3.73 ±0.19 | 4.62 ±0.12 | 9190 ±420 | 3.38 ±0.18 | 5.97 ±0.36 | 0 |

### `realtime`

#### GET /rooms/lobby/messages (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 13148 ±15 | 0.07 ±0.00 | 0.10 ±0.00 | 20201 ±27 | 1.42 ±0.00 | 4.40 ±0.06 | 0 |
| node | 18212 ±121 | 0.05 ±0.00 | 0.06 ±0.00 | 49123 ±452 | 0.57 ±0.01 | 3.07 ±0.02 | 0 |
| deno | 17309 ±109 | 0.06 ±0.00 | 0.07 ±0.00 | 45280 ±187 | 0.65 ±0.00 | 2.06 ±0.01 | 0 |
| bun | 21354 ±54 | 0.04 ±0.00 | 0.06 ±0.01 | 78285 ±2000 | 0.35 ±0.01 | 0.89 ±0.08 | 0 |
