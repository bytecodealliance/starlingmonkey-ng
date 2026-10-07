// Unhandled rejections with `--unhandled-rejections=warn`, and `rejectionHandled`.

import { deepEq } from "./harness.mjs";

const events = [];
process.on("warning", (warning) => {
  const id = warning.message.match(/\(rejection id: \d+\)$/);
  events.push(`${warning.name}: ${id ? id[0] : warning.message}`);
});
process.on("unhandledRejection", (reason) => events.push(`unhandledRejection ${String(reason)}`));
const late = Promise.reject(1);
const quiet = Promise.reject(Symbol("s"));
Promise.reject(Promise.resolve()).catch(() => {});

setTimeout(() => {
  process.on("rejectionHandled", (promise) => events.push(`rejectionHandled ${promise === late}`));
  late.catch(() => {});
  setTimeout(() => {
    process.removeAllListeners("rejectionHandled");
    quiet.catch(() => {});
    setTimeout(() => {
      deepEq(
        events,
        [
          "unhandledRejection 1",
          "unhandledRejection Symbol(s)",
          "UnhandledPromiseRejectionWarning: 1",
          "UnhandledPromiseRejectionWarning: (rejection id: 1)",
          "UnhandledPromiseRejectionWarning: Symbol(s)",
          "UnhandledPromiseRejectionWarning: (rejection id: 2)",
          "rejectionHandled true",
          "PromiseRejectionHandledWarning: (rejection id: 2)",
        ],
        "events",
      );
    }, 1);
  }, 1);
}, 1);
