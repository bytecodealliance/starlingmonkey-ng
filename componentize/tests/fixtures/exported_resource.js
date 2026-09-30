// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
//
// The exported-resource guest module.
//
// `Counter` is a plain JS class the guest owns. When an instance crosses out to
// the host (returned from `makeCounter`, or from the `[constructor]counter`
// path), the interpreter mints a canonical handle for it through the resource's
// `[resource-new]` extern and parks the object in the exported-resource slab,
// keyed by its rep. Host calls to `bump`/`value` arrive as `[method]` exports
// whose `this` is the slab-resident object (looked up by rep). `totalCreated`
// and `liveCount` are statics, dispatched on the class object.
//
// When the host drops the handle, the runtime's `resource_dtor` removes the slab
// entry and calls the object's `[Symbol.dispose]()`. That method decrements
// `liveCount`, so a host-side drop is observable across the boundary as
// `live-count` falling.
//
// `makeCounter` and the `Counter` class are bare exports. The runtime looks
// instance methods up on `Counter.prototype` and statics on `Counter`.

let totalCreated = 0;
let liveCount = 0;
let disposeCalls = 0;

class Counter {
  constructor(start) {
    this.value_ = start;
    totalCreated += 1;
    liveCount += 1;
  }

  // The runtime calls this when the host drops the resource.
  [Symbol.dispose]() {
    liveCount -= 1;
    disposeCalls += 1;
  }

  bump(by) {
    this.value_ += by;
    return this.value_;
  }

  value() {
    return this.value_;
  }

  static totalCreated() {
    return totalCreated;
  }

  static liveCount() {
    return liveCount;
  }

  static disposeCalls() {
    return disposeCalls;
  }
}

// The interface's items are bare exports.
export function makeCounter(start) {
  return new Counter(start);
}

export function makeSealedCounter(start) {
  return Object.preventExtensions(new Counter(start));
}

export { Counter };

export async function readLater(counter) {
  await new Promise((resolve) => setTimeout(resolve, 5));
  return counter.value();
}
