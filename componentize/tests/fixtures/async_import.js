// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The async-import-and-export world guest module.
//
// `bar` is the async WIT import, synthesized into the ES module named after its
// interface (`test:async-import/host`). Its synthesized function returns a
// Promise that settles once the host subtask completes, so the guest `await`s it.
//
// `runner.foo` is the async WIT export, provided in the package layer as
// `testAsyncImport.runner.foo`. It is a genuine `async function`, so the runtime
// drives a per-call event loop until the promise it returns settles, and that
// same loop drives the async import's future.

import { bar, baz } from "test:async-import/host";

let barMessage = "none";

const runner = {
    barMessage() {
        const recorded = barMessage;
        bar(1).then(
            () => { barMessage = "bar succeeded in a sync export"; },
            (e) => { barMessage = e instanceof TypeError ? e.message : String(e); },
        );
        return recorded;
    },

    // `await bar(v)` suspends through the host (the host yields before answering
    // `v + 2`), then `+ 3`. So `foo(42)` => await 44 => 47.
    foo: async function (v) {
        const fromHost = await bar(v);
        return fromHost + 3;
    },
    // The `result` argument arrives as `{tag, val}` and is passed on as-is. The
    // import's `ok` settles the promise with the bare payload, and its `err`
    // rejects with a `ComponentError` whose payload is the value. Each gets `+ 3`.
    qux: async function (v) {
        let fromHost;
        try {
            fromHost = await baz(v);
        } catch (e) {
            if (!(e instanceof ComponentError)) throw e;
            throw new ComponentError(/** @type {number} */ (e.payload) + 3);
        }
        return fromHost + 3;
    },
};

// The package layer of the export names: `test:async-import/runner`'s items
// are members of `testAsyncImport.runner`.
export const testAsyncImport = { runner };
