// CommonJS modules, `require`, and importing CommonJS from ES modules.

import { createRequire, builtinModules, isBuiltin } from "node:module";
import Module from "module";
import math, { add, delete as deleted } from "./commonjs/math.cjs";
import greet from "./commonjs/replace.cjs";
import dual from "dual";
import ambiguousCjs from "./commonjs/ambiguous/cjs.js";
import { kind as ambiguousEsmKind } from "./commonjs/ambiguous/esm.js";
import { deepEq, eq, throws } from "./harness.mjs";

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
throws(() => require("dual/missing"), "ERR_PACKAGE_PATH_NOT_EXPORTED", "a subpath exports omits");
throws(() => require("node:nonexistent"), "ERR_UNKNOWN_BUILTIN_MODULE", "an unknown builtin");
for (const [specifier, code] of [
  ["nonexistent_pkg", "ERR_MODULE_NOT_FOUND"],
  ["./commonjs/missing.mjs", "ERR_MODULE_NOT_FOUND"],
  ["dual/missing", "ERR_PACKAGE_PATH_NOT_EXPORTED"],
  ["node:nonexistent", "ERR_UNKNOWN_BUILTIN_MODULE"],
]) {
  const error = await import(specifier).then(() => null, (error) => error);
  eq(error?.code, code, `the error of import("${specifier}")`);
}
eq(import.meta.resolve("./nope.js"), new URL("./nope.js", import.meta.url).href, "import.meta.resolve of a missing file");
throws(() => import.meta.resolve("nonexistent_pkg"), "ERR_MODULE_NOT_FOUND", "import.meta.resolve of a missing package");
throws(() => require(42), "ERR_INVALID_ARG_TYPE", "require validates its argument");

eq(Module, createRequire.Module ?? Module, "node:module's default export is Module");
eq(Module.createRequire, createRequire, "createRequire is a static of Module");
eq(isBuiltin("fs") && isBuiltin("node:fs") && !isBuiltin("nope"), true, "isBuiltin");
eq(builtinModules.includes("fs") && builtinModules.includes("node:test"), true, "builtinModules");
eq(typeof new Module("/x").exports, "object", "new Module");

// `require` goes through `module.require`, `Module._load`, `Module._resolveFilename`,
// `Module._extensions` and `module._compile`, so replacing them takes effect.
{
  const calls = [];
  const wrap = (object, name, label, index) => {
    const original = object[name];
    object[name] = function (...args) {
      calls.push(`${label} ${args[index].split("/").pop()}`);
      return original.apply(this, args);
    };
    return () => { object[name] = original; };
  };
  const restores = [
    wrap(Module.prototype, "require", "require", 0),
    wrap(Module, "_load", "_load", 0),
    wrap(Module, "_resolveFilename", "resolve", 0),
    wrap(Module._extensions, ".js", "extension", 1),
    wrap(Module.prototype, "_compile", "compile", 1),
  ];
  const path = require.resolve("./commonjs/replace.cjs");
  delete require.cache[path];
  require("./commonjs/replace.cjs");
  require("os");
  for (const restore of restores) restore();
  eq(
    calls.join(),
    [
      "resolve replace.cjs",
      "require replace.cjs",
      "_load replace.cjs",
      "resolve replace.cjs",
      "extension replace.cjs",
      "compile replace.cjs",
      "require os",
      "_load os",
      "resolve os",
    ].join(),
    "require calls the replaceable steps",
  );
  const virtual = Module._resolveFilename;
  Module._resolveFilename = (request, ...rest) => request === "virtual" ? path : virtual(request, ...rest);
  eq(typeof require("virtual"), "function", "a replaced _resolveFilename resolves requires");
  Module._resolveFilename = virtual;
  Module._extensions[".txt"] = (module, filename) => {
    module.exports = `txt ${filename.split("/").pop()}`;
  };
  eq(require("./commonjs/note.txt"), "txt note.txt", "a registered extension's loader runs");
  eq(require("./commonjs/note.spec.txt"), "txt note.spec.txt", "the last extension without a longer one");
  delete require.cache[require.resolve("./commonjs/note.spec.txt")];
  Module._extensions[".spec.txt"] = (module) => {
    module.exports = "spec";
  };
  eq(require("./commonjs/note.spec.txt"), "spec", "the longest registered extension wins");
  delete Module._extensions[".txt"];
  delete Module._extensions[".spec.txt"];
  eq(typeof Module._load("path", null, false).join, "function", "Module._load loads builtins");
  eq(Module._resolveFilename("node:fs", null), "node:fs", "_resolveFilename returns builtin names");
}

// `Module._nodeModulePaths` lists the `node_modules` directories Node.js searches, and `module.load`
// sets `module.paths` through it.
deepEq(Module._nodeModulePaths("/a/node_modules/b"), ["/a/node_modules/b/node_modules", "/a/node_modules", "/node_modules"], "_nodeModulePaths");
deepEq(Module._nodeModulePaths("/"), ["/node_modules"], "_nodeModulePaths of the root");
{
  const nodeModulePaths = Module._nodeModulePaths;
  Module._nodeModulePaths = () => ["patched"];
  const patched = new Module("/x.js");
  try {
    patched.load(require.resolve("./commonjs/math.cjs"));
  } finally {
    Module._nodeModulePaths = nodeModulePaths;
  }
  deepEq(patched.paths, ["patched"], "load calls Module._nodeModulePaths");
}

// A `require` that `require.cache` answers adds the module to the parent's `children`.
{
  const parent = new Module("/parent.js");
  parent.filename = new URL("./parent.js", import.meta.url).pathname;
  parent.require("./commonjs/math.cjs");
  parent.require("./commonjs/math.cjs");
  eq(parent.children.length, 1, "a cached module is added to children once");
  eq(parent.children[0].exports, math, "the cached module is the child");
}
