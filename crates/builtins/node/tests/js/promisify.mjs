// `util.promisify` with `util.promisify.custom` and Node's `customPromisifyArgs`, the functions
// Node.js gives either of them, and `util.callbackify`.

import dns from "node:dns";
import fs from "node:fs";
import timers from "node:timers";
import timersPromises from "node:timers/promises";
import { callbackify, promisify } from "node:util";
import { eq, deepEq, throws } from "./harness.mjs";

const custom = promisify.custom;
const descriptor = (object, key) => {
  const { get, set, value, ...flags } = Object.getOwnPropertyDescriptor(object, key) ?? {};
  return { ...flags, get: get?.name, set: typeof set, value: typeof value };
};

// `setTimeout` and `setImmediate` have an enumerable, non-configurable getter of their
// `timers/promises` versions. `setInterval` has none.
for (const [name, fn] of [["setTimeout", setTimeout], ["setImmediate", setImmediate]]) {
  deepEq(
    descriptor(fn, custom),
    { enumerable: true, configurable: false, get: "get", set: "undefined", value: "undefined" },
    `${name}[promisify.custom]`,
  );
  eq(fn[custom], timersPromises[name], `${name}[promisify.custom] is timers/promises'`);
  eq(promisify(fn), timersPromises[name], `promisify(${name})`);
}
eq(timers.setTimeout[custom], timersPromises.setTimeout, "timers.setTimeout");
eq(Object.getOwnPropertyDescriptor(setInterval, custom), undefined, "setInterval has no custom");
eq(await promisify(setTimeout)(1, "v"), "v", "promisify(setTimeout)");
eq(await promisify(setImmediate)("w"), "w", "promisify(setImmediate)");
deepEq(
  descriptor(timersPromises.setTimeout, custom),
  { writable: false, enumerable: false, configurable: true, set: "undefined", value: "function" },
  "promisify defines the custom function on itself",
);

// `fs.exists` has a non-writable, non-enumerable, non-configurable custom function.
deepEq(
  descriptor(fs.exists, custom),
  { writable: false, enumerable: false, configurable: false, set: "undefined", value: "function" },
  "fs.exists[promisify.custom]",
);
const exists = promisify(fs.exists);
eq(exists, fs.exists[custom], "promisify(fs.exists)");
deepEq([exists.name, exists.length], ["exists", 1], "fs.exists' custom function");
eq(await exists("/tmp"), true, "exists resolves with true");
eq(await exists("/nonexistent/starling"), false, "exists resolves with false");
eq(await exists(), false, "exists without a path resolves with false");

// `customPromisifyArgs` is an own, non-enumerable, non-writable, non-configurable symbol property.
const argsKey = Object.getOwnPropertySymbols(fs.read)[0];
eq(String(argsKey), "Symbol(customPromisifyArgs)", "fs.read's customPromisifyArgs");
eq(Object.getOwnPropertySymbols(fs.write)[0], argsKey, "fs.write's customPromisifyArgs");
deepEq(
  descriptor(fs.read, argsKey),
  { writable: false, enumerable: false, configurable: false, set: "undefined", value: "object" },
  "customPromisifyArgs attributes",
);
deepEq(fs.read[argsKey], ["bytesRead", "buffer"], "fs.read's names");
deepEq(fs.write[argsKey], ["bytesWritten", "buffer"], "fs.write's names");
deepEq(dns.lookup[argsKey], ["address", "family"], "dns.lookup's names");

const dir = fs.mkdtempSync("/tmp/starling-promisify-");
const fd = fs.openSync(`${dir}/file`, "w+");
const written = await promisify(fs.write)(fd, "hello");
deepEq(Object.keys(written), ["bytesWritten", "buffer"], "promisify(fs.write) keys");
deepEq([written.bytesWritten, written.buffer], [5, "hello"], "promisify(fs.write)");
const read = await promisify(fs.read)(fd, Buffer.alloc(4), 0, 4, 1);
deepEq([read.bytesRead, read.buffer.toString()], [4, "ello"], "promisify(fs.read)");
fs.closeSync(fd);
fs.rmSync(dir, { recursive: true });

// The names apply to any function, but only when the callback passes more than one value.
function pair(callback) {
  callback(null, 1, 2);
}
pair[argsKey] = ["first", "second"];
deepEq(await promisify(pair)(), { first: 1, second: 2 }, "names map the values");
function single(callback) {
  callback(null, 1);
}
single[argsKey] = ["first", "second"];
eq(await promisify(single)(), 1, "one value resolves as itself");

// A truthy `promisify.custom` must be a function.
const bad = () => {};
bad[custom] = 5;
throws(() => promisify(bad), "ERR_INVALID_ARG_TYPE", "a custom that isn't a function");
const falsy = () => {};
falsy[custom] = 0;
eq(typeof promisify(falsy), "function", "a falsy custom is ignored");

// `util.callbackify` calls back in the tick queue, after the microtasks queued before the result
// settled, with the call's `this`.
const order = [];
const callbacked = callbackify(async function (x) {
  return x * 2;
});
const target = { name: "target" };
await new Promise((resolve) => {
  callbacked.call(target, 2, function (error, value) {
    order.push(`callback ${this?.name} ${error} ${value}`);
    resolve();
  });
  Promise.resolve()
    .then(() => order.push("microtask 1"))
    .then(() => order.push("microtask 2"))
    .then(() => order.push("microtask 3"));
});
deepEq(
  order,
  ["microtask 1", "microtask 2", "microtask 3", "callback target null 4"],
  "callbackify calls back in the tick queue",
);
const falsyReason = await new Promise((resolve) =>
  callbackify(async () => {
    throw null;
  })(resolve),
);
deepEq(
  [falsyReason.code, falsyReason.message, falsyReason.reason],
  ["ERR_FALSY_VALUE_REJECTION", "Promise was rejected with falsy value", null],
  "callbackify wraps a falsy reason",
);
throws(() => callbackify(() => 5)(() => {}), TypeError, "callbackify of a non-thenable result");
const thenable = await new Promise((resolve) =>
  callbackify(() => ({ then: (fulfill) => fulfill(7) }))((error, value) => resolve([error, value])),
);
deepEq(thenable, [null, 7], "callbackify calls the result's then");
