#!/usr/bin/env bash
# Price the warm-start changes on this branch on a quiet runner, both OSes.
#
#   1. the bootstrap preload dropped for a payload that needs neither eager
#      builtin (`Manifest::standalone_preamble`);
#   2. single-file emission — the CommonJS and runtime chunks folded into the
#      entry chunk (`NUB_BIN_BEFORE` names a nub built before that change); and
#   3. on macOS only, the launcher no longer linking Security.framework and
#      CoreFoundation (`LAUNCHER_BEFORE` names a template linked without
#      `-dead_strip_dylibs`).
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
REPO="${REPO:-$PWD}"
LAUNCHER_BEFORE="${LAUNCHER_BEFORE:-}"   # macOS: a launcher built without -dead_strip_dylibs
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
# cache (the launcher keys the cache by the same short app key as the extraction).
# Globs, not `find | while | head`: under `pipefail` that pipeline's status is the
# loop's LAST `[ -f ]` test, false whenever the matching directory is not the
# final one, and `set -e` then exits on the assignment without a word.
app_dir() { # app_dir <entry chunk name> -> the one compile-app dir holding it
  local hits=("$D"/cache/nub/compile-app/*/"$1")
  [ "${#hits[@]}" -eq 1 ] && [ -f "${hits[0]}" ] ||
    { echo "expected exactly one extracted $1, found: ${hits[*]}" >&2; find "$D/cache" -maxdepth 3 | sort >&2; exit 1; }
  dirname "${hits[0]}"
}
APP=$(app_dir hello.mjs); B="$APP/__nub_compile_bootstrap.cjs"; CC="$D/cache/nub/compile-v8/$(basename "$APP")"
PAPP=$(app_dir probe.mjs); PB="$PAPP/__nub_compile_bootstrap.cjs"; PCC="$D/cache/nub/compile-v8/$(basename "$PAPP")"
N=$(echo "$D"/cache/nub/compile-node/*/node)
for path in "$N" "$B" "$PB" "$CC" "$PCC"; do
  [ -e "$path" ] || { echo "MISSING after two warm runs: $path"; find "$D/cache" -maxdepth 3 | sort; exit 1; }
done
FLAGS="--disable-warning=ExperimentalWarning --experimental-vm-modules --experimental-eventsource --experimental-addon-modules --experimental-import-text --experimental-ffi --experimental-vfs --experimental-stream-iter"
[ "$(node -e 'process.stdout.write("OK")')" = OK ] || { echo "PATH node is not plain node"; exit 1; }
[ "$("$N" -e 'process.stdout.write("OK")')" = OK ] || { echo "extracted node is not plain node"; exit 1; }
echo "extracted tree: $(ls "$APP" | tr '\n' ' ')"
__NUB_LAUNCHER_TIMING=1 ./art 2>&1 | grep -E "argv:|env:" | sed "s#$D##g" | cut -c1-160

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
  # A different bundle is a different app key, so the before-trees sit beside
  # the after-trees; tell them apart by which one the after-artifact extracted.
  before_dir() { # before_dir <entry chunk name>
    local d
    for d in "$D"/cache/nub/compile-app/*/; do
      d="${d%/}"
      [ -f "$d/$1" ] && [ "$d" != "$APP" ] && [ "$d" != "$PAPP" ] && { echo "$d"; return 0; }
    done
    echo "no before-tree holding $1" >&2; exit 1
  }
  APPB=$(before_dir hello.mjs); BB="$APPB/__nub_compile_bootstrap.cjs"; CCB="$D/cache/nub/compile-v8/$(basename "$APPB")"
  PAPPB=$(before_dir probe.mjs); PBB="$PAPPB/__nub_compile_bootstrap.cjs"; PCCB="$D/cache/nub/compile-v8/$(basename "$PAPPB")"
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
