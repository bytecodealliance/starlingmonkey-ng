// `node:vm` contexts are collected once nothing references them. Runs with `--expose-gc`.

import vm from "node:vm";
import { eq } from "./harness.mjs";

const collected = new Set();
const registry = new FinalizationRegistry((held) => collected.add(held));
(() => {
  for (let i = 0; i < 4; i++) {
    const sandbox = { i };
    vm.createContext(sandbox);
    vm.runInContext("var x = i; let y = [x]; z = () => y;", sandbox, { timeout: 1000 });
    registry.register(vm.runInContext("globalThis", sandbox), `global ${i}`);
    registry.register(sandbox, `sandbox ${i}`);
    const bare = vm.createContext(vm.constants.DONT_CONTEXTIFY);
    vm.runInContext("var w = 1", bare);
    registry.register(bare, `bare ${i}`);
  }
})();
const kept = vm.createContext({});
registry.register(vm.runInContext("globalThis", kept), "kept");
for (let i = 0; i < 3; i++) {
  globalThis.gc();
  await new Promise((resolve) => setTimeout(resolve, 10));
}
for (let i = 0; i < 4; i++) {
  eq(collected.has(`global ${i}`), true, `context ${i}'s global is collected`);
  eq(collected.has(`sandbox ${i}`), true, `context ${i}'s object is collected`);
  eq(collected.has(`bare ${i}`), true, `context ${i} without an object is collected`);
}
eq(collected.has("kept"), false, "a referenced context is kept");
eq(vm.runInContext("typeof Object", kept), "function", "and works");
