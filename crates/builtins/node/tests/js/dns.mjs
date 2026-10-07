// `node:dns`'s resolver against a name server on the loopback interface: the result of each
// query method, error codes, TCP after truncation, timeouts, `cancel`, server lists, and the
// module-level functions bound to the default resolver.

import dgram from "node:dgram";
import net from "node:net";
import dns from "node:dns";
import dnsPromises from "node:dns/promises";
import { createRequire } from "node:module";
import { eq, deepEq, throws } from "./harness.mjs";

const require = createRequire(import.meta.url);

const TYPES = { A: 1, NS: 2, CNAME: 5, SOA: 6, PTR: 12, MX: 15, TXT: 16, AAAA: 28, SRV: 33, NAPTR: 35, TLSA: 52, ANY: 255, CAA: 257 };
const NAMES = Object.fromEntries(Object.entries(TYPES).map(([name, code]) => [code, name]));

const u16 = (n) => Buffer.from([n >> 8, n & 0xff]);
const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32BE(n >>> 0); return b; };
const text = (s) => { const b = Buffer.from(s, "latin1"); return Buffer.concat([Buffer.from([b.length]), b]); };
function name(domain) {
  const labels = domain === "" ? [] : domain.split(".");
  return Buffer.concat([...labels.map(text), Buffer.from([0])]);
}

function rdata(rr) {
  switch (rr.type) {
    case "A": return Buffer.from(rr.address.split(".").map(Number));
    case "AAAA": {
      const b = Buffer.alloc(16);
      const [head, tail] = rr.address.split("::").map((part) => (part ? part.split(":") : []));
      [...head, ...Array(8 - head.length - tail.length).fill("0"), ...tail].forEach((p, i) => b.writeUInt16BE(parseInt(p, 16), i * 2));
      return b;
    }
    case "CNAME": case "NS": case "PTR": return name(rr.value);
    case "MX": return Buffer.concat([u16(rr.priority), name(rr.exchange)]);
    case "TXT": return Buffer.concat(rr.entries.map(text));
    case "SRV": return Buffer.concat([u16(rr.priority), u16(rr.weight), u16(rr.port), name(rr.name)]);
    case "NAPTR": return Buffer.concat([u16(rr.order), u16(rr.preference), text(rr.flags), text(rr.service), text(rr.regexp), name(rr.replacement)]);
    case "SOA": return Buffer.concat([name(rr.nsname), name(rr.hostmaster), ...[rr.serial, rr.refresh, rr.retry, rr.expire, rr.minttl].map(u32)]);
    case "CAA": return Buffer.concat([Buffer.from([rr.critical]), text(rr.tag), Buffer.from(rr.value, "latin1")]);
    case "TLSA": return Buffer.concat([Buffer.from([rr.certUsage, rr.selector, rr.match]), Buffer.from(rr.data)]);
    case "RAW": return rr.bytes;
  }
}

const record = (rr) => {
  const data = rdata(rr);
  return Buffer.concat([name(rr.domain), u16(rr.code ?? TYPES[rr.type]), u16(1), u32(rr.ttl ?? 300), u16(data.length), data]);
};

function parseQuery(msg) {
  let offset = 12;
  const labels = [];
  while (msg[offset]) {
    labels.push(msg.toString("latin1", offset + 1, offset + 1 + msg[offset]));
    offset += 1 + msg[offset];
  }
  offset += 1;
  return { id: msg.readUInt16BE(0), flags: msg.readUInt16BE(2), domain: labels.join("."), type: NAMES[msg.readUInt16BE(offset)], end: offset + 4, msg };
}

function response(q, { rcode = 0, answers = [], tc = false, id = q.id, question = true }) {
  return Buffer.concat([
    u16(id), u16(0x8180 | rcode | (tc ? 0x200 : 0)), u16(question ? 1 : 0), u16(answers.length), u16(0), u16(0),
    question ? q.msg.subarray(12, q.end) : Buffer.alloc(0),
    ...answers.map(record),
  ]);
}

const queries = [];

/** A name server on 127.0.0.1 answering from `zone` over UDP and TCP on the same port. */
function startServer(zone) {
  const answer = (msg, tcp) => {
    const q = parseQuery(msg);
    queries.push({ domain: q.domain, type: q.type, tcp, rd: (q.flags & 0x100) !== 0 });
    const entry = zone[`${q.domain}|${q.type}`] ?? zone[q.domain] ?? { rcode: 3 };
    const reply = typeof entry === "function" ? entry(q) : entry;
    if (reply === null || Buffer.isBuffer(reply)) return reply;
    if (reply.truncateUdp && !tcp) return response(q, { tc: true });
    return response(q, reply);
  };
  const udp = dgram.createSocket("udp4");
  udp.on("message", (msg, rinfo) => {
    const reply = answer(msg, false);
    if (reply) udp.send(reply, rinfo.port, rinfo.address);
  });
  const tcp = net.createServer((socket) => {
    let buffer = Buffer.alloc(0);
    socket.on("error", () => {});
    socket.on("data", (data) => {
      buffer = Buffer.concat([buffer, data]);
      while (buffer.length >= 2 && buffer.length >= 2 + buffer.readUInt16BE(0)) {
        const msg = buffer.subarray(2, 2 + buffer.readUInt16BE(0));
        buffer = buffer.subarray(2 + msg.length);
        const reply = answer(msg, true);
        if (reply) socket.write(Buffer.concat([u16(reply.length), reply]));
      }
    });
  });
  return new Promise((resolve) => {
    udp.bind(0, "127.0.0.1", () => {
      const port = udp.address().port;
      tcp.listen(port, "127.0.0.1", () => resolve({ port, close: () => { udp.close(); tcp.close(); } }));
    });
  });
}

/** `value` with the keys of its objects, `undefined` values, holes and `ArrayBuffer`s visible. */
const shape = (value) => {
  if (Array.isArray(value)) return [...Array(value.length).keys()].map((i) => (i in value ? shape(value[i]) : "<hole>"));
  if (value instanceof ArrayBuffer) return `ArrayBuffer(${[...new Uint8Array(value)]})`;
  if (value && typeof value === "object") return Object.keys(value).map((key) => [key, value[key] === undefined ? "<undefined>" : shape(value[key])]);
  return value;
};

/** The class, message and own properties of the rejection of `promise`. */
const rejection = (promise) => promise.then(
  (value) => ["resolved", shape(value)],
  (error) => [error.constructor.name, error.message, ...Object.keys(error).map((key) => `${key}=${error[key]}`)],
);

const zone = {
  "a.test|A": { answers: [{ domain: "a.test", type: "A", address: "1.2.3.4", ttl: 100 }, { domain: "a.test", type: "A", address: "5.6.7.8", ttl: 200 }] },
  "a.test|AAAA": { answers: [{ domain: "a.test", type: "AAAA", address: "::42", ttl: 7 }, { domain: "a.test", type: "AAAA", address: "::1:53", ttl: 8 }] },
  "chain.test|A": { answers: [{ domain: "chain.test", type: "CNAME", value: "end.test", ttl: 50 }, { domain: "end.test", type: "A", address: "9.9.9.9", ttl: 100 }] },
  "chain.test|CNAME": { answers: [{ domain: "chain.test", type: "CNAME", value: "mid.test" }, { domain: "mid.test", type: "CNAME", value: "end.test" }] },
  "mx.test|MX": { answers: [{ domain: "mx.test", type: "MX", priority: 10, exchange: "mail.mx.test" }] },
  "txt.test|TXT": { answers: [{ domain: "txt.test", type: "TXT", entries: ["a", "b\0c"] }, { domain: "txt.test", type: "TXT", entries: ["\xe9", ""] }] },
  "_mongodb._tcp.srv.test|SRV": { answers: [{ domain: "_mongodb._tcp.srv.test", type: "SRV", priority: 1, weight: 2, port: 27017, name: "host.srv.test" }] },
  "naptr.test|NAPTR": { answers: [{ domain: "naptr.test", type: "NAPTR", order: 10, preference: 20, flags: "s", service: "SIP+D2U", regexp: "", replacement: "_sip._udp.naptr.test" }] },
  "soa.test|SOA": { answers: [{ domain: "soa.test", type: "SOA", nsname: "ns.soa.test", hostmaster: "admin.soa.test", serial: 4000000000, refresh: 1, retry: 2, expire: 3, minttl: 4000000000 }] },
  "nosoa.test|SOA": { answers: [{ domain: "nosoa.test", type: "A", address: "1.1.1.1" }] },
  "caa.test|CAA": { answers: [{ domain: "caa.test", type: "CAA", critical: 128, tag: "issue", value: "letsencrypt.org" }] },
  "tlsa.test|TLSA": { answers: [{ domain: "tlsa.test", type: "CNAME", value: "x.test" }, { domain: "tlsa.test", type: "TLSA", certUsage: 3, selector: 1, match: 1, data: [1, 2, 3] }] },
  "ns.test|NS": { answers: [{ domain: "ns.test", type: "NS", value: "ns1.test" }] },
  "escape.test|NS": { answers: [{ domain: "escape.test", type: "RAW", code: 2, bytes: Buffer.from([3, 0x61, 0x2e, 0x62, 2, 0x24, 0x01, 1, 0xe9, 0]) }] },
  "4.3.2.1.in-addr.arpa|PTR": { answers: [{ domain: "4.3.2.1.in-addr.arpa", type: "PTR", value: "one.test" }, { domain: "4.3.2.1.in-addr.arpa", type: "PTR", value: "two.test" }] },
  "any.test|ANY": { answers: [
    { domain: "any.test", type: "A", address: "1.2.3.4", ttl: 5 },
    { domain: "any.test", type: "AAAA", address: "::1", ttl: 6 },
    { domain: "any.test", type: "MX", priority: 1, exchange: "m.test" },
    { domain: "any.test", type: "TXT", entries: ["t1", "t2"] },
    { domain: "any.test", type: "SOA", nsname: "ns.test", hostmaster: "h.test", serial: 1, refresh: 2, retry: 3, expire: 4, minttl: 5 },
    { domain: "any.test", type: "TLSA", certUsage: 1, selector: 0, match: 2, data: [9] },
    { domain: "any.test", type: "CAA", critical: 0, tag: "issue", value: "ca.test" },
  ] },
  "empty.test|A": { answers: [] },
  "servfail.test": { rcode: 2 },
  "refused.test": { rcode: 5 },
  "trunc.test|A": { truncateUdp: true, answers: [{ domain: "trunc.test", type: "A", address: "7.7.7.7" }] },
  "wrongid.test|A": (q) => response(q, { id: (q.id + 1) & 0xffff, answers: [{ domain: "wrongid.test", type: "A", address: "1.1.1.1" }] }),
  "noquestion.test|A": (q) => response(q, { question: false }),
  "silent.test": () => null,
};

const server = await startServer(zone);
const resolver = new dnsPromises.Resolver({ timeout: 100, tries: 1 });
resolver.setServers([`127.0.0.1:${server.port}`]);

// Results of each query method.
deepEq(await resolver.resolve4("a.test"), ["1.2.3.4", "5.6.7.8"], "resolve4");
deepEq(shape(await resolver.resolve4("a.test", { ttl: true })), [[["address", "1.2.3.4"], ["ttl", 100]], [["address", "5.6.7.8"], ["ttl", 200]]], "resolve4 with ttl");
deepEq(await resolver.resolve6("a.test"), ["::42", "::0.1.0.83"], "resolve6 formats as uv_inet_ntop");
deepEq(await resolver.resolve4("chain.test", { ttl: true }), [{ address: "9.9.9.9", ttl: 50 }], "CNAME TTLs bound address TTLs");
deepEq(await resolver.resolveCname("chain.test"), ["mid.test"], "resolveCname");
deepEq(shape(await resolver.resolveMx("mx.test")), [[["exchange", "mail.mx.test"], ["priority", 10], ["type", "MX"]]], "resolveMx");
deepEq(await resolver.resolveTxt("txt.test"), [["a", "b\0c"], ["\xe9", ""]], "resolveTxt");
deepEq(shape(await resolver.resolveSrv("_mongodb._tcp.srv.test")), [[["name", "host.srv.test"], ["port", 27017], ["priority", 1], ["weight", 2], ["type", "SRV"]]], "resolveSrv");
deepEq(shape(await resolver.resolveNaptr("naptr.test")), [[["flags", "s"], ["service", "SIP+D2U"], ["regexp", ""], ["replacement", "_sip._udp.naptr.test"], ["order", 10], ["preference", 20], ["type", "<undefined>"]]], "resolveNaptr");
deepEq(shape(await resolver.resolveSoa("soa.test")), [["nsname", "ns.soa.test"], ["hostmaster", "admin.soa.test"], ["serial", 4000000000], ["refresh", 1], ["retry", 2], ["expire", 3], ["minttl", 4000000000], ["type", "<undefined>"]], "resolveSoa");
deepEq(shape(await resolver.resolveCaa("caa.test")), [[["critical", 128], ["type", "CAA"], ["issue", "letsencrypt.org"]]], "resolveCaa");
deepEq(shape(await resolver.resolveTlsa("tlsa.test")), ["<hole>", [["certUsage", 3], ["selector", 1], ["match", 1], ["data", "ArrayBuffer(1,2,3)"]]], "resolveTlsa");
deepEq(await resolver.resolveNs("ns.test"), ["ns1.test"], "resolveNs");
deepEq(await resolver.resolveNs("escape.test"), ["a\\.b.\\$\\001.\\233"], "names are escaped as c-ares escapes them");
deepEq(await resolver.resolvePtr("4.3.2.1.in-addr.arpa"), ["one.test", "two.test"], "resolvePtr");
deepEq(await resolver.reverse("1.2.3.4"), ["one.test", "two.test"], "reverse");
deepEq(await resolver.resolve("mx.test", "MX"), await resolver.resolveMx("mx.test"), "resolve with an rrtype");
deepEq(shape(await resolver.resolveAny("any.test")), [
  [["address", "1.2.3.4"], ["ttl", 5], ["type", "A"]],
  [["address", "::1"], ["ttl", 6], ["type", "AAAA"]],
  [["exchange", "m.test"], ["priority", 1], ["type", "MX"]],
  [["entries", ["t1", "t2"]], ["type", "TXT"]],
  [["nsname", "ns.test"], ["hostmaster", "h.test"], ["serial", 1], ["refresh", 2], ["retry", 3], ["expire", 4], ["minttl", 5], ["type", "SOA"]],
  "<hole>", "<hole>", "<hole>", "<hole>", "<hole>",
  [["certUsage", 1], ["selector", 0], ["match", 2], ["data", "ArrayBuffer(9)"]],
  [["critical", 0], ["type", "CAA"], ["issue", "ca.test"]],
], "resolveAny");

// Errors.
deepEq(await rejection(resolver.resolve4("empty.test")), ["Error", "queryA ENODATA empty.test", "errno=undefined", "code=ENODATA", "syscall=queryA", "hostname=empty.test"], "ENODATA");
deepEq((await rejection(resolver.resolve4("nx.test")))[1], "queryA ENOTFOUND nx.test", "ENOTFOUND");
deepEq((await rejection(resolver.resolve4("servfail.test")))[1], "queryA ESERVFAIL servfail.test", "ESERVFAIL");
deepEq((await rejection(resolver.resolve4("refused.test")))[1], "queryA EREFUSED refused.test", "EREFUSED");
deepEq((await rejection(resolver.resolveSoa("nosoa.test")))[1], "querySoa EBADRESP nosoa.test", "SOA without an SOA record");
deepEq((await rejection(resolver.resolve4("noquestion.test")))[1], "queryA EBADRESP noquestion.test", "EBADRESP");
deepEq((await rejection(resolver.resolve4("wrongid.test")))[1], "queryA ETIMEOUT wrongid.test", "answers to other IDs are ignored");
deepEq((await rejection(resolver.resolve4("silent.test")))[1], "queryA ETIMEOUT silent.test", "ETIMEOUT");
deepEq((await rejection(resolver.resolve4("a..test")))[1], "queryA EBADNAME a..test", "EBADNAME");
deepEq((await rejection(resolver.resolve4("x".repeat(64))))[2], "errno=undefined", "long labels");
deepEq((await rejection(resolver.resolve4("hidden.onion")))[1], "queryA ENOTFOUND hidden.onion", ".onion names");
deepEq(await rejection(resolver.reverse("nope")), ["Error", "getHostByAddr EINVAL nope", "errno=-22", "code=EINVAL", "syscall=getHostByAddr", "hostname=nope"], "reverse of a non-address");
deepEq((await rejection(resolver.reverse("1.2.3.9")))[1], "getHostByAddr ENOTFOUND 1.2.3.9", "reverse failures are ENOTFOUND");
throws(() => new dns.Resolver().reverse("nope", () => {}), "EINVAL", "the callback reverse throws");

// Query names: IDNA, case and escapes.
queries.length = 0;
await rejection(resolver.resolve4("Bücher.TEST"));
await rejection(resolver.resolve4("a\\066c.test"));
deepEq(queries.map((q) => q.domain), ["xn--bcher-kva.test", "aBc.test"], "query names");
eq(queries.every((q) => q.rd), true, "recursion desired");

// TCP after a truncated UDP response.
queries.length = 0;
deepEq(await resolver.resolve4("trunc.test"), ["7.7.7.7"], "TCP after truncation");
deepEq(queries.map((q) => q.tcp), [false, true], "the TCP retry");

// Retries go through the servers, and a refused server fails the attempt at once.
{
  const second = await startServer({ "silent.test": () => null });
  const retrying = new dnsPromises.Resolver({ timeout: 50, tries: 2, maxTimeout: 50 });
  retrying.setServers([`127.0.0.1:${server.port}`, `127.0.0.1:${second.port}`]);
  queries.length = 0;
  eq((await rejection(retrying.resolve4("silent.test")))[1], "queryA ETIMEOUT silent.test", "retries time out");
  eq(queries.length, 4, "tries times the servers");
  second.close();
  const refused = new dnsPromises.Resolver({ timeout: 2000, tries: 1 });
  refused.setServers([`127.0.0.1:1`]);
  const start = Date.now();
  eq((await rejection(refused.resolve4("a.test")))[1], "queryA ECONNREFUSED a.test", "ECONNREFUSED");
  eq(Date.now() - start < 1500, true, "a refused server doesn't wait for the timeout");
}

// `cancel`.
{
  const callbackResolver = new dns.Resolver();
  callbackResolver.setServers([`127.0.0.1:${server.port}`]);
  const order = [];
  const cancelled = new Promise((resolve) => callbackResolver.resolve4("silent.test", (error) => resolve(error)));
  const request = callbackResolver.resolve4("silent.test", () => order.push("callback"));
  deepEq([request.bindingName, request.hostname], ["queryA", "silent.test"], "the request object");
  deepEq(Object.keys(request), ["bindingName", "callback", "hostname", "oncomplete", "ttl"], "the request object's keys");
  eq(typeof request.oncomplete, "function", "oncomplete");
  throws(() => callbackResolver.setServers(["1.1.1.1"]), "ERR_DNS_SET_SERVERS_FAILED", "setServers while querying");
  callbackResolver.cancel();
  order.push("cancelled");
  const error = await cancelled;
  deepEq([error.code, error.syscall, error.hostname], ["ECANCELLED", "queryA", "silent.test"], "ECANCELLED");
  eq(order[0], "cancelled", "callbacks run later");
  eq((await rejection((() => { const p = resolver.resolve4("silent.test"); resolver.cancel(); return p; })()))[1], "queryA ECANCELLED silent.test", "promise cancel");
}

// Options and arguments.
throws(() => new dns.Resolver({ timeout: "1" }), "ERR_INVALID_ARG_TYPE", "timeout type");
throws(() => new dns.Resolver({ timeout: -2 }), "ERR_OUT_OF_RANGE", "timeout range");
throws(() => new dns.Resolver({ tries: 0 }), "ERR_OUT_OF_RANGE", "tries range");
throws(() => new dns.Resolver({ maxTimeout: -1 }), "ERR_OUT_OF_RANGE", "maxTimeout range");
throws(() => dns.Resolver(), TypeError, "a class");
throws(() => new dns.Resolver().resolve4("a.test"), "ERR_INVALID_ARG_TYPE", "a callback");
throws(() => new dns.Resolver().resolve4(5, () => {}), "ERR_INVALID_ARG_TYPE", "a name");
throws(() => new dns.Resolver().resolve("a.test", "BOGUS", () => {}), "ERR_INVALID_ARG_VALUE", "rrtype");
throws(() => resolver.resolve("a.test", 5), "ERR_INVALID_ARG_TYPE", "promise rrtype");
throws(() => resolver.resolveNs([]), "ERR_INVALID_ARG_TYPE", "promise name");
throws(() => resolver.setLocalAddress("1.2.3.4", "5.6.7.8"), "ERR_INVALID_ARG_VALUE", "two IPv4 addresses");
throws(() => resolver.setLocalAddress(), "ERR_INVALID_ARG_TYPE", "setLocalAddress");
deepEq(Object.keys(new dns.Resolver()), ["_handle"], "own keys");
deepEq(Object.keys(dns.Resolver.prototype), ["resolveAny", "resolve4", "resolve6", "resolveCaa", "resolveCname", "resolveMx", "resolveNs", "resolveTlsa", "resolveTxt", "resolveSrv", "resolvePtr", "resolveNaptr", "resolveSoa", "reverse", "resolve"], "prototype");
deepEq([dns.Resolver.prototype.resolve4.name, dns.Resolver.prototype.resolve4.length, dns.Resolver.prototype.resolve.length, dnsPromises.Resolver.prototype.resolve.length], ["queryA", 2, 3, 2], "method names");
eq(Object.getPrototypeOf(dns.Resolver), Object.getPrototypeOf(dnsPromises.Resolver), "a shared base class");
deepEq(Object.getOwnPropertyNames(Object.getPrototypeOf(dns.Resolver).prototype).sort(), ["cancel", "constructor", "getServers", "setLocalAddress", "setServers"], "base prototype");

// Server lists.
{
  const servers = new dns.Resolver();
  servers.setServers(["::1", "[::1]:53", "[::1]:54", "1.2.3.4:53", "1.2.3.4:5353", "::ffff:1.2.3.4", "[1.2.3.4]:0", "::1:53"]);
  deepEq(servers.getServers(), ["::1", "[::1]:54", "1.2.3.4", "1.2.3.4:5353", "::ffff:1.2.3.4", "::0.1.0.83"], "getServers");
  const holes = ["127.0.0.1"];
  holes[2] = "0.0.0.0";
  servers.setServers(holes);
  deepEq(servers.getServers(), ["127.0.0.1", "0.0.0.0"], "holes");
  for (const bad of ["foobar", "127.0.0.1:va", "1.2.3", ":".repeat(1000), "\n"]) throws(() => servers.setServers([bad]), "ERR_INVALID_IP_ADDRESS", bad);
  throws(() => servers.setServers(["1.2.3.4:65536"]), "ERR_SOCKET_BAD_PORT", "ports");
  throws(() => servers.setServers("1.2.3.4"), "ERR_INVALID_ARG_TYPE", "an array");
  throws(() => servers.setServers([5]), "ERR_INVALID_ARG_TYPE", "strings");
  servers.setServers([]);
  deepEq(servers.getServers(), [], "no servers");
  eq((await rejection(new dnsPromises.Resolver().resolve4.call(servers, "a.test")))[1], "queryA UNKNOWN_ARES_ERROR a.test", "no servers to query");
  eq(new dns.Resolver().getServers().length > 0, true, "default servers");
}

// The module-level functions.
{
  eq(dns.promises, dnsPromises, "dns.promises");
  eq(require("dns/promises"), dnsPromises, "require('dns/promises')");
  deepEq([dns.resolve4.name, dns.resolve4.length, dns.getServers.name, dns.setServers.name], ["bound queryA", 2, "bound getServers", "defaultResolverSetServers"], "bound functions");
  const before = dns.resolve4;
  dns.setServers([`127.0.0.1:${server.port}`]);
  eq(dns.resolve4 === before, false, "setServers rebinds");
  deepEq(dnsPromises.getServers(), [`127.0.0.1:${server.port}`], "and rebinds dns.promises");
  deepEq(await new Promise((resolve) => dns.resolve4("a.test", (error, addresses) => resolve([error, addresses]))), [null, ["1.2.3.4", "5.6.7.8"]], "dns.resolve4");
  deepEq(await dnsPromises.resolveSrv("_mongodb._tcp.srv.test").then((records) => records.map((r) => r.port)), [27017], "dnsPromises.resolveSrv");
  dnsPromises.setServers(["127.0.0.2"]);
  deepEq([dnsPromises.getServers(), dns.getServers()], [["127.0.0.2"], [`127.0.0.1:${server.port}`]], "dnsPromises.setServers");
}

// `lookupService` arguments.
throws(() => dns.lookupService("127.0.0.1"), "ERR_MISSING_ARGS", "lookupService arguments");
throws(() => dnsPromises.lookupService("127.0.0.1"), "ERR_MISSING_ARGS", "promise lookupService arguments");
throws(() => dns.lookupService("nope", 80, () => {}), "ERR_INVALID_ARG_VALUE", "lookupService address");
throws(() => dns.lookupService("127.0.0.1", 65536, () => {}), "ERR_SOCKET_BAD_PORT", "lookupService port");

server.close();
