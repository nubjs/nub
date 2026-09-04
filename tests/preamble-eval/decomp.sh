#!/usr/bin/env bash
# Price each member of the compile preamble's STATIC IMPORT GRAPH.
#
# A static import is evaluated whether or not anything calls into it, so the
# call-gating already in the artifact cannot reach this term. Each variant below
# removes one import (plus the uses that would then be undefined bindings) from
# runtime/compile-preamble.mjs, compiles a hello-world artifact against it, and the
# delta against `full` is that module's evaluation cost.
#
# Interleaved duplicate baselines are the error bar: if they disagree by more than
# the effect, the run is thrown away. Minimums only — a mean on a shared runner
# measures the runner.
set -euo pipefail
NUB_BIN="${NUB_BIN:?}"
REPO="${REPO:-$PWD}"
PRE="$REPO/runtime/compile-preamble.mjs"
ABLATE="$REPO/tests/preamble-eval/ablate.mjs"
NODE_PIN="${NODE_PIN:-$(node -v | tr -d v)}"

D=$(mktemp -d)
cd "$D" || exit 1
pwd
printf '%s\n' "$NODE_PIN" > .node-version
printf '{"name":"d","type":"module"}\n' > package.json
printf 'console.log("hello, nub");\n' > hello.ts
echo "" > nil.js

cp "$PRE" "$D/preamble.orig.mjs"
restore() { cp "$D/preamble.orig.mjs" "$PRE"; }
trap restore EXIT

build() {
  local name="$1"; shift
  if [ "$#" -eq 0 ]; then
    cp "$D/preamble.orig.mjs" "$PRE"
  else
    node "$ABLATE" "$D/preamble.orig.mjs" "$@" > "$PRE"
  fi
  "$NUB_BIN" compile hello.ts --out "$D/art-$name" >/dev/null
  printf '%-12s %9s bytes\n' "$name" "$(wc -c < "$D/art-$name" | tr -d ' ')"
}

echo "--- building variants ---"
build full
build nowp   worker
build nopc   childprocess
build nosp   syncpolyfills
build nospc  syncpolyfills-call
build min    worker childprocess syncpolyfills
build empty  empty
restore

echo "--- sanity: every variant runs ---"
export XDG_CACHE_HOME="$D/cache"
for v in full nowp nopc nosp nospc min empty; do
  printf '%-12s %s\n' "$v" "$("$D/art-$v")"
done

echo "--- measuring ---"
hyperfine -i --warmup 50 --min-runs "${RUNS:-400}" --style none --export-json "$D/r.json" \
  -n 'baseline-A' 'node nil.js' \
  -n 'full'       "$D/art-full" \
  -n 'nowp'       "$D/art-nowp" \
  -n 'baseline-B' 'node nil.js' \
  -n 'nopc'       "$D/art-nopc" \
  -n 'nosp'       "$D/art-nosp" \
  -n 'baseline-C' 'node nil.js' \
  -n 'nospc'      "$D/art-nospc" \
  -n 'min'        "$D/art-min" \
  -n 'empty'      "$D/art-empty" \
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
  console.log(`${b.command.padEnd(12)} min ${t.toFixed(2).padStart(7)} ms   over-node ${(t - b0).toFixed(2).padStart(6)}   saves ${(full - t).toFixed(2).padStart(6)}`);
}' "$D/r.json"
