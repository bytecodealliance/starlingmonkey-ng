# GC Rooting in StarlingMonkey

This is a practical guide to rooting for people writing builtins. Read the first
two sections before you write any builtin that touches a JS value; the rest
explains how it works and how the compiler keeps you honest.

## Why rooting exists

SpiderMonkey's garbage collector is **moving** and **compacting**: it can
relocate a live JS object (or string) to a new address at almost any allocation
point, and it reclaims everything it can't reach. It uses **exact rooting** — it
only knows about references you have explicitly handed it. A raw `*mut JSObject`
or a bare `Value` sitting in a Rust local is invisible to the GC: after a
collection it may point at a moved or freed cell.

So the one rule everything below serves is:

> **Never hold a bare JS reference across anything that can allocate.** Hold it
> in something the GC traces: a rooting scope, or a traced heap slot.

In practice "anything that can allocate" is almost every JS operation: creating
an object, calling a function, resolving a promise, even rooting another value.
Assume any `js::*` call can trigger a GC.

## The four types you'll actually use

| Type | What it is | Use it for |
|------|-----------|-----------|
| `Stack<'s, T>` (and its aliases/newtypes: `Object<'s>`, `Promise<'s>`, `Dog<'s>`) | A JS **object** reference rooted in a scope | Local object handles inside a function |
| `HandleValue<'s>` (= `Handle<'s, Value>`) | A JS **value** (any JSVal) rooted in a scope | Local value handles inside a function |
| `Heap<T>` / `Heap<Value>` | A GC-**traced** slot you can store long-term | Fields of a builtin struct that outlive the call |
| `&Scope<'_>` | The rooting scope itself | Passed into every builtin; the thing you root *with* |

`Stack<'s, T>` and `HandleValue<'s>` are **rooted** — safe to hold across
allocations for the scope lifetime `'s`. `Heap<T>` is **traced** — safe to store
in a struct that is itself traced. Bare `*mut JSObject` and `Value` are neither;
treat them as radioactive and consume them immediately.

User-defined classes get a newtype `Foo<'s>` that wraps `Stack<'s, FooImpl>` and
carries the class's methods. `Object<'s>`, `Promise<'s>`, etc. are type aliases
for `Stack<'s, …>`. They all behave the same way.

## Writing a builtin: the rules of thumb

A builtin method receives a `&Scope<'_>` (declare a `scope: &Scope<'_>`
parameter and the macro wires it up). That scope is your rooting context.

**Most importantly, use the `#[jsclass]` / `[jsmethods]` and `[webidl_interface]` / `#[webidl_methods]` proc macros wherever possible to create new builtin objects.**
They encapsulate the most challenging parts of rooting for you, and automate most of type checking and conversion. For WebIDL builtins, there are additional proc macros, including `#[webidl_dictionary]`, etc.

Beyond that, the following rules apply to all code handling JS values and objects:

**1. Keep local JS references rooted.** Creating or fetching a JS object/value
hands you a rooted handle — keep it as that handle, don't unwrap it to a raw
pointer or bare `Value`:

```rust
let obj = Object::new_plain(scope)?;        // Object<'s> — rooted
let v   = scope.root_value(some_raw_value);  // HandleValue<'s> — rooted
obj.set_property(scope, c"x", v)?;           // safe to use across calls
```

**2. Store long-lived JS references in `Heap` fields.** A reference that must
outlive the current call (a field of your class) goes in a `Heap<T>` for object
references or `Heap<Value>` for arbitrary values:

```rust
#[webidl_interface]
pub struct MyThing {
    callback: Heap<js::object::Object>,  // a JS object reference
    detail:   Heap<Value>,               // an arbitrary JS value
    #[no_trace]
    count:    u32,                        // plain data — not a JS reference
}
```

Object-bearing fields are traced for you (the class's trace hook walks them).
Plain non-JS fields get `#[no_trace]` where needed, or have trivial implementations of the `Trace` trait.

**3. Read a field by rooting it back out.** `Heap::get` roots the stored
reference into the scope and hands you a rooted handle:

```rust
let cb = self.callback.get(scope);   // Function<'s> / Object<'s>
let d  = self.detail.get(scope);     // HandleValue<'s>
// both stay valid across whatever allocates next
```

**4. Write a field in place.** `Heap::set` takes the rooted handle (or a bare
`Stack`) directly — no `Heap::from` round-trip:

```rust
self.callback.set(new_obj);   // new_obj: Object<'s> / Function<'s>
self.detail.set(*some_value); // Heap<Value> takes a Value
```

Use `field.set(x)` to update an existing field (an in-place barriered write, no
allocation). Use `Heap::from(x)` only when *constructing* the struct or filling
an `Option<Heap<T>>` that was `None`.

That's 90% of it. If you follow these four rules you won't have a rooting bug.

## How it works under the hood

**Scopes.** A `Scope` owns a bump-allocated page of root slots drawn from a
process-wide pool. That pool sits on SpiderMonkey's `autoGCRooters` stack for the
whole runtime lifetime, so during every GC the collector walks all live scope slots and
**updates** them to the moved locations.
`scope.root_object(ptr)` / `scope.root_value(val)` write into a slot and return a
`Handle` borrowing that slot for `'s`. When the scope drops, its page returns to
the pool. This is why a `Stack<'s, T>` is safe across allocations: the slot it
borrows is traced and relocated by the GC.

**Heaps.** A `Heap<T>` boxes a SpiderMonkey `Heap` cell (`Pin<Box<…>>`). The box
keeps the GC **write barrier** registered at a stable address even when you move
the `Heap` into a `Vec`, an `Option`, or another struct — which is what makes
`Heap::from(stack)` and storing it anywhere safe. A `Heap` is only traced if
something traces it: that's the job of the `Trace` impl on the struct that holds
it. `#[derive(Traceable)]` generates that impl (walking every field except
`#[no_trace]` ones). Types that can't derive it, such as WebIDL enums, need
hand-written `unsafe impl Trace` implementations.

**The mapping.** Both `Heap::get` and `Heap::take` return `T::Rooted<'s>` — an
associated type on `JSType`/`ClassDef` that names the canonical rooted handle
(`Stack<'s, T>` for builtins, the newtype `Foo<'s>` for user classes). `get` roots
without consuming the `Heap`, `take` consumes it (drop-before-root) for when
you've moved the `Heap` out of its traced home into a local.

## Additional rooting helper macros

In addition to the above, there are some proc macros that help manage and/or enforce proper rooting:

### `#[derive(js::ScopeRoot)]` — for "settle once then discard" types

Some `#[must_root]` types are *consume-leaves*: they're pulled out of a traced
list and settled exactly once (a queued read request, a queue entry, a promise
slot). For these, `#[derive(js::ScopeRoot)]` generates a scope-rooted mirror and
a safe `root` method:

```rust
#[js::must_root]
#[derive(js::ScopeRoot)]
enum ReadRequest {
    Read { promise: Heap<Promise> },
    /* … */
}
// generates `StackReadRequest<'s>` (each Heap<T> → its rooted handle) and
// `ReadRequest::root(self, scope) -> StackReadRequest<'s>`.
```

Put the type's step/consume methods on the mirror (`impl StackReadRequest<'s>`),
where the fields are already rooted and no `allow` is needed, and call them as:

```rust
list.remove(0).root(scope).chunk_steps(scope, chunk)?;
```

The `must_root` value is a method-receiver temporary immediately consumed by
`.root()`, which crown accepts; the result is the non-`must_root` mirror. `root`
is safe by construction: a single-`Heap` group is rooted with `take` (no
allocation); a multi-`Heap` group is moved into a `RootedTraceableBox` and each
field rooted while traced, so rooting one can't stale another.

Don't reach for this on a type that is mutated and re-queued in place (e.g. a
pull-into descriptor) — a consume-once transform doesn't fit it; keep it in a
`RootedTraceableBox` instead.

### `RootedTraceableBox` — keep `impl Trace` types traced across a GC

When you must hold a `Heap`-bearing value *outside* any traced container while
GC-triggering work runs (you're not consuming it, you're keeping it), wrap it in
a `RootedTraceableBox`: it self-registers with the tracer for its lifetime, so
the GC updates its `Heap`s in place.

### `#[allow_unrooted]` — the last-resort escape hatch you really shouldn't use

**THIS IS ALMOST CERTAINLY THE WRONG THING TO USE!**
`#[js::allow_unrooted]` silences crown on one item. It is `#[doc(hidden)]` and
exists for macro-generated code and a handful of audited boundaries (e.g. a
function that takes a `#[must_root]` value by value and immediately pushes it
onto a traced list). If you're writing an open-coded builtin and reaching for it,
you almost certainly want proper rooting instead: root the value, use
`ScopeRoot`, or use `RootedTraceableBox`.

## Enforcing proper rooting at the type system level: the `crown` static analysis lint

Rooting bugs are silent in release builds and intermittent under GC, so we don't
rely on review to catch them. A custom rustc lint, **crown**, enforces the rules
at compile time (`just crown`, and part of `just check-all`):

- A bare **`Value`** held in a downstream (builtin) crate — as a parameter,
  `let`/`match`/`for` binding, struct field, or cast — is an error. Root it
  (`HandleValue`) or, if it's a struct field, store a `Heap<Value>`. (`Value` in
  return position is fine — it's handed straight to the engine.)
- A raw **`*mut JSObject`** held downstream is an error for the same reason.
- A type marked **`#[must_root]`** may not be held by value in a plain local
  across an allocation. Use this on any aggregate that bundles `Heap` fields (so
  it must stay traced): crown then forces callers to keep it traced or consume it
  immediately.

When crown flags something, the fix is almost always one of: root the value, move
it straight into a traced container, or wrap it while you need it.

Crown allows bare `Value`s inside the `js` crate. The types cover the case that matters there: a
raw `Value` doesn't implement `ToJSVal`, so an API taking `impl ToJSVal` only ever receives a
rooted value or one that holds no GC thing, and can GC before it converts it. Pass a raw value
read from a handle to an API that takes one directly, such as `Object::from_raw_value`, with
nothing that can GC in between.

## Ensuring proper rooting through dynamic checks

The tools and practices described here cut down on rooting complexity and catch most bugs statically, but they're not 100% foolproof. To further improve rooting correctness, StarlingMonkey CI runs parts of the test suite with dynamic checks enabled that are very highly likely to catch any remaining issues. These checks cause tests to run many (in some cases: hundreds of) times more slowly, so they're not enabled for the entire test suite.

See the `scripts/test-gc-zeal.sh` script for the different checks, and for details of how to run these checks for your builtins.

## Cheat sheet

| You have… | Do this |
|-----------|---------|
| A new/fetched JS object | Keep the `Object<'s>` / newtype handle; don't unwrap to a pointer |
| A JS value to hold locally | `scope.root_value(v)` → `HandleValue<'s>` |
| A JS reference to store in a field | `Heap<T>` (object) or `Heap<Value>` (any value); trace it |
| To read a `Heap` field | `field.get(scope)` |
| To overwrite a `Heap` field | `field.set(x)` |
| To construct a struct / fill `Option<Heap>` | `Heap::from(x)` |
| A bundle of `Heap`s settled once | `#[must_root] #[derive(js::ScopeRoot)]`, settle via `.root(scope)` |
| To hold a `Heap` value across a GC without consuming it | `RootedTraceableBox::new(x)` |
| A crown error you don't understand | **Root more!** Don't `allow_unrooted`, no matter how tempting it seems |

Always test GC-sensitive builtins with the debug engine (`just build`, which
enables `debugmozjs`) and, for the thorough check, under GC zeal
(`JS_GC_ZEAL=14,1` forces a compacting GC on every allocation, surfacing stale
references that release builds hide).
