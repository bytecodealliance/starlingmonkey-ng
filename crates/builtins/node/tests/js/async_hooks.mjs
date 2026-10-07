// `node:async_hooks`: `AsyncLocalStorage` and `AsyncResource`, and the async context they keep
// their stores in, across promise jobs and event-loop tasks.

import {
  AsyncLocalStorage,
  AsyncResource,
  executionAsyncId,
  triggerAsyncId,
} from "node:async_hooks";
import { eq, deepEq, throws } from "./harness.mjs";

const storage = new AsyncLocalStorage();
const other = new AsyncLocalStorage({ name: "other", defaultValue: "default" });
eq(storage.getStore(), undefined, "no store outside run");
eq(storage.name, "", "name defaults to the empty string");
eq(other.name, "other", "name option");
eq(other.getStore(), "default", "defaultValue option");
eq(executionAsyncId(), 1, "executionAsyncId");
eq(triggerAsyncId(), 0, "triggerAsyncId");

eq(storage.run(1, (a, b) => a + b + storage.getStore(), 10, 20), 31, "run passes arguments");
eq(storage.getStore(), undefined, "run restores the context");
throws(() => storage.run(1, () => { throw new TypeError("thrown"); }), TypeError, "run rethrows");
eq(storage.getStore(), undefined, "run restores the context when the callback throws");

storage.run("outer", () => {
  other.run("other", () => {
    eq(storage.getStore(), "outer", "nested runs keep the outer store");
    eq(other.getStore(), "other", "nested store");
    storage.exit(() => {
      eq(storage.getStore(), undefined, "exit removes the store");
      eq(other.getStore(), "other", "exit keeps other stores");
    });
  });
  eq(other.getStore(), "default", "a nested run is undone");
});

// Each kind of async work, started directly inside `run`. Pending promises are settled from outside
// any context, so their continuations can only see the store they captured.
let release;
const gate = new Promise((resolve) => (release = resolve));
const probes = {
  timeout: () => new Promise((resolve) => setTimeout(() => resolve(storage.getStore()), 1)),
  immediate: () => new Promise((resolve) => setImmediate(() => resolve(storage.getStore()))),
  nextTick: () => new Promise((resolve) => process.nextTick(() => resolve(storage.getStore()))),
  microtask: () => new Promise((resolve) => queueMicrotask(() => resolve(storage.getStore()))),
  "await pending": async () => (await gate, storage.getStore()),
  "then pending": () => gate.then(() => storage.getStore()),
  "await settled": async () => (await null, storage.getStore()),
  "then settled": () => Promise.resolve().then(() => storage.getStore()),
};
const probed = Object.entries(probes).map(([name, probe]) => storage.run(name, probe));
release();
const observed = await Promise.all(probed);
deepEq(
  Object.fromEntries(Object.keys(probes).map((name, index) => [name, observed[index]])),
  Object.fromEntries(Object.keys(probes).map((name) => [name, name])),
  "each kind of async work runs with the store it was started in",
);

// Each callback records the store it saw, in the order the callbacks ran.
const seen = [];
const record = (label) => seen.push(`${label}=${storage.getStore()}`);
await Promise.all(
  ["a", "b"].map((store) =>
    storage.run(store, async () => {
      await null;
      record("await");
      await new Promise((resolve) => setTimeout(() => (record("timeout"), resolve()), 5));
      await new Promise((resolve) => setImmediate(() => (record("immediate"), resolve())));
      await new Promise((resolve) => process.nextTick(() => (record("nextTick"), resolve())));
      await new Promise((resolve) => queueMicrotask(() => (record("microtask"), resolve())));
      await Promise.resolve().then(() => record("then"));
      record("end");
    }),
  ),
);
eq(storage.getStore(), undefined, "awaiting does not leak a store");
for (const store of ["a", "b"]) {
  deepEq(
    seen.filter((entry) => entry.endsWith(`=${store}`)).map((entry) => entry.split("=")[0]),
    ["await", "timeout", "immediate", "nextTick", "microtask", "then", "end"],
    `store ${store} propagates through a chain of async work`,
  );
}

// Timers created in different contexts each run with their own.
const timerStores = await Promise.all(
  [30, 10, 20].map((delay) =>
    storage.run(delay, () => new Promise((resolve) => setTimeout(() => resolve(storage.getStore()), delay))),
  ),
);
deepEq(timerStores, [30, 10, 20], "interleaved timers keep their own stores");

const intervalStores = [];
await new Promise((resolve) => {
  storage.run("interval", () => {
    const interval = setInterval(() => {
      intervalStores.push(storage.getStore());
      storage.enterWith("changed inside the callback");
      if (intervalStores.length === 3) {
        clearInterval(interval);
        resolve();
      }
    }, 1);
  });
});
deepEq(intervalStores, ["interval", "interval", "interval"], "intervals rerun with their context");

const entered = await new Promise((resolve) => {
  setTimeout(() => {
    storage.enterWith("entered");
    const now = storage.getStore();
    setTimeout(() => resolve([now, storage.getStore()]), 1);
  }, 1);
});
deepEq(entered, ["entered", "entered"], "enterWith sets the store for work created afterwards");
eq(storage.getStore(), undefined, "enterWith stays inside its task");

const disabled = new AsyncLocalStorage();
disabled.run("store", () => {
  disabled.disable();
  eq(disabled.getStore(), undefined, "disable removes the store");
});

const bound = storage.run("bound", () => AsyncLocalStorage.bind(() => storage.getStore()));
eq(bound(), "bound", "AsyncLocalStorage.bind");
const snapshot = storage.run("snapshot", () => AsyncLocalStorage.snapshot());
eq(snapshot((value) => `${value} ${storage.getStore()}`, "ran in"), "ran in snapshot", "snapshot");

const resource = storage.run("resource", () => new AsyncResource("TEST"));
eq(typeof resource.asyncId(), "number", "asyncId");
eq(resource.triggerAsyncId(), 1, "triggerAsyncId defaults to the execution ID");
const receiver = {};
eq(
  resource.runInAsyncScope(function (suffix) { return this === receiver && storage.getStore() + suffix; }, receiver, "!"),
  "resource!",
  "runInAsyncScope",
);
const resourceBound = resource.bind(() => storage.getStore());
eq(resourceBound(), "resource", "asyncResource.bind");
eq(resourceBound.asyncResource, resource, "bound functions name their resource");
eq(storage.run("static", () => AsyncResource.bind(() => storage.getStore()))(), "static", "AsyncResource.bind");
throws(() => new AsyncResource(1), "ERR_INVALID_ARG_TYPE", "AsyncResource validates its type");
eq(resource.emitDestroy(), resource, "emitDestroy");

// Settling a promise from off-thread work goes through the engine's own job queue.
const wasm = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
const module = await storage.run("wasm", () => WebAssembly.compile(wasm));
eq(module instanceof WebAssembly.Module, true, "promises settled by off-thread work resolve");

// `AsyncResource` validates `triggerAsyncId`, its IDs differ from `executionAsyncId()`, bound
// functions keep the length, and the state isn't exposed.
throws(() => new AsyncResource("x", { triggerAsyncId: Symbol("t") }), "ERR_INVALID_ASYNC_ID", "triggerAsyncId validation");
throws(() => new AsyncResource("x", { triggerAsyncId: -2 }), "ERR_INVALID_ASYNC_ID", "triggerAsyncId range");
eq(new AsyncResource("x", 7).triggerAsyncId(), 7, "a number option is the triggerAsyncId");
eq(new AsyncResource("x").asyncId() === executionAsyncId(), false, "resource IDs differ from the execution ID");
eq(AsyncLocalStorage.bind(function (a, b) {}).length, 2, "bound functions keep the length");
deepEq(Object.getOwnPropertyNames(new AsyncLocalStorage()), [], "no own properties");
deepEq(Object.getOwnPropertyNames(new AsyncResource("x")), [], "no own properties on resources");
const { createHook } = await import("node:async_hooks");
const hook = createHook({ init() {} });
eq(hook.enable(), hook, "createHook's enable returns the hook");
deepEq(Object.keys(hook), [], "a hook has no own properties");
eq(hook.constructor.name, "AsyncHook", "a hook is an AsyncHook");
eq(new AsyncResource("t", "abc").triggerAsyncId(), new AsyncResource("t").triggerAsyncId(), "options that aren't an object or a number");
throws(() => new AsyncResource("t", null), TypeError, "null options");
throws(() => createHook({ init: 1 }), "ERR_ASYNC_CALLBACK", "createHook validates callbacks");
