#!/usr/bin/env bash
# Compile a corpus of real npm packages, then run each artifact with node_modules
# DELETED and compare against the same program on plain Node.
#
# The comparison is the point. A compiled binary that prints nothing, or crashes
# instantly, looks indistinguishable from a fast one unless its output is checked
# against a control — so every fixture is run on plain Node first and the artifact
# must reproduce that byte for byte.
#
# Usage: NUB=/path/to/nub tests/compile-corpus/run.sh [workdir]
set -uo pipefail

NUB="${NUB:?set NUB to the nub binary under test}"
WORK="${1:-${TMPDIR:-/tmp}/nub-compile-corpus}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$WORK" && cd "$WORK"
# compile refuses to guess a Node version, by design — a compiled binary's runtime
# must be intentional and reproducible. Pin it here so the corpus is too.
: "${NODE_PIN:=26.5.0}"
printf '%s\n' "$NODE_PIN" > .node-version
if [ ! -d node_modules ]; then
  npm init -y >/dev/null 2>&1
  # One tree, many entries: each fixture imports only what it needs, so detection
  # still sees exactly the packages that program reaches.
  npm i --no-audit --no-fund --silent \
    express zod chalk date-fns better-sqlite3 bcrypt sharp pino keyv @parcel/watcher
fi
cp "$HERE"/fixtures/*.mjs "$WORK"/

pass=0; fail=0
printf '%-16s %-6s %-24s %s\n' FIXTURE RESULT OUTPUT EJECTED
for f in a-*.mjs; do
  n="${f%.mjs}"
  control="$(node "$f" 2>&1 | tail -1)"
  if ! "$NUB" compile "$WORK/$f" --out "$WORK/bin-$n" > "$WORK/log-$n" 2>&1; then
    printf '%-16s %-6s %s\n' "$n" FAIL "compile failed — see log-$n"; fail=$((fail+1)); continue
  fi
  ejected=$(grep -A9 '^Shipping' "$WORK/log-$n" 2>/dev/null \
    | grep -oE '^  [a-z@][a-z0-9@/._-]*' | tr -d ' ' | paste -sd, -)

  mv node_modules .nm-hidden
  rm -rf "$WORK/c-$n"
  out="$(cd / && XDG_CACHE_HOME="$WORK/c-$n" "$WORK/bin-$n" 2>/dev/null | tail -1)"
  rc=$?
  mv .nm-hidden node_modules

  if [ "$rc" = 0 ] && [ "$out" = "$control" ]; then
    printf '%-16s %-6s %-24s %s\n' "$n" PASS "$out" "${ejected:--}"; pass=$((pass+1))
  else
    printf '%-16s %-6s %-24s %s\n' "$n" FAIL "$out (want: $control)" "${ejected:--}"; fail=$((fail+1))
  fi
done
printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" = 0 ]
