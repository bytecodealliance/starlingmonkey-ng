// `node:vm`.

import vm from "node:vm";
import util from "node:util";
import { deepEq, eq, throws } from "./harness.mjs";

/** The value `fn` throws, which can be of another realm. */
function caught(fn, message) {
  try {
    fn();
  } catch (error) {
    return error;
  }
  throw new Error(`${message}: did not throw`);
}

// Contextified objects.
const sandbox = { a: 1, console };
eq(vm.createContext(sandbox), sandbox, "createContext returns the object");
eq(vm.isContext(sandbox), true, "contextified");
eq(vm.isContext({}), false, "not contextified");
eq(vm.createContext(sandbox), sandbox, "a context is created once");
throws(() => vm.isContext(1), "ERR_INVALID_ARG_TYPE", "isContext of a primitive");
throws(() => vm.createContext(null), "ERR_INVALID_ARG_TYPE", "createContext of null");

eq(vm.runInContext("a + 1", sandbox), 2, "names resolve to the contextified object");
vm.runInContext("x = 1", sandbox);
eq(sandbox.x, 1, "undeclared assignments go to the contextified object");
vm.runInContext("var y = 2; function f() { return 3; }", sandbox);
eq(sandbox.y, 2, "var declarations go to the contextified object");
eq(sandbox.f(), 3, "function declarations go to the contextified object");
vm.runInContext("let z = 4", sandbox);
eq(sandbox.z, undefined, "let declarations stay in the context");
eq(vm.runInContext("z", sandbox), 4, "let declarations persist");
eq(vm.runInContext("globalThis.a", sandbox), 1, "the global reads the contextified object");
vm.runInContext("globalThis.w = 5", sandbox);
eq(sandbox.w, 5, "assignments to the global go to the contextified object");
vm.runInContext("Object.defineProperty(globalThis, 'v', { value: 6, enumerable: true })", sandbox);
eq(sandbox.v, 6, "properties defined on the global go to the contextified object");
eq(vm.runInContext("typeof Function('return w')", sandbox), "function", "Function works");
eq(vm.runInContext("Function('return w')()", sandbox), 5, "Function code reads the object");
eq(vm.runInContext("(0, eval)('var u = 7'); u", sandbox), 7, "indirect eval");
eq(sandbox.u, 7, "indirect eval declarations go to the contextified object");
eq(caught(() => vm.runInContext("nope", sandbox), "unknown names").name, "ReferenceError", "unknown names throw");
eq(caught(() => vm.runInContext("'use strict'; q = 1", sandbox), "strict").name, "ReferenceError", "strict assignment");
eq("q" in sandbox, false, "a failed strict assignment adds nothing");

// Declarations that the contextified object refuses go to the context's global.
const frozen = vm.createContext(Object.freeze({}));
eq(vm.runInContext("var x = 1; typeof x", frozen), "number", "var in a frozen object");
eq(vm.runInContext("x", frozen), 1, "the var persists");
eq(vm.runInContext("var x; x", frozen), 1, "a repeated var keeps the value");
eq(vm.runInContext("function f() { return 3; } f()", frozen), 3, "function in a frozen object");
eq(Object.getOwnPropertyNames(frozen).length, 0, "the frozen object is unchanged");
eq(vm.runInNewContext("var x = 1; a = 2; x + a", Object.preventExtensions({ a: 1 })), 3, "var in a non-extensible object");
const sealed = Object.seal({ y: 1 });
eq(vm.runInNewContext("var y = 5; y", sealed), 5, "var of a sealed object's property");
eq(sealed.y, 5, "the sealed object's property is assigned");
const readOnly = Object.defineProperty({}, "g", { value: 1, enumerable: true });
eq(vm.runInNewContext("var g = 2; g", readOnly), 1, "var of a read-only property");

// The context's own builtins.
eq(vm.runInNewContext("Object") === Object, false, "a context has its own Object");
eq(vm.runInNewContext("[]") instanceof Array, false, "arrays of a context");
eq(Array.isArray(vm.runInNewContext("[]")), true, "Array.isArray of a context's array");
eq(vm.runInNewContext("Object", { Object: 1 }), 1, "the object shadows the builtins");
eq(
  vm.runInNewContext(
    "typeof process + typeof setTimeout + typeof Buffer + typeof require + typeof URL",
  ),
  "undefinedundefinedundefinedundefinedundefined",
  "no runtime globals",
);
eq(vm.runInNewContext("typeof console.log"), "function", "a context has a console");
eq(vm.runInNewContext("console.log('not printed')"), undefined, "whose methods do nothing");
const window = vm.createContext({});
window.window = window;
eq(vm.runInContext("globalThis.window === globalThis", window), true, "self references");

// Values across realms.
const values = vm.runInNewContext(
  "[new Date(0), new Map([[1, 2]]), /x/g, new Uint8Array(2), Promise.resolve(), { a: [1] }]",
);
eq(util.types.isDate(values[0]), true, "isDate");
eq(util.types.isMap(values[1]), true, "isMap");
eq(util.types.isRegExp(values[2]), true, "isRegExp");
eq(util.types.isUint8Array(values[3]), true, "isUint8Array");
eq(util.types.isPromise(values[4]), true, "isPromise");
eq(util.inspect(values[5]), "{ a: [ 1 ] }", "inspect");
eq(Buffer.isBuffer(values[3]), false, "Buffer.isBuffer");
eq(util.types.isNativeError(vm.runInNewContext("new TypeError()")), true, "isNativeError");

// vm.runInThisContext.
eq(vm.runInThisContext("var vmThisVar = 9; vmThisVar"), 9, "runInThisContext");
eq(globalThis.vmThisVar, 9, "runInThisContext declares globals");

// Errors.
const error = caught(() => vm.runInNewContext("throw new Error('boom')", {}, "file.vm"), "errors");
eq(error instanceof Error, false, "errors are of the context's realm");
eq(error.stack.startsWith("file.vm:1\nthrow new Error('boom')\n"), true, "the arrow message");
const syntax = caught(() => new vm.Script("(", { filename: "f.js" }), "syntax errors");
eq(syntax.name, "SyntaxError", "a SyntaxError");
eq(syntax.stack.startsWith("f.js:1\n(\n"), true, "syntax errors get the arrow message");
const plain = caught(
  () => vm.runInNewContext("throw new Error('d')", {}, { displayErrors: false, filename: "d.js" }),
  "displayErrors",
);
eq(plain.stack.startsWith("d.js:1\n"), false, "displayErrors: false leaves the stack");
const offset = throws(
  () => vm.runInThisContext("\n\nthrow new Error('lo')", { filename: "lo.js", lineOffset: 10 }),
  Error,
  "lineOffset",
);
eq(offset.stack.startsWith("lo.js:13\n"), true, "lineOffset moves the line");

// vm.Script.
const script = new vm.Script("count = (typeof count === 'number' ? count : 0) + 1");
const counter = vm.createContext({});
script.runInContext(counter);
script.runInContext(counter);
eq(counter.count, 2, "a script runs repeatedly");
eq(script.runInNewContext({ count: 10 }), 11, "runInNewContext");
deepEq(Object.keys(script), ["sourceURL", "sourceMapURL"], "own keys");
eq(new vm.Script("1\n//# sourceMappingURL=foo.map").sourceMapURL, "foo.map", "sourceMapURL");
eq(new vm.Script("1", { cachedData: Buffer.alloc(1) }).cachedDataRejected, true, "cached data");
eq(script.createCachedData().length, 0, "createCachedData");
eq(Buffer.isBuffer(script.createCachedData()), true, "createCachedData returns a Buffer");
throws(() => vm.Script("1"), TypeError, "Script needs new");
class Sub extends vm.Script {}
eq(new Sub("2").runInThisContext(), 2, "Script can be extended");
throws(() => script.runInContext({}), "ERR_INVALID_ARG_TYPE", "runInContext needs a context");
throws(() => new vm.Script("1", { lineOffset: 1.5 }), "ERR_OUT_OF_RANGE", "lineOffset");
throws(() => script.runInThisContext({ timeout: 0 }), "ERR_OUT_OF_RANGE", "timeout");

// Timeouts.
const timeout = throws(
  () => vm.runInNewContext("while (true) {}", {}, { timeout: 20 }),
  "ERR_SCRIPT_EXECUTION_TIMEOUT",
  "timeout",
);
eq(timeout.message, "Script execution timed out after 20ms", "timeout message");
const nested = vm.createContext({ vm, inner: vm.createContext({}) });
throws(
  () => vm.runInContext("vm.runInContext('for (;;) {}', inner, { timeout: 5000 })", nested, { timeout: 20 }),
  "ERR_SCRIPT_EXECUTION_TIMEOUT",
  "an enclosing timeout",
);
eq(vm.runInNewContext("1 + 1", {}, { timeout: 1000 }), 2, "a script within its timeout");

// vm.compileFunction.
const add = vm.compileFunction("return a + b + c", ["a", "b"], { contextExtensions: [{ c: 100 }] });
eq(add(1, 2), 103, "contextExtensions");
eq(add.name, "", "compiled functions are anonymous");
const parsing = vm.createContext({ q: 5 });
const inContext = vm.compileFunction("return q + typeof process", [], { parsingContext: parsing });
eq(inContext(), "5undefined", "parsingContext");
throws(() => vm.compileFunction("", "x"), "ERR_INVALID_ARG_TYPE", "params must be an array");
throws(() => vm.compileFunction("", [], { parsingContext: {} }), "ERR_INVALID_ARG_TYPE", "parsingContext");

// Contexts without a contextified object.
const bare = vm.createContext(vm.constants.DONT_CONTEXTIFY);
eq(vm.isContext(bare), true, "DONT_CONTEXTIFY creates a context");
eq(vm.runInContext("globalThis", bare), bare, "whose global is the context");
vm.runInContext("var g = 1", bare);
eq(bare.g, 1, "declarations go to the global");

// Code generation.
const noEval = vm.createContext({}, { codeGeneration: { strings: false } });
eq(caught(() => vm.runInContext("eval('1')", noEval), "eval").name, "EvalError", "strings: false disallows eval");
eq(caught(() => vm.runInContext("eval('1')", noEval), "eval").message, "Code generation from strings disallowed for this context", "the EvalError's message");
eq(vm.runInContext("eval('1')", vm.createContext({})), 1, "eval works by default");

// Dynamic import.
let rejected;
await vm.runInNewContext("import('node:fs')").catch((e) => (rejected = e));
eq(rejected?.code, "ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING", "import() needs a callback");

// Constants and measureMemory.
eq(Object.isFrozen(vm.constants), true, "constants are frozen");
eq(typeof vm.constants.DONT_CONTEXTIFY, "symbol", "DONT_CONTEXTIFY");
const memory = await vm.measureMemory({ execution: "eager" });
eq(typeof memory.total.jsMemoryEstimate, "number", "measureMemory");
throws(() => vm.measureMemory({ mode: "x" }), "ERR_INVALID_ARG_VALUE", "measureMemory mode");

// `runInContext` and `runInNewContext` copy array and function options, as Node.js's spread does.
eq(vm.runInContext("1 + 1", vm.createContext({}), []), 2, "array options");
eq(vm.runInNewContext("1 + 1", {}, () => {}), 2, "function options");

// The arrow message of a Node.js error has the error's code in its header.
const nodeError = caught(() => vm.runInThisContext("Buffer.alloc('x')", { filename: "alloc.js" }), "a Node.js error");
eq(nodeError.stack.includes("\nTypeError [ERR_INVALID_ARG_TYPE]: "), true, "the header of a Node.js error");
