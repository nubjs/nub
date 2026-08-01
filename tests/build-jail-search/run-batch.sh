#!/usr/bin/env bash
# Run the grant search over a worklist. One JSON record per line to stdout, and one
# `results/runs/<pkg>@<version>.json` per package written by search.mjs itself.
#
# Usage:  ./run-batch.sh <nub> [--force] <pkg@ver> [pkg@ver ...]
#         ./run-batch.sh <nub> [--force] --file <worklist.txt>
#
# RESUMABLE BY DEFAULT. search.mjs skips a package whose run file already exists, so a
# re-run costs only what has not been measured — and a batch killed halfway can simply be
# started again. `--force` re-measures everything, which is what a harness or jail change
# calls for, since a run file records the answer AND the harness hash that produced it.
set -u
NUB="${1:-}"; shift || true
[ -x "$NUB" ] || { echo "usage: $0 <nub> [--force] <pkg@ver>... | --file <list>"; exit 2; }
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac

# ⛔ SNAPSHOT THE BINARY, and run the batch against the COPY.
#
# A batch takes hours. Any cargo command on the same profile during that window rewrites
# `target/fast/nub` with WHATEVER FEATURES that command specified — and a binary without
# `build-jail-catalog-override` refuses every override, so every package measured after the
# rebuild records a control failure. MEASURED: a `cargo test --profile fast` run concurrently
# with a batch silently invalidated six packages, which then read as broken-at-the-widest-grant
# while installing fine jailed, unjailed and under npm.
#
# Copying is the fix that does not depend on remembering. The batch reads a file nothing else
# writes, so a rebuild mid-run cannot reach it.
SNAP="${TMPDIR:-/tmp}/nub-batch-$$"
cp "$NUB" "$SNAP" && chmod +x "$SNAP"
trap 'rm -f "$SNAP"' EXIT
echo "batch binary: $(shasum -a256 "$SNAP" | cut -c1-16)  (snapshot of $NUB)" >&2

here="$(cd "$(dirname "$0")" && pwd)"

# And PROVE the override engages before spending hours on it, rather than discovering per-cell.
#
# ⛔ THE PROBE CATALOG COMES FROM `catalogFor`, NEVER FROM A LITERAL HERE. This check used to write
# `{"packages":{}}` — an EMPTY map, which parses under every packages shape there has ever been. So
# when the catalog shape changed under it, the check passed happily while every SYNTHESIZED cell
# catalog was rejected, and a 100-package batch produced 100 HARNESS-ERRORs before anyone looked.
# An empty map is ADJACENT to the question; the artifact the cells actually use IS the question.
_probe="${TMPDIR:-/tmp}/nub-ovcheck-$$"; rm -rf "$_probe"; mkdir -p "$_probe/home"
printf '{"name":"ovcheck","version":"1.0.0"}\n' > "$_probe/package.json"
if ! node "$here/search.mjs" --emit-sample-catalog > "$_probe/c.json"; then
  echo "REFUSING TO RUN: search.mjs could not emit a sample catalog." >&2
  rm -rf "$_probe"; exit 2
fi
_ovlog="$_probe/ov.log"
( cd "$_probe" && NUB_BUILD_JAIL_CATALOG="$_probe/c.json" HOME="$_probe/home" \
    "$SNAP" install > "$_ovlog" 2>&1 )
if ! grep -q "build-jail catalog OVERRIDDEN from" "$_ovlog"; then
  echo "REFUSING TO RUN: the override did not engage with this binary + catalog." >&2
  if grep -q "was REJECTED" "$_ovlog"; then
    echo "  The catalog was REJECTED — the harness emits a shape the parser does not accept:" >&2
    grep -o "was REJECTED ([^)]*)" "$_ovlog" | head -1 | sed 's/^/    /' >&2
  else
    echo "  Rebuild: scripts/rust-build.sh build -p nub-cli --profile fast \\" >&2
    echo "             --features nub-cli/build-jail-catalog-override" >&2
  fi
  rm -rf "$_probe"; exit 2
fi
rm -rf "$_probe"
NUB="$SNAP"

FORCE=""
[ "${1:-}" = "--force" ] && { FORCE="--force"; shift; }

if [ "${1:-}" = "--file" ]; then
  [ -r "${2:-}" ] || { echo "cannot read worklist ${2:-}"; exit 2; }
  set -- $(grep -vE '^\s*(#|$)' "$2")
fi

for spec in "$@"; do
  timeout 2400 node "$here/search.mjs" "$spec" --nub "$NUB" $FORCE 2>/dev/null \
    || echo "{\"pkg\":\"$spec\",\"verdict\":\"HARNESS-TIMEOUT-OR-CRASH\"}"
done
