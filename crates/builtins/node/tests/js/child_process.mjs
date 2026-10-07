// `node:child_process`: spawning child processes, their streams and events, `exec` and
// `execFile`, the synchronous functions, and, where this runtime's binary is built, `fork`.

import childProcess, {
  ChildProcess,
  exec,
  execFile,
  execFileSync,
  execSync,
  fork,
  spawn,
  spawnSync,
} from "node:child_process";
import { existsSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { Socket } from "node:net";
import { promisify } from "node:util";
import { eq, deepEq, throws } from "./harness.mjs";

const once = (emitter, event) => new Promise((resolve) => emitter.once(event, (...args) => resolve(args)));
// Fail instead of hanging if `promise` doesn't settle within five seconds.
const within = (promise, what) =>
  Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${what} timed out`)), 5000).unref()),
  ]);

eq(childProcess.spawn, spawn, "the default export holds the named exports");
deepEq([exec.length, execFile.length, spawn.length, spawnSync.length, fork.length], [3, 4, 3, 3, 1], "lengths");
eq(
  throws(() => fork("x", function () {}), "ERR_INVALID_ARG_TYPE", "fork with function args").message,
  'The "args" argument must be an instance of Array. Received function ',
  "fork with function args message",
);

// A spawned process's own properties, events and streams.
{
  const child = spawn("sh", ["-c", "echo out; echo err >&2; exit 3"]);
  eq(child instanceof ChildProcess, true, "spawn returns a ChildProcess");
  deepEq(
    Object.keys(child).slice(3),
    ["_closesNeeded", "_closesGot", "connected", "signalCode", "exitCode", "killed", "spawnfile",
      "_handle", "spawnargs", "pid", "stdin", "stdout", "stderr", "stdio"],
    "own properties",
  );
  eq(child.stdout instanceof Socket, true, "streams are sockets");
  eq(typeof child.pid, "number", "pid");
  deepEq(child.spawnargs, ["sh", "-c", "echo out; echo err >&2; exit 3"], "spawnargs");
  const events = [];
  child.on("spawn", () => events.push("spawn"));
  child.on("exit", (code, signal) => events.push(`exit ${code} ${signal}`));
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  let out = "";
  let err = "";
  child.stdout.on("data", (chunk) => (out += chunk));
  child.stderr.on("data", (chunk) => (err += chunk));
  const [code, signal] = await within(once(child, "close"), "close");
  deepEq([code, signal, child.exitCode, child.signalCode], [3, null, 3, null], "close");
  deepEq(events, ["spawn", "exit 3 null"], "events");
  deepEq([out, err], ["out\n", "err\n"], "output");
}

// Standard input, the environment, the working directory and `argv0`.
{
  const child = spawn("cat");
  child.stdout.setEncoding("utf8");
  let out = "";
  child.stdout.on("data", (chunk) => (out += chunk));
  child.stdin.write("abc");
  child.stdin.end("def");
  await within(once(child, "close"), "cat");
  eq(out, "abcdef", "stdin");

  const env = Object.setPrototypeOf({ FOO: "foo" }, { BAR: "bar" });
  const shown = spawnSync("sh", ["-c", "echo $FOO $BAR $0; pwd"], { env, cwd: "/", argv0: "zero", encoding: "utf8" });
  eq(shown.stdout, "foo bar zero\n/\n", "inherited env properties, cwd and argv0");

  // The runtime's options variable reaches a child only through the environment it is given.
  const options = (env) =>
    spawnSync("/bin/sh", ["-c", "echo \"[$STARLINGMONKEY_NODE_OPTIONS]\""], { env, encoding: "utf8" }).stdout;
  eq(options({}), "[]\n", "an env without the options variable");
  eq(options({ STARLINGMONKEY_NODE_OPTIONS: "--given" }), "[--given]\n", "an env with the options variable");
}

// A missing program emits `ENOENT` and closes with its negated error number.
{
  const child = spawn("nonexistent-program-xyz", ["a"]);
  const [error] = await within(once(child, "error"), "error");
  deepEq(
    [error.message, error.code, error.syscall, error.path, error.spawnargs],
    ["spawn nonexistent-program-xyz ENOENT", "ENOENT", "spawn nonexistent-program-xyz", "nonexistent-program-xyz", ["a"]],
    "ENOENT",
  );
  const [code] = await within(once(child, "close"), "ENOENT close");
  eq(code, error.errno, "close code");
}

// Signals, `killed`, and the `timeout`, `killSignal` and `signal` options.
{
  const child = spawn("sleep", ["10"]);
  throws(() => child.kill("SIGFOO"), "ERR_UNKNOWN_SIGNAL", "unknown signal");
  const outOfRange = throws(() => process.kill(child.pid, 1e10), "ERR_UNKNOWN_SIGNAL", "signal beyond int32");
  eq(outOfRange.message, "Unknown signal: 10000000000", "signal beyond int32 message");
  eq(child.kill(0), true, "signal 0");
  eq(child.kill(), true, "kill");
  eq(child.killed, true, "killed");
  deepEq(await within(once(child, "exit"), "kill"), [null, "SIGTERM"], "killed by SIGTERM");
  eq(child.kill(), false, "kill after exit");

  const timed = spawn("sleep", ["10"], { timeout: 20, killSignal: "SIGKILL" });
  deepEq(await within(once(timed, "close"), "timeout"), [null, "SIGKILL"], "timeout");

  const controller = new AbortController();
  const aborted = spawn("sleep", ["10"], { signal: controller.signal });
  const failed = once(aborted, "error");
  controller.abort();
  const [abortError] = await within(failed, "abort");
  eq(abortError.name, "AbortError", "abort error");
  deepEq(await within(once(aborted, "close"), "abort close"), [null, "SIGTERM"], "aborted");
}

// Validation.
throws(() => spawn(""), "ERR_INVALID_ARG_VALUE", "empty file");
throws(() => spawn("echo", 3), "ERR_INVALID_ARG_TYPE", "args");
throws(() => spawn("echo", [], { stdio: "foo" }), "ERR_INVALID_ARG_VALUE", "stdio");
throws(() => spawn("echo", [], { timeout: -1 }), "ERR_OUT_OF_RANGE", "timeout");
throws(() => spawn("echo", ["a\0b"]), "ERR_INVALID_ARG_VALUE", "null bytes");
throws(() => new ChildProcess().spawn({ file: 1 }), "ERR_INVALID_ARG_TYPE", "options.file");

// `stdio` entries: inherited and ignored descriptors, and a pipe on descriptor 3.
{
  const child = spawn("sh", ["-c", "echo three >&3"], { stdio: ["ignore", "inherit", "pipe", "pipe"] });
  deepEq([child.stdin, child.stdout, child.stdio.length], [null, null, 4], "stdio entries");
  child.stdio[3].setEncoding("utf8");
  let three = "";
  child.stdio[3].on("data", (chunk) => (three += chunk));
  await within(once(child, "close"), "fd 3");
  eq(three, "three\n", "fd 3");
}

// `exec` and `execFile`, with their errors and limits, and promisified.
{
  const [error, out, err] = await within(
    new Promise((resolve) => exec("echo hi; echo oops >&2; exit 2", (...args) => resolve(args))),
    "exec",
  );
  deepEq(
    [error.message, error.code, error.killed, error.signal, error.cmd, out, err],
    ["Command failed: echo hi; echo oops >&2; exit 2\noops\n", 2, false, null,
      "echo hi; echo oops >&2; exit 2", "hi\n", "oops\n"],
    "exec error",
  );
  const [limit, partial] = await within(
    new Promise((resolve) => exec("yes | head -c 5000", { maxBuffer: 100 }, (...args) => resolve(args))),
    "maxBuffer",
  );
  deepEq([limit.code, partial.length], ["ERR_CHILD_PROCESS_STDIO_MAXBUFFER", 100], "maxBuffer");
  const [timedOut] = await within(
    new Promise((resolve) => execFile("sleep", ["10"], { timeout: 20 }, (...args) => resolve(args))),
    "execFile timeout",
  );
  deepEq([timedOut.killed, timedOut.signal], [true, "SIGTERM"], "execFile timeout");
  const [, buffer] = await within(
    new Promise((resolve) => execFile("echo", ["x"], { encoding: "buffer" }, (...args) => resolve(args))),
    "buffer",
  );
  eq(Buffer.isBuffer(buffer) && buffer.toString(), "x\n", "buffer output");
  const promised = promisify(execFile)("echo", ["promised"]);
  eq(promised.child instanceof ChildProcess, true, "promise.child");
  deepEq(await within(promised, "promisified"), { stdout: "promised\n", stderr: "" }, "promisified");
  const rejected = await promisify(exec)("echo no >&2; exit 1").catch((error) => error);
  deepEq([rejected.code, rejected.stderr], [1, "no\n"], "promisified rejection");
  // `null` and `undefined` arguments join as empty words into the shell command and `cmd`.
  const [, joined] = await within(
    new Promise((resolve) => execFile("echo", [null, undefined], { shell: true }, (...args) => resolve(args))),
    "null arguments with shell",
  );
  eq(joined, "\n", "null arguments with shell");
  const [failed] = await within(
    new Promise((resolve) => execFile("sh", ["-c", "exit 1", null], (...args) => resolve(args))),
    "null argument in cmd",
  );
  eq(failed.cmd, "sh -c exit 1 ", "null argument in cmd");
}

// The synchronous functions.
{
  eq(
    throws(() => spawnSync("cat", { input: 5 }), "ERR_INVALID_ARG_TYPE", "number input").message,
    'The "options.stdio[0]" property must be of type string or an instance of Buffer, TypedArray, or DataView. Received type number (5)',
    "number input message",
  );
  const result = spawnSync("sh", ["-c", "cat; exit 4"], { input: "in" });
  deepEq(
    [Object.keys(result), result.status, result.signal, result.stdout.toString(), result.output[0]],
    [["status", "signal", "output", "pid", "stdout", "stderr"], 4, null, "in", null],
    "spawnSync",
  );
  const missing = spawnSync("nonexistent-program-xyz");
  deepEq([missing.error.code, missing.error.syscall, missing.pid, missing.output], ["ENOENT", "spawnSync nonexistent-program-xyz", 0, null], "spawnSync ENOENT");
  const timeout = spawnSync("sleep", ["10"], { timeout: 20 });
  deepEq([timeout.error.code, timeout.signal], ["ETIMEDOUT", "SIGTERM"], "spawnSync timeout");
  const limited = spawnSync("sh", ["-c", "yes | head -c 100000"], { maxBuffer: 10 });
  eq(limited.error.code, "ENOBUFS", "spawnSync maxBuffer");
  eq(execSync("echo $((1 + 2))", { encoding: "utf8" }), "3\n", "execSync");
  const failure = throws(() => execFileSync("sh", ["-c", "exit 5"], { stdio: "pipe" }), undefined, "execFileSync");
  deepEq([failure.status, failure.message], [5, "Command failed: sh -c exit 5"], "execFileSync error");
}

// An `ipc` entry in `stdio`: messages are lines of JSON, decoded as UTF-8 across reads. The first
// write ends within a character and contains an invalid byte before it.
{
  const script = "printf '\"\\377\\342\\202' >&3; sleep 0.2; printf '\\254\"\\n[1]\\n' >&3";
  const child = spawn("sh", ["-c", script], { stdio: ["ignore", "ignore", "ignore", "ipc"] });
  const messages = [];
  child.on("message", (message) => messages.push(message));
  await within(once(child, "close"), "ipc close");
  deepEq(messages, ["\ufffd\u20ac", [1]], "ipc messages");
}

// `fork` runs the runtime's binary with an IPC channel: `process.execPath` when the script runs
// under it, as `starling/tests/modules.rs` runs it, and otherwise the binary in the directory above
// the test binary's, if one was built.
const runtime =
  basename(process.execPath) === "starlingmonkey"
    ? process.execPath
    : join(dirname(dirname(process.execPath)), "starlingmonkey");
if (existsSync(runtime)) {
  const dir = mkdtempSync(join(tmpdir(), "child-process-"));
  const module = join(dir, "child.cjs");
  writeFileSync(module, `
    process.on("message", (message) => {
      if (message === "bye") process.disconnect();
      else process.send({ echo: message, connected: process.connected });
    });
  `);
  // The test binary isn't the runtime, so the options that enable Node.js compatibility aren't
  // added for it.
  const child = fork(module, [], { execPath: runtime, execArgv: ["--node-compat"] });
  eq(child.connected, true, "connected");
  throws(() => child.send(undefined), "ERR_MISSING_ARGS", "send without a message");
  child.send(1);
  const [reply] = await within(once(child, "message"), "message");
  deepEq(reply, { echo: 1, connected: true }, "reply");
  child.send("bye");
  await within(once(child, "disconnect"), "disconnect");
  deepEq(await within(once(child, "close"), "fork close"), [0, null], "fork close");
}
