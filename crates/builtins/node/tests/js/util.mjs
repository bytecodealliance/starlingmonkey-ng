// `node:util` and `node:util/types`.

import util, { format, inspect, promisify, callbackify, inherits, types, isDeepStrictEqual } from "node:util";
import typesModule from "node:util/types";
import { eq, throws } from "./harness.mjs";

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
throws(() => util.formatWithOptions(() => {}, "x"), "ERR_INVALID_ARG_TYPE", "formatWithOptions function");
