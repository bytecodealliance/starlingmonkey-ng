#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception
#
# Build target/spin-app/: a Spin application of the apps in tests/express-apps, with one
# component per app. The Koa portal is served at `/...`, and each Express app at `/<app>/...`
# through its `spin.mjs`, which mounts the app at its route. The data app's upstream server is a
# private component, which the data app reaches at http://upstream.spin.internal.
#
# Each app's top level runs when it is componentized. The apps that write files at their top level
# get a writable directory at /data then, which becomes the component's /data mount, with the
# files they wrote. Spin copies the mounts, so the apps can write to them with
# `spin up --allow-transient-write`, and each `spin up` starts from the files the build wrote.
#
# Usage:
#   ./scripts/build-spin-app.sh
#   deps/spin-bin/spin up --from target/spin-app/spin.toml --allow-transient-write
#
# Uses target/wasm32-wasip3/release/starling.wasm, which `just build-runtime` builds, unless
# STARLING_RUNTIME names another runtime, and builds starling-componentize. Apps without
# node_modules are installed with `npm ci`, using deps/node-bin if it exists.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
APPS="$ROOT_DIR/tests/express-apps"
OUT="$ROOT_DIR/target/spin-app"
# The directory Cargo builds into: CARGO_TARGET_DIR, made absolute, or the repository's target/.
case "${CARGO_TARGET_DIR:-}" in
    "") TARGET_DIR="$ROOT_DIR/target" ;;
    /*) TARGET_DIR="$CARGO_TARGET_DIR" ;;
    *) TARGET_DIR="$PWD/$CARGO_TARGET_DIR" ;;
esac
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    export CARGO_TARGET_DIR="$TARGET_DIR"
fi
export STARLING_RUNTIME="${STARLING_RUNTIME:-$TARGET_DIR/wasm32-wasip3/release/starling.wasm}"

if [[ ! -f "$STARLING_RUNTIME" ]]; then
    echo "No runtime at $STARLING_RUNTIME. Build it with \`just build-runtime\`." >&2
    exit 1
fi
if [[ -x "$ROOT_DIR/deps/node-bin/bin/npm" ]]; then
    export PATH="$ROOT_DIR/deps/node-bin/bin:$PATH"
fi

# Cargo reads componentize/.cargo/config.toml, which puts the build in the repository's target/,
# only when it runs in that directory.
(cd "$ROOT_DIR/componentize" && cargo build --release)
COMPONENTIZE="$TARGET_DIR/release/starling-componentize"

for app in koa-portal rest-api ssr-site files auth data; do
    if [[ ! -d "$APPS/$app/node_modules" ]]; then
        (cd "$APPS/$app" && npm ci --no-audit --no-fund)
    fi
done

rm -rf "$OUT"
mkdir -p "$OUT"

# componentize NAME APP ENTRY [OPTION...]: componentize the app in tests/express-apps/APP with
# the entry ENTRY into NAME.wasm.
componentize() {
    local name="$1" app="$2" entry="$3"
    shift 3
    echo "Componentizing $name..."
    # WASI doesn't follow a symbolic link out of a preopened directory, so a linked
    # `node_modules` is preopened at its guest path.
    local modules="$APPS/$app/node_modules"
    if [[ -L "$modules" ]]; then
        set -- "$@" --init-dir "$(cd "$modules" && pwd -P)::/node_modules"
    fi
    "$COMPONENTIZE" componentize --serve --node-compat --init-env NODE_ENV=production "$@" \
        -p "$APPS/$app" -o "$OUT/$name.wasm" "$APPS/$app/$entry"
}

# with_data APP: the options that give APP's top level a writable /data, which is $OUT/APP/data.
with_data() {
    mkdir -p "$OUT/$1/data"
    echo --init-env DATA_DIR=/data --init-dir "$OUT/$1/data::/data"
}

componentize koa-portal koa-portal app.mjs
componentize rest-api rest-api spin.mjs
# EJS checks that an included template exists with a synchronous `fs.existsSync` on every render.
componentize ssr-site ssr-site spin.mjs --allow-blocking-host-io $(with_data ssr-site)
componentize files files spin.mjs $(with_data files)
componentize auth auth spin.mjs
componentize data data spin.mjs $(with_data data) --init-env UPSTREAM_URL=http://upstream.spin.internal
componentize upstream data upstream.mjs

mkdir -p "$OUT/koa-portal" "$OUT/ssr-site"
cp -R "$APPS/koa-portal/public" "$OUT/koa-portal/public"
cp -R "$APPS/ssr-site/public" "$APPS/ssr-site/views" "$OUT/ssr-site/"

cat > "$OUT/spin.toml" <<'EOF'
spin_manifest_version = 2

[application]
name = "starling-express-apps"
version = "0.1.0"
description = "The Koa portal and the Express apps of tests/express-apps on StarlingMonkey"

[[trigger.http]]
route = "/..."
component = "koa-portal"

[[trigger.http]]
route = "/rest-api/..."
component = "rest-api"

[[trigger.http]]
route = "/ssr-site/..."
component = "ssr-site"

[[trigger.http]]
route = "/files/..."
component = "files"

[[trigger.http]]
route = "/auth/..."
component = "auth"

[[trigger.http]]
route = "/data/..."
component = "data"

[[trigger.http]]
route = { private = true }
component = "upstream"

[component.koa-portal]
source = "koa-portal.wasm"
files = [{ source = "koa-portal/public", destination = "/public" }]
environment = { NODE_ENV = "production" }

[component.rest-api]
source = "rest-api.wasm"
environment = { NODE_ENV = "production" }

[component.ssr-site]
source = "ssr-site.wasm"
files = [
  { source = "ssr-site/views", destination = "/views" },
  { source = "ssr-site/public", destination = "/public" },
  { source = "ssr-site/data", destination = "/data" },
]
environment = { NODE_ENV = "production", DATA_DIR = "/data" }

[component.files]
source = "files.wasm"
files = [{ source = "files/data", destination = "/data" }]
environment = { NODE_ENV = "production", DATA_DIR = "/data" }

[component.auth]
source = "auth.wasm"
environment = { NODE_ENV = "production" }

[component.data]
source = "data.wasm"
files = [{ source = "data/data", destination = "/data" }]
environment = { NODE_ENV = "production", DATA_DIR = "/data" }
# The upstream component, and the ports nothing listens on that the app's error routes connect to.
allowed_outbound_hosts = ["http://upstream.spin.internal", "http://127.0.0.1:1", "http://127.0.0.1:9"]

[component.upstream]
source = "upstream.wasm"
EOF

echo "Built $OUT. Run it with:"
echo "  deps/spin-bin/spin up --from target/spin-app/spin.toml --allow-transient-write"
