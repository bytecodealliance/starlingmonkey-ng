// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The serve-mode WPT runner: the content script the runtime runs under `--serve --wpt-mode`.
// Each request's body is one assembled test script (harness and test), which this evaluates, and
// the response is the JSON results.
//
// Everything is inside this IIFE because the tests are evaluated into this global, and
// top-level declarations here would collide with theirs, such as testharness.js's `timeout()`.
(() => {
  addEventListener("fetch", (event) => {
    event.respondWith(runTest(event.request));
  });

  const TIMED_OUT = Symbol("timed out");

  /// The wait for a test's results. It is longer than testharness.js's own 10s test timeout, and
  /// shorter than the runner's per-test timeout.
  const WAIT_LIMIT_MS = 20000;

  /// Evaluate one assembled test script and resolve with its results.
  async function runTest(request) {
    const script = await request.text();

    // testharness.js and the results hook below keep their state on the global, so only one test
    // can run in it at a time.
    if (globalThis.__wptRunning) {
      return errorResponse(
        new Error(
          "a WPT test is already running in this global: run the server with one request in " +
            "flight at a time, or with one instance per request",
        ),
      );
    }
    globalThis.__wptRunning = true;

    // post-harness.js passes the results to `__wptReportResults`. It is installed before
    // evaluating, since a synchronous test completes during `evalScript`.
    let deliver;
    const results = new Promise((resolve) => {
      deliver = resolve;
    });
    globalThis.__wptReportResults = deliver;

    try {
      try {
        // `evalScript` (a --wpt-mode global) evaluates in non-syntactic scope, so the script's
        // top-level `let`/`const` are visible to the `evalScript` calls of its META dependencies.
        evalScript(script, "wpt-test");
      } catch (e) {
        // A top-level throw means the harness never reached `done()`, so `results` never settles.
        return errorResponse(e);
      }

      // An async test finishes after `evalScript` returns, so wait for the completion callback.
      const limit = waitLimit();
      const json = await Promise.race([results, limit.reached]);
      // A pending limit would keep this request's event loop alive after the response.
      limit.cancel();
      if (json === TIMED_OUT) {
        return errorResponse(new Error("the test did not complete"));
      }
      return new Response(json, {
        headers: { "content-type": "application/json" },
      });
    } finally {
      globalThis.__wptRunning = false;
      // Stop this request's event loop, now that its response is built. A finished test may have
      // left a timer running, such as `timers/clearinterval-from-callback`'s `setInterval`.
      if (typeof __wptDone === "function") __wptDone();
    }
  }

  /// A promise that resolves to `TIMED_OUT` after `WAIT_LIMIT_MS`, and the function that cancels it.
  function waitLimit() {
    let id;
    const reached = new Promise((resolve) => {
      id = setTimeout(() => resolve(TIMED_OUT), WAIT_LIMIT_MS);
    });
    return { reached, cancel: () => clearTimeout(id) };
  }

  /// A 500 response for a test that didn't report its results.
  function errorResponse(e) {
    // A SpiderMonkey `stack` has only the frames, so the name and message come first.
    let message;
    try {
      message = e instanceof Error ? `${e.name}: ${e.message}\n${e.stack}` : String(e);
    } catch {
      message = "a non-Error value was thrown";
    }
    return new Response(JSON.stringify({ error: message }), {
      status: 500,
      headers: { "content-type": "application/json" },
    });
  }
})();
