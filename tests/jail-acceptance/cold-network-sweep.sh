#!/usr/bin/env bash
# Which per-OS `network: null` overlays are warm-cache artefacts?
#
# ⛔⛔ THE DEFECT THIS HUNTS. A per-OS overlay whose `network` is `null` REMOVES the egress the outer band
# granted, so on that one platform the package installs with no network. 87 overlays across the catalog do
# this (49 macOS, 29 linux, 9 win), and every one of them withdraws a network the outer band had granted.
# `electron@33.4.11` proved the shape is dangerous: measured cold, its postinstall downloads
# `electron-v33.4.11-darwin-arm64.zip` from github.com, and the withdrawal turned a cold macOS install into
# `getaddrinfo ENOTFOUND github.com` with no artefact. The measurement that produced the withdrawal had a
# warm download cache, so the package genuinely needed no network THAT time.
#
# ⛔ A WITHDRAWAL IS NOT WRONG BY DEFAULT, which is why this measures instead of sweeping. A package can
# legitimately need network on Linux and not on macOS — a tarball may bundle a darwin prebuilt and no
# linux one. So each overlay is judged by a COLD install, and only a package that fails cold and succeeds
# with network restored is a confirmed artefact.
#
# THE CONTROL IS THE POINT. Each package is installed TWICE from a fresh home: once with the catalog as
# shipped, once with only that one overlay's `network` withdrawal dropped. A package that fails BOTH ways is
# broken for another reason and is reported separately rather than counted — that distinction is what keeps
# this from manufacturing 49 findings out of unrelated breakage.
#
# Usage: NUB=/path/to/nub ./cold-network-sweep.sh [--os macos|linux|win] [--limit N] [--only pkg]
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# Overridable so a control can point the sweep at a PREVIOUS catalog and prove the verdict changed.
CATALOG="${CATALOG:-$ROOT/crates/nub-sandbox/data/build-jail-catalog-v2.json}"
NUB="${NUB:-$ROOT/target/fast/nub}"
OS_KEY=macos; LIMIT=0; ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --os) OS_KEY="$2"; shift 2 ;;
    --limit) LIMIT="$2"; shift 2 ;;
    --only) ONLY="$2"; shift 2 ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
[ -x "$NUB" ] || { echo "no nub binary at $NUB" >&2; exit 2; }
OUT="${OUT:-/tmp/cold-network-sweep-$OS_KEY.tsv}"
: > "$OUT"

# The worklist comes from the catalog itself, so it cannot drift from what ships.
# NODE, NOT PYTHON, and that is what makes this runnable on Windows. That box has no python3 and no `py`;
# it DOES have node (nub runs on the user's Node, so node is present everywhere nub is) and bash from Git
# for Windows. A python harness could never judge the win overlays — the same "blocked on a Windows runner"
# excuse this effort already paid for once. The port was controlled against the python it replaces: the
# worklists match exactly on all three platforms (41/29/9) and version picking matches on six known bands.
mapfile -t WORK < <(node "$HERE/catalog-tweak.mjs" worklist "$CATALOG" "$OS_KEY" "$ONLY")
[ "$LIMIT" -gt 0 ] && WORK=("${WORK[@]:0:$LIMIT}")
echo "cold-network-sweep: $OS_KEY, ${#WORK[@]} overlays to judge"

# A version the band actually admits. `default` means every version, so the registry's latest is fine;
# a `<X` band needs a version BELOW X or the entry under test is not the one that resolves.
pick_version () {
  node "$HERE/catalog-tweak.mjs" pickversion "$1" "$2"
}

install_once () { # $1=pkg $2=version $3=restore-network(0|1) -> prints rc
  local pkg="$1" ver="$2" restore="$3" XD H FX rc
  XD="$(mktemp -d "$HOME/cns-x-XXXXXX")"; H="$(mktemp -d "$HOME/cns-h-XXXXXX")"; FX="$(mktemp -d "$HOME/cns-f-XXXXXX")"
  mkdir -p "$XD/nub/catalog"
  node "$HERE/catalog-tweak.mjs" tweak "$CATALOG" "$XD/nub/catalog/build-jail-catalog-v2.json" \
    "$pkg" "$OS_KEY" "$restore"
  local spec="$pkg"; [ "$ver" != "latest" ] && spec="$pkg@$ver"
  printf '{"name":"cns","version":"1.0.0","dependencies":{"%s":"%s"},"allowBuilds":{"%s":true}}' \
    "$pkg" "$([ "$ver" = latest ] && echo '*' || echo "$ver")" "$pkg" > "$FX/package.json"
  printf 'side-effects-cache=false\n' > "$FX/.npmrc"
  ( cd "$FX" && env -u ELECTRON_CACHE -u ELECTRON_MIRROR -u PLAYWRIGHT_BROWSERS_PATH -u PUPPETEER_CACHE_DIR \
      XDG_DATA_HOME="$XD" HOME="$H" NUB_CACHE_DIR="$H/nc" timeout 2400 "$NUB" install > "$FX/log" 2>&1 )
  rc=$?
  # The loaded-banner control, per row. Without it a row that silently fell back to the compiled catalog
  # reads exactly like a row where the override made no difference.
  local loaded=no; grep -q 'build-jail catalog updated from' "$FX/log" && loaded=yes
  # ⛔⛔ AN EXIT CODE CANNOT JUDGE A NARROWING, AND THIS SWEEP LEARNED IT THE HARD WAY. `keytar`'s macOS
  # overlay withdrew network; its postinstall runs `prebuild-install`, which tried github.com, was DENIED,
  # printed a warning, and FELL BACK TO COMPILING FROM SOURCE — exiting 0. The rc-only verdict below filed
  # that withdrawal as "OK-cold-as-shipped" when the grant was in fact wrong. A denied egress on macOS
  # surfaces as `getaddrinfo ENOTFOUND` with no jail-branded line anywhere, so nothing else flags it.
  #
  # Missing this is silent and permanent: the package still installs, just slower and from source, and
  # nothing says the grant was wrong. So a network error is recorded even on an rc=0 row.
  local net=no; grep -qE '\b(ENOTFOUND|EAI_AGAIN|ECONNREFUSED)\b|getaddrinfo' "$FX/log" && net=yes
  # ⛔⛔ A DENIED DOWNLOAD CAN EMIT NO ERROR STRING AT ALL. Measured on nub-linux: `re2`'s downloader
  # (`install-from-cache`) prints only `Trying <url> ...` per candidate and then falls back to `node-gyp`
  # SILENTLY — no ENOTFOUND, no EAI_AGAIN, nothing for the grep above. Its row read OK-cold-as-shipped
  # while the jail was in fact refusing three URLs, and the wrong grant survived a full re-sweep.
  #
  # So the denial is ALSO inferred structurally: the package tried to fetch something AND then compiled
  # from source. That pair is what a fallback looks like regardless of how quiet the downloader is.
  local tried compiled fellback=no
  tried=$(grep -cEi 'Trying https?://|node-pre-gyp http GET|prebuild-install http|install-from-cache' "$FX/log" 2>/dev/null || true)
  compiled=$(grep -cE '^\s*(CXX|CC|SOLINK|LIBTOOL)\(target\)|gyp info spawn args' "$FX/log" 2>/dev/null || true)
  [ "${tried:-0}" -gt 0 ] && [ "${compiled:-0}" -gt 0 ] && fellback=yes
  # A row where NOTHING was confined cannot judge a grant either way — it is void, not passing.
  local confined; confined=$(grep -c 'JAILDUMP' "$FX/log" 2>/dev/null || true)
  echo "$rc|$loaded|$net|$fellback|${confined:-0}"
  rm -rf "$XD" "$H" "$FX"
}

conf=0; other=0; fine=0; skipped=0
for row in "${WORK[@]}"; do
  pkg="${row%%$'\t'*}"; band="${row##*$'\t'}"
  ver="$(pick_version "$pkg" "$band")"
  # A LOOKUP FAILURE IS NOT AN EMPTY BAND, and conflating them hid the three most interesting win rows.
  if [ "$ver" = "LOOKUP-FAILED" ]; then
    printf '%s\t%s\t%s\n' "$pkg" "$band" "VOID-registry-lookup-failed" >> "$OUT"; skipped=$((skipped+1)); continue
  fi
  if [ -z "$ver" ]; then
    printf '%s\t%s\t%s\n' "$pkg" "$band" "SKIPPED-no-version-in-band" >> "$OUT"; skipped=$((skipped+1)); continue
  fi
  IFS='|' read -r rc_ship loaded_ship net_ship fell_ship confined_ship <<< "$(install_once "$pkg" "$ver" 0)"
  if [ "$loaded_ship" != yes ]; then
    printf '%s\t%s\t%s\n' "$pkg" "$band" "VOID-override-not-loaded" >> "$OUT"; skipped=$((skipped+1)); continue
  fi
  if [ "${confined_ship:-0}" = 0 ]; then
    # NOTHING ran under the jail, so this overlay was never exercised. Void, not fine.
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "VOID-jail-never-ran" "$ver" >> "$OUT"; skipped=$((skipped+1)); continue
  fi
  if [ "$rc_ship" = 0 ] && [ "$fell_ship" = yes ] && [ "$net_ship" != yes ]; then
    # The quiet case re2 exposed: tried to fetch, no error string, compiled from source anyway.
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "SUSPECT-silent-fallback-to-source-build" "$ver" >> "$OUT"
    conf=$((conf+1)); continue
  fi
  if [ "$rc_ship" = 0 ] && [ "$net_ship" = yes ]; then
    # Installed, but something it wanted was refused and it coped. The withdrawal is SUSPECT, not proven.
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "SUSPECT-rc0-but-network-denied" "$ver" >> "$OUT"
    conf=$((conf+1)); continue
  fi
  if [ "$rc_ship" = 0 ]; then
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "OK-cold-as-shipped" "$ver" >> "$OUT"; fine=$((fine+1)); continue
  fi
  IFS='|' read -r rc_net loaded_net _ _ _ <<< "$(install_once "$pkg" "$ver" 1)"
  if [ "$rc_net" = 0 ]; then
    printf '%s\t%s\t%s\tv=%s\tnet-error=%s\n' "$pkg" "$band" "CONFIRMED-needs-network" "$ver" "$net_ship" >> "$OUT"
    conf=$((conf+1))
  elif [ "$net_ship" = yes ]; then
    # ⛔⛔ FAILING BOTH WAYS DOES NOT CLEAR THE GRANT. `duckdb` and `libxmljs2` both had their prebuilt
    # download DENIED by the jail on macOS — npm reached the same host — and then compiled from source. Each
    # row failed both arms for an UNRELATED reason (a harness timeout, a V8 era mismatch), so the old
    # BROKEN-EITHER-WAY verdict swallowed the denial and two wrong grants survived a full re-sweep. The
    # network signal is independent of the outcome and must be reported on its own.
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "BROKEN-EITHER-WAY-AND-NETWORK-DENIED" "$ver" >> "$OUT"
    conf=$((conf+1))
  else
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "BROKEN-EITHER-WAY" "$ver" >> "$OUT"; other=$((other+1))
  fi
done
echo "confirmed-needs-network=$conf  ok-cold=$fine  broken-either-way=$other  skipped=$skipped"
echo "rows -> $OUT"
