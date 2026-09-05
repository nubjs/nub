#!/usr/bin/env bash
# Price the warm-start changes on this branch on a quiet runner, both OSes.
#
#   1. the bootstrap preload dropped for a payload that needs neither eager
#      builtin (`Manifest::standalone_preamble`);
#   2. single-file emission — the CommonJS and runtime chunks folded into the
#      entry chunk (`NUB_BIN_BEFORE` names a nub built before that change); and
#   3. on macOS only, the launcher no longer linking Security.framework and
#      CoreFoundation (`LAUNCHER_BEFORE` names a template linked without
#      `-dead_strip_dylibs`);
#   4. the chunk shaped so Node's compile cache holds the runtime's startup
#      functions (`NUB_BIN_LAZY_CACHE` names a nub built just before that
#      change); and
#   5. for an older target that still needs the bundled polyfills, the polyfills
#      installed as lazy globals (`NUB_BIN_EAGER_POLYFILLS` names a nub built
#      just before that change; `NUB_BIN_BEFORE` stands in when it is unset).
#
# The preload is isolated on ONE extracted tree rather than on two artifacts: any
# source marker that keeps the preload also keeps an eager builtin load, so two
# artifacts would measure that load as well. The arms are the artifact's own Node
# command line with `--require <bootstrap>` (what the launcher passed before) and
# with `__NUB_COMPILED_BOOTSTRAP=<bootstrap>` (what it passes now), same chunks,
# same warm compile cache. Every Node arm is `env … node …` with no shell wrapper,
# so the arms differ from the artifact by one exec, as the launcher itself does.
# Three instruments, because no single one has resolved terms this size on a
# shared runner: hyperfine with interleaved duplicate baselines (wall),
# `cpu-ab.sh` (child CPU, in-run control), and `performance.nodeTiming` minimums
# (in-process, no fork noise).
set -euo pipefail
NUB_BIN="${NUB_BIN:?}"
NUB_BIN_BEFORE="${NUB_BIN_BEFORE:-}"     # a nub built before single-file emission
NUB_BIN_EAGER_POLYFILLS="${NUB_BIN_EAGER_POLYFILLS:-}" # a nub built before the lazy polyfill globals
NUB_BIN_LAZY_CACHE="${NUB_BIN_LAZY_CACHE:-}"           # a nub built before the compile-cache chunk shape
REPO="${REPO:-$PWD}"
LAUNCHER_BEFORE="${LAUNCHER_BEFORE:-}"   # macOS: a launcher built without -dead_strip_dylibs
NODE_OLD_PIN="${NODE_OLD_PIN:-22.23.2}"  # a target that still needs the bundled polyfills; empty skips that arm
NODE_PIN="${NODE_PIN:-$(node -v | tr -d v)}"
RUNS="${RUNS:-400}"
ROUNDS="${ROUNDS:-40}"

D=$(mktemp -d)
cd "$D" || exit 1
pwd
printf '%s\n' "$NODE_PIN" > .node-version
printf '{"name":"d","type":"module"}\n' > package.json
printf 'console.log("hello, nub");\n' > hello.ts
cat > probe.mjs <<'EOF2'
const user = performance.now();
const t = performance.nodeTiming;
process.stdout.write(`${t.bootstrapComplete.toFixed(3)} ${user.toFixed(3)}\n`);
EOF2
echo "" > nil.mjs

compile() { # compile <nub> <source> <out>
  "$1" compile "$2" --out "$3" > "compile-$(basename "$3").log" 2>&1 ||
    { echo "COMPILE FAILED: $3"; tail -30 "compile-$(basename "$3").log"; exit 1; }
}
compile "$NUB_BIN" hello.ts ./art
compile "$NUB_BIN" probe.mjs ./probe
export XDG_CACHE_HOME="$D/cache"
# The launcher respects an inherited NODE_COMPILE_CACHE; the cache under test is
# the artifact's own, so an ambient one (a dev shell exports it) must not reach it.
unset NODE_COMPILE_CACHE
./art >/dev/null; ./art >/dev/null; ./probe >/dev/null; ./probe >/dev/null

# The extracted tree behind an artifact: its app dir, bootstrap, and compile
# cache. The launcher names the tree itself — its timing print carries the
# NODE_COMPILE_CACHE it sets, keyed by the same short app key as the extraction —
# so every arm reads its key off the artifact it belongs to, and no tree has to
# be told apart from a sibling by its contents or its position in a listing.
tree_of() { # tree_of <artifact> -> its compile-app dir
  local key
  key=$(__NUB_LAUNCHER_TIMING=1 "$1" 2>&1 >/dev/null | sed -n 's/.*NODE_COMPILE_CACHE=[^ ]*\/compile-v8\/\([0-9a-f]*\).*/\1/p' | head -1)
  [ -n "$key" ] && [ -d "$D/cache/nub/compile-app/$key" ] ||
    { echo "could not read the app key of $1 off the launcher's timing print" >&2; __NUB_LAUNCHER_TIMING=1 "$1" >&2; exit 1; }
  echo "$D/cache/nub/compile-app/$key"
}
APP=$(tree_of ./art); B="$APP/__nub_compile_bootstrap.cjs"; CC="$D/cache/nub/compile-v8/$(basename "$APP")"
PAPP=$(tree_of ./probe); PB="$PAPP/__nub_compile_bootstrap.cjs"; PCC="$D/cache/nub/compile-v8/$(basename "$PAPP")"
N=$(echo "$D"/cache/nub/compile-node/*/node)
for path in "$N" "$B" "$PB" "$CC" "$PCC"; do
  [ -e "$path" ] || { echo "MISSING after two warm runs: $path"; find "$D/cache" -maxdepth 3 | sort; exit 1; }
done
# The flags the launcher itself passes for this target, read off its timing
# print rather than copied here: they are version-dependent.
flags_of() { # flags_of <artifact> -> the Node flags before the entry path
  __NUB_LAUNCHER_TIMING=1 "$1" 2>&1 >/dev/null | sed -n 's/^ *argv: //p' | sed 's# /[^ ]*$##'
}
FLAGS=$(flags_of ./art)
[ -n "$FLAGS" ] || { echo "could not read the launcher's flags"; __NUB_LAUNCHER_TIMING=1 ./art; exit 1; }
[ "$(node -e 'process.stdout.write("OK")')" = OK ] || { echo "PATH node is not plain node"; exit 1; }
[ "$("$N" -e 'process.stdout.write("OK")')" = OK ] || { echo "extracted node is not plain node"; exit 1; }
echo "extracted tree: $(ls "$APP" | tr '\n' ' ')"
echo "flags: $FLAGS"
__NUB_LAUNCHER_TIMING=1 ./art 2>&1 | grep -E "env:" | sed "s#$D##g" | cut -c1-160

# Every Node arm carries the artifact's own flags and compile cache; only the
# bootstrap delivery differs. No wrapper script: `env` is one exec, like the launcher.
STANDALONE="env NODE_COMPILE_CACHE=$CC __NUB_COMPILED_BOOTSTRAP=$B $N $FLAGS $APP/hello.mjs"
PRELOAD="env NODE_COMPILE_CACHE=$CC $N $FLAGS --require=$B $APP/hello.mjs"
$STANDALONE; $PRELOAD

report() { # report <hyperfine json>
  node -e '
const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).results;
const m = b => Math.min(...b.times) * 1000;
const bases = r.filter(b => b.command.includes("baseline")).map(m);
const spread = Math.max(...bases) - Math.min(...bases);
console.log(`BASELINE SPREAD: ${spread.toFixed(2)} ms`);
const b0 = Math.min(...bases);
for (const b of r) console.log(`${b.command.padEnd(12)} min ${m(b).toFixed(2).padStart(7)} ms   over-node ${(m(b) - b0).toFixed(2).padStart(6)}`);' "$1"
}

echo "--- wall clock (hyperfine, minimums; the baseline spread is the bar) ---"
hyperfine -N -i --warmup 30 --min-runs "$RUNS" --style none --export-json r.json \
  -n 'baseline-A'  "$N nil.mjs" \
  -n 'standalone'  "$STANDALONE" \
  -n 'preload'     "$PRELOAD" \
  -n 'baseline-B'  "$N nil.mjs" \
  -n 'artifact'    "./art" \
  -n 'baseline-C'  "$N nil.mjs" || true
report r.json

echo "--- child CPU (cpu-ab.sh; control repeats arm 1, its gap is the bar) ---"
PER=5 ROUNDS="$ROUNDS" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
  "standalone=$STANDALONE" \
  "preload=$PRELOAD" \
  "artifact=./art" \
  "control=$STANDALONE"

stats() { awk -v c="$1" '{ if (NR == 1 || $c < mn) mn = $c; sum += $c } END { printf "min %.3f mean %.3f", mn, sum / NR }' "$2"; }
timing() { # timing <label> <file> -> one summary line
  printf '%-18s bootstrapComplete %s   user-code %s\n' "$1" "$(stats 1 "$2")" "$(stats 2 "$2")"
}
echo "--- in-process nodeTiming, minimums of 150 (bootstrapComplete, user code) ---"
: > s.txt; : > p.txt; : > n.txt
for _ in $(seq 1 150); do
  env NODE_COMPILE_CACHE=$PCC __NUB_COMPILED_BOOTSTRAP=$PB "$N" $FLAGS "$PAPP/probe.mjs" >> s.txt
  env NODE_COMPILE_CACHE=$PCC "$N" $FLAGS --require="$PB" "$PAPP/probe.mjs" >> p.txt
  "$N" probe.mjs >> n.txt
done
timing standalone s.txt; timing preload p.txt; timing plain-node n.txt
echo "(plain node = the probe source with no flags and no cache)"

if [ -n "$NUB_BIN_BEFORE" ]; then
  echo "--- single-file emission: the same sources compiled by the pre-change nub ---"
  compile "$NUB_BIN_BEFORE" hello.ts ./art-before
  compile "$NUB_BIN_BEFORE" probe.mjs ./probe-before
  ./art-before >/dev/null; ./art-before >/dev/null; ./probe-before >/dev/null; ./probe-before >/dev/null
  APPB=$(tree_of ./art-before); BB="$APPB/__nub_compile_bootstrap.cjs"; CCB="$D/cache/nub/compile-v8/$(basename "$APPB")"
  PAPPB=$(tree_of ./probe-before); PBB="$PAPPB/__nub_compile_bootstrap.cjs"; PCCB="$D/cache/nub/compile-v8/$(basename "$PAPPB")"
  echo "after : $(ls "$APP" | tr '\n' ' ')"
  echo "before: $(ls "$APPB" | tr '\n' ' ')"
  STANDALONE_BEFORE="env NODE_COMPILE_CACHE=$CCB __NUB_COMPILED_BOOTSTRAP=$BB $N $FLAGS $APPB/hello.mjs"
  $STANDALONE_BEFORE
  hyperfine -N -i --warmup 30 --min-runs "$RUNS" --style none --export-json f.json \
    -n 'baseline-A'   "$N nil.mjs" \
    -n 'after'        "$STANDALONE" \
    -n 'before'       "$STANDALONE_BEFORE" \
    -n 'baseline-B'   "$N nil.mjs" \
    -n 'art-after'    "./art" \
    -n 'art-before'   "./art-before" \
    -n 'baseline-C'   "$N nil.mjs" || true
  report f.json
  PER=5 ROUNDS="$ROUNDS" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
    "after=$STANDALONE" \
    "before=$STANDALONE_BEFORE" \
    "art-after=./art" \
    "art-before=./art-before" \
    "control=$STANDALONE"
  : > sb.txt
  for _ in $(seq 1 150); do
    env NODE_COMPILE_CACHE=$PCCB __NUB_COMPILED_BOOTSTRAP=$PBB "$N" $FLAGS "$PAPPB/probe.mjs" >> sb.txt
  done
  timing after s.txt; timing before sb.txt
fi

if [ -n "$NUB_BIN_LAZY_CACHE" ]; then
  echo "--- eager startup compilation: the same sources compiled by a nub whose chunk left the compile cache incomplete ---"
  compile "$NUB_BIN_LAZY_CACHE" hello.ts ./art-lazy
  compile "$NUB_BIN_LAZY_CACHE" probe.mjs ./probe-lazy
  ./art-lazy >/dev/null; ./art-lazy >/dev/null; ./probe-lazy >/dev/null; ./probe-lazy >/dev/null
  LAPP=$(tree_of ./art-lazy); LPAPP=$(tree_of ./probe-lazy)
  echo "after : $(ls "$APP" | tr '\n' ' ')"
  echo "before: $(ls "$LAPP" | tr '\n' ' ')"
  echo "compile cache, after : $(ls -l "$D"/cache/nub/compile-v8/"$(basename "$APP")"/*/* | awk '{print $5}' | tr '\n' ' ') bytes"
  echo "compile cache, before: $(ls -l "$D"/cache/nub/compile-v8/"$(basename "$LAPP")"/*/* | awk '{print $5}' | tr '\n' ' ') bytes"
  LB="$LAPP/__nub_compile_bootstrap.cjs"; LCC="$D/cache/nub/compile-v8/$(basename "$LAPP")"
  LPB="$LPAPP/__nub_compile_bootstrap.cjs"; LPCC="$D/cache/nub/compile-v8/$(basename "$LPAPP")"
  LAZY="env NODE_COMPILE_CACHE=$LCC __NUB_COMPILED_BOOTSTRAP=$LB $N $FLAGS $LAPP/hello.mjs"
  hyperfine -N -i --warmup 30 --min-runs "$RUNS" --style none --export-json e.json \
    -n 'baseline-A'  "$N nil.mjs" \
    -n 'after'       "$STANDALONE" \
    -n 'before'      "$LAZY" \
    -n 'baseline-B'  "$N nil.mjs" \
    -n 'art-after'   "./art" \
    -n 'art-before'  "./art-lazy" \
    -n 'baseline-C'  "$N nil.mjs" || true
  report e.json
  PER=5 ROUNDS="$ROUNDS" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
    "after=$STANDALONE" \
    "before=$LAZY" \
    "art-after=./art" \
    "art-before=./art-lazy" \
    "control=$STANDALONE"
  : > sl.txt
  for _ in $(seq 1 150); do
    env NODE_COMPILE_CACHE=$PCC __NUB_COMPILED_BOOTSTRAP=$PB "$N" $FLAGS "$PAPP/probe.mjs" >> s.txt
    env NODE_COMPILE_CACHE=$LPCC __NUB_COMPILED_BOOTSTRAP=$LPB "$N" $FLAGS "$LPAPP/probe.mjs" >> sl.txt
  done
  timing after s.txt; timing before sl.txt
fi

# The polyfill arm's "before" is a nub that still installed them eagerly; with
# only the pre-single-file nub available, its several chunks are priced in too.
OLD_BEFORE="${NUB_BIN_EAGER_POLYFILLS:-$NUB_BIN_BEFORE}"
if [ -n "$OLD_BEFORE" ] && [ -n "$NODE_OLD_PIN" ]; then
  echo "--- an older target ($NODE_OLD_PIN): the polyfills it still needs, lazy vs eager ---"
  mkdir -p old && cp probe.mjs hello.ts old/ && printf '{"name":"o","type":"module"}\n' > old/package.json
  compile_old() { # compile_old <nub> <source> <out>
    "$1" compile "old/$2" --target "$NODE_OLD_PIN" --out "$3" > "compile-$(basename "$3").log" 2>&1 ||
      { echo "COMPILE FAILED: $3"; tail -30 "compile-$(basename "$3").log"; exit 1; }
  }
  compile_old "$NUB_BIN" probe.mjs ./old-probe-after
  compile_old "$OLD_BEFORE" probe.mjs ./old-probe-before
  compile_old "$NUB_BIN" hello.ts ./old-art-after
  compile_old "$OLD_BEFORE" hello.ts ./old-art-before
  for a in ./old-probe-after ./old-probe-before ./old-art-after ./old-art-before; do $a >/dev/null; $a >/dev/null; done
  # The launcher dedups an untrimmed embedded Node against an official one of
  # the same version in nub's store, so on a box that has provisioned this
  # version nothing is extracted; the arms must run the Node the artifact does.
  NOLD=
  for n in "$D"/cache/nub/compile-node/"$NODE_OLD_PIN"-*/node "$D/cache/nub/node/$NODE_OLD_PIN/bin/node" "$HOME/.cache/nub/node/$NODE_OLD_PIN/bin/node"; do
    [ -x "$n" ] && { NOLD=$n; break; }
  done
  [ -x "$NOLD" ] || { echo "no Node $NODE_OLD_PIN found in the extraction cache or nub's store"; ls "$D/cache/nub/compile-node"; exit 1; }
  [ "$("$NOLD" -e 'process.stdout.write("OK")')" = OK ] || { echo "extracted old node is not plain node"; exit 1; }
  "$NOLD" -v
  OFLAGS=$(flags_of ./old-probe-after)
  echo "flags: $OFLAGS"
  OPA=$(tree_of ./old-probe-after); OPB=$(tree_of ./old-probe-before)
  echo "after : $(ls "$OPA" | tr '\n' ' ')"
  echo "before: $(ls "$OPB" | tr '\n' ' ')"
  : > oa.txt; : > ob.txt; : > on.txt
  for _ in $(seq 1 150); do
    env NODE_COMPILE_CACHE="$D/cache/nub/compile-v8/$(basename "$OPA")" __NUB_COMPILED_BOOTSTRAP="$OPA/__nub_compile_bootstrap.cjs" "$NOLD" $OFLAGS "$OPA/probe.mjs" >> oa.txt
    env NODE_COMPILE_CACHE="$D/cache/nub/compile-v8/$(basename "$OPB")" __NUB_COMPILED_BOOTSTRAP="$OPB/__nub_compile_bootstrap.cjs" "$NOLD" $OFLAGS "$OPB/probe.mjs" >> ob.txt
    "$NOLD" probe.mjs >> on.txt
  done
  timing after oa.txt; timing before ob.txt; timing plain-node on.txt
  hyperfine -N -i --warmup 30 --min-runs "$RUNS" --style none --export-json o.json \
    -n 'baseline-A'  "$NOLD nil.mjs" \
    -n 'art-after'   "./old-art-after" \
    -n 'art-before'  "./old-art-before" \
    -n 'baseline-B'  "$NOLD nil.mjs" || true
  report o.json
  PER=5 ROUNDS="$ROUNDS" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
    "art-after=./old-art-after" \
    "art-before=./old-art-before" \
    "control=./old-art-after"
fi

if [ -n "$LAUNCHER_BEFORE" ] && [ "$(uname -s)" = Darwin ]; then
  echo "--- macOS: the launcher with and without the Apple frameworks ---"
  otool -L "$LAUNCHER_BEFORE" | tail -n +2
  __NUB_LAUNCHER_TEMPLATE="$LAUNCHER_BEFORE" compile "$NUB_BIN" hello.ts ./art-launcher-before
  otool -L ./art-launcher-before | tail -n +2 | wc -l; otool -L ./art | tail -n +2 | wc -l
  ./art-launcher-before >/dev/null; ./art-launcher-before >/dev/null
  PER=5 ROUNDS="$ROUNDS" bash "$REPO/tests/preamble-eval/cpu-ab.sh" \
    "art-after=./art" \
    "art-before=./art-launcher-before" \
    "probe-after=env __NUB_COMPILED_LAUNCHER_MODE=probe ./art" \
    "probe-before=env __NUB_COMPILED_LAUNCHER_MODE=probe ./art-launcher-before" \
    "control=./art"
  hyperfine -N -i --warmup 30 --min-runs "$RUNS" --style none --export-json l.json \
    -n 'baseline-A' "$N nil.mjs" \
    -n 'art-after'  './art' \
    -n 'art-before' './art-launcher-before' \
    -n 'baseline-B' "$N nil.mjs" || true
  report l.json
fi
