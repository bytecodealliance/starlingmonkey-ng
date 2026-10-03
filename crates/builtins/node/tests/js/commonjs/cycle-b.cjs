const a = require("./cycle-a.cjs");
exports.sawPartialA = a.loaded === false;
exports.done = true;
