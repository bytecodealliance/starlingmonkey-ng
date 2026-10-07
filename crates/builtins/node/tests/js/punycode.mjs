// `node:punycode`, with the examples from Node's documentation.

import punycode, { decode, encode, toASCII, toUnicode, ucs2, version } from "node:punycode";
import { eq, deepEq, throws } from "./harness.mjs";

eq(punycode.encode, encode, "default export");
eq(version, "2.1.0", "version");
eq(encode("mañana"), "maana-pta", "encode");
eq(encode("☃-⌘"), "--dqo34k", "encode symbols");
eq(decode("maana-pta"), "mañana", "decode");
eq(decode("--dqo34k"), "☃-⌘", "decode symbols");
throws(() => decode("\u0080"), RangeError, "decode rejects non-basic code points");
eq(toASCII("mañana.com"), "xn--maana-pta.com", "toASCII");
eq(toASCII("example.com"), "example.com", "toASCII keeps ASCII labels");
eq(toASCII("джумла@джpумлатест.bрфa"), "джумла@xn--p-8sbkgc5ag7bhce.xn--ba-lmcq", "toASCII converts only the email domain");
eq(toUnicode("xn--maana-pta.com"), "mañana.com", "toUnicode");
eq(toUnicode("xn--ls8h.la"), "💩.la", "toUnicode astral");
eq(toUnicode("example.com"), "example.com", "toUnicode keeps other labels");
deepEq(ucs2.decode("abc"), [0x61, 0x62, 0x63], "ucs2.decode");
deepEq(ucs2.decode("𝌆"), [0x1d306], "ucs2.decode combines surrogate pairs");
eq(ucs2.encode([0x61, 0x62, 0x63]), "abc", "ucs2.encode");
eq(ucs2.encode([0x1d306]), "𝌆", "ucs2.encode splits astral code points");

// Punycode.js's errors and edge cases
const rangeError = throws(() => decode("99999999999"), RangeError, "overflow");
eq(rangeError.message, "Overflow: input needs wider integers to process", "overflow message");
eq(rangeError.code, undefined, "no code");
eq(throws(() => decode("é-"), RangeError).message, "Illegal input >= 0x80 (not a basic code point)", "not-basic message");
eq(toASCII("a@b@mañana.com"), "a@b", "only the part after the first @");
eq(encode("\uD800"), "ib9b", "lone surrogates");
eq(encode(5), "", "non-strings encode to nothing");
deepEq(ucs2.decode(5), [], "ucs2.decode of a non-string");
eq(toUnicode("XN--MAANA-PTA.COM"), "XN--MAANA-PTA.COM", "uppercase prefix");
throws(() => ucs2.encode([0x110000]), RangeError, "invalid code point");
throws(() => ucs2.encode([{ valueOf() { throw new TypeError("boom"); } }]), TypeError, "valueOf exceptions propagate");

// The errors of non-string arguments name Punycode.js's parameters.
eq(throws(() => toASCII(5), TypeError).message, "domain.split is not a function", "toASCII of a number");
eq(throws(() => toUnicode(5), TypeError).message, "domain.split is not a function", "toUnicode of a number");
eq(throws(() => encode(["a"]), TypeError).message, "string.charCodeAt is not a function", "encode of an array");
eq(throws(() => decode(["a"]), TypeError).message, "input.charCodeAt is not a function", "decode of an array");
eq(throws(() => decode(5), TypeError).message, "input.lastIndexOf is not a function", "decode of a number");
eq(toASCII(new String("mañana.com")), "xn--maana-pta.com", "toASCII of a String object");
eq(decode(new String("maana-pta")), "mañana", "decode of a String object");
deepEq([ucs2.decode.name, ucs2.encode.name], ["ucs2decode", "ucs2encode"], "ucs2 function names");
eq(Object.keys(punycode).join(), "version,ucs2,decode,encode,toASCII,toUnicode", "key order");
// `ucs2.encode` spreads its argument.
eq(ucs2.encode(new Set([65, 66])), "AB", "ucs2.encode of an iterable");
eq(ucs2.encode("65"), "\u0006\u0005", "ucs2.encode of a string");
throws(() => ucs2.encode(5), TypeError, "ucs2.encode of a number");
