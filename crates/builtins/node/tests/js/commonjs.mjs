// CommonJS modules, `require`, and importing CommonJS from ES modules.

import { createRequire, builtinModules, isBuiltin } from "node:module";
import Module from "module";
import math, { add, delete as deleted } from "./commonjs/math.cjs";
import greet from "./commonjs/replace.cjs";
import dual from "dual";
import ambiguousCjs from "./commonjs/ambiguous/cjs.js";
import { kind as ambiguousEsmKind } from "./commonjs/ambiguous/esm.js";
import { eq, throws } from "./harness.mjs";

eq(add(2, 3), 5, "named exports of a CommonJS module");
eq(math.add, add, "the default export is module.exports");
eq(deleted, "reserved words are fine export names", "reserved-word export names");
eq(math.meta.filename.endsWith("/commonjs/math.cjs"), true, "__filename");
eq(math.meta.dirname.endsWith("/commonjs"), true, "__dirname");
eq(math.meta.thisIsExports, true, "this is exports");
eq(math.json.answer, 42, "require of JSON");
eq(math.esm.fromEsm, "esm", "require of an ES module returns its namespace");
eq(math.esm.default, "esm default", "the namespace has the default export");
eq(math.path, math.nodePath, "require of a builtin, with and without the prefix");
eq(math.path.join("a", "b"), "a/b", "a required builtin works");
eq(greet("you"), "hello you", "module.exports replaced by a function");
eq(greet.extra, 1, "properties of a replaced module.exports");
eq(ambiguousCjs.kind, "commonjs", "a .js file without ES module syntax is CommonJS");
eq(ambiguousEsmKind, "module", "a .js file with ES module syntax is an ES module");

const require = createRequire(import.meta.url);
eq(typeof require, "function", "createRequire");
const a = require("./commonjs/cycle-a.cjs");
eq(a.loaded, true, "a cycle finishes");
eq(a.sawB, true, "the inner module ran");
eq(require("./commonjs/cycle-b.cjs").sawPartialA, true, "a cycle sees partial exports");
eq(require("./commonjs/math.cjs"), math, "require and import share one module.exports");
eq(require("dual"), "required", "require uses the require condition");
eq(dual, "imported", "import uses the import condition");
eq(require.resolve("./commonjs/math.cjs").endsWith("/commonjs/math.cjs"), true, "require.resolve");
eq(require.resolve("fs"), "fs", "require.resolve of a builtin");
eq(require.resolve("node:fs"), "node:fs", "require.resolve keeps the prefix");
const cachedKey = require.resolve("./commonjs/replace.cjs");
eq(require.cache[cachedKey].exports, greet, "require.cache holds the module");
delete require.cache[cachedKey];
const reloaded = require("./commonjs/replace.cjs");
eq(reloaded === greet, false, "deleting from require.cache reloads the module");
throws(() => require("./commonjs/missing.cjs"), "MODULE_NOT_FOUND", "a missing module");
throws(() => require(42), "ERR_INVALID_ARG_TYPE", "require validates its argument");

eq(Module, createRequire.Module ?? Module, "node:module's default export is Module");
eq(Module.createRequire, createRequire, "createRequire is a static of Module");
eq(isBuiltin("fs") && isBuiltin("node:fs") && !isBuiltin("nope"), true, "isBuiltin");
eq(builtinModules.includes("fs") && builtinModules.includes("node:test"), true, "builtinModules");
eq(typeof new Module("/x").exports, "object", "new Module");
