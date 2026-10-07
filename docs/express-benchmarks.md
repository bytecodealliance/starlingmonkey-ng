# Express and Koa app benchmarks

Throughput, latency, startup and memory of the apps in `tests/express-apps` under StarlingMonkey's
`--node-compat` mode, natively and as components, and under Node.js, Deno and Bun. The numbers
are from runs of `tests/express-apps/bench.mjs` on 2026-10-06, after the state of the Node.js
modules moved into Rust structs.
[Before and after the state refactor](#before-and-after-the-state-refactor) compares the native
configurations with the build before that change, and [Fixes](#fixes) lists the earlier fixes,
measured on 2026-10-05.

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
- A response with another status than the route's is an error. The runs had 18 errors in all,
  transport errors under Node.js on `POST /api/feedback` at 32 connections, in 3.5 to 15 million
  requests per configuration.
- Startup is measured in 5 separate processes per app and configuration: the time from spawning
  the server until it accepts a TCP connection, and the latency of the first request after
  that. Compiled components are in wasmtime's and Spin's caches, so the startup of components
  doesn't include compiling them.
- RSS is the sum over the server's process tree, after the first request and at the maximum of
  the samples taken every 500 ms under load. Several apps keep state that grows with the requests
  they serve (`morgan` and `pino` log lines, stored todos and feedback, sessions), so the maximum
  grows with the request rate.
- The machine is an Apple M5 MacBook with 10 cores and 32 GiB of memory, on macOS 27.0, with
  other work running: a Spotlight indexing process took one core throughout, and the load average
  was between 3 and 11 during the main run, and between 2 and 3 during the run of the build before
  the refactor.

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

The components run under wasmtime 49.0.0 and Spin 4.2.1. Both use their default instance reuse
for WASIp3 components: an instance serves up to 128 requests, up to 16 at once, and new instances
start for more concurrent requests. Each instance starts from the component's snapshot, taken
after the app's top level ran. [Instance reuse](#instance-reuse) has numbers for one request per
instance. `realtime` uses WebSockets on its HTTP server, so it runs under `listen`, Node.js, Deno
and Bun only.

## Summary

Requests per second at 1 and at 32 connections, and the p99 latency in ms at 32 connections, for
some of the routes:

| Route | listen | serve | wasmtime | spin | node | deno | bun |
|---|--:|--:|--:|--:|--:|--:|--:|
| `koa-portal` `GET /` | 6680 / 10904 / 5.1 | 5847 / 8727 / 5.6 | 943 / 5112 / 15.3 | 827 / 3365 / 23 | 9277 / 14386 / 3.7 | 7601 / 12533 / 3.9 | 9920 / 16869 / 4.1 |
| `koa-portal` `GET /css/portal.css` gzip | 3821 / 6461 / 12.0 | 2674 / 5284 / 16.1 | 1827 / 9168 / 13.7 | 1320 / 5936 / 18.1 | 5764 / 12024 / 5.7 | 5598 / 11297 / 5.5 | 6021 / 20733 / 4.1 |
| `rest-api` `GET /health` | 12476 / 21322 / 5.9 | 9895 / 18161 / 6.6 | 3267 / 19871 / 7.8 | 2830 / 15953 / 8.7 | 17400 / 41807 / 1.5 | 17047 / 40517 / 1.9 | 19080 / 62055 / 1.6 |
| `rest-api` `GET /api/v1/big` gzip | 5060 / 6990 / 9.5 | 4730 / 6643 / 10.1 | 1714 / 9992 / 9.1 | 1499 / 7668 / 11.5 | 5091 / 12346 / 5.7 | 5115 / 7002 / 8.3 | 5992 / 17431 / 5.4 |
| `rest-api` `POST /api/v1/echo` | 10375 / 14537 / 6.7 | 7499 / 12561 / 7.3 | 2156 / 11967 / 13.3 | 1959 / 10341 / 14.6 | 14863 / 32070 / 1.9 | 13831 / 29633 / 2.4 | 15485 / 40377 / 3.4 |
| `ssr-site` `GET /files/blob.bin` | 4973 / 9702 / 10.4 | 3916 / 8408 / 14.1 | 2705 / 14387 / 12.2 | 1676 / 7028 / 14.9 | 8414 / 15808 / 4.2 | 7784 / 14711 / 3.8 | 8080 / 18657 / 3.3 |
| `ssr-site` `GET /` | 9795 / 16190 / 7.1 | 8894 / 15236 / 7.3 | 2644 / 13792 / 11.4 | 2069 / 8951 / 21 | 13190 / 23056 / 2.5 | 12585 / 19192 / 3.7 | 15850 / 32003 / 2.5 |
| `files` `GET /stream/numbers` | 75 / 719 / 57 | 1062 / 1210 / 28 | 613 / 2964 / 22 | 520 / 2028 / 50 | 80 / 1346 / 25 | 80 / 1269 / 28 | 81 / 1356 / 26 |
| `auth` `POST /sign` | 9213 / 14591 / 5.8 | 7817 / 11320 / 6.4 | 2512 / 11987 / 17.9 | 2265 / 9874 / 18.3 | 11892 / 19102 / 3.3 | 11289 / 20199 / 2.9 | 16061 / 38815 / 1.7 |
| `data` `GET /posts` | 10301 / 23270 / 4.6 | 8714 / 22046 / 5.4 | 4998 / 23327 / 7.1 | 2923 / 17297 / 6.7 | 10579 / 30552 / 1.7 | 10672 / 33210 / 1.9 | 13775 / 55202 / 1.9 |

- Natively, StarlingMonkey serves 59 to 99% of Node.js's requests per second at 1 connection,
  and 45 to 76% at 32 connections, on the routes that don't wait on timers or stream. Its p99
  latency at 32 connections is 1.4 to 3.9 times Node.js's.
- The state refactor raised the native requests per second by a median of 8% at 1 connection and
  25% at 32 connections for `listen`, and 11% and 20% for `serve`. `serve`'s streamed route serves
  4.5 times as many requests as before.
- `serve` serves 70 to 93% of `listen`'s requests per second at 1 connection, as many on the
  routes that wait on timers, and 14 times as many on the streamed route.
- A component serves 14 to 54% of `listen`'s requests per second at 1 connection on the routes
  that don't wait on timers or stream. At 32 connections, wasmtime runs several instances on
  several threads, and serves 47 to 148% of `listen`'s requests per second.
- Spin serves 49 to 91% of wasmtime's requests per second, at higher latency.
- Natively, StarlingMonkey starts as fast as Node.js, and uses less memory after startup. Under
  load, its memory grows more than Node.js's for most apps.

## Startup and memory

The startup times and the RSS after start are the medians of 5 starts, and the maximum RSS under
load the median of the 3 repeats.

| App | Config | Listening ms | First request ms | RSS after start MiB | Max RSS under load MiB |
|---|---|--:|--:|--:|--:|
| koa-portal | listen | 103 ±35 | 2.0 ±0.8 | 52 ±0 | 858 ±13 |
| koa-portal | serve | 107 ±4 | 2.1 ±0.9 | 55 ±0 | 765 ±19 |
| koa-portal | wasmtime | 166 ±7 | 6.5 ±0.1 | 248 ±0 | 1633 ±172 |
| koa-portal | spin | 952 ±6 | 7.6 ±0.3 | 1381 ±0 | 2336 ±184 |
| koa-portal | node | 114 ±6 | 5.4 ±0.2 | 97 ±0 | 392 ±21 |
| koa-portal | deno | 99 ±50 | 6.2 ±2.1 | 77 ±2 | 439 ±10 |
| koa-portal | bun | 82 ±34 | 7.7 ±3.5 | 72 ±0 | 251 ±7 |
| rest-api | listen | 83 ±1 | 1.0 ±0.0 | 41 ±0 | 577 ±8 |
| rest-api | serve | 82 ±1 | 1.0 ±0.1 | 44 ±0 | 603 ±13 |
| rest-api | wasmtime | 151 ±4 | 4.5 ±0.1 | 219 ±0 | 1172 ±93 |
| rest-api | spin | 964 ±52 | 5.4 ±0.3 | 1372 ±0 | 2302 ±103 |
| rest-api | node | 88 ±3 | 3.8 ±0.3 | 76 ±0 | 518 ±57 |
| rest-api | deno | 84 ±18 | 3.4 ±0.1 | 61 ±2 | 706 ±8 |
| rest-api | bun | 71 ±10 | 3.1 ±0.4 | 49 ±0 | 837 ±11 |
| ssr-site | listen | 102 ±10 | 1.0 ±0.1 | 54 ±0 | 439 ±2 |
| ssr-site | serve | 101 ±3 | 1.1 ±0.1 | 57 ±0 | 473 ±3 |
| ssr-site | wasmtime | 171 ±5 | 5.3 ±0.1 | 248 ±0 | 1345 ±132 |
| ssr-site | spin | 949 ±5 | 6.5 ±0.7 | 1382 ±0 | 2271 ±121 |
| ssr-site | node | 107 ±3 | 3.6 ±0.1 | 82 ±0 | 383 ±12 |
| ssr-site | deno | 105 ±13 | 3.6 ±0.9 | 69 ±1 | 486 ±7 |
| ssr-site | bun | 87 ±9 | 3.2 ±0.9 | 64 ±1 | 356 ±14 |
| files | listen | 46 ±4 | 2.4 ±0.2 | 34 ±0 | 279 ±5 |
| files | serve | 51 ±1 | 1.8 ±0.1 | 36 ±0 | 404 ±31 |
| files | wasmtime | 136 ±4 | 5.3 ±0.1 | 200 ±0 | 886 ±45 |
| files | spin | 960 ±48 | 6.9 ±0.6 | 1366 ±0 | 1717 ±35 |
| files | node | 64 ±2 | 5.3 ±0.1 | 67 ±0 | 228 ±3 |
| files | deno | 64 ±8 | 5.2 ±0.2 | 54 ±1 | 170 ±5 |
| files | bun | 50 ±6 | 4.0 ±0.9 | 43 ±0 | 133 ±8 |
| auth | listen | 83 ±3 | 1.1 ±0.2 | 42 ±0 | 295 ±1 |
| auth | serve | 81 ±4 | 1.2 ±0.0 | 44 ±0 | 341 ±4 |
| auth | wasmtime | 151 ±3 | 5.3 ±0.3 | 257 ±0 | 1556 ±8 |
| auth | spin | 968 ±51 | 7.1 ±0.3 | 1384 ±0 | 2526 ±196 |
| auth | node | 88 ±3 | 4.4 ±0.0 | 76 ±0 | 137 ±7 |
| auth | deno | 92 ±11 | 4.1 ±0.6 | 62 ±1 | 236 ±1 |
| auth | bun | 71 ±5 | 3.6 ±0.8 | 49 ±0 | 136 ±4 |
| data | listen | 76 ±3 | 0.8 ±0.1 | 37 ±0 | 384 ±1 |
| data | serve | 75 ±2 | 0.8 ±0.5 | 39 ±0 | 413 ±9 |
| data | wasmtime | 145 ±2 | 4.0 ±0.2 | 203 ±0 | 889 ±100 |
| data | spin | 960 ±54 | 5.0 ±0.1 | 1367 ±0 | 2074 ±50 |
| data | node | 93 ±4 | 3.5 ±0.4 | 80 ±0 | 557 ±0 |
| data | deno | 86 ±11 | 3.2 ±0.1 | 57 ±1 | 484 ±2 |
| data | bun | 71 ±12 | 2.7 ±0.2 | 53 ±0 | 485 ±20 |
| realtime | listen | 51 ±15 | 0.8 ±0.0 | 33 ±0 | 310 ±0 |
| realtime | node | 64 ±5 | 3.2 ±0.1 | 66 ±0 | 240 ±0 |
| realtime | deno | 63 ±6 | 3.1 ±0.2 | 50 ±1 | 161 ±0 |
| realtime | bun | 51 ±4 | 3.3 ±1.0 | 39 ±0 | 101 ±2 |

- StarlingMonkey listens 45 to 110 ms after it is spawned natively, as Node.js does, and a
  component under `wasmtime serve` 135 to 175 ms after. The first request of a component
  instantiates it, which adds 3 to 5 ms to the first request.
- Spin listens about 950 ms after it is spawned, for the whole application with its seven
  components, and its process holds 1.3 GiB after the first request.
- After startup, StarlingMonkey holds 33 to 57 MiB natively, against 66 to 97 MiB for Node.js.
  A `wasmtime serve` process holds 200 to 260 MiB.
- Under load, StarlingMonkey's RSS reaches 280 to 860 MiB natively, against 140 to 560 MiB for
  Node.js. Components reach 0.9 to 1.6 GiB under wasmtime and 1.7 to 2.5 GiB under Spin, with
  several instances alive at 32 connections, each with its own linear memory.
- The maximum RSS under load of `listen` is 17 to 43% higher than before the state refactor for
  six of the seven apps, more than the throughput rose (see
  [Before and after the state refactor](#before-and-after-the-state-refactor)).

## Before and after the state refactor

The state refactor moved the state of streams, sockets, servers, HTTP messages and connections,
zlib streams and the file watchers from JS properties into one Rust struct per instance (see
`docs/node-compat.md`), and made `--serve` read the `FetchEvent`'s request and build its response
natively, with streamed bodies on a native `ReadableStream`. The build before it is commit
`dfea4f45`, which the [Fixes](#fixes) were measured on. Both builds ran the same apps on the
same day, with 3 repeats, the earlier build at a lower load average.

Requests per second at 1 and at 32 connections, and the change:

| Route | listen before | listen after | change c=1 / c=32 | serve before | serve after | change c=1 / c=32 |
|---|--:|--:|--:|--:|--:|--:|
| `koa-portal` `GET /` | 6294 / 9467 | 6680 / 10904 | +6% / +15% | 5513 / 8034 | 5847 / 8727 | +6% / +9% |
| `koa-portal` `GET /api/apps` | 7444 / 11949 | 7879 / 14313 | +6% / +20% | 6353 / 9953 | 6842 / 11086 | +8% / +11% |
| `koa-portal` `GET /css/portal.css` | 4538 / 7343 | 5439 / 9591 | +20% / +31% | 3833 / 5856 | 4407 / 7562 | +15% / +29% |
| `koa-portal` `GET /css/portal.css` gzip | 2712 / 5072 | 3821 / 6461 | +41% / +27% | 2186 / 4210 | 2674 / 5284 | +22% / +26% |
| `koa-portal` `POST /api/feedback` | 9371 / 15651 | 10652 / 20058 | +14% / +28% | 7198 / 11024 | 7921 / 13223 | +10% / +20% |
| `rest-api` `GET /health` | 11539 / 16202 | 12476 / 21322 | +8% / +32% | 8934 / 15093 | 9895 / 18161 | +11% / +20% |
| `rest-api` `GET /api/v1/big` | 8408 / 10682 | 9084 / 11664 | +8% / +9% | 6953 / 10488 | 7611 / 12048 | +9% / +15% |
| `rest-api` `GET /api/v1/big` gzip | 4781 / 6125 | 5060 / 6990 | +6% / +14% | 4200 / 5698 | 4730 / 6643 | +13% / +17% |
| `rest-api` `POST /api/v1/echo` | 9238 / 11975 | 10375 / 14537 | +12% / +21% | 6722 / 10029 | 7499 / 12561 | +12% / +25% |
| `rest-api` `POST /api/v1/todos` | 369 / 12214 | 373 / 14305 | +1% / +17% | 346 / 7208 | 358 / 7826 | +4% / +9% |
| `ssr-site` `GET /static/css/site.css` | 6635 / 11747 | 7426 / 15224 | +12% / +30% | 4966 / 9135 | 5909 / 13204 | +19% / +45% |
| `ssr-site` `GET /files/blob.bin` | 4535 / 7749 | 4973 / 9702 | +10% / +25% | 3423 / 6472 | 3916 / 8408 | +14% / +30% |
| `ssr-site` `GET /` | 9176 / 12930 | 9795 / 16190 | +7% / +25% | 7817 / 12097 | 8894 / 15236 | +14% / +26% |
| `files` `GET /stream/numbers` | 70 / 418 | 75 / 719 | +8% / +72% | 237 / 288 | 1062 / 1210 | +348% / +320% |
| `files` `POST /echo-gzip` | 8340 / 14899 | 9107 / 19189 | +9% / +29% | 7116 / 11401 | 7932 / 15117 | +11% / +33% |
| `auth` `POST /sign` | 8178 / 11820 | 9213 / 14591 | +13% / +23% | 7035 / 9539 | 7817 / 11320 | +11% / +19% |
| `auth` `GET /me` | 10394 / 15415 | 11232 / 18533 | +8% / +20% | 8899 / 15146 | 9552 / 17506 | +7% / +16% |
| `data` `GET /config` | 13698 / 20530 | 14834 / 28043 | +8% / +37% | 10716 / 22534 | 11618 / 27079 | +8% / +20% |
| `data` `GET /posts` | 9369 / 18928 | 10301 / 23270 | +10% / +23% | 8112 / 18618 | 8714 / 22046 | +7% / +18% |
| `data` `GET /export.ndjson` | 131 / 3152 | 135 / 3896 | +3% / +24% | 132 / 3829 | 132 / 4053 | +1% / +6% |
| `realtime` `GET /rooms/lobby/messages` | 13179 / 20349 | 14285 / 26907 | +8% / +32% | - | - | - |

- `listen` serves a median of 8% more requests per second at 1 connection (1 to 41%), and 25%
  more at 32 connections (9 to 72%). `serve` serves a median of 11% and 20% more.
- The median p99 latency at 32 connections changed by -2% for `listen` and -9% for `serve`.
- `serve`'s `GET /stream/numbers` went from 237 to 1062 requests per second at 1 connection: a
  response's chunks now go to a native `ReadableStream` instead of through a JS stream.
- CPU per request of `rest-api` `GET /health`, from the process's CPU time over 8 s of load:
  `listen` 74 to 62 µs at 1 connection and 62 to 48 µs at 32 connections. Node.js takes 36 and
  23 µs. Of `koa-portal` `GET /`: 147 to 130 µs and 112 to 97 µs, against 82 and 76 µs.
- The maximum RSS under load of `listen` rose from 627 to 858 MiB for `koa-portal`, 448 to 577 MiB
  for `rest-api`, 375 to 439 MiB for `ssr-site`, 218 to 295 MiB for `auth`, 269 to 384 MiB for
  `data` and 217 to 310 MiB for `realtime`, and fell from 348 to 279 MiB for `files`. These
  measurements don't establish why.

## Property templates

On 2026-10-06, the own properties of new `IncomingMessage`, `ServerResponse` and `Url` objects,
and of the emitters and readable streams they start as, moved to `js::object::PropertyTemplate`s.
A profile of `listen` before the change had 9.4% of the main thread's samples in
`JS_SetPropertyById`, 5.7% in creating `ServerResponse`s, 4.1% in creating `IncomingMessage`s and
1.9% in `new Url()`, which Express's `parseurl` calls for each request. Each assignment to a new
object looked through the whole prototype chain, and each property name was looked up in the
global's key cache. The messages the server creates now get their properties defined with the
values of the request head, with the keys looked up once per object, and `new Url()` copies its
fields from a `Url` that each global creates at setup. After the change, `JS_SetPropertyById` had
1.7% of the samples.

CPU per request in µs and requests per second, the median of 3 alternating runs of each build,
each 5 s after a 2 s warmup:

| Route | Config | c=1 µs before / after | c=1 req/s before / after | c=32 µs before / after | c=32 req/s before / after |
|---|---|--:|--:|--:|--:|
| `rest-api` `GET /health` | `listen` | 61.9 / 59.0 | 12725 / 13207 | 48.1 / 44.7 | 21004 / 22467 |
| `rest-api` `GET /health` | `serve` | 65.1 / 63.6 | 9923 / 10100 | 55.1 / 51.5 | 18181 / 19424 |
| `koa-portal` `GET /` | `listen` | 130.0 / 129.2 | 6737 / 6779 | 100.2 / 95.2 | 10802 / 11356 |

## Properties in one call

On 2026-10-07, the writers of `PropertyTemplate`s started adding all of an object's properties in
one engine call, SpiderMonkey's `AddDataPropertiesToNewObject`, and the classes whose instances
get their properties from templates started creating them with fixed slots for the templates'
keys, up to SpiderMonkey's 16 fixed slots, which include the state slot of a class's instances.
`Timeout`s, `Immediate`s, `fs.Stats`, `fs.Dirent`s, socket addresses, the options
object of custom inspection methods and mock call records moved to templates too.

In profiles of `listen` serving `GET /health` at 32 connections, `NativeDefineProperty` had 2.4%
of the main thread's samples before the change and 0.1% after, and `AddDataPropertiesToNewObject`
had 1.8% after, most of it in `NativeObject::addProperty` and in growing the dynamic slots of the
messages. Creating `IncomingMessage`s fell from 4.3% to 3.6% of the samples, and creating
`ServerResponse`s stayed at 1.4%.

Other work loaded the machine during these measurements, with a load average of 9 to 17, which
roughly doubled the CPU time per request against the sections above. So the servers of both builds
ran side by side, and 12 pairs of 2 s runs alternated between them, in ABBA order, after a 1 s
warmup of each server on each route. CPU per request in µs and requests per second, the medians
of the runs:

| Route | Config | c=1 µs before / after | c=1 req/s before / after | c=32 µs before / after | c=32 req/s before / after |
|---|---|--:|--:|--:|--:|
| `rest-api` `GET /health` | `listen` | 127.9 / 125.1 | 5454 / 5723 | 125.9 / 120.7 | 6075 / 6391 |
| `rest-api` `GET /health` | `serve` | 254.3 / 260.1 | 2301 / 2227 | 177.6 / 175.9 | 3321 / 3297 |
| `rest-api` `GET /api/v1/big` | `listen` | 319.1 / 321.5 | 1892 / 1882 | 269.7 / 264.7 | 2113 / 2126 |
| `rest-api` `GET /api/v1/big` | `serve` | 350.6 / 321.1 | 1959 / 2171 | 264.6 / 253.9 | 2439 / 2906 |

The median ratio of the CPU per request of the pairs, after to before, was 0.985 to 1.013 in each
cell. A second measurement gave 0.94 to 1.05. The change of the CPU per request is smaller than
the noise of these measurements.

Operations that create such objects, in ns of wall time per operation, the median of 5 alternating
runs of each build with 3 measurements each:

| Operation | Before | After |
|---|--:|--:|
| `url.parse('/api/v1/items?x=1')` | 706 | 572 |
| `fs.statSync('.')` | 1799 | 1391 |
| `clearTimeout(setTimeout(f, 1000))` | 1191 | 865 |
| `clearImmediate(setImmediate(f))` | 921 | 766 |
| `util.inspect` of an object with a custom inspection method | 1200 | 934 |

## Fixes

On 2026-10-05, profiles of `listen` under load (an unstripped release build sampled with macOS's
`sample`) showed where the native runtime spends its time beyond the apps' JavaScript. These
changes are on the branch:

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

For `GET /health` at 32 connections, `listen` spends about 48 µs of CPU per request, and Node.js
about 23 µs (62 and 36 µs at 1 connection). A profile of that run's main thread, by the function
the samples are in:

| Share | Where |
|---|---|
| 15% | the Node.js modules in Rust: `http`, streams, events, `url` |
| 15% | SpiderMonkey's generic property gets and sets (`NativeSetProperty` 4.8%, `NativeGetProperty` 4.7%, property map lookups 1.6%) |
| 13% | `sendto` and `recvfrom` |
| 11% | nursery collections, tracing and write barriers |
| 11% | JIT-compiled JavaScript: Express, its middleware and the app |
| 10% | `malloc` and `free` |
| 9% | the `js` crate: rooting scopes, callback trampolines, the class registry and string conversions |
| 8% | the rest of SpiderMonkey |
| 8% | `memmove`, `memset`, `memcmp` and other |

Before the refactor, the same profile had 41% in SpiderMonkey's runtime (`NativeGetProperty`
7.7%, `NativeSetProperty` 4.8%, property map lookups 2.8%, nursery collections about 8%), 13% in
the Node.js modules, 12% in `malloc` and `free`, 11% in `sendto` and `recvfrom`, and 8% in
JIT-compiled JavaScript, at 62 µs per request. The three named property functions fell from
15.3% of 62 µs to 11.1% of 48 µs, or from about 9.5 to 5.3 µs per request. The system calls take
about 6.5 µs per request in both, so their share grew.

The generic property accesses that remain are Node-public properties, which Node's own code and
the apps read too. About 40% of the sets define the own data properties Node.js gives each
`IncomingMessage` and `ServerResponse` when they are created, 16% initialize emitters (`_events`,
`_eventsCount`, `_maxListeners`, `kCapture` and `shapeMode`), and 14% fill the legacy `Url`
object that `url.parse` returns, which Express's router creates for each request. Most gets read
an emitter's `_events`, in `emit`, `on` and `removeListener`.

A profile of `koa-portal` `GET /` at 32 connections, 97 µs of CPU per request against Node.js's
76 µs, has 27% of its samples in JIT-compiled JavaScript, 17% in the rest of SpiderMonkey, mostly
string building and splitting for the Handlebars page, 9% in generic property gets and sets, 8%
in nursery collections and barriers, 8% in system calls, 7% in the Node.js modules, 5% in
`malloc` and `free`, and 4% in SHA-1, which `@koa/etag` hashes each body with.

The gap grows with concurrency: Node.js serves 1.5 to 2.9 times its 1-connection rate at 32
connections, and `listen` 1.3 to 2.3 times. Node.js's CPU per request of `GET /health` falls from
36 to 23 µs from 1 to 32 connections, and `listen`'s from 62 to 48 µs. These measurements don't
establish why.

### `--serve` against `--node-http-listen`

`serve` handles 7 to 30% fewer requests per second than `listen` at 1 connection, on the routes
that don't wait on timers or stream. The runtime's server dispatches a fetch event with a
`Request`, and the `http` module creates an `IncomingMessage` that reads the request's method, URL,
headers and body natively, and responds with a `Response` it creates natively. So each request
still creates the fetch objects as well as the `http` objects. `serve` is 14 times faster than
`listen` on the streamed route at 1 connection, where `listen`, like Node.js, takes 12 to 14 ms per
request for a response of about 4 KB. These measurements don't establish why.

### Components

A component serves 14 to 54% of `listen`'s requests per second at 1 connection. The JavaScript
engine runs inside the component without a JIT, since WebAssembly can't generate machine code at
runtime. A loop of object creation, string building and JSON runs at 6,161 iterations per second
in the component under `wasmtime run`, against 28,067 natively and 26,290 under Node.js, so
JavaScript runs about 4.6 times slower (measured on 2026-10-05). The routes whose time is mostly
JavaScript, such as the Handlebars page (14% of `listen`), lose the most, and the routes that
mostly move bytes, such as the static files (42 to 54%), the least.

At 32 connections, wasmtime runs up to 16 requests in an instance at once, and more instances on
its thread pool, so the components' throughput at 32 connections comes from the extra cores. Their
p99 latency is 0.95 to 3.3 times `listen`'s.

Spin runs the same components, and serves 49 to 91% of wasmtime's requests per second, with up to
twice its p99 latency at 32 connections.

### Instance reuse

With one request per instance (`--max-instance-reuse-count 1`), each request instantiates the
component from its snapshot. These numbers are from the run of 2026-10-05:

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
`POST /api/v1/todos` (2.7 ms, against 1.5 ms under Node.js) and `GET /export.ndjson` (7.4 ms
against 3.9 ms). Deno takes as long.

### Streams of many chunks

`GET /stream/numbers` pipes 1000 chunks of a few bytes each into the response. `listen` serves
719 requests per second at 32 connections, against 418 before the state refactor and 1346 under
Node.js. Before the refactor, generic property gets and sets took more than a third of the CPU
time of this route.

### Remaining gaps

| Gap | Cause | Fixable |
|---|---|---|
| Native CPU per request 1.3 to 2.1 times Node.js's (`GET /health` 48 against 23 µs, `koa-portal` `GET /` 97 against 76 µs at 32 connections) | Node's own data properties of each message and emitter, added one shape transition at a time, emitters' listeners in JS properties as in Node.js, objects created per request and their nursery collections, and a `sendto` per response | partly, such as by creating messages from a shape cached per template |
| Less gain from concurrency than Node.js | not established | unknown |
| `serve` 7 to 30% slower than `listen` | the fetch objects are created for each request, besides the `http` objects | partly |
| Higher maximum RSS under load than before the state refactor, 17 to 43% for `listen` on six apps | not established | unknown |
| Components 1.9 to 7 times slower than native at 1 connection | no JIT in WebAssembly | no, apart from making the interpreter and the per-request path cheaper |
| `setTimeout` of 1 ms takes 2.3 ms natively | tokio's millisecond timer resolution | yes, with a timer of finer resolution for short waits |
| Spin's RSS of 1.3 GiB at startup | not established. The process serves all seven components | outside the runtime |

## Results by app

Requests per second, and latency in ms, at 1 and at 32 connections.

### `koa-portal`

#### GET / (Handlebars page)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 6680 ±530 | 0.14 ±0.01 | 0.18 ±0.02 | 10904 ±1516 | 2.90 ±0.55 | 5.07 ±1.06 | 0 |
| serve | 5847 ±648 | 0.17 ±0.02 | 0.19 ±0.04 | 8727 ±1345 | 3.65 ±0.74 | 5.58 ±1.56 | 0 |
| wasmtime | 943 ±96 | 1.05 ±0.09 | 1.32 ±0.63 | 5112 ±1357 | 5.96 ±2.91 | 15.34 ±11.01 | 0 |
| spin | 827 ±104 | 1.16 ±0.15 | 1.55 ±0.96 | 3365 ±1098 | 9.09 ±3.50 | 22.64 ±11.71 | 0 |
| node | 9277 ±221 | 0.11 ±0.00 | 0.14 ±0.01 | 14386 ±311 | 2.23 ±0.04 | 3.66 ±0.09 | 0 |
| deno | 7601 ±700 | 0.13 ±0.01 | 0.17 ±0.03 | 12533 ±1491 | 2.60 ±0.17 | 3.94 ±1.30 | 0 |
| bun | 9920 ±181 | 0.10 ±0.00 | 0.12 ±0.00 | 16869 ±224 | 1.73 ±0.02 | 4.05 ±0.05 | 0 |

#### GET /api/apps (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 7879 ±492 | 0.12 ±0.01 | 0.15 ±0.01 | 14313 ±1808 | 1.95 ±0.44 | 5.62 ±0.53 | 0 |
| serve | 6842 ±472 | 0.14 ±0.01 | 0.16 ±0.01 | 11086 ±1433 | 2.84 ±0.52 | 6.44 ±1.31 | 0 |
| wasmtime | 2244 ±181 | 0.42 ±0.02 | 0.83 ±0.21 | 12568 ±3166 | 2.30 ±1.04 | 11.59 ±5.90 | 0 |
| spin | 1881 ±133 | 0.50 ±0.04 | 0.72 ±0.18 | 8050 ±2277 | 3.61 ±1.19 | 15.69 ±4.48 | 0 |
| node | 11724 ±320 | 0.08 ±0.00 | 0.10 ±0.00 | 20637 ±742 | 1.55 ±0.06 | 2.57 ±0.11 | 0 |
| deno | 9243 ±714 | 0.11 ±0.01 | 0.14 ±0.03 | 16955 ±2972 | 2.03 ±0.08 | 2.94 ±4.76 | 0 |
| bun | 12643 ±273 | 0.08 ±0.00 | 0.09 ±0.00 | 30557 ±1602 | 0.94 ±0.06 | 2.51 ±0.09 | 0 |

#### GET /css/portal.css (static)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 5439 ±531 | 0.17 ±0.02 | 0.28 ±0.04 | 9591 ±1626 | 2.95 ±0.78 | 12.87 ±2.47 | 0 |
| serve | 4407 ±285 | 0.21 ±0.01 | 0.32 ±0.04 | 7562 ±1224 | 3.77 ±0.91 | 15.52 ±3.71 | 0 |
| wasmtime | 2556 ±183 | 0.37 ±0.02 | 0.45 ±0.16 | 11438 ±2818 | 2.52 ±1.04 | 12.25 ±5.05 | 0 |
| spin | 1879 ±262 | 0.51 ±0.09 | 0.67 ±0.26 | 6578 ±1672 | 4.23 ±1.35 | 15.10 ±7.09 | 0 |
| node | 8495 ±422 | 0.10 ±0.00 | 0.58 ±0.12 | 17559 ±3142 | 1.63 ±0.24 | 4.74 ±0.72 | 0 |
| deno | 8301 ±235 | 0.11 ±0.00 | 0.18 ±0.02 | 17583 ±1466 | 1.66 ±0.14 | 2.92 ±0.79 | 0 |
| bun | 9335 ±98 | 0.10 ±0.00 | 0.15 ±0.00 | 28858 ±2664 | 1.03 ±0.12 | 2.24 ±0.17 | 0 |

#### GET /css/portal.css (static, gzip)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 3821 ±550 | 0.24 ±0.05 | 0.43 ±0.06 | 6461 ±1107 | 4.38 ±1.10 | 12.05 ±3.65 | 0 |
| serve | 2674 ±109 | 0.36 ±0.01 | 0.49 ±0.02 | 5284 ±779 | 6.34 ±1.19 | 16.11 ±3.98 | 0 |
| wasmtime | 1827 ±98 | 0.52 ±0.02 | 0.67 ±0.25 | 9168 ±2512 | 3.16 ±1.40 | 13.72 ±7.01 | 0 |
| spin | 1320 ±50 | 0.73 ±0.02 | 1.32 ±0.48 | 5936 ±1097 | 4.54 ±0.71 | 18.08 ±2.78 | 0 |
| node | 5764 ±475 | 0.15 ±0.00 | 0.99 ±0.16 | 12024 ±1721 | 2.45 ±0.29 | 5.67 ±0.49 | 0 |
| deno | 5598 ±175 | 0.17 ±0.01 | 0.30 ±0.05 | 11297 ±688 | 2.68 ±0.14 | 5.48 ±0.45 | 0 |
| bun | 6021 ±125 | 0.16 ±0.00 | 0.26 ±0.01 | 20733 ±1945 | 1.41 ±0.15 | 4.08 ±0.32 | 0 |

#### POST /api/feedback (JSON, validated)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 10652 ±728 | 0.09 ±0.01 | 0.12 ±0.01 | 20058 ±2888 | 1.58 ±0.31 | 4.77 ±0.94 | 0 |
| serve | 7921 ±579 | 0.12 ±0.01 | 0.16 ±0.02 | 13223 ±1756 | 2.32 ±0.32 | 6.27 ±0.93 | 0 |
| wasmtime | 2254 ±125 | 0.41 ±0.02 | 0.53 ±0.16 | 11065 ±2814 | 2.51 ±0.90 | 15.52 ±7.38 | 0 |
| spin | 2037 ±40 | 0.44 ±0.00 | 0.70 ±0.16 | 8200 ±1865 | 3.29 ±0.48 | 21.81 ±4.96 | 0 |
| node | 17363 ±2758 | 0.06 ±0.00 | 0.07 ±0.15 | 36955 ±11454 | 0.82 ±0.16 | 1.74 ±3.60 | 18 |
| deno | 14641 ±610 | 0.07 ±0.00 | 0.09 ±0.01 | 35955 ±1908 | 0.89 ±0.02 | 1.60 ±0.45 | 0 |
| bun | 16042 ±304 | 0.06 ±0.00 | 0.07 ±0.00 | 42842 ±1866 | 0.67 ±0.04 | 1.90 ±0.10 | 0 |

### `rest-api`

#### GET /health (text)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 12476 ±359 | 0.07 ±0.00 | 0.09 ±0.02 | 21322 ±489 | 1.33 ±0.04 | 5.86 ±0.11 | 0 |
| serve | 9895 ±182 | 0.09 ±0.00 | 0.11 ±0.01 | 18161 ±332 | 1.59 ±0.03 | 6.61 ±0.26 | 0 |
| wasmtime | 3267 ±53 | 0.29 ±0.00 | 0.67 ±0.03 | 19871 ±299 | 1.40 ±0.01 | 7.84 ±0.66 | 0 |
| spin | 2830 ±50 | 0.33 ±0.00 | 0.79 ±0.04 | 15953 ±602 | 1.77 ±0.07 | 8.75 ±0.62 | 0 |
| node | 17400 ±462 | 0.06 ±0.00 | 0.06 ±0.00 | 41807 ±1542 | 0.71 ±0.03 | 1.49 ±0.09 | 0 |
| deno | 17047 ±356 | 0.06 ±0.00 | 0.07 ±0.00 | 40517 ±1720 | 0.72 ±0.04 | 1.92 ±0.11 | 0 |
| bun | 19080 ±400 | 0.05 ±0.00 | 0.06 ±0.03 | 62055 ±1099 | 0.45 ±0.01 | 1.63 ±0.02 | 0 |

#### GET /api/v1/big (JSON, 12 KB)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 9084 ±221 | 0.10 ±0.00 | 0.12 ±0.01 | 11664 ±338 | 2.53 ±0.08 | 7.44 ±0.28 | 0 |
| serve | 7611 ±95 | 0.12 ±0.00 | 0.14 ±0.01 | 12048 ±207 | 2.43 ±0.01 | 7.25 ±0.32 | 0 |
| wasmtime | 2167 ±43 | 0.44 ±0.01 | 0.83 ±0.07 | 12410 ±214 | 2.31 ±0.02 | 9.13 ±0.92 | 0 |
| spin | 1876 ±17 | 0.50 ±0.00 | 0.97 ±0.01 | 10270 ±749 | 2.72 ±0.23 | 10.21 ±0.98 | 0 |
| node | 11040 ±167 | 0.09 ±0.00 | 0.10 ±0.00 | 16875 ±686 | 1.78 ±0.11 | 3.85 ±0.09 | 0 |
| deno | 9117 ±91 | 0.11 ±0.00 | 0.12 ±0.00 | 11980 ±342 | 2.63 ±0.06 | 3.42 ±0.10 | 0 |
| bun | 12795 ±30 | 0.07 ±0.00 | 0.10 ±0.00 | 24180 ±235 | 1.19 ±0.01 | 3.82 ±0.07 | 0 |

#### GET /api/v1/big (JSON, gzip)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 5060 ±81 | 0.19 ±0.00 | 0.22 ±0.01 | 6990 ±107 | 4.36 ±0.08 | 9.51 ±0.16 | 0 |
| serve | 4730 ±66 | 0.20 ±0.00 | 0.22 ±0.02 | 6643 ±66 | 4.54 ±0.04 | 10.05 ±0.20 | 0 |
| wasmtime | 1714 ±33 | 0.56 ±0.01 | 0.96 ±0.10 | 9992 ±331 | 2.90 ±0.12 | 9.11 ±0.59 | 0 |
| spin | 1499 ±18 | 0.63 ±0.01 | 1.11 ±0.02 | 7668 ±641 | 3.82 ±0.32 | 11.47 ±1.21 | 0 |
| node | 5091 ±36 | 0.19 ±0.00 | 0.31 ±0.01 | 12346 ±1099 | 2.29 ±0.37 | 5.71 ±0.64 | 0 |
| deno | 5115 ±26 | 0.19 ±0.00 | 0.22 ±0.00 | 7002 ±56 | 4.42 ±0.03 | 8.28 ±0.09 | 0 |
| bun | 5992 ±75 | 0.16 ±0.00 | 0.21 ±0.03 | 17431 ±219 | 1.86 ±0.02 | 5.41 ±0.05 | 0 |

#### POST /api/v1/echo (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 10375 ±311 | 0.09 ±0.00 | 0.11 ±0.00 | 14537 ±394 | 2.00 ±0.05 | 6.73 ±0.12 | 0 |
| serve | 7499 ±107 | 0.12 ±0.00 | 0.15 ±0.02 | 12561 ±357 | 2.38 ±0.04 | 7.30 ±0.48 | 0 |
| wasmtime | 2156 ±73 | 0.42 ±0.00 | 0.86 ±0.33 | 11967 ±182 | 2.34 ±0.02 | 13.28 ±1.14 | 0 |
| spin | 1959 ±21 | 0.46 ±0.01 | 0.97 ±0.01 | 10341 ±903 | 2.55 ±0.32 | 14.60 ±0.65 | 0 |
| node | 14863 ±437 | 0.07 ±0.00 | 0.08 ±0.00 | 32070 ±1508 | 0.94 ±0.03 | 1.93 ±0.13 | 0 |
| deno | 13831 ±255 | 0.07 ±0.00 | 0.08 ±0.00 | 29633 ±158 | 0.97 ±0.00 | 2.43 ±0.01 | 0 |
| bun | 15485 ±108 | 0.06 ±0.00 | 0.07 ±0.01 | 40377 ±821 | 0.68 ±0.01 | 3.41 ±0.05 | 0 |

#### POST /api/v1/todos (JSON, zod, 1 ms timer)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 373 ±19 | 2.65 ±0.23 | 3.00 ±0.15 | 14305 ±26 | 1.88 ±0.02 | 6.76 ±0.27 | 0 |
| serve | 358 ±22 | 2.69 ±0.21 | 3.12 ±0.18 | 7826 ±118 | 3.96 ±0.01 | 9.13 ±0.24 | 0 |
| wasmtime | 258 ±23 | 3.63 ±0.34 | 5.34 ±0.23 | 8979 ±194 | 3.34 ±0.09 | 10.34 ±0.30 | 0 |
| spin | 260 ±12 | 3.60 ±0.37 | 5.82 ±0.24 | 8397 ±332 | 3.50 ±0.10 | 15.70 ±2.63 | 0 |
| node | 685 ±30 | 1.46 ±0.07 | 1.78 ±0.07 | 16665 ±1153 | 2.12 ±0.25 | 3.05 ±0.03 | 0 |
| deno | 352 ±19 | 2.91 ±0.14 | 3.11 ±0.15 | 8108 ±326 | 4.14 ±0.15 | 6.51 ±0.33 | 0 |
| bun | 657 ±39 | 1.46 ±0.07 | 1.73 ±0.22 | 26109 ±351 | 1.14 ±0.02 | 3.54 ±0.10 | 0 |

### `ssr-site`

#### GET /static/css/site.css (static)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 7426 ±13 | 0.13 ±0.00 | 0.16 ±0.00 | 15224 ±68 | 1.90 ±0.01 | 5.91 ±0.04 | 0 |
| serve | 5909 ±71 | 0.16 ±0.00 | 0.19 ±0.00 | 13204 ±142 | 2.19 ±0.01 | 7.32 ±0.11 | 0 |
| wasmtime | 3127 ±33 | 0.30 ±0.00 | 0.79 ±0.04 | 16706 ±906 | 1.66 ±0.12 | 10.12 ±0.42 | 0 |
| spin | 2259 ±7 | 0.42 ±0.00 | 1.09 ±0.02 | 10623 ±1368 | 2.58 ±0.22 | 11.08 ±1.61 | 0 |
| node | 11984 ±554 | 0.08 ±0.00 | 0.10 ±0.01 | 26164 ±3570 | 1.21 ±0.14 | 1.51 ±0.50 | 0 |
| deno | 9420 ±599 | 0.10 ±0.01 | 0.14 ±0.04 | 21555 ±2386 | 1.43 ±0.16 | 2.66 ±0.24 | 0 |
| bun | 10866 ±314 | 0.09 ±0.00 | 0.11 ±0.03 | 34157 ±1191 | 0.92 ±0.02 | 1.62 ±0.14 | 0 |

#### GET /files/blob.bin (static, 70 KB)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 4973 ±25 | 0.19 ±0.00 | 0.27 ±0.01 | 9702 ±32 | 2.94 ±0.01 | 10.44 ±0.08 | 0 |
| serve | 3916 ±13 | 0.24 ±0.00 | 0.31 ±0.02 | 8408 ±130 | 3.72 ±0.08 | 14.09 ±0.08 | 0 |
| wasmtime | 2705 ±31 | 0.34 ±0.00 | 1.06 ±0.09 | 14387 ±1252 | 1.88 ±0.21 | 12.24 ±0.64 | 0 |
| spin | 1676 ±32 | 0.56 ±0.01 | 1.32 ±0.03 | 7028 ±704 | 3.73 ±0.32 | 14.92 ±1.49 | 0 |
| node | 8414 ±569 | 0.11 ±0.01 | 0.27 ±0.07 | 15808 ±2116 | 1.91 ±0.22 | 4.22 ±0.49 | 0 |
| deno | 7784 ±308 | 0.12 ±0.00 | 0.16 ±0.04 | 14711 ±842 | 2.11 ±0.11 | 3.84 ±0.24 | 0 |
| bun | 8080 ±101 | 0.12 ±0.00 | 0.15 ±0.00 | 18657 ±62 | 1.66 ±0.01 | 3.35 ±0.14 | 0 |

#### GET / (EJS page, new session)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 9795 ±173 | 0.09 ±0.00 | 0.12 ±0.02 | 16190 ±145 | 1.75 ±0.01 | 7.15 ±0.09 | 0 |
| serve | 8894 ±167 | 0.10 ±0.00 | 0.13 ±0.00 | 15236 ±184 | 1.84 ±0.04 | 7.31 ±0.58 | 0 |
| wasmtime | 2644 ±10 | 0.35 ±0.00 | 0.82 ±0.03 | 13792 ±707 | 2.02 ±0.11 | 11.37 ±0.27 | 0 |
| spin | 2069 ±14 | 0.45 ±0.00 | 1.01 ±0.07 | 8951 ±323 | 2.42 ±0.10 | 20.64 ±0.88 | 0 |
| node | 13190 ±379 | 0.07 ±0.00 | 0.10 ±0.01 | 23056 ±1229 | 1.42 ±0.08 | 2.47 ±0.16 | 0 |
| deno | 12585 ±118 | 0.08 ±0.00 | 0.09 ±0.02 | 19192 ±199 | 1.67 ±0.02 | 3.68 ±0.23 | 0 |
| bun | 15850 ±100 | 0.06 ±0.00 | 0.08 ±0.00 | 32003 ±504 | 1.01 ±0.02 | 2.49 ±0.05 | 0 |

### `files`

#### GET /stream/numbers (Readable.pipe, 1000 chunks)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 75 ±0 | 13.26 ±0.05 | 13.65 ±0.08 | 719 ±10 | 44.32 ±0.60 | 57.33 ±1.01 | 0 |
| serve | 1062 ±4 | 0.94 ±0.00 | 1.08 ±0.01 | 1210 ±22 | 26.53 ±0.44 | 28.35 ±0.66 | 0 |
| wasmtime | 613 ±3 | 1.60 ±0.01 | 1.88 ±0.03 | 2964 ±170 | 10.36 ±0.34 | 21.64 ±1.55 | 0 |
| spin | 520 ±1 | 1.89 ±0.01 | 2.41 ±0.66 | 2028 ±168 | 12.93 ±1.12 | 50.02 ±6.41 | 0 |
| node | 80 ±0 | 12.46 ±0.02 | 12.80 ±0.04 | 1346 ±43 | 23.64 ±0.98 | 25.33 ±1.55 | 0 |
| deno | 80 ±1 | 12.49 ±0.06 | 12.98 ±1.05 | 1269 ±38 | 24.11 ±0.87 | 27.82 ±6.31 | 0 |
| bun | 81 ±1 | 12.28 ±0.09 | 12.96 ±1.37 | 1356 ±27 | 22.94 ±0.64 | 26.12 ±1.57 | 0 |

#### POST /echo-gzip (request body stream)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 9107 ±9 | 0.10 ±0.00 | 0.14 ±0.00 | 19189 ±160 | 1.50 ±0.01 | 4.57 ±0.09 | 0 |
| serve | 7932 ±13 | 0.12 ±0.00 | 0.14 ±0.00 | 15117 ±93 | 2.05 ±0.03 | 5.46 ±0.01 | 0 |
| wasmtime | 4722 ±471 | 0.20 ±0.03 | 0.46 ±0.03 | 20016 ±1285 | 1.41 ±0.09 | 7.90 ±0.68 | 0 |
| spin | 2526 ±9 | 0.38 ±0.00 | 0.73 ±0.08 | 12170 ±778 | 2.30 ±0.13 | 9.04 ±0.45 | 0 |
| node | 12815 ±305 | 0.07 ±0.00 | 0.10 ±0.04 | 32657 ±2932 | 0.96 ±0.09 | 1.91 ±0.17 | 0 |
| deno | 11294 ±143 | 0.09 ±0.00 | 0.10 ±0.00 | 26105 ±2054 | 1.15 ±0.08 | 2.09 ±0.55 | 0 |
| bun | 14513 ±73 | 0.07 ±0.00 | 0.08 ±0.01 | 42084 ±5540 | 0.66 ±0.08 | 1.91 ±0.19 | 0 |

### `auth`

#### POST /sign (JWT sign + verify)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 9213 ±44 | 0.10 ±0.00 | 0.14 ±0.01 | 14591 ±13 | 1.90 ±0.01 | 5.82 ±0.01 | 0 |
| serve | 7817 ±14 | 0.12 ±0.00 | 0.15 ±0.00 | 11320 ±134 | 2.64 ±0.05 | 6.36 ±0.10 | 0 |
| wasmtime | 2512 ±18 | 0.35 ±0.00 | 0.84 ±0.00 | 11987 ±1140 | 2.22 ±0.19 | 17.87 ±1.17 | 0 |
| spin | 2265 ±38 | 0.38 ±0.00 | 0.98 ±0.02 | 9874 ±883 | 2.73 ±0.23 | 18.33 ±1.06 | 0 |
| node | 11892 ±31 | 0.08 ±0.00 | 0.10 ±0.04 | 19102 ±328 | 1.62 ±0.04 | 3.30 ±0.05 | 0 |
| deno | 11289 ±30 | 0.09 ±0.00 | 0.10 ±0.00 | 20199 ±335 | 1.48 ±0.02 | 2.85 ±0.08 | 0 |
| bun | 16061 ±188 | 0.06 ±0.00 | 0.08 ±0.00 | 38815 ±1934 | 0.76 ±0.05 | 1.67 ±0.11 | 0 |

#### GET /me (JWT verify)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 11232 ±14 | 0.08 ±0.00 | 0.11 ±0.00 | 18533 ±54 | 1.53 ±0.01 | 5.14 ±0.06 | 0 |
| serve | 9552 ±47 | 0.10 ±0.00 | 0.12 ±0.00 | 17506 ±186 | 1.65 ±0.03 | 5.72 ±0.04 | 0 |
| wasmtime | 4490 ±23 | 0.19 ±0.00 | 0.69 ±0.01 | 17599 ±1767 | 1.50 ±0.13 | 12.46 ±1.28 | 0 |
| spin | 3202 ±49 | 0.28 ±0.01 | 0.92 ±0.02 | 15197 ±853 | 1.79 ±0.11 | 11.99 ±1.18 | 0 |
| node | 14878 ±95 | 0.07 ±0.00 | 0.08 ±0.01 | 28516 ±715 | 1.09 ±0.03 | 2.20 ±0.07 | 0 |
| deno | 14147 ±145 | 0.07 ±0.00 | 0.08 ±0.00 | 26899 ±841 | 1.10 ±0.04 | 2.08 ±0.01 | 0 |
| bun | 18235 ±133 | 0.05 ±0.00 | 0.07 ±0.00 | 50143 ±783 | 0.59 ±0.02 | 1.36 ±0.03 | 0 |

### `data`

#### GET /config (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 14834 ±75 | 0.06 ±0.00 | 0.07 ±0.00 | 28043 ±190 | 1.01 ±0.01 | 3.94 ±0.01 | 0 |
| serve | 11618 ±111 | 0.08 ±0.00 | 0.10 ±0.00 | 27079 ±277 | 1.05 ±0.01 | 4.68 ±0.07 | 0 |
| wasmtime | 5981 ±16 | 0.15 ±0.00 | 0.38 ±0.01 | 28081 ±201 | 0.98 ±0.01 | 6.15 ±0.21 | 0 |
| spin | 5016 ±13 | 0.18 ±0.00 | 0.48 ±0.00 | 24522 ±442 | 1.14 ±0.02 | 6.49 ±0.10 | 0 |
| node | 19948 ±58 | 0.05 ±0.00 | 0.06 ±0.00 | 56883 ±660 | 0.52 ±0.00 | 1.10 ±0.02 | 0 |
| deno | 19071 ±125 | 0.05 ±0.00 | 0.06 ±0.00 | 52552 ±260 | 0.57 ±0.00 | 1.80 ±0.01 | 0 |
| bun | 21970 ±46 | 0.04 ±0.00 | 0.06 ±0.00 | 79639 ±215 | 0.34 ±0.00 | 1.35 ±0.03 | 0 |

#### GET /posts (lowdb file read)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 10301 ±344 | 0.09 ±0.00 | 0.12 ±0.01 | 23270 ±259 | 1.22 ±0.01 | 4.61 ±0.06 | 0 |
| serve | 8714 ±55 | 0.11 ±0.00 | 0.13 ±0.00 | 22046 ±228 | 1.32 ±0.01 | 5.41 ±0.03 | 0 |
| wasmtime | 4998 ±23 | 0.18 ±0.00 | 0.40 ±0.00 | 23327 ±175 | 1.20 ±0.01 | 7.06 ±0.06 | 0 |
| spin | 2923 ±65 | 0.33 ±0.00 | 0.61 ±0.13 | 17297 ±1292 | 1.64 ±0.11 | 6.69 ±0.77 | 0 |
| node | 10579 ±21 | 0.09 ±0.00 | 0.11 ±0.00 | 30552 ±210 | 1.02 ±0.01 | 1.66 ±0.02 | 0 |
| deno | 10672 ±126 | 0.09 ±0.00 | 0.11 ±0.00 | 33210 ±293 | 0.90 ±0.01 | 1.87 ±0.06 | 0 |
| bun | 13775 ±55 | 0.07 ±0.00 | 0.08 ±0.00 | 55202 ±497 | 0.45 ±0.00 | 1.90 ±0.09 | 0 |

#### GET /export.ndjson (stream, 3 × 1 ms timer)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 135 ±6 | 7.39 ±0.38 | 8.05 ±0.41 | 3896 ±303 | 7.73 ±0.93 | 13.96 ±0.93 | 0 |
| serve | 132 ±6 | 7.46 ±0.38 | 8.28 ±0.37 | 4053 ±85 | 7.62 ±0.07 | 12.84 ±0.93 | 0 |
| wasmtime | 122 ±6 | 8.07 ±0.52 | 10.51 ±0.91 | 4371 ±101 | 7.14 ±0.22 | 10.11 ±0.42 | 0 |
| spin | 116 ±8 | 8.48 ±0.54 | 11.92 ±1.18 | 4237 ±78 | 7.36 ±0.16 | 10.47 ±0.14 | 0 |
| node | 257 ±12 | 3.86 ±0.22 | 4.39 ±0.23 | 5608 ±211 | 5.90 ±0.18 | 9.49 ±0.54 | 0 |
| deno | 137 ±6 | 7.38 ±0.38 | 7.93 ±0.39 | 3572 ±142 | 8.88 ±0.46 | 12.48 ±0.76 | 0 |
| bun | 260 ±12 | 3.76 ±0.22 | 4.97 ±0.07 | 9226 ±433 | 3.38 ±0.18 | 5.99 ±0.26 | 0 |

### `realtime`

#### GET /rooms/lobby/messages (JSON)

| Config | c=1 req/s | c=1 p50 ms | c=1 p99 ms | c=32 req/s | c=32 p50 ms | c=32 p99 ms | Errors |
|---|--:|--:|--:|--:|--:|--:|--:|
| listen | 14285 ±23 | 0.07 ±0.00 | 0.08 ±0.00 | 26907 ±391 | 1.10 ±0.02 | 3.75 ±0.03 | 0 |
| node | 18178 ±55 | 0.05 ±0.00 | 0.06 ±0.00 | 49855 ±198 | 0.56 ±0.00 | 3.04 ±0.01 | 0 |
| deno | 17275 ±20 | 0.06 ±0.00 | 0.06 ±0.00 | 45592 ±1272 | 0.65 ±0.01 | 2.03 ±0.06 | 0 |
| bun | 21237 ±348 | 0.04 ±0.00 | 0.06 ±0.01 | 79786 ±771 | 0.35 ±0.00 | 0.90 ±0.08 | 0 |
