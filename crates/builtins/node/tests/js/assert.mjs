// `node:assert` and `node:assert/strict`.

import assert, { AssertionError, strictEqual, deepStrictEqual, throws as assertThrows } from "node:assert";
import strict from "node:assert/strict";
import { eq, throws } from "./harness.mjs";

eq(typeof assert, "function", "assert is callable");
eq(assert.ok, assert, "assert.ok is assert");
eq(assert.strict, strict, "assert.strict is node:assert/strict");
eq(assert.AssertionError, AssertionError, "AssertionError export");

assert(true);
assert.ok(1);
const falsy = throws(() => assert(0), "ERR_ASSERTION", "falsy value");
eq(falsy instanceof AssertionError && falsy instanceof Error, true, "AssertionError is an Error");
eq(falsy.name, "AssertionError", "name");
eq(falsy.actual, 0, "actual");
eq(falsy.generatedMessage, true, "generatedMessage");
eq(throws(() => assert.ok(false, "custom")).message, "custom", "custom message");
const custom = new TypeError("mine");
eq(throws(() => assert.ok(false, custom)), custom, "an Error message is thrown as is");
throws(() => assert.ok(), "ERR_ASSERTION", "no value");

assert.equal(1, "1");
throws(() => assert.equal(1, 2), "ERR_ASSERTION", "equal");
assert.notEqual(1, 2);
strictEqual(NaN, NaN);
const strictError = throws(() => strictEqual(1, "1"), "ERR_ASSERTION", "strictEqual");
eq(strictError.message, "Expected values to be strictly equal:\n\n1 !== '1'\n", "strictEqual message");
eq(strictError.operator, "strictEqual", "operator");
assert.notStrictEqual(1, "1");
assert.deepEqual({ a: 1 }, { a: "1" });
deepStrictEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] });
throws(() => deepStrictEqual({ a: 1 }, { a: "1" }), "ERR_ASSERTION", "deepStrictEqual");
assert.notDeepStrictEqual({ a: 1 }, { a: "1" });
throws(() => strict.equal(1, "1"), "ERR_ASSERTION", "strict.equal is strictEqual");
throws(() => strict.deepEqual({ a: 1 }, { a: "1" }), "ERR_ASSERTION", "strict.deepEqual is deepStrictEqual");

assertThrows(() => {
  throw new TypeError("bad thing");
}, TypeError);
assertThrows(() => {
  throw new Error("bad thing");
}, /bad/);
assertThrows(
  () => {
    throw Object.assign(new Error("x"), { code: "E1" });
  },
  { code: "E1", message: "x" },
);
assertThrows(() => {
  throw new Error("v");
}, (error) => error.message === "v");
throws(() => assertThrows(() => {}), "ERR_ASSERTION", "missing exception");
eq(
  throws(() => assertThrows(() => {}, "should throw")).message,
  "Missing expected exception: should throw",
  "missing exception message",
);
const wrongClass = throws(() =>
  assertThrows(() => {
    throw new RangeError("r");
  }, TypeError),
);
eq(wrongClass.code, "ERR_ASSERTION", "an error of the wrong class fails");
eq(wrongClass.message, 'The error is expected to be an instance of "TypeError". Received "RangeError"\n\nError message:\n\nr', "wrong class message");
assert.doesNotThrow(() => {});
throws(() => assert.doesNotThrow(() => {
  throw new Error("x");
}), "ERR_ASSERTION", "doesNotThrow");

await assert.rejects(Promise.reject(new Error("no")), /no/);
await assert.rejects(async () => {
  throw new TypeError("t");
}, TypeError);
let missing = null;
await assert.rejects(Promise.resolve(1)).catch((error) => (missing = error.code));
eq(missing, "ERR_ASSERTION", "missing rejection");
await assert.doesNotReject(Promise.resolve(1));

assert.match("abc", /b/);
throws(() => assert.match("abc", /x/), "ERR_ASSERTION", "match");
assert.doesNotMatch("abc", /x/);
throws(() => assert.match("abc", "b"), "ERR_INVALID_ARG_TYPE", "match needs a RegExp");
assert.ifError(null);
const ifErr = new Error("if");
const ifFailure = throws(() => assert.ifError(ifErr), "ERR_ASSERTION", "ifError");
eq(ifFailure.message, "ifError got unwanted exception: if", "ifError message");
eq(ifFailure.actual, ifErr, "ifError actual");

// Node.js behaviors the port follows
eq(throws(() => assertThrows(() => {}, TypeError)).message, "Missing expected exception (TypeError).", "missing class name");
throws(() => assertThrows(() => { throw new Error("m"); }, "m"), "ERR_AMBIGUOUS_ARGUMENT", "ambiguous message");
throws(() => assertThrows(() => {}, 1), "ERR_INVALID_ARG_TYPE", "invalid expected");
throws(() => assertThrows(() => { throw new Error("m"); }, {}), "ERR_INVALID_ARG_VALUE", "empty object");
assertThrows(() => { throw new Error("m"); }, new Error("m"));
throws(() => assertThrows(() => { throw new TypeError("bad"); }, { name: "TypeError", extra: undefined }), "ERR_ASSERTION", "missing key");
eq(throws(() => assertThrows(() => { throw new TypeError("bad"); }, () => false)).message, 'The validation function is expected to return "true". Received false\n\nCaught error:\n\nTypeError: bad', "validation function");
eq(throws(() => assert.doesNotThrow(() => { throw new TypeError("bad"); }, "custom")).message, 'Got unwanted exception: custom\nActual message: "bad"', "doesNotThrow message");
eq(throws(() => assert.doesNotThrow(() => { throw new TypeError("bad"); }, RangeError)) instanceof TypeError, true, "doesNotThrow rethrows a non-matching error");
let code = null;
await assert.rejects(1).catch((error) => (code = error.code));
eq(code, "ERR_INVALID_ARG_TYPE", "rejects target");
await assert.rejects(() => 1).catch((error) => (code = error.code));
eq(code, "ERR_INVALID_RETURN_VALUE", "rejects return value");
await assert.rejects(Promise.reject(new TypeError("x")), RangeError).catch((error) => (code = error.code));
eq(code, "ERR_ASSERTION", "rejects wrong class");
throws(() => assert.doesNotMatch(1, /1/), "ERR_ASSERTION", "doesNotMatch a non-string");
const generated = new AssertionError({ actual: 1, expected: 2, operator: "strictEqual" });
eq(generated.generatedMessage, true, "generatedMessage");
eq(generated.message, "Expected values to be strictly equal:\n\n1 !== 2\n", "generated message");
class SubError extends AssertionError {}
eq(new SubError({ message: "q" }) instanceof SubError, true, "subclass");
throws(() => new AssertionError(), "ERR_INVALID_ARG_TYPE", "options required");
eq(throws(() => deepStrictEqual({ a: 1, b: [1, 2] }, { a: 1, b: [1, 3] })).message, "Expected values to be strictly deep-equal:\n+ actual - expected\n\n  {\n    a: 1,\n    b: [\n      1,\n+     2\n-     3\n    ]\n  }\n", "deepStrictEqual diff");
eq(throws(() => strictEqual(1, 2, "custom")).message, "custom\n\n1 !== 2\n", "custom message keeps the diff");
eq(assert.strict.strict, assert.strict, "strict.strict");
eq(throws(() => assert.fail()).message, "Failed", "fail");
eq(throws(() => assert.fail("boom")).message, "boom", "fail message");
