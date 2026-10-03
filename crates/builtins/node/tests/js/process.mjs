// The `process` global and `node:process`.

import processModule, { cwd, env, nextTick, platform } from "node:process";
import { eq, throws } from "./harness.mjs";

eq(processModule, process, "the default export is the global");
eq(cwd, process.cwd, "named exports are process's properties");
eq(nextTick, process.nextTick, "nextTick export");
eq(platform, process.platform, "platform export");
eq(env, process.env, "env export");
eq(globalThis.global, globalThis, "global");

eq(typeof process.on, "function", "process is an EventEmitter");
eq(process.version.startsWith("v"), true, "version");
eq(typeof process.versions.node, "string", "versions.node");
eq(process.cwd().startsWith("/"), true, "cwd is absolute");
eq(Array.isArray(process.argv), true, "argv is an array");
eq(process.argv[1].endsWith("process.mjs"), true, "argv[1] is the entry module");
eq(typeof process.env.PATH, "string", "env holds the environment");
process.env.STARLING_TEST = "1";
eq(process.env.STARLING_TEST, "1", "env is writable");
eq(process.env, process.env, "env is one object");
eq(typeof process.pid, "number", "pid");
eq(process.exitCode, undefined, "exitCode");

const [seconds, nanos] = process.hrtime();
eq(Number.isInteger(seconds) && Number.isInteger(nanos), true, "hrtime");
eq(typeof process.hrtime.bigint(), "bigint", "hrtime.bigint");
eq(process.uptime() >= 0, true, "uptime");
eq(typeof process.memoryUsage().heapUsed, "number", "memoryUsage");
eq(typeof process.stdout.write, "function", "stdout.write");
eq(process.stdout.write(""), true, "writing to stdout");
throws(() => process.chdir("/does/not/exist"), "ENOENT", "chdir to a missing directory");
throws(() => process.nextTick(42), "ERR_INVALID_ARG_TYPE", "nextTick validates the callback");

const ticks = [];
await new Promise((resolve) => {
  process.nextTick((a, b) => ticks.push(a + b), 1, 2);
  process.nextTick(resolve);
});
eq(ticks[0], 3, "nextTick passes its arguments");

let warned = null;
process.once("warning", (warning) => (warned = warning));
process.emitWarning("careful", "CustomWarning", "W001");
eq(warned, null, "emitWarning emits warning in a tick");
await new Promise((resolve) => process.nextTick(resolve));
eq(warned?.name, "CustomWarning", "emitWarning emits warning");
eq(warned?.code, "W001", "warning code");

// `process.env` converts values to strings, and `process` has the members Node.js has.
process.env.STARLING_NUMBER = 3000;
eq(process.env.STARLING_NUMBER, "3000", "env values are strings");
delete process.env.STARLING_NUMBER;
eq(process.env.STARLING_NUMBER, undefined, "env properties can be deleted");
eq(process.getBuiltinModule("node:path"), (await import("node:path")).default, "getBuiltinModule");
eq(process.getBuiltinModule("not-a-builtin"), undefined, "getBuiltinModule of a non-builtin");
eq(typeof process.dlopen, "function", "unimplemented members are stubs");
