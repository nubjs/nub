#!/bin/bash
# Local macOS falsification-pair driver — build-jail grant narrowing WITHOUT CI.
#
# Ports the ARM half of `harness/v2/measure-macos.sh` (its `verify()`) from the
# nubjs/build-jail-corpus repo, with NO dtrace and NO root. It reuses that harness's own
# dep-scaffold.mjs / artifact-gate.mjs / arm-falsifiability.mjs unmodified, so this cannot
# drift from CI's definition of "the arm succeeded".
#
# WHAT IT CAN AND CANNOT DO. It tests grants you STATE; it cannot SYNTHESIZE one from observed
# syscalls, and it cannot attribute a red arm to a capability — both are dtrace phases needing
# uid 0. OBSERVE here is only the untraced npm artifact reference, which is sound because the
# trace never feeds the artifact gate.
#
# Every arm gets a FRESH throwaway HOME, which gives it a fresh CAS store, side-effects memo,
# jail private home and tool-cache leaves. That makes all four of CI's replay paths structurally
# absent — but it also MOVES the `userHome` scope, because `sandbox_homes` (build_jail.rs) reads
# $HOME. A package resolving its home via getpwuid, or reading a tool under the real home, is
# then refused at every rung. That direction is safe (false RED => keep the wider grant) but it
# makes some specs unmeasurable here.
#
#   usage: arm.sh <pkg> <ver> <label> <grant-json> [<label> <grant-json> ...]
#   The label `jailoff` is special: it runs with buildJail:false. Run it FIRST — a package nub
#   cannot install unjailed makes every jailed arm red for a reason that is not the grant.
#
#   env: NUB           path to a nub built with --features nub-cli/build-jail-catalog-override
#                      (the nub-sandbox/... spelling is WRONG and compiles half of it out)
#        CORPUS_HARNESS  path to build-jail-corpus/harness/v2
#        WORK          scratch root (default /tmp/nub-localfals)
set -uo pipefail

PKG="${1:?usage: arm.sh <pkg> <ver> <label> <grant> ...}"; VER="${2:?ver}"; shift 2
H="${CORPUS_HARNESS:?set CORPUS_HARNESS to build-jail-corpus/harness/v2}"
NUB="${NUB:?set NUB to a nub built with --features nub-cli/build-jail-catalog-override}"
WORK="${WORK:-/tmp/nub-localfals}"
SLUG="$(printf '%s' "$PKG" | tr '/@' '__')"
ROOT="$WORK/run/$SLUG-$VER"; rm -rf "$ROOT"; mkdir -p "$ROOT"
OBS="$ROOT/Observe"; mkdir -p "$OBS"

echo "=== SPEC $PKG@$VER ==="
# Assert the override feature by EXERCISING it — Rust does not embed feature names, and the
# literal string appears only in the error a binary WITHOUT the feature prints, so a content
# search matches the broken binary and misses the working one.
#
# ⛔ REDIRECT, THEN GREP THE FILE — NEVER PIPE nub INTO `grep -q`. `grep -q` exits on its FIRST
# match and SIGPIPEs the producer; under `set -o pipefail` the pipeline then reports 141, so this
# probe ABORTS on a binary that printed the banner correctly. Measured while writing this file:
# that false negative is indistinguishable from a genuinely featureless binary.
_pc="$(mktemp "$WORK/probecat-XXXXXX")"; _po="$(mktemp "$WORK/probeout-XXXXXX")"
_pcache="$(mktemp -d "$WORK/probecache-XXXXXX")"
printf '{"packages":{"__override_probe__":{"default":{"network":true}}}}' > "$_pc"
NUB_CACHE_DIR="$_pcache" NUB_BUILD_JAIL_CATALOG="$_pc" "$NUB" --version > "$_po" 2>&1
grep -q 'catalog OVERRIDDEN from' "$_po" \
  || { echo "  ABORT: \$NUB lacks build-jail-catalog-override; every arm would measure the shipped catalog"
       sed 's/^/    | /' "$_po"; rm -rf "$_pc" "$_po" "$_pcache"; exit 1; }
rm -rf "$_pc" "$_po" "$_pcache"

# ── OBSERVE: the artifact reference. UNCONFINED npm, under a THROWAWAY HOME. ──────────────────
OBSHOME="$WORK/home-obs-$SLUG-$VER"; rm -rf "$OBSHOME"; mkdir -p "$OBSHOME"
printf '{"name":"o","version":"1.0.0","private":true}\n' > "$OBS/package.json"
# Prove the redirect BEFORE anything installs, never after.
ACTUAL_HOME="$(env HOME="$OBSHOME" node -e 'process.stdout.write(require("os").homedir())')"
[ "$ACTUAL_HOME" = "$OBSHOME" ] || { echo "  ABORT: HOME redirect did not take (got $ACTUAL_HOME)"; exit 1; }
echo "  HOME-REDIRECT-VERIFIED $ACTUAL_HOME"

( cd "$OBS" && env HOME="$OBSHOME" npm install --no-audit --no-fund --ignore-scripts "$PKG@$VER" ) \
  > "$OBS/fetch.log" 2>&1
FETCH_RC=$?
if [ "$FETCH_RC" -ne 0 ]; then
  echo "  => BROKEN-WITHOUT-JAIL-TOO (unjailed fetch failed rc=$FETCH_RC)"
  tail -12 "$OBS/fetch.log" | sed 's/^/    | /'; exit 0
fi
node "$H/arm-falsifiability.mjs" --snapshot "$OBS" --pkg "$PKG" --ver "$VER" --out "$ROOT/pre.json"
( cd "$OBS" && env HOME="$OBSHOME" npm rebuild "$PKG" ) > "$OBS/rebuild.log" 2>&1
echo "  OBSERVE rebuild rc=$?"
node "$H/arm-falsifiability.mjs" --obs "$OBS" --pre "$ROOT/pre.json" --pkg "$PKG" --ver "$VER" 2>&1 | sed 's/^/  /'

# ── ARMS ──────────────────────────────────────────────────────────────────────────────────────
arm () {
  local label="$1" grant="$2"
  local v="$ROOT/verify-$label"; mkdir -p "$v"
  local ah="$WORK/home-$SLUG-$VER-$label"; rm -rf "$ah"; mkdir -p "$ah"
  # A unique root name per arm: nub memoises a lifecycle outcome keyed on package identity.
  local name="v$(echo "$label" | tr -dc 'a-z0-9')$RANDOM"
  printf '{"name":"%s","version":"1.0.0","dependencies":{"%s":"%s"}}\n' "$name" "$PKG" "$VER" > "$v/package.json"
  if [ "$label" = "jailoff" ]; then printf '{"install":{"buildJail":false}}\n' > "$v/nub.jsonc"
  else printf '{"install":{"buildJail":true}}\n' > "$v/nub.jsonc"; fi
  # Opt out of the resolve-time supply-chain gates: they refuse during RESOLUTION, before any
  # lifecycle script exists to confine, so when they fire the jail question cannot be asked.
  printf 'side-effects-cache=false\ntrust-policy=off\nminimum-release-age=0\nblockExoticSubdeps=false\n' > "$v/.npmrc"
  node "$H/dep-scaffold.mjs" "$v" "$PKG" "$grant" "$OBS" || { echo "  ARM[$label] scaffold FAILED"; return 1; }
  local cache="$v/nubcache"
  env HOME="$ah" NUB_CACHE_DIR="$cache" NUB_BUILD_JAIL_CATALOG="$v/cat.json" \
    sh -c "cd '$v' && '$NUB' install --ignore-scripts" > "$v/resolve.log" 2>&1
  if [ $? -ne 0 ]; then
    echo "  ARM[$label] rc=- RESOLVE-FAILED (harness error, not a grant result)"
    grep -avE '(^|[[:space:]])DEBUG[[:space:]]' "$v/resolve.log" | tail -8 | sed 's/^/       /'; return 1
  fi
  env HOME="$ah" NUB_CACHE_DIR="$cache" NUB_BUILD_JAIL_CATALOG="$v/cat.json" \
    sh -c "cd '$v' && '$NUB' install > '$v/i.log' 2>&1; '$NUB' approve-builds --all > '$v/a.log' 2>&1"
  local rc=$?
  # A malformed override WARNS AND FALLS BACK to the compiled-in catalog SILENTLY. Without this
  # an arm can measure the SHIPPED policy while you believe it measured yours.
  local ovr rej gate grc
  ovr=$(cat "$v"/*.log 2>/dev/null | grep -c 'catalog OVERRIDDEN')
  rej=$(cat "$v"/*.log 2>/dev/null | grep -c 'REJECTED')
  gate=$(node "$H/artifact-gate.mjs" --obs "$OBS" --arm "$v" --pkg "$PKG" --ver "$VER" 2>&1); grc=$?
  local layout=hoisted
  { [ -d "$v/node_modules/.store" ] || [ -L "$v/node_modules/$PKG" ]; } && layout=isolated
  echo "  ARM[$label] rc=$rc gate=$grc $(printf '%s' "$gate" | head -1) OVERRIDDEN=$ovr REJECTED=$rej layout=$layout grant=$grant"
  printf '%s\n' "$gate" | tail -n +2 | sed 's/^/       /'
  [ "$rc" -eq 0 ] || grep -avE '(^|[[:space:]])DEBUG[[:space:]]' "$v/i.log" "$v/a.log" 2>/dev/null \
    | grep -iE 'error|denied|EACCES|EPERM|ENOTFOUND|EAI_|not permitted' | head -6 | sed 's/^/       | /'
}
while [ $# -gt 0 ]; do arm "$1" "$2"; shift 2; done
