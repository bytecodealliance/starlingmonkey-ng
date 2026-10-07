// `node:test`: running tests, suites and hooks, the reporter's output and summary, and mocks.
//
// Some of the tests fail on purpose. The reporter's output is captured from `process.stdout.write`
// and compared once `process` emits `beforeExit`, after the runner wrote the summary. The module
// waits for that comparison, and resets the exit code the failures set.

import test, { describe, it, before, after, beforeEach, afterEach, mock, getTestContext } from "node:test";
import { createRequire } from "node:module";
import { deepEq, eq, throws } from "./harness.mjs";

const require = createRequire(import.meta.url);
eq(require("node:test"), test, "require returns test");
eq(test.test, test, "test.test");
eq(it, test, "it is test");
eq(describe, test.suite, "describe is suite");
eq(typeof test.skip, "function", "test.skip");
eq(getTestContext(), undefined, "no context outside tests");

const output = [];
const write = process.stdout.write;
process.stdout.write = (chunk) => {
  output.push(String(chunk));
  return true;
};

const log = [];

before(() => log.push("root before"));
after(() => log.push("root after"));
beforeEach((t) => log.push(`beforeEach ${t.name}`));
afterEach((t) => log.push(`afterEach ${t.name}`));

test("passes", (t) => {
  eq(t.name, "passes", "t.name");
  eq(getTestContext(), t, "getTestContext");
  t.diagnostic("a note");
});

test("fails", () => {
  throw new Error("expected failure");
});

test("skipped", { skip: "not now" }, () => {
  log.push("skipped body ran");
});

test.todo("todo", () => {
  throw new Error("todo failure");
});

test("skips itself", (t) => {
  t.skip();
});

// A test waits for the subtests it started until none is pending. The one started after that is
// cancelled.
test("subtests", async (t) => {
  t.test("first", () => {});
  await t.test("second", async () => {
    await new Promise((resolve) => setTimeout(resolve, 1));
  });
  t.test("not awaited", (t) => {
    t.after(() => log.push("not awaited after"));
  });
});

test("failing subtest", async (t) => {
  const result = await t.test("inner", () => {
    throw new Error("inner failure");
  });
  eq(result, undefined, "t.test resolves with undefined");
});

test("callback", (t, done) => {
  setTimeout(done, 1);
});

test("callback error", (t, done) => {
  setTimeout(() => done(new Error("callback failure")), 1);
});

test("plan", (t) => {
  t.plan(2);
  t.assert.ok(true);
  t.assert.strictEqual(1, 1);
});

test("plan mismatch", (t) => {
  t.plan(2);
  t.assert.ok(true);
});

test("plan twice", (t) => {
  t.plan(1);
  t.plan(1);
});

test("timeout", { timeout: 10 }, () => new Promise(() => {}));

test("uncaught", (t, done) => {
  setTimeout(() => {
    throw new Error("thrown from a timer");
  }, 1);
});

test("only subtests", async (t) => {
  t.runOnly(true);
  await t.test("left out", () => {
    log.push("left out ran");
  });
  await t.test("kept", { only: true }, () => {});
});

test("signal", (t) => {
  eq(t.signal.aborted, false, "signal during the test");
  log.push(t.signal);
});

describe("suite", () => {
  before(() => log.push("suite before"));
  after(() => log.push("suite after"));
  beforeEach(() => log.push("suite beforeEach"));
  it("in suite", (t) => {
    eq(t.fullName, "suite > in suite", "t.fullName");
  });
  describe("nested", () => {
    it("deep", () => {});
  });
  it.skip("skipped in suite", () => {});
});

// The suite's timeout cancels the test that runs, whose hooks still run, and the one that didn't
// start.
describe("suite timeout", { timeout: 20 }, () => {
  it("cancelled", async (t) => {
    t.after(() => log.push("cancelled after"));
    await new Promise((resolve) => setTimeout(resolve, 200));
  });
  it("never started", () => {});
});

test("mock.fn", (t) => {
  const fn = t.mock.fn((a, b) => a + b);
  eq(fn(1, 2), 3, "calls the implementation");
  eq(fn.mock.callCount(), 1, "callCount");
  deepEq(fn.mock.calls[0].arguments, [1, 2], "arguments");
  eq(fn.mock.calls[0].result, 3, "result");
  fn.mock.mockImplementationOnce(() => "once");
  eq(fn(1, 1), "once", "mockImplementationOnce");
  eq(fn(1, 1), 2, "back to the implementation");
  fn.mock.mockImplementation(() => "new");
  eq(fn(), "new", "mockImplementation");
  fn.mock.resetCalls();
  eq(fn.mock.callCount(), 0, "resetCalls");
  const throwing = t.mock.fn(() => {
    throw new TypeError("mock threw");
  });
  throws(() => throwing(), TypeError, "a throwing mock throws");
  eq(throwing.mock.calls[0].error.message, "mock threw", "the error is recorded");
  const limited = t.mock.fn(
    () => "original",
    () => "mocked",
    { times: 1 },
  );
  eq(limited(), "mocked", "times: the mocked call");
  eq(limited(), "original", "times: restored");
  class Point {
    constructor(x) {
      this.x = x;
    }
  }
  const MockPoint = t.mock.fn(Point);
  eq(new MockPoint(3).x, 3, "constructing a mock");
  eq(MockPoint.mock.calls[0].target, Point, "the construction's target");
  eq(MockPoint.name, "Point", "mocks keep the function's properties");
});

const object = {
  value: 1,
  method(x) {
    return x * this.value;
  },
  get prop() {
    return "real";
  },
};

test("mock.method", (t) => {
  const spy = t.mock.method(object, "method");
  eq(object.method(2), 2, "spies call the original");
  eq(spy.mock.calls[0].this, object, "this");
  t.mock.method(object, "method", () => "replaced");
  eq(object.method(2), "replaced", "a replacement");
  t.mock.getter(object, "prop", () => "mocked");
  eq(object.prop, "mocked", "mock.getter");
  throws(() => t.mock.method(object, "value"), "ERR_INVALID_ARG_VALUE", "only methods");
  const badName = throws(() => t.mock.method(object, 1), "ERR_INVALID_ARG_TYPE", "method names");
  eq(badName.message.startsWith('The "methodName" argument must be one of type string or symbol.'), true, "the methodName message");
  const fixed = Object.defineProperty({}, "m", { value() {}, writable: false, configurable: false });
  eq(throws(() => t.mock.method(fixed, "m", () => {}), TypeError, "a fixed method").message, "Cannot redefine property: m", "V8's message");
});

test("mock restored after the test", () => {
  eq(object.method(2), 2, "t.mock is reset");
  eq(object.prop, "real", "getters are restored");
  const fn = mock.fn();
  fn();
  eq(fn.mock.callCount(), 1, "the module's mock tracker");
  mock.reset();
});

const finished = new Promise((resolve) => process.on("beforeExit", resolve));
await finished;
process.stdout.write = write;

const expected = `
✔ passes (*ms)
ℹ a note
✖ fails (*ms)
﹣ skipped (*ms) # not now
⚠ todo (*ms) # TODO
﹣ skips itself (*ms) # SKIP
▶ subtests
  ✔ first (*ms)
  ✔ second (*ms)
  ✖ not awaited (*ms)
✖ subtests (*ms)
▶ failing subtest
  ✖ inner (*ms)
✖ failing subtest (*ms)
✔ callback (*ms)
✖ callback error (*ms)
✔ plan (*ms)
✖ plan mismatch (*ms)
✖ plan twice (*ms)
✖ timeout (*ms)
✖ uncaught (*ms)
▶ only subtests
  ✔ kept (*ms)
✔ only subtests (*ms)
✔ signal (*ms)
▶ suite
  ✔ in suite (*ms)
  ▶ nested
    ✔ deep (*ms)
  ✔ nested (*ms)
  ﹣ skipped in suite (*ms) # SKIP
✔ suite (*ms)
▶ suite timeout
  ✖ cancelled (*ms)
  ✖ never started
✖ suite timeout (*ms)
✔ mock.fn (*ms)
✔ mock.method (*ms)
✔ mock restored after the test (*ms)
ℹ tests 29
ℹ suites 3
ℹ pass 13
ℹ fail 8
ℹ cancelled 4
ℹ skipped 3
ℹ todo 1
ℹ duration_ms *

✖ failing tests:

✖ fails (*ms)
  Error: expected failure
      at *

⚠ todo (*ms) # TODO
  Error: todo failure
      at *

✖ not awaited (*ms)
  'test did not finish before its parent and was cancelled'

✖ inner (*ms)
  Error: inner failure
      at *

✖ callback error (*ms)
  Error: callback failure
      at *

✖ plan mismatch (*ms)
  'plan expected 2 assertions but received 1'

✖ plan twice (*ms)
  'cannot set plan more than once'

✖ timeout (*ms)
  'test timed out after 10ms'

✖ uncaught (*ms)
  Error: thrown from a timer
      at *

✖ cancelled (*ms)
  'test did not finish before its parent and was cancelled'

✖ never started
  'test did not finish before its parent and was cancelled'

✖ suite timeout (*ms)
  'test timed out after 20ms'
`.slice(1);

// The durations and stack frames vary, and errors have one stack frame line per frame. Node.js
// adds the location of each failed test, which this runtime leaves out.
const normalize = (text) =>
  text
    .replace(/^test at .*\n/gm, "")
    .replace(/\(\d+(\.\d+)?ms\)/g, "(*ms)")
    .replace(/duration_ms [\d.]+/, "duration_ms *")
    .replace(/( {6}at ).*(\n {6}at .*)*/g, "$1*");
eq(normalize(output.join("")), expected, "the reporter's output");

deepEq(
  log.filter((entry) => typeof entry === "string" && /^(root|suite) /.test(entry)),
  ["root before", "suite before", "suite beforeEach", "suite beforeEach", "suite after", "root after"],
  "the hooks of the root and the suite",
);
eq(log.includes("beforeEach passes") && log.includes("afterEach passes"), true, "root beforeEach and afterEach");
eq(log.includes("beforeEach skipped"), false, "beforeEach skips skipped tests");
eq(log.includes("beforeEach inner"), true, "beforeEach runs for subtests");
eq(log.includes("skipped body ran"), false, "skipped tests don't run");
eq(log.includes("left out ran"), false, "runOnly leaves out subtests without only");
const cancelledHooks = ["afterEach not awaited", "not awaited after", "afterEach cancelled", "cancelled after"];
deepEq(
  log.filter((entry) => cancelledHooks.includes(entry)),
  cancelledHooks,
  "cancelled tests run their remaining hooks once",
);
eq(log.find((entry) => typeof entry === "object").aborted, true, "the signal is aborted after the test");
eq(process.exitCode, 1, "failures set the exit code");
process.exitCode = 0;
