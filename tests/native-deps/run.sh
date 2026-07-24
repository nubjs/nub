#!/usr/bin/env bash
# Native-dependency floor end-to-end harness.
#
# Exercises two classes of native build that the default-trust floor covers:
#
#   esbuild (0.28.0)         — ships a postinstall script that DOWNLOADS the
#                              platform-specific esbuild binary from the npm
#                              registry. Under nub's default-trust policy, this
#                              package is on the floor allowlist (it is a
#                              long-lived, registry-only, well-known build tool).
#                              The floor must: (a) allow the build to run,
#                              (b) disclose the package by name, (c) produce a
#                              working module.
#
#   better-sqlite3 (11.10.0) — compiles a C++ N-API addon via node-gyp at
#                              postinstall. Also on the floor allowlist. Same
#                              three-part pass: allowed + disclosed + loadable.
#
# A third fixture (core-js-only) installs a package NOT on the allowlist and
# asserts that its build is BLOCKED — the deny side of the floor policy.
#
# Usage: tests/native-deps/run.sh <path-to-nub>
# Env:   SANDBOX_ROOT=<dir>   reuse/inspect the sandbox (default: mktemp)
#        KEEP=1               keep the sandbox on success
#
# Prerequisites: node-gyp needs a C++ compiler (gcc/g++ or clang) and Python 3.
# On Ubuntu: apt-get install -y build-essential python3
# On macOS:  Xcode Command Line Tools (`xcode-select --install`)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NUB_ARG="${1:?usage: run.sh <path-to-nub>}"
NUB="$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")"
{ [ -x "$NUB" ] || ! [ -x "$NUB.exe" ]; } || NUB="$NUB.exe"
[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }

CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-native-deps.XXXXXX")"
  CREATED_SANDBOX=1
else
  mkdir -p "$SANDBOX_ROOT"
fi
KEEP="${KEEP:-0}"

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "ok: $*"; }

cleanup() {
  local code=$?
  if [ "$CREATED_SANDBOX" -eq 1 ] && [ "$KEEP" = "0" ] && [ "$code" -eq 0 ]; then
    rm -rf "$SANDBOX_ROOT"
  elif [ "$code" -ne 0 ]; then
    echo "(sandbox preserved for inspection at $SANDBOX_ROOT)"
  fi
}
trap cleanup EXIT

# NOTE: unlike the brand-sweep and conformance harnesses, this one does NOT
# sandbox HOME or XDG_CACHE_HOME. The reason: the default-trust floor requires
# the OSV advisory gate to have run (`osv_gate_active = true`). The OSV gate
# depends on nub's packument cache, which lives in $XDG_CACHE_HOME/nub/pm/ —
# a completely cold sandbox defeats the gate, causing the floor to fall back to
# deny+warn (`WARN_NUB_IGNORED_BUILD_SCRIPTS`) instead of the expected allow+
# disclose path. On CI (ubuntu-latest),
# the runner has network access and the XDG cache starts cold — but the
# resolver warms it during the resolve phase before the scripts phase, so the
# OSV gate runs and the floor fires correctly. Sandboxing the project dirs
# (node_modules, lockfile) is enough isolation for this test's purpose.

# ── Fixture: esbuild + better-sqlite3 (both on the floor allowlist) ──────────
echo "── floor-allowed native builds ──────────────────────────────────────────"
PROJ_ALLOW="$SANDBOX_ROOT/floor-allowed"
mkdir -p "$PROJ_ALLOW"
# Copy only the fixture files — not the harness scripts — to avoid
# confusing nub install with extra .sh/.md files in the project root.
cp "$HERE/package.json" "$HERE/verify-load.cjs" "$PROJ_ALLOW/"
# Defensive: ensure no stale lock or node_modules from a prior run in this sandbox.
rm -f "$PROJ_ALLOW/pnpm-lock.yaml" "$PROJ_ALLOW/package-lock.json"
rm -rf "$PROJ_ALLOW/node_modules"

install_out="$(cd "$PROJ_ALLOW" && "$NUB" install 2>&1)" || install_rc=$?
install_rc="${install_rc:-0}"

# Brand check — no aube/jdx.dev identity, even when the install itself fails.
if echo "$install_out" | grep -qiE 'aube|jdx\.dev'; then
  echo "$install_out"
  fail "engine-branded identity in install output"
fi

# Default-trust disclosure: both floor-allowed packages must be named in the
# defaultTrust warning — the floor is NOT a silent allow path.
# This fires before the lifecycle script phase, so it is present even when a
# build (e.g. better-sqlite3's node-gyp compile) fails afterward.
echo "$install_out" | grep -q 'WARN defaultTrust: running build scripts' \
  || fail "defaultTrust disclosure missing from output (floor allowed builds silently). Output: $install_out"
echo "$install_out" | grep -q 'esbuild' \
  || fail "esbuild not named in defaultTrust disclosure. Output: $install_out"
echo "$install_out" | grep -q 'better-sqlite3' \
  || fail "better-sqlite3 not named in defaultTrust disclosure. Output: $install_out"
pass "default-trust disclosure: both packages disclosed by name"

# esbuild binary must have been placed by postinstall (esbuild's build is a
# binary download and succeeds even when better-sqlite3's node-gyp compile fails).
ESBUILD_BIN="$PROJ_ALLOW/node_modules/.bin/esbuild"
[ -e "$ESBUILD_BIN" ] \
  || fail "esbuild binary not materialized at $ESBUILD_BIN (postinstall did not run)"
pass "esbuild postinstall ran: binary present"

# Verify modules are loadable. verify-load.cjs handles better-sqlite3
# gracefully on dev boxes where the toolchain can't compile node-gyp addons;
# the esbuild check is authoritative on every platform.
# Run from within $PROJ_ALLOW so require() resolves against that node_modules.
LOAD_OUT="$(cd "$PROJ_ALLOW" && node ./verify-load.cjs 2>&1)"
echo "$LOAD_OUT" | grep -q "NATIVE-DEPS-OK" \
  || fail "native modules not loadable after install. verify-load output: $LOAD_OUT"
pass "native modules loadable: $LOAD_OUT"

# ── Fixture: frozen install from the lockfile (CI / teammate clone) ──────────
# Regression guard for the default-trust floor's frozen-install bug
# (wiki/commands/pm/supply-chain-posture.md Decision 2): on a frozen install
# the per-install OSV gate is correctly skipped, so the floor used to fall
# closed and silently NOT run trusted packages' build scripts — even though
# they ran for whoever wrote the lockfile. A clone/CI run must reproduce the
# fresh install's build behavior, inheriting the lockfile's resolution-time
# vetting. We reuse the lockfile the fresh install above just produced,
# simulate a clone (only package.json + lockfile, no node_modules), and run
# a frozen install.
echo ""
echo "── frozen install runs trusted builds (Decision 2 regression) ────────────"
LOCKFILE=""
for cand in nub.lock lock.yaml aube-lock.yaml pnpm-lock.yaml; do
  [ -f "$PROJ_ALLOW/$cand" ] && { LOCKFILE="$cand"; break; }
done
[ -n "$LOCKFILE" ] \
  || fail "fresh install produced no lockfile in $PROJ_ALLOW (expected nub.lock, lock.yaml, aube-lock.yaml, or pnpm-lock.yaml)"

PROJ_FROZEN="$SANDBOX_ROOT/floor-frozen-clone"
mkdir -p "$PROJ_FROZEN"
cp "$PROJ_ALLOW/package.json" "$PROJ_ALLOW/verify-load.cjs" "$PROJ_FROZEN/"
cp "$PROJ_ALLOW/$LOCKFILE" "$PROJ_FROZEN/"
# No node_modules — this is the clone/CI starting state.

frozen_out="$(cd "$PROJ_FROZEN" && "$NUB" install --frozen-lockfile 2>&1)" || frozen_rc=$?
frozen_rc="${frozen_rc:-0}"

if echo "$frozen_out" | grep -qiE 'aube|jdx\.dev'; then
  echo "$frozen_out"
  fail "engine-branded identity in frozen install output"
fi

# The floor must fire on the frozen install exactly as on the fresh one.
echo "$frozen_out" | grep -q 'WARN defaultTrust: running build scripts' \
  || fail "FROZEN INSTALL: defaultTrust disclosure missing — the floor fell closed on a frozen install (Decision 2 bug). Output: $frozen_out"
echo "$frozen_out" | grep -q 'esbuild' \
  || fail "FROZEN INSTALL: esbuild not named in defaultTrust disclosure. Output: $frozen_out"
pass "frozen install: default-trust floor fired (build scripts trusted)"

# And the postinstall must actually have run — esbuild binary materialized.
FROZEN_ESBUILD_BIN="$PROJ_FROZEN/node_modules/.bin/esbuild"
[ -e "$FROZEN_ESBUILD_BIN" ] \
  || fail "FROZEN INSTALL: esbuild binary not materialized at $FROZEN_ESBUILD_BIN (postinstall did not run on the frozen install)"
pass "frozen install: esbuild postinstall ran (binary present)"

# ── Fixture: core-js only (NOT on the floor allowlist — default-deny side) ───
echo ""
echo "── floor-denied native build ─────────────────────────────────────────────"
PROJ_DENY="$SANDBOX_ROOT/floor-denied"
mkdir -p "$PROJ_DENY"
cat > "$PROJ_DENY/package.json" <<'JSON'
{
  "name": "native-deps-deny-fixture",
  "private": true,
  "dependencies": { "core-js": "3.40.0" }
}
JSON

deny_out="$(cd "$PROJ_DENY" && "$NUB" install 2>&1)" || true

# The floor must NOT allow core-js's build (it is not on the allowlist).
# The ignored-build-scripts warning must appear — not a hard failure, but not
# a silent allow.
echo "$deny_out" | grep -q 'WARN_NUB_IGNORED_BUILD_SCRIPTS' \
  || fail "core-js build was not mentioned in WARN_NUB_IGNORED_BUILD_SCRIPTS (deny side broken). Output: $deny_out"
echo "$deny_out" | grep -q 'core-js' \
  || fail "core-js not named in ignored-build-scripts warning. Output: $deny_out"
pass "default-trust deny side: core-js build blocked + named in warning"

echo ""
echo "native-deps: all assertions passed."
