// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Generators for the differential fuzzer. Each generator has a name and a set of variants, and each
// variant is a function from a `Gen` to the source of an operation: the body of an async function
// that returns the value to compare, or throws. Bodies share no state, so each one runs the same
// alone as in a program with others. `Gen` draws random values and renders them as JavaScript.

import { createRequire } from "node:module";
import zlibHost from "node:zlib";

/** A seeded random source with helpers that render JavaScript source. */
export class Gen {
  constructor(seed) {
    this.state = seed >>> 0;
  }

  /** A float in [0, 1) from mulberry32. */
  next() {
    let t = (this.state = (this.state + 0x6d2b79f5) >>> 0);
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  }

  int(max) {
    return Math.floor(this.next() * max);
  }

  range(min, max) {
    return min + this.int(max - min + 1);
  }

  chance(probability) {
    return this.next() < probability;
  }

  pick(items) {
    return items[this.int(items.length)];
  }

  /** `count` items drawn by `draw`. */
  many(count, draw) {
    return Array.from({ length: count }, () => draw());
  }

  /** A string of up to `max` characters, drawn from one of several alphabets. */
  string(max = 12) {
    const length = this.chance(0.1) ? 0 : this.range(1, max);
    const alphabet = this.pick(["ascii", "ascii", "latin1", "unicode", "special", "surrogates"]);
    let out = "";
    for (let index = 0; index < length; index++) {
      switch (alphabet) {
        case "ascii":
          out += String.fromCharCode(this.range(0x20, 0x7e));
          break;
        case "latin1":
          out += String.fromCharCode(this.chance(0.5) ? this.range(0x20, 0x7e) : this.range(0x80, 0xff));
          break;
        case "unicode":
          out += this.pick(["a", "é", "ß", "€", "中", "😀", "\u0301", "\u0000", "\ufeff", "𝄞", "\u200d", "Ω"]);
          break;
        case "special":
          out += this.pick(["%", "&", "=", "+", "#", "?", "/", " ", ";", ":", "@", "%2", "%zz", "%41", "a", "\\", "."]);
          break;
        case "surrogates":
          out += this.pick(["\ud83d", "\ude00", "😀", "a", "\udfff", "\ud800"]);
          break;
      }
    }
    return out;
  }

  /** Up to `max` random bytes, biased towards UTF-8 lead and continuation bytes. */
  bytes(max = 16) {
    const length = this.chance(0.1) ? 0 : this.range(1, max);
    return Buffer.from(
      this.many(length, () =>
        this.pick([
          () => this.range(0x20, 0x7e),
          () => this.range(0x80, 0xbf),
          () => this.range(0xc0, 0xff),
          () => this.int(256),
        ])(),
      ),
    );
  }

  /** A JavaScript literal for `value`. */
  lit(value) {
    if (value === undefined) return "undefined";
    if (typeof value === "number") {
      if (Object.is(value, -0)) return "-0";
      return String(value);
    }
    if (typeof value === "bigint") return `${value}n`;
    if (Buffer.isBuffer(value)) return `Buffer.from(${JSON.stringify(value.toString("hex"))}, "hex")`;
    if (Array.isArray(value)) return `[${value.map((item) => this.lit(item)).join(", ")}]`;
    if (value && typeof value === "object" && value.__raw) return value.__raw;
    if (value && typeof value === "object") {
      return `{ ${Object.entries(value)
        .map(([key, item]) => `${JSON.stringify(key)}: ${this.lit(item)}`)
        .join(", ")} }`;
    }
    return JSON.stringify(value);
  }

  /** An integer-ish argument, including edge cases for validation. */
  number() {
    return this.pick([
      0, 1, 2, 3, 4, 7, 8, -1, -2, 255, 256, 65535, 2 ** 31 - 1, 2 ** 31, 2 ** 32, -(2 ** 31), 2 ** 53, NaN, Infinity,
      -Infinity, -0, 1.5, "1", undefined, null,
    ]);
  }

  /** A size argument: small, or invalid, but never large enough to allocate much. */
  size() {
    return this.pick([0, 1, 2, 7, 8, 16, -1, NaN, Infinity, -Infinity, -0, 1.5, "1", undefined, null, 2 ** 53, 2 ** 60]);
  }

  /** A small index argument, mostly in range for short buffers. */
  index(max = 8) {
    return this.chance(0.8) ? this.range(-2, max) : this.number();
  }

  encoding() {
    return this.pick([
      "utf8", "utf-8", "UTF8", "hex", "base64", "base64url", "latin1", "binary", "ascii", "ucs2", "ucs-2", "utf16le",
      "utf-16le", undefined, "bogus",
    ]);
  }

  /** Text for `encoding` that is mostly valid input to it. */
  textFor(encoding) {
    const valid = this.chance(0.8);
    switch (String(encoding).toLowerCase()) {
      case "hex":
        return valid ? this.bytes().toString("hex") : this.pick(["abc", "zz", "0g", "a", "001"]) + this.string(3);
      case "base64":
      case "base64url":
        return valid
          ? this.bytes().toString(encoding).slice(0, this.chance(0.3) ? -1 : undefined)
          : this.pick(["a", "ab=", "a=b", "@@", "Zm9v YmFy", "Zm9v\nYmFy", "-_", "+/", "===="]);
      default:
        return this.string();
    }
  }

  /** A Buffer literal or a string literal, with the encoding to read the string with. */
  data() {
    if (this.chance(0.5)) return { source: this.lit(this.bytes()), encoding: undefined };
    const encoding = this.encoding();
    return { source: this.lit(this.textFor(encoding)), encoding };
  }
}

/** A `Buffer.from(...)` expression that never throws. */
function bufferExpr(g) {
  return g.chance(0.5) ? g.lit(g.bytes()) : `Buffer.from(${g.lit(g.string())})`;
}

/** Trailing optional arguments: drops `undefined`s at the end. */
function args(g, ...values) {
  const list = values.map((value) => g.lit(value));
  while (list.length > 0 && list[list.length - 1] === "undefined" && g.chance(0.7)) list.pop();
  return list.join(", ");
}

const INT_METHODS = [
  ["UInt8", 1], ["Int8", 1], ["UInt16LE", 2], ["UInt16BE", 2], ["Int16LE", 2], ["Int16BE", 2], ["UInt32LE", 4],
  ["UInt32BE", 4], ["Int32LE", 4], ["Int32BE", 4], ["FloatLE", 4], ["FloatBE", 4], ["DoubleLE", 8], ["DoubleBE", 8],
  ["BigInt64LE", 8], ["BigInt64BE", 8], ["BigUInt64LE", 8], ["BigUInt64BE", 8], ["Uint8", 1], ["Uint16LE", 2],
  ["Uint32BE", 4], ["BigUint64LE", 8],
];

const buffer = {
  name: "buffer",
  variants: {
    "from-to-string": (g) => {
      const { source, encoding } = g.data();
      return `return Buffer.from(${args(g, { __raw: source }, encoding)}).toString(${args(g, g.encoding(), g.index(), g.index())});`;
    },
    "byte-length": (g) => {
      const { source, encoding } = g.data();
      return `return Buffer.byteLength(${args(g, { __raw: source }, encoding)});`;
    },
    alloc: (g) => {
      const size = g.chance(0.8) ? g.range(0, 12) : g.size();
      const fill = g.pick([undefined, 0, 257, -1, "ab", "", g.string(4), g.bytes(3), "zz"]);
      return `return Buffer.alloc(${args(g, size, fill, g.encoding())});`;
    },
    write: (g) => {
      const encoding = g.encoding();
      return [
        `const b = Buffer.alloc(${g.range(0, 10)});`,
        `const n = b.write(${args(g, g.textFor(encoding), g.index(), g.index(), encoding)});`,
        `return [n, b];`,
      ].join("\n");
    },
    search: (g) => {
      const method = g.pick(["indexOf", "lastIndexOf", "includes"]);
      const needle = g.pick([
        () => g.lit(g.string(3)),
        () => g.lit(g.bytes(2)),
        () => g.lit(g.pick([0, 97, 256, -1, 1.5])),
        () => `Buffer.alloc(0)`,
        () => `new Uint8Array([${g.int(256)}])`,
      ])();
      const encoding = g.pick([undefined, "utf8", "hex", "latin1", "ucs2", "base64"]);
      return `return ${bufferExpr(g)}.${method}(${needle}${encoding === undefined && g.chance(0.5) ? "" : `, ${g.lit(g.index())}, ${g.lit(encoding)}`});`;
    },
    compare: (g) => {
      const a = bufferExpr(g);
      const b = bufferExpr(g);
      return g.pick([
        () => `return Buffer.compare(${a}, ${b});`,
        () => `return ${a}.compare(${b}, ${args(g, g.index(), g.index(), g.index(), g.index())});`,
        () => `return ${a}.equals(${b});`,
        () => `return Buffer.concat([${a}, ${b}]${g.chance(0.5) ? `, ${g.lit(g.chance(0.8) ? g.range(0, 20) : g.size())}` : ""});`,
      ])();
    },
    "read-number": (g) => {
      const [name, width] = g.pick(INT_METHODS);
      const offset = g.chance(0.8) ? g.range(0, 10 - width) : g.number();
      return `return ${g.lit(g.bytes(10))}.read${name}(${args(g, offset)});`;
    },
    "write-number": (g) => {
      const [name, width] = g.pick(INT_METHODS);
      const big = name.startsWith("Big");
      const value = big
        ? g.pick(["0n", "-1n", "1n", "2n ** 63n", "2n ** 64n", "-(2n ** 63n)", "255n", "1", "-(2n ** 63n) - 1n"])
        : g.lit(g.pick([0, 1, -1, 127, 128, 255, 256, 65535, 65536, -129, 2 ** 31, 2 ** 32 - 1, 1.5, NaN, -0, 3.4e38, 1e40]));
      const offset = g.chance(0.8) ? g.range(0, 10 - width) : g.number();
      return `const b = Buffer.alloc(10);\nconst end = b.write${name}(${value}, ${g.lit(offset)});\nreturn [end, b];`;
    },
    "var-int": (g) => {
      const name = g.pick(["IntLE", "IntBE", "UIntLE", "UIntBE", "UintLE"]);
      const width = g.chance(0.85) ? g.range(1, 6) : g.number();
      const offset = g.chance(0.85) ? g.range(0, 3) : g.number();
      if (g.chance(0.5)) return `return ${g.lit(g.bytes(9))}.read${name}(${g.lit(offset)}, ${g.lit(width)});`;
      const value = g.pick([0, 1, -1, 255, -129, 2 ** 40, -(2 ** 47), 2 ** 48 - 1, 2 ** 48, 1.5]);
      return `const b = Buffer.alloc(9);\nreturn [b.write${name}(${g.lit(value)}, ${g.lit(offset)}, ${g.lit(width)}), b];`;
    },
    slice: (g) =>
      `return ${bufferExpr(g)}.${g.pick(["slice", "subarray"])}(${args(g, g.index(), g.index())});`,
    swap: (g) => `return ${g.lit(g.bytes(16))}.${g.pick(["swap16", "swap32", "swap64"])}();`,
    fill: (g) => {
      const value = g.pick([0, 97, 256, -1, "ab", "", "\u00e9", g.bytes(3), "zz", "a"]);
      return `return Buffer.alloc(${g.range(0, 8)}).fill(${args(g, value, g.index(), g.index(), g.encoding())});`;
    },
    copy: (g) =>
      `const target = Buffer.alloc(${g.range(0, 8)});\nconst n = ${bufferExpr(g)}.copy(target, ${args(g, g.index(), g.index(), g.index())});\nreturn [n, target];`,
    misc: (g) =>
      g.pick([
        () => `return Buffer.isEncoding(${g.lit(g.encoding())});`,
        () => `return ${bufferExpr(g)}.toJSON();`,
        () => `return Buffer.from(${bufferExpr(g)}.toJSON());`,
        () => `return Buffer.from([${g.many(g.range(0, 5), () => g.lit(g.number())).join(", ")}]);`,
        () => `return Buffer.from(new Uint16Array([${g.many(3, () => g.int(65536)).join(", ")}]).buffer, ${args(g, g.index(6), g.index(6))});`,
        () => `return ${bufferExpr(g)}.toString("hex", ${g.lit(g.index())});`,
        () => `return Buffer.from(${g.lit(g.string())}).toLocaleString();`,
        () => `return require("node:buffer").transcode(${bufferExpr(g)}, ${g.lit(g.pick(["utf8", "latin1", "ucs2", "ascii"]))}, ${g.lit(g.pick(["utf8", "latin1", "ucs2", "ascii"]))});`,
        () => `const { isUtf8, isAscii } = require("node:buffer");\nconst b = ${g.lit(g.bytes())};\nreturn [isUtf8(b), isAscii(b)];`,
        () => `return [...${bufferExpr(g)}.entries()];`,
        () => `return [Buffer[Symbol.species].name, Buffer[Symbol.species].prototype === Buffer.prototype, ${bufferExpr(g)}.map((x) => x + 1), ${bufferExpr(g)}.filter((x) => x % 2) instanceof Buffer];`,
        () => `const { SlowBuffer } = require("node:buffer");\ntry { return [SlowBuffer(${g.pick(["0", "3", "\"x\"", "undefined"])}), new SlowBuffer(2).length, SlowBuffer.name, SlowBuffer.length]; } catch (e) { return [e.name, e.code, e.message]; }`,
        () => `const ab = new ArrayBuffer(4, { maxByteLength: 8 });\nconst b = Buffer.from(ab, ${g.range(0, 4)}${g.chance(0.3) ? `, ${g.range(0, 2)}` : ""});\nab.resize(${g.range(0, 8)});\nreturn [b.length, b.byteOffset];`,
      ])(),
  },
};

const stringDecoder = {
  name: "string_decoder",
  variants: {
    chunks: (g) => {
      const encoding = g.pick(["utf8", "utf8", "utf16le", "ucs2", "base64", "base64url", "hex", "latin1", "ascii", undefined]);
      const bytes = g.chance(0.5) ? Buffer.from(g.string(10), encoding === "utf16le" || encoding === "ucs2" ? "utf16le" : "utf8") : g.bytes(20);
      const chunks = [];
      let rest = bytes;
      while (rest.length > 0) {
        const size = g.range(0, Math.min(rest.length, 4));
        chunks.push(rest.subarray(0, size));
        rest = rest.subarray(size);
      }
      const end = g.chance(0.3) ? g.lit(g.bytes(3)) : "";
      return [
        `const { StringDecoder } = require("node:string_decoder");`,
        `const decoder = new StringDecoder(${g.lit(encoding)});`,
        `const out = [decoder.encoding];`,
        ...chunks.map((chunk) => `out.push(decoder.write(${g.lit(chunk)}));`),
        `out.push(decoder.end(${end}));`,
        `out.push(decoder.write(${g.lit(g.bytes(3))}));`,
        `return out;`,
      ].join("\n");
    },
    invalid: (g) =>
      g.pick([
        () => `const { StringDecoder } = require("node:string_decoder");\nreturn new StringDecoder(${g.lit(g.pick(["bogus", 1, null, "UTF-16LE", "Binary"]))}).encoding;`,
        () => `const { StringDecoder } = require("node:string_decoder");\nreturn new StringDecoder("utf8").write(${g.lit(g.pick([null, 1, "str", {}]))});`,
        () => `const { StringDecoder } = require("node:string_decoder");\nreturn new StringDecoder("utf8").write(new Uint8Array([${g.many(4, () => g.int(256)).join(", ")}]));`,
      ])(),
  },
};

function pathSegment(g) {
  return g.pick(["", ".", "..", "a", "b.c", "/", "//", ".d", "e.", "f.g.h", "a/b", "../x", "./y", "/z/", "~", "a\\b", "C:"]);
}

function globPath(g) {
  const segments = g.many(g.range(0, 4), () =>
    g.pick(["a", "b", "ab", "A", "a.js", "B.JS", ".a", "..", ".", "", "x y", "é", "1", "abc", "a]b", "{a}", "(a)", "!a", "a\\b", "aa", "ba"]),
  );
  return (g.chance(0.2) ? "/" : "") + segments.join(g.chance(0.1) ? "//" : "/");
}

function globPattern(g) {
  const atom = () =>
    g.pick([
      "a", "b", "A", "*", "*", "?", ".", "..", "[ab]", "[!a]", "[^b]", "[a-b]", "[z-a]", "[]", "[]]", "[a", "{a,b}", "{,a}",
      "{a}", "{1..3}", "{a..c}", "@(a|b)", "!(a)", "+(a|b)", "*(a)", "?(a)", "!(a|)", "!(*.js)", "@(a|!(b))", ".js", ".JS",
      "[[:alpha:]]", "[[:digit:]]", "[[:upper:]]", "\\", "#", "!", "é", " ",
    ]);
  const segment = () => (g.chance(0.15) ? "**" : g.many(g.range(1, 3), atom).join(""));
  return (g.chance(0.15) ? "/" : "") + g.many(g.range(1, 3), segment).join("/");
}

const path = {
  name: "path",
  variants: {
    matchesGlob: (g) => {
      const paths = g.many(g.range(1, 6), () => globPath(g));
      const module = `require("node:path")${g.chance(0.5) ? ".posix" : ""}`;
      return `return ${g.lit(paths)}.map((p) => ${module}.matchesGlob(p, ${g.lit(globPattern(g))}));`;
    },
    posix: (g) => {
      const segments = g.many(g.range(0, 4), () => pathSegment(g));
      const call = g.pick([
        () => `join(${args(g, ...segments)})`,
        () => `resolve(${args(g, ...segments)})`,
        () => `normalize(${g.lit(segments.join(""))})`,
        () => `relative(${g.lit(segments.slice(0, 2).join(""))}, ${g.lit(segments.slice(2).join(""))})`,
        () => `dirname(${g.lit(segments.join(""))})`,
        () => `basename(${args(g, segments.join(""), g.pick([undefined, ".c", "c", ".h", "b.c", ""]))})`,
        () => `extname(${g.lit(segments.join(""))})`,
        () => `parse(${g.lit(segments.join(""))})`,
        () => `format(${g.lit({ root: g.pick([undefined, "/", ""]), dir: g.pick([undefined, "/a", "a/"]), base: g.pick([undefined, "b.c", ""]), name: g.pick([undefined, "n"]), ext: g.pick([undefined, ".e", "e"]) })})`,
        () => `isAbsolute(${g.lit(segments.join(""))})`,
        () => `toNamespacedPath(${g.lit(segments.join(""))})`,
      ])();
      return `return require("node:path").posix.${call};`;
    },
    invalid: (g) =>
      `return require("node:path").${g.pick(["join", "resolve", "basename", "extname", "dirname", "normalize", "matchesGlob"])}(${g.lit(g.pick([null, 1, {}, undefined]))});`,
    win32: (g) => {
      const segments = g.many(g.range(1, 3), () => g.pick(["C:\\", "a", "..", "\\\\server\\share", "b.c", "C:", "\\x", "/y"]));
      return `return require("node:path").win32.${g.pick(["join", "normalize", "basename", "dirname", "isAbsolute"])}(${args(g, ...segments)});`;
    },
  },
};

const querystring = {
  name: "querystring",
  variants: {
    parse: (g) => {
      const parts = g.many(g.range(0, 5), () => `${g.string(4)}${g.pick(["=", "", "=="])}${g.string(4)}`);
      const sep = g.pick([undefined, undefined, ";", "&&", ""]);
      const eq = g.pick([undefined, undefined, ":", ""]);
      const options = g.pick([undefined, { maxKeys: g.pick([0, 1, 2, -1]) }, { decodeURIComponent: { __raw: "(s) => s.toUpperCase()" } }]);
      return `return require("node:querystring").parse(${args(g, parts.join(sep ?? "&"), sep, eq, options)});`;
    },
    stringify: (g) => {
      const object = Object.fromEntries(
        g.many(g.range(0, 4), () => [
          g.string(4),
          g.pick([g.string(4), g.int(100), true, null, undefined, NaN, Infinity, 10n, [g.string(3), 1], {}, -0, 1.5]),
        ]),
      );
      return `return require("node:querystring").stringify(${args(g, object, g.pick([undefined, ";", ""]), g.pick([undefined, ":"]))});`;
    },
    escape: (g) =>
      `return require("node:querystring").${g.pick(["escape", "unescape", "unescapeBuffer"])}(${g.lit(g.string())});`,
  },
};

const HASHES = ["sha1", "sha256", "sha512", "md5", "sha384", "sha224", "SHA256", "sha512-256", "sha3-256", "bogus"];
const DIGEST_ENCODINGS = [undefined, "hex", "base64", "base64url", "latin1", "binary", "ucs2", "bogus"];

const crypto = {
  name: "crypto",
  variants: {
    hash: (g) => {
      const updates = g.many(g.range(0, 3), () => {
        const { source, encoding } = g.data();
        return `h.update(${args(g, { __raw: source }, encoding)});`;
      });
      return [
        `const h = require("node:crypto").createHash(${g.lit(g.pick(HASHES))});`,
        ...updates,
        `return h.digest(${args(g, g.pick(DIGEST_ENCODINGS))});`,
      ].join("\n");
    },
    hmac: (g) => {
      const key = g.pick([() => g.lit(g.string(8)), () => g.lit(g.bytes(70)), () => `""`, () => g.lit(g.pick([null, 1]))])();
      const { source, encoding } = g.data();
      return `return require("node:crypto").createHmac(${g.lit(g.pick(HASHES))}, ${key}).update(${args(g, { __raw: source }, encoding)}).digest(${args(g, g.pick(DIGEST_ENCODINGS))});`;
    },
    "one-shot": (g) => {
      const { source } = g.data();
      return `return require("node:crypto").hash(${args(g, g.pick(HASHES), { __raw: source }, g.pick(DIGEST_ENCODINGS))});`;
    },
    lifecycle: (g) =>
      g.pick([
        () => `const h = require("node:crypto").createHash("sha256");\nh.digest();\nreturn h.digest();`,
        () => `const h = require("node:crypto").createHash("sha256");\nh.digest();\nreturn h.update("a");`,
        () => `const h = require("node:crypto").createHash("sha256").update(${g.lit(g.string())});\nconst c = h.copy();\nreturn [h.digest("hex"), c.update("x").digest("hex")];`,
        () => `const h = require("node:crypto").createHash("sha256").update("a");\nh.digest();\nreturn h.copy();`,
        () => `return require("node:crypto").createHash("sha256").update(${g.lit(g.pick([1, null, {}, undefined]))});`,
        () => `return require("node:crypto").getHashes().includes(${g.lit(g.pick(HASHES))});`,
      ])(),
    "timing-safe-equal": (g) =>
      `return require("node:crypto").timingSafeEqual(${g.lit(g.bytes(3))}, ${g.lit(g.bytes(3))});`,
    kdf: (g) => {
      const password = g.pick([() => g.string(6), () => g.bytes(5), () => g.pick([1, null, undefined])])();
      const salt = g.pick([() => g.string(4), () => g.bytes(4), () => g.pick([1, {}])])();
      return g.pick([
        () => {
          const iterations = g.pick([1, 2, 3, 0, -1, 1.5, "1"]);
          const keylen = g.pick([0, 1, 20, 33, 64, -1, 2.5]);
          return `return require("node:crypto").pbkdf2Sync(${args(g, password, salt, iterations, keylen, g.pick(HASHES))}).toString("hex");`;
        },
        () => {
          const keylen = g.pick([0, 1, 16, 33, -1]);
          const options = g.pick([
            undefined, null, 1, { N: 16 }, { N: 3 }, { N: 1 }, { N: 0, r: 0, p: 0 }, { cost: 16, blockSize: 1 },
            { N: 16, cost: 16 }, { r: 2, p: 2, N: 32 }, { maxmem: 1 }, { N: 2 ** 20 }, { N: -1 }, { parallelization: "2" },
          ]);
          return `return require("node:crypto").scryptSync(${args(g, password, salt, keylen, options)}).toString("hex");`;
        },
        () => {
          const info = g.pick([() => g.string(4), () => g.bytes(1025), () => 1])();
          const length = g.pick([0, 1, 16, 255 * 20, 255 * 32 + 1, -1, 1.5]);
          return `return Buffer.from(require("node:crypto").hkdfSync(${args(g, g.pick(HASHES), password, salt, info, length)})).toString("hex");`;
        },
      ])();
    },
    "secret-key": (g) => {
      const key = g.pick([() => g.string(6), () => g.bytes(5), () => g.pick([1, null, ""])])();
      const encoding = g.pick([undefined, "hex", "base64", "latin1", "bogus"]);
      return g.pick([
        () => `const k = require("node:crypto").createSecretKey(${args(g, key, encoding)});\nreturn [k.type, k.symmetricKeySize, k.export().toString("hex"), k.export(${args(g, g.pick([undefined, { format: "jwk" }, { format: "buffer" }, { format: "pem" }, 1]))})];`,
        () => `const c = require("node:crypto");\nconst k = c.createSecretKey(${g.lit(g.bytes(4))});\nreturn k.equals(${g.pick([() => `c.createSecretKey(${g.lit(g.bytes(4))})`, () => "k", () => g.lit(g.string(4))])()});`,
        () => `const c = require("node:crypto");\nconst k = c.createSecretKey(${g.lit(g.bytes(8))});\nreturn c.createHmac(${g.lit(g.pick(HASHES))}, k).update(${g.lit(g.string())}).digest("hex") === c.createHmac(${g.lit(g.pick(HASHES))}, k.export()).update("x").digest("hex");`,
        () => `return require("node:crypto").createHmac("sha256", ${g.lit(g.string(6))}, ${g.lit(g.pick([undefined, { encoding: "hex" }, { encoding: "base64" }, { encoding: 1 }, null]))}).update("x").digest("hex");`,
      ])();
    },
    random: (g) =>
      g.pick([
        () => `return require("node:crypto").randomBytes(${g.lit(g.size())}).length;`,
        () => `return /^[0-9a-f-]{36}$/.test(require("node:crypto").randomUUID());`,
        () => `const n = require("node:crypto").randomInt(${args(g, g.number(), g.number())});\nreturn typeof n;`,
        () => `const b = Buffer.alloc(8);\nrequire("node:crypto").randomFillSync(b, ${args(g, g.index(), g.index())});\nreturn b.length;`,
      ])(),
  },
};


/** A `try` block that logs `label` with the value of `expression`, or with the error it throws. */
function step(label, expression) {
  return `try { __log(${JSON.stringify(label)}, ${expression}); } catch (error) { __log(${JSON.stringify(label)}, error); }`;
}

const events = {
  name: "events",
  variants: {
    sequence: (g) => {
      const name = () => g.lit(g.pick(["a", "a", "b", "error"]));
      const listener = () => `ls[${g.int(4)}]`;
      const lines = [
        `const { EventEmitter } = require("node:events");`,
        `const e = new EventEmitter();`,
        `const ls = [0, 1, 2, 3].map((i) => function (...args) { __log("l" + i, this === e, ...args); });`,
      ];
      if (g.chance(0.3)) lines.push(`e.on("newListener", (n, l) => __log("new", n, ls.indexOf(l)));`);
      if (g.chance(0.3)) lines.push(`e.on("removeListener", (n, l) => __log("removed", n, ls.indexOf(l)));`);
      for (let index = g.range(3, 10); index > 0; index--) {
        lines.push(
          g.pick([
            () => step("add", `e.${g.pick(["on", "addListener", "prependListener", "once", "prependOnceListener"])}(${name()}, ${listener()}) === e`),
            () => step("remove", `e.${g.pick(["off", "removeListener"])}(${name()}, ${listener()}) === e`),
            () => step("remove-all", `e.removeAllListeners(${g.chance(0.5) ? name() : ""}) === e`),
            () => step("emit", `e.emit(${name()}, ${g.many(g.range(0, 2), () => g.lit(g.pick([1, "x", null])) ).concat(g.chance(0.2) ? ["new Error(\"boom\")"] : []).join(", ")})`),
            () => step("count", `e.listenerCount(${name()}${g.chance(0.3) ? `, ${listener()}` : ""})`),
            () => step("names", `e.eventNames()`),
            () => step("listeners", `e.listeners(${name()}).map((l) => ls.indexOf(l))`),
            () => step("raw", `e.rawListeners(${name()}).map((l) => ls.indexOf(l.listener ?? l))`),
            () => step("max", `[e.setMaxListeners(${g.lit(g.pick([0, 1, 5, -1, NaN, "2"]))}) === e, e.getMaxListeners()]`),
            () => `e.on(${name()}, function self(...args) { __log("self", ...args); e.off(${name()}, self); e.on(${name()}, ${listener()}); });`,
          ])(),
        );
      }
      lines.push(`return [e.eventNames(), e.listenerCount("a")];`);
      return lines.join("\n");
    },
    statics: (g) =>
      g.pick([
        () => `const events = require("node:events");\nconst e = new events.EventEmitter();\nprocess.nextTick(() => e.emit(${g.lit(g.pick(["x", "error"]))}, ${g.pick(["1, 2", "new Error(\"e\")", ""])}));\nreturn await events.once(e, "x");`,
        () => `const events = require("node:events");\nconst e = new events.EventEmitter();\nprocess.nextTick(() => { e.emit("x", 1); e.emit("x", 2); e.emit(${g.lit(g.pick(["x", "error"]))}, 3); });\nconst out = [];\nfor await (const value of events.on(e, "x")) { out.push(value); if (out.length === ${g.range(1, 3)}) break; }\nreturn [out, e.listenerCount("x"), e.listenerCount("error")];`,
        () => `const events = require("node:events");\nconst e = new events.EventEmitter();\ne.on("x", () => {});\nreturn [events.getEventListeners(e, "x").length, events.listenerCount(e, "x"), events.EventEmitter.defaultMaxListeners, typeof events.errorMonitor];`,
        () => `const { EventEmitter, errorMonitor } = require("node:events");\nconst e = new EventEmitter();\ne.on(errorMonitor, (err) => __log("monitor", err));\n${g.chance(0.5) ? `e.on("error", (err) => __log("handler", err));\n` : ""}e.emit("error", ${g.pick(["new Error(\"m\")", "\"str\"", "undefined", "{ message: \"o\" }"])});\nreturn "emitted";`,
        () => `const { EventEmitter } = require("node:events");\nconst e = new EventEmitter({ captureRejections: true });\ne.on("x", async () => { throw new Error("rejected"); });\nconst seen = new Promise((resolve) => e.on("error", resolve));\ne.emit("x");\nreturn await __settle(seen);`,
        () => `const { EventEmitter } = require("node:events");\nconst e = new EventEmitter();\nreturn e.emit("error", ${g.pick(["new Error(\"u\")", "\"s\"", "42", "undefined", "null", "{ a: 1 }"])});`,
        () => `const { EventEmitter } = require("node:events");\nconst e = new EventEmitter();\nreturn e.on(${g.lit(g.pick([null, 1, "x"]))}, ${g.pick(["null", "1", "() => {}", "{}"])}) === e;`,
        () => `const { EventEmitter } = require("node:events");\nclass Sub extends EventEmitter {}\nconst s = new Sub();\nconst sym = Symbol("evt");\ns.on(sym, (v) => __log("sym", v));\ns.emit(sym, 7);\nreturn [s instanceof EventEmitter, s.eventNames().length, typeof s.eventNames()[0]];`,
      ])(),
  },
};

/** Source for a Readable over `chunks`, which are string literals, in object mode or not. */
function readableSource(g, chunks, objectMode) {
  return g.pick([
    () => `Readable.from(${g.lit(chunks)}, { objectMode: ${objectMode} })`,
    () =>
      `(() => { const queue = ${g.lit(chunks)}; return new Readable({ objectMode: ${objectMode}, highWaterMark: ${g.pick([1, 2, 16, 1024])}, read() { ${g.pick(["", "if (queue.length === 2) return void setImmediate(() => this.push(queue.shift()));"])} this.push(queue.length > 0 ? queue.shift() : null); } }); })()`,
  ])();
}

function transformSource(g, objectMode, failAt) {
  const body = g.pick([
    `callback(null, typeof chunk === "string" ? chunk.toUpperCase() : chunk.toString().toUpperCase());`,
    `this.push(chunk); this.push(chunk); callback();`,
    `if (index % 2 === 0) this.push(chunk); callback();`,
    `setImmediate(() => callback(null, chunk));`,
    `callback(null, chunk);`,
  ]);
  const flush = g.pick(["", `flush(callback) { __log("flush"); callback(null, ${objectMode ? '"tail"' : 'Buffer.from("tail")'}); },`]);
  return `(() => { let index = 0; return new Transform({ objectMode: ${objectMode}, ${flush} transform(chunk, encoding, callback) { index++; ${failAt ? `if (index === ${failAt}) return callback(new Error("transform"));` : ""} ${body} } }); })()`;
}

function logStream(name) {
  return ["end", "finish", "close", "error"].map((event) => `${name}.on(${JSON.stringify(event)}, (...args) => __log(${JSON.stringify(`${name} ${event}`)}, ...args));`).join("\n");
}

const stream = {
  name: "stream",
  variants: {
    "duplex-pair": (g) => {
      const chunks = g.many(g.range(0, 4), () => g.string(4));
      const end = g.pick(["end", "destroy", "destroy-error", "none"]);
      const lines = [
        `const { duplexPair } = require("node:stream");`,
        `const [a, b] = duplexPair(${g.pick(["", "{ objectMode: true }", "{ highWaterMark: 1 }", "{ encoding: \"utf8\" }"])});`,
        logStream("a"),
        logStream("b"),
        `b.on("data", (chunk) => __log("b data", chunk));`,
        ...chunks.map((chunk) => `__log("write", a.write(${g.lit(chunk)}));`),
      ];
      if (end === "end") lines.push(`a.end();`);
      if (end === "destroy") lines.push(`a.destroy();`);
      if (end === "destroy-error") lines.push(`a.destroy(new Error("boom"));`);
      lines.push(`return await __settle(new Promise((resolve) => { b.on("end", () => resolve("ended")); b.on("close", () => resolve("closed")); }), 200);`);
      return lines.join("\n");
    },
    flow: (g) => {
      const objectMode = g.chance(0.4);
      const chunks = g.many(g.range(0, 5), () => g.string(5) || "z");
      const failAt = g.chance(0.2) ? g.range(1, 3) : 0;
      const consume = g.pick(["data", "iterate", "pipe", "readable"]);
      const lines = [
        `const { Readable, Transform, Writable } = require("node:stream");`,
        `const src = ${readableSource(g, chunks, objectMode)};`,
        `const t = ${transformSource(g, objectMode, failAt)};`,
        logStream("src"),
        logStream("t"),
        `src.pipe(t);`,
      ];
      if (g.chance(0.3) && !objectMode) lines.push(`t.setEncoding(${g.lit(g.pick(["utf8", "hex", "base64", "latin1"]))});`);
      switch (consume) {
        case "data":
          lines.push(`t.on("data", (chunk) => __log("data", chunk));`, `return await __settle(new Promise((resolve) => { t.on("end", () => resolve("ended")); t.on("error", () => resolve("errored")); }));`);
          break;
        case "iterate":
          lines.push(`const out = [];`, `for await (const chunk of t) out.push(chunk);`, `return out;`);
          break;
        case "pipe":
          lines.push(
            `const w = new Writable({ objectMode: ${objectMode}, write(chunk, encoding, callback) { __log("write", chunk, encoding); callback(); } });`,
            logStream("w"),
            `t.pipe(w);`,
            `return await __settle(new Promise((resolve) => { w.on("finish", () => resolve("finished")); t.on("error", () => resolve("errored")); }));`,
          );
          break;
        case "readable":
          lines.push(`t.on("readable", () => { let chunk; while ((chunk = t.read()) !== null) __log("read", chunk); });`, `return await __settle(new Promise((resolve) => { t.on("end", () => resolve("ended")); t.on("error", () => resolve("errored")); }));`);
          break;
      }
      return lines.join("\n");
    },
    writable: (g) => {
      const objectMode = g.chance(0.3);
      const schedule = g.pick(["callback()", "process.nextTick(callback)", "setImmediate(callback)", "callback(new Error(\"w\"))"]);
      const lines = [
        `const { Writable } = require("node:stream");`,
        `const w = new Writable({ objectMode: ${objectMode}, highWaterMark: ${g.pick([1, 3, 16, 0])}, decodeStrings: ${g.chance(0.7)}, write(chunk, encoding, callback) { __log("write", chunk, encoding); ${schedule}; },${g.chance(0.3) ? ` final(callback) { __log("final"); callback(); },` : ""} });`,
        logStream("w"),
        `w.on("drain", () => __log("drain"));`,
        `w.on("prefinish", () => __log("prefinish"));`,
      ];
      for (let index = g.range(1, 6); index > 0; index--) {
        lines.push(
          g.pick([
            () => step("write", `w.write(${g.lit(g.string(4))}${g.chance(0.3) ? `, ${g.lit(g.pick(["hex", "latin1", "utf8"]))}` : ""}${g.chance(0.3) ? ", (err) => __log(\"write cb\", err ?? null)" : ""})`),
            () => step("write-buffer", `w.write(${g.lit(g.bytes(3))})`),
            () => step("cork", `(w.cork(), w.writableCorked)`),
            () => step("uncork", `(w.uncork(), w.writableCorked)`),
            () => step("state", `[w.writableLength, w.writableNeedDrain, w.writableEnded, w.writableFinished]`),
            () => step("write-invalid", `w.write(${g.pick(["null", "1", "{}", "undefined"])})`),
          ])(),
        );
      }
      lines.push(g.pick([
        `w.end(${g.chance(0.5) ? g.lit(g.string(3)) : ""});`,
        `w.end(() => __log("end cb"));`,
        `w.destroy(${g.chance(0.5) ? "new Error(\"destroyed\")" : ""});`,
        `w.end(); ${step("write-after-end", `w.write("late", (err) => __log("late cb", err))`)}`,
      ]));
      lines.push(`return await __settle(new Promise((resolve) => w.on("close", () => resolve([w.destroyed, w.writableFinished, w.errored]))));`);
      return lines.join("\n");
    },
    pipeline: (g) => {
      const objectMode = g.chance(0.3);
      const chunks = g.many(g.range(0, 4), () => g.string(4) || "y");
      const failAt = g.chance(0.4) ? g.range(1, 3) : 0;
      const transforms = g.many(g.range(0, 2), () => transformSource(g, objectMode, g.chance(0.5) ? failAt : 0));
      const dest = `new Writable({ objectMode: ${objectMode}, write(chunk, encoding, callback) { __log("sink", chunk); callback(${g.chance(0.1) ? "new Error(\"sink\")" : ""}); } })`;
      const streams = [readableSource(g, chunks, objectMode), ...transforms, dest].join(", ");
      return g.pick([
        () => `const { Readable, Transform, Writable, pipeline } = require("node:stream");\nreturn await __settle(new Promise((resolve) => pipeline(${streams}, (err) => resolve(err ?? "ok"))));`,
        () => `const { Readable, Transform, Writable } = require("node:stream");\nconst { pipeline } = require("node:stream/promises");\nawait pipeline(${streams});\nreturn "ok";`,
        () => `const { Readable, Transform, Writable, finished } = require("node:stream");\nconst dest = ${dest};\n${readableSource(g, chunks, objectMode)}.pipe(dest);\nreturn await __settle(new Promise((resolve) => finished(dest, (err) => resolve(err ?? "finished"))));`,
      ])();
    },
    read: (g) => {
      const objectMode = g.chance(0.3);
      const lines = [
        `const { Readable } = require("node:stream");`,
        `const r = new Readable({ objectMode: ${objectMode}, highWaterMark: ${g.pick([1, 4, 16])}, read() {} });`,
        `r.on("error", (error) => __log("error", error));`,
      ];
      if (!objectMode && g.chance(0.3)) lines.push(`r.setEncoding(${g.lit(g.pick(["utf8", "hex", "base64", "utf16le"]))});`);
      for (let index = g.range(2, 8); index > 0; index--) {
        lines.push(
          g.pick([
            () => step("push", `r.push(${g.chance(0.5) ? g.lit(g.string(4)) : g.lit(g.bytes(4))})`),
            () => step("read", `r.read(${g.chance(0.3) ? "" : g.lit(g.pick([0, 1, 2, 3, 5, 100, -1]))})`),
            () => step("unshift", `(r.unshift(${g.lit(g.string(2) || "u")}), r.readableLength)`),
            () => step("length", `[r.readableLength, r.readableEnded, r.readableFlowing]`),
            () => step("end", `r.push(null)`),
          ])(),
        );
      }
      lines.push(`return [r.readableLength, r.readableEnded, r.destroyed];`);
      return lines.join("\n");
    },
    misc: (g) =>
      g.pick([
        () => `const { PassThrough } = require("node:stream");\nconst p = new PassThrough(${g.pick(["", "{ objectMode: true }"])});\np.write(${g.lit(g.string(3) || "p")});\np.end(${g.lit(g.string(3) || "q")});\nconst out = [];\nfor await (const chunk of p) out.push(chunk);\nreturn out;`,
        () => `const { Readable } = require("node:stream");\nconst out = [];\nfor await (const chunk of Readable.from(${g.lit(g.pick([g.string(5), [1, 2], ["a", null], [Buffer.from("x")]]))})) out.push(chunk);\nreturn out;`,
        () => `const { Duplex } = require("node:stream");\nconst d = new Duplex({ read() { this.push("r"); this.push(null); }, write(chunk, encoding, callback) { __log("w", chunk); callback(); } });\nd.end("in");\nconst out = [];\nfor await (const chunk of d) out.push(chunk);\nreturn out;`,
        () => `const stream = require("node:stream");\nconst r = new stream.Readable({ read() {} });\nr.on("error", (error) => __log("error", error));\nconst before = [stream.isReadable(r), stream.isErrored(r)];\nr.destroy(new Error("x"));\nawait new Promise((resolve) => r.on("close", resolve));\nreturn [before, stream.isReadable(r), stream.isErrored(r), r.destroyed];`,
        () => `const stream = require("node:stream");\nreturn [stream.getDefaultHighWaterMark(false), stream.getDefaultHighWaterMark(true)];`,
        () => `const { Readable } = require("node:stream");\nconst r = new Readable({ read() { this.push(${g.lit(g.string(3) || "c")}); this.push(null); } });\nr.pause();\nr.on("data", (c) => __log("data", c));\nawait new Promise((resolve) => setImmediate(resolve));\n__log("flowing", r.readableFlowing);\nr.resume();\nreturn await __settle(new Promise((resolve) => r.on("end", () => resolve("ended"))));`,
        () => `const { Writable } = require("node:stream");\nconst w = new Writable({ writev(chunks, callback) { __log("writev", chunks.map((c) => c.chunk)); callback(); } });\nw.cork();\nw.write("a");\nw.write("b");\nprocess.nextTick(() => w.uncork());\nw.end("c");\nreturn await __settle(new Promise((resolve) => w.on("finish", () => resolve("finished"))));`,
      ])(),
    web: (g) => {
      const chunks = g.many(g.range(0, 4), () => g.string(4) || "w");
      const objectMode = g.chance(0.4);
      const bytes = `(chunk) => (typeof chunk === "string" ? chunk : Array.from(chunk))`;
      return g.pick([
        () => `const { Readable } = require("node:stream");\nconst web = Readable.toWeb(Readable.from(${g.lit(chunks)}, { objectMode: ${objectMode} })${g.chance(0.3) ? `, { strategy: new CountQueuingStrategy({ highWaterMark: ${g.range(0, 3)} }) }` : ""});\nconst out = [];\nfor await (const chunk of web) out.push((${bytes})(chunk));\nreturn out;`,
        () => `const { Readable } = require("node:stream");\nconst r = new Readable({ read() { ${g.pick(["this.destroy(new Error(\"r\"));", "this.push(\"a\"); this.push(null);", "this.destroy();"])} } });\nconst reader = Readable.toWeb(r).getReader();\nconst results = [];\nfor (let i = 0; i < 3; i++) results.push(await reader.read().then((result) => [result.done, result.value && Array.from(result.value)], (error) => [error.name, error.code, error.cause?.code]));\nreturn results;`,
        () => `const { Readable } = require("node:stream");\nconst source = new ReadableStream({ start(c) { for (const chunk of ${g.lit(chunks)}) c.enqueue(${objectMode ? "chunk" : "new TextEncoder().encode(chunk)"}); ${g.pick(["c.close();", "c.error(new Error(\"web\"));"])} } });\nconst r = Readable.fromWeb(source, { objectMode: ${objectMode}${g.chance(0.3) ? ", encoding: \"utf8\"" : ""} });\n${logStream("r")}\nconst out = [];\nr.on("data", (chunk) => out.push(typeof chunk === "string" ? chunk : Array.from(chunk)));\nreturn await __settle(new Promise((resolve) => r.on("close", () => resolve(out))));`,
        () => `const { Writable } = require("node:stream");\nconst w = new Writable({ objectMode: ${objectMode}, highWaterMark: ${g.pick([1, 2, 16])}, write(chunk, encoding, callback) { __log("write", typeof chunk === "string" ? chunk : Array.from(chunk)); ${g.pick(["callback();", "setImmediate(callback);", "callback(new Error(\"w\"));"])} } });\nconst writer = Writable.toWeb(w).getWriter();\nfor (const chunk of ${g.lit(chunks)}) await writer.write(${objectMode ? "chunk" : "new TextEncoder().encode(chunk)"}).catch((error) => __log("write rejected", error.message));\nreturn await writer.close().then(() => ["closed", w.writableFinished], (error) => ["close rejected", error.name]);`,
        () => `const { Writable } = require("node:stream");\nconst sink = new WritableStream({ write(chunk) { __log("sink", typeof chunk === "string" ? chunk : Array.from(chunk)); ${g.chance(0.2) ? "throw new Error(\"sink\");" : ""} } });\nconst w = Writable.fromWeb(sink, { objectMode: ${objectMode}, decodeStrings: ${g.chance(0.5)} });\n${logStream("w")}\nfor (const chunk of ${g.lit(chunks)}) w.write(chunk);\nw.end();\nreturn await __settle(new Promise((resolve) => w.on("close", () => resolve([w.writableFinished, w.destroyed]))));`,
        () => `const { Duplex, PassThrough } = require("node:stream");\nconst { readable, writable } = Duplex.toWeb(new PassThrough(${objectMode ? "{ objectMode: true }" : ""}));\nconst writer = writable.getWriter();\nfor (const chunk of ${g.lit(chunks)}) writer.write(${objectMode ? "chunk" : "new TextEncoder().encode(chunk)"});\nwriter.close();\nconst out = [];\nfor await (const chunk of readable) out.push((${bytes})(chunk));\nreturn out;`,
        () => `const { Duplex } = require("node:stream");\nconst d = Duplex.fromWeb(new TransformStream({ transform(chunk, controller) { controller.enqueue(chunk.toUpperCase()); } }), { objectMode: true });\nfor (const chunk of ${g.lit(chunks)}) d.write(chunk);\nd.end();\nconst out = [];\nfor await (const chunk of d) out.push(chunk);\nreturn out;`,
        () => `const { Readable } = require("node:stream");\nconst { EventEmitter } = require("node:events");\nconst old = new EventEmitter();\n${g.chance(0.5) ? "old.readableObjectMode = false;" : ""}\nconst r = Readable.wrap(old${g.chance(0.4) ? ", { highWaterMark: 2 }" : ""});\n__log("mode", r.readableObjectMode, r.readableHighWaterMark);\nsetImmediate(() => { for (const chunk of ${g.lit(chunks)}) old.emit("data", chunk); old.emit(${g.lit(g.pick(["end", "end", "close"]))}); });\nconst out = [];\nr.on("data", (chunk) => out.push(String(chunk)));\nreturn await __settle(new Promise((resolve) => r.on("close", () => resolve(out))));`,
        () => `const { Readable, Writable, Duplex } = require("node:stream");\nconst calls = [() => Readable.toWeb(${g.pick(["{}", "null", "new Readable()"])}${g.chance(0.5) ? `, ${g.pick(["{ type: \"bytes\" }", "{ type: \"x\" }", "1"])}` : ""}), () => Readable.fromWeb(${g.pick(["{}", "new ReadableStream()"])}, ${g.pick(["undefined", "{ objectMode: 1 }", "{ encoding: \"x\" }", "null"])}), () => Writable.fromWeb(${g.pick(["{}", "new WritableStream()"])}, ${g.pick(["undefined", "{ decodeStrings: 1 }", "{ objectMode: \"x\" }"])}), () => Writable.toWeb(${g.pick(["{}", "1", "new Writable()"])}), () => Duplex.fromWeb(${g.pick(["{}", "1", "{ readable: new ReadableStream(), writable: new WritableStream() }"])}), () => Duplex.toWeb(${g.pick(["new Readable()", "{}", "new Duplex()"])})];\nreturn calls.map((call) => { try { const result = call(); return [typeof result, result?.constructor?.name]; } catch (error) { return [error.name, error.code]; } });`,
      ])();
    },
    operators: (g) => {
      const values = g.many(g.range(0, 6), () => g.range(0, 9));
      const fn = (kind) => {
        const sync = g.chance(0.5) ? "" : "async ";
        return {
          map: `${sync}(x) => x * ${g.range(1, 3)} + ${g.range(0, 2)}`,
          filter: `${sync}(x) => x % ${g.range(2, 3)} === 0`,
          flatMap: g.pick([`${sync}(x) => [x, x]`, `(x) => Readable.from([x, -x])`, `${sync}(x) => []`]),
          reduce: `${sync}(a, x) => a + x`,
          predicate: `${sync}(x) => x > ${g.range(0, 9)}`,
        }[kind];
      };
      const options = () => (g.chance(0.3) ? `, { concurrency: ${g.range(1, 3)}${g.chance(0.5) ? `, highWaterMark: ${g.range(0, 2)}` : ""} }` : "");
      let chain = `Readable.from(${g.lit(values)})`;
      for (let index = g.range(0, 3); index > 0; index--) {
        chain += g.pick([
          () => `.map(${fn("map")}${options()})`,
          () => `.filter(${fn("filter")}${options()})`,
          () => `.flatMap(${fn("flatMap")})`,
          () => `.drop(${g.pick([0, 1, 2, "1.5", "\"2\"", "NaN"])})`,
          () => `.take(${g.pick([0, 1, 2, "1.5", "Infinity"])})`,
        ])();
      }
      const end = g.pick([
        () => `.toArray()`,
        () => `.reduce(${fn("reduce")}${g.chance(0.6) ? ", 0" : ""})`,
        () => `.some(${fn("predicate")})`,
        () => `.every(${fn("predicate")})`,
        () => `.find(${fn("predicate")})`,
        () => `.forEach((x) => { __log("each", x); })`,
      ])();
      return g.pick([
        () => `const { Readable } = require("node:stream");\nreturn await ${chain}${end};`,
        () => `const { Readable } = require("node:stream");\nconst out = [];\nfor await (const x of ${chain}) out.push(x);\nreturn out;`,
        () => `const { Readable } = require("node:stream");\nconst calls = [() => Readable.from([1]).map(${g.pick(["1", "null", "(x) => x"])}, ${g.pick(["1", "{ concurrency: 0 }", "{ concurrency: \"x\" }", "{ highWaterMark: -1 }", "{ signal: true }", "{}"])}), () => Readable.from([1]).drop(${g.pick(["-1", "-Infinity", "\"a\"", "1"])}), () => Readable.from([1]).take(1, ${g.pick(["1", "{ signal: 1 }", "undefined"])})];\nconst sync = calls.map((call) => { try { return typeof call(); } catch (error) { return [error.name, error.code, String(error).split(":")[0]]; } });\nconst rejected = await Promise.all([Readable.from([]).reduce(${g.pick(["1", "(a, b) => a + b"])}), Readable.from([1]).forEach(${g.pick(["1", "() => {}"])}), Readable.from([1]).toArray(${g.pick(["1", "{ signal: AbortSignal.abort() }", "undefined"])})].map((promise) => promise.then((value) => ["ok", value], (error) => [error.name, error.code])));\nreturn [sync, rejected];`,
      ])();
    },
  },
};

const FS_NAMES = ["a", "a", "b", "d", "d/e", "missing/x", "l"];
const FS_DATA = () => ["", "hello", "héllo wörld", "line1\nline2", "\u0000\u0001", "😀"];

function fsStep(g) {
  const name = () => `p(${g.lit(g.pick(FS_NAMES))})`;
  const data = () => g.lit(g.pick(FS_DATA()));
  return g.pick([
    () => step("writeFileSync", `fs.writeFileSync(${name()}, ${data()}${g.chance(0.4) ? `, ${g.lit(g.pick(["latin1", "hex", { flag: "a" }, { flag: "wx" }, { encoding: "base64" }, { flag: "r" }]))}` : ""})`),
    () => step("appendFileSync", `fs.appendFileSync(${name()}, ${data()})`),
    () => step("readFileSync", `fs.readFileSync(${name()}${g.chance(0.5) ? `, ${g.lit(g.pick(["utf8", "hex", "latin1", { encoding: "base64" }, { flag: "a+" }]))}` : ""})`),
    () => step("mkdirSync", `rel(fs.mkdirSync(${name()}${g.chance(0.5) ? ", { recursive: true }" : ""}))`),
    () => step("readdirSync", g.pick([`fs.readdirSync(${name()}).sort()`, `fs.readdirSync(${name()}, { withFileTypes: true }).map((d) => [d.name, d.isFile(), d.isDirectory(), d.isSymbolicLink()]).sort()`, `fs.readdirSync(dir, { recursive: true }).sort()`])),
    () => step("statSync", `(s => s && [s.size, s.isFile(), s.isDirectory(), s.isSymbolicLink()])(fs.${g.pick(["statSync", "lstatSync"])}(${name()}${g.chance(0.3) ? ", { throwIfNoEntry: false }" : ""}))`),
    () => step("existsSync", `fs.existsSync(${name()})`),
    () => step("renameSync", `fs.renameSync(${name()}, ${name()})`),
    () => step("unlinkSync", `fs.unlinkSync(${name()})`),
    () => step("rmdirSync", `fs.rmdirSync(${name()})`),
    () => step("rmSync", `fs.rmSync(${name()}${g.chance(0.6) ? `, ${g.lit(g.pick([{ recursive: true }, { force: true }, { recursive: true, force: true }]))}` : ""})`),
    () => step("copyFileSync", `fs.copyFileSync(${name()}, ${name()}${g.chance(0.3) ? ", fs.constants.COPYFILE_EXCL" : ""})`),
    () => step("truncateSync", `(fs.truncateSync(${name()}, ${g.lit(g.pick([0, 2, 10]))}), fs.readFileSync(${name()}))`),
    () => step("fd", `(() => { const fd = fs.openSync(${name()}, ${g.lit(g.pick(["r", "w", "a", "r+", "wx", "a+"]))}); try { const written = fs.writeSync(fd, ${data()}${g.chance(0.5) ? `, ${g.pick(["0", "2", "null"])}` : ""}); const buffer = Buffer.alloc(8); const read = fs.readSync(fd, buffer, 0, 8, ${g.pick(["0", "null", "1"])}); return [written, read, buffer]; } finally { fs.closeSync(fd); } })()`),
    () => step("accessSync", `fs.accessSync(${name()}${g.chance(0.5) ? `, fs.constants.${g.pick(["R_OK", "W_OK", "X_OK", "F_OK"])}` : ""})`),
    () => step("realpathSync", `rel(fs.realpathSync(${name()}))`),
    () => step("symlinkSync", `fs.symlinkSync(${g.lit(g.pick(["a", "d", "nowhere"]))}, ${name()})`),
    () => step("readlinkSync", `fs.readlinkSync(${name()})`),
    () => step("utimes", `(fs.utimesSync(${name()}, 1000, ${g.lit(g.pick([2000, 1.5e9, "1700000000"]))}), fs.statSync(${name()}).mtimeMs)`),
    () => step("chmod", `(fs.chmodSync(${name()}, ${g.lit(g.pick([0o644, 0o755, 0o600, "755"]))}), fs.statSync(${name()}).mode & 0o777)`),
    () => step("mkdtempSync", `rel(fs.mkdtempSync(p("t-"))).length`),
    () => step("promises.readFile", `await fs.promises.readFile(${name()}${g.chance(0.5) ? ', "utf8"' : ""})`),
    () => step("promises.writeFile", `await fs.promises.writeFile(${name()}, ${data()})`),
    () => step("promises.stat", `(s => [s.size, s.isDirectory()])(await fs.promises.stat(${name()}))`),
    () => step("promises.readdir", `(await fs.promises.readdir(${name()})).sort()`),
    () => step("promises.mkdir", `rel(await fs.promises.mkdir(${name()}, { recursive: ${g.chance(0.5)} }))`),
    () => step("promises.rm", `await fs.promises.rm(${name()}, { recursive: ${g.chance(0.5)}, force: ${g.chance(0.5)} })`),
    () => step("promises.handle", `await (async () => { const handle = await fs.promises.open(${name()}, ${g.lit(g.pick(["r", "w+", "a+"]))}); try { const { bytesWritten } = await handle.write(${data()}); const { bytesRead, buffer } = await handle.read(Buffer.alloc(6), 0, 6, 0); return [bytesWritten, bytesRead, buffer, (await handle.stat()).size]; } finally { await handle.close(); } })()`),
    () => step("callback.readFile", `await __cb(fs.readFile, ${name()}${g.chance(0.5) ? ', "utf8"' : ""})`),
    () => step("callback.writeFile", `await __cb(fs.writeFile, ${name()}, ${data()})`),
    () => step("callback.stat", `(s => s.size)(await __cb(fs.stat, ${name()}))`),
    () => step("callback.readdir", `(await __cb(fs.readdir, ${name()})).sort()`),
    () => step("callback.exists", `await new Promise((resolve) => fs.exists(${name()}, resolve))`),
    () => step("streams", `await (async () => { const w = fs.createWriteStream(${name()}${g.chance(0.3) ? ', { flags: "a" }' : ""}); w.write(${data()}); w.end(${data()}); await new Promise((resolve, reject) => { w.on("finish", resolve); w.on("error", reject); }); const chunks = []; for await (const chunk of fs.createReadStream(${name()}, { highWaterMark: ${g.pick([1, 3, 64])}${g.chance(0.3) ? ", start: 1, end: 4" : ""} })) chunks.push(chunk); return chunks; })()`),
    () => step("invalid", `fs.${g.pick(["readFileSync", "writeFileSync", "statSync", "mkdirSync", "openSync"])}(${g.pick(["null", "1", "{}", "''", "'\\u0000x'"])})`),
  ])();
}

const CP_PATHS = ["t", "t", "t/f", "t/s", "t/s/g", "t/l", "t/ld", "u", "u", "u/f", "u/s", "a", "d", "t/s/x", "missing/x", "u/"];

/**
 * The options argument of a `cp` call, with a leading comma, or nothing. Its filter logs its calls,
 * and returns a promise sometimes, more often if `async`.
 */
function cpOptions(g, async) {
  if (g.chance(0.1)) return g.pick(["", ", undefined", ", null", ", 1", ", []"]);
  const options = {};
  for (const key of ["recursive", "force", "errorOnExist", "dereference", "verbatimSymlinks", "preserveTimestamps"]) {
    if (g.chance(key === "recursive" ? 0.75 : 0.25)) options[key] = g.chance(0.9) ? g.chance(0.6) : g.pick([1, "yes", null]);
  }
  if (g.chance(0.1)) options.mode = g.pick([0, 1, 2, 8, -1, 1.5, "1"]);
  if (g.chance(0.4)) {
    const result = g.pick(["true", '!s.endsWith("g")', '!s.endsWith("s")', "s.length % 2 === 0", "0", '"yes"']);
    const body = g.chance(async ? 0.5 : 0.05) ? `Promise.resolve(${result})` : result;
    options.filter = { __raw: `(s, d) => (__log("filter", rel(s), rel(d)), ${body})` };
  } else if (g.chance(0.03)) {
    options.filter = 1;
  }
  return `, ${g.lit(options)}`;
}

function cpStep(g) {
  const name = () => `p(${g.lit(g.pick(CP_PATHS))})`;
  return g.pick([
    () => step("cpSync", `(fs.cpSync(${name()}, ${name()}${cpOptions(g, false)}), tree())`),
    () => step("cpSync", `(fs.cpSync(${name()}, ${name()}${cpOptions(g, false)}), tree())`),
    () => step("promises.cp", `(await fs.promises.cp(${name()}, ${name()}${cpOptions(g, true)}), tree())`),
    () => step("callback.cp", `(await __cb(fs.cp, ${name()}, ${name()}${cpOptions(g, true)}), tree())`),
    () => {
      const options = `preserveTimestamps: true${g.chance(0.3) ? ", force: false" : ""}${g.chance(0.3) ? ", filter: () => true" : ""}`;
      const [call, target] = g.pick([
        [`fs.cpSync(p("t/f"), p("pt"), { ${options} })`, "pt"],
        [`await fs.promises.cp(p("t/f"), p("pt"), { ${options} })`, "pt"],
        [`await __cb(fs.cp, p("t/f"), p("pt"), { ${options} })`, "pt"],
        [`fs.cpSync(p("t"), p("pt"), { recursive: true, ${options} })`, "pt/f"],
        [`await fs.promises.cp(p("t"), p("pt"), { recursive: true, ${options} })`, "pt/f"],
      ]);
      return step("preserveTimestamps", `(${call}, Math.round(fs.statSync(p("t/f")).mtimeMs) === Math.round(fs.statSync(p("${target}")).mtimeMs))`);
    },
    () => step("invalid", `fs.cpSync(${g.pick(["null", "1", "{}", "''", "'\\u0000x'"])}, ${name()})`),
    () => step("invalid", `fs.cp(${name()}, ${name()}${g.pick(["", ", {}"])})`),
  ])();
}

/**
 * The source of an operation that watches `target` with `options` after `setup`, repeats `action`
 * every 50 ms until a `change` event names `name`, and returns whether its type is `rename` or
 * `change`, and its filename. Repeating the action keeps the result independent of how long the
 * host takes to start watching, which macOS's FSEvents does after `fs.watch` returns.
 */
function watchUntil(setup, target, options, action, name) {
  return [
    `const fs = require("node:fs");`,
    `const dir = __dir();`,
    ...setup,
    `await new Promise((resolve) => setTimeout(resolve, 120));`,
    `const watcher = fs.watch(${target}, ${options});`,
    `const event = __settle(new Promise((resolve) => watcher.on("change", (type, filename) => { if (String(filename) === ${JSON.stringify(name)}) resolve([type === "rename" || type === "change", String(filename)]); })), 3000);`,
    `const timer = setInterval(() => ${action}, 50);`,
    `const result = await event;`,
    `clearInterval(timer);`,
    `watcher.close();`,
    `return result;`,
  ].join("\n");
}

const fsGenerator = {
  name: "fs",
  variants: {
    cp: (g) =>
      [
        `const fs = require("node:fs");`,
        `const dir = __dir();`,
        `const real = fs.realpathSync(dir);`,
        `const p = (name) => dir + "/" + name;`,
        `const rel = (value) => (typeof value === "string" ? value.replaceAll(real, "<dir>").replaceAll(dir, "<dir>") : value);`,
        `const tree = () => {`,
        `  const out = [];`,
        `  const walk = (name) => {`,
        `    const s = fs.lstatSync(p(name));`,
        `    if (s.isSymbolicLink()) return out.push(name + " -> " + rel(fs.readlinkSync(p(name))));`,
        `    if (!s.isDirectory()) return out.push(name + " = " + fs.readFileSync(p(name), "latin1") + " " + (s.mode & 0o777).toString(8));`,
        `    out.push(name + "/ " + (s.mode & 0o777).toString(8));`,
        `    for (const child of fs.readdirSync(p(name)).sort()) walk(name + "/" + child);`,
        `  };`,
        `  for (const child of fs.readdirSync(dir).sort()) walk(child);`,
        `  return out;`,
        `};`,
        `fs.mkdirSync(p("t/s"), { recursive: true });`,
        `fs.writeFileSync(p("t/f"), "f");`,
        `fs.writeFileSync(p("t/s/g"), "g");`,
        // Node.js's `cpSync` aborts the process on some dangling symlinks, so the links resolve.
        ...(g.chance(0.5) ? [`fs.symlinkSync(${g.lit(g.pick(["f", "s/g"]))}, p("t/l"));`] : []),
        ...(g.chance(0.3) ? [`fs.symlinkSync(${g.pick(['p("t/s")', '"s"'])}, p("t/ld"));`] : []),
        ...(g.chance(0.5) ? [`fs.mkdirSync(p("u"));`, `fs.writeFileSync(p("u/f"), "old");`] : []),
        ...(g.chance(0.3) ? [`fs.writeFileSync(p("a"), "a");`] : []),
        ...(g.chance(0.15) ? [`fs.chmodSync(p("t/f"), 0o444);`] : []),
        ...(g.chance(0.15) ? [`fs.chmodSync(p("t/s"), 0o700);`] : []),
        ...g.many(g.range(1, 4), () => cpStep(g)),
        `return tree();`,
      ].join("\n"),

    watchFile: (g) => {
      const exists = g.chance(0.5);
      const action = exists
        ? g.pick([`fs.writeFileSync(file, "changed!")`, `fs.unlinkSync(file)`, `fs.truncateSync(file, 0)`])
        : `fs.writeFileSync(file, ${g.lit(g.pick(["", "abc", "héllo"]))})`;
      return [
        `const fs = require("node:fs");`,
        `const file = __dir() + "/watched";`,
        ...(exists ? [`fs.writeFileSync(file, ${g.lit(g.pick(["x", "hello"]))});`] : []),
        `const events = [];`,
        `const shape = (s) => [s.size, s.isFile(), s.mtimeMs === 0, s.ino === 0, s.nlink];`,
        `const done = new Promise((resolve) => {`,
        `  fs.watchFile(file, { interval: ${g.pick([1, 5, 20])} }, (curr, prev) => {`,
        `    events.push([shape(curr), shape(prev)]);`,
        `    if (events.length === ${exists ? 1 : 2}) resolve();`,
        exists ? "" : `    else ${action};`,
        `  });`,
        `});`,
        exists ? `await new Promise((resolve) => setTimeout(resolve, 50));
${action};` : "",
        `const settled = await __settle(done, 3000);`,
        `await new Promise((resolve) => setTimeout(resolve, 60));`,
        `const watcher = fs.watchFile(file, () => {});`,
        `const stops = [];`,
        `watcher.on("stop", () => stops.push("stop"));`,
        `fs.unwatchFile(file${g.chance(0.5) ? "" : ", () => {}"});`,
        `const before = stops.length;`,
        `fs.unwatchFile(file);`,
        `await new Promise((resolve) => setImmediate(resolve));`,
        `return [settled, events, before, stops];`,
      ].join("\n");
    },
    watch: (g) =>
      g.pick([
        () =>
          watchUntil(
            [`fs.mkdirSync(dir + "/w");`],
            `dir + "/w"`,
            g.lit(g.pick([{}, { recursive: true }, { encoding: "buffer" }, { ignore: "*.log" }])),
            `fs.writeFileSync(dir + "/w/f", String(Date.now()))`,
            "f",
          ),
        () =>
          watchUntil(
            [`fs.mkdirSync(dir + "/w/d", { recursive: true });`],
            `dir + "/w"`,
            "{ recursive: true }",
            `fs.writeFileSync(dir + "/w/d/f", String(Date.now()))`,
            "d/f",
          ),
        () => watchUntil([`fs.writeFileSync(dir + "/f", "x");`], `dir + "/f"`, "{}", `fs.appendFileSync(dir + "/f", "more")`, "f"),
        () => [
          `const fs = require("node:fs");`,
          `const watcher = fs.watch(__dir()${g.chance(0.5) ? ", { persistent: false }" : ""});`,
          `const events = [];`,
          `watcher.on("close", () => events.push("close"));`,
          `events.push(watcher.ref() === watcher, watcher.unref() === watcher);`,
          `watcher.close();`,
          `events.push("closed", watcher._handle);`,
          `watcher.close();`,
          `await new Promise((resolve) => setImmediate(resolve));`,
          `return events;`,
        ].join("\n"),
        () => {
          const options = g.pick([
            1, "bogus", { encoding: "bogus" }, { ignore: "" }, { ignore: [1] }, { ignore: [/x/, "*.js", ""] }, { signal: 1 },
            { recursive: "1" }, { persistent: 1 }, { throwIfNoEntry: false }, { maxQueue: "x" }, { overflow: "x" },
          ]);
          const target = g.pick(["__dir()", "__dir() + \"/missing\"", "1", "null", "new URL(\"file:///nonexistent-dir\")"]);
          return g.pick([
            `const w = require("node:fs").watch(${target}, ${g.lit(options)});\nconst handle = typeof w._handle;\nw.close();\nreturn handle;`,
            `const it = require("node:fs").promises.watch(${target}, ${g.lit(options)});\nconst first = await Promise.race([it.next().then(() => "no error", (error) => error), new Promise((resolve) => setTimeout(resolve, 300, "no error"))]);\nawait it.return();\nreturn first;`,
            `const w = require("node:fs").watchFile(${target}, ${g.lit(options)}, () => {});\nrequire("node:fs").unwatchFile(${target});\nreturn typeof w._handle;`,
          ]);
        },
        () => [
          `const fs = require("node:fs");`,
          `const dir = __dir();`,
          `const ac = new AbortController();`,
          `const iterator = fs.promises.watch(dir, { signal: ac.signal });`,
          `const first = iterator.next();`,
          `${g.chance(0.5) ? "ac.abort();" : `ac.abort(new Error("why"));`}`,
          `const settled = await __settle(first.then((value) => ["value", value], (error) => ["error", error.name, error.code, error.cause?.name, error.cause?.message === "why"]), 1000);`,
          `return [settled, await iterator.next()];`,
        ].join("\n"),
      ])(),
    sequence: (g) =>
      [
        `const fs = require("node:fs");`,
        `const dir = __dir();`,
        `const p = (name) => dir + "/" + name;`,
        `const rel = (value) => (typeof value === "string" ? value.replaceAll(dir, "<dir>") : value);`,
        ...(g.chance(0.7) ? [`fs.writeFileSync(p("a"), ${g.lit(g.pick(FS_DATA()))});`] : []),
        ...(g.chance(0.5) ? [`fs.mkdirSync(p("d"));`] : []),
        ...g.many(g.range(2, 8), () => fsStep(g)),
        `return fs.readdirSync(dir).sort();`,
      ].join("\n"),
  },
};

const COMPRESSORS = [
  ["deflateSync", "inflateSync", "deflate", "inflate", "createDeflate", "createInflate"],
  ["gzipSync", "gunzipSync", "gzip", "gunzip", "createGzip", "createGunzip"],
  ["deflateRawSync", "inflateRawSync", "deflateRaw", "inflateRaw", "createDeflateRaw", "createInflateRaw"],
  ["brotliCompressSync", "brotliDecompressSync", "brotliCompress", "brotliDecompress", "createBrotliCompress", "createBrotliDecompress"],
  ["zstdCompressSync", "zstdDecompressSync", "zstdCompress", "zstdDecompress", "createZstdCompress", "createZstdDecompress"],
];

function zlibData(g) {
  return g.pick([() => g.bytes(40), () => Buffer.from(g.string(10).repeat(g.range(1, 20))), () => Buffer.alloc(0), () => Buffer.alloc(g.range(1, 3000), g.int(256))])();
}

function zlibOptions(g, index, valid = true) {
  if (index === 4) {
    return g.pick([undefined, { params: { [zlibHost.constants.ZSTD_c_compressionLevel]: g.range(1, 19) } }, { chunkSize: 64 }, ...(valid ? [] : [{ params: { 999: 1 } }, { params: { 99: 1 } }, { flush: 3 }])]);
  }
  if (index === 3) {
    return g.pick([undefined, { params: { [zlibHost.constants.BROTLI_PARAM_QUALITY]: g.range(0, 11) } }, { chunkSize: 64 }, ...(valid ? [] : [{ params: { 999: 1 } }, { chunkSize: 1 }])]);
  }
  return g.pick([
    undefined,
    { level: g.range(-1, 9) },
    { windowBits: g.range(9, 15), memLevel: g.range(1, 9) },
    { strategy: g.range(0, 4) },
    { chunkSize: 64 },
    ...(valid ? [] : [{ level: 10 }, { windowBits: 7 }, { memLevel: 0 }, { strategy: 5 }, { chunkSize: 10 }, { level: "1" }]),
  ]);
}

const zlib = {
  name: "zlib",
  variants: {
    fixture: (g) => {
      const index = g.int(COMPRESSORS.length);
      const [compress] = COMPRESSORS[index];
      const data = zlibData(g);
      let compressed = zlibHost[compress](data, zlibOptions(g, index));
      const mutation = g.pick(["none", "none", "truncate", "flip", "append", "other"]);
      if (mutation === "truncate") compressed = compressed.subarray(0, g.int(compressed.length));
      if (mutation === "flip" && compressed.length > 0) {
        compressed = Buffer.from(compressed);
        compressed[g.int(compressed.length)] ^= 1 << g.int(8);
      }
      if (mutation === "append") compressed = Buffer.concat([compressed, g.bytes(4)]);
      const decompress = mutation === "other" ? g.pick([...COMPRESSORS.map((c) => c[1]), "unzipSync"]) : g.chance(0.2) && index < 2 ? "unzipSync" : COMPRESSORS[index][1];
      const options = g.pick([undefined, undefined, { finishFlush: zlibHost.constants.Z_SYNC_FLUSH }, { maxOutputLength: g.range(1, 100) }, { chunkSize: 64 }]);
      return `return require("node:zlib").${decompress}(${args(g, compressed, /^(brotli|zstd)/.test(decompress) && options?.finishFlush ? undefined : options)});`;
    },
    roundtrip: (g) => {
      const index = g.int(COMPRESSORS.length);
      const [compress, decompress] = COMPRESSORS[index];
      const data = zlibData(g);
      const options = zlibOptions(g, index, g.chance(0.7));
      return `const zlib = require("node:zlib");\nconst data = ${g.lit(data)};\nconst out = zlib.${decompress}(zlib.${compress}(data, ${g.lit(options)}), ${g.lit(options)});\nreturn [out.equals(data), out.length];`;
    },
    async: (g) => {
      const index = g.int(COMPRESSORS.length);
      const [, , compress, decompress] = COMPRESSORS[index];
      const data = g.chance(0.5) ? g.lit(zlibData(g)) : g.lit(g.string(8));
      return `const zlib = require("node:zlib");\nconst compressed = await __cb(zlib.${compress}, ${data});\nconst out = await __cb(zlib.${decompress}, compressed);\nreturn out;`;
    },
    stream: (g) => {
      const index = g.int(COMPRESSORS.length);
      const [, , , , create, createInverse] = COMPRESSORS[index];
      const chunks = g.many(g.range(0, 4), () => g.chance(0.5) ? g.bytes(20) : Buffer.from(g.string(6)));
      return [
        `const zlib = require("node:zlib");`,
        `const c = zlib.${create}(${g.lit(zlibOptions(g, index))});`,
        `const d = zlib.${createInverse}();`,
        `const out = [];`,
        `d.on("data", (chunk) => out.push(chunk));`,
        `c.pipe(d);`,
        ...chunks.map((chunk) => `c.write(${g.lit(chunk)});${g.chance(0.3) ? " c.flush();" : ""}`),
        `c.end();`,
        `return await __settle(new Promise((resolve) => { d.on("end", () => resolve(Buffer.concat(out))); d.on("error", resolve); }));`,
      ].join("\n");
    },
    misc: (g) =>
      g.pick([
        () => `return require("node:zlib").crc32(${args(g, g.chance(0.5) ? g.string(8) : g.bytes(12), g.pick([undefined, 0, 1, 0xffffffff, -1, "x"]))});`,
        () => `const zlib = require("node:zlib");\nreturn [zlib.constants.Z_BEST_COMPRESSION, zlib.constants.Z_SYNC_FLUSH, zlib.constants.BROTLI_PARAM_QUALITY, zlib.codes.Z_DATA_ERROR, zlib.codes["-3"]];`,
        () => `return require("node:zlib").${g.pick(COMPRESSORS.map((c) => c[1]))}(${g.pick(["null", "1", "\"text\"", "{}", "new Uint8Array(0)"])});`,
        () => `return require("node:zlib").${g.pick(COMPRESSORS.map((c) => c[0]))}(${g.pick(["null", "1", "\"text\"", "new Uint8Array([1, 2])", "new ArrayBuffer(2)"])}).length > 0;`,
      ])(),
  },
};

/** A JavaScript expression for a random value, for inspection and comparison. */
function valueExpr(g, depth = 0) {
  const leaf = () =>
    g.pick([
      () => g.lit(g.pick([0, -0, 1, -1, 1.5, NaN, Infinity, 1e21, 123456789, 2 ** 53])),
      () => g.lit(g.string(8)),
      () => g.pick(["0n", "-12n", "12345678901234567890n"]),
      () => g.pick(["true", "false", "null", "undefined"]),
      () => `Symbol(${g.lit(g.pick(["", "s", "a b"]))})`,
    ])();
  if (depth >= 3 || g.chance(0.35)) return leaf();
  const child = () => valueExpr(g, depth + 1);
  const key = () => g.lit(g.pick(["a", "b", "key with space", "0", "1", "__proto__x", "", "é", "constructor"]));
  return g.pick([
    () => `[${g.many(g.range(0, 4), child).join(", ")}]`,
    () => `[${g.int(2) ? "1, , 3" : ", ,"}]`,
    () => `{ ${g.many(g.range(0, 4), () => `${key()}: ${child()}`).join(", ")} }`,
    () => `new Map([${g.many(g.range(0, 3), () => `[${child()}, ${child()}]`).join(", ")}])`,
    () => `new Set([${g.many(g.range(0, 3), child).join(", ")}])`,
    () => `new Date(${g.pick([0, 1e12, -1e10, NaN])})`,
    () => g.pick(["/a+b/gi", "/[/]\\//", "new RegExp(\"\")"]),
    () => g.lit(g.bytes(6)),
    () => `new Uint8Array([${g.many(3, () => g.int(256)).join(", ")}])`,
    () => `new Float64Array([1.5, -0, NaN])`,
    () => `Object.assign(Object.create(null), { ${key()}: ${child()} })`,
    () => `new (class Foo { constructor() { this.x = ${child()}; } })()`,
    () => g.pick(["new Number(3)", "new String(\"ab\")", "new Boolean(false)", "Object(Symbol(\"b\"))", "Object(1n)"]),
    () => g.pick(["function named() {}", "() => {}", "async function af() {}", "class K {}", "function* gen() {}"]),
    () => `{ get g() { return 1; }, set s(v) {} }`,
    () => `{ [Symbol("k")]: ${child()}, n: 1 }`,
    () => `(() => { const o = { a: ${child()} }; o.self = o; return o; })()`,
    () => `new WeakMap()`,
    () => `Promise.resolve(${child()})`,
    () => `[new Array(${g.range(0, 120)}).fill(0)]`,
    () => `"${"long ".repeat(g.range(10, 30))}"`,
  ])();
}

const util = {
  name: "util",
  variants: {
    format: (g) => {
      const tokens = g.many(g.range(0, 5), () => g.pick(["%s", "%d", "%i", "%f", "%j", "%%", "%c", "%x", "text", " ", "%", "%s%s"])).join("");
      const values = g.many(g.range(0, 5), () =>
        g.pick([() => g.lit(g.pick([1, -0, 1.5, NaN, "str", "42", null, undefined, true, ""])), () => g.pick(["10n", "Symbol(\"s\")", "{ a: 1 }", "[1, 2]", "{ toString() { return \"ts\"; } }", "new Date(0)", "Object.create(null)"])])(),
      );
      return `return require("node:util").format(${[g.chance(0.85) ? g.lit(tokens) : g.pick(["1", "{ a: 1 }", "null"]), ...values].join(", ")});`;
    },
    "format-inspect": (g) => {
      const token = g.pick(["%o", "%O", "%s", "%j", "%d"]);
      return `return require("node:util").format(${g.lit(`${token} ${token}`)}, ${valueExpr(g)}, ${valueExpr(g)});`;
    },
    inspect: (g) => {
      const options = Object.fromEntries(
        g.many(g.range(0, 3), () =>
          g.pick([
            ["depth", g.pick([0, 1, 2, null, Infinity])],
            ["compact", g.pick([false, true, 1, 3])],
            ["breakLength", g.pick([Infinity, 20, 80])],
            ["sorted", true],
            ["maxArrayLength", g.pick([0, 2, null])],
            ["maxStringLength", g.pick([0, 5, null])],
            ["numericSeparator", true],
            ["getters", true],
            ["showHidden", g.chance(0.5)],
          ]),
        ),
      );
      return `return require("node:util").inspect(${valueExpr(g)}${Object.keys(options).length ? `, ${g.lit(options)}` : ""});`;
    },
    "deep-equal": (g) => {
      const value = valueExpr(g);
      const other = g.chance(0.4) ? value : valueExpr(g);
      return `return require("node:util").isDeepStrictEqual(${value}, ${other});`;
    },
    types: (g) => {
      const check = g.pick([
        "isDate", "isRegExp", "isMap", "isSet", "isPromise", "isTypedArray", "isUint8Array", "isArrayBuffer", "isAnyArrayBuffer",
        "isBoxedPrimitive", "isNumberObject", "isStringObject", "isSymbolObject", "isBigIntObject", "isNativeError", "isAsyncFunction",
        "isGeneratorFunction", "isGeneratorObject", "isMapIterator", "isSetIterator", "isWeakMap", "isArrayBufferView", "isDataView", "isProxy",
      ]);
      const value = g.pick([() => valueExpr(g), () => g.pick(["new Error(\"e\")", "new TypeError()", "new Map().keys()", "new Set().values()", "(function* () {})()", "new Proxy({}, {})", "new DataView(new ArrayBuffer(2))", "new SharedArrayBuffer(2)", "(function () { return arguments; })()"])])();
      return `return require("node:util").types.${check}(${value});`;
    },
    misc: (g) =>
      g.pick([
        () => `return require("node:util").stripVTControlCharacters(${g.lit(g.pick(["\u001b[31mred\u001b[0m", "\u001b]8;;http://x\u0007link\u001b]8;;\u0007", "plain", "\u001b[1;2;3m", "\u009b31m"]))});`,
        () => `return require("node:util").toUSVString(${g.lit(g.string())});`,
        () => `return require("node:util").getSystemErrorName(${g.lit(g.pick([-2, -13, -17, -1, -4058, 0, 1]))});`,
        () => `const util = require("node:util");\nconst fn = util.promisify((a, cb) => ${g.pick(["cb(null, a + 1)", "cb(new Error(\"p\"))", "cb(null, a, 2)", "setImmediate(cb, null, a)"])});\nreturn await fn(1);`,
        () => `const util = require("node:util");\nconst fn = util.callbackify(async (a) => { ${g.pick(["return a * 2;", "throw new Error(\"c\");", "throw null;", "return undefined;"])} });\nreturn await new Promise((resolve) => fn(3, (err, value) => resolve([err, value])));`,
        () => `const util = require("node:util");\nfunction A() {}\nfunction B() {}\nutil.inherits(A, B);\nreturn [new A() instanceof B, A.super_ === B];`,
        () => `return require("node:util").styleText(${g.lit(g.pick(["red", ["bold", "underline"], "notAStyle", "italic"]))}, "text", { validateStream: false });`,
        () => `const util = require("node:util");\nreturn util.inspect({ [util.inspect.custom](depth, options) { return ${g.pick(["\"custom\"", "{ replaced: depth }", "options.stylize(\"s\", \"string\")", "this"])}; } });`,
        () => `return require("node:util").isArray(${valueExpr(g)});`,
        () => `const util = require("node:util");\nfunction F(a, b) {}\nF.prototype.m = 1;\nconst d = util.deprecate(F, "msg", "DEP_X"${g.pick(["", ", {}", ", { modifyPrototype: false }", ", { modifyPrototype: true }"])});\nreturn [d.prototype === F.prototype, Object.getPrototypeOf(d) === F, d.prototype.m, d.length];`,
      ])(),
    "parse-args": (g) => {
      const argv = Array.from({ length: g.range(0, 5) }, () =>
        g.pick(["-f", "-v", "-fv", "-ofile", "-o", "--flag", "--out", "--out=x", "--no-flag", "--no-color", "--", "-", "pos", "--unknown", "-x", "--out=-y", "-vo"]),
      );
      const options = g.pick([
        "{ flag: { type: \"boolean\", short: \"f\" }, out: { type: \"string\", short: \"o\" }, v: { type: \"boolean\", multiple: true } }",
        "{ out: { type: \"string\", multiple: true, default: [\"d\"] }, flag: { type: \"boolean\", default: false } }",
        "{ color: { type: \"boolean\" }, flag: { type: \"boolean\" } }",
        "{}",
      ]);
      const extra = g.pick(["", ", strict: false", ", allowPositionals: true", ", allowNegative: true", ", tokens: true", ", strict: false, allowNegative: true, tokens: true"]);
      return `const { values, positionals, tokens } = require("node:util").parseArgs({ args: ${JSON.stringify(argv)}, options: ${options}${extra} });\nreturn { values: { ...values }, positionals, tokens };`;
    },
    console: (g) => {
      const calls = Array.from({ length: g.range(1, 4) }, () =>
        g.pick(["c.log(\"a %s\", 1, { b: [2] })", "c.error(\"e\")", "c.warn(new Error(\"w\").message)", "c.info(1, 2)", "c.dir({ d: { e: 1 } }, { depth: 0 })", "c.assert(false, \"x %d\", 3)", "c.debug()"]),
      );
      const ctor = g.pick(["new Console(out, err)", "new Console({ stdout: out, stderr: err })", "new Console(out)", "Console(out)", "new Console({})"]);
      return `const { Console } = require("node:console");\nconst { Writable } = require("node:stream");\nconst chunks = [];\nconst out = new Writable({ write(chunk, encoding, cb) { chunks.push(["out", String(chunk)]); cb(); } });\nconst err = new Writable({ write(chunk, encoding, cb) { chunks.push(["err", String(chunk)]); cb(); } });\nconst c = ${ctor};\n${calls.join(";\n")};\nreturn chunks;`;
    },
  },
};

const ASSERTIONS = ["deepStrictEqual", "deepEqual", "notDeepStrictEqual", "notDeepEqual", "strictEqual", "notStrictEqual", "equal", "notEqual"];

const assert = {
  name: "assert",
  variants: {
    "call-tracker": (g) => {
      const expected = g.pick(["", "0", "1", "2", "-1", "1.5", "\"2\"", "undefined"]);
      const named = g.chance(0.5);
      const callCount = g.range(0, 3);
      return [
        `process.removeAllListeners("warning"); process.on("warning", () => {});`,
        `const tracker = new (require("node:assert").CallTracker)();`,
        named ? `function f_named(a) { return a * 2; }` : ``,
        `const tracked = tracker.calls(${named ? "f_named" : ""}${expected && (named ? ", " : "") + expected});`,
        ...Array.from({ length: callCount }, (_, i) => `__log("call", tracked.call({ i: ${i} }, ${i}));`),
        `__log("calls", tracker.getCalls(tracked).map((c) => [c.thisArg?.i, [...c.arguments]]));`,
        `__log("report", tracker.report().map((r) => [r.message, r.actual, r.expected, r.operator, typeof r.stack]));`,
        g.chance(0.3) ? `tracker.reset(${g.pick(["", "tracked", "() => {}"])});` : ``,
        `try { tracker.verify(); return "verified"; } catch (e) { return [e.name, e.code, e.message, e["message 0"], e["operator 0"]]; }`,
      ].join("\n");
    },
    compare: (g) => {
      const value = valueExpr(g);
      const other = g.pick([() => value, () => valueExpr(g), () => g.pick(["1", "\"1\"", "-0", "0", "NaN", "[]", "{}", "null", "undefined"])])();
      return `require("node:assert").${g.pick(ASSERTIONS)}(${value}, ${other});\nreturn "passed";`;
    },
    near: (g) => {
      const [a, b] = g.pick([
        ["[1, 2, 3]", "[1, 2, \"3\"]"], ["{ a: 1 }", "{ a: 1, b: undefined }"], ["-0", "0"], ["NaN", "NaN"], ["new Map([[1, 2]])", "new Map([[1, \"2\"]])"],
        ["new Set([1, 2])", "new Set([2, 1])"], ["[1, , 3]", "[1, undefined, 3]"], ["new Date(0)", "new Date(1)"], ["/a/g", "/a/i"],
        ["Object.create(null)", "{}"], ["new Number(1)", "new Number(2)"], ["{ [Symbol.for(\"s\")]: 1 }", "{ [Symbol.for(\"s\")]: 2 }"],
        ["new Error(\"a\")", "new Error(\"b\")"], ["Buffer.from(\"a\")", "new Uint8Array([97])"], ["[new Set([{ a: 1 }])]", "[new Set([{ a: 1 }])]"],
        ["new Map([[{}, 1]])", "new Map([[{}, 1]])"], ["{ a: [1, { b: 2 }] }", "{ a: [1, { b: \"2\" }] }"], ["\"a\"", "new String(\"a\")"],
        ["new Float64Array([NaN])", "new Float64Array([NaN])"], ["new WeakMap()", "new WeakMap()"], ["[0]", "[-0]"],
      ]);
      return `require("node:assert").${g.pick(ASSERTIONS)}(${g.chance(0.5) ? `${a}, ${b}` : `${b}, ${a}`});\nreturn "passed";`;
    },
    throws: (g) => {
      const thrown = g.pick(["new TypeError(\"bad value\")", "new Error(\"plain\")", "\"string\"", "{ code: \"X\", message: \"obj\" }", "null"]);
      const fn = g.chance(0.8) ? `() => { throw ${thrown}; }` : "() => {}";
      const expected = g.pick(["", ", TypeError", ", Error", ", RangeError", ", /bad/", ", /nomatch/", ", { message: \"bad value\" }", ", { code: \"X\" }", ", (err) => err instanceof TypeError", ", \"message only\"", ", { name: \"TypeError\", message: /value/ }"]);
      return g.pick([
        () => `require("node:assert").throws(${fn}${expected});\nreturn "passed";`,
        () => `require("node:assert").doesNotThrow(${fn}${expected.startsWith(", \"") ? expected : ""});\nreturn "passed";`,
        () => `await require("node:assert").rejects(async ${fn}${expected});\nreturn "passed";`,
        () => `await require("node:assert").${g.pick(["rejects", "doesNotReject"])}(${g.pick(["Promise.reject(new Error(\"r\"))", "Promise.resolve(1)", "1", "() => 1"])});\nreturn "passed";`,
      ])();
    },
    misc: (g) =>
      g.pick([
        () => `require("node:assert")${g.pick(["", ".ok"])}(${g.pick(["0", "1", "\"\"", "\"a\"", "null", "[]", "NaN"])});\nreturn "passed";`,
        () => `require("node:assert").ifError(${g.pick(["null", "undefined", "0", "new Error(\"i\")", "\"str\"", "false"])});\nreturn "passed";`,
        () => `require("node:assert").${g.pick(["match", "doesNotMatch"])}(${g.pick(["\"abc\"", "123", "\"\""])}, ${g.pick(["/b/", "/^x/", "\"b\""])});\nreturn "passed";`,
        () => `require("node:assert").fail(${g.pick(["", "\"msg\"", "new TypeError(\"t\")"])});\nreturn "passed";`,
        () => `const { AssertionError } = require("node:assert");\nconst e = new AssertionError({ actual: 1, expected: 2, operator: "strictEqual" });\nreturn [e.name, e.code, e.actual, e.expected, e.operator, e.generatedMessage];`,
        () => `require("node:assert/strict").equal(${g.pick(["1, \"1\"", "1, 1", "NaN, NaN", "{}, {}"])});\nreturn "passed";`,
      ])(),
  },
};

function urlString(g) {
  const pieces = [
    g.pick(["http:", "https:", "file:", "mailto:", "javascript:", "", "HTTP:", "foo:", "ws:"]),
    g.pick(["//", "//", "", "/", "///", "\\\\"]),
    g.pick(["", "", "user:pw@", "user@", ":@", "a%40b@"]),
    g.pick(["example.com", "EXAMPLE.com", "[::1]", "127.0.0.1", "xn--nxasmq6b", "ex ample", "a..b", "", "münchen.de", "0x7f.1", "host_name"]),
    g.pick(["", "", ":8080", ":", ":abc", ":0", ":65536"]),
    g.pick(["", "/", "/a/b", "/%7E", "/a/../b", "/ x", "\\a", "/é", "/a%2Fb"]),
    g.pick(["", "?a=1&b", "?", "?q=%zz&q=2", "?x=é"]),
    g.pick(["", "#h", "#", "#a b"]),
  ];
  return g.chance(0.1) ? g.string() : pieces.join("");
}

const url = {
  name: "url",
  variants: {
    parse: (g) => `return require("node:url").parse(${args(g, urlString(g), g.chance(0.3) ? true : undefined, g.chance(0.2) ? true : undefined)});`,
    format: (g) =>
      g.pick([
        () => `const url = require("node:url");\nreturn url.format(url.parse(${g.lit(urlString(g))}));`,
        () => `return require("node:url").format(${g.lit(Object.fromEntries(g.many(g.range(1, 4), () => g.pick([["protocol", g.pick(["http", "https:", "file", "mailto:"])], ["host", "h.com:81"], ["hostname", "h.com"], ["port", g.pick([80, "8080"])], ["pathname", g.pick(["/p", "p", "/a b"])], ["search", g.pick(["?s", "s"])], ["query", { a: "1", b: ["2", "3"] }], ["hash", g.pick(["#x", "x"])], ["slashes", true], ["auth", "u:p"]]))))});`,
        () => `return require("node:url").format(new URL(${g.lit(g.pick(["http://u:p@xn--nxasmq6b.com:8080/p?q#h", "https://münchen.de/", "file:///a/b"]))}), ${g.lit({ auth: g.chance(0.5), fragment: g.chance(0.5), search: g.chance(0.5), unicode: g.chance(0.5) })});`,
        () => `return require("node:url").format(${g.pick(["null", "1", "\"http://x/y\""])});`,
      ])(),
    resolve: (g) => `return require("node:url").resolve(${g.lit(g.pick([urlString(g), "http://a/b/c/d;p?q", "/one/two/three", "http://example.com/one", "mailto:a@b"]))}, ${g.lit(g.pick(["g", "./g", "../g", "/g", "//g", "?y", "#s", "", "..", "../../../g", "g;x?y#s", "http:g", urlString(g)]))});`,
    file: (g) =>
      g.pick([
        () => `return require("node:url").fileURLToPath(${g.lit(g.pick(["file:///a/b%20c", "file://host/x", "http://x/", "file:///a%2fb", "file:///%E2%82%AC", "file:///C:/x", "file:///a/b/", "not a url"]))});`,
        () => `return require("node:url").pathToFileURL(${g.lit(g.pick(["/a/b c", "/a%b", "/a#b?c", "/é/x", "/a\\b", "/a\nb", "/trailing/", "/a/./b/../c"]))}).href;`,
        () => `return require("node:url").fileURLToPath(new URL(${g.lit(g.pick(["file:///x/y", "file:///%00"]))}));`,
        () => `return require("node:url").urlToHttpOptions(new URL(${g.lit(g.pick(["http://u:p@a.com:81/p?q=1#h", "https://[::1]/", "http://a.com"]))}));`,
      ])(),
    domain: (g) => `return require("node:url").${g.pick(["domainToASCII", "domainToUnicode"])}(${g.lit(g.pick(["español.com", "xn--espaol-zwa.com", "xn--iñvalid.com", "EXAMPLE.com", "", "a..b", "中文.中国", "xn--", g.string(6)]))});`,
  },
};

function timerTree(g, depth, budget, allowTimeout) {
  const lines = [];
  for (let index = g.range(1, 3); index > 0 && budget.count > 0; index--) {
    budget.count--;
    const label = `t${budget.next++}`;
    const children = depth < 2 && g.chance(0.5) ? timerTree(g, depth + 1, budget, false) : "";
    const body = `() => { __log(${JSON.stringify(label)}); ${children} pending--; check(); }`;
    budget.scheduled++;
    const kind = g.pick(["nextTick", "microtask", "then", "immediate", ...(allowTimeout ? ["timeout"] : [])]);
    switch (kind) {
      case "nextTick":
        lines.push(`pending++; process.nextTick(${body});`);
        break;
      case "microtask":
        lines.push(`pending++; queueMicrotask(${body});`);
        break;
      case "then":
        lines.push(`pending++; Promise.resolve().then(${body});`);
        break;
      case "immediate":
        lines.push(`pending++; setImmediate(${body});`);
        break;
      case "timeout":
        lines.push(`pending++; setTimeout(${body}, ${40 * budget.timeouts++ + 20});`);
        break;
    }
  }
  return lines.join(" ");
}

const timers = {
  name: "timers",
  variants: {
    order: (g) => {
      const budget = { count: 8, next: 0, scheduled: 0, timeouts: 0 };
      return [
        `let pending = 0;`,
        `let finish;`,
        `const done = new Promise((resolve) => (finish = resolve));`,
        `const check = () => { if (pending === 0) finish("all ran"); };`,
        timerTree(g, 0, budget, true),
        `return await __settle(done, 2000);`,
      ].join("\n");
    },
    api: (g) =>
      g.pick([
        () => `const t = setTimeout(() => __log("fired"), 10);\nconst info = [typeof t.ref, t.hasRef(), t.unref() === t, t.hasRef(), typeof t[Symbol.toPrimitive]];\nclearTimeout(t);\nreturn info;`,
        () => `const t = setTimeout(() => __log("never"), 10);\nclearTimeout(${g.pick(["t", "+t", "t[Symbol.toPrimitive]()"])});\nawait new Promise((resolve) => setTimeout(resolve, 30));\nreturn "cleared";`,
        () => `const { setTimeout: sleep, setImmediate: immediate } = require("node:timers/promises");\nreturn [await sleep(${g.pick([1, 5])}, ${g.lit(g.pick(["v", 1, undefined]))}), await immediate(${g.lit(g.pick(["i", undefined]))})];`,
        () => `const { setTimeout: sleep } = require("node:timers/promises");\nconst controller = new AbortController();\nconst p = sleep(1000, "late", { signal: controller.signal });\ncontroller.abort(${g.pick(["", "\"reason\""])});\nreturn await p;`,
        () => `const { setInterval: every } = require("node:timers/promises");\nconst out = [];\nfor await (const value of every(5, "tick")) { out.push(value); if (out.length === ${g.range(1, 3)}) break; }\nreturn out;`,
        () => `let count = 0;\nreturn await new Promise((resolve) => { const id = setInterval(() => { __log("tick", ++count); if (count === ${g.range(1, 3)}) { clearInterval(id); resolve(count); } }, 5); });`,
        () => `return await new Promise((resolve) => setTimeout((...a) => resolve(a), 1, ${g.lit(g.pick([1, "a", null]))}, 2));`,
        () => `return await new Promise((resolve) => setImmediate((...a) => resolve(a), ${g.lit(g.pick([1, "a"]))}));`,
        () => `const timers = require("node:timers");\nreturn [timers.setTimeout === setTimeout, typeof timers.setImmediate, typeof timers.promises];`,
        () => `return setTimeout(${g.pick(["null", "\"code\"", "1", "{}"])}, 1);`,
        () => `const t = setTimeout(() => __log("refreshed fired"), 20);\nawait new Promise((resolve) => setTimeout(resolve, 10));\nt.refresh();\nawait new Promise((resolve) => setTimeout(resolve, 15));\n__log("before");\nawait new Promise((resolve) => setTimeout(resolve, 20));\nreturn "after";`,
      ])(),
  },
};

const punycode = {
  name: "punycode",
  variants: {
    convert: (g) => {
      const input = g.pick([() => g.string(), () => g.pick(["mañana", "xn--maana-pta", "bücher.example", "xn--bcher-kva.example", "ü", "--", "xn--", "a-", "abc-def", "\u0000", "日本語.jp", "xn--wgv71a119e.jp", "z".repeat(70)])])();
      return `return require("node:punycode").${g.pick(["encode", "decode", "toASCII", "toUnicode"])}(${g.lit(input)});`;
    },
    ucs2: (g) =>
      g.pick([
        () => `return require("node:punycode").ucs2.decode(${g.lit(g.string())});`,
        () => `return require("node:punycode").ucs2.encode([${g.many(g.range(0, 4), () => g.pick([65, 0x1f600, 0xd800, 0xffff, 0x10ffff, 0])).join(", ")}]);`,
        () => `return require("node:punycode").version;`,
      ])(),
  },
};

const os = {
  name: "os",
  variants: {
    constants: (g) =>
      g.pick([
        () => `const os = require("node:os");\nreturn [os.EOL, os.endianness(), os.platform(), os.type(), os.arch(), os.devNull];`,
        () => `const { constants } = require("node:os");\nreturn [constants.signals.SIGINT, constants.signals.SIGTERM, constants.errno.ENOENT, constants.errno.EACCES, constants.priority.PRIORITY_HIGH, constants.UV_UDP_REUSEADDR];`,
        () => `const os = require("node:os");\nreturn [typeof os.availableParallelism(), Array.isArray(os.cpus()), typeof os.totalmem(), typeof os.uptime(), typeof os.homedir(), typeof os.hostname(), typeof os.tmpdir()];`,
        () => `const os = require("node:os");\nconst info = os.userInfo();\nreturn Object.keys(info).sort();`,
        () => `return require("node:os").getPriority(${g.pick(["", "0", "\"x\"", "-1"])});`,
      ])(),
  },
};

/** An IPv4 address string, drawn so that addresses repeat and overlap, or an invalid one. */
function ipv4Address(g) {
  return g.pick([
    () => [g.pick([1, 10, 127, 192, 0, 255]), g.pick([0, 1, 168, 255]), g.int(3), g.int(12)].join("."),
    () => [g.pick([1, 10, 127, 192, 0, 255]), g.pick([0, 1, 168, 255]), g.int(3), g.int(12)].join("."),
    () => g.many(4, () => g.int(256)).join("."),
    () => g.pick(["01.2.3.4", "256.1.1.1", "1.2.3", "1.2.3.4.", " 1.2.3.4", "1.2.3.4\u0000x", "", "1.2.3.04", "0.0.0.0", "255.255.255.255", "abc", "::1"]),
  ])();
}

/** An IPv6 address string, including IPv4-mapped and IPv4-compatible ones, or an invalid one. */
function ipv6Address(g) {
  const group = () => g.pick(["0", "1", "ffff", "a", "fe80", "2001", "db8", "FFFF", g.int(65536).toString(16)]);
  return g.pick([
    () => `::ffff:${ipv4Address(g)}`,
    () => `::ffff:${ipv4Address(g)}`,
    () => `::${ipv4Address(g)}`,
    () => `::${g.chance(0.5) ? "" : group()}`,
    () => g.many(8, () => (g.chance(0.4) ? "0" : group())).join(":"),
    () => `${group()}::${group()}`,
    () => `2001:db8::${g.int(20).toString(16)}`,
    () => `::ffff:${group()}:${group()}`,
    () => `fe80::${group()}${g.chance(0.2) ? "%eth0" : ""}`,
    () => g.pick([":::", "12345::", "1:2:3:4:5:6:7:8:9", "1::2::3", "::1.2.3", "fe80::1%", "::ffff:1.2.3.4.5", "1:2:3:4:5:6:1.2.3.4", "1:2:3:4:5:6:7::", "::2:3:4:5:6:7:8", "1.2.3.4", g.string(6)]),
  ])();
}

/** An address string and the family argument to pass with it, which is sometimes wrong. */
function netAddress(g) {
  const ipv6 = g.chance(0.5);
  const address = ipv6 ? ipv6Address(g) : ipv4Address(g);
  const family = g.pick([
    () => (ipv6 ? "ipv6" : g.pick([undefined, "ipv4"])),
    () => (ipv6 ? "ipv6" : g.pick([undefined, "ipv4"])),
    () => (ipv6 ? g.pick(["IPv6", "IPV6"]) : "IPv4"),
    () => g.pick(["ipv4", "ipv6", "foo", "", 4, null]),
  ])();
  return [address, family];
}

/** The source of a `SocketAddress` construction, with options that are usually valid. */
function socketAddressSource(g, valid = true) {
  const [address, family] = netAddress(g);
  const options = {};
  if (g.chance(0.9)) options.address = valid && g.chance(0.8) ? (family === "ipv6" ? ipv6Address(g) : ipv4Address(g)) : address;
  if (g.chance(0.8)) options.family = valid ? g.pick(["ipv4", "ipv6", "IPv6", "IPV4", undefined]) : family;
  if (g.chance(0.3)) options.port = g.pick([0, 80, 65535, "443", "0x50", ...(valid ? [] : [-1, 65536, 1.5, "", true, null])]);
  if (g.chance(0.2)) options.flowlabel = g.pick([0, 1, 0xfffff, ...(valid ? [] : [-1, 1.5, "1", null])]);
  return g.chance(0.05) && !valid ? g.pick(["null", "[]", "1", "\"x\""]) : g.lit(options);
}

/** A prefix length argument, usually in range for one family or the other. */
function netPrefix(g) {
  return g.pick([0, 1, 7, 8, 12, 16, 24, 31, 32, 48, 64, 96, 100, 104, 120, 127, 128, () => g.pick([33, 129, -1, -0, 1.5, "8", NaN, undefined])]);
}

/** A CIDR string, usually valid. */
function cidrString(g) {
  const prefix = g.pick(["8", "16", "24", "32", "0", "64", "96", "120", "128", "08", "-1", "", "abc", "1e1", "+8", " 8", "33", "129"]);
  const address = g.chance(0.5) ? ipv4Address(g) : ipv6Address(g);
  return g.pick([`${address}/${prefix}`, `${address}/${prefix}`, address, `${address}/${prefix}/8`]);
}

/** The source of one random operation on the `BlockList` `bl`. */
function blockListOp(g) {
  const fam = (family) => (family === undefined ? "" : `, ${g.lit(family)}`);
  const [address, family] = netAddress(g);
  const objectOf = () => `new SocketAddress(${socketAddressSource(g)})`;
  const value = () => (g.chance(0.2) ? objectOf() : g.lit(address));
  switch (g.int(13)) {
    case 0:
    case 1:
      return `bl.addAddress(${value()}${fam(family)})`;
    case 2: {
      const [end] = netAddress(g);
      return `bl.addRange(${value()}, ${g.chance(0.2) ? objectOf() : g.lit(end)}${fam(family)})`;
    }
    case 3:
    case 4:
      return `bl.addSubnet(${value()}, ${g.lit(netPrefix(g))}${fam(family)})`;
    case 5:
      return `bl.addCIDR(${g.lit(cidrString(g))})`;
    case 6:
      return `bl.addCIDRs(${g.lit(g.many(g.range(0, 3), () => cidrString(g)))})`;
    case 7:
      return `bl.addAddresses([${g.many(g.range(0, 3), () => (g.chance(0.2) ? objectOf() : g.lit(netAddress(g)[0]))).join(", ")}]${fam(family)})`;
    case 8:
      return `bl.removeAddress(${value()}${fam(family)})`;
    case 9: {
      const [end] = netAddress(g);
      return `bl.removeRange(${value()}, ${g.lit(end)}${fam(family)})`;
    }
    case 10:
      return `bl.removeSubnet(${value()}, ${g.lit(netPrefix(g))}${fam(family)})`;
    case 11:
      return `bl.removeCIDR(${g.lit(cidrString(g))})`;
    default:
      return g.pick(["bl.clear()", `bl.addAddress(${g.pick(["1", "null", "{}"])})`, `bl.check(${g.pick(["1", "null"])})`, `bl.addSubnet("1.1.1.1", ${g.pick(["\"8\"", "null"])})`]);
  }
}

/** Source that sorts `bl.rules` so the address rules, which Node.js lists in hash order, compare. */
const SORTED_RULES = `const sortedRules = (rules) => [...rules.filter((r) => r.startsWith("Address")).sort(), ...rules.filter((r) => !r.startsWith("Address"))];`;

/** Source that runs `f` and records its error's name, code and message, or `null`, in `errors`. */
const RECORD = `const errors = [];\nconst record = (f) => { try { f(); errors.push(null); } catch (e) { errors.push([e.name, e.code, e.name === "SyntaxError" ? "" : e.message]); } };`;

const netGenerator = {
  name: "net",
  variants: {
    blockList: (g) => {
      const ops = g.many(g.range(1, 8), () => blockListOp(g));
      const checks = g.many(g.range(1, 8), () => {
        const [address, family] = netAddress(g);
        return g.chance(0.15) ? `new SocketAddress(${socketAddressSource(g)})` : `${g.lit(address)}${family === undefined ? "" : `, ${g.lit(family)}`}`;
      });
      return [
        `const { BlockList, SocketAddress } = require("node:net");`,
        SORTED_RULES,
        RECORD,
        `const bl = new BlockList();`,
        ...ops.map((op) => `record(() => ${op});`),
        `const checks = [];`,
        ...checks.map((check) => `try { checks.push(bl.check(${check})); } catch (e) { checks.push(e.code); }`),
        `return { rules: sortedRules(bl.rules), size: bl.size, json: sortedRules(bl.toJSON()), checks, errors };`,
      ].join("\n");
    },
    normalizeArgs: (g) => {
      const callArgs = g.pick(["", "{ port: 1 }", "80, \"host\"", "80, 1", "\"/pipe\"", "\"81\"", "() => {}", "{}, () => {}", "null"]);
      return `const a = require("node:net")._normalizeArgs([${callArgs}]);\nreturn [a[0], typeof a[1], a.length];`;
    },
    socketAddress: (g) => {
      const source = socketAddressSource(g, g.chance(0.6));
      return [
        `const { SocketAddress } = require("node:net");`,
        `const sa = new SocketAddress(${source});`,
        `return [sa.toJSON(), require("node:util").inspect(sa), JSON.stringify(sa), SocketAddress.isSocketAddress(sa), sa instanceof SocketAddress];`,
      ].join("\n");
    },
    parse: (g) => {
      const input = g.pick([
        () => `${ipv4Address(g)}:${g.pick([0, 1, 80, 443, 65535, 65536, ""])}`,
        () => `[${ipv6Address(g)}]:${g.pick([0, 1, 80, 8080, 65535])}`,
        () => `[${ipv6Address(g)}]`,
        () => g.pick(["1.2.3.4", "256", "0xffffffff", "0x.0x.0", "192.168.257:1", "999999999:12", "[1:0::]", "not an ip", "abc.123", "259.1.1.1", "12:12:12", "example.com", "user@1.2.3.4/x", "", "[fe80::1%25lo0]:1", "1.2.3.4:80"]),
        () => g.string(10),
      ])();
      const value = g.chance(0.05) ? g.pick(["1", "null", "undefined"]) : g.lit(input);
      return `const sa = require("node:net").SocketAddress.parse(${value});\nreturn sa === undefined ? sa : sa.toJSON();`;
    },
    fromJSON: (g) => {
      const rule = () =>
        g.pick([
          () => `Address: IPv4 ${ipv4Address(g)}`,
          () => `Address: IPv6 ${ipv6Address(g)}`,
          () => `Subnet: IPv4 ${ipv4Address(g)}/${g.int(40)}`,
          () => `Subnet: IPv6 ${ipv6Address(g)}/${g.int(140)}`,
          () => `Range: IPv4 ${ipv4Address(g)}-${ipv4Address(g)}`,
          () => `Range: IPv6 ${ipv6Address(g)}-${ipv6Address(g)}`,
          () => `${g.pick(["subnet: ipv6 ::/0 IPv6", "x Address: IPv4 1.2.3.4567", "Range: IPv4 1.1.1.1-2.2.2.2 IPv6", "Address: IPv6 ::ffff:1.2.3.4", "Subnet: IPv4 1234.1.1.1/8", "Address: IPv4 ", "IPv4 IPv6", g.string(8)])}`,
        ])();
      const data = g.many(g.range(0, 5), rule);
      const arg = g.pick([() => g.lit(data), () => g.lit(JSON.stringify(data)), () => g.pick(["1", "null", "\"x\"", "\"123\"", "[1]", "\"{}\""])])();
      return [`const { BlockList } = require("node:net");`, SORTED_RULES, RECORD, `const bl = new BlockList();`, `record(() => bl.fromJSON(${arg}));`, `return [sortedRules(bl.rules), errors];`].join("\n");
    },
    misc: (g) =>
      g.pick([
        () => `const { BlockList } = require("node:net");\nconst bl = new BlockList();\n${g.many(g.range(0, 3), () => blockListOp(g)).map((op) => `try { ${op}; } catch {}`).join("\n")}\nreturn require("node:util").inspect(bl, ${g.lit({ depth: g.pick([-1, 0, 1, null, undefined]) })}).replace(/'Address: [^']*'/g, "'Address'");`,
        () => `const { BlockList } = require("node:net");\nreturn [BlockList.PRIVATE_RANGES, Object.isFrozen(BlockList.PRIVATE_RANGES), BlockList.isBlockList(new BlockList()), BlockList.isBlockList(${g.pick(["{}", "null", "undefined", "1", "new (require(\"node:net\").SocketAddress)()"])})];`,
        () => `const { BlockList, SocketAddress } = require("node:net");\nreturn [BlockList, SocketAddress, BlockList.prototype.addAddress, BlockList.prototype.addRange, BlockList.prototype.check, BlockList.prototype.fromJSON, SocketAddress.parse, SocketAddress.isSocketAddress].map((f) => [f.name, f.length]);`,
        () => `const { ${g.pick(["BlockList", "SocketAddress"])}: C } = require("node:net");\ntry { C(); return "called"; } catch (e) { return [e.name, e.message]; }`,
        () => `const { BlockList } = require("node:net");\nconst bl = new BlockList();\nbl.addCIDRs(BlockList.PRIVATE_RANGES);\nreturn [${g.many(4, () => g.lit(netAddress(g)[0])).join(", ")}].map((a) => [bl.check(a), bl.check(a, "ipv6")]);`,
      ])(),
  },
};

const processGenerator = {
  name: "process",
  variants: {
    misc: (g) =>
      g.pick([
        () => `return await new Promise((resolve) => process.nextTick((...a) => resolve(a), ${g.many(g.range(0, 3), () => g.lit(g.pick([1, "a", null, undefined]))).join(", ")}));`,
        () => `const key = "NODE_DIFF_" + ${g.lit(g.int(1000))};\nprocess.env[key] = ${g.pick(["1", "true", "null", "undefined", "\"s\"", "{}"])};\nconst value = process.env[key];\nconst had = key in process.env;\ndelete process.env[key];\nreturn [value, had, key in process.env];`,
        () => `const seen = new Promise((resolve) => process.once("warning", (w) => resolve([w.name, w.code, w.message])));\nprocess.emitWarning(${g.pick(["\"msg\"", "\"msg\", \"CustomWarning\"", "\"msg\", { code: \"C1\", type: \"T\" }", "new Error(\"err warning\")", "\"msg\", \"DeprecationWarning\", \"DEP0\""])});\nreturn await __settle(seen);`,
        () => `return [typeof process.hrtime.bigint(), process.hrtime().length, process.hrtime([0, 0]).length, typeof process.uptime()];`,
        () => `const before = process.exitCode;\ntry { process.exitCode = ${g.pick(["1", "\"2\"", "1.5", "\"x\"", "null", "undefined"])}; return process.exitCode; } finally { process.exitCode = before; }`,
        () => `return [process.platform === require("node:os").platform(), typeof process.pid, process.argv.length >= 2, typeof process.execPath, process.release.name];`,
        () => `return Object.keys(process.memoryUsage()).sort();`,
        () => `const { EventEmitter } = require("node:events");\nconst e = new EventEmitter();\ne.setMaxListeners(${g.range(0, 2)});\nconst type = ${g.pick(["\"x\"", "Symbol(\"s\")", "null"])};\nconst seen = new Promise((resolve) => process.on("warning", function listener(w) { if (w.name !== "MaxListenersExceededWarning") return; process.off("warning", listener); resolve([w.type === type, w.count, w.emitter === e, w.message]); }));\nfor (let i = 0; i < 3; i++) e.on(type, () => {});\nreturn await __settle(seen);`,
        () => {
          const timeouts = g.range(0, 3);
          const immediates = g.range(0, 3);
          const unref = g.chance(0.3);
          return [
            `const count = () => { const info = process.getActiveResourcesInfo(); return [info.filter((t) => t === "Timeout").length, info.filter((t) => t === "Immediate").length]; };`,
            `const delta = (from) => count().map((n, i) => n - from[i]);`,
            `const before = count();`,
            `const timers = Array.from({ length: ${timeouts} }, (_, i) => setTimeout(() => {}, i));`,
            `const immediates = Array.from({ length: ${immediates} }, () => setImmediate(() => {}));`,
            unref ? `timers.forEach((t) => t.unref());` : ``,
            `const during = delta(before);`,
            `timers.forEach(clearTimeout); immediates.forEach(clearImmediate);`,
            `return [during, delta(before)];`,
          ].join("\n");
        },
        () => `return [process.cwd() === require("node:path").resolve("."), typeof process.chdir, typeof process.umask()];`,
        () => `return process.${g.pick(["kill", "chdir", "setUncaughtExceptionCaptureCallback", "emitWarning"])}(${g.pick(["", "null", "{}", "\"x\""])});`,
      ])(),
  },
};

// The implemented builtin modules whose export shapes `shape` compares.
const SHAPE_MODULES = [
  "assert", "async_hooks", "buffer", "constants", "crypto", "diagnostics_channel", "dns", "events", "fs", "http", "module",
  "net", "os", "path", "punycode", "querystring", "readline", "stream", "string_decoder", "timers", "tty", "url", "util",
  "zlib", "node:test",
];
const hostRequire = createRequire(import.meta.url);

/**
 * Compares what libraries that copy or enumerate exports see: a module's enumerable export names,
 * and for a member Node.js enumerates on an export, or on its prototype, the member's type and
 * whether it is enumerable where the prototype chain defines it. Exports the runtime only stubs
 * throw `ERR_METHOD_NOT_IMPLEMENTED`. One variant per module, and a log label per member, so
 * signatures name both.
 */
const shape = {
  name: "shape",
  variants: Object.fromEntries(
    SHAPE_MODULES.map((name) => [
      name,
      (g) => {
        const module = hostRequire(name);
        const exports = Object.keys(module).sort();
        if (g.chance(0.15) || exports.length === 0) return `__log("exports", Object.keys(require(${g.lit(name)})).sort());\nreturn null;`;
        const key = g.pick(exports);
        const value = module[key];
        const onPrototype = typeof value === "function" && value.prototype && g.chance(0.6);
        const holder = onPrototype ? value.prototype : value;
        const members = holder !== null && typeof holder === "object" || typeof holder === "function" ? Object.keys(holder) : [];
        const member = members.length > 0 ? g.pick(members) : null;
        return [
          `const v = require(${g.lit(name)})[${g.lit(key)}];`,
          `const stub = v?.[Symbol.for("starling.unimplemented")];`,
          `if (stub) throw Object.assign(new Error(\`The \${stub} method is not implemented\`), { code: "ERR_METHOD_NOT_IMPLEMENTED" });`,
          member === null
            ? `__log(${g.lit(key)}, typeof v);`
            : [
                `const holder = ${onPrototype ? "v.prototype" : "v"};`,
                `let owner = holder;`,
                `while (owner !== null && !Object.prototype.hasOwnProperty.call(owner, ${g.lit(member)})) owner = Object.getPrototypeOf(owner);`,
                `__log(${g.lit(`${key}${onPrototype ? ".prototype" : ""}.${member}`)}, typeof v, typeof holder[${g.lit(member)}], owner === null ? "absent" : Object.prototype.propertyIsEnumerable.call(owner, ${g.lit(member)}));`,
              ].join("\n"),
          `return null;`,
        ].join("\n");
      },
    ]),
  ),
};

/** The `CallSite` getters `stack` compares, which don't depend on the engine's line and column. */
const CALL_SITE_FIELDS =
  "[s.getFunctionName(), require(\"node:path\").basename(s.getFileName() ?? \"\"), s.getLineNumber(), typeof s.getColumnNumber(), s.isEval(), s.isNative(), s.isAsync(), s.isToplevel(), typeof s.toString()]";

/**
 * V8's stack trace API: the `CallSite`s `Error.prepareStackTrace` receives for frames of generated
 * functions, named `f_*`, called in different ways.
 */
const stackTrace = {
  name: "stack",
  variants: {
    capture: (g) => {
      const depth = g.range(1, 4);
      const skip = g.chance(0.3) ? g.int(depth) : null;
      const lines = [];
      for (let index = 0; index < depth; index++) {
        const call = index === depth - 1 ? `capture(${skip === null ? "" : `f_${skip}`})` : `f_${index + 1}()`;
        lines.push(`function f_${index}() { return ${call}; }`);
      }
      return [
        `function capture(fn) {`,
        `  const previous = Error.prepareStackTrace;`,
        `  Error.prepareStackTrace = (_, sites) => sites.filter((s) => /^f_/.test(s.getFunctionName() ?? "")).map((s) => ${CALL_SITE_FIELDS});`,
        `  const holder = {};`,
        `  try { Error.captureStackTrace(holder, fn); return holder.stack; } finally { Error.prepareStackTrace = previous; }`,
        `}`,
        ...lines,
        `return f_0();`,
      ].join("\n");
    },
    "error-stack": (g) => {
      const message = g.lit(g.string(6));
      return [
        `const previous = Error.prepareStackTrace;`,
        `Error.prepareStackTrace = (error, sites) => [error.message, sites.filter((s) => /^f_/.test(s.getFunctionName() ?? "")).map((s) => s.getFunctionName())];`,
        `function f_make() { return new ${g.pick(["Error", "TypeError", "RangeError"])}(${message}); }`,
        `function f_outer() { return f_make(); }`,
        `try { const error = f_outer(); return [error.stack, error.stack]; } finally { Error.prepareStackTrace = previous; }`,
      ].join("\n");
    },
    kinds: (g) => {
      const kind = g.pick(["method", "class", "arrow", "async", "nested", "property"]);
      const hook = `const previous = Error.prepareStackTrace;\nError.prepareStackTrace = (_, sites) => sites.filter((s) => /f_/.test(s.getFunctionName() ?? "")).map((s) => s.getFunctionName());\nconst grab = () => { const h = {}; Error.captureStackTrace(h); return h.stack; };`;
      const body = {
        method: `const o = { f_method() { return grab(); } };\nreturn o.f_method();`,
        class: `class f_C { constructor() { this.s = grab(); } f_m() { return grab(); } }\nconst c = new f_C();\nreturn [c.s, c.f_m()];`,
        arrow: `const f_arrow = () => grab();\nreturn f_arrow();`,
        async: `async function f_async() { await null; return grab(); }\nreturn await f_async();`,
        nested: `function f_outer() { return [1].map(function f_inner() { return grab(); })[0]; }\nreturn f_outer();`,
        property: `const o = {};\no.f_assigned = function () { return grab(); };\nreturn o.f_assigned();`,
      }[kind];
      return `${hook}\ntry {\n${body}\n} finally { Error.prepareStackTrace = previous; }`;
    },
  },
};

/** A summary of the recorded calls of the mock `name`, without their `stack`. */
function mockCalls(name) {
  return `${name}.mock.calls.map((c) => [c.arguments, c.result, c.error, c.this === undefined ? "undefined" : typeof c.this, typeof c.target])`;
}

/**
 * `node:test`'s mocks, with the module's `mock` tracker: `mock.fn()` and `mock.method()` called in
 * sequences of calls and changes to their implementations, compared by the calls they record and
 * the values the calls return, and the errors invalid arguments throw.
 */
const testRunner = {
  name: "test_runner",
  variants: {
    fn: (g) => {
      const impls = ["(a, b) => a + b", "function () { return this === undefined ? \"no this\" : typeof this; }", "() => { throw new TypeError(\"impl\"); }", "(...a) => a.length", "function F(x) { this.x = x; }"];
      const original = g.chance(0.2) ? "" : g.pick(impls);
      const implementation = original && g.chance(0.4) ? `, ${g.pick(impls)}` : "";
      const times = g.chance(0.3) ? `${original ? "" : "undefined, "}${implementation ? "" : original ? "undefined, " : ""}{ times: ${g.pick([1, 2, 3, "Infinity"])} }` : "";
      const optionsArgs = times ? `${original}${implementation}${original ? ", " : ""}${times}`.replace(/^undefined, undefined, /, "") : `${original}${implementation}`;
      const lines = [`const { mock } = require("node:test");`, `const f = mock.fn(${optionsArgs});`];
      for (let index = g.range(2, 8); index > 0; index--) {
        lines.push(
          g.pick([
            () => step("call", `f(${g.many(g.range(0, 3), () => g.lit(g.pick([1, 2, "x", null]))).join(", ")})`),
            () => step("call-this", `f.call(${g.pick(["{}", "null", "1"])}, 1)`),
            () => step("new", `typeof new f(1)`),
            () => step("once", `f.mock.mockImplementationOnce(${g.pick(impls)}${g.chance(0.3) ? `, ${g.pick([0, 1, 2, 5])}` : ""})`),
            () => step("impl", `f.mock.mockImplementation(${g.pick(impls)})`),
            () => step("count", `f.mock.callCount()`),
            () => step("reset", `f.mock.resetCalls()`),
            () => step("restore", `f.mock.restore()`),
          ])(),
        );
      }
      lines.push(`const calls = ${mockCalls("f")};`, `mock.reset();`, `return [f.mock.callCount(), calls, typeof f, f.length];`);
      return lines.join("\n");
    },
    method: (g) => {
      const accessor = g.pick(["method", "method", "getter", "setter"]);
      const onPrototype = g.chance(0.4);
      const body = `{ value: 2, m(x) { return x * this.value; }, get p() { return "real"; }, set p(v) { this.value = v; } }`;
      const lines = [
        `const { mock } = require("node:test");`,
        onPrototype ? `const proto = ${body};\nconst o = Object.create(proto);` : `const o = ${body};`,
      ];
      const name = accessor === "method" ? "m" : "p";
      const impl = g.pick(["", accessor === "setter" ? ", function (v) { this.value = v * 10; }" : accessor === "getter" ? ", () => \"mocked\"" : ", (x) => -x"]);
      const options = g.chance(0.4) ? `${impl ? "" : ", undefined"}, { times: ${g.pick([1, 2])} }` : "";
      const call = accessor === "method" ? `mock.method(o, "m"${impl}${options})` : `mock.${accessor}(o, "p"${impl}${options})`;
      lines.push(`const spy = ${call.replace(", undefined, {", ", {")};`);
      for (let index = g.range(1, 5); index > 0; index--) {
        lines.push(
          g.pick([
            () => step("use", accessor === "method" ? `o.m(${g.pick([1, 3])})` : accessor === "getter" ? "o.p" : `(o.p = ${g.pick([1, 4])}, o.value)`),
            () => step("own", `Object.prototype.hasOwnProperty.call(o, ${g.lit(name)})`),
            () => step("restore", `spy.mock.restore()`),
            () => step("restore-all", `mock.restoreAll()`),
            () => step("count", `spy.mock.callCount()`),
          ])(),
        );
      }
      lines.push(`const calls = ${mockCalls("spy")};`, `mock.reset();`, `return [calls, typeof Object.getOwnPropertyDescriptor(o, ${g.lit(name)})?.${accessor === "method" ? "value" : "get"}];`);
      return lines.join("\n");
    },
    errors: (g) =>
      g.pick([
        () => `return require("node:test").mock.fn(${g.pick(["1", "null", "\"f\"", "() => {}, 1", "() => {}, () => {}, 1"])});`,
        () => `return require("node:test").mock.fn(() => {}, { times: ${g.pick(["0", "-1", "1.5", "\"2\"", "NaN"])} });`,
        () => `return require("node:test").mock.method({ m() {} }, ${g.pick(["1", "null", "\"missing\"", "\"m\", 1", "\"m\", () => {}, 1"])});`,
        () => `return require("node:test").mock.method(${g.pick(["1", "null", "\"s\""])}, "m");`,
        () => `return require("node:test").mock.method({ m() {}, get g() { return 1; } }, ${g.pick(["\"m\", { getter: true }", "\"g\"", "\"m\", { getter: true, setter: true }", "\"m\", { getter: 1 }"])});`,
        () => `return require("node:test").mock.${g.pick(["getter", "setter"])}({ get g() { return 1; }, set g(v) {} }, "g", ${g.pick(["{ getter: false }", "{ setter: false }", "1"])});`,
        () => `const f = require("node:test").mock.fn();\nf();\nreturn f.mock.mockImplementationOnce(() => 1, ${g.pick(["0", "-1", "1.5", "\"1\"", "1"])});`,
        () => `const f = require("node:test").mock.fn();\nreturn f.mock.mockImplementation(${g.pick(["1", "null", "undefined"])});`,
      ])(),
  },
};

// Header names and values for requests and responses: mixed case, duplicates once lowercased, the
// names Node.js keeps one value of, joins with `; ` or collects in an array, and invalid ones.
const HTTP_NAMES = [
  "X-Test", "x-test", "X-TEST", "X-Dup", "x-dup", "Set-Cookie", "set-cookie", "Cookie", "cookie", "Content-Type",
  "content-type", "Age", "age", "Authorization", "Accept-Language", "X-Empty", "Via", "ETag", "Location",
];
const HTTP_VALUES = ["a", "b", "1", "", " padded ", "x=1; Path=/", "a, b", "é", "text/plain", "5", "0", "日本", "\u0001"];
// The values a client sends the same way: Node.js encodes the request head as UTF-8 when it sends
// it with a string body, and the runtime's HTTP client as Latin-1.
const ASCII_VALUES = HTTP_VALUES.filter((value) => /^[\x20-\x7e]*$/.test(value));
const HTTP_STATUSES = [200, 200, 201, 202, 204, 206, 301, 302, 304, 400, 404, 418, 500, 503, 599, 600, 99, 1000, 200.5, "201"];
const HTTP_MESSAGES = ["OK", "Custom", "", "Bad\nMessage", "Ünïcode"];

/** A header value for `setHeader`: a string, a number, or an array of strings. */
function httpValue(g) {
  return g.pick([
    () => g.pick(HTTP_VALUES),
    () => g.pick(HTTP_VALUES),
    () => g.int(100),
    () => g.many(g.range(1, 3), () => g.pick(HTTP_VALUES)),
  ])();
}

/** A body chunk expression for `write` or `end`, mostly valid, with an encoding if `encoded`. */
function httpChunk(g, encoded) {
  return g.pick([
    () => g.lit(g.pick(["hello", "", "a".repeat(20), "héllo", "😀", "chunk\r\n"])),
    () => g.lit(g.bytes(12)),
    () => (encoded ? `${g.lit(g.pick(["68656c6c6f", "aGVsbG8=", "é"]))}, ${g.lit(g.pick(["hex", "base64", "latin1"]))}` : g.lit("hello")),
    () => g.pick(["1", "null", "{}"]),
  ])();
}

/**
 * The source of a response program for `res`: header and status changes, `writeHead`, writes and
 * `end`, each recorded by `rec` with the error it throws. A response the program leaves unended is
 * ended with a valid status, so every exchange completes. String chunks have encodings if
 * `encoded`.
 */
function httpResponseProgram(g, encoded) {
  const steps = g.many(g.range(0, 4), () =>
    g.pick([
      () => `res.setHeader(${g.lit(g.pick(HTTP_NAMES))}, ${g.lit(httpValue(g))})`,
      () => `res.appendHeader(${g.lit(g.pick(HTTP_NAMES))}, ${g.lit(httpValue(g))})`,
      () => `res.removeHeader(${g.lit(g.pick([...HTTP_NAMES, "Content-Length", "Transfer-Encoding", "Connection", "Date"]))})`,
      () => `res.setHeader(${g.lit(g.pick(["Connection", "connection"]))}, ${g.lit(g.pick(["close", "keep-alive", "upgrade"]))})`,
      () => `res.statusCode = ${g.lit(g.pick(HTTP_STATUSES))}`,
      () => `res.statusMessage = ${g.lit(g.pick(HTTP_MESSAGES))}`,
      () => `res.sendDate = ${g.pick(["false", "true"])}`,
    ])(),
  );
  if (g.chance(0.5)) {
    const headers = g.pick([
      () => undefined,
      () => Object.fromEntries(g.many(g.range(1, 3), () => [g.pick(HTTP_NAMES), httpValue(g)])),
      () => g.many(g.range(1, 3), () => [g.pick(HTTP_NAMES), g.pick(HTTP_VALUES)]).flat(),
      () => g.many(g.range(1, 3), () => [g.pick(HTTP_NAMES), g.pick(HTTP_VALUES)]),
    ])();
    const message = g.chance(0.3) ? `, ${g.lit(g.pick(HTTP_MESSAGES))}` : "";
    const rest = headers === undefined ? "" : `, ${g.lit(headers)}`;
    steps.push(`res.writeHead(${g.lit(g.pick(HTTP_STATUSES))}${message}${rest})`);
  }
  if (g.chance(0.15)) steps.push("res.flushHeaders()");
  for (let index = g.range(0, 3); index > 0; index--) steps.push(`res.write(${httpChunk(g, encoded)})`);
  if (g.chance(0.2)) steps.push(`res.addTrailers(${g.lit({ [g.pick(["X-Trailer", "x-t"])]: g.pick(HTTP_VALUES) })})`);
  steps.push(g.chance(0.6) ? `res.end(${httpChunk(g, encoded)})` : "res.end()");
  return [
    ...steps.map((step) => `rec(() => ${step});`),
    `if (!res.writableEnded) { res.statusCode = 200; res.statusMessage = undefined; rec(() => res.end()); }`,
    `if (!res.writableEnded) res.destroy();`,
    `log.push(["response", res.headersSent, res.statusCode, String(res._header).replace(/Date: [^\\r]*\\r\\n/, "Date: -\\r\\n")]);`,
  ].join("\n");
}

/** The source that creates `server`, whose handler records each request in `log` and runs `program`. */
function httpServerSource(program, rawHeaders) {
  return [
    `const http = require("node:http");`,
    `const log = [];`,
    `const rec = (f) => { try { f(); } catch (e) { log.push(["throws", e.name, e.code]); } };`,
    `const server = http.createServer((req, res) => {`,
    `  const chunks = [];`,
    `  req.on("data", (chunk) => chunks.push(chunk));`,
    `  req.on("end", () => {`,
    `    log.push(["request", req.method, req.url, req.httpVersion, ${rawHeaders ? "req.rawHeaders, req.headers" : "names.map((name) => req.headers[name.toLowerCase()])"}, Buffer.concat(chunks).toString("latin1"), req.complete, req.trailers]);`,
    program.replace(/^/gm, "    "),
    `  });`,
    `});`,
    `await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));`,
    `const port = server.address().port;`,
  ].join("\n");
}

/** A raw HTTP/1.x request, the last of an exchange if `last`. */
function rawRequest(g, last) {
  const method = g.pick(["GET", "GET", "POST", "PUT", "HEAD", "DELETE", "OPTIONS", "PATCH", "FOO", "get"]);
  const path = g.pick(["/", "/a?b=1", "/%20x", "*", "http://example.com/x", "/a b"]);
  const version = g.pick(["HTTP/1.1", "HTTP/1.1", "HTTP/1.1", "HTTP/1.0", "HTTP/2.0"]);
  const lines = [`${method} ${path} ${version}`];
  if (g.chance(0.9)) lines.push(`${g.pick(["Host", "host", "HOST"])}: localhost`);
  for (let index = g.range(0, 3); index > 0; index--) lines.push(`${g.pick(HTTP_NAMES)}: ${g.pick(HTTP_VALUES.slice(0, 11))}`);
  if (g.chance(0.1)) lines.push("Expect: 100-continue");
  if (g.chance(0.05)) lines.push(g.pick(["Bad Header: x", "X-Bad: a\rb", ": empty", "X-Ctl: \u0007"]));
  const body = g.chance(0.4) ? g.pick(["hello", "", "a=1&b=2", "é"]) : null;
  let encoded = "";
  if (body !== null) {
    if (g.chance(0.5)) {
      lines.push(`Content-Length: ${g.chance(0.9) ? Buffer.byteLength(body) : g.pick(["abc", "-1", "1, 2"])}`);
      encoded = body;
    } else {
      lines.push(`Transfer-Encoding: ${g.pick(["chunked", "chunked", "gzip, chunked", "chunked, gzip"])}`);
      const bytes = Buffer.from(body);
      let offset = 0;
      while (offset < bytes.length) {
        const size = Math.min(bytes.length - offset, g.range(1, 4));
        encoded += `${size.toString(16)}${g.chance(0.2) ? ";ext=1" : ""}\r\n${bytes.subarray(offset, offset + size).toString("latin1")}\r\n`;
        offset += size;
      }
      encoded += `0\r\n${g.chance(0.3) ? "X-Trailer: t\r\n" : ""}\r\n`;
    }
  }
  if (last && version !== "HTTP/1.0") lines.push(g.pick(["Connection: close", "connection: Close"]));
  if (!last && version === "HTTP/1.0") lines.push("Connection: keep-alive");
  return `${lines.join("\r\n")}\r\n\r\n${encoded}`;
}

const httpGenerator = {
  name: "http",
  variants: {
    // A request from `http.request` and the response the client receives. The client's
    // `statusMessage` is left out, since the runtime's HTTP client reports the standard reason
    // phrase, and so are the request headers the op didn't set, which the clients choose.
    request: (g) => {
      const method = g.pick(["GET", "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"]);
      const headers = Object.fromEntries(
        g.many(g.range(0, 3), () => [g.pick(HTTP_NAMES.filter((name) => !/cookie|authorization/i.test(name))), g.chance(0.2) ? [g.pick(ASCII_VALUES), g.pick(ASCII_VALUES)] : g.pick(ASCII_VALUES)]),
      );
      const path = g.pick(["/", "/path?x=1&y=2", "/%7Euser", "/?q=%20", "/a;b"]);
      const body = ["POST", "PUT", "PATCH"].includes(method) ? g.pick([[], ["hello"], ["a", "b", "c"], ["é"]]) : [];
      return [
        `const names = ${g.lit(Object.keys(headers))};`,
        httpServerSource(httpResponseProgram(g, false), false),
        `const response = await new Promise((resolve) => {`,
        `  const req = http.request({ host: "127.0.0.1", port, method: ${g.lit(method)}, path: ${g.lit(path)}, headers: ${g.lit(headers)} }, (res) => {`,
        `    const chunks = [];`,
        `    res.on("data", (chunk) => chunks.push(chunk));`,
        `    res.on("end", () => resolve([res.statusCode, Object.entries(res.headers).filter(([name]) => name !== "date").sort(), Buffer.concat(chunks).toString("latin1")]));`,
        `    res.on("error", (e) => resolve(["response error", e.code]));`,
        `  });`,
        `  req.on("error", (e) => resolve(["request error", e.code]));`,
        ...body.slice(0, -1).map((chunk) => `  req.write(${g.lit(chunk)});`),
        `  req.end(${body.length ? g.lit(body[body.length - 1]) : ""});`,
        `});`,
        `server.closeAllConnections();`,
        `server.close();`,
        `for (const [label, ...values] of log) __log(label, ...values);`,
        `__log("client", ...response);`,
      ].join("\n");
    },
    // Raw requests over a `net` socket, and the bytes the server sends back until it closes the
    // connection, with the `Date` header's value masked.
    raw: (g) => {
      const count = g.chance(0.25) ? 2 : 1;
      const requests = Array.from({ length: count }, (_, index) => rawRequest(g, index === count - 1));
      const clientError = g.chance(0.5);
      return [
        httpServerSource(httpResponseProgram(g, true), true),
        clientError
          ? `server.on("clientError", (e, socket) => { log.push(["clientError", e.code, e.bytesParsed], ["clientError socket", socket.writable]); socket.end("HTTP/1.1 400 Custom\\r\\n\\r\\n"); });`
          : ``,
        `const net = require("node:net");`,
        `const bytes = await new Promise((resolve) => {`,
        `  const socket = net.connect(port, "127.0.0.1", () => socket.write(${g.lit(requests.join(""))}));`,
        `  const chunks = [];`,
        `  const timer = setTimeout(() => { chunks.push(Buffer.from("(open)")); socket.destroy(); }, 1000);`,
        `  socket.on("data", (chunk) => chunks.push(chunk));`,
        `  socket.on("error", () => {});`,
        `  socket.on("close", () => { clearTimeout(timer); resolve(Buffer.concat(chunks).toString("latin1").replace(/Date: [^\\r]*\\r\\n/g, "Date: -\\r\\n")); });`,
        `});`,
        `server.close();`,
        `for (const [label, ...values] of log) __log(label, ...values);`,
        `__log("bytes", bytes);`,
      ].join("\n");
    },
  },
};

export const GENERATORS = [
  buffer, stringDecoder, path, querystring, crypto, events, stream, fsGenerator, zlib, util, assert, url, timers, punycode, os,
  processGenerator, shape, stackTrace, testRunner, netGenerator, httpGenerator,
];

/** An operation drawn from a random variant of `generator` with `g`. */
export function makeOp(generator, g) {
  const variants = Object.keys(generator.variants);
  const variant = g.pick(variants);
  return { generator: generator.name, variant, source: generator.variants[variant](g) };
}
