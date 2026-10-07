// `node:util` and `node:util/types`.

import util, { format, inspect, promisify, callbackify, inherits, types, isDeepStrictEqual } from "node:util";
import typesModule from "node:util/types";
import assert from "node:assert";
import { deepEq, eq, throws } from "./harness.mjs";

eq(types, typesModule, "util.types is node:util/types");
eq(util.TextEncoder, TextEncoder, "TextEncoder");

// inspect
eq(inspect("a'b"), `"a'b"`, "string quoting");
eq(inspect("x"), "'x'", "single quotes");
eq(inspect(-0), "-0", "negative zero");
eq(inspect(10n), "10n", "bigint");
eq(inspect(Symbol("s")), "Symbol(s)", "symbol");
eq(inspect([1, "a", [2, [3, [4, [5]]]]]), "[ 1, 'a', [ 2, [ 3, [Array] ] ] ]", "nested arrays and depth");
eq(inspect({ a: 1, "b-c": { d: null } }), "{ a: 1, 'b-c': { d: null } }", "objects");
eq(inspect({}), "{}", "empty object");
eq(inspect([]), "[]", "empty array");
eq(inspect([1, , 3]), "[ 1, <1 empty item>, 3 ]", "holes");
eq(inspect(Object.create(null)), "[Object: null prototype] {}", "null prototype");
class Point { constructor() { this.x = 1; } }
eq(inspect(new Point()), "Point { x: 1 }", "class instance");
eq(inspect(Point), "[class Point]", "class");
eq(inspect(function named() {}), "[Function: named]", "function");
eq(inspect(() => {}), "[Function (anonymous)]", "anonymous function");
eq(inspect(async function af() {}), "[AsyncFunction: af]", "async function");
eq(inspect(new Map([["k", 1]])), "Map(1) { 'k' => 1 }", "Map");
eq(inspect(new Set([1, 2])), "Set(2) { 1, 2 }", "Set");
eq(inspect(new Date(0)), "1970-01-01T00:00:00.000Z", "Date");
eq(inspect(/a+/g), "/a+/g", "RegExp");
eq(inspect(new Number(3)), "[Number: 3]", "boxed number");
eq(inspect(Promise.resolve(4)), "Promise { 4 }", "resolved promise");
eq(inspect(new Promise(() => {})), "Promise { <pending> }", "pending promise");
eq(inspect(new Uint8Array([1, 2])), "Uint8Array(2) [ 1, 2 ]", "typed array");
eq(inspect(Buffer.from("hi")), "<Buffer 68 69>", "Buffer");
eq(inspect({ get g() { return 1; } }), "{ g: [Getter] }", "getter");
const circular = { name: "c" };
circular.self = circular;
eq(inspect(circular), "<ref *1> { name: 'c', self: [Circular *1] }", "circular");
eq(inspect({ a: { b: { c: { d: 1 } } } }, { depth: 0 }), "{ a: [Object] }", "depth option");
eq(inspect({ a: { b: { c: { d: 1 } } } }, { depth: null }), "{\n  a: { b: { c: { d: 1 } } }\n}", "unlimited depth");
eq(inspect({ [inspect.custom]: () => "custom!" }), "custom!", "inspect.custom");
eq(inspect.custom, Symbol.for("nodejs.util.inspect.custom"), "inspect.custom symbol");
const long = inspect({ alpha: "a".repeat(30), beta: "b".repeat(30), gamma: "c".repeat(30) });
eq(long, `{\n  alpha: '${"a".repeat(30)}',\n  beta: '${"b".repeat(30)}',\n  gamma: '${"c".repeat(30)}'\n}`, "line breaking");
eq(inspect(new Error("boom")).startsWith("Error: boom\n"), true, "error with stack");
function Es5Error(message) {
  this.message = message;
}
Es5Error.prototype = Object.create(Error.prototype);
Es5Error.prototype.constructor = Es5Error;
eq(inspect(new Es5Error("x")), "[Es5Error: x]", "an instanceof Error is formatted as an error");
eq(inspect(Object.create(Error.prototype, { cause: { get() { return 1; } } })), "[Error] { [cause]: [Getter] }", "Object.create(Error.prototype)");
{
  let error;
  try { util.styleText(1, "x"); } catch (e) { error = e; }
  eq(inspect(error).startsWith("TypeError [ERR_INVALID_ARG_VALUE]: "), true, "the stack header of an ERR_ error has its code");
  error = new assert.AssertionError({ actual: 1, expected: 2, operator: "strictEqual" });
  eq(inspect(error).startsWith("AssertionError [ERR_ASSERTION]: Expected"), true, "the stack header of an AssertionError has its code");
  eq(inspect(new TypeError("plain")).startsWith("TypeError: plain\n"), true, "an error without a code keeps the plain header");
}
// The errors of `ERR_` codes inherit `toString` and `kIsNodeError` from a prototype for their kind
// and code, as Node's `makeNodeErrorWithCode` classes do.
{
  const first = throws(() => Buffer.alloc("x"), "ERR_INVALID_ARG_TYPE");
  const second = throws(() => Buffer.alloc("y"), "ERR_INVALID_ARG_TYPE");
  const range = throws(() => Buffer.alloc(-1), "ERR_OUT_OF_RANGE");
  eq(Object.hasOwn(first, "toString"), false, "an ERR_ error has no own toString");
  eq(Object.getOwnPropertyNames(first).includes("code"), true, "an ERR_ error has an own code");
  eq(Object.getPrototypeOf(second), Object.getPrototypeOf(first), "errors of one code share a prototype");
  eq(Object.getPrototypeOf(range) === Object.getPrototypeOf(first), false, "errors of different codes have different prototypes");
  eq(first instanceof TypeError && range instanceof RangeError, true, "instanceof the kind");
  deepEq([first.constructor.name, range.constructor.name], ["TypeError", "RangeError"], "constructor is the kind's");
  first.code = "CHANGED";
  eq(String(first).startsWith("TypeError [ERR_INVALID_ARG_TYPE]: "), true, "toString uses the code the error was created with");
  eq(inspect(first).startsWith("TypeError [CHANGED]: "), true, "the stack header uses the current code");
  const decoder = throws(() => new TextDecoder("nope"), "ERR_ENCODING_NOT_SUPPORTED");
  const prototype = Object.getPrototypeOf(decoder);
  eq(Object.getPrototypeOf(prototype), RangeError.prototype, "the prototype inherits from the kind's prototype");
  deepEq(Reflect.ownKeys(prototype).map(String), ["constructor", "toString", "Symbol(kIsNodeError)"], "the prototype's own keys");
  eq(Object.getOwnPropertyDescriptor(prototype, "constructor").get.call(null), RangeError, "the constructor getter");
  eq(Object.hasOwn(decoder, "toString"), false, "an ERR_ error of a web class has no own toString");
  eq(String(decoder).startsWith("RangeError [ERR_ENCODING_NOT_SUPPORTED]: "), true, "an ERR_ error of a web class has the code in toString");
  const url = throws(() => new URL("x"), "ERR_INVALID_URL");
  eq(Object.getPrototypeOf(url), TypeError.prototype, "an error Node creates natively has its kind's prototype");
  eq(inspect(url).startsWith("TypeError: Invalid URL\n"), true, "an error Node creates natively has the plain header");
}

// format
eq(format("%s:%d:%i:%f", "a", 1.5, 2.9, "3.5"), "a:1.5:2:3.5", "format placeholders");
eq(format("%j", { a: [1] }), '{"a":[1]}', "format %j");
eq(format("%o", [1]).startsWith("[ 1"), true, "format %o");
eq(format("%O", { a: 1 }), "{ a: 1 }", "format %O");
eq(format("100%% %s", "done"), "100% done", "format %%");
eq(format("a", "b", 1, { c: 2 }), "a b 1 { c: 2 }", "format extra arguments");
eq(format(1, "x"), "1 x", "format without a template");
eq(format("%s", { a: { b: 1 } }), "{ a: [Object] }", "format %s with an object");
eq(format("%c%s", "color: red", "x"), "x", "format %c");

// promisify and callbackify
const delayed = (value, callback) => setTimeout(() => callback(null, value * 2), 0);
eq(await promisify(delayed)(21), 42, "promisify resolves");
const failing = (callback) => callback(new Error("nope"));
let rejected = null;
await promisify(failing)().catch((error) => (rejected = error.message));
eq(rejected, "nope", "promisify rejects");
const custom = () => {};
custom[promisify.custom] = () => "custom";
eq(promisify(custom)(), "custom", "promisify.custom");
const callbacked = await new Promise((resolve) =>
  callbackify(async (x) => x + 1)(1, (error, value) => resolve([error, value])),
);
eq(callbacked[0], null, "callbackify error");
eq(callbacked[1], 2, "callbackify value");

// inherits, deprecate, debuglog, types
function Base() {}
Base.prototype.hello = () => "hello";
function Derived() {}
inherits(Derived, Base);
eq(new Derived().hello(), "hello", "inherits");
eq(Derived.super_, Base, "super_");
let warnings = 0;
process.on("warning", () => warnings++);
const old = util.deprecate(() => "old", "old is deprecated", "DEP0");
eq(old(), "old", "deprecate calls through");
old();
await new Promise((resolve) => setTimeout(resolve, 0));
eq(warnings, 1, "deprecate warns once");
eq(util.debuglog("starling-test").enabled, false, "debuglog disabled");
eq(types.isDate(new Date()) && !types.isDate({}), true, "isDate");
eq(types.isPromise(Promise.resolve()), true, "isPromise");
eq(types.isUint8Array(Buffer.alloc(1)) && types.isTypedArray(new Float32Array(1)), true, "typed array checks");
eq(types.isAsyncFunction(async () => {}), true, "isAsyncFunction");
eq(types.isNativeError(new TypeError()), true, "isNativeError");
eq(types.isBoxedPrimitive(new String("")), true, "isBoxedPrimitive");
eq(types.isMap(new Map()) && types.isSet(new Set()), true, "isMap/isSet");

// deep equality
eq(isDeepStrictEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] }), true, "deep equal");
eq(isDeepStrictEqual({ a: 1 }, { a: "1" }), false, "strict types");
eq(isDeepStrictEqual(new Set([{ a: 1 }]), new Set([{ a: 1 }])), true, "sets of objects");
eq(isDeepStrictEqual(new Map([[1, "a"]]), new Map([[1, "b"]])), false, "map values");
eq(isDeepStrictEqual([1, 2], { 0: 1, 1: 2, length: 2 }), false, "arrays vs objects");
eq(isDeepStrictEqual(new Proxy([1], {}), [1]), true, "a proxied array equals an array");
eq(isDeepStrictEqual([], new Proxy([], {})), true, "an array equals a proxied array");
eq(util.isArray(new Proxy([], {})), true, "util.isArray sees through a proxy");
eq(isDeepStrictEqual(NaN, NaN), true, "NaN");
eq(isDeepStrictEqual(new Date(1), new Date(1)), true, "dates");
const a = { x: 1 };
a.self = a;
const b = { x: 1 };
b.self = b;
eq(isDeepStrictEqual(a, b), true, "cycles");

eq(util.stripVTControlCharacters("\u001b[31mred\u001b[39m"), "red", "stripVTControlCharacters");
const unvalidated = { validateStream: false };
eq(util.styleText("red", "x", unvalidated), "\u001b[31mx\u001b[39m", "styleText");
eq(
  util.styleText(["bold", "red"], "x", unvalidated),
  "\u001b[1m\u001b[31mx\u001b[39m\u001b[22m",
  "styleText nesting",
);
eq(util.getSystemErrorName(-2), "ENOENT", "getSystemErrorName");
throws(() => inherits(1, Base), "ERR_INVALID_ARG_TYPE", "inherits validates");

// Node.js behaviors the port follows
eq(inspect(1e21), "1e+21", "exponent notation");
eq(inspect(2 ** 63), "9223372036854776000", "large integers");
eq(format("%d", 1e20), "100000000000000000000", "%d large");
eq(inspect({ [Symbol("k")]: 1 }), "{ Symbol(k): 1 }", "symbol keys");
eq(inspect({ $x: 1 }), "{ '$x': 1 }", "key quoting");
eq(inspect("\x85\ud800"), "'\\x85\\ud800'", "C1 and lone surrogate escapes");
eq(inspect("\u{1F389}\u{1F389}", { maxStringLength: 1 }), "'\\ud83c'... 3 more characters", "maxStringLength counts code units");
eq(inspect("abc", { maxStringLength: -1 }), "'ab'... 4 more characters", "a negative maxStringLength counts from the end");
eq(inspect("x".repeat(1e6)).length, 10028, "a long string is cut at maxStringLength");
eq(inspect(Buffer.from([0, 15, 16, 171, 255])), "<Buffer 00 0f 10 ab ff>", "Buffer hex digits");
{
  const array = [1];
  array["01"] = "a";
  array[4294967295] = "max";
  eq(inspect(array), "[ 1, '01': 'a', '4294967295': 'max' ]", "an array's keys that aren't indices");
  eq(inspect(Object.assign(new Uint8Array(3), { x: 1 })), "Uint8Array(3) [ 0, 0, 0, x: 1 ]", "a typed array's keys that aren't indices");
}
{
  class Foo {
    get x() { return 1; }
    method() {}
  }
  class Bar extends Foo {
    y = 2;
  }
  eq(inspect(new Foo(), { showHidden: true }), "Foo { [x]: [Getter] }", "showHidden shows the prototype's properties");
  eq(inspect(new Bar(), { showHidden: true }), "Bar { y: 2, [x]: [Getter] }", "showHidden shows the properties of the prototypes below");
  eq(format("%o", new Bar()), "Bar { y: 2, [x]: [Getter] }", "%o shows the prototype's properties");
  eq(inspect(new Bar(), { showHidden: true, getters: true }), "Bar { y: 2, [x]: [Getter: 1] }", "prototype getters are called on the object");
  eq(inspect(new Bar()), "Bar { y: 2 }", "without showHidden the prototype's properties are left out");
}
eq(inspect(new Array(1000)), "[ <1000 empty items> ]", "sparse arrays");
eq(inspect(new Error("boom", { cause: "c" })).endsWith("[cause]: 'c'\n}"), true, "error cause");
eq(inspect(new (class Foo extends Map {})([[1, 2]])), "Foo(1) [Map] { 1 => 2 }", "Map subclass");
eq(inspect(Map.prototype), "Object [Map] {}", "Map.prototype");
eq(inspect({ [Symbol.toStringTag]: "Map" }), "{ Symbol(Symbol.toStringTag): 'Map' }", "spoofed tag");
eq(inspect(new ArrayBuffer(2)), "ArrayBuffer { [Uint8Contents]: <00 00>, [byteLength]: 2 }", "ArrayBuffer");
eq(inspect([1, 2], { showHidden: true }), "[ 1, 2, [length]: 2 ]", "showHidden array length");
eq(inspect({ a: { b: { c: Buffer.from("hi") } } }), "{ a: { b: { c: <Buffer 68 69> } } }", "Buffer past the depth");
eq(inspect({ get a() { throw new Error("g"); } }, { getters: true }).includes("<Inspection threw (Error: g"), true, "throwing getter");
const e2 = new Error("x");
e2.stack = "Error: x\n    at foo (bar.js:1:1)";
eq(inspect(e2), "Error: x\n    at foo (bar.js:1:1)", "assigned V8 stack");
e2.stack = undefined;
eq(inspect(e2), "[Error: x]", "missing stack");
eq(inspect({ a: { [inspect.custom](d, o, i) { return i({ y: 2 }, o); } } }), "{ a: { y: 2 } }", "custom inspect arguments");
eq(inspect({ a: { [inspect.custom]() { return "l1\nl2"; } } }), "{\n  a: l1\n  l2\n}", "custom inspect indentation");
eq(format("%s", { toString() { return "custom"; } }), "custom", "%s with a toString");
eq(format("%s", function f() {}), "function f() {}", "%s with a function");
eq(format("%d:%i:%f", "0x10", "42.9abc", "1.5abc"), "16:42:1.5", "%d %i %f parsing");
throws(() => format("%j", { toJSON() { throw new Error("boom"); } }), undefined, "%j rethrows");
eq(format("%j", (() => { const c = {}; c.c = c; return c; })()), "[Circular]", "%j circular");
const old2 = inspect.defaultOptions.depth;
inspect.defaultOptions.depth = 0;
eq(inspect({ a: { b: 1 } }), "{ a: [Object] }", "defaultOptions");
inspect.defaultOptions = { depth: old2 };
eq(isDeepStrictEqual(() => 1, () => 1), false, "functions by identity");
eq(isDeepStrictEqual(new WeakMap(), new WeakMap()), false, "weak maps by identity");
eq(isDeepStrictEqual(new Error("a", { cause: 1 }), new Error("a", { cause: 2 })), false, "error causes");
eq(isDeepStrictEqual(new Error("m"), new Error("m")), true, "errors");
const u8 = new Uint8Array([1]);
u8.x = 1;
eq(isDeepStrictEqual(u8, new Uint8Array([1])), false, "typed array properties");
eq(isDeepStrictEqual({ [Symbol.toStringTag]: "Date" }, { [Symbol.toStringTag]: "Date" }), true, "spoofed tags compare as objects");
const many = (n, reverse) => { const s = new Set(); for (let i = 0; i < n; i++) s.add({ i: reverse ? n - 1 - i : i }); return s; };
eq(isDeepStrictEqual(many(2000, false), many(2000, true)), true, "large sets");
let fulfilled = await promisify((cb) => cb(0, "v"))();
eq(fulfilled, "v", "promisify resolves for a falsy error");
eq(Object.keys(promisify(function f(cb) {})).length, 0, "promisify.custom is not enumerable");
eq(promisify(function f(a, cb) {}).length, 2, "promisify copies length");
eq(callbackify(async function af(a) {}).name, "afCallbackified", "callbackify name");
eq(types.isDate({ [Symbol.toStringTag]: "Date" }), false, "isDate brand check");
eq(types.isMap(new (class extends Map { get [Symbol.toStringTag]() { return "X"; } })()), true, "isMap brand check");
eq(types.isNumberObject(new Number(1)) && !types.isNumberObject({ [Symbol.toStringTag]: "Number" }), true, "isNumberObject brand check");
eq(util.styleText("none", "x"), "x", "styleText none");
throws(() => util.styleText("red", 1), "ERR_INVALID_ARG_TYPE", "styleText text");
throws(() => util.styleText(1, "x"), "ERR_INVALID_ARG_VALUE", "styleText format");
throws(() => util.getSystemErrorName("x"), "ERR_INVALID_ARG_TYPE", "getSystemErrorName type");
throws(() => util.stripVTControlCharacters(1), "ERR_INVALID_ARG_TYPE", "stripVTControlCharacters type");
eq(util.stripVTControlCharacters("\u001b(B x"), " x", "stripVTControlCharacters charset");
eq(util.stripVTControlCharacters("\u00e9\u009b31m\u6f22\u001b]0;t\u0007y\u001b[1;2"), "\u00e9\u6f22y", "stripVTControlCharacters around non-ASCII text");
eq(util.stripVTControlCharacters(`\u001b[1${";".repeat(1e6)}m`), "", "stripVTControlCharacters of a million parameters");
eq(util.stripVTControlCharacters(`\u001b[1${";".repeat(1e6)}é`).length, 1e6 + 1, "a million parameters without a final character");
eq(util.stripVTControlCharacters("\u001b[12;34;x"), ";x", "parameters before a character that isn't final");
eq(util.stripVTControlCharacters("\u001b[12345m"), "m", "parameters of more than four digits");
let thrown;
try {
  inspect({ a: { [inspect.custom]() { throw 1; } } });
} catch (error) {
  thrown = error;
}
eq(thrown, 1, "inspect passes on a primitive thrown by a custom inspector");
const byteLength = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "byteLength");
Object.defineProperty(ArrayBuffer.prototype, "byteLength", { get() { return {}; }, configurable: true });
const hidden = inspect(new Uint8Array(1), { showHidden: true });
Object.defineProperty(ArrayBuffer.prototype, "byteLength", byteLength);
eq(hidden.includes("[buffer]: ArrayBuffer { [byteLength]: [object Object] }"), true, "a byteLength getter returning an object");
throws(() => util.formatWithOptions(() => {}, "x"), "ERR_INVALID_ARG_TYPE", "formatWithOptions function");

// Values nested deeper than the native stack allows.
const nested = (make) => {
  let value = null;
  for (let i = 0; i < 100000; i++) value = make(value);
  return value;
};
const interrupted = "Inspection interrupted prematurely. Maximum call stack size exceeded.]";
eq(inspect(nested((a) => ({ a })), { depth: null }).includes(`a: [Object: ${interrupted}`), true, "deep object");
eq(inspect(nested((a) => [a]), { depth: null }).includes(`[Array: ${interrupted}`), true, "deep array");
eq(inspect(nested((a) => ({ __proto__: null, a })), { depth: null }).includes(`[[Object: null prototype]: ${interrupted}`), true, "deep null-prototype object");
throws(() => inspect(nested((a) => new Proxy(a ?? {}, {})), { showProxy: true, depth: null }), RangeError, "deep proxy chain");
eq(throws(() => isDeepStrictEqual(nested((a) => [a]), nested((a) => [a])), RangeError).message, "Maximum call stack size exceeded", "deep isDeepStrictEqual of arrays");
eq(throws(() => isDeepStrictEqual(nested((a) => ({ a })), nested((a) => ({ a }))), RangeError).message, "Maximum call stack size exceeded", "deep isDeepStrictEqual of objects");

// Depths of proxies and of values inspected without one.
eq(format("%o", new Proxy({}, {})), "Proxy [ {}, {} ]", "%o of a proxy");
eq(inspect({ a: { b: new Proxy({ c: {} }, {}) } }, { showProxy: true }), "{ a: { b: Proxy [ [Object], {} ] } }", "nested proxy");
eq(inspect(Object.defineProperty(function () {}, "name", { value: { x: 1 } })), "[Function: {\n  x: 1\n}]", "function named by an object");
eq(inspect(Object.assign(new Error("e"), { stack: { s: 1 } })), "[Error: e\n    {\n      s: 1\n    }]", "error with an object stack");

// Proxies, detached buffers and symbol names.
const traps = [];
const tracing = new Proxy({ a: 1 }, { get: (target, key) => (traps.push(key), target[key]) });
eq(format("%s", tracing), "{ a: 1 }", "%s of a proxy");
eq(traps.length, 0, "%s doesn't run proxy traps");
const revocable = Proxy.revocable({}, {});
revocable.revoke();
eq(format("%s", revocable.proxy), "<Revoked Proxy>", "%s of a revoked proxy");
const detached = new ArrayBuffer(4);
structuredClone(detached, { transfer: [detached] });
eq(inspect(detached), "ArrayBuffer { (detached), [byteLength]: 0 }", "detached ArrayBuffer");
class SymbolNamed {}
Object.defineProperty(SymbolNamed, "name", { value: Symbol("s") });
eq(inspect(new SymbolNamed()), "Symbol(s) {}", "class named by a symbol");

// `parseArgs` rejects `options` and option configurations that are arrays or functions.
throws(() => util.parseArgs({ args: [], options: [] }), "ERR_INVALID_ARG_TYPE", "parseArgs rejects array options");
throws(() => util.parseArgs({ args: [], options: { a: () => {} } }), "ERR_INVALID_ARG_TYPE", "parseArgs rejects a function option");

// A custom inspection method of a proxied object that returns the proxy falls back to the target.
{
  const proxy = new Proxy({ a: 1, [inspect.custom]() { return proxy; } }, {});
  eq(inspect(proxy, { breakLength: Infinity }), "{ a: 1, Symbol(nodejs.util.inspect.custom): [Function: [nodejs.util.inspect.custom]] }", "custom inspect returning the proxy");
}
// A `sorted` function is `Array.prototype.sort`'s comparator.
eq(inspect({ b: 1, a: 2, c: 0 }, { sorted: (x, y) => y.localeCompare(x) }), "{ c: 0, b: 1, a: 2 }", "sorted comparator");
throws(() => inspect({ b: 1, a: 2 }, { sorted() { throw new RangeError("x"); } }), RangeError, "sorted comparator exceptions propagate");
throws(() => inspect({ b: 1, a: 2 }, { sorted: "yes" }), TypeError, "sorted that isn't a function");
eq(inspect({ [inspect.custom](depth, options) { return typeof options.sorted; } }, { sorted: () => 0 }), "function", "custom inspection gets the comparator");
// A fractional `maxArrayLength` shows the entries below it and reports the fractional remainder.
eq(inspect([1, 2, 3], { maxArrayLength: 1.5 }), "[ 1, 2, ... 1.5 more items ]", "fractional maxArrayLength");
eq(inspect(new Set([1, 2, 3]), { maxArrayLength: 1.5 }), "Set(3) { 1, 2, ... 1.5 more items }", "fractional maxArrayLength of a set");
eq(inspect([1, , 3, 4], { maxArrayLength: 1.5 }), "[ 1, <1 empty item>, 3, <1 empty item> ]", "fractional maxArrayLength of a sparse array");
eq(inspect([1, 2, 3], { maxArrayLength: NaN }), "[  ]", "NaN maxArrayLength");
throws(() => inspect(new Uint8Array(3), { maxArrayLength: 1.5 }), RangeError, "fractional maxArrayLength of a typed array");
eq(inspect(new ArrayBuffer(3), { maxArrayLength: 1.5, breakLength: Infinity }), "ArrayBuffer { [Uint8Contents]: <00 ... 1.5 more bytes>, [byteLength]: 3 }", "fractional maxArrayLength of an ArrayBuffer");
{
  const text = "x";
  eq(format(text), text, "format of one string");
}

// `inspect.defaultOptions` is sealed, and assigning an array or a function throws.
eq(Object.isSealed(inspect.defaultOptions), true, "defaultOptions is sealed");
throws(() => { inspect.defaultOptions = []; }, "ERR_INVALID_ARG_TYPE", "defaultOptions rejects arrays");
throws(() => { inspect.defaultOptions = () => {}; }, "ERR_INVALID_ARG_TYPE", "defaultOptions rejects functions");
throws(() => { inspect.defaultOptions = { bogus: 1 }; }, TypeError, "defaultOptions rejects unknown keys");

// `types.isGeneratorObject` is a brand check.
eq(types.isGeneratorObject(Object.create((function* () {}).prototype)), false, "an object inheriting from a generator");
eq(types.isGeneratorObject((function* () {})()), true, "a generator");
eq(types.isGeneratorObject((async function* () {})()), true, "an async generator");

// `deprecate`'s wrapper has a `prototype` of its own when the function has none.
{
  const wrapped = util.deprecate(() => {}, "m");
  eq("prototype" in wrapped, true, "deprecate wrapper has a prototype");
  eq(wrapped.prototype.constructor, wrapped, "deprecate wrapper prototype's constructor");
}

// `getCallSites` validates its arguments.
throws(() => util.getCallSites("1"), "ERR_INVALID_ARG_TYPE", "getCallSites frameCount type");
eq(throws(() => util.getCallSites(1.5), "ERR_OUT_OF_RANGE").message.includes("an integer"), true, "getCallSites frameCount integer");
throws(() => util.getCallSites({ sourceMap: 1 }), "ERR_INVALID_ARG_TYPE", "getCallSites options.sourceMap");
throws(() => util.getCallSites(2, { sourceMap: 1 }), "ERR_INVALID_ARG_TYPE", "getCallSites options.sourceMap after frameCount");
throws(() => util.getCallSites(null), "ERR_INVALID_ARG_TYPE", "getCallSites null options");
eq(util.getCallSites({}).length > 0, true, "getCallSites options only");

// `debuglog` calls its callback with the logging function on the first call.
{
  let got;
  const logger = util.debuglog("starling-test-disabled", (debug) => { got = debug; });
  eq(logger.name, "logger", "logger name");
  logger("x");
  eq(typeof got, "function", "debuglog callback");
  eq(got.enabled, false, "debuglog callback gets a disabled logger");
}

// `parseArgs` follows Node's `parseArgs` for its configuration and its stored values.
{
  const values = (result) => ({ ...result.values });
  deepEq(values(util.parseArgs({ args: ["--b=true"], options: { b: { type: "boolean" } }, strict: false })), { b: "true" }, "parseArgs keeps a boolean option's inline value");
  deepEq(Object.getOwnPropertyNames(util.parseArgs({ args: ["--__proto__", "x"], strict: false }).values), [], "parseArgs ignores __proto__");
  eq(throws(() => util.parseArgs({ args: ["--no-b=1"], options: { b: { type: "boolean", short: "b" } }, allowNegative: true }), "ERR_PARSE_ARGS_INVALID_OPTION_VALUE").message, "Option '-b, --b' does not take an argument", "parseArgs negated option with a value");
  eq(throws(() => util.parseArgs({ args: ["-f", "-x"], options: { f: { type: "string", short: "f" } } }), "ERR_PARSE_ARGS_INVALID_OPTION_VALUE").message.split("\n")[0], "Option '-f' argument is ambiguous.", "parseArgs ambiguous short option");
  deepEq(values(util.parseArgs(1)), {}, "parseArgs of a primitive config");
  throws(() => util.parseArgs(null), TypeError, "parseArgs of null");
  eq(Array.isArray(util.parseArgs({ args: null, strict: false }).positionals), true, "parseArgs args null");
  for (const name of ["strict", "allowPositionals", "allowNegative", "tokens"]) {
    throws(() => util.parseArgs({ args: [], [name]: 1 }), "ERR_INVALID_ARG_TYPE", `parseArgs validates ${name}`);
  }
  throws(() => util.parseArgs({ args: [], options: { x: { type: "string", multiple: 1 } } }), "ERR_INVALID_ARG_TYPE", "parseArgs validates multiple");
  throws(() => util.parseArgs({ args: [], options: { x: { type: "string", short: 1 } } }), "ERR_INVALID_ARG_TYPE", "parseArgs validates short");
  throws(() => util.parseArgs({ args: [], options: { x: { type: "string", short: "😀" } } }), "ERR_INVALID_ARG_VALUE", "parseArgs short is one UTF-16 unit");
  eq(throws(() => util.parseArgs({ args: [], options: { x: { type: "string", multiple: true, default: [1] } } }), "ERR_INVALID_ARG_TYPE").message.startsWith('The "options.x.default[0]" property must be of type string'), true, "parseArgs default element");
}
