#!/usr/bin/env bash
# Repeats measure-macos.sh's phase split with the two binaries alternated, six
# rounds, order reversed every other round. A loaded runner drifts by more than
# the effect under test, so one sequential A then B reading cannot tell drift
# from a difference; alternation turns drift into a per-round sign flip.
# usage: phases.sh <baseline-binary> <variant-binary> <harness-dir> <outdir>
set -uo pipefail
base=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
var=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
harness=$(cd "$3" && pwd); out=$4; mkdir -p "$out"; cd "$out"
chmod +x "$base" "$var"
cc -O2 -dynamiclib -o libinterpose2.dylib "$harness/interpose2.c"
cc -O2 -o spawnbench "$harness/spawnbench.c"
export INTERPOSE_LIB="$PWD/libinterpose2.dylib"
printf 'console.log("hi")\n' > hello.js
cat > watchy.js <<'JS'
const fs = require('fs');
const w = fs.watch(process.cwd(), () => {});
w.close();
JS
sw_vers | sed -n 2p; uptime | sed 's/.*load/load/'
echo "########## phase split, min of 40 spawns per cell"
for round in 1 2 3 4 5 6; do
  if (( round % 2 )); then order="baseline:$base macos-cf:$var"; else order="macos-cf:$var baseline:$base"; fi
  for pair in $order; do
    label=${pair%%:*}; bin=${pair#*:}
    for cmd in "--version" "-e 0" "hello.js" "--use-system-ca -e 0" "watchy.js"; do
      # shellcheck disable=SC2086
      printf 'round=%d bin=%s cmd=%s ' "$round" "$label" "$cmd"; ./spawnbench 40 "$bin" $cmd
    done
  done
  uptime | sed 's/.*load/load/'
done
echo "########## nodeTiming, min of 20 spawns per cell"
timing='const {spawnSync}=require("child_process");const keys=["nodeStart","v8Start","environment","bootstrapComplete"];const mins={};
for(let i=0;i<20;i++){const t=JSON.parse(spawnSync(process.execPath,["-e","console.log(JSON.stringify(performance.nodeTiming.toJSON()))"],{encoding:"utf8"}).stdout);for(const k of keys)mins[k]=Math.min(mins[k]??1e9,t[k]);}
console.log(keys.map((k)=>k+"="+mins[k].toFixed(2)).join(" "));'
for round in 1 2 3; do
  for pair in "baseline:$base" "macos-cf:$var" "macos-cf:$var" "baseline:$base"; do
    label=${pair%%:*}; bin=${pair#*:}
    printf 'round=%d bin=%s ' "$round" "$label"; "$bin" -e "$timing"
  done
done
uptime | sed 's/.*load/load/'
