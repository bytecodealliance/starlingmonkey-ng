exports.loaded = false;
const b = require("./cycle-b.cjs");
exports.sawB = b.done;
exports.loaded = true;
