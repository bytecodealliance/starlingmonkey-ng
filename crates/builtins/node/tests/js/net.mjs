// `node:net` sockets and servers on the loopback interface, and `node:dns` lookups that need no
// name server.

import net, { Server, Socket } from "node:net";
import dns from "node:dns";
import dnsPromises from "node:dns/promises";
import { Duplex } from "node:stream";
import { eq, deepEq, throws } from "./harness.mjs";

const once = (emitter, event) => new Promise((resolve) => emitter.once(event, (...args) => resolve(args)));
// Fail instead of hanging if `promise` doesn't settle within two seconds.
const within = (promise, what) =>
  Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${what} timed out`)), 2000).unref()),
  ]);
const closed = (socket) => (socket.destroyed ? Promise.resolve() : once(socket, "close"));

// A server that upper-cases what it receives, and ends with "!" once the client ends.
const accepted = [];
const server = net.createServer((socket) => {
  accepted.push(socket);
  socket.on("data", (data) => socket.write(data.toString().toUpperCase()));
  socket.on("end", () => socket.end("!"));
});
eq(server instanceof Server, true, "createServer");
eq(server.address(), null, "address before listen");
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { address, family, port } = server.address();
deepEq([address, family, port > 0], ["127.0.0.1", "IPv4", true], "server address");
throws(() => server.listen(0), "ERR_SERVER_ALREADY_LISTEN", "listening twice");

// Connecting, writing before the connection is up, half-closing.
{
  const client = net.connect(port, "127.0.0.1");
  eq(client instanceof Socket && client instanceof Duplex, true, "sockets are Duplexes");
  deepEq([client.connecting, client.pending, client.readyState], [true, true, "opening"], "connecting");
  client.write("hello");
  const received = [];
  client.on("data", (data) => received.push(data.toString()));
  await once(client, "connect");
  deepEq([client.remoteAddress, client.remotePort, client.readyState, client.pending], ["127.0.0.1", port, "open", false], "connected");
  eq(client.address().address, "127.0.0.1", "socket address");
  if (accepted.length === 0) await within(once(server, "connection"), "accepting");
  const [, connections] = await new Promise((resolve) => server.getConnections((...args) => resolve(args)));
  eq(connections, 1, "getConnections");
  client.end();
  const [hadError] = await once(client, "close");
  eq(hadError, false, "close after end has no error");
  eq(received.join(""), "HELLO!", "the server's replies, after the client ended");
  deepEq([client.bytesWritten, client.bytesRead], [5, 6], "byte counts");
  deepEq([client.remoteAddress, client.localPort, client.pending], ["127.0.0.1", undefined, true], "addresses after close");
  await within(closed(accepted[0]), "the server's socket closing");
  const [, open] = await new Promise((resolve) => server.getConnections((...args) => resolve(args)));
  eq(open, 0, "getConnections counts open connections");
}

// Idle timeouts and destroy.
{
  const client = net.createConnection({ port, host: "127.0.0.1" });
  await once(client, "connect");
  let timedOut = false;
  client.setTimeout(20, () => {
    timedOut = true;
    client.destroy();
  });
  await once(client, "close");
  eq(timedOut && client.destroyed, true, "setTimeout");
  throws(() => new Socket().setTimeout("x"), "ERR_INVALID_ARG_TYPE", "setTimeout needs a number");
  throws(() => new Socket().setTimeout(-1), "ERR_OUT_OF_RANGE", "setTimeout needs a non-negative number");
}

// Strings are written with `decodeStrings: false`.
{
  const client = net.connect({ port, host: "127.0.0.1", decodeStrings: false });
  const received = [];
  client.on("data", (data) => received.push(data.toString()));
  client.end("abc");
  await within(once(client, "close"), "a string write");
  eq(received.join(""), "ABC!", "decodeStrings: false");
}

// The end of the connection arrives without a consumer.
{
  const ended = [];
  const quiet = net.createServer({ allowHalfOpen: true }, (socket) => {
    socket.on("end", () => {
      ended.push(socket.allowHalfOpen);
      setTimeout(() => socket.end("late"), 10);
    });
  });
  await new Promise((resolve) => quiet.listen(0, "127.0.0.1", resolve));
  const client = net.connect({ port: quiet.address().port, host: "127.0.0.1" });
  const received = [];
  client.on("data", (data) => received.push(data.toString()));
  client.end();
  await within(once(client, "close"), "a half-open exchange");
  deepEq([ended, received.join("")], [[true], "late"], "allowHalfOpen servers");
  const closing = new Promise((resolve) => quiet.close(resolve));
  await within(closing, "closing a drained server");
}

// Errors.
{
  const inUse = net.createServer();
  const [listenError] = await new Promise((resolve) => {
    inUse.on("error", (...args) => resolve(args));
    inUse.listen(port, "127.0.0.1");
  });
  deepEq([listenError.code, listenError.syscall, listenError.port], ["EADDRINUSE", "listen", port], "listening on a used port");
  await within(new Promise((resolve) => server.close(resolve)), "closing the server");
  eq(server.listening, false, "close");
  const [notRunning] = await new Promise((resolve) => net.createServer().close((...args) => resolve(args)));
  eq(notRunning.code, "ERR_SERVER_NOT_RUNNING", "closing a server that isn't listening");
  const refused = net.connect(port, "127.0.0.1");
  const [error] = await once(refused, "error");
  deepEq([error.code, error.syscall, error.address, error.port], ["ECONNREFUSED", "connect", "127.0.0.1", port], "connection errors");
  eq(error.message, `connect ECONNREFUSED 127.0.0.1:${port}`, "connection error message");
  eq((await once(refused, "close"))[0], true, "close after an error");
  throws(() => net.connect(70000), "ERR_SOCKET_BAD_PORT", "ports");
  throws(() => net.connect(), "ERR_MISSING_ARGS", "connecting needs a port");
  throws(() => net.connect({ port: true }), "ERR_INVALID_ARG_TYPE", "port types");
  throws(() => net.createServer().listen({ path: "/tmp/socket" }), "ERR_METHOD_NOT_IMPLEMENTED", "IPC paths");
  throws(() => net.createServer().listen({}), "ERR_INVALID_ARG_VALUE", "listen options need a port");
  throws(() => net.createServer().listen(true), "ERR_INVALID_ARG_VALUE", "listen port types");
  throws(() => net.createServer().getConnections(1), "ERR_INVALID_ARG_TYPE", "getConnections needs a callback");
}

// Lookups.
{
  const [lookupError, lookedUp, lookedUpFamily] = await new Promise((resolve) => dns.lookup("localhost", (...args) => resolve(args)));
  eq(lookupError, null, "lookup");
  eq(net.isIP(lookedUp), lookedUpFamily, "lookup gives an address and its family");
  deepEq(await dnsPromises.lookup("127.0.0.1", { all: true }), [{ address: "127.0.0.1", family: 4 }], "literal addresses");
  eq(dns.promises.lookup, dnsPromises.lookup, "dns.promises");
  deepEq(await dnsPromises.lookup("::1", 4), { address: "::1", family: 6 }, "literal addresses keep their family");
  deepEq(await new Promise((resolve) => dns.lookup("", (...args) => resolve(args))), [null, null, 4], "empty host names");
  throws(() => dns.lookup(5, () => {}), "ERR_INVALID_ARG_TYPE", "host names");
  throws(() => dnsPromises.lookup(5), "ERR_INVALID_ARG_TYPE", "promise lookups throw synchronously");
  throws(() => dns.lookup("localhost", { family: "4" }, () => {}), "ERR_INVALID_ARG_VALUE", "family values");
  throws(() => dns.lookup("localhost", { all: 1 }, () => {}), "ERR_INVALID_ARG_TYPE", "all is a boolean");
  eq(dns.getDefaultResultOrder(), "verbatim", "getDefaultResultOrder");
  dns.setDefaultResultOrder("ipv4first");
  eq(dnsPromises.getDefaultResultOrder(), "ipv4first", "setDefaultResultOrder");
  dns.setDefaultResultOrder("verbatim");
  throws(() => dns.setDefaultResultOrder("bogus"), "ERR_INVALID_ARG_VALUE", "result orders");
}
