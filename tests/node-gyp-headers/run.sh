#!/usr/bin/env bash
# node-gyp header-cache end-to-end harness.
#
# nub seeds node-gyp's header cache from the Node it provisioned, so node-gyp
# compiles an addon without downloading `node-v<ver>-headers.tar.gz`
# (crates/nub-core/src/node/headers.rs). This proves the whole path in one
# falsifiable run: the fixture builds with node-gyp's download host pointed at a
# port nothing listens on, so a build that reaches the network FAILS instead of
# passing quietly.
#
# It also guards an assumption nub does not own: the cache layout node-gyp
# accepts (`<devdir>/<version>/include` plus an `installVersion` marker). A
# node-gyp that changes either would silently send every native build back to
# downloading, and this harness is what says so.
#
# Usage: tests/node-gyp-headers/run.sh <path-to-nub>
# Env:   NODE_VERSION=<x.y.z>  the version to provision (default below)
#
# Prerequisites: a C++ compiler and Python 3, as node-gyp itself requires.
set -euo pipefail

NUB_ARG="${1:?usage: run.sh <path-to-nub>}"
NUB="$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")"
NODE_VERSION="${NODE_VERSION:-26.8.2}"
DEAD_HOST="http://127.0.0.1:9"

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "ok: $*"; }

for tool in python3 make; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is required (node-gyp needs it)"
done
command -v c++ >/dev/null 2>&1 || command -v g++ >/dev/null 2>&1 || fail "a C++ compiler is required"

SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/nub-node-gyp-headers-XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT
DEVDIR="$SANDBOX/devdir"

# The seeding reads an installed Node, so provision it first. On a machine that
# has never seen this version, the install that provisions it still downloads
# headers once — that is the documented limit, not a failure.
"$NUB" node install "$NODE_VERSION" >"$SANDBOX/provision.log" 2>&1 ||
  fail "could not provision Node $NODE_VERSION: $(tail -3 "$SANDBOX/provision.log")"
pass "provisioned Node $NODE_VERSION"

# A local dependency with a binding.gyp: a real node-gyp compile, no registry.
APP="$SANDBOX/app"
mkdir -p "$APP/dep"
printf '{ "name": "dep", "version": "1.0.0-%s", "scripts": { "install": "node-gyp rebuild --verbose > \\"$PROBE_LOG\\" 2>&1" } }\n' "$$" > "$APP/dep/package.json"
printf '{"targets":[{"target_name":"probe","sources":["probe.cc"]}]}' > "$APP/dep/binding.gyp"
cat > "$APP/dep/probe.cc" <<'CC'
#include <node_api.h>
static napi_value Init(napi_env env, napi_value exports) { return exports; }
NAPI_MODULE(NODE_GYP_MODULE_NAME, Init)
CC
cat > "$APP/package.json" <<'JSON'
{
  "name": "node-gyp-headers-fixture",
  "version": "1.0.0",
  "private": true,
  "dependencies": { "dep": "file:./dep" },
  "allowScripts": { "dep@file:./dep": true }
}
JSON
echo "$NODE_VERSION" > "$APP/.nvmrc"

cd "$APP"
PROBE_LOG="$SANDBOX/node-gyp.log" \
  npm_config_devdir="$DEVDIR" \
  npm_config_disturl="$DEAD_HOST" \
  "$NUB" install >"$SANDBOX/install.log" 2>&1 ||
  fail "install failed with the header download unreachable: $(tail -5 "$SANDBOX/install.log")
node-gyp said: $(grep -m2 'http GET\|ECONNREFUSED\|gyp ERR' "$SANDBOX/node-gyp.log" 2>/dev/null)"

[ -f "$APP/node_modules/dep/build/Release/probe.node" ] || fail "no addon was built"
pass "the addon compiled without reaching nodejs.org"

[ -f "$DEVDIR/$NODE_VERSION/installVersion" ] ||
  fail "no cache entry at $DEVDIR/$NODE_VERSION"
[ -f "$DEVDIR/$NODE_VERSION/include/node/node_version.h" ] ||
  fail "the cache entry carries no headers"
pass "cache entry seeded (installVersion=$(cat "$DEVDIR/$NODE_VERSION/installVersion"))"

if grep -q 'http GET' "$SANDBOX/node-gyp.log"; then
  fail "node-gyp still tried to download: $(grep -m1 'http GET' "$SANDBOX/node-gyp.log")"
fi
grep -q 'version is good' "$SANDBOX/node-gyp.log" ||
  fail "node-gyp did not accept the seeded entry: $(grep -m2 'install\|node dir' "$SANDBOX/node-gyp.log" | tr '\n' ' ')"
pass "node-gyp read the seeded entry instead of downloading"

# The counterpart: nothing exports npm_config_nodedir, which would override a
# target that a caller — or a tool it spawns — selected for another runtime.
if grep -q -- '--nodedir dev files' "$SANDBOX/node-gyp.log"; then
  fail "a nodedir reached node-gyp: $(grep -m1 -- '--nodedir dev files' "$SANDBOX/node-gyp.log")"
fi
pass "no nodedir was exported"

echo "node-gyp header cache: all checks passed"
