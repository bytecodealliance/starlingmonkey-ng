# Express applications

Express applications with realistic middleware stacks, and a Koa landing page that links to them,
each run under Node.js and under StarlingMonkey's `--node-compat` mode with the same scripted
requests, and compared response by response: status, the headers `run.mjs` records, and the body.

```sh
deps/node-bin/bin/node tests/express-apps/run.mjs                 # every app, natively
deps/node-bin/bin/node tests/express-apps/run.mjs auth data       # some apps
deps/node-bin/bin/node tests/express-apps/run.mjs --mode=serve    # with --serve
deps/node-bin/bin/node tests/express-apps/run.mjs --mode=wasm     # starling.wasm under wasmtime
deps/node-bin/bin/node tests/express-apps/run.mjs --mode=spin     # the Spin application
```

The driver installs an app's pinned dependencies with `npm ci` when its `node_modules` is
missing. It uses `target/release/starlingmonkey` unless `--runtime` names another binary, and
`target/wasm32-wasip3/release/starling.wasm` for `--mode=wasm`, which needs `wasmtime` on the
`PATH`. Each app writes its files to a new temporary directory, `DATA_DIR`, in each run.

## Apps

| App | Module format, Express | Covers |
|---|---|---|
| `koa-portal` | ES module, Koa 3 | the landing page: `@koa/router` with named routes and params, `koa-body` with urlencoded, JSON and multipart bodies and limits, `koa-static`, Handlebars views and partials compiled at startup, `koa-compress` with gzip, Brotli and zstd, `@koa/cors` with preflight, `koa-session` with signed cookies and flash messages, `koa-conditional-get` and `@koa/etag`, error handling with `ctx.throw` |
| `rest-api` | CommonJS, Express 4 | `express.Router` with params and `route()`, `express.json` and `express.text` with limits, zod validation, async handlers, error middleware, `cors` with preflight, `helmet`, `compression` (gzip and deflate), `morgan` logging to a stream, `express-rate-limit`, `uuid` v4 and v5, content negotiation with `res.format`, ETags and conditional requests |
| `ssr-site` | CommonJS, Express 4 | EJS views with includes and Pug views with layouts, mixins and includes, `express.static` with ETags, `Last-Modified`, 304s, single, suffix and multiple ranges and `If-Range`, dotfiles and path traversal, `cookie-parser` with signed and JSON cookies, `express-session` with `MemoryStore`, session regeneration and destruction, flash messages, CSRF tokens, urlencoded form posts, `method-override`, `res.download`, `res.sendFile`, redirects, `trust proxy` |
| `files` | ES module, Express 5 | `multer` with disk and memory storage, `single`, `array`, `fields`, `any` and `none`, limits and file filters, `busboy` used directly, streamed request bodies piped to disk, `res.sendFile` with ranges, `res.download`, generated streams with backpressure, gzip of streamed files, gzipped request bodies, `PassThrough` responses |
| `auth` | ES module, Express 5 | `jsonwebtoken` HS256, HS384 and HS512 tokens with expiry, issuer and audience checks, `bcryptjs` hashes, `passport` with `passport-local`, `express-validator`, refresh-token rotation, `node:crypto`'s `scrypt`, `pbkdf2`, `hkdf`, `createSecretKey`, `createHmac` and `timingSafeEqual` |
| `data` | ES module, Express 5 | a `lowdb` JSON file store, server-sent events fed by other requests, NDJSON export and import, proxies with `http-proxy-middleware` and with the classic `http-proxy`, `fetch` and `http.get` to an upstream, `Readable.fromWeb`, `winston` logging to a file and a stream, `pino`, `dotenv`, `AsyncLocalStorage` |
| `realtime` | CommonJS, Express 4 | WebSockets with `ws` on the HTTP server's `upgrade` event, `cookie-session` read during the upgrade, rooms and broadcasts between WebSocket and HTTP clients, binary messages, payload limits and close codes, long polling |

## The Spin application

`scripts/build-spin-app.sh` builds `target/spin-app/`, a [Spin](https://spinframework.dev)
application with a component for each app but `realtime`: the Koa portal at `/...`, and each
Express app at `/<app>/...`. Spin 4.2 runs the WASIp3 components `starling-componentize` builds.
`scripts/fetch-spin.sh` downloads it to `deps/spin-bin`.

```sh
./scripts/build-spin-app.sh
deps/spin-bin/spin up --from target/spin-app/spin.toml --allow-transient-write
deps/node-bin/bin/node tests/express-apps/check-portal-links.mjs http://127.0.0.1:3000
```

- An Express app's component runs its `spin.mjs`, which mounts the app at its route. Spin passes
  the whole path to the component, so the apps make their links and redirects relative to
  `req.baseUrl`.
- Each app's top level runs when it is componentized, so the components start from a snapshot of
  the configured app, with its modules loaded and, for the portal and `ssr-site`, its views
  compiled. The apps that write files at their top level get a read-write `/data` then, through
  `starling-componentize --init-dir`, which becomes the component's `/data` mount.
- `ssr-site` is componentized with `--allow-blocking-host-io`, for EJS's `fs.existsSync`.
- The data app's upstream server, `data/upstream.mjs`, is a private component, which the data
  app reaches at `http://upstream.spin.internal`.
- Spin copies the mounted files for each `spin up`, which `--allow-transient-write` makes
  writable, and leaves empty directories out, so the files app creates its upload directory for
  each request.

`run.mjs --mode=spin` starts the application with `spin up` for each app, and sends the scenario's
requests to the app's route. Node.js runs the app's `spin.mjs` then, so the app is mounted at the
same path. `check-portal-links.mjs` requests every link of the portal's pages, and submits every
form.

## Benchmarks

`bench.mjs` measures a few routes of each app under StarlingMonkey, Node.js, Deno and Bun with
[`oha`](https://github.com/hatoo/oha), which must be on the `PATH`. `docs/express-benchmarks.md`
has the results and their analysis.

```sh
deps/node-bin/bin/node tests/express-apps/bench.mjs                      # every app and config
deps/node-bin/bin/node tests/express-apps/bench.mjs --apps=rest-api --configs=listen,node
deps/node-bin/bin/node tests/express-apps/bench.mjs --report=target/express-bench/results.json
```

- The configs are `listen` (`--node-compat --node-http-listen`), `serve` (`--node-compat
  --serve`), `wasmtime` (the app's component under `wasmtime serve`), `spin` (the Spin
  application under `spin up`), `node`, `deno` (with `--unstable-detect-cjs`) and `bun`.
  `wasmtime` and `spin` use the components `scripts/build-spin-app.sh` builds, with the default
  instance reuse of both: 128 requests per instance, and up to 16 at once. `--reuse=N` sets the
  requests per instance. `--runtime` and `--spin-app` name another `starlingmonkey` binary and
  another directory of components, to compare builds.
- Every config runs the same entry, so the apps do the same work: an Express app's `spin.mjs`,
  which mounts it at `/<app>`, the portal's `app.mjs`, and `realtime`'s `app.js`. `realtime` runs
  natively only.
- Each route runs for `--warmup` seconds (1 by default) at the highest concurrency, then for
  `--duration` seconds (5) at each `--concurrency` (1 and 32). Each repeat (`--repeat`, 3) starts
  every app under every config once, in turn. Routes that add state to the app, such as the
  `POST`s that store items, run last. A response with another status than the route's is an
  error.
- Startup is measured in `--startup-runs` (5) separate processes: the time until the server
  accepts a TCP connection, and the latency of the first request after that. The first request
  of a component instantiates it.
- RSS is the sum over the server's process tree, after the first request and at its maximum
  during the run, sampled every 500 ms.
- The results go to `target/express-bench/results.json` unless `--out` names another file.
  `--report` prints a results file's tables: the median of the repeats, with half the range
  between the smallest and the largest as `±`.

## Differences that are not bugs

The driver ignores these:

- Values that differ between runs: session IDs, CSRF tokens, the modification times in the ETags
  of uploaded files, dates and the app's port, which each scenario masks.
- With `--serve`, the host frames responses: a body Node.js sends chunked, because the app wrote
  the head before the body, can have a `Content-Length`, and a `204` response has no
  `Content-Length`, which RFC 9110 forbids, although Node.js sends the one the app set.
- On wasm, wasmtime's HTTP client leaves `Transfer-Encoding` out of the responses it receives, so
  a proxied response that was chunked upstream can have a `Content-Length` instead.
- Under Spin, the static files are copies with new modification times, which their ETags hold,
  the data app's upstream is `upstream.spin.internal`, and requests have no client address, so
  `req.ip` is undefined and `trust proxy` trusts no proxy. The scenarios mask these or leave the
  responses out with `ignoreIn`.

## Limitations

These are out of scope, or need more than these apps do:

- With `--serve`, synchronous file system calls throw `ERR_BLOCKING_NOT_ALLOWED` while requests
  are served (see "Synchronous host I/O" in `docs/node-compat.md`). `ssr-site` compiles its views
  at startup, but EJS checks that an included template exists with `fs.existsSync` on every
  render, so `ssr-site` is served with `--allow-blocking-host-io`. The other apps use the
  asynchronous forms.
- With `--serve`, served requests have no socket, so WebSockets and other upgrades can't work, and
  `realtime` only runs natively and on wasm. A served request's work ends when its response is
  sent: timers and `http.request`s that the handler left running are abandoned. Pending `node:fs`
  operations and response bodies that are still being written keep the request running.
- `http.request` can't upgrade connections, since the host manages them, so WebSocket clients
  such as `ws`'s don't work. WebSocket servers do, with `--node-http-listen`.
- TLS: `https.createServer` doesn't accept TLS connections itself, and the client's TLS options
  are ignored, since the host terminates TLS.
- `node:crypto` has no asymmetric keys, signatures or ciphers, so `jsonwebtoken`'s RS*, PS* and
  ES* algorithms, and anything else that needs OpenSSL, fail.
- Native addons (`.node` files), such as `bcrypt` and `sharp`, can't be loaded. Pure JavaScript
  replacements, such as `bcryptjs`, work.
- Child processes, worker threads and `cluster` aren't available.
