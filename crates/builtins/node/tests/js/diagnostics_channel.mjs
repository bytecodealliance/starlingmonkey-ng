// `node:diagnostics_channel`: channels, stores and tracing channels.

import dc, { channel, Channel, hasSubscribers, subscribe, unsubscribe, tracingChannel } from "node:diagnostics_channel";
import { AsyncLocalStorage } from "node:async_hooks";
import { eq, deepEq, throws } from "./harness.mjs";

const events = channel("events");
eq(channel("events"), events, "channels are cached by name");
eq(events instanceof Channel, true, "channels are Channel instances");
eq(events.name, "events", "name");
eq(events.hasSubscribers, false, "no subscribers yet");
eq(dc.channel, channel, "default export");
const symbolName = Symbol("symbol channel");
eq(channel(symbolName).name, symbolName, "symbol names");
throws(() => channel(1), "ERR_INVALID_ARG_TYPE", "names are strings or symbols");

const received = [];
const onMessage = (message, name) => received.push([message, name]);
events.subscribe(onMessage);
eq(events.hasSubscribers, true, "hasSubscribers after subscribe");
eq(hasSubscribers("events"), true, "module-level hasSubscribers");
events.publish({ n: 1 });
deepEq(received, [[{ n: 1 }, "events"]], "publish calls subscribers with the message and name");
eq(events.unsubscribe(onMessage), true, "unsubscribe returns true");
eq(events.unsubscribe(onMessage), false, "unsubscribe of a non-subscriber returns false");
eq(events.hasSubscribers, false, "hasSubscribers after unsubscribe");
throws(() => events.subscribe("nope"), "ERR_INVALID_ARG_TYPE", "subscribers are functions");

subscribe("module", onMessage);
channel("module").publish("via module");
eq(unsubscribe("module", onMessage), true, "module-level unsubscribe");
deepEq(received.at(-1), ["via module", "module"], "module-level subscribe");

// A throwing subscriber's error is uncaught, and doesn't stop the others.
const uncaught = new Promise((resolve) => process.once("uncaughtException", resolve));
const after = [];
const throwing = channel("throwing");
throwing.subscribe(() => {
  throw new Error("subscriber failure (expected in this test)");
});
throwing.subscribe((message) => after.push(message));
throwing.publish("still delivered");
deepEq(after, ["still delivered"], "later subscribers run after one throws");
eq((await uncaught).message, "subscriber failure (expected in this test)", "the error is uncaught");

// Bound stores are set while runStores runs its function, and the message is published inside.
const storage = new AsyncLocalStorage();
const stored = channel("stored");
stored.bindStore(storage, (message) => ({ wrapped: message }));
const seenInSubscriber = [];
stored.subscribe(() => seenInSubscriber.push(storage.getStore()));
eq(stored.hasSubscribers, true, "bound stores count as subscribers");
const result = stored.runStores("data", function (a, b) {
  return [this.self, a, b, storage.getStore()];
}, { self: "this" }, 1, 2);
deepEq(result, ["this", 1, 2, { wrapped: "data" }], "runStores runs fn with the transformed store");
deepEq(seenInSubscriber, [{ wrapped: "data" }], "the message is published with the stores set");
eq(storage.getStore(), undefined, "runStores restores the store");
eq(stored.unbindStore(storage), true, "unbindStore");
eq(stored.unbindStore(storage), false, "unbindStore of an unbound store");

// Tracing channels publish start/end, error, and the async events for promises and callbacks.
const trace = tracingChannel("op");
eq(trace.start, channel("tracing:op:start"), "tracing channels are named channels");
eq(trace.hasSubscribers, false, "a tracing channel without subscribers");
eq(trace.traceSync((x) => x * 2, {}, undefined, 21), 42, "traceSync without subscribers");
const log = [];
const handlers = Object.fromEntries(
  ["start", "end", "asyncStart", "asyncEnd", "error"].map((event) => [
    event,
    (context) => log.push([event, context.result ?? context.error?.message ?? null]),
  ]),
);
trace.subscribe(handlers);
eq(trace.hasSubscribers, true, "hasSubscribers after subscribe");

eq(trace.traceSync((a, b) => a + b, {}, undefined, 1, 2), 3, "traceSync returns the result");
deepEq(log.splice(0), [["start", null], ["end", 3]], "traceSync events");
throws(() => trace.traceSync(() => { throw new TypeError("sync failure"); }), TypeError, "traceSync rethrows");
deepEq(log.splice(0), [["start", null], ["error", "sync failure"], ["end", "sync failure"]], "traceSync error events");

eq(await trace.tracePromise(async (x) => x + 1, {}, undefined, 1), 2, "tracePromise resolves with the result");
deepEq(log.splice(0), [["start", null], ["end", null], ["asyncStart", 2], ["asyncEnd", 2]], "tracePromise events");
await trace.tracePromise(async () => { throw new Error("async failure"); }).then(
  () => { throw new Error("tracePromise should reject"); },
  (error) => eq(error.message, "async failure", "tracePromise rejects with the reason"),
);
deepEq(
  log.splice(0),
  [["start", null], ["end", null], ["error", "async failure"], ["asyncStart", "async failure"], ["asyncEnd", "async failure"]],
  "tracePromise rejection events",
);

const callbackResult = await new Promise((resolve) => {
  trace.traceCallback((value, done) => setTimeout(() => done(null, value * 3), 1), 1, {}, undefined, 5, (error, value) => resolve([error, value]));
});
deepEq(callbackResult, [null, 15], "traceCallback calls the original callback");
deepEq(log.splice(0), [["start", null], ["end", null], ["asyncStart", 15], ["asyncEnd", 15]], "traceCallback events");

eq(trace.unsubscribe(handlers), true, "unsubscribe");
eq(trace.hasSubscribers, false, "no subscribers after unsubscribe");
throws(() => tracingChannel({ start: 1 }), "ERR_INVALID_ARG_TYPE", "tracing channels need channels");

// `new Channel` registers the channel, `hasSubscribers` creates none, and falsy handlers and
// callback errors are skipped.
const manual = new Channel("manual");
eq(channel("manual"), manual, "new Channel registers the channel");
eq(hasSubscribers("never-created"), false, "hasSubscribers of an unknown name");
const falsy = [];
const falsyTrace = tracingChannel("falsy");
falsyTrace.subscribe({ start: null, asyncStart: (context) => falsy.push(["asyncStart", context.result]), error: () => falsy.push(["error"]) });
falsyTrace.traceCallback((callback) => callback(0, "zero"), -1, {}, null, () => {});
deepEq(falsy, [["asyncStart", "zero"]], "a falsy callback error is success");
