# starling-componentize

`starling-componentize` builds WebAssembly components from JavaScript modules on the StarlingMonkey runtime. See the
StarlingMonkey repository's README for its usage.

```bash
npx @bytecodealliance/starling-componentize --serve componentize app.js -o app.wasm
```

This package holds no binary itself. npm installs the one for your platform from an optional dependency, and
`require('@bytecodealliance/starling-componentize').binaryPath()` returns its path.
