// A CommonJS entry script: `require` and the wrapper's bindings exist, and top-level `return`
// is allowed.
const path = require("path");
if (typeof module !== "object" || module.exports !== exports) throw new Error("module binding");
if (path.basename(__filename) !== "commonjs-entry.cjs") throw new Error(`__filename: ${__filename}`);
const { add } = require("./commonjs/math.cjs");
if (add(1, 1) !== 2) throw new Error("required module");
return;
