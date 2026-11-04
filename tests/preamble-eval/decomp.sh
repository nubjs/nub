#!/usr/bin/env bash
# Price each member of the compile preamble's STATIC IMPORT GRAPH, and the two
# top-level statements inside that graph that do real work.
#
# A static import is evaluated whether or not anything calls into it, so the
# call-gating already in the artifact cannot reach this term. Each variant below
# removes one import (plus the uses that would then be undefined bindings) or one
# top-level statement, compiles a hello-world artifact against it, and the delta
# against `full` is that piece's evaluation cost.
#
# Interleaved duplicate baselines are the error bar: if they disagree by more than
# the effect, the run is thrown away. Minimums only — a mean on a shared runner
# measures the runner.
set -euo pipefail
NUB_BIN="${NUB_BIN:?}"
REPO="${REPO:-$PWD}"
ABLATE="$REPO/tests/preamble-eval/ablate.mjs"
NODE_PIN="${NODE_PIN:-$(node -v | tr -d v)}"

D=$(mktemp -d)
cd "$D" || exit 1
pwd
printf '%s\n' "$NODE_PIN" > .node-version
printf '{"name":"d","type":"module"}\n' > package.json
printf 'console.log("hello, nub");\n' > hello.ts
echo "" > nil.js

declare -A SRC=(
  [preamble]="$REPO/runtime/compile-preamble.mjs"
  [preload]="$REPO/runtime/preload-common.cjs"
)
for k in "${!SRC[@]}"; do cp "${SRC[$k]}" "$D/$k.orig"; done
restore() { for k in "${!SRC[@]}"; do cp "$D/$k.orig" "${SRC[$k]}"; done; }
trap restore EXIT

# build <name> [<file>:<drop> …]
build() {
  local name="$1"; shift
  restore
  local spec file drop
  for spec in "$@"; do
    file="${spec%%:*}"
    drop="${spec#*:}"
    node "$ABLATE" "${SRC[$file]}" "$drop" > "$D/tmp.src"
    cp "$D/tmp.src" "${SRC[$file]}"
  done
  "$NUB_BIN" compile hello.ts --out "$D/art-$name" > "$D/compile-$name.log" 2>&1 ||
    { echo "COMPILE FAILED: $name"; tail -20 "$D/compile-$name.log"; exit 1; }
  printf '%-10s %s\n' "$name" "$(grep -oE 'app [0-9.]+ [KM]B' "$D/compile-$name.log" | head -1)"
}

echo "--- building variants ---"
build full
build ungate preload:pc-ungate
build nowp   preamble:worker
build nopc   preamble:childprocess
build min    preamble:worker preamble:childprocess preamble:syncpolyfills
restore

echo "--- sanity: every variant runs ---"
export XDG_CACHE_HOME="$D/cache"
for v in full ungate nowp nopc min; do
  printf '%-10s %s\n' "$v" "$("$D/art-$v")"
done

echo "--- measuring ---"
hyperfine -i --warmup 50 --min-runs "${RUNS:-700}" --style none --export-json "$D/r.json" \
  -n 'baseline-A' 'node nil.js' \
  -n 'full'       "$D/art-full" \
  -n 'ungate'     "$D/art-ungate" \
  -n 'baseline-B' 'node nil.js' \
  -n 'nowp'       "$D/art-nowp" \
  -n 'nopc'       "$D/art-nopc" \
  -n 'baseline-C' 'node nil.js' \
  -n 'min'        "$D/art-min" \
  -n 'baseline-D' 'node nil.js' || true

node -e '
const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).results;
const m = b => Math.min(...b.times) * 1000;
const bases = r.filter(b => b.command.includes("baseline")).map(m);
const spread = Math.max(...bases) - Math.min(...bases);
console.log(`\nBASELINE SPREAD: ${spread.toFixed(2)} ms  <- the error bar on every row`);
if (spread > 0.5) console.log("INSTRUMENT TOO NOISY — do not read the rows.");
const full = m(r.find(b => b.command === "full"));
const b0 = Math.min(...bases);
for (const b of r) {
  const t = m(b);
  console.log(`${b.command.padEnd(10)} min ${t.toFixed(2).padStart(7)} ms   over-node ${(t - b0).toFixed(2).padStart(6)}   saves ${(full - t).toFixed(2).padStart(6)}`);
}' "$D/r.json"

# The same variants again, priced in CHILD CPU rather than wall clock. On every
# runner this has been given, the hyperfine table above could not resolve terms
# under ~1 ms while its own baselines drifted 0.50-1.16 ms; the CPU instrument
# separates the same terms against a 0.115 ms in-run control. `control` repeats
# arm 1 on purpose — its gap IS the error bar, and a run whose control exceeds the
# effect gets thrown away.
echo "--- measuring again, in child CPU time ---"
PER=5 ROUNDS="${ROUNDS:-40}" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
  "full=$D/art-full" \
  "ungate=$D/art-ungate" \
  "nowp=$D/art-nowp" \
  "nopc=$D/art-nopc" \
  "min=$D/art-min" \
  "control=$D/art-full"
