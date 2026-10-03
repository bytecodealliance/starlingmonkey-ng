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
import { eq, throws } from "./harness.mjs";

const text = "hello hello hello world";
const fixtures = {
  deflate: "eJzLSM3JyVfIQCLL84tyUgBofQjF",
  gzip: "H4sIAAAAAAAAE8tIzcnJV8hAIsvzi3JSACbmWoEXAAAA",
  raw: "y0jNyclXyEAiy/OLclIA",
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
eq(failure(() => gunzipSync(Buffer.concat([gzipSync("ab"), Buffer.from("junk")]))), "Error Z_DATA_ERROR -3 incorrect header check", "non-zero bytes after a member start another");
throws(() => deflateSync(5), "ERR_INVALID_ARG_TYPE", "input type");
throws(() => deflate("x"), "ERR_INVALID_ARG_TYPE", "callback functions need a callback");

eq(crc32("hello"), 907060870, "crc32");
eq(crc32("world", crc32("hello ")), crc32("hello world"), "crc32 continues from a value");
eq(crc32(Buffer.from([1, 2, 3])), 1438416925, "crc32 of a buffer");
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
