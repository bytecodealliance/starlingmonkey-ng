// `node:querystring`.

import qs from "node:querystring";
import { deepEq, eq } from "./harness.mjs";

const parsed = qs.parse("foo=bar&abc=xyz&abc=123&empty=&flag&sp=a+b&enc=%E2%82%AC&bad=%zz");
eq(Object.getPrototypeOf(parsed), null, "no prototype");
deepEq(
  { ...parsed },
  { foo: "bar", abc: ["xyz", "123"], empty: "", flag: "", sp: "a b", enc: "€", bad: "%zz" },
  "parse",
);
deepEq({ ...qs.parse("a:1;b:2", ";", ":") }, { a: "1", b: "2" }, "custom separators");
deepEq(Object.keys(qs.parse("a=1&b=2&c=3", null, null, { maxKeys: 2 })), ["a", "b"], "maxKeys");
eq(
  qs.stringify({ foo: "bar", baz: ["qux", "quux"], corge: "", n: 1, b: true, o: {} }),
  "foo=bar&baz=qux&baz=quux&corge=&n=1&b=true&o=",
  "stringify",
);
eq(qs.stringify({ w: "€ x!" }), "w=%E2%82%AC%20x!", "stringify escapes");
eq(qs.stringify({ a: "b" }, ";", ":"), "a:b", "stringify separators");
eq(qs.escape("a b&c"), "a%20b%26c", "escape");
eq(qs.unescape("a%20b+c"), "a b+c", "unescape keeps +");
eq(typeof qs.decode, "function", "decode");
eq(qs.encode({ x: 1 }), "x=1", "encode");

// Replaced exports and realm globals
const originalDecode = globalThis.decodeURIComponent;
globalThis.decodeURIComponent = () => "PATCHED";
eq(qs.unescape("%41"), "A", "unescape ignores a patched decodeURIComponent");
globalThis.decodeURIComponent = originalDecode;
const originalKeys = Object.keys;
Object.keys = () => ["z"];
eq(qs.stringify({ a: 1 }), "a=1", "stringify ignores a patched Object.keys");
Object.keys = originalKeys;
const originalUnescape = qs.unescape;
qs.unescape = () => "Z";
deepEq({ ...qs.parse("a=%41") }, { Z: "Z" }, "parse uses a replaced unescape");
qs.unescape = originalUnescape;
const originalEscape = qs.escape;
qs.escape = (text) => `<${text}>`;
eq(qs.stringify({ a: "b" }), "<a>=<b>", "stringify uses a replaced escape");
qs.escape = originalEscape;
let popError = null;
try {
  qs.parse("a=1&a=2", null, null, {
    decodeURIComponent: (s) => (s === "a" ? s : { get pop() { throw new Error("popthrow"); } }),
  });
} catch (error) {
  popError = error.message;
}
eq(popError, "popthrow", "pop getter exceptions propagate");
