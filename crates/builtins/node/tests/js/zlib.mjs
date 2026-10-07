// `node:zlib`, with compressed fixtures and error shapes from Node.

import zlib, {
  codes,
  constants,
  crc32,
  createDeflate,
  createGunzip,
  createGzip,
  deflate,
  deflateRawSync,
  deflateSync,
  Gzip,
  gunzip,
  gunzipSync,
  gzipSync,
  inflateRawSync,
  inflateSync,
  unzipSync,
} from "node:zlib";
import { Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { deepEq, eq, throws } from "./harness.mjs";

const text = "hello hello hello world";
const fixtures = {
  deflate: "eJzLSM3JyVfIQCLL84tyUgBofQjF",
  gzip: "H4sIAAAAAAAAE8tIzcnJV8hAIsvzi3JSACbmWoEXAAAA",
  raw: "y0jNyclXyEAiy/OLclIA",
  brotli: "GxYA+I2Ubt5EVYaWbCBvNUg8dUAJiSM=",
  zstd: "KLUv/SAXjQAAWGhlbGxvIHdvcmxkAQDhShE=",
};
const fixture = (name) => Buffer.from(fixtures[name], "base64");

eq(zlib.gzipSync, gzipSync, "default export");
eq(inflateSync(fixture("deflate")).toString(), text, "inflate Node's deflate output");
eq(gunzipSync(fixture("gzip")).toString(), text, "gunzip Node's gzip output");
eq(inflateRawSync(fixture("raw")).toString(), text, "inflate Node's raw deflate output");
eq(unzipSync(fixture("deflate")).toString() + unzipSync(fixture("gzip")).toString(), text + text, "unzip detects the format");
eq(inflateSync(deflateSync(text)).toString(), text, "deflate round trip");
eq(gunzipSync(gzipSync(new TextEncoder().encode(text))).toString(), text, "gzip round trip from a Uint8Array");
eq(inflateRawSync(deflateRawSync(new TextEncoder().encode(text).buffer)).toString(), text, "raw round trip from an ArrayBuffer");
eq(deflateSync(text).subarray(0, 2).toString("hex"), "789c", "zlib header");
eq(gzipSync(text).subarray(0, 2).toString("hex"), "1f8b", "gzip header");
eq(deflateSync(text, { level: 0 }).length, 34, "level 0 stores");
eq(deflateSync("x".repeat(1000), { level: 9 }).length < 20, true, "level 9 compresses");

const large = Buffer.alloc(200_000, "abcdefghij");
eq(inflateSync(deflateSync(large)).equals(large), true, "inputs larger than the output chunk");

const multi = Buffer.concat([gzipSync("ab"), gzipSync("cd")]);
eq(gunzipSync(multi).toString(), "abcd", "gunzip decompresses every member");
eq(unzipSync(multi).toString(), "abcd", "unzip decompresses every gzip member");
eq(gunzipSync(Buffer.concat([gzipSync("ab"), Buffer.alloc(3)])).toString(), "ab", "zero padding after a member is ignored");
eq(inflateSync(Buffer.concat([deflateSync("ab"), Buffer.from("junk")])).toString(), "ab", "bytes after a zlib stream are ignored");

const failure = (fn) => {
  try {
    fn();
  } catch (error) {
    return [error.constructor.name, error.code, error.errno, error.message].join(" ");
  }
};
eq(failure(() => inflateSync(Buffer.from("garbage"))), "Error Z_DATA_ERROR -3 incorrect header check", "bad header");
eq(failure(() => gunzipSync(fixture("gzip").subarray(0, 10))), "Error Z_BUF_ERROR -5 unexpected end of file", "truncated input");
eq(failure(() => inflateRawSync(Buffer.from([0xff, 0xff]))), "Error Z_DATA_ERROR -3 invalid block type", "bad block");
// `deflateSync("abcabcabc", { dictionary: Buffer.from("abc") })`.
const needsDictionary = Buffer.from([120, 187, 2, 77, 1, 39, 75, 132, 33, 0, 17, 61, 3, 115]);
eq(failure(() => inflateSync(needsDictionary)), "Error Z_NEED_DICT 2 Missing dictionary", "data that needs a dictionary");
eq(failure(() => gunzipSync(Buffer.concat([gzipSync("ab"), Buffer.from("junk")]))), "Error Z_DATA_ERROR -3 incorrect header check", "non-zero bytes after a member start another");
throws(() => deflateSync(5), "ERR_INVALID_ARG_TYPE", "input type");
throws(() => deflate("x"), "ERR_INVALID_ARG_TYPE", "callback functions need a callback");

// Brotli and zstd.
eq(zlib.brotliDecompressSync(fixture("brotli")).toString(), text, "decompress Node's Brotli output");
eq(zlib.zstdDecompressSync(fixture("zstd")).toString(), text, "decompress Node's zstd output");
eq(zlib.brotliDecompressSync(zlib.brotliCompressSync(large)).equals(large), true, "Brotli round trip");
eq(zlib.zstdDecompressSync(zlib.zstdCompressSync(large)).equals(large), true, "zstd round trip");
const quality = (q) => zlib.brotliCompressSync(large, { params: { [constants.BROTLI_PARAM_QUALITY]: q } }).length;
eq(quality(9) < quality(0), true, "BROTLI_PARAM_QUALITY selects the quality");
eq(zlib.zstdCompressSync("").subarray(0, 4).toString("hex"), "28b52ffd", "an empty input is one zstd frame");
eq(zlib.zstdDecompressSync(Buffer.concat([fixture("zstd"), fixture("zstd")])).toString(), text, "zstd decompresses the first frame");
eq(zlib.brotliDecompressSync(Buffer.concat([fixture("brotli"), Buffer.from("xx")])).toString(), text, "bytes after a Brotli stream are ignored");
eq(failure(() => zlib.brotliDecompressSync(Buffer.from("garbage data here"))).startsWith("Error ERR__ERROR_FORMAT_"), true, "Brotli errors");
eq(failure(() => zlib.zstdDecompressSync(Buffer.from("garbage data here"))), "Error ZSTD_error_prefix_unknown 10 Unknown frame descriptor", "zstd errors");
eq(failure(() => zlib.brotliDecompressSync(fixture("brotli").subarray(0, 10))), "Error Z_BUF_ERROR -5 unexpected end of file", "truncated Brotli input");
eq(failure(() => zlib.zstdDecompressSync(fixture("zstd").subarray(0, 10))), "Error Z_BUF_ERROR -5 unexpected end of file", "truncated zstd input");
throws(() => zlib.createBrotliCompress({ params: { 99: 1 } }), "ERR_BROTLI_INVALID_PARAM", "Brotli parameter keys");
throws(() => zlib.createBrotliCompress({ params: { 1: "x" } }), "ERR_INVALID_ARG_TYPE", "Brotli parameter values");
throws(() => zlib.createZstdCompress({ params: { 999: 1 } }), "ERR_ZSTD_INVALID_PARAM", "zstd parameter keys");
throws(() => zlib.createZstdCompress({ params: { 99: 1 } }), "ERR_ZLIB_INITIALIZATION_FAILED", "unknown zstd parameters");
throws(() => zlib.createBrotliCompress({ flush: 4 }), "ERR_OUT_OF_RANGE", "Brotli flush range");
{
  const [error, result] = await new Promise((resolve) => zlib.brotliCompress(text, (...args) => resolve(args)));
  eq(error, null, "brotliCompress succeeds");
  eq(zlib.brotliDecompressSync(result).toString(), text, "brotliCompress output");
  const brotli = zlib.createBrotliCompress();
  eq(brotli instanceof zlib.BrotliCompress && brotli instanceof Transform, true, "Brotli streams are Transforms");
  const parts = [];
  brotli.on("data", (chunk) => parts.push(chunk));
  brotli.write("part one ");
  await new Promise((resolve) => brotli.flush(resolve));
  const flushed = zlib.brotliDecompressSync(Buffer.concat(parts), { finishFlush: constants.BROTLI_OPERATION_FLUSH });
  eq(flushed.toString(), "part one ", "a Brotli flush makes the input so far decompressible");
  brotli.end("part two");
  await new Promise((resolve) => brotli.on("end", resolve));
  eq(zlib.brotliDecompressSync(Buffer.concat(parts)).toString(), "part one part two", "Brotli stream output");
  const piped = zlib.createZstdCompress();
  const back = zlib.createZstdDecompress();
  const backChunks = [];
  back.on("data", (chunk) => backChunks.push(chunk));
  const done = new Promise((resolve) => back.on("end", resolve));
  piped.pipe(back);
  piped.write("zstd ");
  piped.end("stream");
  await done;
  eq(Buffer.concat(backChunks).toString(), "zstd stream", "zstd streams");
}

eq(crc32("hello"), 907060870, "crc32");
eq(crc32("world", crc32("hello ")), crc32("hello world"), "crc32 continues from a value");
eq(crc32(Buffer.from([1, 2, 3])), 1438416925, "crc32 of a buffer");
throws(() => crc32(new ArrayBuffer(2)), "ERR_INVALID_ARG_TYPE", "crc32 rejects an ArrayBuffer");
const viewed = Buffer.from("view");
const view = new DataView(viewed.buffer, viewed.byteOffset, viewed.length);
eq(inflateSync(deflateSync(view)).toString(), "view", "conversions of a DataView");
eq(codes.Z_DATA_ERROR, -3, "codes by name");
eq(codes["-3"], "Z_DATA_ERROR", "codes by number");
eq(constants.Z_BEST_COMPRESSION, 9, "constants");

// The callback functions.
{
  let sync = true;
  const [error, result] = await new Promise((resolve) => gunzip(fixture("gzip"), (...args) => resolve([...args, sync])));
  sync = false;
  eq(error, null, "callback without an error");
  eq(result.toString(), text, "callback result");
  const [failed, missing] = await new Promise((resolve) => zlib.inflate(Buffer.from("x"), {}, (...args) => resolve(args)));
  eq(failed.code, "Z_BUF_ERROR", "callback with an error and options");
  eq(missing, undefined, "no result after an error");
}

// The streams.
{
  const gzip = createGzip();
  eq(gzip instanceof Gzip && gzip instanceof Transform, true, "zlib streams are Transforms");
  eq(Object.getPrototypeOf(Gzip.prototype).constructor.name, "Zlib", "the base class");
  eq(gzip.bytesWritten, 0, "bytesWritten starts at 0");
  const chunks = [];
  const gunzipStream = createGunzip();
  gunzipStream.on("data", (chunk) => chunks.push(chunk));
  const ended = new Promise((resolve) => gunzipStream.on("end", resolve));
  gzip.pipe(gunzipStream);
  gzip.write("hello ");
  gzip.write(Buffer.from("stream"));
  gzip.end();
  await ended;
  eq(Buffer.concat(chunks).toString(), "hello stream", "gzip piped into gunzip");
  eq(gzip.bytesWritten, 12, "bytesWritten counts the input");

  const output = [];
  await pipeline(
    async function* () {
      yield fixture("deflate").subarray(0, 5);
      yield fixture("deflate").subarray(5);
    },
    zlib.createInflate(),
    async function* (source) {
      for await (const chunk of source) output.push(chunk);
    },
  );
  eq(Buffer.concat(output).toString(), text, "inflate input split across chunks");

  const broken = zlib.createInflate();
  const error = new Promise((resolve) => broken.on("error", resolve));
  broken.end(Buffer.from("garbage"));
  eq((await error).code, "Z_DATA_ERROR", "stream errors");

  const finished = createDeflate();
  const finishedError = new Promise((resolve) => finished.on("error", resolve));
  finished.write("a");
  finished.flush(zlib.constants.Z_FINISH, () => {
    finished.write("b");
    finished.end();
  });
  const { code, errno, message } = await finishedError;
  eq(`${code} ${errno} ${message}`, "Z_STREAM_ERROR -2 stream error", "a write after Z_FINISH");

  const truncated = createGunzip();
  const truncatedError = new Promise((resolve) => truncated.on("error", resolve));
  truncated.resume();
  truncated.end(fixture("gzip").subarray(0, 10));
  eq((await truncatedError).code, "Z_BUF_ERROR", "a stream that ends early fails");

  const flushing = createDeflate();
  const flushedChunks = [];
  flushing.on("data", (chunk) => flushedChunks.push(chunk));
  flushing.write("hello ");
  await new Promise((resolve) => flushing.flush(resolve));
  const flushed = Buffer.concat(flushedChunks);
  eq(flushed.subarray(-4).toString("hex"), "0000ffff", "flush ends the output with an empty stored block");
  const partial = zlib.createInflate();
  const partialChunks = [];
  partial.on("data", (chunk) => partialChunks.push(chunk));
  await new Promise((resolve) => partial.write(flushed, resolve));
  eq(Buffer.concat(partialChunks).toString(), "hello ", "flushed output decompresses on its own");
  flushing.end("world");
  await new Promise((resolve) => flushing.on("end", resolve));
  eq(inflateSync(Buffer.concat(flushedChunks)).toString(), "hello world", "the stream continues after a flush");

  const levels = createDeflate({ level: 0 });
  const stored = [];
  levels.on("data", (chunk) => stored.push(chunk));
  await new Promise((resolve) => levels.params(9, 0, resolve));
  levels.end("x".repeat(1000));
  await new Promise((resolve) => levels.on("end", resolve));
  eq(Buffer.concat(stored).length < 100, true, "params changes the level");
  deepEq([levels._level, levels._strategy], [9, 0], "params records the level and strategy");
  throws(() => createDeflate().params(99, 0, () => {}), "ERR_OUT_OF_RANGE", "params validates the level");
  throws(() => createDeflate().params(1, 9, () => {}), "ERR_OUT_OF_RANGE", "params validates the strategy");
  throws(() => createDeflate().params("x", 0, () => {}), "ERR_INVALID_ARG_TYPE", "params needs a number");
  throws(() => createDeflate().params(-1, 0), "ERR_INVALID_ARG_TYPE", "params without a change needs a callback");
  const unchanged = createDeflate();
  const order = [];
  unchanged.params(-1, 0, () => order.push("params"));
  queueMicrotask(() => order.push("microtask"));
  await new Promise((resolve) => setImmediate(resolve));
  deepEq(order, ["microtask", "params"], "params without a change calls back in a tick");

  const reused = zlib.createUnzip();
  const reusedChunks = [];
  reused.on("data", (chunk) => reusedChunks.push(chunk));
  await new Promise((resolve) => reused.write(gzipSync("first"), resolve));
  reused.reset();
  await new Promise((resolve) => reused.write(deflateSync("second"), resolve));
  eq(Buffer.concat(reusedChunks).toString(), "firstsecond", "reset makes Unzip detect the format again");

  const closed = createGzip();
  await new Promise((resolve) => closed.close(resolve));
  eq(closed.destroyed, true, "close destroys the stream");
}

// Output limits apply while the output is produced.
{
  const zeros = Buffer.alloc(1 << 20);
  const bombs = {
    gunzipSync: gzipSync(zeros),
    inflateSync: deflateSync(zeros),
    brotliDecompressSync: zlib.brotliCompressSync(zeros),
    zstdDecompressSync: zlib.zstdCompressSync(zeros),
  };
  for (const [name, bomb] of Object.entries(bombs)) {
    const error = throws(() => zlib[name](bomb, { maxOutputLength: 1024 }), "ERR_BUFFER_TOO_LARGE", `${name} with maxOutputLength`);
    eq(error.message, "Cannot create a Buffer larger than 1024 bytes", `${name}'s message`);
    eq(zlib[name](bomb, { maxOutputLength: 1 << 20 }).length, 1 << 20, `${name} at maxOutputLength`);
  }
  const truncated = gzipSync(zeros).subarray(0, 500);
  throws(() => gunzipSync(truncated, { maxOutputLength: 1024 }), "ERR_BUFFER_TOO_LARGE", "the limit is reached before the end");

  for (const [create, bomb] of [
    [createGunzip, bombs.gunzipSync],
    [zlib.createBrotliDecompress, bombs.brotliDecompressSync],
    [zlib.createZstdDecompress, bombs.zstdDecompressSync],
  ]) {
    const sizes = new Set();
    const stream = create({ chunkSize: 4096 });
    stream.on("data", (chunk) => sizes.add(chunk.length));
    await new Promise((resolve) => stream.on("end", resolve).end(bomb));
    eq([...sizes].join(), "4096", `${create.name} pushes chunkSize buffers`);

    const paused = create();
    paused.write(bomb);
    await new Promise((resolve) => setTimeout(resolve, 10));
    eq(paused.readableLength <= 64 * 1024, true, `${create.name} waits while the readable side is full`);
    let total = 0;
    paused.on("data", (chunk) => { total += chunk.length; });
    await new Promise((resolve) => paused.on("end", resolve).end());
    eq(total, 1 << 20, `${create.name} continues once read`);
  }
}

// Argument checks and shapes that follow Node.js.
{
  eq(throws(() => deflateSync("a", { chunkSize: Infinity }), "ERR_OUT_OF_RANGE", "an infinite chunkSize").message.endsWith("Received Infinity"), true, "the received number is JS number text");
  eq(throws(() => deflateSync("a", { level: 2 ** 40 }), "ERR_OUT_OF_RANGE", "a large level").message.endsWith("Received 1_099_511_627_776"), true, "a large received integer has separators");
  throws(() => createDeflate().flush(99, () => {}), "ERR_OUT_OF_RANGE", "flush validates kind");
  throws(() => createDeflate().flush("x", () => {}), "ERR_INVALID_ARG_TYPE", "flush rejects a non-number kind");
  throws(() => zlib.createBrotliCompress().flush(4, () => {}), "ERR_OUT_OF_RANGE", "flush validates a Brotli kind");
  throws(() => deflate("a", { level: 99 }, "notfn"), "ERR_OUT_OF_RANGE", "options are validated before the callback");
  eq(throws(() => deflate(5, () => {}), "ERR_INVALID_ARG_TYPE", "invalid data").message.startsWith('The "chunk" argument'), true, "callback functions name the data chunk");
  eq(Object.isFrozen(codes), true, "codes is frozen");
  const proto = zlib.Deflate.prototype;
  deepEq([proto.flush.length, proto.params.length, proto.close.length], [2, 3, 1], "method lengths");
  const stream = createDeflate();
  eq(stream._closed, false, "_closed before destroy");
  stream.destroy();
  eq(stream._closed, true, "_closed after destroy");
  const flushed = createDeflate();
  flushed.on("data", () => {});
  flushed.write("a");
  await new Promise((resolve) => flushed.flush(constants.Z_FINISH, resolve));
  eq(flushed._outOffset, 9, "_outOffset counts the bytes pushed of the output buffer");
}
