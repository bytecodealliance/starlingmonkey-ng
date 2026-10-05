// `node:path`, with cases from Node's `test/parallel/test-path-*.js` for POSIX.

import path, { basename, join, posix, sep } from "node:path";
import barePath from "path";
import pathPosix from "node:path/posix";
import { deepEq, eq, throws } from "./harness.mjs";

eq(barePath, path, "the bare and prefixed specifiers import one module");
eq(pathPosix, path, "path/posix is path");
eq(posix, path, "path.posix is the module object");
eq(path.posix, path, "path.posix is the module object");
eq(sep, "/", "sep");
eq(path.delimiter, ":", "delimiter");
eq(basename, path.basename, "named exports are the default export's properties");

for (const [args, expected] of [
  [["/var/lib", "../", "file/"], "/var/file"],
  [["/var/lib", "/../", "file/"], "/file"],
  [["/some/dir", ".", "/absolute/"], "/absolute"],
  [["/foo/tmp.3/", "../tmp.3/cycles/root.js"], "/foo/tmp.3/cycles/root.js"],
  [["/a/b", "c", "..", "d"], "/a/b/d"],
]) {
  eq(path.resolve(...args), expected, `resolve(${args})`);
}
eq(path.resolve("a"), `${process.cwd()}/a`, "resolve against the cwd");

for (const [args, expected] of [
  [[".", "x/b", "..", "/b/c.js"], "x/b/c.js"],
  [[], "."],
  [["/.", "x/b", "..", "/b/c.js"], "/x/b/c.js"],
  [["/foo", "../../../bar"], "/bar"],
  [["foo", "../../../bar"], "../../bar"],
  [["foo/", "../../../bar"], "../../bar"],
  [["foo/x", "../../../bar"], "../bar"],
  [["foo/x", "./bar"], "foo/x/bar"],
  [["foo/x/", "./bar"], "foo/x/bar"],
  [["./"], "./"],
  [[".", "./"], "./"],
  [[".", ".", "."], "."],
  [["", ""], "."],
  [["foo", ""], "foo"],
  [["", "/foo"], "/foo"],
  [["/", "/foo"], "/foo"],
  [["/", "//foo"], "/foo"],
  [["/", "", "/foo"], "/foo"],
  [[" /foo"], " /foo"],
  [[" ", "foo"], " /foo"],
  [["/", "foo"], "/foo"],
]) {
  eq(join(...args), expected, `join(${JSON.stringify(args)})`);
}

for (const [input, expected] of [
  ["./fixtures///b/../b/c.js", "fixtures/b/c.js"],
  ["/foo/../../../bar", "/bar"],
  ["a//b//../b", "a/b"],
  ["a//b//./c", "a/b/c"],
  ["a//b//.", "a/b"],
  ["/a/b/c/../../../x/y/z", "/x/y/z"],
  ["///..//./foo/.//bar", "/foo/bar"],
  ["bar/foo../../", "bar/"],
  ["bar/foo../..", "bar"],
  ["bar/foo../../baz", "bar/baz"],
  ["../../../foo/../../../bar", "../../../../../bar"],
  ["../.../.././.../../../bar", "../../bar"],
  ["", "."],
  ["/", "/"],
]) {
  eq(path.normalize(input), expected, `normalize(${input})`);
}

for (const [input, expected] of [
  ["/a/b/", "/a"],
  ["/a/b", "/a"],
  ["/a", "/"],
  ["", "."],
  ["/", "/"],
  ["////", "/"],
  ["//a", "//"],
  ["foo", "."],
]) {
  eq(path.dirname(input), expected, `dirname(${input})`);
}

for (const [args, expected] of [
  [["/dir/basename.ext"], "basename.ext"],
  [["/basename.ext"], "basename.ext"],
  [["basename.ext"], "basename.ext"],
  [["basename.ext/"], "basename.ext"],
  [["basename.ext//"], "basename.ext"],
  [["aaa/bbb", "/bbb"], "bbb"],
  [["aaa/bbb", "a/bbb"], "bbb"],
  [["aaa/bbb", "bbb"], "bbb"],
  [["aaa/bbb//", "bbb"], "bbb"],
  [["aaa/bbb", "bb"], "b"],
  [["aaa/bbb", "b"], "bb"],
  [["/aaa/bbb", "/bbb"], "bbb"],
  [["/aaa/bbb", "a/bbb"], "bbb"],
  [["/aaa/bbb", "bbb"], "bbb"],
  [["/aaa/bbb//", "bbb"], "bbb"],
  [["/aaa/", "aaa"], "aaa"],
  [["/aaa/b", "b"], "b"],
  [["/a/b", "b"], "b"],
  [["//a", "a"], "a"],
  [["file.js", ".js"], "file"],
  [["file.js.old", ".js.old"], "file"],
  [["a", "a"], ""],
  [["/日本語/ファイル.txt", ".txt"], "ファイル"],
]) {
  eq(path.basename(...args), expected, `basename(${JSON.stringify(args)})`);
}

for (const [input, expected] of [
  ["", ""],
  ["/path/to/file", ""],
  ["/path/to/file.ext", ".ext"],
  ["/path.to/file.ext", ".ext"],
  ["/path.to/file", ""],
  ["/path.to/.file", ""],
  ["/path.to/.file.ext", ".ext"],
  ["/path/to/f.ext", ".ext"],
  ["/path/to/..ext", ".ext"],
  ["/path/to/..", ""],
  ["file", ""],
  ["file.ext", ".ext"],
  [".file", ""],
  [".file.ext", ".ext"],
  ["/file", ""],
  ["/file.ext", ".ext"],
  ["/.file", ""],
  ["/.file.ext", ".ext"],
  [".path/file.ext", ".ext"],
  ["file.ext.ext", ".ext"],
  ["file.", "."],
  [".", ""],
  ["./", ""],
  [".file.ext", ".ext"],
  [".file", ""],
  [".file.", "."],
  [".file..", "."],
  ["..", ""],
  ["../", ""],
  ["..file.ext", ".ext"],
  ["..file", ".file"],
  ["..file.", "."],
  ["..file..", "."],
  ["...", "."],
  ["...ext", ".ext"],
  ["....", "."],
  ["file.ext/", ".ext"],
  ["file.ext//", ".ext"],
  ["file/", ""],
  ["file//", ""],
  ["file./", "."],
  ["file.//", "."],
]) {
  eq(path.extname(input), expected, `extname(${input})`);
}

for (const [from, to, expected] of [
  ["/var/lib", "/var", ".."],
  ["/var/lib", "/bin", "../../bin"],
  ["/var/lib", "/var/lib", ""],
  ["/var/lib", "/var/apache", "../apache"],
  ["/var/", "/var/lib", "lib"],
  ["/", "/var/lib", "var/lib"],
  ["/foo/test", "/foo/test/bar/package.json", "bar/package.json"],
  ["/Users/a/web/b/test/mails", "/Users/a/web/b", "../.."],
  ["/foo/bar/baz-quux", "/foo/bar/baz", "../baz"],
  ["/foo/bar/baz", "/foo/bar/baz-quux", "../baz-quux"],
  ["/baz-quux", "/baz", "../baz"],
  ["/baz", "/baz-quux", "../baz-quux"],
  ["/page1/page2/foo", "/", "../../.."],
]) {
  eq(path.relative(from, to), expected, `relative(${from}, ${to})`);
}

for (const [input, expected] of [
  ["/home/user/dir/file.txt", { root: "/", dir: "/home/user/dir", base: "file.txt", ext: ".txt", name: "file" }],
  ["/home/user/a dir/another File.zip", { root: "/", dir: "/home/user/a dir", base: "another File.zip", ext: ".zip", name: "another File" }],
  ["user/dir/another File.zip", { root: "", dir: "user/dir", base: "another File.zip", ext: ".zip", name: "another File" }],
  ["file", { root: "", dir: "", base: "file", ext: "", name: "file" }],
  [".\\file", { root: "", dir: "", base: ".\\file", ext: "", name: ".\\file" }],
  ["./file", { root: "", dir: ".", base: "file", ext: "", name: "file" }],
  ["/.file", { root: "/", dir: "/", base: ".file", ext: "", name: ".file" }],
  ["/", { root: "/", dir: "/", base: "", ext: "", name: "" }],
  ["", { root: "", dir: "", base: "", ext: "", name: "" }],
  ["//", { root: "/", dir: "/", base: "", ext: "", name: "" }],
]) {
  deepEq(path.parse(input), expected, `parse(${input})`);
  if (input !== "" && input !== "//") eq(path.format(path.parse(input)), input === "//" ? "/" : input, `format(parse(${input}))`);
}
eq(path.format({ root: "/", name: "file", ext: "txt" }), "/file.txt", "format adds the dot");
eq(path.format({ dir: "a", base: "b" }), "a/b", "format joins dir and base");

eq(path.isAbsolute("/home/foo"), true, "isAbsolute");
eq(path.isAbsolute("bar/"), false, "isAbsolute relative");
eq(path.toNamespacedPath("/a/b"), "/a/b", "toNamespacedPath");

for (const fn of ["join", "resolve", "normalize", "isAbsolute", "relative", "dirname", "basename", "extname", "parse"]) {
  throws(() => path[fn](42, "x"), "ERR_INVALID_ARG_TYPE", `${fn} validates its arguments`);
}
const error = throws(() => path.basename(null), "ERR_INVALID_ARG_TYPE");
eq(error instanceof TypeError, true, "ERR_INVALID_ARG_TYPE is a TypeError");
eq(error.message, 'The "path" argument must be of type string. Received null', "error message");

// Globs, matched as minimatch matches them.
const nocase = process.platform === "darwin" || process.platform === "win32";
for (const [file, pattern, expected] of [
  ["foo/bar/baz", "foo/[bcr]ar/baz", true],
  ["foo/bar/baz", "foo/[!bcr]ar/baz", false],
  ["foo/bar/baz", "foo/*/!bar/*/baz", false],
  ["foo/bar/baz", "foo/**", true],
  ["foo/bar/baz", "*", false],
  ["a/b/c.js", "**/*.js", true],
  ["a/.b/c.js", "**/*.js", false],
  ["a/b/c.js", "a/{b,x}/*.{js,ts}", true],
  ["a/./b", "a/b", true],
  ["a/x/../b", "a/b", true],
  ["file3", "file{1..3}", true],
  ["a.JS", "*.js", nocase],
  ["A/b", "a/b", false],
  ["ab", "+(a|b)", true],
  ["x", "!(a|b)", true],
  ["a", "!(a|b)", false],
  [".hidden", "*", false],
  [".hidden", ".*", true],
  ["a1", "a[[:digit:]]", true],
  ["", "", true],
  ["a", "", false],
  ["a,b{c}", "a,b{c}", true],
  ["xa{b}c", "x{a{b}c,d}", true],
  ["xac", "x{a{b,c},d}", true],
  ["a3b", "a{1,{2,3}}b", true],
  ["p,q", "{p,q},{r}", false],
  ["p,{r}", "{p,q},{r}", true],
]) {
  eq(path.matchesGlob(file, pattern), expected, `matchesGlob(${file}, ${pattern})`);
}
throws(() => path.matchesGlob(1, "a"), "ERR_INVALID_ARG_TYPE", "matchesGlob validates the path");
throws(() => path.matchesGlob("a"), "ERR_INVALID_ARG_TYPE", "matchesGlob validates the pattern");
throws(() => path.matchesGlob("a", "{a,".repeat(16000) + "b" + "}".repeat(16000)), RangeError, "deep braces");
throws(() => path.matchesGlob("a", "+(".repeat(5000) + "a" + ")".repeat(5000)), RangeError, "deep extglobs");
