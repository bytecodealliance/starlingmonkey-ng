// `node:buffer` and the `Buffer` global.

import { Blob as BlobExport, Buffer as BufferExport, File as FileExport, constants, kMaxLength, isUtf8, isAscii, atob } from "node:buffer";
import bufferModule from "buffer";
import { deepEq, eq, throws } from "./harness.mjs";

eq(BufferExport, Buffer, "the global is the module's Buffer");
eq(bufferModule.Buffer, Buffer, "the default export holds Buffer");
eq(Object.getPrototypeOf(Buffer.prototype), Uint8Array.prototype, "Buffer extends Uint8Array");
eq(constants.MAX_LENGTH, kMaxLength, "constants");
eq(atob, globalThis.atob, "atob is the global's");

// Creation and encodings.
const hello = Buffer.from("héllo wörld");
eq(hello instanceof Uint8Array && hello instanceof Buffer, true, "from(string) is a Buffer");
eq(Buffer.isBuffer(hello) && !Buffer.isBuffer(new Uint8Array(1)), true, "isBuffer");
eq(hello.length, 13, "UTF-8 length");
eq(hello.toString(), "héllo wörld", "UTF-8 round trip");
for (const [encoding, text, bytes] of [
  ["hex", "48656c6c6f", [72, 101, 108, 108, 111]],
  ["base64", "SGVsbG8=", [72, 101, 108, 108, 111]],
  ["base64url", "SGVsbG8", [72, 101, 108, 108, 111]],
  ["latin1", "éÿ", [0xe9, 0xff]],
  ["utf16le", "Hi", [72, 0, 105, 0]],
  ["ascii", "Hi", [72, 105]],
]) {
  const buffer = Buffer.from(text, encoding);
  deepEq([...buffer], bytes, `from(${encoding})`);
  eq(buffer.toString(encoding), text, `toString(${encoding})`);
}
deepEq([...Buffer.from("SGVs bG8\n", "base64")], [72, 101, 108, 108, 111], "base64 skips whitespace");
deepEq([...Buffer.from("abcz12", "hex")], [0xab], "hex stops at an invalid pair");
eq(Buffer.from([0xff, 0xfe]).toString("utf8"), "��", "invalid UTF-8 is replaced");
eq(Buffer.from("😀").length, 4, "astral characters");
deepEq([...Buffer.from([1, 2, 300, -1])], [1, 2, 44, 255], "from(array) wraps bytes");
deepEq([...Buffer.from({ length: 2, 0: 7, 1: 8 })], [7, 8], "from(array-like)");
deepEq([...Buffer.from(Buffer.from([1, 2]).toJSON())], [1, 2], "from(toJSON())");
const arrayBuffer = new ArrayBuffer(8);
const shared = Buffer.from(arrayBuffer, 2, 4);
shared[0] = 9;
eq(new Uint8Array(arrayBuffer)[2], 9, "from(arrayBuffer) shares memory");
eq(Buffer.from(new String("str")).toString(), "str", "from(valueOf)");
throws(() => Buffer.from(42), "ERR_INVALID_ARG_TYPE", "from(number) throws");
throws(() => Buffer.from("x", "nope"), "ERR_UNKNOWN_ENCODING", "unknown encoding");
deepEq([...Buffer.alloc(4, "ab")], [97, 98, 97, 98], "alloc with a string fill");
deepEq([...Buffer.alloc(3, 1)], [1, 1, 1], "alloc with a number fill");
eq(Buffer.allocUnsafe(2).length, 2, "allocUnsafe");
throws(() => Buffer.alloc(-1), "ERR_OUT_OF_RANGE", "negative size");
eq(Buffer(3).length, 3, "Buffer(size) without new");
eq(new Buffer("ab").toString(), "ab", "new Buffer(string)");
eq(Buffer.byteLength("héllo"), 6, "byteLength");
eq(Buffer.byteLength("aGk=", "base64"), 2, "byteLength in base64");
eq(Buffer.isEncoding("UTF-8") && !Buffer.isEncoding("x"), true, "isEncoding");

// Comparing, joining and slicing.
eq(Buffer.compare(Buffer.from("a"), Buffer.from("b")), -1, "compare");
eq(Buffer.from("abc").equals(Buffer.from("abc")), true, "equals");
eq(Buffer.from("b").compare(Buffer.from("a")), 1, "compare method");
eq(Buffer.concat([Buffer.from("ab"), new Uint8Array([99])]).toString(), "abc", "concat");
eq(Buffer.concat([Buffer.from("abc")], 2).toString(), "ab", "concat with totalLength");
const source = Buffer.from("abcdef");
const view = source.subarray(1, -1);
eq(view.toString(), "bcde", "subarray with a negative end");
view[0] = 0x42;
eq(source.toString(), "aBcdef", "subarray shares memory");
eq(source.slice(-2).toString(), "ef", "slice");
eq(Buffer.isBuffer(view), true, "subarray is a Buffer");
const target = Buffer.alloc(4);
eq(Buffer.from("xyz").copy(target, 1), 3, "copy returns the bytes copied");
deepEq([...target], [0, 120, 121, 122], "copy");
eq(source.toString("utf8", 1, 3), "Bc", "toString with a range");

// Writing, filling and searching.
const written = Buffer.alloc(6);
eq(written.write("héllo", 1), 5, "write returns bytes written");
eq(written.write("é", 5), 0, "a character that doesn't fit isn't split");
eq(written.toString("utf8", 1), "héll", "write");
eq(Buffer.alloc(5).fill("ab", 1).toString(), "\u0000abab", "fill");
const haystack = Buffer.from("this is a buffer");
eq(haystack.indexOf("is"), 2, "indexOf string");
eq(haystack.indexOf("is", 3), 5, "indexOf with offset");
eq(haystack.lastIndexOf("is"), 5, "lastIndexOf");
eq(haystack.indexOf(Buffer.from("a b")), 8, "indexOf buffer");
eq(haystack.indexOf(0x62), 10, "indexOf number");
eq(haystack.includes("buffer") && !haystack.includes("nope"), true, "includes");
deepEq(Buffer.from("ab").toJSON(), { type: "Buffer", data: [97, 98] }, "toJSON");
deepEq([...Buffer.from([1, 2, 3, 4]).swap16()], [2, 1, 4, 3], "swap16");
throws(() => Buffer.from([1, 2, 3]).swap16(), "ERR_INVALID_BUFFER_SIZE", "swap16 size");

// Numbers.
const numbers = Buffer.alloc(16);
eq(numbers.writeUInt16BE(0x1234, 0), 2, "write returns the next offset");
eq(numbers.readUInt16BE(0), 0x1234, "readUInt16BE");
eq(numbers.readUInt16LE(0), 0x3412, "readUInt16LE");
numbers.writeInt32LE(-2, 2);
eq(numbers.readInt32LE(2), -2, "Int32LE round trip");
eq(numbers.readUint32LE(2), 0xfffffffe, "Uint alias");
numbers.writeDoubleBE(Math.PI, 6);
eq(numbers.readDoubleBE(6), Math.PI, "Double round trip");
numbers.writeFloatLE(1.5, 0);
eq(numbers.readFloatLE(0), 1.5, "Float round trip");
numbers.writeBigInt64LE(-5n, 8);
eq(numbers.readBigInt64LE(8), -5n, "BigInt64 round trip");
eq(numbers.readBigUInt64LE(8), 2n ** 64n - 5n, "BigUInt64");
numbers.writeUIntBE(0x123456, 0, 3);
eq(numbers.readUIntBE(0, 3), 0x123456, "variable-width round trip");
eq(numbers.readIntLE(0, 1), 0x12, "readIntLE");
throws(() => numbers.writeUInt8(256, 0), "ERR_OUT_OF_RANGE", "value out of range");
throws(() => numbers.readUInt32LE(14), "ERR_OUT_OF_RANGE", "offset out of range");
eq(throws(() => numbers.writeBigInt64LE(1, 0), TypeError, "BigInt writes take a bigint").code, undefined, "BigInt writes throw a plain TypeError");

eq(isUtf8(Buffer.from("é")) && !isUtf8(Buffer.from([0xff])), true, "isUtf8");
eq(isAscii(Buffer.from("a")) && !isAscii(Buffer.from("é")), true, "isAscii");

// Node.js assigns Buffer's methods, so they are enumerable, which libraries such as safer-buffer
// copy with `for...in`.
const statics = Object.keys(Buffer);
for (const name of ["from", "alloc", "allocUnsafe", "isBuffer", "concat", "byteLength", "of"]) {
  eq(statics.includes(name), true, `Buffer.${name} is enumerable`);
}
eq(Object.keys(Buffer.prototype).includes("readUInt8"), true, "prototype methods are enumerable");
eq(Object.keys(Buffer.prototype).includes("constructor"), false, "constructor isn't");
deepEq([...Buffer.of(1, 2, 257)], [1, 2, 1], "Buffer.of");
eq(Buffer.from("ab").inspect(), "<Buffer 61 62>", "inspect");

// `Blob` and `File` are the globals, which Koa checks response bodies against.
eq(BlobExport, globalThis.Blob, "Blob is the global's");
eq(FileExport, globalThis.File, "File is the global's");
{
  const blob = new Blob(["ab", Buffer.from("cd"), new Blob(["e"])], { type: "Text/Plain" });
  eq(blob.size, 5, "Blob size");
  eq(blob.type, "text/plain", "Blob type");
  eq(await blob.text(), "abcde", "Blob text");
  eq(await blob.slice(1, -1).text(), "bcd", "Blob slice");
  eq(Buffer.from(await blob.arrayBuffer()).toString(), "abcde", "Blob arrayBuffer");
  const chunks = [];
  for await (const chunk of blob.stream()) chunks.push(Buffer.from(chunk));
  eq(Buffer.concat(chunks).toString(), "abcde", "Blob stream");
  const file = new File([blob], "name.txt", { lastModified: 42 });
  eq(file instanceof Blob, true, "File extends Blob");
  eq(`${file.name} ${file.size} ${file.lastModified}`, "name.txt 5 42", "File attributes");
  const response = new Response(blob);
  eq(response.headers.get("content-type"), "text/plain", "a Blob body's content type");
  eq(await response.text(), "abcde", "a Blob body");
}
