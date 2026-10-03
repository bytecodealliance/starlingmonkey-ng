// `node:http` and `node:https` clients against the test's local server, whose port is
// `globalThis.HTTP_PORT`, and the parts of servers that work without serving.

import http, { Agent, ClientRequest, IncomingMessage, OutgoingMessage, Server, ServerResponse } from "node:http";
import https from "node:https";
import { Readable, Writable } from "node:stream";
import { eq, deepEq, throws } from "./harness.mjs";

const base = `http://127.0.0.1:${globalThis.HTTP_PORT}`;
const read = (response) =>
  new Promise((resolve, reject) => {
    const chunks = [];
    response.on("data", (chunk) => chunks.push(chunk));
    response.on("end", () => resolve(Buffer.concat(chunks).toString()));
    response.on("error", reject);
  });
const respond = (request) =>
  new Promise((resolve, reject) => {
    request.on("response", resolve);
    request.on("error", reject);
  });

eq(new IncomingMessage() instanceof Readable, true, "IncomingMessage is a Readable");
eq(new ServerResponse() instanceof OutgoingMessage, true, "ServerResponse extends OutgoingMessage");
eq(http.globalAgent instanceof Agent && https.globalAgent instanceof https.Agent, true, "global agents");
eq(new https.Agent().defaultPort, 443, "https agents default to port 443");
eq(http.STATUS_CODES[418], "I'm a Teapot", "STATUS_CODES");

// GET, response headers and duplicates.
{
  const response = await new Promise((resolve) => http.get(`${base}/echo?x=1`, resolve));
  eq(response instanceof IncomingMessage, true, "responses are IncomingMessages");
  deepEq([response.statusCode, response.statusMessage], [200, "OK"], "status");
  deepEq(response.headers["set-cookie"], ["a=1", "b=2"], "set-cookie is an array");
  eq(response.headers["x-dup"], "1, 2", "other duplicates are joined");
  eq(response.rawHeaders.filter((_, index) => index % 2 === 0).filter((name) => name.toLowerCase() === "x-dup").length, 2, "rawHeaders keeps duplicates");
  const echoed = JSON.parse(await read(response));
  deepEq([echoed.method, echoed.path], ["GET", "/echo?x=1"], "request line");
  eq(response.complete, true, "complete after the body");
}

// POST with options, headers and a body written in pieces.
{
  const request = http.request({
    host: "127.0.0.1",
    port: globalThis.HTTP_PORT,
    path: "/submit",
    method: "post",
    headers: { "Content-Type": "text/plain", "X-Many": ["a", "b"] },
    auth: "user:pass",
  });
  eq(request instanceof ClientRequest && request instanceof Writable === false, true, "ClientRequest is an OutgoingMessage");
  eq(request.getHeader("content-type"), "text/plain", "getHeader");
  request.setHeader("X-Later", 5);
  deepEq(request.getHeaderNames().sort(), ["authorization", "content-type", "x-later", "x-many"], "getHeaderNames");
  request.write("hello ");
  request.end("world");
  eq(request.headersSent, true, "headersSent after write");
  throws(() => request.setHeader("X-Too-Late", "1"), "ERR_HTTP_HEADERS_SENT", "setHeader after sending");
  const echoed = JSON.parse(await read(await respond(request)));
  const headers = Object.fromEntries(echoed.headers.map(([name, value]) => [name.toLowerCase(), value]));
  deepEq([echoed.method, echoed.path, echoed.body], ["POST", "/submit", "hello world"], "method, path and body");
  eq(headers["x-later"], "5", "numeric header values");
  eq(headers.authorization, `Basic ${Buffer.from("user:pass").toString("base64")}`, "auth");
  eq(echoed.headers.filter(([name]) => name.toLowerCase() === "x-many").length, 2, "array values send one header each");
}

// URL inputs, streamed responses and redirects.
{
  const request = http.request(new URL(`${base}/chunked`), { method: "GET" });
  request.end();
  const response = await respond(request);
  let chunks = 0;
  response.on("data", () => chunks++);
  eq(await read(response), "onetwothree", "chunked bodies");
  eq(chunks >= 1, true, "data events");
  const redirect = await new Promise((resolve) => http.get(`${base}/redirect`, resolve));
  deepEq([redirect.statusCode, redirect.headers.location], [302, "/echo"], "redirects aren't followed");
  redirect.resume();
}

// Timeouts, destroy and connection errors.
{
  const slow = http.get(`${base}/slow`);
  const events = [];
  slow.setTimeout(50, () => {
    events.push("timeout");
    slow.destroy();
  });
  slow.on("error", (error) => events.push(error.code));
  await new Promise((resolve) => slow.on("close", resolve));
  deepEq(events, ["timeout", "ECONNRESET"], "setTimeout, then destroy without an error");
  const refused = http.get(`http://127.0.0.1:${globalThis.REFUSED_PORT}/`);
  const refusedError = await new Promise((resolve) => refused.on("error", resolve));
  eq(refusedError.code, "ECONNREFUSED", "connection errors");
  throws(() => http.request("ftp://example.com/"), "ERR_INVALID_PROTOCOL", "protocols");
  throws(() => http.request(`https://127.0.0.1:${globalThis.HTTP_PORT}/`), "ERR_INVALID_PROTOCOL", "http requests can't use https:");
  throws(() => http.request({ host: "127.0.0.1", path: "/\u20ac" }), "ERR_UNESCAPED_CHARACTERS", "paths");
}

// Agents, request targets and late response listeners.
{
  const request = http.get(`${base}/echo`);
  eq(request.agent, http.globalAgent, "requests use the global agent");
  const response = await respond(request);
  await new Promise((resolve) => setTimeout(resolve, 20));
  eq(JSON.parse(await read(response)).path, "/echo", "a response read after the callback returned");
  const target = http.request({ host: "127.0.0.1", port: globalThis.HTTP_PORT, path: "@example.invalid/x" });
  target.end();
  const echoed = JSON.parse(await read(await respond(target)));
  eq(echoed.path.endsWith("@example.invalid/x"), true, "a path can't change the host");
}

// https defaults, without sending.
{
  const request = https.request({ host: "127.0.0.1", port: 1 });
  eq(request.protocol, "https:", "https requests default to https:");
  request.on("error", () => {});
  request.destroy();
}

// Servers, which can't serve when the script runs as a command.
{
  const server = http.createServer(() => {});
  eq(server instanceof Server, true, "createServer");
  eq(server.address(), null, "address before listen");
  const error = await new Promise((resolve) => server.listen(8080).on("error", resolve));
  eq(error.code, "ERR_SERVE_MODE_REQUIRED", "listen needs serve mode");

  const response = new ServerResponse({ method: "GET" });
  eq(response.writeHead(201, "Made", { "X-A": "1" }), response, "writeHead returns the response");
  deepEq([response.statusCode, response.statusMessage, response.getHeader("x-a"), response.headersSent], [201, "Made", "1", true], "writeHead");
  throws(() => response.removeHeader("x-a"), "ERR_HTTP_HEADERS_SENT", "removeHeader after writeHead");
  throws(() => new ServerResponse().setHeader("bad name", "x"), "ERR_INVALID_HTTP_TOKEN", "header names");
  const finished = new Promise((resolve) => response.on("finish", resolve));
  response.end("ignored");
  await finished;
  eq(response.writableFinished, true, "finish");
  const lateErrors = [];
  response.on("error", (error) => lateErrors.push(error.code));
  response.write("more", (error) => lateErrors.push(error.code));
  await new Promise((resolve) => setTimeout(resolve, 0));
  deepEq(lateErrors, ["ERR_STREAM_WRITE_AFTER_END", "ERR_STREAM_WRITE_AFTER_END"], "write after end");

  const headers = new ServerResponse({ method: "GET" });
  headers.setHeader("a", ["1"]);
  headers.appendHeader("A", "2");
  headers.appendHeader("a", ["3", "4"]);
  deepEq(headers.getHeader("a"), ["1", "2", "3", "4"], "appendHeader");
  throws(() => headers.getHeader(5), "ERR_INVALID_ARG_TYPE", "header names are strings");
  eq(headers.getHeader("a\0b"), undefined, "names that can't be headers");
  headers.setHeader("X-A", "1");
  headers.writeHead(200.5, ["X-A", "2", "x-a", "3"]);
  deepEq([headers.statusCode, headers.getHeader("x-a")], [200, ["2", "3"]], "writeHead truncates the status and replaces headers");
  throws(() => new ServerResponse({}).writeHead(200, ["X-A"]), "ERR_INVALID_ARG_VALUE", "header arrays have pairs");
  throws(() => http.validateHeaderValue("x", Symbol()), TypeError, "header values convert to strings");
}
