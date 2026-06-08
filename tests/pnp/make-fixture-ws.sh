#!/bin/bash
# Build a reproducible Yarn 4 PnP *workspace* (monorepo) fixture — guards the cases a
# single-package fixture can't: workspace-sibling resolution, running from a member
# subdir (so `.pnp.cjs` is at an ancestor, exercising the cwd walk), and `nubx` bin
# resolution from a member (which must enumerate the MEMBER's deps, not the workspace
# root's — `yarn bin` from a member lists the member's dep bins).
#
# Layout: root (workspaces) + packages/a (depends on the sibling pkg-b + lodash, has a
# cowsay devDep bin) + packages/b (a library that also ships a bin). Scenarios run
# from packages/a. Usage: tests/pnp/make-fixture-ws.sh [dest]  (default /tmp/nub-pnp-ws)
set -euo pipefail

DEST="${1:-/tmp/nub-pnp-ws}"
command -v corepack >/dev/null 2>&1 || { echo "error: corepack not found (need Node 18.19+)" >&2; exit 1; }

rm -rf "$DEST"; mkdir -p "$DEST/packages/a" "$DEST/packages/b"
cd "$DEST"
corepack enable >/dev/null 2>&1 || true

cat > package.json <<'JSON'
{ "name": "ws-root", "private": true, "packageManager": "yarn@4.9.1",
  "workspaces": ["packages/*"] }
JSON
cat > .yarnrc.yml <<'YML'
nodeLinker: pnp
enableGlobalCache: true
YML

# pkg-b: workspace library + a bin.
cat > packages/b/package.json <<'JSON'
{ "name": "pkg-b", "version": "1.0.0", "main": "./index.js",
  "bin": { "pkg-b-bin": "./cli.js" } }
JSON
echo 'exports.hello = () => "from-pkg-b";' > packages/b/index.js
printf '#!/usr/bin/env node\nconsole.log("WS-SIBLING-BIN-OK");\n' > packages/b/cli.js

# pkg-a: depends on the sibling + an external lib, with its own devDep bin (cowsay).
cat > packages/a/package.json <<'JSON'
{ "name": "pkg-a", "version": "1.0.0",
  "dependencies": { "pkg-b": "workspace:*", "lodash": "*" },
  "devDependencies": { "cowsay": "*" } }
JSON
cat > packages/a/ws-esm.mjs <<'JS'
import { hello } from "pkg-b"; // workspace sibling (lexer-detectable named export)
import _ from "lodash";        // external dep of this member
console.log("WS-ESM-OK", hello(), _.capitalize("ok"));
JS
cat > packages/a/ws-cjs.cjs <<'JS'
const { hello } = require("pkg-b");
const _ = require("lodash");
console.log("WS-CJS-OK", hello(), _.capitalize("ok"));
JS

corepack yarn install >/dev/null 2>&1
echo "WS PnP fixture ready at $DEST (member dir: $DEST/packages/a, .pnp.cjs $( [ -f .pnp.cjs ] && echo present || echo MISSING ))"
