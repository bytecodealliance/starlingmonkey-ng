// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// Pre-harness: sets up the global environment that the WPT testharness.js
// expects before any tests are loaded.
//
// This script runs before testharness.js is loaded.

// `self` is the global object in workers and the WPT harness expects it.
globalThis.self = globalThis;

// WPT testharness.js checks for GLOBAL.isWindow(), isWorker(), etc.
globalThis.GLOBAL = {
  isWindow: function() { return false; },
  isWorker: function() { return true; },
  isShadowRealm: function() { return false; },
};

// Some tests reference `window` or `Window`.
globalThis.window = globalThis;
globalThis.Window = {
  prototype: {}
};

// Where post-harness.js passes a finished test's results. Serve mode (wpt-server.js) installs its
// own before this script runs, so this is command mode's.
globalThis.__wptReportResults ??= function(json) {
  // The runner reads this line from stdout.
  console.log("WPT_RESULTS_JSON:" + json);
  // Stop the event loop, since a finished test may have left a timer running.
  if (typeof __wptDone === "function") __wptDone();
};
