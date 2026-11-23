#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Smoke-test the tarballs `componentize/npm/package.mjs --pack` wrote: unpack this platform's
# packages as a scratch project's node_modules, and check that the installed command runs with the
# bundled release runtime by default, the one `STARLING_RUNTIME` names, and with
# `STARLING_RUNTIME_BUILD=debug` the debug runtime, which it downloads, verifies and caches.
#
# Usage: scripts/check-npm-packages.sh <tarballs dir> <debug starling.wasm>
#
# The debug runtime is served from a local server in place of the GitHub release.
set -euo pipefail

tarballs=${1:?usage: check-npm-packages.sh <tarballs dir> <debug starling.wasm>}
debug_runtime=${2:?usage: check-npm-packages.sh <tarballs dir> <debug starling.wasm>}
platform=$(node -p 'process.platform + "-" + process.arch')
scratch=$(mktemp -d)
server=
trap '[ -z "$server" ] || kill "$server"; rm -rf "$scratch"' EXIT

# `npm install` would fetch the other platforms' packages from the registry, so unpack instead.
for tarball in "$tarballs"/*.tgz; do
  case "$tarball" in
    *"-$platform-"*) ;;
    *-darwin-* | *-linux-* | *-win32-*) continue ;;
  esac
  name=$(tar -xOzf "$tarball" package/package.json | node -p 'JSON.parse(require("fs").readFileSync(0, "utf8")).name')
  mkdir -p "$scratch/node_modules/$name"
  tar -xzf "$tarball" -C "$scratch/node_modules/$name" --strip-components=1
done

pkg="$scratch/node_modules/@bytecodealliance/starling-componentize"
version() {
  env -u STARLING_RUNTIME -u STARLING_RUNTIME_BUILD STARLING_CACHE_DIR="$scratch/cache" "$@" \
    node "$pkg/bin/starling-componentize.js" --version
}
fail() { echo "$*" >&2; exit 1; }

default=$(version)
grep -q 'runtime sha256:' <<<"$default" || fail "no runtime by default: $default"
[ "$default" = "$(version STARLING_RUNTIME="$pkg/starling.wasm")" ] \
  || fail "the default is not the bundled release runtime"
! version STARLING_RUNTIME="$scratch/nonexistent.wasm" >/dev/null 2>&1 || fail "STARLING_RUNTIME is not honored"

# Serve the release runtime as `bad.wasm` and the debug one as `debug.wasm`.
mkdir "$scratch/www"
cp "$pkg/starling.wasm" "$scratch/www/bad.wasm"
cp "$debug_runtime" "$scratch/www/debug.wasm"
port_file="$scratch/port"
node -e '
  const http = require("http"), fs = require("fs"), path = require("path");
  const server = http.createServer((req, res) => {
    fs.readFile(path.join(process.argv[1], path.basename(req.url)), (err, data) => {
      res.writeHead(err ? 404 : 200);
      res.end(err ? "" : data);
    });
  });
  server.listen(0, "127.0.0.1", () => fs.writeFileSync(process.argv[2], String(server.address().port)));
' "$scratch/www" "$port_file" &
server=$!
for _ in $(seq 50); do [ -s "$port_file" ] && break; sleep 0.1; done
[ -s "$port_file" ] || fail "the debug runtime server did not start"
url="http://127.0.0.1:$(cat "$port_file")"
curl -fsS -o /dev/null "$url/debug.wasm" || fail "the debug runtime server does not serve debug.wasm"

# `--runtime` takes precedence, so no debug runtime is downloaded, and neither is one for help.
no_download() {
  env -u STARLING_RUNTIME STARLING_RUNTIME_BUILD=debug STARLING_DEBUG_RUNTIME_URL="$url/gone.wasm" \
    STARLING_CACHE_DIR="$scratch/cache" node "$pkg/bin/starling-componentize.js" "$@" >/dev/null
}
no_download imports --export-cli --runtime "$pkg/starling.wasm" || fail "--runtime does not take precedence over the debug download"
no_download imports --export-cli --runtime="$pkg/starling.wasm" || fail "--runtime= does not take precedence over the debug download"
no_download --help || fail "--help downloads the debug runtime"
[ ! -e "$scratch/cache" ] || fail "the debug runtime was cached before it was asked for"

# Fails with the digest's error, not for any other reason.
if bad=$(version STARLING_RUNTIME_BUILD=debug STARLING_DEBUG_RUNTIME_URL="$url/bad.wasm" 2>&1); then
  fail "a debug runtime with the wrong digest was accepted"
fi
grep -q 'has SHA-256' <<<"$bad" || fail "a wrong digest failed for another reason: $bad"

debug=$(version STARLING_RUNTIME_BUILD=debug STARLING_DEBUG_RUNTIME_URL="$url/debug.wasm")
grep -q 'runtime sha256:' <<<"$debug" || fail "no debug runtime: $debug"
[ "$debug" != "$default" ] || fail "STARLING_RUNTIME_BUILD=debug does not pick the debug runtime"

# A cached file that no longer has the digest is downloaded again.
cached=$(ls "$scratch"/cache/starling-debug-*.wasm)
[ -z "$(ls "$scratch"/cache/*.tmp 2>/dev/null)" ] || fail "a temporary file was left in the cache"
printf 'truncated' >"$cached"
[ "$debug" = "$(version STARLING_RUNTIME_BUILD=debug STARLING_DEBUG_RUNTIME_URL="$url/debug.wasm")" ] \
  || fail "a corrupt cached debug runtime is not replaced"

# The next use takes the cached copy, so it works with the server gone.
kill "$server"
wait "$server" 2>/dev/null || true
server=
[ "$debug" = "$(version STARLING_RUNTIME_BUILD=debug STARLING_DEBUG_RUNTIME_URL="$url/gone.wasm")" ] \
  || fail "the debug runtime is not cached"
echo ok
