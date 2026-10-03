// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The Component Model streams and futures world guest module.
//
// A `stream<T>` argument arrives as a `ReadableStream`: a byte stream of
// `Uint8Array` chunks for `stream<u8>`, and one chunk per element otherwise. A
// `future<T>` argument arrives as a `Promise`. A `stream<T>` result may be any
// `ReadableStream` or iterable, and a `future<T>` result any promise or value.
//
// `HostThing` is the class the runtime synthesized for the imported `host-thing`
// resource: the elements of a `stream<host-thing>` are instances of it.

import { HostThing } from "test:streams/host-thing-interface"
import { Failure, lookup } from "test:streams/failing"
import { count, later, plusOne, sum } from "test:streams/transfers"

async function echoStreamIdiomatic(incoming) {
    let { readable, writable } = new TransformStream()
    incoming.pipeTo(writable)
    return readable
}

async function echoFutureIdiomatic(incoming) {
    let value = await incoming
    return value
}

function passStream(incoming) {
    return incoming
}

async function* reyield(incoming) {
    for await (const element of incoming) {
        yield element
    }
}

async function echoElements(incoming) {
    return reyield(incoming)
}

// A guest-owned resource streamed as `stream<thing>`. Streaming a `thing` lifts
// each incoming `own<thing>` to its slab object and lowers it back out.
class Thing {
    constructor(value) {
        this.value_ = value
    }

    get() {
        return this.value_
    }
}

async function byobRead(incoming) {
    const reader = incoming.getReader({ mode: "byob" })
    const decoder = new TextDecoder()
    let text = ""
    let buffer = new ArrayBuffer(7)
    for (;;) {
        const { value, done } = await reader.read(new Uint8Array(buffer))
        if (done) {
            break
        }
        text += decoder.decode(value, { stream: true })
        buffer = value.buffer
    }
    return text + decoder.decode()
}

function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms))
}

async function* timedChunks(count) {
    for (let i = 0; i < count; i++) {
        await sleep(2)
        yield new Uint8Array([i])
    }
}

async function timedChunksExport(count) {
    return timedChunks(count)
}

async function afterFirstChunk(incoming) {
    const reader = incoming.getReader()
    const { done } = await reader.read()
    assert(!done, "expected a first chunk")
    reader.releaseLock()
    return incoming
}

async function cancelAfterFirstChunk(incoming) {
    const reader = incoming.getReader()
    const { value, done } = await reader.read()
    assert(!done, "expected a first chunk")
    await reader.cancel("enough")
    return value.length
}

let endlessCancelled = false

function endless() {
    return new ReadableStream({
        pull(controller) {
            controller.enqueue(new Uint8Array(1024))
        },
        cancel() {
            endlessCancelled = true
        },
    })
}

function wrongChunks() {
    // @ts-expect-error TypeScript's DOM library has no `ReadableStream.from`.
    return ReadableStream.from(["not", "bytes"])
}

function notAStream() {
    return 42
}

// An async export returning a `future<T>` returns the future itself: the
// function's own promise becomes the future, so these settle it after the call
// has returned.
async function futureResult(ok, value) {
    await sleep(1)
    if (ok) {
        return value
    }
    throw new Error(value)
}

async function futureResultBare(value) {
    await sleep(1)
    throw value
}

async function describeFutureResult(incoming) {
    try {
        return `ok: ${await incoming}`
    } catch (e) {
        return `err: ${e instanceof ComponentError} ${e.payload}`
    }
}

async function describeFailureFuture(incoming) {
    try {
        return `ok: ${await incoming}`
    } catch (e) {
        return [
            e instanceof Failure,
            e instanceof ComponentError,
            e.name,
            JSON.stringify(e.payload),
        ].join(" ")
    }
}

// The imported `lookup` fails with an instance of `Failure`, the class of its
// `err` type, which extends `ComponentError`.
async function describeLookup(key) {
    try {
        return `found ${await lookup(key)}`
    } catch (e) {
        return [
            e instanceof Failure,
            e instanceof ComponentError,
            e.name,
            JSON.stringify(e.payload),
        ].join(" ")
    }
}

async function forwardLookup(key) {
    return await lookup(key)
}

async function futureDenied() {
    await sleep(1)
    throw new Error("denied")
}

// Read every host thing, check each one, then return them all.
async function shortReadsHost(incoming) {
    const things = []
    for await (const thing of incoming) {
        things.push(thing)
    }
    const expected = ["a", "b", "c", "d", "e"]
    assert(
        things.length === expected.length,
        `expected ${expected.length} things, got ${things.length}`,
    )
    for (let i = 0; i < things.length; i++) {
        assert(things[i] instanceof HostThing, `element ${i} is not a HostThing`)
        assert(things[i].get() === expected[i], `element ${i} get() is not ${expected[i]}`)
        assert(
            HostThing.getStatic(things[i]) === expected[i],
            `element ${i} getStatic() is not ${expected[i]}`,
        )
    }
    return things
}

// A stream and a future passed to imports, and the ones imports return.
async function useTransfers() {
    const total = await sum([1, 2, 3, 4])
    const counted = []
    for await (const value of await count(3)) {
        counted.push(value)
    }
    const delivered = await later(7)
    const incremented = await plusOne(Promise.resolve(41))
    return `sum=${total} count=${counted.join(",")} later=${delivered} plus-one=${incremented}`
}

async function* manyStrings() {
    for (let i = 0; i < 50; i++) {
        yield `s${i}`.repeat(20)
    }
}

async function* manyLists() {
    for (let i = 0; i < 50; i++) {
        yield new Uint32Array(8 + (i % 5))
    }
}

function stringLater() {
    return sleep(1).then(() => "later".repeat(20))
}

/** @param {string} s */
async function ping(s) {
    await sleep(5)
    return `${s}${s.length}`
}

function arrayBufferChunks() {
    // @ts-expect-error TypeScript's DOM library has no `ReadableStream.from`.
    return ReadableStream.from([new Uint8Array([1, 2]).buffer, new Uint8Array([3]).buffer])
}

function emptyChunks() {
    // @ts-expect-error TypeScript's DOM library has no `ReadableStream.from`.
    return ReadableStream.from([
        new Uint8Array(0),
        new Uint8Array([1]),
        new ArrayBuffer(0),
        new Uint8Array([2, 3]),
        new Uint8Array(0),
    ])
}

function lockedStream() {
    const stream = new ReadableStream()
    stream.getReader()
    return stream
}

async function plusOnePlain() {
    return await plusOne(41)
}

function wrongPoints() {
    // @ts-expect-error TypeScript's DOM library has no `ReadableStream.from`.
    return ReadableStream.from([{ x: 1, y: 2 }, { x: "a", y: 3 }])
}

function shiftingPoints() {
    let reads = 0
    // @ts-expect-error TypeScript's DOM library has no `ReadableStream.from`.
    return ReadableStream.from([{ get x() { return ++reads === 1 ? 1 : "a" }, y: 2 }])
}

function rejectLater() {
    return sleep(1).then(() => {
        throw new Error("no value")
    })
}

async function slowFuture(value) {
    await sleep(20)
    return value
}

async function thingFuture(value) {
    await sleep(1)
    return new Thing(value)
}

async function cancelPendingRead(incoming) {
    const reader = incoming.getReader()
    const read = reader.read()
    await sleep(5)
    await reader.cancel("waited long enough")
    const { done } = await read
    return done ? "cancelled" : "read a chunk"
}

let idleCancelled = false

function idleStream() {
    return new ReadableStream({
        start(controller) {
            controller.enqueue(new Uint8Array([1]))
        },
        pull() {
            return new Promise(() => {})
        },
        cancel() {
            idleCancelled = true
        },
    })
}

async function streamMisuseMessages() {
    const locked = new ReadableStream()
    locked.getReader()
    const messages = []
    for (const arg of [5, {}, locked]) {
        messages.push(await sum(/** @type {any} */ (arg)).then(
            () => "no error",
            (e) => e instanceof TypeError ? e.message : `not a TypeError: ${e}`,
        ))
    }
    messages.push(String(await sum(/** @type {any} */ (new Set([1, 2])))))
    return messages
}

/** @type {ReadableStream<Uint8Array> | null} */
let heldStream = null
let syncReadMessage = "none"

function syncStreamMessage() {
    const recorded = syncReadMessage
    if (heldStream) {
        heldStream.getReader().read().then(
            () => { syncReadMessage = "the read succeeded in a sync export" },
            (e) => { syncReadMessage = e instanceof TypeError ? e.message : String(e) },
        )
        heldStream = null
    }
    return recorded
}

function assert(condition, message) {
    if (!condition) {
        throw new Error(message)
    }
}

/** @type {(value: number) => void} */
let settleHeld = () => {}

/** @type {WeakRef<Promise<number>> | null} */
let unreferencedFuture = null

/** A promise that never settles, which only `unreferencedFuture` references. */
function unreferencedNeverFuture() {
    const promise = new Promise(() => {})
    unreferencedFuture = new WeakRef(promise)
    return promise
}

/**
 * Allocate garbage and wait on timers until the promise `unreferencedNeverFuture`
 * returned is collected, for at most 1000 turns. Returns whether it was. Each
 * turn's allocation stays referenced into the next turn, so it outlives a minor
 * collection and the tenured heap grows until a major collection runs. The
 * allocation comes before the `deref` in each turn, since `deref` keeps the
 * target alive until the turn ends.
 */
async function collectUnreferencedFuture() {
    const future = unreferencedFuture
    if (future === null) {
        throw new Error('unreferencedNeverFuture has not been called')
    }
    let retained = null
    let collected = false
    for (let turn = 0; turn < 1000 && !collected; turn++) {
        retained = Array.from({ length: 20000 }, (_, i) => ({ i, s: `garbage ${i}` }))
        collected = future.deref() === undefined
        await new Promise((resolve) => setTimeout(resolve, 0))
    }
    retained = null
    return collected
}

export const echoes = {
    Thing,
    echoStreamU8: echoStreamIdiomatic,
    passStreamU8: passStream,
    echoFutureString: echoFutureIdiomatic,
    echoFutureU32: echoFutureIdiomatic,
    echoStreamString: echoElements,
    echoStreamPoint: echoElements,
    echoStreamThing: echoElements,
    byobRead,
    timedChunks: timedChunksExport,
    afterFirstChunk,
    cancelAfterFirstChunk,
    endless,
    endlessCancelled: () => endlessCancelled,
    wrongChunks,
    notAStream,
    futureResult,
    futureResultBare,
    describeFutureResult,
    describeFailureFuture,
    describeLookup,
    forwardLookup,
    futureDenied,
    shortReadsHost,
    useTransfers,
    wrongPoints,
    arrayBufferChunks,
    emptyChunks,
    manyStrings: async () => manyStrings(),
    manyLists: async () => manyLists(),
    stringLater,
    ping,
    lockedStream,
    plusOnePlain,
    shiftingPoints,
    slowFuture,
    rejectedFuture: rejectLater,
    rejectedResultFuture: rejectLater,
    thingFuture,
    cancelPendingRead,
    passAsBytes: passStream,
    idleStream,
    idleCancelled: () => idleCancelled,
    streamMisuseMessages,
    holdStream: async (s) => { heldStream = s },
    syncStreamMessage,
    neverFuture: () => new Promise(() => {}),
    unreferencedNeverFuture,
    collectUnreferencedFuture,
    heldFuture: () => new Promise((resolve) => { settleHeld = resolve }),
    /** @param {number} value */
    settleHeldFuture(value) { settleHeld(value) },
    /** @param {number} value */
    settleHeldFutureAsync: async (value) => { settleHeld(value) },
    /** @param {string} value */
    disposedThingFuture(value) {
        const thing = new HostThing(value)
        setTimeout(() => thing[Symbol.dispose](), 0)
        return Promise.resolve(thing)
    },
}
