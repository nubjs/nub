#!/bin/sh
# Official-image smoke test — runs INSIDE a built nub image where `nub` is already
# on PATH via `npm install -g @nubjs/nub`. Verifies the image is usable end-to-end:
#
#   1. nub --version returns a semver string (binary is real, not a stub).
#   2. nub <file.ts> transpiles + runs TypeScript on the in-image Node.
#   3. nub install materializes a real dep and the module is require()-loadable.
#
# This is the image counterpart of tests/docker-smoke/smoke.sh (which tests a
# from-source binary). Here we test the SHIPPED npm path on the chosen Node base.
#
# Usage: docker run --rm nub:slim /usr/local/bin/smoke.sh
set -eu

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "ok: $*"; }

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

# ── 1. version ────────────────────────────────────────────────────────────────
ver="$(nub --version 2>/dev/null)"
echo "$ver" | grep -qE '^v[0-9]+\.[0-9]+\.[0-9]+$' || fail "--version returned non-semver: '$ver'"
pass "nub --version: $ver"

# ── 2. TypeScript run ─────────────────────────────────────────────────────────
cat > "$SANDBOX/hello.ts" <<'TS'
const greet = (name: string): string => `TS-SMOKE-OK ${name}`;
console.log(greet("nub"));
TS
out="$(nub "$SANDBOX/hello.ts" 2>&1)"
echo "$out" | grep -q "TS-SMOKE-OK nub" || fail "TS execution failed: $out"
pass "TypeScript run: $out"

# ── 3. PM install + module load ───────────────────────────────────────────────
mkdir -p "$SANDBOX/pm"
cat > "$SANDBOX/pm/package.json" <<'JSON'
{ "name": "smoke-pm", "private": true, "dependencies": { "kleur": "4.1.5" } }
JSON
install_out="$(cd "$SANDBOX/pm" && nub install 2>&1)"
[ -e "$SANDBOX/pm/node_modules/kleur" ] || fail "nub install did not materialize kleur. Output: $install_out"
load_out="$(cd "$SANDBOX/pm" && node -e "const k=require('kleur'); console.log('PM-SMOKE-OK', typeof k.red)" 2>&1)"
echo "$load_out" | grep -q "PM-SMOKE-OK function" || fail "installed module not loadable: $load_out"
pass "nub install + module load: kleur materialized and require()-loadable"

echo "ALL SMOKE CHECKS PASSED"
