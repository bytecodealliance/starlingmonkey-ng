// Reads lines from standard input until the line `stop` or the end of the input, then prints
// them. Closing the interface pauses `process.stdin`, which lets the process exit while standard
// input stays open.

import readline from "node:readline";

const rl = readline.createInterface({ input: process.stdin });
const lines = [];
rl.on("line", (line) => {
  lines.push(line);
  if (line === "stop") rl.close();
});
await new Promise((resolve) => rl.on("close", resolve));
console.log(JSON.stringify({ lines, tty: process.stdin.isTTY }));
