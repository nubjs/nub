#!/usr/bin/env bash
# Price the two warm-start changes on this branch on a quiet runner, both OSes.
#
#   1. the bootstrap preload dropped for a payload that needs neither eager
#      builtin (`Manifest::standalone_preamble`), and
#   2. on macOS only, the launcher no longer linking Security.framework and
#      CoreFoundation (`-dead_strip_dylibs`).
#
# The preload is isolated on ONE extracted tree rather than on two artifacts: any
# source marker that keeps the preload also keeps an eager builtin load, so two
# artifacts would measure that load as well. The arms are the artifact's own Node
# command line with `--require <bootstrap>` (what the launcher passed before) and
# with `__NUB_COMPILED_BOOTSTRAP=<bootstrap>` (what it passes now), same chunks,
# same warm compile cache. Three instruments, because no single one has resolved
# terms this size on a shared runner: hyperfine with interleaved duplicate
# baselines (wall), `cpu-ab.sh` (child CPU, in-run control), and
# `performance.nodeTiming` minimums (in-process, no fork noise).
set -euo pipefail
NUB_BIN="${NUB_BIN:?}"
REPO="${REPO:-$PWD}"
LAUNCHER_BEFORE="${LAUNCHER_BEFORE:-}"   # macOS: a launcher built without -dead_strip_dylibs
NODE_PIN="${NODE_PIN:-$(node -v | tr -d v)}"
RUNS="${RUNS:-400}"

D=$(mktemp -d)
cd "$D" || exit 1
pwd
printf '%s\n' "$NODE_PIN" > .node-version
printf '{"name":"d","type":"module"}\n' > package.json
printf 'console.log("hello, nub");\n' > hello.ts
cat > probe.ts <<'EOF'
const user = performance.now();
const t = performance.nodeTiming;
process.stdout.write(`${t.bootstrapComplete.toFixed(3)} ${user.toFixed(3)}\n`);
EOF
echo "" > nil.mjs

"$NUB_BIN" compile hello.ts --out ./art > compile-hello.log 2>&1
"$NUB_BIN" compile probe.ts --out ./probe > compile-probe.log 2>&1
export XDG_CACHE_HOME="$D/cache"
./art >/dev/null; ./art >/dev/null; ./probe >/dev/null; ./probe >/dev/null
APP=$(find "$D/cache/nub/compile-app" -maxdepth 1 -mindepth 1 -type d | while read -r d; do [ -f "$d/hello.mjs" ] && echo "$d"; done | head -1)
PAPP=$(find "$D/cache/nub/compile-app" -maxdepth 1 -mindepth 1 -type d | while read -r d; do [ -f "$d/probe.mjs" ] && echo "$d"; done | head -1)
N=$(find "$D/cache/nub/compile-node" -maxdepth 2 -name node -type f | head -1)
B="$APP/__nub_compile_bootstrap.cjs"
PB="$PAPP/__nub_compile_bootstrap.cjs"
CC=$(find "$D/cache/nub/compile-v8" -maxdepth 1 -mindepth 1 -type d | head -1)
FLAGS="--disable-warning=ExperimentalWarning --experimental-vm-modules --experimental-eventsource --experimental-addon-modules --experimental-import-text --experimental-ffi --experimental-vfs --experimental-stream-iter"
[ "$(node -e 'process.stdout.write("OK")')" = OK ] || { echo "PATH node is not plain node"; exit 1; }
[ "$("$N" -e 'process.stdout.write("OK")')" = OK ] || { echo "extracted node is not plain node"; exit 1; }
echo "extracted tree: $(ls "$APP" | tr '\n' ' ')"
__NUB_LAUNCHER_TIMING=1 ./art 2>&1 | grep -E "argv:|env:" | sed "s#$D##g" | cut -c1-160

# Every Node arm below carries the artifact's own flags and compile cache; only
# the bootstrap delivery differs.
cat > run-standalone.sh <<EOF
#!/usr/bin/env bash
exec env NODE_COMPILE_CACHE=$CC __NUB_COMPILED_BOOTSTRAP=$B "$N" $FLAGS "\$@"
EOF
cat > run-preload.sh <<EOF
#!/usr/bin/env bash
exec env NODE_COMPILE_CACHE=$CC "$N" $FLAGS --require=$B "\$@"
EOF
chmod +x run-standalone.sh run-preload.sh
./run-standalone.sh "$APP/hello.mjs"; ./run-preload.sh "$APP/hello.mjs"

echo "--- wall clock (hyperfine, minimums; the baseline spread is the bar) ---"
hyperfine -i --warmup 30 --min-runs "$RUNS" --style none --export-json r.json \
  -n 'baseline-A'  "$N nil.mjs" \
  -n 'standalone'  "./run-standalone.sh $APP/hello.mjs" \
  -n 'preload'     "./run-preload.sh $APP/hello.mjs" \
  -n 'baseline-B'  "$N nil.mjs" \
  -n 'artifact'    "./art" \
  -n 'baseline-C'  "$N nil.mjs" || true
node -e '
const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).results;
const m = b => Math.min(...b.times) * 1000;
const bases = r.filter(b => b.command.includes("baseline")).map(m);
const spread = Math.max(...bases) - Math.min(...bases);
console.log(`BASELINE SPREAD: ${spread.toFixed(2)} ms`);
const b0 = Math.min(...bases);
for (const b of r) console.log(`${b.command.padEnd(12)} min ${m(b).toFixed(2).padStart(7)} ms   over-node ${(m(b) - b0).toFixed(2).padStart(6)}`);' r.json

echo "--- child CPU (cpu-ab.sh; control repeats arm 1, its gap is the bar) ---"
PER=5 ROUNDS="${ROUNDS:-40}" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
  "standalone=./run-standalone.sh $APP/hello.mjs" \
  "preload=./run-preload.sh $APP/hello.mjs" \
  "artifact=./art" \
  "control=./run-standalone.sh $APP/hello.mjs"

echo "--- in-process nodeTiming, minimums of 150 (bootstrapComplete, user code) ---"
: > s.txt; : > p.txt; : > n.txt
for _ in $(seq 1 150); do
  env NODE_COMPILE_CACHE=$CC __NUB_COMPILED_BOOTSTRAP=$PB "$N" $FLAGS "$PAPP/probe.mjs" >> s.txt
  env NODE_COMPILE_CACHE=$CC "$N" $FLAGS --require="$PB" "$PAPP/probe.mjs" >> p.txt
  "$N" probe.mjs >> n.txt 2>/dev/null || "$N" "$PAPP/probe.mjs" >> n.txt
done
stats() { awk -v c="$1" '{ if (NR == 1 || $c < mn) mn = $c; sum += $c } END { printf "min %.3f mean %.3f", mn, sum / NR }' "$2"; }
for f in s.txt p.txt n.txt; do
  printf '%-12s bootstrapComplete %s   user-code %s\n' "$f" "$(stats 1 "$f")" "$(stats 2 "$f")"
done
echo "(s = standalone, p = preload, n = plain node on the same probe chunk)"

if [ -n "$LAUNCHER_BEFORE" ] && [ "$(uname -s)" = Darwin ]; then
  echo "--- macOS: the launcher with and without the Apple frameworks ---"
  otool -L "$LAUNCHER_BEFORE" | tail -n +2
  __NUB_LAUNCHER_TEMPLATE="$LAUNCHER_BEFORE" "$NUB_BIN" compile hello.ts --out ./art-before > compile-before.log 2>&1
  otool -L ./art-before | tail -n +2 | wc -l; otool -L ./art | tail -n +2 | wc -l
  ./art-before >/dev/null; ./art-before >/dev/null
  PER=5 ROUNDS="${ROUNDS:-40}" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
    "art-after=./art" \
    "art-before=./art-before" \
    "probe-after=env __NUB_COMPILED_LAUNCHER_MODE=probe ./art" \
    "probe-before=env __NUB_COMPILED_LAUNCHER_MODE=probe ./art-before" \
    "control=./art"
  hyperfine -i --warmup 30 --min-runs "$RUNS" --style none --export-json l.json \
    -n 'baseline-A' "$N nil.mjs" \
    -n 'art-after'  './art' \
    -n 'art-before' './art-before' \
    -n 'baseline-B' "$N nil.mjs" || true
  node -e '
const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).results;
const m = b => Math.min(...b.times) * 1000;
const bases = r.filter(b => b.command.includes("baseline")).map(m);
console.log(`BASELINE SPREAD: ${(Math.max(...bases) - Math.min(...bases)).toFixed(2)} ms`);
for (const b of r) console.log(`${b.command.padEnd(12)} min ${m(b).toFixed(2).padStart(7)} ms`);' l.json
fi
