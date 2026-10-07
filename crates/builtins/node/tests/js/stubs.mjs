// Every builtin module can be imported, and the ones not implemented throw when used.

import { createRequire } from "node:module";
import { connect, Http2ServerRequest } from "node:http2";
import http2 from "http2";
import { test } from "node:test";
import { eq, throws } from "./harness.mjs";

eq(typeof connect, "function", "stub functions are functions");
eq(http2.connect, connect, "the default export holds the named exports");
throws(() => connect("http://localhost"), "ERR_METHOD_NOT_IMPLEMENTED", "stub functions throw");
throws(() => new Http2ServerRequest(), "ERR_METHOD_NOT_IMPLEMENTED", "stub classes throw");
class Sub extends Http2ServerRequest {}
eq(Sub.prototype instanceof Http2ServerRequest, true, "stub classes can be extended");
eq(typeof test, "function", "node:-only modules import");
eq(typeof createRequire, "function", "module exports");
const os = await import("node:os");
eq(os.EOL, "\n", "primitive constants are exported as is");
let failed = false;
try {
  await import("test");
} catch {
  failed = true;
}
eq(failed, true, "node:-only modules have no bare specifier");

// A stub's export takes a global's value only in the modules whose exports are globals.
globalThis.start = 42;
const repl = await import("node:repl");
eq(typeof repl.start, "function", "a user-defined global doesn't replace a stub export");
delete globalThis.start;

// Symbol exports are symbols, registered ones from the global registry.
eq(typeof repl.REPL_MODE_SLOPPY, "symbol", "an unregistered symbol export");
const workerThreads = await import("node:worker_threads");
eq(workerThreads.SHARE_ENV, Symbol.for("nodejs.worker_threads.SHARE_ENV"), "a registered symbol export");

// Exports Node.js defines read-only keep that shape.
const zlib = await import("node:zlib");
const codes = Object.getOwnPropertyDescriptor(zlib.default, "codes");
eq(codes.writable || codes.configurable, false, "zlib.codes is read-only and non-configurable");
eq(Object.getOwnPropertyDescriptor(os.default, "EOL").writable, false, "os.EOL is read-only");

// `_http_common.methods` is a copy of `http.METHODS` made without `Array.prototype.slice`.
const slice = Array.prototype.slice;
Array.prototype.slice = () => "replaced";
let httpCommon;
try {
  httpCommon = await import("_http_common");
} finally {
  Array.prototype.slice = slice;
}
const { METHODS } = await import("node:http");
eq(httpCommon.methods.join(), METHODS.join(), "_http_common.methods");
