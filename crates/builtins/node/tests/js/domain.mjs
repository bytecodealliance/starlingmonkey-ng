// `node:domain`: the domain stack, explicit and implicit binding, and the routing of errors thrown
// in callbacks and emitted by emitters to the domain's `error` event.

import domain from "node:domain";
import EventEmitter from "node:events";
import { AsyncResource } from "node:async_hooks";
import fs from "node:fs";
import { eq, deepEq, throws } from "./harness.mjs";

eq(domain.create, domain.createDomain, "create is createDomain");
eq(EventEmitter.usingDomains, true, "usingDomains");
eq(process.domain, null, "no domain is active at first");
eq(domain.active, null, "domain.active starts null");
throws(() => domain.Domain(), TypeError, "Domain needs new");
deepEq(
  Object.keys(domain.Domain.prototype),
  ["members", "_errorHandler", "enter", "exit", "add", "remove", "run", "intercept", "bind"],
  "prototype members",
);

const d = domain.create();
eq(d instanceof EventEmitter, true, "a domain is an emitter");
eq(d.domain, null, "a domain has no domain");
eq(Object.getOwnPropertyDescriptor(d, "domain").enumerable, false, "domain isn't enumerable");
deepEq(d.members, [], "no members");
deepEq(d.eventNames(), ["removeListener", "newListener"], "listeners of a new domain");

// The stack.
const [a, b, c] = [domain.create(), domain.create(), domain.create()];
a.enter();
b.enter();
c.enter();
eq(domain._stack.length, 3, "three entered");
eq(process.domain, c, "the last entered is active");
eq(domain.active, c, "domain.active follows");
b.exit();
eq(domain._stack.length, 1, "exit removes the domains entered after it");
eq(process.domain, a, "the domain below is active");
domain.create().exit();
eq(domain._stack.length, 1, "exiting a domain that isn't entered does nothing");
a.exit();
eq(process.domain, undefined, "exit with an empty stack leaves undefined");

eq(d.run(function (x, y) { return [this, x + y]; }, 1, 2)[1], 3, "run passes arguments");
eq(d.run(function () { return this; }), d, "run calls with the domain as this");
eq(process.domain, undefined, "run exits");
throws(
  () => d.run(() => new AsyncResource("sync").runInAsyncScope(() => { throw new TypeError("sync"); })),
  TypeError,
  "an exception propagates out of runInAsyncScope and run",
);
eq(process.domain, d, "run leaves the domain entered when its callback throws");
d.exit();

const bound = d.bind(function (x) { return [process.domain, this, x]; });
eq(bound.name, "runBound", "bind's name");
eq(bound.domain, d, "a bound function has a domain");
const receiver = {};
const [boundDomain, boundThis, boundArg] = bound.call(receiver, 5);
eq(boundDomain, d, "bind enters the domain");
eq(boundThis, receiver, "bind keeps this");
eq(boundArg, 5, "bind passes arguments");

// Explicit binding and error events.
const errors = [];
d.on("error", (error) => errors.push(error));
const emitter = new EventEmitter();
d.add(emitter);
eq(emitter.domain, d, "add sets domain");
eq(d.members.length, 1, "add adds a member");
d.add(emitter);
eq(d.members.length, 1, "adding again does nothing");
const emitted = new Error("emitted");
eq(emitter.emit("error", emitted), false, "an error emitted on a domain returns false");
eq(errors[0], emitted, "the domain gets the emitter's error");
eq(emitted.domain, d, "error.domain");
eq(emitted.domainEmitter, emitter, "error.domainEmitter");
eq(emitted.domainThrown, false, "error.domainThrown");
emitter.emit("error");
eq(errors[1].code, "ERR_UNHANDLED_ERROR", "a missing error becomes ERR_UNHANDLED_ERROR");
let inListener;
emitter.on("data", () => (inListener = process.domain));
emitter.emit("data");
eq(inListener, d, "listeners run in the emitter's domain");
d.remove(emitter);
eq(emitter.domain, null, "remove clears domain");
eq(d.members.length, 0, "remove removes the member");

const intercepted = d.intercept((x, y) => [process.domain, x, y]);
deepEq(intercepted(null, 1, 2).slice(1), [1, 2], "intercept drops the first argument");
const interceptedError = new Error("intercepted");
intercepted(interceptedError);
eq(errors[2], interceptedError, "intercept emits an Error argument on the domain");
eq(interceptedError.domainThrown, false, "intercepted error is not thrown");
eq(typeof interceptedError.domainBound, "function", "error.domainBound");

// Implicit binding.
let implicit;
d.run(() => (implicit = new EventEmitter()));
eq(implicit.domain, d, "emitters created in a domain belong to it");
eq(new EventEmitter().domain, null, "emitters created outside have domain null");
d.run(() => {
  eq(new AsyncResource("test").domain, d, "an async resource gets the active domain");
  eq(setTimeout(() => {}, 0).domain, d, "a timeout gets the active domain");
});

// Errors thrown in callbacks go to the domain the callback was scheduled in.
const thrown = await new Promise((resolve) => {
  const caught = [];
  const t = domain.create();
  t.on("error", (error) => {
    caught.push(error);
    if (caught.length === 4) resolve(caught);
  });
  t.run(() => {
    process.nextTick(() => { throw new Error("tick"); });
    setTimeout(() => { throw new Error("timeout"); }, 1);
    setImmediate(() => { throw new Error("immediate"); });
    Promise.reject(new Error("rejection"));
  });
});
deepEq(
  thrown.map((error) => error.message).sort(),
  ["immediate", "rejection", "tick", "timeout"],
  "each kind of callback routes its error to its domain",
);
eq(thrown.find((error) => error.message === "tick").domainThrown, true, "thrown errors");

const fromFs = await new Promise((resolve) => {
  const f = domain.create();
  f.on("error", (error) => resolve([error.message, error.domainThrown]));
  f.run(() => fs.stat(".", () => { throw new Error("fs"); }));
});
deepEq(fromFs, ["fs", true], "an I/O callback routes its error to its domain");

const fromMicrotask = await new Promise((resolve) => {
  const m = domain.create();
  m.on("error", () => resolve("domain"));
  process.once("uncaughtException", (error) => resolve([error.message, process.domain]));
  m.run(() => queueMicrotask(() => { throw new Error("microtask"); }));
});
deepEq(fromMicrotask, ["microtask", null], "a queueMicrotask callback's exception skips the domain");
deepEq(process.listeners("uncaughtException"), [], "the stack clearing listener is removed");

const observed = await new Promise((resolve) => {
  const p = domain.create();
  p.run(() => {
    Promise.resolve().then(() => {
      setTimeout(() => resolve([process.domain === p, domain._stack.length]), 0);
    });
  });
});
deepEq(observed, [true, 1], "a callback runs with its domain entered");
eq(process.domain, undefined, "the stack is empty between callbacks");

// A domain without `error` listeners doesn't capture exceptions.
eq(process.hasUncaughtExceptionCaptureCallback(), false, "no capture outside domains");
d.run(() => eq(process.hasUncaughtExceptionCaptureCallback(), true, "capture in a domain"));
throws(
  () => process.setUncaughtExceptionCaptureCallback(null),
  "ERR_DOMAIN_CANNOT_SET_UNCAUGHT_EXCEPTION_CAPTURE",
  "domains own the capture callback",
);
