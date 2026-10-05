// `node:string_decoder`.

import { StringDecoder } from "node:string_decoder";
import { eq, throws } from "./harness.mjs";

const utf8 = new StringDecoder("utf8");
eq(utf8.encoding, "utf8", "encoding");
const euro = Buffer.from("€");
eq(utf8.write(euro.subarray(0, 1)), "", "an incomplete character is held back");
eq(utf8.write(euro.subarray(1, 2)), "", "still incomplete");
eq(utf8.write(euro.subarray(2)), "€", "completed");
eq(utf8.write(Buffer.from([0xe2, 0x82])), "", "held back");
eq(utf8.end(), "�", "end replaces an incomplete character");
eq(utf8.write("text"), "text", "strings pass through");

const utf16 = new StringDecoder("utf16le");
eq(utf16.write(Buffer.from([0x61])), "", "odd byte held back");
eq(utf16.write(Buffer.from([0x00, 0x62, 0x00])), "ab", "UTF-16 pairs");
const smile = Buffer.from("😀", "utf16le");
eq(utf16.write(smile.subarray(0, 2)), "", "a high surrogate waits");
eq(utf16.write(smile.subarray(2)), "😀", "surrogate pair completed");

const base64 = new StringDecoder("base64");
eq(base64.write(Buffer.from("ab")), "", "base64 groups of three");
eq(base64.write(Buffer.from("c")), "YWJj", "completed group");
eq(base64.end(Buffer.from("d")), "ZA==", "end pads the rest");

eq(new StringDecoder("hex").write(Buffer.from([1, 255])), "01ff", "hex");
throws(() => new StringDecoder("nope"), "ERR_UNKNOWN_ENCODING", "unknown encoding");
throws(() => new StringDecoder({ toString() { return "hex"; } }), "ERR_UNKNOWN_ENCODING", "a non-string encoding");

// Constructors that call `StringDecoder` on themselves, as iconv-lite's do.
function Wrapper(encoding) {
  StringDecoder.call(this, encoding);
}
Wrapper.prototype = StringDecoder.prototype;
const wrapped = new Wrapper("utf8");
eq(wrapped.encoding, "utf8", "encoding of a decoder initialized by call");
eq(wrapped.write(euro.subarray(0, 2)) + wrapped.end(euro.subarray(2)), "€", "a called decoder decodes");
throws(() => StringDecoder.prototype.write.call({}, Buffer.from("a")), "ERR_INVALID_THIS", "methods need a decoder");
eq(Object.keys(new StringDecoder()).includes("encoding"), true, "encoding is an own property");

// Small typed arrays keep their bytes inline, where a GC can move them.
const inline = new StringDecoder("utf8");
for (let i = 0; i < 50; i++) {
  eq(inline.write(new Uint8Array([0x68, 0xc3, 0xa9, 0x6c])), "hél", "decoding a small typed array");
}
const target = Buffer.alloc(16);
const rope = "ab".repeat(3) + "ü";
eq(target.write(rope), 8, "writing a rope as UTF-8");
eq(target.toString("utf8", 0, 8), "abababü", "the written rope");
