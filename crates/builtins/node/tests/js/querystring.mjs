// `node:querystring`.

import qs from "node:querystring";
import { deepEq, eq, throws } from "./harness.mjs";

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
eq(qs.stringify({ a: new Proxy([1, 2], {}), c: [] }), "a=1&a=2", "stringify writes a proxied array as one pair per element");
eq(qs.unescape("%E9%ZZ+x", true), "�%ZZ x", "unescape falls back to the bytes as UTF-8");
{
  let calls = 0;
  const encoded = qs.stringify({ k: [1, 2] }, null, null, { encodeURIComponent: (text) => text + calls++ });
  eq(encoded, "k0=11&k0=22", "stringify encodes each element as it writes its pair");
}

// `unescapeBuffer` fails for a value without a string's `length` and `charCodeAt`.
throws(() => qs.unescapeBuffer(5), "ERR_INVALID_ARG_TYPE", "unescapeBuffer of a number");
deepEq([...qs.unescapeBuffer(["a", "b"])], [0x61, 0x2c], "unescapeBuffer reads String(text) up to text.length");
deepEq([...qs.unescapeBuffer({ length: 17 })].slice(14), [0x5d, 0, 0], "unescapeBuffer pads with zeros");
eq(qs.unescapeBuffer(new String("a%20b")).toString(), "a b", "unescapeBuffer of a String object");
deepEq([qs.decode.length, qs.encode.length], [4, 4], "decode and encode lengths");
eq(Object.keys(qs).join(), "unescapeBuffer,unescape,escape,stringify,encode,parse,decode", "key order");
