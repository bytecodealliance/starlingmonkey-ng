# starling-componentize

`starling-componentize` builds WebAssembly components from JavaScript modules on the StarlingMonkey runtime. The
[StarlingMonkey README](https://github.com/bytecodealliance/starlingmonkey#componentizing-javascript) describes its
usage.

```bash
npx @bytecodealliance/starling-componentize --export-http componentize app.js -o app.wasm
```

This package holds no binary itself. npm installs the one for your platform from an optional dependency, and
`require('@bytecodealliance/starling-componentize').binaryPath()` returns its path. There are binaries for macOS
(arm64 and x64), Linux (arm64 and x64) and Windows (x64). The Linux binaries are statically linked, so they run with
any C library, such as glibc or musl.
