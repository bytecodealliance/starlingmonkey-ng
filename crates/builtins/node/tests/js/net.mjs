// `node:net` sockets and servers on the loopback interface, socket addresses and block lists, and
// `node:dns` lookups that need no name server.

import net, { Server, Socket } from "node:net";
import dns from "node:dns";
import dnsPromises from "node:dns/promises";
import { Duplex } from "node:stream";
import { inspect } from "node:util";
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
  throws(() => net.connect({ port: 42, hints: 3370 }), "ERR_INVALID_ARG_VALUE", "connect hints");
  throws(() => net.connect({ port: 1, host: "localhost", family: 5 }), "ERR_INVALID_ARG_VALUE", "connect families");
  net.connect({ port: 1, host: "127.0.0.1", family: 5 }).on("error", () => {}).destroy();
  throws(() => net.connect({ port: 1, host: "127.0.0.1", autoSelectFamily: "x" }), "ERR_INVALID_ARG_TYPE", "autoSelectFamily");
  throws(() => net.connect({ port: 1, host: "127.0.0.1", autoSelectFamilyAttemptTimeout: 0 }), "ERR_OUT_OF_RANGE", "autoSelectFamilyAttemptTimeout");
  throws(() => net.setDefaultAutoSelectFamily("x"), "ERR_INVALID_ARG_TYPE", "setDefaultAutoSelectFamily");
  throws(() => net.setDefaultAutoSelectFamilyAttemptTimeout(1.5), "ERR_OUT_OF_RANGE", "setDefaultAutoSelectFamilyAttemptTimeout integers");
  throws(() => net.setDefaultAutoSelectFamilyAttemptTimeout(-5), "ERR_OUT_OF_RANGE", "setDefaultAutoSelectFamilyAttemptTimeout range");
  const timeout = net.getDefaultAutoSelectFamilyAttemptTimeout();
  net.setDefaultAutoSelectFamilyAttemptTimeout(5);
  eq(net.getDefaultAutoSelectFamilyAttemptTimeout(), 10, "attempt timeouts below 10 become 10");
  net.setDefaultAutoSelectFamilyAttemptTimeout(timeout);
  const listener = () => {};
  deepEq(net._normalizeArgs({}), [{}, null], "_normalizeArgs of an object without a length");
  deepEq(net._normalizeArgs({ length: "x" }), [{}, null], "_normalizeArgs of a non-numeric length");
  deepEq(net._normalizeArgs({ length: -1, 0: { a: 1 } }), [{ a: 1 }, null], "_normalizeArgs ignores a negative length");
  deepEq(net._normalizeArgs([80, "h"]), [{ port: 80, host: "h" }, null], "_normalizeArgs of a port and a host");
  eq(net._normalizeArgs({ length: "2", 0: 80, 1: listener })[1], listener, "_normalizeArgs of a string length");
  throws(() => net._normalizeArgs({ length: 2n }), TypeError, "_normalizeArgs of a BigInt length");
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
  const warned = new Promise((resolve) => process.once("warning", resolve));
  deepEq(await new Promise((resolve) => dns.lookup("", (...args) => resolve(args))), [null, null, 4], "empty host names");
  const warning = await within(warned, "the empty host name warning");
  deepEq(
    [warning.name, warning.code, warning.message],
    ["DeprecationWarning", "DEP0118", 'The provided hostname "" is not a valid hostname, and is supported in the dns module solely for compatibility.'],
    "empty host name warning",
  );
  Object.defineProperty(Object.prototype, "address", { set() { throw new Error("setter called"); }, configurable: true });
  try {
    deepEq(await dnsPromises.lookup("127.0.0.1"), { address: "127.0.0.1", family: 4 }, "results don't call setters");
  } finally {
    delete Object.prototype.address;
  }
  throws(() => dns.lookup(5, () => {}), "ERR_INVALID_ARG_TYPE", "host names");
  throws(() => dnsPromises.lookup(5), "ERR_INVALID_ARG_TYPE", "promise lookups throw synchronously");
  throws(() => dns.lookup("localhost", { family: "4" }, () => {}), "ERR_INVALID_ARG_VALUE", "family values");
  throws(() => dns.lookup("localhost", { all: 1 }, () => {}), "ERR_INVALID_ARG_TYPE", "all is a boolean");
  const mapped = await dnsPromises.lookup("localhost", { family: 6, hints: dns.V4MAPPED | dns.ALL, all: true });
  eq(mapped.every((entry) => entry.family === 6), true, "V4MAPPED gives IPv6 addresses");
  eq(mapped.at(-1).address, "::ffff:127.0.0.1", "V4MAPPED with ALL adds the mapped IPv4 addresses");
  const v6 = await dnsPromises.lookup("localhost", { family: 6, hints: dns.V4MAPPED, all: true });
  eq(v6.length > 0 && v6.every((entry) => entry.family === 6), true, "V4MAPPED alone gives IPv6 addresses");
  eq(dns.getDefaultResultOrder(), "verbatim", "getDefaultResultOrder");
  dns.setDefaultResultOrder("ipv4first");
  eq(dnsPromises.getDefaultResultOrder(), "ipv4first", "setDefaultResultOrder");
  dns.setDefaultResultOrder("verbatim");
  throws(() => dns.setDefaultResultOrder("bogus"), "ERR_INVALID_ARG_VALUE", "result orders");
}

// Socket addresses and block lists.
{
  const { BlockList, SocketAddress } = net;
  deepEq(new SocketAddress().toJSON(), { address: "127.0.0.1", port: 0, family: "ipv4", flowlabel: 0 }, "SocketAddress defaults");
  const mapped = new SocketAddress({ address: "0::ffff:102:304", family: "IPv6", port: "80", flowlabel: 5 });
  deepEq([mapped.address, mapped.port, mapped.family, mapped.flowlabel], ["::ffff:1.2.3.4", 80, "ipv6", 5], "IPv6 addresses");
  deepEq(["::0.0.0.1", "1::1.2.3.4", "::1.2.3.4", "1:2:3:4:5:6:7::", "fe80::1%eth0"].map((address) => new SocketAddress({ address, family: "ipv6" }).address),
    ["::1", "1::102:304", "::1.2.3.4", "1:2:3:4:5:6:7:0", "fe80::1"], "IPv6 formatting");
  throws(() => new SocketAddress({ address: "01.2.3.4" }), "ERR_INVALID_ADDRESS", "IPv4 parsing");
  throws(() => new SocketAddress({ family: "IPV4" }), "ERR_INVALID_ADDRESS", "the default address follows the family as given");
  throws(() => new SocketAddress({ family: "foo" }), "ERR_INVALID_ARG_VALUE", "families");
  throws(() => new SocketAddress([]), "ERR_INVALID_ARG_TYPE", "options");
  throws(() => new SocketAddress({ port: -1 }), "ERR_SOCKET_BAD_PORT", "SocketAddress ports");
  throws(() => new SocketAddress({ flowlabel: -1 }), "ERR_OUT_OF_RANGE", "flow labels");
  let called;
  try { SocketAddress(); } catch (error) { called = error.message; }
  eq(called, "Class constructor SocketAddress cannot be invoked without 'new'", "class constructors need new");
  const parsed = [["1.2.3.4", "1.2.3.4", 0], ["192.168.257:1", "192.168.1.1", 1], ["0xffffffff", "255.255.255.255", 0], ["[1::8]:123", "1::8", 123], ["1.2.3.4:80", "1.2.3.4", 0]];
  for (const [input, address, port] of parsed) deepEq([SocketAddress.parse(input)?.address, SocketAddress.parse(input)?.port], [address, port], `parse(${input})`);
  deepEq(["not an ip", "abc.123", "259.1.1.1", "12:12:12"].map((input) => SocketAddress.parse(input)), [undefined, undefined, undefined, undefined], "parse failures");
  eq(SocketAddress.isSocketAddress(mapped) && !SocketAddress.isSocketAddress({}), true, "isSocketAddress");

  const list = new BlockList();
  list.addAddress("1.1.1.1");
  list.addAddress("::ffff:1.1.1.2", "ipv6");
  list.addRange("10.0.0.1", "10.0.0.10");
  list.addSubnet("8592:757c:efae:4e45::", 64, "IpV6");
  list.addCIDR("192.168.0.0/16");
  deepEq(list.rules, ["Address: IPv4 1.1.1.1", "Address: IPv6 ::ffff:1.1.1.2", "Subnet: IPv4 192.168.0.0/16", "Subnet: IPv6 8592:757c:efae:4e45::/64", "Range: IPv4 10.0.0.1-10.0.0.10"], "rules");
  eq(list.size, 5, "size");
  deepEq([list.check("1.1.1.1"), list.check("::ffff:1.1.1.1", "IPV6"), list.check("1.1.1.2"), list.check("::ffff:10.0.0.5", "ipv6"), list.check("8592:757c:efae:4e45::f", "ipv6"), list.check("192.168.3.4"), list.check("1.1.1.1", "ipv6"), list.check("not an ip")],
    [true, true, true, true, true, true, false, false], "check");
  throws(() => list.addRange("1.1.1.2", "1.1.1.1"), "ERR_INVALID_ARG_VALUE", "ranges are ordered");
  throws(() => list.addSubnet("1.1.1.1", 33), "ERR_OUT_OF_RANGE", "prefix lengths");
  throws(() => list.addCIDR("10.0.0.0/08"), "ERR_INVALID_ARG_VALUE", "CIDR prefixes");
  throws(() => list.addCIDRs(["10.0.0.0/8", "bad"]), "ERR_INVALID_ARG_VALUE", "addCIDRs validates first");
  eq(list.size, 5, "a failed addCIDRs adds nothing");
  const copy = new BlockList();
  copy.fromJSON(JSON.stringify(list));
  // As in Node.js, `fromJSON` reads `::ffff:1.1.1.2` as `::ffff:1`, and adds the subnets in the
  // order given, so they are listed in reverse.
  deepEq(copy.rules, ["Address: IPv4 1.1.1.1", "Address: IPv6 ::255.255.0.1", "Subnet: IPv6 8592:757c:efae:4e45::/64", "Subnet: IPv4 192.168.0.0/16", "Range: IPv4 10.0.0.1-10.0.0.10"], "toJSON and fromJSON");
  list.removeCIDR("192.168.0.0/16");
  list.removeAddress("1.1.1.2");
  eq(list.check("::ffff:1.1.1.2", "ipv6") || list.check("192.168.3.4"), false, "removing rules");
  eq(inspect(new BlockList(), { depth: -1 }), "[BlockList]", "inspect at a negative depth");
  eq(Object.isFrozen(BlockList.PRIVATE_RANGES) && BlockList.isBlockList(list), true, "PRIVATE_RANGES and isBlockList");

  // A socket's block list refuses addresses, and a server's closes connections.
  const blocked = new BlockList();
  blocked.addAddress("127.0.0.1");
  const guarded = net.createServer({ blockList: blocked });
  guarded.on("connection", () => { throw new Error("a blocked peer connected"); });
  await new Promise((resolve) => guarded.listen(0, "127.0.0.1", resolve));
  const client = net.connect(guarded.address().port, "127.0.0.1");
  await within(once(client, "close"), "a blocked peer's connection closing");
  const refused = net.connect({ port: guarded.address().port, host: "127.0.0.1", blockList: blocked });
  const [error] = await once(refused, "error");
  deepEq([error.code, error.message], ["ERR_IP_BLOCKED", "IP(127.0.0.1) is blocked by net.BlockList"], "blocked connects");
  throws(() => net.createServer({ blockList: {} }), "ERR_INVALID_ARG_TYPE", "blockList options");
  await within(new Promise((resolve) => guarded.close(resolve)), "closing the guarded server");
}

// A socket connects again from its `close` listener, as Node's `test-net-reconnect.js` does.
{
  const server = net.createServer((socket) => socket.end());
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const client = new Socket();
  const ports = [];
  let connects = 0;
  client.on("connect", () => {
    connects++;
    ports.push(client.localPort);
  });
  client.resume();
  const reconnected = new Promise((resolve) =>
    client.on("close", () => {
      if (connects < 2) client.connect(server.address().port, "127.0.0.1");
      else resolve();
    }),
  );
  client.connect(server.address().port, "127.0.0.1");
  await within(reconnected, "reconnecting a closed socket");
  eq(connects, 2, "a closed socket connects again");
  eq(ports[0] !== ports[1], true, "the second connection has a new local port");
  eq(client.connecting, false, "connecting after the reconnect");
  await within(new Promise((resolve) => server.close(resolve)), "closing the reconnect server");
}

// `localPort` without `localAddress` binds the unspecified address with that port.
{
  const server = net.createServer((socket) => socket.end());
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const probe = net.createServer();
  await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
  const localPort = probe.address().port;
  await new Promise((resolve) => probe.close(resolve));
  const client = net.connect({ port: server.address().port, host: "127.0.0.1", localPort });
  await within(once(client, "connect"), "connecting from a local port");
  eq(client.localPort, localPort, "the local port of a connect with localPort only");
  client.destroy();
  await within(new Promise((resolve) => server.close(resolve)), "closing the localPort server");
}

// `listen`'s `signal` option closes the server, and a truthy `host` must be a string.
{
  throws(() => net.createServer().listen({ port: 0, host: 123 }), "ERR_INVALID_ARG_TYPE", "listen hosts");
  throws(() => net.createServer().listen({ port: 0, signal: 5 }), "ERR_INVALID_ARG_TYPE", "listen signals");
  const controller = new AbortController();
  const server = net.createServer();
  await new Promise((resolve) => server.listen({ port: 0, host: "127.0.0.1", signal: controller.signal }, resolve));
  const closed = once(server, "close");
  controller.abort();
  await within(closed, "closing on abort");
  eq(server.listening, false, "a server closed by its signal");

  const aborted = AbortSignal.abort();
  const events = [];
  const early = net.createServer();
  early.on("listening", () => events.push("listening"));
  early.on("close", () => events.push("close"));
  early.listen({ port: 0, host: "127.0.0.1", signal: aborted });
  eq(early.listening, true, "listening until the tick queue runs");
  await within(once(early, "close"), "closing on an aborted signal");
  deepEq(events, ["close"], "an aborted signal closes the server before listening is emitted");
}
