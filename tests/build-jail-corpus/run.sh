#!/usr/bin/env bash
# Build-jail no-regression corpus (epic 5.2a). Proves property 6 — the shared
# zero-privilege engine runs REAL dependency lifecycle scripts default-on without
# breaking them.
#
#   NUB_BIN=/path/to/nub tests/build-jail-corpus/run.sh
#
# Linux (Landlock) / macOS (Seatbelt). The lifecycle-script seam
# (crates/nub-cli/src/pm_engine/sandbox_closure.rs) confines DEPENDENCY scripts.
#
# COMPAT arm: the native-addon / postinstall-download packages users actually
# install run their build scripts DEFAULT-ON (nub's curated build-approval floor —
# no --allow-build, no approve-builds) under the engine, and load. That is the
# "default-on without breaking the vast majority of users" bar.
#
# CONTROL: the engine actually confines a lifecycle script. Same out-of-jail write,
# two ways — run BARE it succeeds (the discriminator is valid); run as a sandboxed
# DEPENDENCY postinstall (a non-trusted file: dep, opted in via `approve-builds
# --all` + `rebuild`, which runs through the same jailed seam) it is BLOCKED.
set -u

NUB="${NUB_BIN:-nub-dev}"
command -v "$NUB" >/dev/null 2>&1 || NUB="$(command -v "$NUB" || echo "$NUB")"
REALHOME="$HOME"
PASS=0; FAIL=0
note() { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); note "PASS: $*"; }
bad()  { FAIL=$((FAIL+1)); note "FAIL: $*"; }

# Exercise the build jail ON. It defaults OFF today (jailBuilds=false, planned
# default-on in the next major); property 6 is precisely that this flip is safe, so
# the corpus must enable it or it measures the unconfined path. Neutral, nub-honored
# (AUBE_JAIL_BUILDS is not read under nub).
export npm_config_jail_builds=true
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nub-corpus-XXXXXX")" || exit 2
cd "$WORK" || exit 2
note ">>> corpus workroot: $WORK   NUB=$NUB   (npm_config_jail_builds=true)"

# ── compat arm: default-on lifecycle-script packages install + load ────────────
CORPUS=(
  "better-sqlite3|better-sqlite3@11.8.1|require('better-sqlite3')"
  "bcrypt|bcrypt@5.1.1|require('bcrypt')"
  "esbuild|esbuild@0.24.0|require('esbuild').transformSync('const x=1',{})"
)
for row in "${CORPUS[@]}"; do
  IFS='|' read -r nm spec probe <<<"$row"
  d="$WORK/pkg-$nm"; mkdir -p "$d"; cd "$d"
  printf '{"name":"corpus-%s","private":true}\n' "$nm" > package.json
  "$NUB" add "$spec" > install.log 2>&1
  rc=$?
  if [ $rc -ne 0 ]; then bad "$nm install (exit $rc); tail:"; tail -6 install.log; cd "$WORK"; continue; fi
  if grep -qi "WARN_NUB_IGNORED_BUILD_SCRIPTS" install.log; then
    bad "$nm build script was NOT default-on (ignored) — not the default-on property"; cd "$WORK"; continue
  fi
  if node -e "$probe" > probe.log 2>&1; then
    ok "$nm builds default-on + loads (lifecycle script ran under the engine)"
  else
    bad "$nm installed (exit 0) but load failed:"; tail -6 probe.log
  fi
  cd "$WORK"
done

# ── control: the engine actually confines a DEPENDENCY lifecycle script ───────
EVIL="$REALHOME/nub-corpus-EVIL-$$"
rm -f "$EVIL"
# (a) failing control — the identical write run BARE (no sandbox) → must succeed.
if node -e "require('fs').writeFileSync('$EVIL','x')" >/dev/null 2>&1 && [ -f "$EVIL" ]; then
  ok "control-bare: out-of-jail write succeeds unsandboxed (discriminator valid)"
  rm -f "$EVIL"
else
  bad "control-bare: out-of-jail write failed even unsandboxed — control is not discriminating"
fi
# (b) attack — the same write as a sandboxed dep postinstall → must be blocked.
dep="$WORK/evildep"; mkdir -p "$dep"
cat > "$dep/pi.js" <<JS
const fs = require('fs');
try { fs.writeFileSync('$EVIL', 'x'); console.log('EVIL_WRITE_OK'); }
catch (e) { console.log('EVIL_WRITE_BLOCKED:' + (e.code || e.message)); }
fs.writeFileSync('./in-pkg-ok', 'y');
JS
cat > "$dep/package.json" <<'JSON'
{ "name": "evildep", "version": "1.0.0", "scripts": { "postinstall": "node ./pi.js" } }
JSON
proj="$WORK/evilproj"; mkdir -p "$proj"; cd "$proj"
printf '{"name":"evilproj","private":true}\n' > package.json
"$NUB" add "file:../evildep" > add.log 2>&1
"$NUB" approve-builds --all > approve.log 2>&1
"$NUB" rebuild > rebuild.log 2>&1
depmark="$proj/node_modules/evildep/in-pkg-ok"
if [ -f "$EVIL" ]; then
  bad "control-sandboxed: LEAK — out-of-jail write SUCCEEDED under confinement"; rm -f "$EVIL"
elif grep -q "EVIL_WRITE_BLOCKED" rebuild.log add.log 2>/dev/null; then
  ok "control-sandboxed: out-of-jail write BLOCKED during dep postinstall ($(grep -h -o 'EVIL_WRITE_BLOCKED:[^"]*' rebuild.log add.log 2>/dev/null | head -1))"
elif [ -f "$depmark" ]; then
  ok "control-sandboxed: out-of-jail write BLOCKED (in-pkg marker present, postinstall ran, EVIL absent)"
else
  bad "control-sandboxed: cannot confirm the dep postinstall ran (in-pkg marker absent) — inconclusive"
  note "--- add.log ---"; tail -4 add.log; note "--- approve.log ---"; tail -4 approve.log; note "--- rebuild.log ---"; tail -6 rebuild.log
fi
cd "$WORK"

note ""
note "RESULT: PASS=$PASS FAIL=$FAIL"
rm -rf "$WORK"
[ "$FAIL" -eq 0 ]
