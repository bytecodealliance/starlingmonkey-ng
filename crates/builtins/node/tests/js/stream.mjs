// `node:stream`, with cases adapted from Node's `test/parallel/test-stream-*.js`.

import stream, { Readable, Writable, Duplex, Transform, PassThrough, pipeline, finished } from "node:stream";
import { pipeline as pipelinePromise, finished as finishedPromise } from "node:stream/promises";
import { EventEmitter } from "node:events";
import { eq, deepEq, throws } from "./harness.mjs";

const tick = () => new Promise((resolve) => setTimeout(resolve, 1));
const collect = (readable) =>
  new Promise((resolve, reject) => {
    const chunks = [];
    readable.on("data", (chunk) => chunks.push(chunk));
    readable.on("end", () => resolve(chunks));
    readable.on("error", reject);
  });

eq(stream.Readable, Readable, "the default export is Stream, with the classes as properties");
eq(stream.Stream, stream, "Stream.Stream");
eq(new Readable() instanceof EventEmitter, true, "streams are EventEmitters");
eq(new Duplex() instanceof Readable, true, "Duplex extends Readable");
eq(new Duplex() instanceof Writable, true, "a Duplex is an instance of Writable");
eq(new Transform() instanceof Duplex, true, "Transform extends Duplex");
eq(new PassThrough() instanceof Transform, true, "PassThrough extends Transform");
eq({} instanceof Writable, false, "plain objects aren't Writables");

// Reading: push, read, end, close.
{
  const readable = new Readable({ read() {} });
  const events = [];
  readable.on("end", () => events.push("end"));
  readable.on("close", () => events.push("close"));
  readable.push("ab");
  readable.push("cd");
  readable.push(null);
  eq(readable.readableLength, 4, "pushed bytes are buffered");
  eq(readable.read(1).toString(), "a", "read(n) returns n bytes");
  eq(readable.read().toString(), "bcd", "read() returns the rest");
  eq(events.length, 0, "end waits for a tick");
  await tick();
  deepEq(events, ["end", "close"], "end, then close with autoDestroy");
  eq(readable.destroyed, true, "autoDestroy destroys after end");
  eq(readable.readableEnded, true, "readableEnded");
}

// Flowing mode, setEncoding, unshift.
{
  const readable = new Readable({ read() {} });
  readable.setEncoding("utf8");
  readable.push(Buffer.from([0xe2, 0x82]));
  readable.push(Buffer.from([0xac]));
  readable.push(null);
  deepEq(await collect(readable), ["€"], "setEncoding decodes characters split across chunks");
  const unshifted = new Readable({ read() {} });
  unshifted.push("world");
  unshifted.unshift("hello ");
  unshifted.push(null);
  eq((await collect(unshifted)).join(""), "hello world", "unshift puts data in front");
}

// Object mode with a high water mark of 1.
{
  let reads = 0;
  const objects = new Readable({
    objectMode: true,
    highWaterMark: 1,
    read() {
      reads++;
      this.push(reads <= 3 ? { n: reads } : null);
    },
  });
  deepEq(await collect(objects), [{ n: 1 }, { n: 2 }, { n: 3 }], "object mode reads one object at a time");
  throws(() => new Readable({ highWaterMark: -1 }), "ERR_INVALID_ARG_VALUE", "high water marks are validated");
  const pushAfterEnd = new Readable({ read() {} });
  const error = await new Promise((resolve) => {
    pushAfterEnd.on("error", resolve);
    pushAfterEnd.push(null);
    pushAfterEnd.push("late");
  });
  eq(error.code, "ERR_STREAM_PUSH_AFTER_EOF", "push after EOF");
}

// Writing: backpressure and drain.
{
  const written = [];
  const callbacks = [];
  const writable = new Writable({
    highWaterMark: 3,
    write(chunk, encoding, callback) {
      written.push(chunk.toString());
      setTimeout(callback, 1);
    },
  });
  let drains = 0;
  writable.on("drain", () => drains++);
  eq(writable.write("ab", () => callbacks.push(1)), true, "below the high water mark");
  eq(writable.write("cd", () => callbacks.push(2)), false, "write() returns false past the high water mark");
  eq(writable.writableNeedDrain, true, "writableNeedDrain");
  eq(writable.writableLength, 4, "writableLength");
  await new Promise((resolve) => writable.once("drain", resolve));
  eq(drains, 1, "drain fires once");
  deepEq(callbacks, [1, 2], "write callbacks run in order");
  const finishedEvents = [];
  writable.on("finish", () => finishedEvents.push("finish"));
  writable.on("close", () => finishedEvents.push("close"));
  writable.end("ef");
  await new Promise((resolve) => writable.on("close", resolve));
  deepEq(written, ["ab", "cd", "ef"], "every chunk is written");
  deepEq(finishedEvents, ["finish", "close"], "finish, then close");
  const writeAfterEnd = await new Promise((resolve) => writable.write("x", resolve));
  eq(writeAfterEnd.code, "ERR_STREAM_WRITE_AFTER_END", "write after end");
}

// cork, writev, final, decodeStrings.
{
  const batches = [];
  let finalCalled = false;
  const writable = new Writable({
    decodeStrings: false,
    writev(chunks, callback) {
      batches.push(chunks.map(({ chunk }) => chunk));
      callback();
    },
    final(callback) {
      finalCalled = true;
      callback();
    },
  });
  writable.cork();
  writable.write("a");
  writable.write("b");
  writable.uncork();
  writable.end();
  await new Promise((resolve) => writable.on("finish", resolve));
  deepEq(batches, [["a", "b"]], "corked writes go to writev together");
  eq(finalCalled, true, "final runs before finish");
}

// pipe with a slow writable pauses and resumes the readable.
{
  let pushed = 0;
  const source = new Readable({
    highWaterMark: 2,
    read() {
      pushed++;
      this.push(pushed <= 20 ? "x" : null);
    },
  });
  const received = [];
  let pauses = 0;
  source.on("pause", () => pauses++);
  const slow = new Writable({
    highWaterMark: 2,
    write(chunk, encoding, callback) {
      received.push(chunk.toString());
      setTimeout(callback, 1);
    },
  });
  source.pipe(slow);
  await new Promise((resolve) => slow.on("finish", resolve));
  eq(received.join(""), "x".repeat(20), "pipe delivers every chunk");
  eq(pauses > 0, true, "pipe paused the readable on backpressure");
}

// Transform, PassThrough, flush.
{
  const upper = new Transform({
    transform(chunk, encoding, callback) {
      callback(null, chunk.toString().toUpperCase());
    },
    flush(callback) {
      callback(null, "!");
    },
  });
  upper.end("hello");
  eq((await collect(upper)).join(""), "HELLO!", "transform and flush");
  const passthrough = new PassThrough({ objectMode: true });
  passthrough.write({ a: 1 });
  passthrough.end({ b: 2 });
  deepEq(await collect(passthrough), [{ a: 1 }, { b: 2 }], "PassThrough");
}

// Duplex with allowHalfOpen false.
{
  const duplex = new Duplex({
    allowHalfOpen: false,
    read() {},
    write(chunk, encoding, callback) {
      callback();
    },
  });
  duplex.resume();
  duplex.push(null);
  await new Promise((resolve) => duplex.on("finish", resolve));
  eq(duplex.writableEnded, true, "ending the readable side ends the writable side");
}

// destroy, errors, finished.
{
  const readable = new Readable({ read() {} });
  const events = [];
  readable.on("error", (error) => events.push(`error:${error.message}`));
  readable.on("close", () => events.push("close"));
  readable.destroy(new Error("boom"));
  eq(readable.destroyed, true, "destroyed");
  await tick();
  deepEq(events, ["error:boom", "close"], "destroy(err) emits error, then close");
  eq(readable.errored.message, "boom", "errored");

  const done = new Readable({ read() {} });
  const result = new Promise((resolve) => finished(done, resolve));
  done.resume();
  done.push(null);
  eq(await result, undefined, "finished calls back once the stream ended");

  const premature = new Readable({ read() {} });
  const prematureResult = new Promise((resolve) => finished(premature, resolve));
  premature.destroy();
  eq((await prematureResult).code, "ERR_STREAM_PREMATURE_CLOSE", "a destroyed stream finishes prematurely");

  const promised = new PassThrough();
  setTimeout(() => promised.end("x"), 1);
  promised.resume();
  eq(await finishedPromise(promised), undefined, "stream/promises finished");
}

// Async iteration, also over a stream that errors midway.
{
  const chunks = [];
  for await (const chunk of Readable.from(["a", "b", "c"])) chunks.push(chunk);
  deepEq(chunks, ["a", "b", "c"], "for await over a readable");

  const failing = new Readable({
    objectMode: true,
    read() {},
  });
  failing.push(1);
  setTimeout(() => failing.destroy(new Error("midway")), 1);
  const seen = [];
  let caught;
  try {
    for await (const value of failing) seen.push(value);
  } catch (error) {
    caught = error;
  }
  deepEq(seen, [1], "values before the error");
  eq(caught?.message, "midway", "for await rethrows the stream's error");

  const early = Readable.from([1, 2, 3]);
  for await (const value of early) {
    if (value === 1) break;
  }
  await tick();
  eq(early.destroyed, true, "breaking out of for await destroys the stream");
}

// Readable.from with async iterables and promise values.
{
  async function* generate() {
    yield "x";
    await tick();
    yield "y";
  }
  deepEq(await collect(Readable.from(generate())), ["x", "y"], "Readable.from an async generator");
  deepEq(await collect(Readable.from([Promise.resolve(1), 2])), [1, 2], "Readable.from awaits promise values");
  deepEq(await collect(Readable.from("whole")), ["whole"], "Readable.from a string is one chunk");
}

// pipeline.
{
  const output = [];
  const sink = new Writable({
    objectMode: true,
    write(chunk, encoding, callback) {
      output.push(chunk);
      callback();
    },
  });
  const error = await new Promise((resolve) =>
    pipeline(
      Readable.from([1, 2, 3]),
      new Transform({
        objectMode: true,
        transform(chunk, encoding, callback) {
          callback(null, chunk * 10);
        },
      }),
      sink,
      resolve,
    ),
  );
  eq(error, undefined, "pipeline succeeds");
  deepEq(output, [10, 20, 30], "pipeline moves every chunk");

  const source = new Readable({ read() {} });
  const destination = new Writable({ write(chunk, encoding, callback) { callback(); } });
  let calls = 0;
  const failing = new Promise((resolve) =>
    pipeline(
      source,
      new Transform({
        transform(chunk, encoding, callback) {
          callback(new Error("transform failed"));
        },
      }),
      destination,
      (error) => {
        calls++;
        resolve(error);
      },
    ),
  );
  source.push("x");
  const failure = await failing;
  await tick();
  eq(failure.message, "transform failed", "pipeline reports the failure");
  eq(calls, 1, "the pipeline callback runs once");
  eq(source.destroyed && destination.destroyed, true, "every stream is destroyed after a failure");

  const generated = [];
  await pipelinePromise(
    Readable.from(["a", "b"]),
    async function* (source) {
      for await (const chunk of source) yield chunk.toUpperCase();
    },
    new Writable({
      objectMode: true,
      write(chunk, encoding, callback) {
        generated.push(chunk);
        callback();
      },
    }),
  );
  deepEq(generated, ["A", "B"], "stream/promises pipeline with a generator stage");
  throws(() => pipeline(new PassThrough(), () => {}), "ERR_MISSING_ARGS", "pipeline needs two streams");
}

// Legacy Stream pipe and helpers.
{
  eq(stream.isReadable(Readable.from([])), true, "isReadable");
  eq(stream.isErrored(new Readable()), false, "isErrored");
  eq(stream.getDefaultHighWaterMark(false), 65536, "getDefaultHighWaterMark");
  eq(stream.getDefaultHighWaterMark(true), 16, "object mode default");
  const legacy = new stream.Stream();
  const target = new PassThrough();
  legacy.pipe(target);
  legacy.emit("data", "legacy");
  legacy.emit("end");
  eq((await collect(target)).join(""), "legacy", "legacy Stream#pipe");
  const controller = new AbortController();
  const aborted = stream.addAbortSignal(controller.signal, new Readable({ read() {} }));
  const abortError = new Promise((resolve) => aborted.on("error", resolve));
  controller.abort();
  eq((await abortError).name, "AbortError", "addAbortSignal destroys with an AbortError");
}

// Review regressions: argument handling and catchable errors.
{
  const parsed = new Readable({ read() {} });
  parsed.push("abcdef");
  eq(String(parsed.read(1.5)), "a", "read(n) parses a non-integer n with parseInt");
  eq(String(parsed.read("2x")), "bc", "read(n) parses a string n with parseInt");
  throws(() => parsed.read(Symbol()), TypeError, "read(n) propagates a ToString failure");
  const big = new Readable({ read() {} });
  let rangeError;
  try {
    big.read(5e9);
  } catch (error) {
    rangeError = error;
  }
  eq(rangeError.message.endsWith("Received 5_000_000_000"), true, "large sizes get numeric separators");

  const decoded = new Readable({ read() {} });
  decoded.setEncoding("utf8");
  decoded.unshift("%zz");
  decoded.push(null);
  eq(decoded.read(2), "%z", "read(n) slices a decoded string");
  eq(decoded.read(100), "z", "the rest of a sliced string keeps its length");

  const ended = new Readable({ read() {} });
  ended.push(Buffer.from([0xe2, 0x82]));
  ended.push(null);
  ended.setEncoding("utf8");
  eq(ended.read(), "�", "setEncoding flushes the decoder of an ended stream");

  throws(() => stream.setDefaultHighWaterMark(false, "x"), "ERR_INVALID_ARG_TYPE", "setDefaultHighWaterMark validates the type");
  throws(() => stream.setDefaultHighWaterMark(false, 1.5), "ERR_OUT_OF_RANGE", "setDefaultHighWaterMark validates integers");
  let hwmError;
  try {
    new Duplex({ highWaterMark: 1.5 });
  } catch (error) {
    hwmError = error;
  }
  eq(hwmError.message.includes("options.readableHighWaterMark"), true, "a duplex names its readable high water mark");

  throws(() => new Writable({ decodeStrings: false, write(c, e, cb) { cb(); } }).write("a", "buffer"), "ERR_UNKNOWN_ENCODING", "a string chunk can't have the encoding 'buffer'");
  eq(Writable[Symbol.hasInstance].call(1, {}), false, "Symbol.hasInstance with a non-callable receiver");
  eq(Object.getOwnPropertyDescriptor(Writable, Symbol.hasInstance).writable, false, "Writable[Symbol.hasInstance] is read-only");
  throws(() => new stream.Stream().pipe(5), TypeError, "legacy pipe to a primitive throws a TypeError");
  throws(() => finished(new Readable()), "ERR_INVALID_ARG_TYPE", "finished needs a callback");
  throws(() => stream.addAbortSignal(new AbortController().signal), "ERR_INVALID_ARG_TYPE", "addAbortSignal needs a stream");
  eq(stream.destroy(), undefined, "destroy without a stream does nothing");
  const unbuffered = new Readable({ read() {} });
  unbuffered._readableState.buffer = null;
  unbuffered._readableState.length = 1;
  throws(() => unbuffered.read(), TypeError, "a broken buffer throws a catchable TypeError");
  const corked = new Writable({ write(c, e, cb) { cb(); } });
  corked.cork();
  corked.write("a");
  corked._writableState.bufferedIndex = 5;
  throws(() => corked.uncork(), TypeError, "a broken buffered index throws a catchable TypeError");
  eq(Duplex.from === Readable.from, false, "Duplex.from isn't Readable.from");
  eq(Object.keys(new PassThrough()).includes("_transformCallback"), false, "the transform callback isn't an enumerable key");
  eq(typeof Writable.prototype[Symbol.asyncDispose], "function", "Writable[Symbol.asyncDispose]");
  const disposed = new Readable({ read() {} });
  await disposed[Symbol.asyncDispose]();
  eq(disposed.destroyed, true, "asyncDispose destroys the stream");
}

// Review regressions: construction, iteration and Readable.from.
{
  const duplex = new Duplex({ signal: AbortSignal.abort(), read() {}, write(c, e, cb) { cb(); } });
  const events = [];
  duplex.on("error", (error) => events.push(error.name));
  duplex.on("close", () => events.push("close"));
  await tick();
  deepEq(events, ["AbortError", "close"], "a duplex with an aborted signal is destroyed");

  const lazy = new Readable({ read() {} });
  const iterator = lazy[Symbol.asyncIterator]();
  const data = [];
  lazy.on("data", (chunk) => data.push(String(chunk)));
  lazy.push("q");
  await tick();
  deepEq(data, ["q"], "creating an iterator doesn't pause the stream");
  const untouched = new Readable({ read() {} });
  deepEq(await untouched[Symbol.asyncIterator]().return(5), { value: 5, done: true }, "return() before next()");
  eq(untouched.destroyed, false, "return() before next() leaves the stream alone");
  eq(untouched.listenerCount("readable"), 0, "return() before next() adds no listener");
  void iterator;

  const destroyed = [];
  const custom = Readable.from(["a"], {
    destroy(error, callback) {
      destroyed.push("custom");
      callback(error);
    },
  });
  custom.destroy();
  eq(destroyed.length, 1, "Readable.from keeps the destroy option");

  const nullResult = Readable.from({ [Symbol.iterator]() { return { next() { return null; } }; } });
  const nullError = await new Promise((resolve) => nullResult.on("error", resolve).resume());
  eq(nullError instanceof TypeError, true, "a null iterator result is a TypeError");
}

// Review regressions: finished and pipeline.
{
  const controller = new AbortController();
  const calls = [];
  const cleanup = finished(new Readable({ read() {} }), { signal: controller.signal }, (error) => calls.push(error));
  cleanup();
  controller.abort();
  await tick();
  eq(calls.length, 0, "an abort after the cleanup calls nothing");

  const aborted = new AbortController();
  aborted.abort();
  const writable = new Writable({ autoDestroy: false, write(c, e, cb) { cb(); } });
  writable.end();
  await new Promise((resolve) => writable.on("finish", resolve));
  const results = [];
  finished(writable, { signal: aborted.signal }, (error) => results.push(error));
  await tick();
  deepEq(results, [undefined], "a finished stream reports once despite an aborted signal");
  throws(() => finishedPromise(new Readable(), { cleanup: "yes" }), "ERR_INVALID_ARG_TYPE", "finished validates cleanup");

  const last = new Duplex({ read() {}, write(c, e, cb) { cb(); } });
  const listeners = await new Promise((resolve) =>
    pipeline(Readable.from(["a"]), last, () => resolve(last.listenerCount("error"))),
  );
  eq(listeners, 0, "pipeline removes its error listener from a readable last stream");

  let source;
  await new Promise((resolve) =>
    pipeline(
      [1, 2, 3],
      (src) => {
        source = src;
        return (async function* () {
          for await (const value of src) yield value;
        })();
      },
      new Writable({ objectMode: true, write(c, e, cb) { cb(); } }),
      resolve,
    ),
  );
  eq(Array.isArray(source), true, "a function stage gets an iterable source as is");
  throws(
    () => pipeline(Readable.from(["a"]), (src) => { src.resume(); return ["sync"]; }, () => {}),
    "ERR_INVALID_RETURN_VALUE",
    "a destination must return an async iterable",
  );
  let returned;
  try {
    pipeline(Readable.from(["a"]), () => 5, () => {});
  } catch (error) {
    returned = error;
  }
  eq(returned.message.endsWith("but got type number (5)."), true, "the invalid return value is described");
}
