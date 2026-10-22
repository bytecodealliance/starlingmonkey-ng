# GC Rooting in StarlingMonkey

This is a practical guide to rooting for people writing builtins. Read the first two sections
before you write a builtin that handles JS values. The rest describes how rooting works, and how
the `crown` lint and the GC zeal tests check it.

## Why rooting exists

SpiderMonkey's garbage collector is moving and compacting. It can relocate a live JS object or
string to a new address at almost any allocation, and it reclaims everything it can't reach. It
uses exact rooting, so it only traces the references that were explicitly registered with it.
A raw `*mut JSObject` or a bare `Value` in a Rust local is invisible to the GC, and after a
collection it may point at a moved or freed cell.

Everything below follows from one rule:

> Never hold a bare JS reference across anything that can allocate. Hold it in something the GC
> traces: a rooting scope, or a traced heap slot.

Almost every JS operation can allocate: creating an object, calling a function, resolving a
promise, and even rooting another value. Assume that any `js::*` call can trigger a GC.

## The four types you'll use

| Type | What it is | Use it for |
|------|-----------|-----------|
| `Stack<'s, T>`, and its aliases and newtypes such as `Object<'s>`, `Promise<'s>` and `Dog<'s>` | A JS object reference rooted in a scope | Local object handles inside a function |
| `HandleValue<'s>` (`Handle<'s, Value>`) | Any JS value, rooted in a scope | Local value handles inside a function |
| `Heap<T>` and `Heap<Value>` | A traced slot for long-term storage | Fields of a builtin struct that outlive the call |
| `&Scope<'_>` | The rooting scope | Passed into every builtin, which roots its values in it |

`Stack<'s, T>` and `HandleValue<'s>` are rooted. They stay valid across allocations for the scope
lifetime `'s`. `Heap<T>` is traced. It stays valid when it is stored in a struct that is itself
traced. Bare `*mut JSObject` and `Value` are neither, so use them as soon as you have them and
don't keep them around.

Each user-defined class gets a newtype `Foo<'s>` that wraps `Stack<'s, FooImpl>` and implements
the class's methods. `Object<'s>`, `Promise<'s>` and the other builtin types are type aliases for
`Stack<'s, …>`. All of them behave the same way.

## Writing a builtin: the rules of thumb

A builtin method that declares a `scope: &Scope<'_>` parameter receives the current scope from the
macro. That scope is the rooting context for everything the method does.

**Most importantly, use the `#[jsclass]` / `#[jsmethods]` and `#[webidl_interface]` /
`#[webidl_methods]` proc macros wherever possible to create new builtin objects.** They encapsulate
the most challenging parts of rooting for you, and automate most type checking and conversion. For
WebIDL builtins, there are additional proc macros, such as `#[webidl_dictionary]`.

Beyond that, the following rules apply to all code handling JS values and objects.

### 1. Keep local JS references rooted

Creating or fetching a JS object or value returns a rooted handle. Keep using that handle, and
don't unwrap it to a raw pointer or a bare `Value`:

```rust
let obj = Object::new_plain(scope)?;        // Object<'s>, rooted
let v   = scope.root_value(some_raw_value);  // HandleValue<'s>, rooted
obj.set_property(scope, c"x", v)?;           // both stay valid across calls
```

### 2. Store long-lived JS references in `Heap` fields

A reference that must outlive the current call, such as a field of your class, goes in a `Heap<T>`
for an object or a `Heap<Value>` for any value:

```rust
#[webidl_interface]
pub struct MyThing {
    callback: Heap<js::object::Object>,  // a JS object reference
    detail:   Heap<Value>,               // any JS value
    #[no_trace]
    count:    u32,                        // plain data, not a JS reference
}
```

The class's trace hook traces the fields that hold JS references. Fields that hold none are marked
`#[no_trace]` where needed, or have a trivial implementation of the `Trace` trait.

### 3. Read a field by rooting it

`Heap::get` roots the stored reference in the scope and returns a rooted handle:

```rust
let cb = self.callback.get(scope);   // Function<'s> or Object<'s>
let d  = self.detail.get(scope);     // HandleValue<'s>
// both stay valid across whatever allocates next
```

### 4. Write a field in place

`Heap::set` takes a rooted handle directly, without a `Heap::from` round trip:

```rust
self.callback.set(new_obj);   // new_obj: Object<'s> or Function<'s>
self.detail.set(*some_value); // Heap<Value> takes a Value
```

Use `field.set(x)` to update an existing field. It writes in place through the GC's write barrier
and doesn't allocate. Use `Heap::from(x)` only when constructing the struct. For an
`Option<Heap<T>>` field, `OptionHeapExt` provides `get`, `set` and `take_rooted`.

## How it works under the hood

**Scopes.** A `Scope` owns a bump-allocated page of root slots drawn from a process-wide pool. The
pool is registered on SpiderMonkey's `autoGCRooters` stack for the whole lifetime of the runtime,
so during every GC the collector visits all live scope slots and updates them to the objects' new
locations. `scope.root_object(ptr)` and `scope.root_value(val)` write into a slot and return a
`Handle` that borrows that slot for `'s`. When the scope is dropped, its page returns to the pool.
A `Stack<'s, T>` is safe across allocations because the GC traces and updates the slot it borrows.

**Heaps.** A `Heap<T>` boxes a SpiderMonkey `Heap` cell (`Pin<Box<…>>`). The box keeps the cell,
for which the GC write barrier is registered, at a stable address when the `Heap` is moved into a
`Vec`, an `Option` or another struct. That makes `Heap::from(stack)` safe, and a `Heap` can be
stored anywhere. A `Heap` is traced only if the struct that holds it traces it in its `Trace`
implementation. `#[derive(Traceable)]` generates that implementation, which traces every field
except those marked `#[no_trace]`. Types that can't derive it, such as WebIDL enums, need a
hand-written `unsafe impl Trace`.

**Rooted types.** `Heap::get` and `Heap::take` both return `T::Rooted<'s>`, an associated type on
`JSType` and `ClassDef` that names the rooted handle type: `Stack<'s, T>` for builtin types, and
the newtype `Foo<'s>` for user classes. `get` roots the referent and leaves the `Heap` in place.
`take` consumes the `Heap`, dropping it before rooting the referent. Use `take` when the `Heap` was
moved out of its traced location into a local.

## Additional rooting helpers

The following macros and types cover references that don't fit the rules above.

### `#[derive(js::ScopeRoot)]` for types used once and then discarded

Some `#[must_root]` types are taken out of a traced list and used exactly once, such as a queued
read request, a queue entry or a promise slot. For these, `#[derive(js::ScopeRoot)]` generates a
mirror type whose fields are rooted in a scope, and a safe `root` method:

```rust
#[js::must_root]
#[derive(js::ScopeRoot)]
enum ReadRequest {
    Read { promise: Heap<Promise> },
    /* … */
}
// generates `StackReadRequest<'s>`, with each Heap<T> replaced by its rooted handle,
// and `ReadRequest::root(self, scope) -> StackReadRequest<'s>`.
```

Put the type's step and consume methods on the mirror (`impl StackReadRequest<'s>`). Its fields
are already rooted there, and no `allow` attribute is needed. Call them like this:

```rust
list.remove(0).root(scope).chunk_steps(scope, chunk)?;
```

Crown accepts the `must_root` value here because it is a temporary method receiver that `.root()`
consumes immediately. The result is the mirror, which isn't `must_root`. `root` is safe by
construction. If a variant or struct has a single `Heap<T>` field for an object type, `root` roots
it with `take`, which doesn't allocate. If it has several `Heap` fields or a `Heap<Value>`, `root`
first moves the value into a `RootedTraceableBox` and roots each field while the box keeps the
others traced, so rooting one field can't invalidate another.

Don't use `ScopeRoot` for a type that is mutated and re-queued in place, such as a pull-into
descriptor. Keep such a value in a `RootedTraceableBox` instead.

### `RootedHeap` and `RootedTraceableBox`: traced GC references outside a traced container

To hold one GC reference outside any traced container, such as in a timer's state, use a
`RootedHeap<T>`, where `T` is an object type or `Value`. It roots itself for its lifetime, and
creating and dropping one takes constant time. `RootedHeap::new(object)` creates one for an object,
`RootedHeap::from(value)` creates one for a `Value`, and `.get(scope)` reads it.

To keep an `impl Trace` value with several `Heap` fields outside any traced container while code
that can GC runs, without consuming it, wrap it in a `RootedTraceableBox`. The box registers itself
with the tracer for its lifetime, so the GC updates its `Heap`s in place.

### `#[allow_unrooted]`

Builtin code should almost never use this attribute. `#[js::allow_unrooted]` silences crown on one
item. It is `#[doc(hidden)]`, and exists for macro-generated code and a few audited boundaries,
such as a function that takes a `#[must_root]` value by value and immediately pushes it onto a
traced list. Hand-written builtin code roots the value, uses `ScopeRoot`, or uses
`RootedTraceableBox` instead.

## Enforcing proper rooting at the type system level: the `crown` static analysis lint

Rooting bugs are silent in release builds and intermittent under GC, so we don't rely on review to
catch them. A custom rustc lint, crown, enforces the rules at compile time. It's run with
`just check-gc` and also part of `just check-all`.

- A bare `Value` held in a downstream (builtin) crate is an error, whether it's a parameter, a
  `let`, `match` or `for` binding, a struct field or a cast. Root it as a `HandleValue`, or store
  it in a `Heap<Value>` if it's a struct field. A `Value` in return position is allowed, since it's
  passed straight to the engine.
- A raw `*mut JSObject` held downstream is an error for the same reason.
- A type marked `#[must_root]` can't be held by value in a plain local across an allocation. Mark
  any aggregate that bundles `Heap` fields with it, since such an aggregate must stay traced. Crown
  then requires callers to keep it traced or consume it immediately.

The fix for a crown error is almost always one of these: root the value, move it straight into a
traced container, or wrap it for as long as you need it.

Crown allows bare `Value`s inside the `js` crate. The types cover the case that matters there: a
raw `Value` doesn't implement `ToJSVal`, so an API taking `impl ToJSVal` only ever receives a
rooted value or one that holds no GC thing, and can GC before it converts it. Pass a raw value
read from a handle to an API that takes one directly, such as `Object::from_raw_value`, with
nothing that can GC in between.

## Ensuring proper rooting through dynamic checks

The tools and practices described here cut down on rooting complexity and catch most bugs
statically, but they're not 100% foolproof. To further improve rooting correctness, StarlingMonkey
CI runs parts of the test suite with dynamic checks enabled that are very likely to catch any
remaining issues. These checks make tests run many times more slowly, in some cases hundreds of
times, so they're not enabled for the entire test suite.

`just gc-zeal` runs them, and `scripts/test-gc-zeal.sh` lists the zeal modes and arguments:

```bash
just gc-zeal                               # quick mode, covering the `js` and `core-runtime` crates
just gc-zeal full                          # exhaustive checks for the same crates, takes a few minutes
just gc-zeal full --workspace --examples   # every mode over the examples' tests
```

Crown suffices for most code outside the `js` and `core-runtime` crates. A failure the dynamic
checks find outside those crates indicates a bug in one of them.

Test GC-sensitive builtins with the debug engine, which `just build` and `just test` enable through
the `debugmozjs` feature. For a thorough check, also run them under GC zeal: `JS_GC_ZEAL=14,1` runs
a compacting GC on every allocation, which exposes stale references that release builds hide.

## Cheat sheet

| You have… | Do this |
|-----------|---------|
| A new or fetched JS object | Keep the `Object<'s>` or newtype handle. Don't unwrap it to a pointer |
| A JS value to hold locally | `scope.root_value(v)`, which returns a `HandleValue<'s>` |
| A JS reference to store in a field | `Heap<T>` for an object or `Heap<Value>` for any value, traced by the struct |
| A `Heap` field to read | `field.get(scope)` |
| A `Heap` field to overwrite | `field.set(x)` |
| A struct to construct | `Heap::from(x)` for each `Heap` field |
| An `Option<Heap<T>>` field | `OptionHeapExt::{get, set, take_rooted}` |
| A bundle of `Heap`s settled once | `#[must_root] #[derive(js::ScopeRoot)]`, settled through `.root(scope)` |
| One reference to hold outside a traced container | `RootedHeap::new(x)` |
| An `impl Trace` value with several `Heap`s to keep across a GC without consuming it | `RootedTraceableBox::new(x)` |
| A crown error you don't understand | Root the value as described above. Don't use `allow_unrooted` |
