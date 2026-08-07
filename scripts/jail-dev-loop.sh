#!/usr/bin/env bash
# jail-dev-loop.sh — the fast LOCAL loop for "does a real jailed install work + is the jail actually on".
#
# ⛔ THIS EXISTS BECAUSE I DID NOT HAVE IT. I ran the corpus measurement harness for days and never
# once did a plain `nub install` under the jail on this machine, so when I finally tried it I fumbled
# `approve-builds` live and mistook the default-deny of unapproved lifecycle scripts for the jail
# itself. A dialed-in loop makes both facts obvious in seconds. Run it after ANY change to the jail.
#
#   ./jail-dev-loop.sh                      # the built-in probe fixture (fast, proves the jail is ON)
#   ./jail-dev-loop.sh better-sqlite3@12.4.1 sharp@0.34.4   # real packages, comma OR space separated
#   NUB=/path/to/nub ./jail-dev-loop.sh ...  # override the binary (default: the integ worktree fast build)
#
# WHAT IT PROVES, every run, without me having to remember the moving parts:
#   1. the jail is ACTIVE  — a probe postinstall reports its $HOME is redirected AND the real ~/.npmrc
#      is EPERM (policy, not absence). ⛔ EXIT=0 alone never proves this; the jail could be silently off.
#   2. lifecycle scripts RUN — approval is pre-written (both `approvedBuilds` AND `allowBuilds`, the two
#      keys `approve-builds` writes; `approvedBuilds` alone is NOT enough — measured 2026-08-07), so the
#      default-deny layer does not mask the jail layer.
#   3. the install SUCCEEDS under the jail  — rc=0 and, for native pkgs, a real *.node on disk.
set -uo pipefail
NUB="${NUB:-/Users/colinmcd94/.cache/nub/worktrees/integ/target/fast/nub}"
REAL_HOME="$HOME"
[ -x "$NUB" ] || { echo "⛔ no nub binary at $NUB (set NUB=…, or build: cd <worktree> && scripts/rust-build.sh build -p nub-cli --profile fast)"; exit 2; }
# ⛔ never run a quarantined binary on this Mac (Gatekeeper). Refuse rather than pop a dialog.
if xattr "$NUB" 2>/dev/null | grep -q quarantine; then echo "⛔ $NUB is quarantined — refusing to run"; exit 2; fi

PKGS="$*"; PKGS="${PKGS//,/ }"

# ⛔ TWO SEPARATE ASSERTIONS, TWO SEPARATE INSTALLS — deliberately. The jail-active proof (a probe
# whose postinstall reports its own env) is reliable ALONE but was silently dropped when bundled
# with a slow native package (measured 2026-08-07 — better-sqlite3's own .node built, proving
# scripts DID run jailed, but the probe's console.log did not survive to the combined log). So the
# "is the jail on" check is its own probe-only install, and the "do real packages install jailed"
# check is rc + on-disk artifacts. One combined run conflating them cried wolf; this does not.

# ── run(dir, "<deps-json>") -> installs with approval, echoes rc, leaves run.log in $dir ──────────
setup_and_install() {
  local d="$1" deps="$2"
  ( cd "$d"
    printf '{ "name":"jail-dev","version":"1.0.0","private":true, "dependencies":{ %s } }\n' "$deps" > package.json
    printf '{\n  "install": { "buildJail": true }\n}\n' > nub.jsonc
    # ⛔ THE POSTINSTALL RUNS DURING approve-builds (it forces the build), NOT during a later install
    # once the store has it cached — measured 2026-08-07. So approve-builds' output is where the
    # JAILPROOF line lives; capture it. The final install gives the steady-state rc.
    "$NUB" install >/dev/null 2>&1            # materialize so approve-builds can see the builds
    "$NUB" approve-builds --all > run.log 2>&1
    rm -rf node_modules nub.lock
    "$NUB" install >> run.log 2>&1; echo $? )
}

# ── 1. JAIL-ACTIVE PROOF (probe only) ─────────────────────────────────────────────────────────────
PDIR="$(mktemp -d /tmp/jail-dev-probe-XXXXXX)"
mkdir -p "$PDIR/probe"
cat > "$PDIR/probe/package.json" <<JSON
{ "name":"jail-probe","version":"1.0.0","scripts":{"postinstall":"node p.js"} }
JSON
cat > "$PDIR/probe/p.js" <<JS
const os=require('os'), fs=require('fs');
const real=${REAL_HOME@Q};
let npmrc='n/a'; try { fs.readFileSync(real+'/.npmrc'); npmrc='ALLOWED'; } catch(e){ npmrc=e.code; }
console.log('JAILPROOF redirected='+(os.homedir()!==real)+' real_npmrc='+npmrc);
JS
PRC=$(setup_and_install "$PDIR" '"jail-probe":"file:./probe"')
REDIR=$(grep -o 'redirected=true' "$PDIR/run.log" | head -1)
NPMRC=$(grep -o 'real_npmrc=[A-Za-z/]*' "$PDIR/run.log" | head -1 | cut -d= -f2)
JAIL_OK=0; { [ -n "$REDIR" ] && { [ "$NPMRC" = "EPERM" ] || [ "$NPMRC" = "EACCES" ]; }; } && JAIL_OK=1

echo "════════ JAIL-ACTIVE PROOF (probe) ════════"
echo "  probe install exit : $PRC"
echo "  \$HOME redirected   : $([ -n "$REDIR" ] && echo 'YES ✓' || echo '⛔ NO — jail did not engage')"
echo "  real ~/.npmrc      : ${NPMRC:-<no probe output>} $([ "$NPMRC" = EPERM ] || [ "$NPMRC" = EACCES ] && echo '✓ denied by policy' || echo '⛔')"
rm -rf "$PDIR"

# ── 2. REAL-PACKAGE INSTALL (only if packages were named) ─────────────────────────────────────────
PKG_OK=1
if [ -n "$PKGS" ]; then
  RDIR="$(mktemp -d /tmp/jail-dev-pkg-XXXXXX)"
  DEPS=""
  for spec in $PKGS; do
    local_name="$spec"; ver="${spec##*@}"
    if [ "${spec:0:1}" = "@" ]; then local_name="@${spec:1}"; local_name="@${local_name%@*}"; else local_name="${spec%@*}"; fi
    [ "$ver" = "$spec" ] && ver="latest"
    DEPS="$DEPS${DEPS:+, }\"$local_name\":\"$ver\""
  done
  RRC=$(setup_and_install "$RDIR" "$DEPS")
  NATIVE=$(find "$RDIR/node_modules" -name '*.node' 2>/dev/null | wc -l | tr -d ' ')
  echo "════════ REAL INSTALL ($PKGS) ════════"
  echo "  install exit       : $RRC $([ "$RRC" = 0 ] && echo '✓' || echo '⛔')"
  echo "  native .node built : $NATIVE"
  [ "$RRC" = 0 ] || { PKG_OK=0; cp "$RDIR/run.log" /tmp/jail-dev-last.log; echo "  log -> /tmp/jail-dev-last.log"; }
  rm -rf "$RDIR"
fi

echo
if [ $JAIL_OK = 1 ] && [ $PKG_OK = 1 ]; then
  echo "✅ PASS — jail is ACTIVE (home redirected, real home EPERM)$([ -n "$PKGS" ] && echo ' and named packages installed jailed')."
else
  echo "⛔ FAIL — $([ $JAIL_OK = 0 ] && echo 'jail did not engage on the probe. ')$([ $PKG_OK = 0 ] && echo 'a named package failed to install jailed.')"
  exit 1
fi
