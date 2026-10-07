// `node:dgram` sockets on the loopback interface: sends and receives, connected sockets, the
// operations that wait for an implicit bind, argument errors, and `ref`/`unref`.

import dgram, { Socket } from "node:dgram";
import { EventEmitter } from "node:events";
import { eq, deepEq, throws } from "./harness.mjs";

const once = (emitter, event) => new Promise((resolve) => emitter.once(event, (...args) => resolve(args)));
const within = (promise, what) =>
  Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${what} timed out`)), 2000).unref()),
  ]);

const server = dgram.createSocket("udp4");
eq(server instanceof Socket && server instanceof EventEmitter, true, "sockets are EventEmitters");
eq(server.type, "udp4", "type");
throws(() => server.address(), "EBADF", "address before bind");
throws(() => server.setBroadcast(true), "EBADF", "options before bind");
await within(new Promise((resolve) => server.bind(0, "127.0.0.1", resolve)), "binding");
const { address, family, port } = server.address();
deepEq([address, family, port > 0], ["127.0.0.1", "IPv4", true], "bound address");
throws(() => server.bind(0), "ERR_SOCKET_ALREADY_BOUND", "binding twice");

// A send binds the client, waits for the bind, and the server gets the datagram and its `rinfo`.
{
  const client = dgram.createSocket("udp4");
  const sent = new Promise((resolve) => client.send(Buffer.from("xxhelloyy"), 2, 5, port, "127.0.0.1", (...args) => resolve(args)));
  const [message, rinfo] = await within(once(server, "message"), "receiving");
  eq(message.toString(), "hello", "the sliced datagram");
  deepEq(Object.keys(rinfo), ["address", "family", "port", "size"], "rinfo");
  deepEq([rinfo.address, rinfo.family, rinfo.port, rinfo.size], ["127.0.0.1", "IPv4", client.address().port, 5], "rinfo values");
  deepEq(await sent, [null, 5], "the send callback");
  server.send(["a", Buffer.from("b"), new Uint8Array([99])], rinfo.port, rinfo.address);
  const [reply] = await within(once(client, "message"), "the reply");
  eq(reply.toString(), "abc", "a list of buffers");
  client.close();
  await once(client, "close");
  throws(() => client.send("x", port), "ERR_SOCKET_DGRAM_NOT_RUNNING", "send after close");
  throws(() => client.close(), "ERR_SOCKET_DGRAM_NOT_RUNNING", "closing twice");
}

// A connected socket sends to its peer, and rejects other destinations.
{
  const client = dgram.createSocket("udp4");
  throws(() => client.remoteAddress(), "ERR_SOCKET_DGRAM_NOT_CONNECTED", "remoteAddress before connect");
  throws(() => client.connect(0), "ERR_SOCKET_BAD_PORT", "connecting to port 0");
  await within(new Promise((resolve) => client.connect(port, "127.0.0.1", resolve)), "connecting");
  eq(client.remoteAddress().port, port, "remoteAddress");
  throws(() => client.connect(port), "ERR_SOCKET_DGRAM_IS_CONNECTED", "connecting twice");
  throws(() => client.send("x", 0, 1, port), "ERR_SOCKET_DGRAM_IS_CONNECTED", "a destination while connected");
  client.send("connected");
  const [message] = await within(once(server, "message"), "a connected send");
  eq(message.toString(), "connected", "the connected send");
  client.disconnect();
  throws(() => client.disconnect(), "ERR_SOCKET_DGRAM_NOT_CONNECTED", "disconnecting twice");
  await new Promise((resolve) => client.close(resolve));
}

// Argument errors.
{
  const socket = dgram.createSocket("udp4");
  throws(() => dgram.createSocket("udp5"), "ERR_SOCKET_BAD_TYPE", "a bad type");
  throws(() => dgram.createSocket({ type: "udp4", lookup: 1 }), "ERR_INVALID_ARG_TYPE", "a bad lookup");
  throws(() => socket.send("x"), "ERR_SOCKET_BAD_PORT", "a send without a port");
  throws(() => socket.send(1, port), "ERR_INVALID_ARG_TYPE", "a send of a number");
  throws(() => socket.send(Buffer.alloc(2), 3, 1, port, "127.0.0.1"), "ERR_BUFFER_OUT_OF_BOUNDS", "an offset past the end");
  throws(() => socket.send(Buffer.alloc(4), 1, 4294967295, port, "127.0.0.1"), "ERR_BUFFER_OUT_OF_BOUNDS", "a length past the end");
  throws(() => socket.setTTL("x"), "ERR_INVALID_ARG_TYPE", "a bad TTL");
  throws(() => socket.setTTL(1000), "EINVAL", "a TTL out of range");
  throws(() => socket.addMembership(), "ERR_MISSING_ARGS", "a membership without an address");
  throws(() => socket.addMembership("256.1.1.1"), "EINVAL", "a membership of a bad address");
  throws(() => socket.setRecvBufferSize(-1), "ERR_SOCKET_BAD_BUFFER_SIZE", "a negative buffer size");
  throws(() => socket.getRecvBufferSize(), "ERR_SOCKET_BUFFER_SIZE", "a buffer size before bind");
  deepEq([socket.getSendQueueSize(), socket.getSendQueueCount()], [0, 0], "an empty send queue");
  socket.close();
}

// The default lookup reads `dns.lookup` for host names, and a custom one gets the family.
{
  const families = [];
  const lookup = (host, family, callback) => {
    families.push([host, family]);
    callback(null, "127.0.0.1", 4);
  };
  const socket = dgram.createSocket({ type: "udp4", lookup });
  await within(new Promise((resolve) => socket.bind(0, "example.invalid", resolve)), "a custom lookup");
  deepEq(families, [["example.invalid", 4]], "the custom lookup's arguments");
  eq(socket.address().address, "127.0.0.1", "the custom lookup's address");
  socket.close();
}

// A send that fails without a callback emits `error`, with one it calls the callback instead.
{
  const socket = dgram.createSocket("udp4");
  const [error] = await within(
    new Promise((resolve) => socket.send(Buffer.alloc(70000), port, "127.0.0.1", (...args) => resolve(args))),
    "an oversized send",
  );
  deepEq([error.code, error.syscall], ["EMSGSIZE", "send"], "an oversized send");
  socket.close();
}

// Closing during the implicit bind waits for it, and `close` is emitted once.
{
  const socket = dgram.createSocket("udp4");
  socket.send("x", port, "127.0.0.1");
  let closes = 0;
  socket.close(() => closes++);
  await within(once(socket, "close"), "closing after an implicit bind");
  eq(closes, 1, "close callback");
}

// `signal` closes the socket, and `Symbol.asyncDispose` waits for `close`.
{
  const controller = new AbortController();
  const socket = dgram.createSocket({ type: "udp4", signal: controller.signal });
  controller.abort();
  await within(once(socket, "close"), "closing on abort");
  const disposed = dgram.createSocket("udp4");
  await disposed[Symbol.asyncDispose]();
  throws(() => disposed.address(), "ERR_SOCKET_DGRAM_NOT_RUNNING", "after asyncDispose");
}

// An unreferenced socket still receives while a timer keeps the event loop alive, and lets the
// event loop finish otherwise, which this script finishing shows.
{
  server.unref();
  const keepAlive = setTimeout(() => {}, 2000);
  const client = dgram.createSocket("udp4");
  client.send("unreferenced", port, "127.0.0.1", () => client.close());
  const [message] = await within(once(server, "message"), "an unreferenced receive");
  eq(message.toString(), "unreferenced", "the unreferenced receive");
  clearTimeout(keepAlive);
}
