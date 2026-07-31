#!/bin/bash
# run-probe.sh <arm A0|PROD> — enforcement proof on the SAME binary the sweep runs.
# Mirrors run-shard.sh's isolation exactly (env -i, private HOME/cache/tmp, absolute
# paths) so the result is about the jail and not about the harness.
set -u -o pipefail
ARM="${1:?A0|PROD}"
NUB="${NUB_BIN:?set NUB_BIN}"
STUDY_PATH="${STUDY_PATH:-/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin}"

# Outside every observed root, and NOT under /tmp: a multi-hour run's inputs and
# logs must survive a host reboot, which clears /tmp.
PROBE_ROOT="${PROBE_ROOT:-$HOME/build-jail-probe}"
# The machine's real home — the escape target that is NOT the run's redirected HOME.
REAL_HOME="${REAL_HOME:-$HOME}"
ROOT="$PROBE_ROOT/$ARM"
rm -rf "$ROOT"; mkdir -p "$ROOT"
PROJ="$ROOT/proj"; H="$ROOT/home"; CACHE="$ROOT/cache"; TMPD="$ROOT/tmp"
mkdir -p "$PROJ" "$H" "$CACHE" "$TMPD" "$PROJ/pkg"
NONCE="Probe$(date +%s)$RANDOM"

# A planted secret inside the run's HOME, and a marker target in the machine's
# REAL home — two different escapes, because the jail redirects HOME and a test
# that only probes the redirected path proves less than it looks.
SECRET="$H/.corpus-secret"
printf 'TOPSECRET-%s\n' "$NONCE" > "$SECRET"
rm -f "$REAL_HOME/realhome-$NONCE.txt" "$H/outerhome-$NONCE.txt" "$PROJ/project-$NONCE.txt"

# BAKE the absolute paths into the script. Exporting them does not survive the jail's
# env scrub, and the failure is silent: an undefined path makes path.join throw a
# TypeError that reads as a clean DENIED from a jail that was never consulted.
sed -e "s|@@NONCE@@|$NONCE|g" -e "s|@@OUTER_HOME@@|$H|g" -e "s|@@REAL_HOME@@|$REAL_HOME|g" \
    -e "s|@@PROJECT@@|$PROJ|g" -e "s|@@SECRET@@|$SECRET|g" \
    "$(dirname "${BASH_SOURCE[0]}")/probe.js.tmpl" > "$PROJ/pkg/probe.js"
grep -q "@@" "$PROJ/pkg/probe.js" && { echo "FATAL: unsubstituted placeholder in probe"; exit 6; }
cat > "$PROJ/pkg/package.json" <<EOF
{ "name": "jailprobe", "version": "1.0.0", "scripts": { "postinstall": "node probe.js" } }
EOF

META=""
[ "$ARM" = "A0" ] && META=', "dependenciesMeta": { "jailprobe": { "sandbox": false } }'
cat > "$PROJ/package.json" <<EOF
{ "name": "probe-host", "version": "1.0.0", "private": true,
  "dependencies": { "jailprobe": "file:./pkg" }$META }
EOF
{ echo "minimumReleaseAge=0"; echo "minimumReleaseAgeStrict=false"
  echo "side-effects-cache=false"; } > "$PROJ/.npmrc"

LOG="$ROOT/run.log"
{ echo "=== PROBE PROVENANCE ==="
  echo "arm=$ARM nonce=$NONCE"
  echo "nub_bin=$NUB"
  echo "nub_bin_sha256=$(sha256sum "$NUB" | awk '{print $1}')"
  echo "nub_version=$("$NUB" --version 2>&1 | head -1)"
  echo "host=$(uname -srm)"; echo "date=$(date -u +%FT%TZ)"; } > "$LOG"

runnub() {
  ( cd "$PROJ" && env -i \
      PATH="$STUDY_PATH" HOME="$H" TMPDIR="$TMPD" \
      NUB_CACHE_DIR="$CACHE" \
      "$NUB" "$@" ) >> "$LOG" 2>&1
}

NODE_DIR="$(cd "$(dirname "$(command -v node)")" && pwd)"
case ":$STUDY_PATH:" in *":$NODE_DIR:"*) ;; *) STUDY_PATH="$NODE_DIR:$STUDY_PATH" ;; esac

runnub install; RC_I=$?
runnub approve-builds jailprobe; RC_A=$?
echo "install rc=$RC_I approve rc=$RC_A" >> "$LOG"

WARN=$(grep -c "running without the build sandbox" "$LOG" || true)
if [ "$ARM" = "A0" ]; then
  [ "$WARN" -gt 0 ] && EFFECT=confirmed || EFFECT=FAILED-no-optout-warning
else
  [ "$WARN" -eq 0 ] && EFFECT=confirmed || EFFECT=FAILED-optout-leaked
fi
OVR=$(grep -c "build-jail catalog OVERRIDDEN from" "$LOG" || true)

echo "=== ARM=$ARM arm_effect=$EFFECT optout_warnings=$WARN overridden_banners=$OVR"
grep -h "PROBE_RESULT" "$LOG" || echo "PROBE_RESULT MISSING — the script did not run"
# The landing evidence, read from OUTSIDE the jail: what is actually on disk after.
echo "on_disk: realhome=$([ -f "$REAL_HOME/realhome-$NONCE.txt" ] && echo PRESENT || echo absent)" \
     "outerhome=$([ -f "$H/outerhome-$NONCE.txt" ] && echo PRESENT || echo absent)" \
     "project=$([ -f "$PROJ/project-$NONCE.txt" ] && echo PRESENT || echo absent)"
rm -f "$REAL_HOME/realhome-$NONCE.txt"
