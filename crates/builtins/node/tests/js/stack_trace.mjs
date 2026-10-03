// V8's stack trace API: `Error.prepareStackTrace` and `CallSite` objects.

import { deepEq, eq, throws } from "./harness.mjs";

/** The `CallSite`s `Error.captureStackTrace` gives a `prepareStackTrace` that returns them. */
function callSites() {
  const previous = Error.prepareStackTrace;
  Error.prepareStackTrace = (_, sites) => sites;
  const holder = {};
  Error.captureStackTrace(holder);
  const sites = holder.stack;
  Error.prepareStackTrace = previous;
  return sites;
}

function named() {
  return callSites();
}

const sites = named();
eq(Array.isArray(sites), true, "the hook receives an array");
const [first, second] = sites;
eq(first.getFunctionName(), "callSites", "the innermost frame comes first");
eq(second.getFunctionName(), "named", "the caller follows");
eq(first.getFileName().endsWith("stack_trace.mjs"), true, "getFileName");
eq(typeof first.getLineNumber(), "number", "getLineNumber");
eq(typeof first.getColumnNumber(), "number", "getColumnNumber");
eq(first.isEval(), false, "isEval");
eq(first.isNative(), false, "isNative");
eq(String(second).startsWith("named ("), true, "toString names the function");
eq(String(second).endsWith(`:${second.getLineNumber()}:${second.getColumnNumber()})`), true, "toString ends with the location");
eq(throws(() => new first.constructor(), Error, "CallSite can't be constructed").message, "Not supported", "the constructor's error");

// Without a hook, `stack` is the engine's string.
const plain = {};
Error.captureStackTrace(plain);
eq(typeof plain.stack, "string", "the default stack is a string");

// The hook's result is the stack of errors too, and is computed once.
let calls = 0;
Error.prepareStackTrace = (error, frames) => {
  calls++;
  // A `stack` read while the hook runs gets the engine's string.
  const inner = typeof new Error("inner").stack;
  return `${error.message}:${frames.length > 0}:${inner}`;
};
const error = new Error("outer");
eq(error.stack, "outer:true:string", "an error's stack");
eq(error.stack, "outer:true:string", "read again");
eq(calls, 1, "the hook ran once");
Error.prepareStackTrace = undefined;
eq(typeof new Error("x").stack, "string", "removing the hook restores the string");

// `captureStackTrace` skips the frames above and including `constructorOpt`.
function skipped() {
  const holder = {};
  Error.prepareStackTrace = (_, frames) => frames.map((frame) => frame.getFunctionName());
  Error.captureStackTrace(holder, skipped);
  const names = holder.stack;
  Error.prepareStackTrace = undefined;
  return names;
}
function caller() {
  return skipped();
}
deepEq(caller().slice(0, 1), ["caller"], "constructorOpt");

// A function name with an `@` stays whole, and the file name doesn't take part of it.
{
  const holder = {};
  holder["a@b"] = function () { return new Error("at sign"); };
  const prepare = Error.prepareStackTrace;
  Error.prepareStackTrace = (_, frames) => frames[0];
  const frame = holder["a@b"]().stack;
  Error.prepareStackTrace = prepare;
  eq(frame.getFunctionName().includes("a@b"), true, "the function name keeps its @");
  eq(frame.getFileName().endsWith("stack_trace.mjs"), true, "the file name");
}
