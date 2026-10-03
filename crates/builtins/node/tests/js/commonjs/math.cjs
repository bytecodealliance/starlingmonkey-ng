exports.add = (a, b) => a + b;
exports.delete = "reserved words are fine export names";
exports.meta = { filename: __filename, dirname: __dirname, thisIsExports: this === exports };
exports.json = require("./data.json");
exports.esm = require("./esm.mjs");
exports.path = require("path");
exports.nodePath = require("node:path");
