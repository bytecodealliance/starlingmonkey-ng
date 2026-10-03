// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The runtime half of the differential fuzzer: fuzz.mjs puts this before each generated program,
// which runs in Node.js and in StarlingMonkey. `__op` runs one operation and writes its result or
// error as a line of the trace, and `__log` writes a line for the running operation. The trace
// ends with a `done` line. Values are serialized by `__serialize`, which avoids `util.inspect` so
// that differences in inspection don't hide the behavior under test.
//
// Each operation gets its own directory, `__dir()`, under the directory `NODE_DIFF_DIR` names.

"use strict";
const __fs = require("node:fs");
const __compareMessages = process.env.NODE_DIFF_MESSAGES === "1";
let __current = "-";

function __serialize(value, depth = 0, seen = new Set()) {
  if (value === null) return "null";
  switch (typeof value) {
    case "undefined":
      return "undefined";
    case "boolean":
      return String(value);
    case "number":
      return Object.is(value, -0) ? "-0" : String(value);
    case "bigint":
      return `${value}n`;
    case "string":
      return JSON.stringify(value);
    case "symbol":
      return `Symbol(${value.description ?? ""})`;
    case "function":
      return `[Function ${value.name || "anonymous"}]`;
  }
  if (seen.has(value)) return "[Circular]";
  if (depth > 5) return "[Deep]";
  seen.add(value);
  try {
    if (value instanceof Error) {
      const parts = [`name=${value.name}`];
      if (value.code !== undefined) parts.push(`code=${__serialize(value.code)}`);
      for (const key of ["errno", "syscall", "path", "dest"]) {
        if (value[key] !== undefined) parts.push(`${key}=${__serialize(__relative(value[key]))}`);
      }
      if (__compareMessages || (value.code === undefined && value.name === "Error") || value.code === "ERR_METHOD_NOT_IMPLEMENTED") {
        parts.push(`message=${JSON.stringify(value.message)}`);
      }
      return `Error{${parts.join(" ")}}`;
    }
    if (ArrayBuffer.isView(value)) {
      const bytes = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
      const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
      const kind = Buffer.isBuffer(value) ? "Buffer" : value.constructor.name;
      return `${kind}<${hex}>`;
    }
    if (value instanceof ArrayBuffer) return `ArrayBuffer(${value.byteLength})`;
    if (value instanceof Date) return `Date(${Number.isNaN(value.getTime()) ? "Invalid" : value.toISOString()})`;
    if (value instanceof RegExp) return String(value);
    if (value instanceof Promise) return "[Promise]";
    if (value instanceof Map) {
      const entries = [...value].map(([k, v]) => `${__serialize(k, depth + 1, seen)}=>${__serialize(v, depth + 1, seen)}`);
      return `Map{${entries.join(", ")}}`;
    }
    if (value instanceof Set) return `Set{${[...value].map((v) => __serialize(v, depth + 1, seen)).join(", ")}}`;
    if (Array.isArray(value)) return `[${value.map((v) => __serialize(v, depth + 1, seen)).join(", ")}]`;
    const proto = Object.getPrototypeOf(value);
    const name = proto === null ? "null-proto" : proto === Object.prototype ? "" : (proto.constructor?.name ?? "?");
    const keys = Object.keys(value).sort();
    return `${name}{${keys.map((key) => `${JSON.stringify(key)}: ${__serialize(value[key], depth + 1, seen)}`).join(", ")}}`;
  } finally {
    seen.delete(value);
  }
}

/** `value` with the operation directory's path replaced by `<dir>`, if it is a string. */
function __relative(value) {
  return typeof value === "string" ? value.replaceAll(process.env.NODE_DIFF_DIR, "<dir>") : value;
}

function __write(line) {
  __fs.writeSync(1, line.replaceAll("\n", "\\n") + "\n");
}

function __log(...values) {
  __write(`${__current} log ${values.map((value) => __serialize(value)).join(" ")}`);
}

/**
 * Calls `fn` with `args` and a Node.js-style callback, and resolves with the callback's value. A
 * callback called before `fn` returns, called again, or called with an error that is neither
 * `null` nor an error writes a log line.
 */
function __cb(fn, ...args) {
  return new Promise((resolve, reject) => {
    let returned = false;
    let calls = 0;
    fn(...args, (error, value) => {
      calls++;
      if (!returned) __log("callback called synchronously");
      if (calls > 1) return __log("callback called again", calls);
      if (error !== null && !(error instanceof Error)) __log("callback error argument", error);
      return error ? reject(error) : resolve(value);
    });
    returned = true;
  });
}

/** Resolves with `promise`'s value, or with "unsettled" if it doesn't settle within `ms`. */
function __settle(promise, ms = 1000) {
  let timer;
  const timeout = new Promise((resolve) => (timer = setTimeout(resolve, ms, "unsettled")));
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** The running operation's directory, created on first use. */
function __dir() {
  const dir = `${process.env.NODE_DIFF_DIR}/${__current}`;
  __fs.mkdirSync(dir, { recursive: true });
  return dir;
}

async function __op(id, body) {
  __current = id;
  try {
    const result = await body();
    __write(`${id} ok ${__serialize(result)}`);
  } catch (error) {
    __write(`${id} throws ${__serialize(error)}`);
  }
}

function __done() {
  __current = "-";
  __write("done");
}
