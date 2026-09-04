#!/usr/bin/env bash
# Warm-start comparison of `nub compile` against the other Node.js single-file
# packaging tools, on one pinned Node and one bundled application per fixture.
#
# The dev Mac cannot host this: its noise floor (~1.4 ms) is a large fraction of
# the gaps being separated, and it is routinely under load from other builds. Run
# it on a CI runner (.github/workflows/packaging-bench.yml).
#
#   NUB_BIN=... __NUB_LAUNCHER_TEMPLATE=... bash tests/packaging-bench/bench.sh
#
# Every table interleaves DUPLICATE baselines — the same `node <bundle>` command
# two or three times, spread through the run order. The spread between them is
# the error bar, and a row whose margin over its neighbour is smaller than that
# spread is not a result.
set -uo pipefail

NUB_BIN="${NUB_BIN:?set NUB_BIN to a nub built with --features compile}"
NODE_PIN="${NODE_PIN:-26.8.1}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WARMUP="${WARMUP:-30}"
MIN_RUNS="${MIN_RUNS:-150}"

case "$(uname -s)" in
  Linux)  OS=linux;  PKG_PLAT=linux;  CAXA_OK=1 ;;
  Darwin) OS=darwin; PKG_PLAT=macos;  CAXA_OK=1 ;;
  *) echo "unsupported host" >&2; exit 2 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH=x64; NUB_PLAT=$OS-x64 ;;
  arm64|aarch64) ARCH=arm64; NUB_PLAT=$OS-arm64 ;;
esac

W="${BENCH_WORKDIR:-$(mktemp -d)}"
mkdir -p "$W"
cd "$W" || { echo "cannot enter $W" >&2; exit 2; }
echo "workdir: $(pwd)"
export XDG_CACHE_HOME="$W/xdg-cache"      # nub's extraction root, ours to wipe
export TMPDIR="$W/tmp"; mkdir -p "$TMPDIR" # caxa's extraction root, ours to wipe

# ---------------------------------------------------------------- 1. toolchain
cp -R "$HERE/app" ./app
cat > package.json <<'EOF'
{ "name": "packaging-bench", "private": true, "version": "1.0.0" }
EOF
echo "$NODE_PIN" > .node-version

echo "== installing tooling and app dependencies"
npm install --silent --no-audit --no-fund --prefix "$W/app" > install-app.log 2>&1
echo "  app deps: $?"
npm install --silent --no-audit --no-fund --no-save \
  esbuild@0.28.2 @yao-pkg/pkg@6.22.0 @cdxgen/caxa@3.1.1 postject@1.0.0-alpha.6 \
  > install-tools.log 2>&1
echo "  tools: $?"
ESBUILD=./node_modules/.bin/esbuild
PKG=./node_modules/.bin/pkg
CAXA=./node_modules/.bin/caxa

# ---------------------------------------------------------------- 2. one bundle
# SEA and pkg cannot bundle, so every tool is fed the SAME esbuild output. It is
# minified because a shipped CLI is, and because nub's own path minifies by
# default — an unminified shared bundle would hand nub a smaller parse job than
# its competitors on the row that is meant to hold the application constant.
mkdir -p bundles
for F in hello cli; do
  "$ESBUILD" "app/$F.mjs" --bundle --minify --platform=node --format=cjs \
    --target=node26 --outfile="bundles/$F.cjs" --metafile="bundles/$F.meta.json" \
    > "esbuild-$F.log" 2>&1
  RC=$?
  MODULES=$(node -p "Object.keys(require('$W/bundles/$F.meta.json').outputs['bundles/$F.cjs'].inputs).length" 2>/dev/null)
  echo "  bundled $F: exit $RC, $(wc -c < "bundles/$F.cjs") bytes, ${MODULES:-?} modules"
done

# ------------------------------------------------------------- 3. build artifacts
declare -a BUILT=() DROPPED=()
note_drop() { DROPPED+=("$1: $2"); echo "  DROPPED $1 — $2"; }

for F in hello cli; do
  B="$W/bundles/$F.cjs"

  echo "== $F / Node SEA"
  mkdir -p art
  # `--build-sea` is the one-step form (Node 25.5+, and only in a build carrying
  # LIEF). Fall back to the original two-step sea-config + postject flow rather
  # than dropping the row, so a runner whose Node lacks LIEF still produces a
  # SEA number — it is the same artifact either way.
  cat > "sea-$F.json" <<EOF
{ "main": "$B", "output": "$W/blob-$F.blob", "disableExperimentalSEAWarning": true }
EOF
  SEA_HOW=""
  cat > "sea-build-$F.json" <<EOF
{ "main": "$B", "output": "$W/art/sea-$F", "disableExperimentalSEAWarning": true }
EOF
  if node --build-sea "sea-build-$F.json" > "build-sea-$F.log" 2>&1; then
    chmod +x "art/sea-$F"; BUILT+=("sea-$F"); SEA_HOW="node --build-sea"
  elif [ "$OS" = linux ] \
       && node --experimental-sea-config "sea-$F.json" >> "build-sea-$F.log" 2>&1 \
       && cp "$(command -v node)" "art/sea-$F" \
       && chmod +w "art/sea-$F" \
       && ./node_modules/.bin/postject "art/sea-$F" NODE_SEA_BLOB "$W/blob-$F.blob" \
            --sentinel-fuse NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2 \
            >> "build-sea-$F.log" 2>&1; then
    chmod +x "art/sea-$F"; BUILT+=("sea-$F"); SEA_HOW="sea-config + postject"
  else note_drop "sea-$F" "$(tail -3 "build-sea-$F.log" | tr '\n' ' ')"; fi
  echo "  sea built via: ${SEA_HOW:-FAILED}"

  echo "== $F / @yao-pkg/pkg"
  if "$PKG" "$B" --target "node${NODE_PIN}-${PKG_PLAT}-${ARCH}" \
       --output "art/pkg-$F" > "build-pkg-$F.log" 2>&1; then
    BUILT+=("pkg-$F")
  else note_drop "pkg-$F" "$(tail -3 "build-pkg-$F.log" | tr '\n' ' ')"; fi

  echo "== $F / @cdxgen/caxa"
  rm -rf "caxa-in-$F"; mkdir -p "caxa-in-$F"; cp "$B" "caxa-in-$F/app.cjs"
  if "$CAXA" --input "caxa-in-$F" --output "art/caxa-$F" \
       -- "{{caxa}}/node_modules/.bin/node" "{{caxa}}/app.cjs" \
       > "build-caxa-$F.log" 2>&1; then
    BUILT+=("caxa-$F")
  else note_drop "caxa-$F" "$(tail -3 "build-caxa-$F.log" | tr '\n' ' ')"; fi

  echo "== $F / nub compile (shared bundle)"
  # --no-minify because the input is already minified; nub still re-bundles it
  # through Rolldown, so this row is "nub running the same application", not
  # "nub running byte-identical output".
  if "$NUB_BIN" compile "$B" --no-minify --target "$NODE_PIN" \
       --out "art/nub-$F" > "build-nub-$F.log" 2>&1; then
    BUILT+=("nub-$F")
  else note_drop "nub-$F" "$(tail -5 "build-nub-$F.log" | tr '\n' ' ')"; fi

  echo "== $F / nub compile (own bundling path)"
  if "$NUB_BIN" compile "app/$F.mjs" --target "$NODE_PIN" \
       --out "art/nubown-$F" > "build-nubown-$F.log" 2>&1; then
    BUILT+=("nubown-$F")
  else note_drop "nubown-$F" "$(tail -5 "build-nubown-$F.log" | tr '\n' ' ')"; fi
done

# ------------------------------------------------------- 4. correctness + warm
# A wrong-output artifact would time as fast and mean nothing, so every row is
# checked against plain node's output BEFORE it is measured. `ulimit -u` is the
# fork-bomb guard: a compiled artifact publishes ITSELF as process.execPath.
echo
echo "== verifying output and warming (ulimit -u 800, timeout 30)"
for F in hello cli; do
  EXPECTED=$(node "bundles/$F.cjs")
  for A in "sea-$F" "pkg-$F" "caxa-$F" "nub-$F" "nubown-$F"; do
    [ -x "art/$A" ] || continue
    GOT=$( (ulimit -u 800; timeout 30 "./art/$A") 2>/dev/null )
    if [ "$GOT" = "$EXPECTED" ]; then
      (ulimit -u 800; timeout 30 "./art/$A") >/dev/null 2>&1   # second warm run
      echo "  ok   $A"
    else
      note_drop "$A" "output mismatch: got [$GOT] want [$EXPECTED]"
      mv "art/$A" "art/.bad-$A"
    fi
  done
done

echo
echo "== versions"
echo "  host node          $(node -v)   ($OS-$ARCH)"
echo "  pinned Node        $NODE_PIN"
echo "  nub                $("$NUB_BIN" --version 2>&1 | head -1)"
echo "  pkg                $("$PKG" --version 2>&1 | tail -1)"
echo "  caxa               $("$CAXA" --version 2>&1 | tail -1)"
echo "  SEA runtime        $(node -v)  (--build-sea copies the running node)"
echo "  caxa runtime       $(node -v)  (copies process.execPath)"
echo "  pkg runtime        node ${NODE_PIN} from pkg-fetch (a PATCHED Node build, not stock)"
echo "  nub runtime        node ${NODE_PIN}, embedded"

# ------------------------------------------------------------- 5. warm measure
run_table() {
  local F="$1"
  local base="node $W/bundles/$F.cjs"
  local letters=(A B C D E F G)
  local -a args=()
  local n=0
  args+=(-n "baseline-${letters[0]} (plain node)" "$base")
  for A in "sea-$F" "pkg-$F" "caxa-$F" "nub-$F" "nubown-$F"; do
    [ -x "art/$A" ] || continue
    args+=(-n "$A" "$W/art/$A")
    n=$((n + 1))
    # A duplicate baseline every two artifacts: the drift between these identical
    # commands is the only honest error bar on the rows around them.
    if [ $((n % 2)) -eq 0 ]; then
      args+=(-n "baseline-${letters[$((n / 2))]} (plain node)" "$base")
    fi
  done
  args+=(-n "baseline-Z (plain node)" "$base")
  echo
  echo "######## WARM START — fixture: $F — $OS-$ARCH"
  hyperfine -i --warmup "$WARMUP" --min-runs "$MIN_RUNS" --style none \
    --export-json "warm-$F.json" "${args[@]}" > "hyperfine-$F.log" 2>&1
  node "$HERE/summarize.mjs" "warm-$F.json"
}

ulimit -u 800
for F in hello cli; do run_table "$F"; done

# ------------------------------------------------------------ 6. first-run cost
# nub extracts its embedded Node into XDG_CACHE_HOME on first run; caxa extracts
# its payload into TMPDIR. Both are one-time, and blending them into the warm
# table would misrepresent both numbers, so they get their own table.
echo
echo "######## FIRST RUN (extraction) — $OS-$ARCH"
for F in hello cli; do
  for A in "nub-$F" "nubown-$F" "caxa-$F"; do
    [ -x "art/$A" ] || continue
    case "$A" in
      caxa-*) PREP="rm -rf $TMPDIR/caxa" ;;
      *)      PREP="rm -rf $XDG_CACHE_HOME/nub/compile-app" ;;
    esac
    hyperfine -i --warmup 0 --runs 12 --style none --prepare "$PREP" \
      --export-json "cold-$A.json" -n "$A (cold)" "$W/art/$A" > /dev/null 2>&1
    node -e '
      const r = JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")).results[0];
      const min = Math.min(...r.times)*1000, mean = r.mean*1000;
      console.log(`  ${r.command.padEnd(22)} min ${min.toFixed(1).padStart(8)} ms   mean ${mean.toFixed(1).padStart(8)} ms`);
    ' "cold-$A.json"
  done
done

if [ -n "$NEXE_BUILT" ]; then
  echo
  echo "######## nexe FOOTNOTE — hello only, Node $NEXE_TARGET, NOT comparable to the tables above"
  GOT=$( (ulimit -u 800; timeout 30 ./art/nexe-hello) 2>/dev/null )
  echo "  output check: [$GOT]"
  (ulimit -u 800; timeout 30 ./art/nexe-hello) >/dev/null 2>&1
  hyperfine -i --warmup "$WARMUP" --min-runs "$MIN_RUNS" --style none \
    --export-json warm-nexe.json \
    -n "baseline-A (plain node)" "node $W/bundles/hello.cjs" \
    -n "nexe-hello (Node $NEXE_TARGET)" "$W/art/nexe-hello" \
    -n "baseline-Z (plain node)" "node $W/bundles/hello.cjs" \
    > hyperfine-nexe.log 2>&1
  node "$HERE/summarize.mjs" warm-nexe.json
fi

echo
echo "######## ARTIFACT SIZES"
ls -l art/ | awk 'NR>1 {printf "  %-22s %10.1f MB\n", $9, $5/1000000}'

echo
echo "######## DROPPED"
if [ ${#DROPPED[@]} -eq 0 ]; then echo "  (none)"; else printf '  %s\n' "${DROPPED[@]}"; fi
echo
echo "raw json under: $W"
