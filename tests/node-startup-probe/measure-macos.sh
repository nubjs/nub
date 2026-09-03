#!/usr/bin/env bash
# Measure cold-start cost of one node binary on macOS, decomposed into the phases that matter:
# exec+dyld (spawn -> first dylib initializer), in-process (initializer -> exit()), exit handlers,
# kernel teardown; plus the dyld-side facts that drive the first bucket (weak-def coalescing
# imports, exported/weak symbols) and node's own performance.nodeTiming marks.
# usage: measure-macos.sh <node-binary> <label> <harness-dir>
set -uo pipefail
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1"); label=$2; harness=$3
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
cc -O2 -dynamiclib -o "$tmp/libinterpose2.dylib" "$harness/interpose2.c"
cc -O2 -o "$tmp/spawnbench" "$harness/spawnbench.c"
printf 'console.log("hi")\n' > "$tmp/hello.js"
echo "label=$label"; echo "node=$("$bin" --version)"; uptime | sed 's/.*load/load/'

echo "--- hyperfine (min / median / mean, ms; 100 runs after 10 warmup)"
hyperfine -N --warmup 10 --runs 100 --export-json "$tmp/hf.json" "$bin --version" "$bin -e 0" "$bin $tmp/hello.js" > /dev/null 2>&1
python3 - "$tmp/hf.json" <<'PY'
import json, sys
for r in json.load(open(sys.argv[1]))["results"]:
    cmd = r["command"].split("/")[-1] if "hello.js" in r["command"] else r["command"].split(" ", 1)[1]
    print("%-14s min %6.2f  median %6.2f  mean %6.2f" % (cmd, r["min"]*1e3, r["median"]*1e3, r["mean"]*1e3))
PY

echo "--- phase decomposition (min of 40 spawns, ms)"
export INTERPOSE_LIB="$tmp/libinterpose2.dylib"
"$tmp/spawnbench" 40 "$bin" --version
"$tmp/spawnbench" 40 "$bin" -e 0
"$tmp/spawnbench" 40 "$bin" "$tmp/hello.js"

echo "--- nodeTiming (min of 20, ms since process start)"
"$bin" -e '
const {spawnSync}=require("child_process");const keys=["nodeStart","v8Start","environment","bootstrapComplete"];const mins={};
for(let i=0;i<20;i++){const t=JSON.parse(spawnSync(process.execPath,["-e","console.log(JSON.stringify(performance.nodeTiming.toJSON()))"],{encoding:"utf8"}).stdout);for(const k of keys)mins[k]=Math.min(mins[k]??1e9,t[k]);}
let prev=0;for(const k of keys){console.log(k.padEnd(20),mins[k].toFixed(2),"delta",(mins[k]-prev).toFixed(2));prev=mins[k];}'

echo "--- dyld / symbol facts"
echo "size            $(stat -f %z "$bin")"
echo "nlist symbols   $(nm "$bin" | wc -l | tr -d ' ')"
echo "exported        $(nm -gU "$bin" | wc -l | tr -d ' ')"
echo "weak externals  $(nm -m "$bin" | grep -c 'weak external')"
echo "unique imports  $(dyld_info -imports "$bin" | awk 'NR>2' | wc -l | tr -d ' ')"
echo "weak-coalesce   $(dyld_info -imports "$bin" | grep -c 'weak-def-coalesce')"
echo "fixups          $(dyld_info -fixups "$bin" | awk 'NR>3' | wc -l | tr -d ' ')"
echo "header flags    $(otool -hv "$bin" | tail -1 | grep -oE 'WEAK_DEFINES|BINDS_TO_WEAK' | tr '\n' ' ')"
echo "initializers    $(DYLD_PRINT_INITIALIZERS=1 "$bin" --version 2>&1 | grep -c 'running initializer')"
