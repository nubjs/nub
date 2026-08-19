#!/usr/bin/env bash
# The population the jail actually applies to: packages that HAVE an install script.
#
# ⛔⛔ WHY THIS EXISTS ALONGSIDE `ecosystem-sweep.sh`. That sweep installs real framework trees, which is
# the right shape for "does a user's project install" — but it turned out that only 6 of 17 such trees run
# ANY lifecycle script, so it produces 6 rows of evidence about confinement and 7 rows of nothing. The
# reason is a real and welcome fact: much of the ecosystem has moved from install scripts to platform
# `optionalDependencies`. `sharp`, `better-sqlite3`, `playwright` and even `electron@43` have no scripts at
# all now. Measuring the jail against trees that have nothing to confine cannot support a bar.
#
# So this sweep's population is the packages where confinement APPLIES, checked against the registry rather
# than assumed: 20 of 30 popular native packages still carry an install/postinstall.
#
# ⛔ APPROVAL AND GRANT ARE DIFFERENT AXES, AND CONFLATING THEM IS HOW THIS GOES VACUOUS. nub's default is
# to SKIP an unapproved build script — so measuring at "pure defaults" measures a skipped script, i.e. the
# jail never runs. This sweep therefore APPROVES the build (`allowBuilds`, which is what a user does via
# `nub approve-builds`) while leaving the GRANT entirely alone: no catalog override, no `no-jail`, no
# `dependenciesMeta`. Approval is the user's decision; the grant is what is under test.
#
# ⛔ COLD EVERY TIME. A warm download cache hides the failure that matters — `electron` was once measured as
# needing no network because the measuring machine already had its zip.
#
# Usage: NUB=/path/to/nub ./install-script-sweep.sh [--only <pkg>] [--keep-logs <dir>] [--population <tsv>]
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
NUB="${NUB:-$ROOT/target/fast/nub}"
ONLY=""; KEEP=""
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY="$2"; shift 2 ;;
    --keep-logs) KEEP="$2"; shift 2 ;;
    --population) POPULATION="$2"; shift 2 ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
[ -x "$NUB" ] || { echo "no nub binary at $NUB" >&2; exit 2; }
OUT="${OUT:-/tmp/install-script-sweep.tsv}"
: > "$OUT"
[ -n "$KEEP" ] && mkdir -p "$KEEP"

# ⛔⛔ THE POPULATION IS DISCOVERED, NOT HARDCODED, AND THAT IS LOAD-BEARING. The set of packages the jail
# actually confines is shrinking fast: `sharp`, `better-sqlite3`, `playwright` and `electron@43` have all
# dropped their install scripts in favour of platform `optionalDependencies` (measured: 37 of 123 popular
# candidates no longer carry one). A hardcoded list rots in the direction that makes this sweep LOOK fine
# while measuring nothing — a package that drops its script keeps "passing" as a row where the jail was
# never exercised. `discover-install-scripts.mjs` asks the registry instead, and its output is checked in
# under `results/` so a run is reproducible against the population it actually used.
#
# Offline or registry-down: pass `--population <file>` to reuse a checked-in list.
POPULATION="${POPULATION:-}"
if [ -z "$POPULATION" ]; then
  POPULATION="$(mktemp -t iss-pop)"
  node "$HERE/discover-install-scripts.mjs" --out "$POPULATION" || {
    echo "could not discover the population; pass --population <file>" >&2; exit 2; }
fi
PKGS="$(awk -F'\t' '{print $1}' "$POPULATION" | tr '\n' ' ')"

ran_total=0; ok=0; jail=0; other=0; noscript=0
for pkg in $PKGS; do
  [ -n "$ONLY" ] && [ "$ONLY" != "$pkg" ] && continue
  home="$(mktemp -d "$HOME/iss-XXXXXX")"
  proj="$home/project"
  mkdir -p "$proj"
  # APPROVED so the script runs; the GRANT is whatever nub ships.
  # The exact version the population file names, so a re-run measures the same thing rather than whatever
  # `latest` has become since.
  ver="$(awk -F'\t' -v p="$pkg" '$1==p{print $2}' "$POPULATION" | head -1)"
  [ -n "$ver" ] || ver="latest"
  # ⛔ NODE, NOT PYTHON, AND THAT IS WHAT MAKES THIS RUNNABLE ON WINDOWS. `nub-win3` has no python3
  # and no `py`, so the heredoc that used to be here wrote no fixture at all and every Windows row read
  # NO-SCRIPT-RAN — 87 rows summarising as "0 jail-suspect failures" over installs that never happened.
  # node is present wherever nub is, by construction. The sibling cold-network sweep was ported for this
  # exact reason; this file was missed.
  node -e '
    const [out, pkg, ver] = process.argv.slice(1);
    require("fs").writeFileSync(out, JSON.stringify({
      name: "iss", version: "1.0.0",
      dependencies: { [pkg]: ver },
      allowBuilds: { [pkg]: true },
    }));
  ' "$proj/package.json" "$pkg" "$ver"
  printf 'side-effects-cache=false\n' > "$proj/.npmrc"
  # ⛔⛔ A FRESH `HOME` IS NOT A FRESH STORE ON WINDOWS, AND THAT INVALIDATED A WHOLE SWEEP.
  # `nub_data_dir()` resolves `$XDG_DATA_HOME/nub`, THEN `%LOCALAPPDATA%\nub` on Windows, then
  # `<home>/.local/share/nub`. This loop set only `HOME`, which the Windows branch never consults — so
  # every row shared the machine's real `%LOCALAPPDATA%\nub\pm\store`. Measured: 13 of 87 rows failed
  # with `Cannot find module 'C:\Users\nub\AppData\Local\nub\pm\store\simple-get@…\once\once.js'`,
  # i.e. one row's store state breaking another's resolution, reported as package failures.
  # `XDG_DATA_HOME` alone would fix it, but naming all three keeps the row isolated whichever branch a
  # platform takes.
  log="$home/install.log"
  ( cd "$proj" && env -u ELECTRON_CACHE -u ELECTRON_MIRROR -u PLAYWRIGHT_BROWSERS_PATH \
      -u PUPPETEER_CACHE_DIR -u npm_config_cache -u CYPRESS_INSTALL_BINARY \
      NUB_JAIL_DUMP_POLICY=1 HOME="$home" NUB_CACHE_DIR="$home/.cache/nub" \
      XDG_DATA_HOME="$home/xdg" USERPROFILE="$home" LOCALAPPDATA="$home/AppData/Local" \
      timeout 1200 "$NUB" install > "$log" 2>&1 )
  rc=$?

  # ⛔⛔ COUNT THE JAIL'S OWN PER-SPAWN DUMP, NOT A WARNING. The first version counted
  # `running build scripts`, which is emitted only on the defaultTrust path — so with an explicit
  # `allowBuilds` approval (which is what this sweep uses) it never appears, and every row reported
  # NO-SCRIPT-RAN while the script had in fact run and been confined. Measured: esbuild with allowBuilds
  # produced 0 of that line and 36 JAILDUMP lines naming `esbuild@0.28.2`.
  #
  # `NUB_JAIL_DUMP_POLICY` prints one line per CONFINED spawn, on every platform and every approval path,
  # so it measures the thing the verdict depends on — did the jail actually run — rather than a message
  # that happens to accompany one route to it.
  ran=$(grep -c 'JAILDUMP' "$log" 2>/dev/null || true)
  jail_lines=$(grep -ciE 'nub build sandbox: blocked|could not confine|sandbox could not be applied|WARN_NUB_JAIL_NET_DENIED' "$log" 2>/dev/null || true)
  # A denied egress on macOS surfaces as a DNS failure with no jail-branded line — measured. So a network
  # error is its own gating bucket rather than being filed as unrelated.
  net_err=$(grep -cE '\b(ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ETIMEDOUT)\b' "$log" 2>/dev/null || true)

  if [ "$ran" = 0 ]; then
    verdict=NO-SCRIPT-RAN; noscript=$((noscript+1))
  elif [ "$rc" = 0 ] && [ "$jail_lines" = 0 ] && [ "$net_err" = 0 ]; then
    verdict=OK; ok=$((ok+1)); ran_total=$((ran_total+1))
  elif [ "$jail_lines" != 0 ] || [ "$net_err" != 0 ]; then
    verdict=JAIL-SUSPECT; jail=$((jail+1)); ran_total=$((ran_total+1))
  else
    verdict=FAILED-OTHER; other=$((other+1)); ran_total=$((ran_total+1))
  fi
  err=$(grep -viE '^\s*(WARN|npm warn|warning|gyp WARN)' "$log" 2>/dev/null \
    | grep -oiE 'nub build sandbox: blocked[^.]*|could not confine[^.]*|ENOTFOUND [a-z0-9.-]*|node-pre-gyp ERR![^,]*|fatal error: [^ ]*|Cannot find module [^ ]*|× lifecycle script [a-z]* failed for [^ ]*' \
    | head -1)
  printf '%s\t%s\trc=%s\tscripts-ran=%s\tjail-lines=%s\tnet-err=%s\t%s\n' \
    "$pkg" "$verdict" "$rc" "$ran" "$jail_lines" "$net_err" "${err:-—}" >> "$OUT"
  echo "  $pkg -> $verdict (rc=$rc, ran=$ran)"
  [ -n "$KEEP" ] && cp "$log" "$KEEP/$(echo "$pkg" | tr '/' '_').log" 2>/dev/null
  rm -rf "$home"
done

echo
echo "── install-script packages, jail DEFAULT-ON, approved builds, cold ──"
awk -F'\t' '{print $2}' "$OUT" | sort | uniq -c | sort -rn
total=$(wc -l < "$OUT" | tr -d ' ')
echo "packages whose script actually ran: $ran_total / $total"
echo "jail-suspect failures: $jail / $ran_total"
[ "$jail" = 0 ] || exit 1
