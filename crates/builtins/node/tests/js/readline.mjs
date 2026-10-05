// `node:readline` and `node:readline/promises` over `PassThrough` streams.

import readline from "node:readline";
import readlinePromises from "node:readline/promises";
import { PassThrough } from "node:stream";
import { eq, deepEq, throws } from "./harness.mjs";

const once = (emitter, event) => new Promise((resolve) => emitter.once(event, resolve));

// Lines end at \n, \r\n and \r, also across chunks, and Buffers decode as UTF-8 across chunks.
{
  const input = new PassThrough();
  const rl = readline.createInterface({ input });
  eq(rl instanceof readline.Interface, true, "createInterface");
  const lines = [];
  rl.on("line", (line) => lines.push(line));
  input.write("one\r");
  input.write("\ntwo\nth");
  input.write(Buffer.from([0xc3]));
  input.write(Buffer.from([0xa9, 0x0a]));
  input.write("four\rlast");
  input.end();
  await once(rl, "close");
  deepEq(lines, ["one", "two", "thé", "four", "last"], "lines");
}

// Questions, prompts and closing.
{
  const input = new PassThrough();
  const output = new PassThrough();
  output.setEncoding("utf8");
  const written = [];
  output.on("data", (data) => written.push(data));
  const rl = readline.createInterface(input, output);
  const answer = new Promise((resolve) => rl.question("name? ", resolve));
  input.write("Ada\n");
  eq(await answer, "Ada", "question");
  rl.setPrompt("$ ");
  eq(rl.getPrompt(), "$ ", "getPrompt");
  rl.prompt();
  await null;
  eq(written.join(""), "name? $ ", "questions and prompts write to the output");
  const closed = once(rl, "close");
  rl.close();
  await closed;
  eq(input.isPaused(), true, "close pauses the input");
  throws(() => rl.question("again? ", () => {}), "ERR_USE_AFTER_CLOSE", "questions after close");
}

// Async iteration.
{
  const input = new PassThrough();
  const rl = readline.createInterface({ input });
  const iterator = rl[Symbol.asyncIterator]();
  input.end("a\nb\nc");
  const lines = [];
  for await (const line of { [Symbol.asyncIterator]: () => iterator }) lines.push(line);
  deepEq(lines, ["a", "b", "c"], "for await over the lines");
}

// Leaving a loop early detaches the iterator and leaves the interface open.
{
  const input = new PassThrough();
  const rl = readline.createInterface({ input });
  eq(rl[Symbol.asyncIterator](), rl[Symbol.asyncIterator](), "one iterator per interface");
  input.write("start\nnext\n");
  for await (const line of rl) {
    if (line === "start") break;
  }
  eq(rl.closed, undefined, "break leaves the interface open");
  eq(rl.listenerCount("line"), 0, "break removes the line listener");
  const answer = new Promise((resolve) => rl.question("name? ", resolve));
  input.write("Ada\n");
  eq(await answer, "Ada", "questions after a loop ended early");
  rl.close();
}

// The signal option.
{
  const controller = new AbortController();
  const rl = readline.createInterface({ input: new PassThrough(), signal: controller.signal });
  const closed = once(rl, "close");
  controller.abort();
  await closed;
  eq(rl.closed, true, "aborting closes the interface");
  const aborted = readline.createInterface({ input: new PassThrough(), signal: AbortSignal.abort() });
  eq(aborted.closed, undefined, "an aborted signal closes the interface later");
  await new Promise((resolve) => process.nextTick(resolve));
  eq(aborted.closed, true, "an aborted signal closes the interface in a tick");
  throws(() => readline.createInterface({ input: new PassThrough(), signal: 5 }), "ERR_INVALID_ARG_TYPE", "the signal option is validated");
  readline.createInterface({ input: new PassThrough(), signal: 0 }).close();
}

// The promise API.
{
  const input = new PassThrough();
  const rl = readlinePromises.createInterface({ input, output: new PassThrough() });
  eq(rl instanceof readlinePromises.Interface, true, "readline/promises Interface");
  const answer = rl.question("sure? ");
  input.write("yes\n");
  eq(await answer, "yes", "promise questions");
  rl.close();
  eq((await rl.question("again? ").catch((error) => error)).code, "ERR_USE_AFTER_CLOSE", "rejected after close");
  eq(readline.promises.createInterface, readlinePromises.createInterface, "readline.promises");
}

// Cursor helpers write escape sequences.
{
  const sequences = [];
  const stream = {
    write(text) {
      sequences.push(text);
      return true;
    },
  };
  readline.cursorTo(stream, 2, 3);
  readline.cursorTo(stream, 5);
  readline.moveCursor(stream, -1, 2);
  readline.clearLine(stream, 0);
  readline.clearScreenDown(stream);
  deepEq(sequences, ["\x1b[4;3H", "\x1b[6G", "\x1b[1D\x1b[2B", "\x1b[2K", "\x1b[0J"], "escape sequences");
}
