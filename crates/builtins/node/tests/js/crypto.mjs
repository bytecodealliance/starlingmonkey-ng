// `node:crypto`'s hashes, HMACs, secret keys, key derivation functions and random values, with
// digests computed by Node.

import crypto, {
  KeyObject,
  createHash,
  createHmac,
  createSecretKey,
  hkdf,
  hkdfSync,
  pbkdf2,
  pbkdf2Sync,
  scrypt,
  scryptSync,
  getHashes,
  getRandomValues,
  hash,
  randomBytes,
  randomFillSync,
  randomInt,
  randomUUID,
  timingSafeEqual,
  webcrypto,
} from "node:crypto";
import { eq, deepEq, throws } from "./harness.mjs";

const expected = {
  md5: "900150983cd24fb0d6963f7d28e17f72",
  sha1: "a9993e364706816aba3e25717850c26c9cd0d89d",
  sha224: "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7",
  sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  sha384: "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
  sha512: "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
  "sha512-224": "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa",
  "sha512-256": "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23",
};
for (const [algorithm, digest] of Object.entries(expected)) {
  eq(createHash(algorithm).update("abc").digest("hex"), digest, `${algorithm} digest`);
}
eq(crypto.createHash, createHash, "default export");
eq(createHash("SHA-256").update("a").update("bc").digest("hex"), expected.sha256, "incremental updates and algorithm aliases");
eq(createHash("sha256").update("").digest("base64url"), "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU", "base64url output");
eq(createHash("sha1").update("é", "latin1").digest("hex"), "1599e9fa41ec68c80230491902786bee889f5bcb", "input encodings");
eq(Buffer.isBuffer(createHash("md5").update(Buffer.from("abc")).digest()), true, "digest() returns a Buffer");
const partial = createHash("sha256").update("a");
const copy = partial.copy();
eq(partial.update("bc").digest("hex"), expected.sha256, "copy keeps the original going");
eq(copy.update("bc").digest("hex"), expected.sha256, "copy continues from the same state");
const finished = createHash("sha1");
finished.digest();
throws(() => finished.update("x"), "ERR_CRYPTO_HASH_FINALIZED", "update after digest");
throws(() => finished.digest(), "ERR_CRYPTO_HASH_FINALIZED", "digest twice");
throws(() => createHash("nope"), Error, "unsupported digests");
eq(hash("sha1", "abc"), expected.sha1, "one-shot hash");
eq(Buffer.isBuffer(hash("sha1", "abc", "buffer")), true, "one-shot hash as a buffer");
eq(getHashes().includes("sha256"), true, "getHashes");

eq(
  createHmac("sha256", "key").update("The quick brown fox jumps over the lazy dog").digest("hex"),
  "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8",
  "HMAC-SHA256",
);
eq(createHmac("sha1", "k".repeat(100)).update("data").digest("base64"), "49w5GgPBJmP9MxSLlZ2kfQxbjGY=", "keys longer than the block are hashed");
eq(
  createHmac("sha512", Buffer.from([1, 2, 3])).update("x").digest("hex"),
  "f9f018933e4071b9007c8488cbdb556eaa23d87acbe941037206a7bdf8d5a54f700fdfe45df813eed05fbe1e851da67c4a6f82aa17ed9d60a374fd8fa4b55eb9",
  "HMAC-SHA512 with a buffer key",
);

eq(randomBytes(16).length, 16, "randomBytes");
eq(randomBytes(32).equals(randomBytes(32)), false, "randomBytes differ");
throws(() => randomBytes(-1), "ERR_OUT_OF_RANGE", "randomBytes validates the size");
const viaCallback = await new Promise((resolve) => randomBytes(8, (error, bytes) => resolve([error, bytes.length])));
deepEq(viaCallback, [null, 8], "randomBytes with a callback");
const filled = randomFillSync(Buffer.alloc(8), 4);
eq(filled.subarray(0, 4).every((byte) => byte === 0), true, "randomFillSync leaves bytes before the offset alone");
eq(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(randomUUID()), true, "randomUUID is a version 4 UUID");
for (let index = 0; index < 100; index++) {
  const value = randomInt(3, 7);
  if (value < 3 || value >= 7 || !Number.isInteger(value)) throw new Error(`randomInt out of range: ${value}`);
}
throws(() => randomInt(10, 5), "ERR_OUT_OF_RANGE", "randomInt needs max > min");
eq(timingSafeEqual(Buffer.from("abc"), Buffer.from("abc")), true, "timingSafeEqual");
eq(timingSafeEqual(Buffer.from("abc"), Buffer.from("abd")), false, "timingSafeEqual detects differences");
throws(() => timingSafeEqual(Buffer.from("a"), Buffer.from("ab")), "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH", "timingSafeEqual lengths");
eq(timingSafeEqual(new ArrayBuffer(2), new DataView(new ArrayBuffer(2))), true, "timingSafeEqual of other buffer sources");
throws(() => timingSafeEqual(Buffer.from("a"), "a"), "ERR_INVALID_ARG_TYPE", "timingSafeEqual checks buf2");
eq(
  (() => { try { timingSafeEqual(1, Buffer.from("a")); } catch (error) { return error.message; } })(),
  'The "buf1" argument must be an instance of ArrayBuffer, Buffer, TypedArray, or DataView.',
  "timingSafeEqual's argument error has Node.js's C++ message",
);
eq(getRandomValues(new Uint32Array(4)).length, 4, "getRandomValues");
eq(webcrypto, globalThis.crypto, "webcrypto is the global crypto");

const secret = createSecretKey("abc");
eq(secret instanceof KeyObject, true, "createSecretKey returns a KeyObject");
eq(secret.type, "secret", "secret key type");
eq(secret.symmetricKeySize, 3, "symmetricKeySize");
eq(Object.prototype.toString.call(secret), "[object KeyObject]", "KeyObject toStringTag");
eq(secret.export().toString("hex"), "616263", "export as a Buffer");
deepEq(secret.export({ format: "jwk" }), { kty: "oct", k: "YWJj" }, "export as a JSON Web Key");
throws(() => secret.export({ format: "pem" }), "ERR_INVALID_ARG_VALUE", "export validates the format");
eq(secret.equals(createSecretKey("616263", "hex")), true, "equals compares the material");
eq(secret.equals(createSecretKey("abd")), false, "equals detects different material");
throws(() => secret.equals("abc"), "ERR_INVALID_ARG_TYPE", "equals needs a KeyObject");
throws(() => new KeyObject(), "ERR_INVALID_ARG_VALUE", "KeyObject validates the type");
throws(() => new KeyObject("secret", {}), "ERR_INVALID_ARG_TYPE", "KeyObject validates the handle");
throws(() => createSecretKey(1), "ERR_INVALID_ARG_TYPE", "createSecretKey validates the key");
throws(() => createSecretKey("ab", "bogus"), "ERR_UNKNOWN_ENCODING", "createSecretKey validates the encoding");
eq(createSecretKey(new ArrayBuffer(5)).symmetricKeySize, 5, "createSecretKey takes an ArrayBuffer");
eq(
  createHmac("sha256", secret).update("x").digest("hex"),
  createHmac("sha256", "abc").update("x").digest("hex"),
  "createHmac takes a secret KeyObject",
);
eq(
  createHmac("sha256", "ff", { encoding: "hex" }).update("x").digest("hex"),
  createHmac("sha256", Buffer.from([255])).update("x").digest("hex"),
  "createHmac decodes a string key with options.encoding",
);
throws(() => createHmac("sha256", 1), "ERR_INVALID_ARG_TYPE", "createHmac validates the key");

eq(pbkdf2Sync("pw", "salt", 1, 20, "sha1").toString("hex"), "95c9338ea3a049dc400b5ed6617b5c45bdda0c14", "PBKDF2-SHA1");
eq(
  pbkdf2Sync("pw", "salt", 1000, 64, "sha512").toString("hex"),
  "dba2956d5d8f05e6f06dc9732ca6037d32a4fcbcdde4b35e62594b632d059275a4612b69ad28703b5340833cb47d99636d088ef65309684ac39b5c45372b025e",
  "PBKDF2-SHA512 with more than one block",
);
throws(() => pbkdf2Sync("pw", "salt", 1, 20, "nope"), "ERR_CRYPTO_INVALID_DIGEST", "pbkdf2Sync validates the digest");
throws(() => pbkdf2Sync("pw", "salt", 0, 20, "sha1"), "ERR_OUT_OF_RANGE", "pbkdf2Sync validates the iterations");
throws(() => pbkdf2Sync("pw", "salt", 1, 20), "ERR_INVALID_ARG_TYPE", "pbkdf2Sync needs a digest");
throws(() => pbkdf2("pw", "salt", 1, 20, "sha1"), "ERR_INVALID_ARG_TYPE", "pbkdf2 needs a callback");
const pbkdf2Result = await new Promise((resolve) => pbkdf2("pw", "salt", 1, 20, "sha1", (error, key) => resolve([error, key.toString("hex")])));
deepEq(pbkdf2Result, [null, "95c9338ea3a049dc400b5ed6617b5c45bdda0c14"], "pbkdf2 with a callback");

eq(scryptSync("pw", "salt", 16).toString("hex"), "c0b515908e61334cae6d6003c00be60e", "scrypt with the default parameters");
eq(scryptSync("pw", "salt", 16, { N: 1024, r: 8, p: 1 }).toString("hex"), "9f1f8695838e682c1689750f45a69fb9", "scrypt with N, r and p");
eq(
  scryptSync("", "", 64, { cost: 16, blockSize: 1, parallelization: 1 }).toString("hex"),
  "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906",
  "scrypt test vector of RFC 7914",
);
eq(scryptSync("pw", "salt", 0).length, 0, "scrypt with an empty key");
throws(() => scryptSync("pw", "salt", 16, { N: 3 }), "ERR_CRYPTO_INVALID_SCRYPT_PARAMS", "scrypt needs a power of two");
throws(() => scryptSync("pw", "salt", 16, { N: 2 ** 20 }), "ERR_CRYPTO_INVALID_SCRYPT_PARAMS", "scrypt checks maxmem");
throws(() => scryptSync("pw", "salt", 16, { N: 16, cost: 16 }), "ERR_INCOMPATIBLE_OPTION_PAIR", "scrypt option pairs");
const memoryLimit = "Invalid scrypt params: error:030000AC:digital envelope routines::memory limit exceeded";
const scryptMessage = (options) => {
  try {
    scryptSync("pw", "salt", 16, options);
    return "derived";
  } catch (error) {
    return error.message;
  }
};
eq(scryptMessage({ N: 2, r: 8, p: 2 ** 27 - 1, maxmem: 2 ** 50 }), memoryLimit, "scrypt's blocks exceed 2^31 - 1 bytes");
eq(scryptMessage({ N: 2 ** 16, r: 1, maxmem: 2 ** 40 }), memoryLimit, "scrypt's cost must be below 2^(16 * r)");
eq(scryptMessage({ N: 16, r: 8, maxmem: 19455 }), memoryLimit, "scrypt's memory one byte above maxmem");
eq(scryptMessage({ N: 16, r: 8, maxmem: 19456 }), "derived", "scrypt's memory at maxmem");
eq(
  scryptMessage({ N: 2 ** 31, r: 2 ** 14, maxmem: 2 ** 53 - 1 }),
  "error:078C0100:common libcrypto routines::malloc failure",
  "scrypt memory that can't be allocated",
);
const scryptResult = await new Promise((resolve) => scrypt("pw", "salt", 16, { N: 16 }, (error, key) => resolve([error, key.toString("hex")])));
deepEq(scryptResult, [null, "c5d06c368e4a07b8a98e9a06773fcf4a"], "scrypt with a callback");

eq(crypto.pseudoRandomBytes, randomBytes, "pseudoRandomBytes is randomBytes");
eq(crypto.prng, randomBytes, "prng is randomBytes");
eq(Object.keys(crypto).includes("rng"), false, "the randomBytes aliases aren't enumerable");

const hkdfKey = hkdfSync("sha256", "key", "salt", "info", 16);
eq(hkdfKey instanceof ArrayBuffer, true, "hkdfSync returns an ArrayBuffer");
eq(Buffer.from(hkdfKey).toString("hex"), "9ca0d662557439e3b83365f2da4626d3", "HKDF-SHA256");
eq(Buffer.from(hkdfSync("sha256", createSecretKey(Buffer.from("key")), "", "", 8)).toString("hex"), "80c724c4a65d7956", "HKDF with a KeyObject and no salt");
throws(() => hkdfSync("sha256", "key", "salt", "info", 255 * 32 + 1), "ERR_CRYPTO_INVALID_KEYLEN", "hkdfSync limits the length");
throws(() => hkdfSync("sha256", "key", "salt", Buffer.alloc(1025), 16), "ERR_OUT_OF_RANGE", "hkdfSync limits the info");
const hkdfResult = await new Promise((resolve) => hkdf("sha512", "key", "salt", "info", 16, (error, key) => resolve([error, Buffer.from(key).toString("hex")])));
deepEq(hkdfResult, [null, "24156e2c35525baaf3d0fbb92b734c80"], "hkdf with a callback");

// `getHashes` sorts its names in byte order.
const hashes = getHashes();
deepEq(hashes, [...hashes].sort(), "getHashes is sorted");

// `KeyObject.export` rejects array options.
throws(() => createSecretKey(Buffer.from("ab")).export([]), "ERR_INVALID_ARG_TYPE", "export with array options");

// Out-of-range numbers above 2 ** 32 are shown with `_` separators.
eq(throws(() => crypto.randomFillSync(Buffer.alloc(4), 2 ** 40), "ERR_OUT_OF_RANGE").message.endsWith("Received 1_099_511_627_776"), true, "randomFillSync offset");
eq(throws(() => crypto.randomInt(0, 2 ** 48), "ERR_OUT_OF_RANGE").message.endsWith("Received 281_474_976_710_656"), true, "randomInt range");

// The callback forms validate the callback before deriving.
throws(() => pbkdf2("p", "s", 1e9, 64, "sha512", "notfn"), "ERR_INVALID_ARG_TYPE", "pbkdf2 checks the callback first");

// `outputLength` must be a uint32, and the digest's own length for a digest without extendable output.
throws(() => createHash("sha256", { outputLength: -1 }), "ERR_OUT_OF_RANGE", "a negative outputLength");
throws(() => createHash("sha256", { outputLength: 4 }), "ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH", "outputLength of a fixed-length digest");
eq(createHash("sha256", { outputLength: 32 }).update("a").digest("hex").length, 64, "outputLength equal to the digest's length");
throws(() => createHash("sha256").copy({ outputLength: 4 }), "ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH", "copy checks outputLength");

// `randomFill` checks `buf` before the callback, and both fill a `SharedArrayBuffer`.
eq(throws(() => crypto.randomFill(5), "ERR_INVALID_ARG_TYPE").message.startsWith('The "buf" argument'), true, "randomFill checks buf first");
eq(crypto.randomFillSync(new SharedArrayBuffer(4)).byteLength, 4, "randomFillSync of a SharedArrayBuffer");
