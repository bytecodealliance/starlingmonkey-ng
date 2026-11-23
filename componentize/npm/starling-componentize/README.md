# starling-componentize

`starling-componentize` builds WebAssembly components from JavaScript modules on the StarlingMonkey runtime. The
[StarlingMonkey README](https://github.com/bytecodealliance/starlingmonkey#componentizing-javascript) describes its
usage.

```bash
npx @bytecodealliance/starling-componentize --export-http componentize app.js -o app.wasm
```

The package bundles the release runtime, `starling.wasm`, which the command uses unless `--runtime` or
`STARLING_RUNTIME` names another. With `STARLING_RUNTIME_BUILD=debug` it uses the debug runtime, which it downloads
from the GitHub release on first use, verifies against the SHA-256 digest the package records, and caches. `STARLING_CACHE_DIR` names the cache
directory, by default `starling-componentize` in the user's cache directory, and `STARLING_DEBUG_RUNTIME_URL` another
URL to download from. A package built from an unreleased version has no release to download from.

This package holds no native binary itself. npm installs the one for your platform from an optional dependency, and
`require('@bytecodealliance/starling-componentize').binaryPath()` returns its path. There are binaries for macOS
(arm64 and x64), Linux (arm64 and x64) and Windows (x64). The Linux binaries are statically linked, so they run with
any C library, such as glibc or musl.
