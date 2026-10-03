// `node:events`, with cases from Node's `test/parallel/test-event-emitter-*.js`.

import EventEmitter, { errorMonitor, getEventListeners, listenerCount, on, once } from "node:events";
import events from "events";
import { eq, deepEq, throws } from "./harness.mjs";

eq(events, EventEmitter, "the default export is EventEmitter");
eq(EventEmitter.EventEmitter, EventEmitter, "EventEmitter.EventEmitter");
eq(errorMonitor, EventEmitter.errorMonitor, "errorMonitor is mirrored");
eq(typeof errorMonitor, "symbol", "errorMonitor is a symbol");
eq(EventEmitter.defaultMaxListeners, 10, "defaultMaxListeners");
eq(once, EventEmitter.once, "named exports are properties of EventEmitter");

// Listeners run in order, with `this` the emitter, and `emit` reports whether any ran.
const e = new EventEmitter();
const calls = [];
e.on("a", function (x, y) {
  calls.push(["first", this === e, x, y]);
});
e.prependListener("a", () => calls.push(["prepended"]));
eq(e.emit("a", 1, 2), true, "emit with listeners");
eq(e.emit("b"), false, "emit without listeners");
deepEq(calls, [["prepended"], ["first", true, 1, 2]], "listener order and arguments");
eq(e.listenerCount("a"), 2, "listenerCount");
deepEq(e.eventNames(), ["a"], "eventNames");

// `once` listeners run once, and `listeners` unwraps them while `rawListeners` doesn't.
let onceCount = 0;
const onceListener = () => onceCount++;
e.once("o", onceListener);
eq(e.listeners("o")[0], onceListener, "listeners unwraps once");
eq(e.rawListeners("o")[0].listener, onceListener, "rawListeners keeps the wrapper");
e.emit("o");
e.emit("o");
eq(onceCount, 1, "once runs once");
eq(e.listenerCount("o"), 0, "once removes itself");

// `removeListener` removes by identity, also a `once` listener before it ran.
const removable = () => {
  throw new Error("removed listener ran");
};
e.on("r", removable);
e.once("r", removable);
e.off("r", removable);
e.removeListener("r", removable);
e.emit("r");
eq(e.listenerCount("r"), 0, "removeListener");

// `newListener` and `removeListener` events.
const seen = [];
const watched = new EventEmitter();
watched.on("newListener", (name) => seen.push(`new:${String(name)}`));
watched.on("removeListener", (name) => seen.push(`remove:${String(name)}`));
const listener = () => {};
watched.on("x", listener);
watched.removeListener("x", listener);
watched.removeAllListeners();
deepEq(seen, ["new:removeListener", "new:x", "remove:x", "remove:newListener"], "meta events");

// An `error` event without listeners throws, after `errorMonitor` listeners saw it.
const failing = new EventEmitter();
let monitored = null;
failing.on(errorMonitor, (error) => (monitored = error));
const boom = new Error("boom");
eq(throws(() => failing.emit("error", boom)), boom, "error events throw their error");
eq(monitored, boom, "errorMonitor listeners see the error");
throws(() => failing.emit("error", "text"), "ERR_UNHANDLED_ERROR", "non-errors are wrapped");
throws(() => failing.on("x", 42), "ERR_INVALID_ARG_TYPE", "listeners must be functions");

// ES5-style subclasses, and copying the prototype's methods onto another object (as Express does
// with its application function).
function Legacy() {
  EventEmitter.call(this);
  this.legacy = true;
}
Object.setPrototypeOf(Legacy.prototype, EventEmitter.prototype);
const legacy = new Legacy();
let legacyHeard = false;
legacy.on("ping", () => (legacyHeard = true));
legacy.emit("ping");
eq(legacyHeard, true, "EventEmitter.call(this) initializes a subclass instance");

class Modern extends EventEmitter {}
const modern = new Modern();
eq(modern instanceof EventEmitter, true, "class extends EventEmitter");
eq(modern.emit("x"), false, "a subclass instance emits");

const app = function () {};
for (const key of Object.getOwnPropertyNames(EventEmitter.prototype)) {
  if (key !== "constructor") app[key] = EventEmitter.prototype[key];
}
let appHeard = false;
app.on("mount", () => (appHeard = true));
app.emit("mount");
eq(appHeard, true, "the prototype's methods work on any object");

// Module-level functions.
eq(listenerCount(e, "a"), 2, "events.listenerCount");
eq(getEventListeners(e, "a").length, 2, "getEventListeners");
eq(e.setMaxListeners(1), e, "setMaxListeners returns the emitter");
eq(e.getMaxListeners(), 1, "getMaxListeners");

// `once` resolves with the event's arguments, rejects on `error`, and removes its listeners.
const waited = new EventEmitter();
const firing = once(waited, "ready");
waited.emit("ready", 1, 2);
deepEq(await firing, [1, 2], "once resolves with the arguments");
eq(waited.listenerCount("ready") + waited.listenerCount("error"), 0, "once removes its listeners");
const failingOnce = once(waited, "ready");
waited.emit("error", new Error("boom"));
eq(await failingOnce.catch((error) => error.message), "boom", "once rejects on error");
eq(waited.listenerCount("ready"), 0, "a rejected once removes its listener");
const target = new EventTarget();
const fromTarget = once(target, "ping");
target.dispatchEvent(new Event("ping"));
eq((await fromTarget)[0].type, "ping", "once works on an EventTarget");
const aborter = new AbortController();
const aborted = once(waited, "never", { signal: aborter.signal });
aborter.abort("why");
const abortError = await aborted.catch((error) => error);
eq(abortError.name, "AbortError", "an aborted once rejects with an AbortError");
eq(abortError.code, "ABORT_ERR", "AbortError code");
eq(abortError.cause, "why", "the abort reason is the cause");
eq((await once(waited, "x", "bad options").catch((error) => error)).code, "ERR_INVALID_ARG_TYPE", "once rejects on invalid options");

// `on` iterates the events' arguments, including ones emitted before `next()` is called.
const stream = new EventEmitter();
const iterator = on(stream, "data", { close: ["end"] });
stream.emit("data", "a");
stream.emit("data", "b", "c");
setTimeout(() => {
  stream.emit("data", "d");
  stream.emit("end");
}, 1);
const collected = [];
for await (const args of iterator) collected.push(args);
deepEq(collected, [["a"], ["b", "c"], ["d"]], "on yields each event's arguments until a close event");
eq(stream.listenerCount("data") + stream.listenerCount("end") + stream.listenerCount("error"), 0, "on removes its listeners");
eq(iterator[Symbol.asyncIterator](), iterator, "on returns an async iterator");

const failing2 = new EventEmitter();
const failingIterator = on(failing2, "data");
const nextFailure = failingIterator.next();
failing2.emit("error", new Error("iteration failure"));
eq(await nextFailure.catch((error) => error.message), "iteration failure", "an error rejects the pending next()");
deepEq(await failingIterator.next(), { value: undefined, done: true }, "the iterator is done after an error");

const returned = on(new EventEmitter(), "data");
const pendingNext = returned.next();
deepEq(await returned.return(), { value: undefined, done: true }, "return() is done");
deepEq(await pendingNext, { value: undefined, done: true }, "return() finishes pending next() calls");
throws(() => on(new EventEmitter(), "data").throw("not an error"), "ERR_INVALID_ARG_TYPE", "throw() takes an Error");

// `once` and `on` take symbol names, validate the water marks, and stop pausing after closing.
const symbolName = Symbol("s");
const symbolEmitter = new EventEmitter();
const symbolOnce = once(symbolEmitter, symbolName);
symbolEmitter.emit(symbolName, 1, 2);
deepEq(await symbolOnce, [1, 2], "once with a symbol name");
throws(() => on(new EventEmitter(), "d", { highWaterMark: "x" }), "ERR_INVALID_ARG_TYPE", "highWaterMark type");
throws(() => on(new EventEmitter(), "d", { highWaterMark: 0 }), "ERR_OUT_OF_RANGE", "highWaterMark range");
const flow = [];
const paused = new EventEmitter();
paused.pause = () => flow.push("paused");
paused.resume = () => flow.push("resumed");
const pausedIterator = on(paused, "d", { close: ["end"], highWaterMark: 2, lowWaterMark: 1 });
paused.emit("d", 1);
paused.emit("d", 2);
paused.emit("d", 3);
paused.emit("end");
for await (const value of pausedIterator) flow.push(value[0]);
deepEq(flow, ["paused", 1, 2, 3], "a closed iterator doesn't resume the emitter");
