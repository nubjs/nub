#!/bin/bash
# Build a reproducible Yarn 4 Plug'n'Play fixture for exercising nub's PnP support.
#
# Why a real install (not a hand-rolled .pnp.cjs): PnP's behavior — the
# `_resolveFilename` patch, the `pnpapi` builtin, zip-stored packages, the
# conditions-rejection quirk, the ESM format-detection corner — only reproduces
# against a genuine `yarn install`. The whole point of this fixture is to test
# against the real thing. See wiki/research/pnp-preload-feasibility.md.
#
# The fixture deliberately spans the three dependency shapes that resolve through
# different Node code paths, because they fail independently:
#   - lodash  — CJS package, `require()`d AND `import`ed (CJS-from-ESM path)
#   - chalk@5 — pure ESM package (`"type":"module"`), zip-stored: the case that
#               needs the resolve hook to emit an explicit `format` or Node ≤20.11
#               mis-detects it as CJS → ERR_REQUIRE_ESM
#   - cowsay  — CJS package shipping a bin, zip-stored: exercises `nubx`
#
# Usage: tests/pnp/make-fixture.sh [dest-dir]   (default: /tmp/nub-pnp-fixture)
set -euo pipefail

DEST="${1:-/tmp/nub-pnp-fixture}"

if ! command -v corepack >/dev/null 2>&1; then
  echo "error: corepack not found (needed to run Yarn 4). Install Node 18.19+." >&2
  exit 1
fi

rm -rf "$DEST"
mkdir -p "$DEST"
cd "$DEST"

# Pin Yarn 4 (berry) via corepack and force PnP linking.
corepack enable >/dev/null 2>&1 || true
cat > package.json <<'JSON'
{
  "name": "nub-pnp-fixture",
  "packageManager": "yarn@4.9.1",
  "scripts": { "start": "node script-runner.cjs" }
}
JSON
cat > .yarnrc.yml <<'YML'
nodeLinker: pnp
enableGlobalCache: true
YML

corepack yarn add lodash chalk@5 cowsay >/dev/null 2>&1

# Test entries, one per resolution shape.
cat > cjs-test.cjs <<'JS'
const _ = require("lodash");
console.log("CJS-OK", _.capitalize("pnp works"));
JS
cat > esm-test.mjs <<'JS'
import _ from "lodash"; // CJS dep imported from ESM (CJS-from-ESM path)
console.log("ESM-OK", _.capitalize("pnp works"));
JS
cat > esm-pure.mjs <<'JS'
import chalk from "chalk"; // pure-ESM dep, zip-stored
console.log("PURE-ESM-OK", typeof chalk.green);
JS
cat > ts-test.ts <<'TS'
import _ from "lodash"; // TS entry (nub transpiles) importing a PnP dep
const msg: string = _.capitalize("ts pnp works");
console.log("TS-OK", msg);
TS
cat > script-runner.cjs <<'JS'
const _ = require("lodash");
console.log("SCRIPT-OK", _.kebabCase("Pnp Script Runner"));
JS

echo "PnP fixture ready at $DEST (yarn $(corepack yarn --version), .pnp.cjs $( [ -f .pnp.cjs ] && echo present || echo MISSING ))"
