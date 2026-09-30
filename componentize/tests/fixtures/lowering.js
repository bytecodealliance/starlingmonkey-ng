// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The lowering-world guest module. Each case of `runner.run` changes a value
// after passing it to an import, and describes what the import received.

import * as hostModule from "test:lowering/host"
import { Thing, makeBare, bareValue, consume, peek, drain, takeTwo, accept, firstByte } from "test:lowering/host"

/** @param {unknown} e */
function describe(e) {
    return e instanceof Error ? `${e.constructor.name}: ${e.message}` : `not an Error: ${e}`
}

/** @param {() => Promise<unknown>} f */
async function settled(f) {
    try {
        return `ok ${await f()}`
    } catch (e) {
        return describe(e)
    }
}

async function peekDisposed() {
    using thing = new Thing(6)
    return peek(thing)
}

/** @type {Record<string, () => Promise<string>>} */
const cases = {
    async disposeAfterConsume() {
        const thing = new Thing(5)
        const consumed = consume(thing)
        thing[Symbol.dispose]()
        return settled(() => consumed)
    },
    async consumeTwice() {
        const thing = new Thing(5)
        return settled(() => Promise.all([consume(thing), consume(thing)]))
    },
    async disposeWhilePeeked() {
        return settled(peekDisposed)
    },
    async consumeWhilePeeked() {
        const thing = new Thing(3)
        const peeked = peek(thing)
        const consumed = await settled(() => consume(thing))
        return `${consumed}, ${await settled(() => peeked)}`
    },
    async drainTwice() {
        const stream = new ReadableStream({
            start(controller) {
                controller.enqueue(1)
                controller.enqueue(2)
                controller.close()
            },
        })
        return settled(() => Promise.all([drain(stream), drain(stream)]))
    },
    async changeBufferAfterCall() {
        const buffer = new Uint8Array([1, 2, 3])
        const first = firstByte(buffer)
        buffer[0] = 99
        return settled(() => first)
    },
    async pullDisposesArgument() {
        const thing = new Thing(9)
        const stream = new ReadableStream({
            pull(controller) {
                thing[Symbol.dispose]()
                controller.enqueue(1)
                controller.close()
            },
        }, { highWaterMark: 0 })
        return settled(async () => takeTwo(stream, thing))
    },
    async bareResource() {
        const bare = makeBare()
        const value = bareValue(bare)
        bare[Symbol.dispose]()
        return `${typeof hostModule.Bare} ${bare instanceof hostModule.Bare} ${value}`
    },
}

class Widget {
    get() {
        return 1
    }

    /** @param {boolean} sub */
    static make(sub) {
        return sub ? new SubWidget() : new Widget()
    }
}

class SubWidget extends Widget {
    get() {
        return 2
    }
}

export const runner = {
    Widget,
    /** @param {string} which */
    run(which) {
        return cases[which]()
    },
    give() {
        try {
            accept(Promise.resolve(42))
            return "no error"
        } catch (e) {
            return describe(e)
        }
    },
    /** @param {boolean} together */
    async things(together) {
        const thing = new Thing(3)
        if (!together) {
            return [thing, thing]
        }
        return new ReadableStream({
            start(controller) {
                controller.enqueue(thing)
                controller.enqueue(thing)
                controller.close()
            },
        })
    },
}
