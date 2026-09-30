#!/usr/bin/env bash
# Compile real npm packages, then run cold and warm artifacts with their source
# and node_modules hidden, comparing complete stdout against successful Node runs.
#
# The comparison is the point. A compiled binary that prints nothing, or crashes
# instantly, looks indistinguishable from a fast one unless its output is checked
# against a control — so every fixture is run on plain Node first and the artifact
# must reproduce that byte for byte.
#
# Usage: NUB=/path/to/nub tests/compile-corpus/run.sh [workdir]
set -euo pipefail

NUB="${NUB:?set NUB to the nub binary under test}"
WORK="${1:-${TMPDIR:-/tmp}/nub-compile-corpus}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$WORK" && cd "$WORK"
WORK="$PWD"
if [ -e .nm-hidden ] || [ -e .source-hidden ]; then
  echo "restore .nm-hidden/.source-hidden from an interrupted run before retrying" >&2
  exit 1
fi
# compile refuses to guess a Node version, by design — a compiled binary's runtime
# must be intentional and reproducible. Pin it here so the corpus is too.
: "${NODE_PIN:=$(node -p process.versions.node)}"
if [ "$NODE_PIN" != "$(node -p process.versions.node)" ]; then
  echo "NODE_PIN must match the Node running the control and installing native addons" >&2
  exit 1
fi
printf '%s\n' "$NODE_PIN" > .node-version
cp "$HERE/package.json" "$HERE/package-lock.json" "$WORK/"
npm ci --userconfig "$HERE/../../.npmrc" --no-audit --no-fund --silent
# A deleted or renamed fixture must not survive a reused work directory.
rm -f "$WORK"/a-*.mjs "$WORK"/fork-child.mjs
cp "$HERE"/fixtures/*.mjs "$WORK"/

restore_source() {
  if [ -d "$WORK/.source-hidden" ]; then
    mv "$WORK/.source-hidden/"*.mjs "$WORK/"
    rmdir "$WORK/.source-hidden"
  fi
  if [ -d "$WORK/.nm-hidden" ]; then mv "$WORK/.nm-hidden" "$WORK/node_modules"; fi
}
trap restore_source EXIT

pass=0; fail=0
printf '%-16s %-6s %-24s %s\n' FIXTURE RESULT OUTPUT EJECTED
for f in a-*.mjs; do
  n="${f%.mjs}"
  if ! env -u NODE_OPTIONS -u NODE_PATH node "$f" > "$WORK/control-$n.stdout" 2> "$WORK/control-$n.stderr"; then
    printf '%-16s %-6s %s\n' "$n" FAIL "Node control failed — see control-$n.stderr"
    fail=$((fail+1)); continue
  fi
  if ! "$NUB" compile "$WORK/$f" --out "$WORK/bin-$n" > "$WORK/log-$n" 2>&1; then
    printf '%-16s %-6s %s\n' "$n" FAIL "compile failed — see log-$n"; fail=$((fail+1)); continue
  fi
  # The header states how many entries follow, so take exactly that many rather
  # than guessing where the list stops. `grep -A9` guessed with a fixed nine-line
  # window and swept in the resolved-build block's `output` / `runtime` / `target`
  # rows; guessing by leading character instead would drop a digit-leading package
  # such as `7zip-bin`, which sorts first and would hide every entry behind it.
  # This column is the one a reader trusts to spot over-ejection, so a parser that
  # can either invent or swallow entries is worse than no column.
  ejected=$(awk '
    /^Shipping / { left = $2 + 0; next }
    left > 0 && /^  [^ ]/ { print $1; left--; next }
    left > 0 { exit }
  ' "$WORK/log-$n" 2>/dev/null | paste -sd, -)

  mv node_modules .nm-hidden
  mkdir .source-hidden
  mv ./*.mjs .source-hidden/
  rm -rf "$WORK/c-$n"
  good=1
  for state in cold warm; do
    rc=0
    (cd / && env -u NODE_OPTIONS -u NODE_PATH XDG_CACHE_HOME="$WORK/c-$n" "$WORK/bin-$n") \
      > "$WORK/$state-$n.stdout" 2> "$WORK/$state-$n.stderr" || rc=$?
    if [ "$rc" != 0 ] || ! cmp -s "$WORK/control-$n.stdout" "$WORK/$state-$n.stdout"; then
      printf '%-16s %-6s %s\n' "$n" FAIL "$state rc=$rc — see $state-$n.stdout/.stderr and control-$n.stdout"
      good=0
      break
    fi
  done
  restore_source

  out="$(tail -1 "$WORK/control-$n.stdout")"
  if [ "$good" = 1 ]; then
    printf '%-16s %-6s %-24s %s\n' "$n" PASS "$out" "${ejected:--}"; pass=$((pass+1))
  else
    fail=$((fail+1))
  fi
done
printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" = 0 ]
