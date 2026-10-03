// `node:crypto`'s hashes, HMACs and random values, with digests computed by Node.

import crypto, {
  createHash,
  createHmac,
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
eq(getRandomValues(new Uint32Array(4)).length, 4, "getRandomValues");
eq(webcrypto, globalThis.crypto, "webcrypto is the global crypto");
