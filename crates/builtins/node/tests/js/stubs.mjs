// Every builtin module can be imported, and the ones not implemented throw when used.

import { createRequire } from "node:module";
import { spawn, ChildProcess } from "node:child_process";
import childProcess from "child_process";
import { test } from "node:test";
import { eq, throws } from "./harness.mjs";

eq(typeof spawn, "function", "stub functions are functions");
eq(childProcess.spawn, spawn, "the default export holds the named exports");
throws(() => spawn("ls"), "ERR_METHOD_NOT_IMPLEMENTED", "stub functions throw");
throws(() => new ChildProcess(), "ERR_METHOD_NOT_IMPLEMENTED", "stub classes throw");
class Sub extends ChildProcess {}
eq(Sub.prototype instanceof ChildProcess, true, "stub classes can be extended");
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
