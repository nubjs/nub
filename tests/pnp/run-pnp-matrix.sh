#!/bin/bash
# Exercise nub's Yarn PnP support against a real fixture across multiple Node
# versions — the matrix that found (and now guards) every PnP corner.
#
# Each scenario probes a distinct resolution path that fails independently:
#   cjs          require() a CJS PnP dep                  (.pnp.cjs --require + _resolveFilename)
#   esm-cjsdep   import a CJS PnP dep from ESM            (CJS-from-ESM sub-loader)
#   esm-puredep  import a pure-ESM zip-stored PnP dep     (resolve hook must emit `format`)
#   run          `nub run` a script using a PnP dep       (compute_augmentation_env path)
#   nubx         `nubx <bin>` for a zip-stored bin        (pnpapi bin-resolver + entry load)
#   node-off     `nub --node` must NOT resolve the dep    (augmentation disabled)
#
# KNOWN GAP (asserted, not failed): nubx of a *zip-stored* bin only works on the
# fast tier (Node 22.15+); on the compat tier the --import preload routes the entry
# through the ESM loader whose existence check bypasses PnP's fs patch. Workaround:
# Node 22.15+ or `yarn unplug`. Tracked in wiki/research/pnp-preload-feasibility.md.
#
# Usage:
#   tests/pnp/run-pnp-matrix.sh [nub-binary] [node-bin-dir ...]
# With no node-bin-dirs it sweeps every ~/.nvm/versions/node/* it finds; otherwise
# pass explicit bin dirs (e.g. a container's /usr/local/bin). Defaults nub to
# target/release/nub, then target/debug/nub.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
FIXTURE="${NUB_PNP_FIXTURE:-/tmp/nub-pnp-fixture}"

NUB="${1:-}"
if [ -z "$NUB" ]; then
  NUB="$REPO_DIR/target/release/nub"; [ -x "$NUB" ] || NUB="$REPO_DIR/target/debug/nub"
fi
shift || true
if [ ! -x "$NUB" ]; then echo "error: nub binary not found/executable at $NUB" >&2; exit 1; fi
# Absolutize — the probes `cd` into the fixture, so a relative nub path would break.
NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")"

# nubx dispatch is by argv0 — symlink a `nubx` next to the binary under test.
NUBX="$(dirname "$NUB")/nubx"; ln -sf "$NUB" "$NUBX"

[ -f "$FIXTURE/.pnp.cjs" ] || "$SCRIPT_DIR/make-fixture.sh" "$FIXTURE"

# Node bin dirs to sweep.
NODE_DIRS=("$@")
if [ ${#NODE_DIRS[@]} -eq 0 ]; then
  for d in "$HOME"/.nvm/versions/node/*/bin; do [ -d "$d" ] && NODE_DIRS+=("$d"); done
fi
[ ${#NODE_DIRS[@]} -gt 0 ] || { echo "error: no node bin dirs to test" >&2; exit 1; }

# probe <node-bin-dir> <invocation...> -> grep token; prints OK/X
run() { ( cd "$FIXTURE" && PATH="$1:$PATH" "${@:2}" ) 2>&1; }
fails=0
ver_ge_2215() { [ "$1" -gt 22 ] || { [ "$1" -eq 22 ] && [ "$2" -ge 15 ]; }; }

printf "nub: %s\n\n" "$NUB"
for bin in "${NODE_DIRS[@]}"; do
  nv="$("$bin/node" -v 2>/dev/null)"; nv="${nv#v}"; [ -n "$nv" ] || continue
  maj="${nv%%.*}"; rest="${nv#*.}"; min="${rest%%.*}"
  [ "$maj" -gt 18 ] 2>/dev/null || { [ "$maj" -eq 18 ] && [ "$min" -ge 19 ]; } || { printf "%-12s SKIP (below 18.19 floor)\n" "v$nv"; continue; }

  ok=0; tot=0; line=""
  check() { tot=$((tot+1)); if echo "$2" | grep -q "$3"; then ok=$((ok+1)); line+=" $1:✓"; else line+=" $1:✗"; fails=$((fails+1)); fi; }

  check cjs         "$(run "$bin" "$NUB" cjs-test.cjs)"  "CJS-OK"
  check esm-cjsdep  "$(run "$bin" "$NUB" esm-test.mjs)"  "ESM-OK"
  check esm-puredep "$(run "$bin" "$NUB" esm-pure.mjs)"  "PURE-ESM-OK"
  check run         "$(run "$bin" "$NUB" run start)"     "SCRIPT-OK"
  # --node must DISABLE PnP: the dep must NOT resolve.
  check node-off    "$(run "$bin" "$NUB" --node cjs-test.cjs)" "Cannot find module"

  # nubx zip-bin: a hard PASS only on the fast tier; on compat it's the known gap
  # (asserted as expected-fail so a future fix flips it to a visible pass).
  nubx_out="$(run "$bin" "$NUBX" cowsay hi)"
  if ver_ge_2215 "$maj" "$min"; then
    check nubx "$nubx_out" "< hi >"
  else
    if echo "$nubx_out" | grep -q "< hi >"; then line+=" nubx:✓(gap-now-fixed!)"; else line+=" nubx:~(known-compat-gap)"; fi
  fi

  printf "%-12s %d/%d %s\n" "v$nv" "$ok" "$tot" "$line"
done

echo
if [ "$fails" -eq 0 ]; then echo "PnP matrix: all required scenarios passed."; else echo "PnP matrix: $fails required scenario(s) FAILED."; fi
exit $fails
