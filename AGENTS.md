# Instructions for coding agents

Read these before changing the runtime:

- [docs/builtin-development.md](docs/builtin-development.md): the practices all builtins follow,
  how to verify changes, and how to work on this repository as an agent.
- [docs/rooting.md](docs/rooting.md): GC rooting for any code that handles JS values.
- [docs/node-compat.md](docs/node-compat.md#implementation-rules): the implementation rules for the
  Node.js builtins in `crates/builtins/node`.

The rules that past changes most often broke:

- Before adding a helper, cache, wrapper, macro or other plumbing, search `crates/js`,
  `crates/core-runtime`, `crates/starling-macro` and the web builtins for an existing one and use
  it. A second implementation of an existing capability is a defect.
- All of an instance's state is one Rust struct in slot 0, the class's private data. State is
  never kept in JS properties, hidden properties, JS arrays, WeakMaps or other side tables.
- Every class, including internal ones, is registered when a global is set up, never on first use.
- No borrow of a class's private data is held across a call into JavaScript.
- Builtins use `instance_local!`, never `thread_local!`.
- Native recursion over structures from content code is bounded, and allocations whose size comes
  from content code or the network are fallible.

[README.md](README.md#building-and-testing) describes building and testing.
