#!/usr/bin/env bash
# Real-app daily-loop smoke harness.
#
# Exercises the complete nub surface against a real Vite + React + TypeScript
# project — the scenarios that unit tests and synthetic fixtures miss:
#
#   install   nub installs a pnpm project (real lockfile, real registry)
#   type-check  `nub run type-check` → tsc --noEmit on real TS source
#   build     `nub run build` → Vite build (TS + JSX → dist/)
#   test      `nub run test` → vitest run (real test runner)
#   ts-run    `nub <file.ts>` with a real node_modules import (kleur)
#   node-off  `nub --node <file.ts>` must FAIL (no transpile augmentation)
#
# Usage: tests/daily-driver/run.sh <path-to-nub>
# Env:   FIXTURE=<dir>      use an existing pre-installed fixture
#        KEEP=1             keep the sandbox on success
#        SANDBOX_ROOT=<dir> pin the sandbox directory (implies KEEP)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NUB_ARG="${1:?usage: run.sh <path-to-nub>}"
NUB="$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")"
{ [ -x "$NUB" ] || ! [ -x "$NUB.exe" ]; } || NUB="$NUB.exe"
[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }

KEEP="${KEEP:-0}"
CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-daily-driver.XXXXXX")"
  CREATED_SANDBOX=1
fi

cleanup() {
  local code=$?
  if [ "$CREATED_SANDBOX" -eq 1 ] && [ "$KEEP" = "0" ] && [ "$code" -eq 0 ]; then
    rm -rf "$SANDBOX_ROOT"
  elif [ "$code" -ne 0 ]; then
    echo "(sandbox preserved for inspection at $SANDBOX_ROOT)"
  fi
}
trap cleanup EXIT

fail() { echo "FAIL [$1]: $2"; exit 1; }
pass() { echo "ok: $1"; }

# Sandbox user dirs.
export HOME="$SANDBOX_ROOT/home"
export XDG_CACHE_HOME="$SANDBOX_ROOT/xdg/cache"
export XDG_DATA_HOME="$SANDBOX_ROOT/xdg/data"
export XDG_CONFIG_HOME="$SANDBOX_ROOT/xdg/config"
export XDG_STATE_HOME="$SANDBOX_ROOT/xdg/state"
mkdir -p "$HOME"

# Build or reuse fixture.
FIXTURE="${FIXTURE:-}"
if [ -z "$FIXTURE" ]; then
  FIXTURE="$SANDBOX_ROOT/fixture"
  "$HERE/make-fixture.sh" "$FIXTURE"
fi
[ -f "$FIXTURE/package.json" ] || { echo "error: fixture not found at $FIXTURE" >&2; exit 2; }
cd "$FIXTURE"

# ── 1. Install ────────────────────────────────────────────────────────────────
echo "── install ───────────────────────────────────────────────────────────────"
install_out="$("$NUB" install 2>&1)" \
  || fail install "nub install exited nonzero. Output: $install_out"
[ -d node_modules/vite ] || fail install "node_modules/vite not present after install. Output: $install_out"
[ -d node_modules/react ] || fail install "node_modules/react not present after install. Output: $install_out"
# The fixture pins the version nub embeds, so the engine runs IN-PROCESS under
# pnpm's own identity — pnpm's codes and links are correct here and nub's must
# not leak in. A pin naming any OTHER pnpm version is a different path entirely:
# nub provisions that pnpm and delegates the command to it.
echo "$install_out" | grep -q 'using pnpm v' \
  || fail install "the install summary does not name pnpm. Output: $install_out"
if echo "$install_out" | grep -qE 'ERR_NUB_|WARN_NUB_'; then
  echo "$install_out"
  fail install "nub's identity reached a pnpm project's output"
fi
pass "install (pnpm project, real registry)"

# ── 2. Type-check ─────────────────────────────────────────────────────────────
echo "── type-check ────────────────────────────────────────────────────────────"
tc_out="$("$NUB" run type-check 2>&1)" \
  || fail type-check "tsc --noEmit failed. Output: $tc_out"
pass "type-check (tsc --noEmit clean)"

# ── 3. Build ──────────────────────────────────────────────────────────────────
echo "── build ─────────────────────────────────────────────────────────────────"
build_out="$("$NUB" run build 2>&1)" \
  || fail build "Vite build failed. Output: $build_out"
[ -d dist ] || fail build "dist/ not produced by Vite build. Output: $build_out"
pass "build (Vite → dist/)"

# ── 4. Test ───────────────────────────────────────────────────────────────────
echo "── test ──────────────────────────────────────────────────────────────────"
test_out="$("$NUB" run test 2>&1)" \
  || fail test "vitest run failed. Output: $test_out"
echo "$test_out" | grep -qiE '(pass|✓|passed)' \
  || fail test "vitest output does not indicate passing tests: $test_out"
pass "test (vitest run)"

# ── 5. nub <file.ts> — transpile + real node_modules import ──────────────────
echo "── ts-run ────────────────────────────────────────────────────────────────"
ts_out="$("$NUB" run-kleur.ts 2>&1)" \
  || fail ts-run "nub run-kleur.ts failed. Output: $ts_out"
echo "$ts_out" | grep -q "DAILY-TS-OK" \
  || fail ts-run "expected DAILY-TS-OK in output: $ts_out"
pass "ts-run (nub <file.ts> with real node_modules import)"

# ── 6. nub --node disables transpilation ─────────────────────────────────────
# run-kleur.ts uses `const enum` — non-erasable TypeScript syntax. Node's own
# strip-only mode (Node 22.6+) handles plain type annotations but rejects const
# enums with ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX. `nub` handles it; `nub --node`
# must not, proving the transpile augmentation is off. This is a better
# negative control than bare type annotations because Node 22.6+ erases those
# natively (making them pass under both nub and nub --node).
echo "── node-off ──────────────────────────────────────────────────────────────"
node_off_out="$("$NUB" --node run-kleur.ts 2>&1)" && \
  fail node-off "expected nub --node to fail on non-erasable TS (const enum); it succeeded: $node_off_out"
echo "$node_off_out" | grep -qiE '(ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX|SyntaxError|Unexpected token|const enum)' \
  || fail node-off "nub --node failed but not with an unsupported-TS error: $node_off_out"
pass "node-off (nub --node rejects const enum → augmented transpile disabled)"

echo ""
echo "daily-driver: all scenarios passed."
