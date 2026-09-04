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
# 26.5.1, not the newest 26: it is the newest Node the INSTALLED @yao-pkg/pkg-fetch
# can build (patches.json in 3.6.5), and a single-Node comparison is worth more
# than three fresher patch releases. Everything — the runner's node, SEA, caxa,
# pkg and nub — is on this one version. Re-check with:
#   node -p 'Object.keys(require("@yao-pkg/pkg-fetch/patches/patches.json")).at(-1)'
NODE_PIN="${NODE_PIN:-26.5.1}"
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

  # SEA again with `useCodeCache`, its own supported option. Without it SEA is the
  # only packaged row with no bytecode cache at all, which would flatter caxa and
  # nub on the fixture that has a module graph.
  cat > "sea-cc-$F.json" <<EOF
{ "main": "$B", "output": "$W/art/seacc-$F", "disableExperimentalSEAWarning": true, "useCodeCache": true }
EOF
  if node --build-sea "sea-cc-$F.json" > "build-seacc-$F.log" 2>&1; then
    chmod +x "art/seacc-$F"; BUILT+=("seacc-$F")
  else note_drop "seacc-$F" "$(tail -3 "build-seacc-$F.log" | tr '\n' ' ')"; fi

  echo "== $F / @yao-pkg/pkg"
  # NODE_PIN must be a version pkg-fetch can actually build, or every pkg row drops
  # with "No available node version satisfies 'vX'". The gate is the `patches.json`
  # baked into the INSTALLED @yao-pkg/pkg-fetch, not the assets on its GitHub
  # release — 3.6.5 (the newest, and what pkg 6.22.0 pins) tops out at v26.5.1 even
  # though the v3.6 release has `node-v26.8.1-linux-x64` uploaded. To keep every
  # tool on one Node, pin to the newest version this prints:
  #   node -p 'Object.keys(require("@yao-pkg/pkg-fetch/patches/patches.json"))'
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
#
# `timeout` is GNU coreutils. macOS does not ship it and the GitHub macOS runner
# image installs no coreutils, so hardcoding it made the guard itself the thing
# that failed: every artifact resolved to "command not found", printed nothing,
# and was dropped as an output mismatch. Both macOS legs produced zero rows that
# way while the unguarded plain-node baselines ran fine. Resolve the guard once,
# and accept having none rather than run the whole comparison against a command
# that does not exist.
# Deliberately an unquoted string, not an array: macOS ships bash 3.2, where
# expanding an EMPTY array under `set -u` is itself an unbound-variable error.
if command -v timeout >/dev/null 2>&1; then GUARD="timeout 30"
elif command -v gtimeout >/dev/null 2>&1; then GUARD="gtimeout 30"
else GUARD=""; fi
echo
echo "== verifying output and warming (ulimit -u 800, guard: ${GUARD:-none})"
for F in hello cli; do
  EXPECTED=$(node "bundles/$F.cjs")
  for A in "sea-$F" "seacc-$F" "pkg-$F" "caxa-$F" "nub-$F" "nubown-$F"; do
    [ -x "art/$A" ] || continue
    # Keep stderr. Discarding it is what hid "timeout: command not found" behind a
    # bare "got []" for a whole run on both macOS legs.
    GOT=$( (ulimit -u 800; $GUARD "./art/$A") 2>"err-$A.log" )
    if [ "$GOT" = "$EXPECTED" ]; then
      (ulimit -u 800; $GUARD "./art/$A") >/dev/null 2>&1   # second warm run
      echo "  ok   $A"
    else
      note_drop "$A" "output mismatch: got [$GOT] want [$EXPECTED]; stderr: $(tr '\n' ' ' < "err-$A.log" | cut -c1-200)"
      mv "art/$A" "art/.bad-$A"
    fi
  done
done

for F in hello cli; do
  mkdir -p "$W/nodecc-$F.v8"
  NODE_COMPILE_CACHE="$W/nodecc-$F.v8" node "bundles/$F.cjs" > /dev/null 2>&1
  echo "  ok   nodecc-$F (plain node, warm V8 compile cache)"
done

# Runner identity. Two ubuntu-latest runs of this exact harness (33925518838 and
# 33926407075) produced plain-node minimums of 13.90 ms and 22.79 ms for the same
# 22-byte script — the runner varies by ~1.6x. Every row inside ONE run scales
# with it, so the RATIO column reproduces across runners where the absolute
# milliseconds do not. Record what we ran on so a reader can see which is which.
echo
echo "== runner"
echo "  uname              $(uname -srm)"
if [ "$OS" = linux ]; then
  echo "  cpus               $(nproc)"
  echo "  model              $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//')"
  echo "  memtotal           $(grep -m1 MemTotal /proc/meminfo | awk '{printf "%.1f GB", $2/1048576}')"
else
  echo "  cpus               $(sysctl -n hw.ncpu)"
  echo "  model              $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo '?')"
  echo "  memtotal           $(sysctl -n hw.memsize | awk '{printf "%.1f GB", $1/1073741824}')"
fi

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
echo
echo "== V8 compile cache (NODE_COMPILE_CACHE) — NOT uniform across these rows"
echo "  plain node         off"
echo "  nodecc             ON    (plain node, NODE_COMPILE_CACHE set by this script)"
echo "  sea                off   (the sea config sets no useCodeCache)"
echo "  seacc              ON    (sea config useCodeCache: true)"
echo "  caxa               ON    (stub childEnv, stubs/stub.go — points at its extraction dir)"
echo "  nub / nubown       ON    (launcher compile_cache_dir — \$cache/compile-v8/<key>)"
echo "  => on a fixture with a real module graph, caxa and nub start with warm V8"
echo "     bytecode that plain node and SEA do not have, so their margins over the"
echo "     baseline understate their true packaging overhead. The 'hello' fixture has"
echo "     nothing to cache, which is why it is the clean apples-to-apples row, and"
echo "     'caxa-nocc-*' below isolates the same effect directly."

# ------------------------------------------------------------- 5. warm measure
run_table() {
  local F="$1"
  local base="node $W/bundles/$F.cjs"
  local letters=(A B C D E F G)
  local -a args=()
  local n=0
  args+=(-n "baseline-${letters[0]} (plain node)" "$base")
  # `caxa-nocc-*` is the SAME caxa artifact with its V8 compile cache switched off.
  # caxa's stub sets NODE_COMPILE_CACHE unconditionally, so its margin over the
  # baseline on a fixture with a module graph is stub overhead MINUS a bytecode-cache
  # saving that plain node and SEA never get. This row separates the two, and the
  # difference between it and `caxa-*` is what the cache is worth on this fixture.
  # `nodecc-*` is plain node with the SAME V8 compile cache caxa and nub give
  # themselves. It is the row that makes the comparison separable: a packaged
  # row's margin over `baseline-*` is packaging overhead MINUS a cache saving,
  # while its margin over `nodecc-*` is the packaging overhead alone.
  for A in "nodecc-$F" "sea-$F" "seacc-$F" "pkg-$F" "caxa-$F" "caxanocc-$F" "nub-$F" "nubown-$F"; do
    local art="$A" pre="" full=""
    case "$A" in
      nodecc-*)   full="NODE_COMPILE_CACHE=$W/nodecc-$F.v8 node $W/bundles/$F.cjs" ;;
      caxanocc-*) art="caxa-$F"; pre="CAXA_DISABLE_COMPILE_CACHE=1 " ;;
    esac
    if [ -z "$full" ]; then
      [ -x "art/$art" ] || continue
      full="$pre$W/art/$art"
    fi
    args+=(-n "$A" "$full")
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
#
# The nub prepare wipes the WHOLE cache root, not just `compile-app`. A compiled
# artifact keeps three separate trees under it — `compile-node` (the zstd Node it
# decompresses once), `compile-app` (the bundle), and `compile-v8` (Node's code
# cache) — and step 2 of the launcher explicitly skips the Node decompression
# when a compatible Node is already cached. Clearing only `compile-app` therefore
# re-extracts the cheapest tree against a warm Node and a warm V8 cache, and
# reports a "first run" an order of magnitude below the real one.
echo
echo "######## FIRST RUN (extraction) — $OS-$ARCH"
for F in hello cli; do
  for A in "nub-$F" "nubown-$F" "caxa-$F"; do
    [ -x "art/$A" ] || continue
    case "$A" in
      caxa-*) PREP="rm -rf $TMPDIR/caxa" ;;
      *)      PREP="rm -rf $XDG_CACHE_HOME/nub" ;;
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

echo
echo "######## ARTIFACT SIZES"
ls -l art/ | awk 'NR>1 {printf "  %-22s %10.1f MB\n", $9, $5/1000000}'

echo
echo "######## DROPPED"
if [ ${#DROPPED[@]} -eq 0 ]; then echo "  (none)"; else printf '  %s\n' "${DROPPED[@]}"; fi
echo
echo "raw json under: $W"
