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
CATALOG="$ROOT/crates/nub-sandbox/data/build-jail-catalog-v2.json"
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
mapfile -t WORK < <(python3 - "$CATALOG" "$OS_KEY" "$ONLY" <<'PY'
import json,sys
c=json.load(open(sys.argv[1])); osk=sys.argv[2]; only=sys.argv[3]
rows=[]
for name,ent in c["packages"].items():
    bands={}
    if isinstance(ent.get("default"),dict): bands["default"]=ent["default"]
    for k,v in (ent.get("versions") or {}).items(): bands[k]=v
    for band,body in bands.items():
        if not isinstance(body,dict): continue
        ov=body.get(osk)
        if isinstance(ov,dict) and "network" in ov and ov["network"] is None and body.get("network") is True:
            if only and name!=only: continue
            rows.append(f"{name}\t{band}")
for r in sorted(set(rows)): print(r)
PY
)
[ "$LIMIT" -gt 0 ] && WORK=("${WORK[@]:0:$LIMIT}")
echo "cold-network-sweep: $OS_KEY, ${#WORK[@]} overlays to judge"

# A version the band actually admits. `default` means every version, so the registry's latest is fine;
# a `<X` band needs a version BELOW X or the entry under test is not the one that resolves.
pick_version () {
  python3 - "$1" "$2" <<'PY'
import json,subprocess,sys
name,band=sys.argv[1],sys.argv[2]
if band=="default": print("latest"); sys.exit()
if band.startswith("<"):
    # Ask the registry for the newest version strictly below the bound.
    try:
        out=subprocess.run(["npm","view",name,"versions","--json"],capture_output=True,text=True,timeout=90).stdout
        vs=json.loads(out)
        bound=band[1:]
        def key(v): return [int(x) if x.isdigit() else 0 for x in v.replace('-','.').split('.')[:3]]
        cand=[v for v in vs if "-" not in v and key(v)<key(bound)]
        print(cand[-1] if cand else "")
    except Exception: print("")
else: print("")
PY
}

install_once () { # $1=pkg $2=version $3=restore-network(0|1) -> prints rc
  local pkg="$1" ver="$2" restore="$3" XD H FX rc
  XD="$(mktemp -d "$HOME/cns-x-XXXXXX")"; H="$(mktemp -d "$HOME/cns-h-XXXXXX")"; FX="$(mktemp -d "$HOME/cns-f-XXXXXX")"
  mkdir -p "$XD/nub/catalog"
  python3 - "$CATALOG" "$XD/nub/catalog/build-jail-catalog-v2.json" "$pkg" "$OS_KEY" "$restore" <<'PY'
import json,sys
src,dst,pkg,osk,restore=sys.argv[1],sys.argv[2],sys.argv[3],sys.argv[4],sys.argv[5]
c=json.load(open(src))
if restore=="1":
    ent=c["packages"][pkg]
    for body in ([ent["default"]] if isinstance(ent.get("default"),dict) else []) + list((ent.get("versions") or {}).values()):
        if isinstance(body,dict) and isinstance(body.get(osk),dict) and body[osk].get("network","x") is None:
            del body[osk]["network"]
            if not body[osk]: del body[osk]
# The stamp is what makes the override win the newer-than comparison. It lives under `provenance`, NOT at
# the top level — an earlier version of this put it top-level and every row silently measured the COMPILED
# catalog while reporting as though the override were in force.
c.setdefault("provenance",{})["generatedAt"]="2099-01-01T00:00:00Z"
json.dump(c,open(dst,"w"))
PY
  local spec="$pkg"; [ "$ver" != "latest" ] && spec="$pkg@$ver"
  printf '{"name":"cns","version":"1.0.0","dependencies":{"%s":"%s"},"allowBuilds":{"%s":true}}' \
    "$pkg" "$([ "$ver" = latest ] && echo '*' || echo "$ver")" "$pkg" > "$FX/package.json"
  printf 'side-effects-cache=false\n' > "$FX/.npmrc"
  ( cd "$FX" && env -u ELECTRON_CACHE -u ELECTRON_MIRROR -u PLAYWRIGHT_BROWSERS_PATH -u PUPPETEER_CACHE_DIR \
      XDG_DATA_HOME="$XD" HOME="$H" NUB_CACHE_DIR="$H/nc" timeout 300 "$NUB" install > "$FX/log" 2>&1 )
  rc=$?
  # The loaded-banner control, per row. Without it a row that silently fell back to the compiled catalog
  # reads exactly like a row where the override made no difference.
  local loaded=no; grep -q 'build-jail catalog updated from' "$FX/log" && loaded=yes
  local net=no; grep -qiE 'ENOTFOUND|EAI_AGAIN|ECONNREFUSED|getaddrinfo|network' "$FX/log" && net=yes
  echo "$rc|$loaded|$net"
  rm -rf "$XD" "$H" "$FX"
}

conf=0; other=0; fine=0; skipped=0
for row in "${WORK[@]}"; do
  pkg="${row%%$'\t'*}"; band="${row##*$'\t'}"
  ver="$(pick_version "$pkg" "$band")"
  if [ -z "$ver" ]; then
    printf '%s\t%s\t%s\n' "$pkg" "$band" "SKIPPED-no-version-in-band" >> "$OUT"; skipped=$((skipped+1)); continue
  fi
  IFS='|' read -r rc_ship loaded_ship net_ship <<< "$(install_once "$pkg" "$ver" 0)"
  if [ "$loaded_ship" != yes ]; then
    printf '%s\t%s\t%s\n' "$pkg" "$band" "VOID-override-not-loaded" >> "$OUT"; skipped=$((skipped+1)); continue
  fi
  if [ "$rc_ship" = 0 ]; then
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "OK-cold-as-shipped" "$ver" >> "$OUT"; fine=$((fine+1)); continue
  fi
  IFS='|' read -r rc_net loaded_net _ <<< "$(install_once "$pkg" "$ver" 1)"
  if [ "$rc_net" = 0 ]; then
    printf '%s\t%s\t%s\tv=%s\tnet-error=%s\n' "$pkg" "$band" "CONFIRMED-needs-network" "$ver" "$net_ship" >> "$OUT"
    conf=$((conf+1))
  else
    printf '%s\t%s\t%s\tv=%s\n' "$pkg" "$band" "BROKEN-EITHER-WAY" "$ver" >> "$OUT"; other=$((other+1))
  fi
done
echo "confirmed-needs-network=$conf  ok-cold=$fine  broken-either-way=$other  skipped=$skipped"
echo "rows -> $OUT"
