// `http.Server` accepting connections itself, as with `--node-http-listen`: requests from
// `http.request` and raw requests over `net` sockets, the response wire format, keep-alive and
// pipelining, `Expect: 100-continue`, `upgrade` and `clientError`.

import http, { IncomingMessage, Server, ServerResponse } from "node:http";
import net from "node:net";
import { eq, deepEq } from "./harness.mjs";

const once = (emitter, event) => new Promise((resolve) => emitter.once(event, (...args) => resolve(args)));
// Fail instead of hanging if `promise` doesn't settle within five seconds.
const within = (promise, what) =>
  Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${what} timed out`)), 5000).unref()),
  ]);
const listen = async (server) => {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return server.address().port;
};
const read = (stream) =>
  new Promise((resolve, reject) => {
    const chunks = [];
    stream.on("data", (chunk) => chunks.push(chunk));
    stream.on("end", () => resolve(Buffer.concat(chunks).toString("latin1")));
    stream.on("error", reject);
  });
// The bytes a server sends back for the raw request `request`, until it closes the connection,
// with the `Date` header's value replaced by `-`.
const exchange = async (port, request) => {
  const socket = net.connect(port, "127.0.0.1");
  await once(socket, "connect");
  socket.write(request);
  const chunks = [];
  socket.on("data", (chunk) => chunks.push(chunk));
  await within(once(socket, "close"), "raw exchange");
  return Buffer.concat(chunks).toString("latin1").replace(/Date: [^\r]*\r\n/g, "Date: -\r\n");
};

eq(new Server() instanceof net.Server, true, "http.Server extends net.Server");

// A request from `http.request`, with a body, and a response with set and written headers.
{
  const server = http.createServer(async (req, res) => {
    const body = await read(req);
    res.setHeader("X-Set", ["a", "b"]);
    res.statusCode = 201;
    res.setHeader("X-Mixed-Case", "1");
    res.end(JSON.stringify({
      method: req.method,
      url: req.url,
      version: req.httpVersion,
      body,
      dup: req.headers["x-dup"],
      raw: req.rawHeaders.filter((_, index) => index % 2 === 0).map((name) => name.toLowerCase()).filter((name) => name.startsWith("x-")),
      complete: req.complete,
      incoming: req instanceof IncomingMessage && res instanceof ServerResponse,
    }));
  });
  const port = await listen(server);
  eq(typeof port === "number" && port > 0, true, "address() reports the port of listen(0)");
  const request = http.request({
    host: "127.0.0.1",
    port,
    method: "POST",
    path: "/submit?x=1",
    headers: { "X-Dup": ["1", "2"], "X-Case": "y" },
  });
  request.end("hello");
  const [response] = await within(once(request, "response"), "response");
  const echoed = JSON.parse(await read(response));
  deepEq(
    [echoed.method, echoed.url, echoed.version, echoed.body, echoed.dup, echoed.raw, echoed.complete, echoed.incoming],
    ["POST", "/submit?x=1", "1.1", "hello", "1, 2", ["x-dup", "x-dup", "x-case"], true, true],
    "the request the server parsed",
  );
  deepEq([response.statusCode, response.statusMessage], [201, "Created"], "status");
  deepEq(response.headers["x-set"], "a, b", "array header values are separate lines");
  eq(response.headers["x-mixed-case"], "1", "set headers");
  eq(response.headers["content-length"], String(Buffer.byteLength(JSON.stringify(echoed))), "end() with the whole body sets Content-Length");
  server.close();
  await within(once(server, "close"), "server close");
}

// The wire format of responses to raw requests: chunked bodies, HEAD, 204, HTTP/1.0, pipelining
// and trailers.
{
  const server = http.createServer((req, res) => {
    switch (req.url) {
      case "/chunked":
        res.write("one");
        res.addTrailers({ "X-Trailer": "t" });
        res.end("two");
        break;
      case "/raw":
        res.end(JSON.stringify([req.rawHeaders, req.headers]));
        break;
      case "/empty":
        res.writeHead(204, { "X-A": "1" });
        res.end("dropped");
        break;
      default:
        res.end(`${req.method} ${req.url} ${req.httpVersion}`);
    }
  });
  const port = await listen(server);
  eq(
    await exchange(port, "GET /chunked HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    "HTTP/1.1 200 OK\r\nDate: -\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n3\r\ntwo\r\n0\r\nX-Trailer: t\r\n\r\n",
    "chunked responses and trailers",
  );
  const raw = await exchange(port, "GET /raw HTTP/1.1\r\nHost: x\r\nX-Dup: 1\r\nx-DUP: 2\r\nCookie: a\r\ncookie: b\r\nAge: 1\r\nage: 2\r\nConnection: close\r\n\r\n");
  deepEq(
    JSON.parse(raw.slice(raw.indexOf("\r\n\r\n") + 4)),
    [
      ["Host", "x", "X-Dup", "1", "x-DUP", "2", "Cookie", "a", "cookie", "b", "Age", "1", "age", "2", "Connection", "close"],
      { host: "x", "x-dup": "1, 2", cookie: "a; b", age: "1", connection: "close" },
    ],
    "rawHeaders keep the names as sent, and headers join duplicates as Node.js does",
  );
  eq(
    await exchange(port, "HEAD /head HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    "HTTP/1.1 200 OK\r\nDate: -\r\nConnection: close\r\n\r\n",
    "HEAD responses have no body",
  );
  eq(
    await exchange(port, "GET /empty HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    "HTTP/1.1 204 No Content\r\nX-A: 1\r\nDate: -\r\nConnection: close\r\n\r\n",
    "204 responses have no body",
  );
  eq(
    await exchange(port, "GET /old HTTP/1.0\r\n\r\n"),
    "HTTP/1.1 200 OK\r\nDate: -\r\nConnection: close\r\n\r\nGET /old 1.0",
    "HTTP/1.0 requests close the connection",
  );
  eq(
    await exchange(port, "GET /1 HTTP/1.1\r\nHost: x\r\n\r\nPOST /2 HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nab\r\n0\r\n\r\n"),
    "HTTP/1.1 200 OK\r\nDate: -\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nContent-Length: 10\r\n\r\nGET /1 1.1" +
      "HTTP/1.1 200 OK\r\nDate: -\r\nConnection: close\r\nContent-Length: 11\r\n\r\nPOST /2 1.1",
    "pipelined requests get their responses in order",
  );
  eq(
    await exchange(port, "GET / HTTP/1.1\r\n\r\n"),
    "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nDate: -\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    "HTTP/1.1 requests need a Host header",
  );
  server.close();
}

// `Expect: 100-continue`, `upgrade` and `clientError`.
{
  const server = http.createServer(async (req, res) => {
    res.end(await read(req));
  });
  const errors = [];
  server.on("clientError", (error, socket) => {
    errors.push([error.code, error.bytesParsed, String(error.rawPacket)]);
    socket.end("HTTP/1.1 400 Bad Request\r\n\r\n");
  });
  server.on("upgrade", (req, socket, head) => {
    socket.end(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: ${req.headers.upgrade}\r\n\r\n${head}`);
  });
  const port = await listen(server);
  eq(
    await exchange(port, "POST / HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"),
    "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nDate: -\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok",
    "100 Continue before the response",
  );
  eq(
    await exchange(port, "GET / HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: test\r\n\r\nrest"),
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: test\r\n\r\nrest",
    "upgrade gets the socket and the bytes after the head",
  );
  eq(await exchange(port, "FOO / HTTP/1.1\r\n\r\n"), "HTTP/1.1 400 Bad Request\r\n\r\n", "clientError");
  deepEq(errors, [["HPE_INVALID_METHOD", 1, "FOO / HTTP/1.1\r\n\r\n"]], "the parse error");
  server.close();
  await within(once(server, "close"), "server close");
}

// A client request emits `socket` in a later tick, and a proxy that pipes the request body from
// its `socket` listener, as httpxy does, completes.
{
  const upstream = http.createServer(async (req, res) => res.end(`upstream got ${await read(req)}`));
  const upstreamPort = await listen(upstream);
  const proxy = http.createServer((req, res) => {
    const proxyReq = http.request({ port: upstreamPort, host: "127.0.0.1", method: req.method, path: req.url });
    proxyReq.on("socket", (socket) => {
      if (socket.pending) socket.on("connect", () => req.pipe(proxyReq));
      else req.pipe(proxyReq);
    });
    proxyReq.on("response", (proxyRes) => proxyRes.pipe(res));
  });
  const proxyPort = await listen(proxy);
  const events = [];
  const req = http.request({ port: proxyPort, host: "127.0.0.1", method: "POST" });
  eq(req.socket, null, "no socket before the socket event");
  req.on("socket", (socket) => {
    events.push(["socket", socket === req.socket, socket.setNoDelay(true) === socket]);
  });
  process.nextTick(() => events.push(["tick"]));
  req.end("hello");
  const [res] = await within(once(req, "response"), "proxied response");
  eq(res.socket, req.socket, "the response has the request's socket");
  eq(await read(res), "upstream got hello", "proxied body");
  deepEq(events, [["socket", true, true], ["tick"]], "socket event");
  proxy.close();
  upstream.close();
}

// A `keepAlive` agent reuses the socket of a finished response for the next request to the same
// host and port.
{
  const server = http.createServer((req, res) => res.end("ok"));
  const port = await listen(server);
  const agent = new http.Agent({ keepAlive: true });
  const get = () =>
    new Promise((resolve) =>
      http.get({ port, host: "127.0.0.1", agent }, (res) => {
        res.resume();
        res.on("end", () => process.nextTick(() => resolve([res.socket, res.req.reusedSocket])));
      }),
    );
  const [first, firstReused] = await within(get(), "first keep-alive request");
  const [second, secondReused] = await within(get(), "second keep-alive request");
  deepEq([first === second, firstReused, secondReused], [true, false, true], "socket reuse");
  server.close();
}
