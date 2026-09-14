#!/usr/bin/env bash
# Native-dependency builds under the approve-builds gate, end to end.
#
# A nub project decides which dependencies may run install scripts in
# package.json `allowScripts`, and nub runs exactly those, the way pnpm 12 runs
# `allowBuilds`. The fixture approves two classes of native build:
#
#   esbuild (0.28.0)         — a postinstall that checks for, or downloads, the
#                              platform-specific esbuild binary.
#   better-sqlite3 (11.10.0) — an install script that fetches a prebuilt N-API
#                              addon or compiles one with node-gyp.
#
# Every project gets its own HOME and XDG dirs, so its own store. Asserted:
#   1. approved builds run and both modules load;
#   2. a frozen install from the lockfile into a FRESH store runs them again —
#      the state a teammate's clone or a CI job starts from;
#   3. a build nobody decided about fails the install with
#      ERR_NUB_IGNORED_BUILDS, naming the package and `nub approve-builds`,
#      as pnpm 12 fails;
#   4. a build decided `false` is skipped and the install succeeds.
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

# in_sandbox <box> <cmd...> — run with HOME and every XDG dir inside <box>. A
# store shared between projects would already hold the first install's
# finished builds, and a later install would link them instead of building.
in_sandbox() {
  local box=$1
  shift
  mkdir -p "$box/home"
  env HOME="$box/home" XDG_DATA_HOME="$box/xdg/data" XDG_CACHE_HOME="$box/xdg/cache" \
    XDG_CONFIG_HOME="$box/xdg/config" XDG_STATE_HOME="$box/xdg/state" "$@"
}

# assert_nub_identity <output> <label> — a nub project reports the gate in
# nub's words.
assert_nub_identity() {
  if echo "$1" | grep -nE 'ERR_PNPM_|WARN_PNPM_|pnpm approve-builds'; then
    fail "pnpm's identity reached a nub project's output ($2)"
  fi
}

# assert_builds_ran <output> <label>
assert_builds_ran() {
  echo "$1" | grep -q 'node_modules/esbuild postinstall: Done' \
    || fail "esbuild's approved postinstall did not run ($2). Output: $1"
  echo "$1" | grep -q 'node_modules/better-sqlite3 install: Done' \
    || fail "better-sqlite3's approved install script did not run ($2). Output: $1"
}

# assert_loadable <project> <label>
#
# The status is captured apart from the assignment: a native addon that aborts
# (SIGABRT, exit 134) rather than throwing takes node down, and under `set -e` a
# failing command substitution ended run.sh with only the raw exit code and none
# of the output node wrote.
assert_loadable() {
  local load_rc=0 load_out
  load_out="$(cd "$1" && node ./verify-load.cjs 2>&1)" || load_rc=$?
  [ "$load_rc" -eq 0 ] \
    || fail "verify-load.cjs exited $load_rc in $2 (a signal death is 128+n, so 134=SIGABRT in a native addon). Output: $load_out"
  echo "$load_out" | grep -q "NATIVE-DEPS-OK" \
    || fail "native modules not loadable ($2). verify-load output: $load_out"
}

# ── 1. approved builds run ───────────────────────────────────────────────────
echo "── approved native builds ───────────────────────────────────────────────"
PROJ_ALLOW="$SANDBOX_ROOT/approved"
mkdir -p "$PROJ_ALLOW"
# Only the fixture files, not the harness scripts.
cp "$HERE/package.json" "$HERE/verify-load.cjs" "$PROJ_ALLOW/"
rm -rf "$PROJ_ALLOW/node_modules" "$PROJ_ALLOW/nub.lock"

install_rc=0
install_out="$(cd "$PROJ_ALLOW" && in_sandbox "$SANDBOX_ROOT/approved-home" "$NUB" install 2>&1)" || install_rc=$?
[ "$install_rc" -eq 0 ] || fail "install with every build approved exited $install_rc. Output: $install_out"
assert_nub_identity "$install_out" "approved install"
assert_builds_ran "$install_out" "approved install"
assert_loadable "$PROJ_ALLOW" "approved install"
pass "approved builds ran and both modules load"

# ── 2. a frozen install into a fresh store runs them again ───────────────────
echo ""
echo "── frozen install into a fresh store ────────────────────────────────────"
[ -f "$PROJ_ALLOW/nub.lock" ] || fail "the approved install wrote no nub.lock"
PROJ_FROZEN="$SANDBOX_ROOT/frozen-clone"
mkdir -p "$PROJ_FROZEN"
cp "$PROJ_ALLOW/package.json" "$PROJ_ALLOW/verify-load.cjs" "$PROJ_ALLOW/nub.lock" "$PROJ_FROZEN/"

frozen_rc=0
frozen_out="$(cd "$PROJ_FROZEN" && in_sandbox "$SANDBOX_ROOT/frozen-home" "$NUB" install --frozen-lockfile 2>&1)" || frozen_rc=$?
[ "$frozen_rc" -eq 0 ] || fail "frozen install exited $frozen_rc. Output: $frozen_out"
assert_nub_identity "$frozen_out" "frozen install"
assert_builds_ran "$frozen_out" "frozen install"
assert_loadable "$PROJ_FROZEN" "frozen install"
pass "frozen install into a fresh store ran the approved builds again"

# ── 3. a build nobody decided about fails the install ────────────────────────
echo ""
echo "── undecided build ──────────────────────────────────────────────────────"
PROJ_UNDECIDED="$SANDBOX_ROOT/undecided"
mkdir -p "$PROJ_UNDECIDED"
cat > "$PROJ_UNDECIDED/package.json" <<'JSON'
{
  "name": "native-deps-undecided-fixture",
  "private": true,
  "dependencies": { "core-js": "3.40.0" }
}
JSON

undecided_rc=0
undecided_out="$(cd "$PROJ_UNDECIDED" && in_sandbox "$SANDBOX_ROOT/undecided-home" "$NUB" install 2>&1)" || undecided_rc=$?
[ "$undecided_rc" -ne 0 ] || fail "an install with an undecided build script exited 0, where pnpm 12 fails. Output: $undecided_out"
echo "$undecided_out" | grep -q 'ERR_NUB_IGNORED_BUILDS' \
  || fail "the undecided build did not fail with ERR_NUB_IGNORED_BUILDS. Output: $undecided_out"
echo "$undecided_out" | grep -q 'core-js@3.40.0' \
  || fail "core-js is not named in the ignored-builds error. Output: $undecided_out"
echo "$undecided_out" | grep -q 'Run "nub approve-builds"' \
  || fail "the ignored-builds error does not point at nub approve-builds. Output: $undecided_out"
assert_nub_identity "$undecided_out" "undecided build"
pass "an undecided build fails the install, named, with the approve-builds hint"

# ── 4. a build decided false is skipped ──────────────────────────────────────
echo ""
echo "── denied build ─────────────────────────────────────────────────────────"
PROJ_DENIED="$SANDBOX_ROOT/denied"
mkdir -p "$PROJ_DENIED"
cat > "$PROJ_DENIED/package.json" <<'JSON'
{
  "name": "native-deps-denied-fixture",
  "private": true,
  "dependencies": { "core-js": "3.40.0" },
  "allowScripts": { "core-js": false }
}
JSON

denied_rc=0
denied_out="$(cd "$PROJ_DENIED" && in_sandbox "$SANDBOX_ROOT/denied-home" "$NUB" install 2>&1)" || denied_rc=$?
[ "$denied_rc" -eq 0 ] || fail "an install whose only build is decided false exited $denied_rc. Output: $denied_out"
if echo "$denied_out" | grep -q 'node_modules/core-js postinstall'; then
  fail "core-js's build ran although it is decided false. Output: $denied_out"
fi
assert_nub_identity "$denied_out" "denied build"
pass "a build decided false is skipped and the install succeeds"

echo ""
echo "native-deps: all assertions passed."
