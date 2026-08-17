#!/bin/bash
# End-to-end sweep for the virtual-store / trees garbage collector.
# Runs entirely inside a throwaway HOME, so a developer's real store cannot be touched.
set -u
NUB=$(cd "$(dirname "${1:-${NUB:-target/fast/nub}}")" && pwd)/$(basename "${1:-${NUB:-target/fast/nub}}")
SANDBOX=$(mktemp -d /tmp/gvsgc.XXXXXX)
export HOME="$SANDBOX/home" XDG_CACHE_HOME="$SANDBOX/home/.cache" XDG_DATA_HOME="$SANDBOX/home/.local/share"
mkdir -p "$HOME/.cache" "$HOME/.local/share"
unset CI
GVS="$HOME/.cache/nub/pm/store"
PASS=0; FAIL=0
ok(){ if [ "$2" = "$3" ]; then echo "PASS  $1"; PASS=$((PASS+1)); else echo "FAIL  $1: got [$2] want [$3]"; FAIL=$((FAIL+1)); fi; }
entries(){ ls "$GVS" 2>/dev/null | grep -v '^\.' | wc -l | tr -d ' '; }
# Registry records are hex hashes; the dot-named files in there are the
# collector's own bookkeeping and must not be counted as projects.
count_reg(){ ls -A "$GVS/.projects" 2>/dev/null | grep -v '^\.' | wc -l | tr -d ' '; }

echo "=== sandbox: $SANDBOX ==="

# ---------- CASE 1: prune with NO registered projects must delete nothing ----------
mkdir -p "$GVS/orphan@9.9.9-deadbeefdeadbeef/node_modules"
echo x > "$GVS/orphan@9.9.9-deadbeefdeadbeef/node_modules/marker"
cd "$SANDBOX" && "$NUB" store prune > "$SANDBOX/prune0.log" 2>&1
ok "empty registry deletes nothing" "$(entries)" "1"
# POSITIVE CONTROL. Without this, the case above passes when prune returns
# early for an unrelated reason (an absent CAS root did exactly that) and
# the guard is never reached. Assert the guard actually ran.
ok "  ...and the guard is what stopped it" \
   "$(grep -qc 'No projects are registered' "$SANDBOX/prune0.log" && echo reached || echo NOT-REACHED)" "reached"

# ---------- set up two real projects ----------
for p in projA projB; do
  mkdir -p "$SANDBOX/$p" && cd "$SANDBOX/$p"
  printf '{"name":"%s","version":"1.0.0","dependencies":{"debug":"4.3.4"}}' "$p" > package.json
  "$NUB" install > "$SANDBOX/install-$p.log" 2>&1 || { echo "FAIL  install $p"; FAIL=$((FAIL+1)); }
done
LIVE=$(entries)
echo "      live entries after two installs: $LIVE"
ok "both projects registered" "$(count_reg)" "2"

# Backdate every unreferenced record so the next prune treats it as expired.
# Removal is two-phase: the first sweep only records, so without this the
# suite could only ever observe the deferral half.
expire_all(){ [ -f "$GVS/.gc-state" ] && sed 's/^[0-9]*	/0	/' "$GVS/.gc-state" > "$GVS/.gc-state.tmp" && mv "$GVS/.gc-state.tmp" "$GVS/.gc-state"; }

# ---------- CASE 2: the first prune DEFERS, the second removes ----------
# An entry unreferenced only because its project has not reinstalled yet is
# indistinguishable from garbage, so one sighting is never enough to delete.
cd "$SANDBOX/projA" && "$NUB" store prune > "$SANDBOX/prune1.log" 2>&1
ok "first prune defers rather than deleting" "$(entries)" "$LIVE"
ok "  ...and reports the deferral" \
   "$(grep -q 'Holding' "$SANDBOX/prune1.log" && echo reported || echo SILENT)" "reported"
expire_all
cd "$SANDBOX/projA" && "$NUB" store prune > "$SANDBOX/prune1b.log" 2>&1
ok "second prune removes the expired orphan" "$(entries)" "$((LIVE-1))"
ok "planted orphan is gone" "$([ -e "$GVS/orphan@9.9.9-deadbeefdeadbeef" ] && echo present || echo gone)" "gone"

# ---------- CASE 3: the projects STILL RESOLVE after the prune ----------
# The check a code reviewer structurally cannot make.
for p in projA projB; do
  cd "$SANDBOX/$p"
  out=$(node -e 'const d=require("debug"); console.log(typeof d)' 2>&1)
  ok "$p still resolves debug after prune" "$out" "function"
  # Compare realpaths: on macOS $GVS is under /tmp while Node reports
  # /private/tmp, so a literal prefix test reports a false mismatch.
  real=$(node -e 'console.log(require("fs").realpathSync(require.resolve("debug")))' 2>&1)
  gvsreal=$(cd "$GVS" && pwd -P)
  ok "  ...through the store, not a stray copy" \
     "$(case "$real" in "$gvsreal"/*) echo store;; *) echo "$real";; esac)" "store"
done

# ---------- CASE 4: a project we cannot see is dropped, not fatal ----------
# A missing path means deleted OR an unmounted disk OR an untraversable
# parent, and those are indistinguishable — so its registration is kept and
# it stops counting as a store user. It must NOT stop the whole sweep: an
# unresolvable record says nothing about any other project's entries, and
# every install path that points at a directory it later deletes would
# otherwise be one total outage away. Whatever it alone reached simply looks
# unreferenced, which is what the grace period is for.
rm -rf "$SANDBOX/projB"
cd "$SANDBOX/projA" && "$NUB" store prune > "$SANDBOX/prune2.log" 2>&1
ok "an unreachable project is reported" \
   "$(grep -q 'not reachable right now' "$SANDBOX/prune2.log" && echo reported || echo SILENT)" "reported"
ok "  ...but does NOT stop the sweep" \
   "$(grep -q 'not counted as a store user' "$SANDBOX/prune2.log" && echo continued || echo STOPPED)" "continued"
ok "  ...and its registration is kept, not destroyed" "$(count_reg)" "2"
ok "projA survives its sibling's disappearance" "$(node -e 'require("debug");console.log("ok")' 2>&1)" "ok"
# Once the record itself ages out it is dropped entirely.
[ -f "$GVS/.projects/.gc-state" ] && sed 's/^[0-9]*	/0	/' "$GVS/.projects/.gc-state" > "$GVS/.projects/.gc-state.tmp" && mv "$GVS/.projects/.gc-state.tmp" "$GVS/.projects/.gc-state"
cd "$SANDBOX/projA" && "$NUB" store prune > "$SANDBOX/prune2b.log" 2>&1
ok "an expired registration is dropped" "$(count_reg)" "1"
ok "projA still fine afterwards" "$(node -e 'require("debug");console.log("ok")' 2>&1)" "ok"

# ---------- CASE 5: a second prune is a no-op (idempotent) ----------
BEFORE=$(entries)
"$NUB" store prune > "$SANDBOX/prune3.log" 2>&1
ok "re-prune is idempotent" "$(entries)" "$BEFORE"

# ---------- CASE 6: the registry dir itself is never swept ----------
ok ".projects survives the sweep" "$([ -d "$GVS/.projects" ] && echo present || echo gone)" "present"

# ---------- CASE 7: a non-GVS project registers too ----------
# It owns extracted-tree entries keyed by its own un-hashed dep-path names,
# and only its own registration can protect them.
REG_BEFORE=$(count_reg)
mkdir -p "$SANDBOX/flat" && cd "$SANDBOX/flat"
printf '{"name":"flat","version":"1.0.0","dependencies":{"ms":"2.1.3"}}' > package.json
printf 'node-linker=hoisted\n' > .npmrc
"$NUB" install > "$SANDBOX/install-flat.log" 2>&1
ok "hoisted project registers as a store user" \
   "$(count_reg)" "$((REG_BEFORE+1))"
ok "hoisted project resolves" "$(node -e 'require("ms");console.log("ok")' 2>&1)" "ok"

# ---------- CASE 8: a WARM (no-op) install still registers ----------
# This is the pre-registry-upgrade path and the one the docs' repair advice
# depends on. `try_install_fast_path` returns before the link phase, so a
# project whose tree is already current never reaches the link-phase
# registration. Deleting the record and reinstalling simulates that project.
cd "$SANDBOX/projA"
rm -f "$GVS"/.projects/*
ok "registry emptied for the warm-path check" "$(count_reg)" "0"
RUST_LOG=debug "$NUB" install > "$SANDBOX/warm.log" 2>&1
# PIN the precondition. Without this the case passes when the install falls
# through to the slow path, which registers via `run_link_phase` and so proves
# nothing about the warm path. "Already up to date" is NOT the tell — the slow
# path prints it too (summary.rs:4). The absence of `phase:link ` is, since
# that line only runs inside `run_link_phase`. Trailing space: `phase:link_bins`
# would otherwise match.
ok "  ...and the install took the WARM path" \
   "$(grep -q 'phase:link ' "$SANDBOX/warm.log" && echo slow-path || echo warm)" "warm"
ok "a warm install re-registers the project" "$(count_reg)" "1"
# POSITIVE CONTROL for the absence assertion above. If the label moves, the
# RUST_LOG plumbing changes, or the debug channel goes quiet, the grep stops
# matching and the case reports `warm` unconditionally — passing green on
# exactly the fall-through it exists to catch. Prove the string CAN appear by
# forcing a slow path. Runs AFTER the count assertion: this install registers a
# project of its own, which would otherwise inflate it.
mkdir -p "$SANDBOX/ctl" && (cd "$SANDBOX/ctl" \
  && printf '{"name":"ctl","version":"1.0.0","dependencies":{"ms":"2.1.3"}}' > package.json \
  && RUST_LOG=debug "$NUB" install > "$SANDBOX/ctl.log" 2>&1)
ok "  ...and the WARM tell can actually appear" \
   "$(grep -q 'phase:link ' "$SANDBOX/ctl.log" && echo emitted || echo NEVER-EMITTED)" "emitted"
# ...and the entries it depends on now survive a prune.
"$NUB" store prune > "$SANDBOX/prune4.log" 2>&1
ok "projA still resolves after re-register + prune" "$(node -e 'require("debug");console.log("ok")' 2>&1)" "ok"

# ---------- CASE 9: the UPGRADE path ----------
# A store built before the registry existed, with several projects that have
# never reinstalled. This is every existing user on the day they upgrade, and
# the sequence that deleted 9 of 11 entries before the grace period existed.
# DISTINCT deps per project: with a shared dependency the one reinstalled
# project marks the others' entries too and the hazard cannot show.
U="$SANDBOX/upg"
for p in one two three; do
  mkdir -p "$U/$p" && cd "$U/$p"
  case $p in one) d=chalk v=4.1.2;; two) d=semver v=7.5.4;; three) d=uuid v=9.0.1;; esac
  printf '{"name":"u-%s","version":"1.0.0","dependencies":{"%s":"%s"}}' "$p" "$d" "$v" > package.json
  "$NUB" install > "$SANDBOX/u-$p.log" 2>&1
done
rm -rf "$GVS/.projects" "$GVS/.gc-state"     # the pre-registry state
UPG_BEFORE=$(entries)
# Only ONE project reinstalls, then the user prunes — twice, with the window
# expired in between, which is the worst case short of waiting 30 days.
(cd "$U/one" && "$NUB" install > "$SANDBOX/u-reinstall.log" 2>&1)
cd "$SANDBOX" && "$NUB" store prune > "$SANDBOX/u-prune1.log" 2>&1
ok "upgrade: first prune keeps every entry" "$(entries)" "$UPG_BEFORE"
ok "upgrade: untouched project still resolves" \
   "$(cd "$U/two" && node -e 'require("semver");console.log("ok")' 2>&1 | tail -1)" "ok"
# Now the rescue: the other projects check in before the window expires.
for p in two three; do (cd "$U/$p" && "$NUB" install > "$SANDBOX/u-fix-$p.log" 2>&1); done
expire_all
cd "$SANDBOX" && "$NUB" store prune > "$SANDBOX/u-prune2.log" 2>&1
ok "upgrade: reinstalling inside the window rescues them" \
   "$(cd "$U/two" && node -e 'require("semver");console.log("ok")' 2>&1 | tail -1)" "ok"
ok "upgrade: and the third one too" \
   "$(cd "$U/three" && node -e 'require("uuid");console.log("ok")' 2>&1 | tail -1)" "ok"

# ---------- CASE 10: nubx must NOT register its scratch project ----------
# `dlx` installs into a temp dir it deletes on exit, so a record from one names
# a path nothing can resolve. The sweep drops such a record and carries on, so
# this is hygiene — but every `nubx` would otherwise leave one behind.
REG_PRE=$(count_reg)
cd "$SANDBOX/projA" && "$NUB" x cowsay@1.6.0 moo > "$SANDBOX/dlx.log" 2>&1
# PIN the precondition. A dlx that failed to run registers nothing either, so
# without this the case passes vacuously — which it did, when a bad package
# spec made every invocation error out.
ok "  ...the dlx actually ran" \
   "$(grep -q '(oo)' "$SANDBOX/dlx.log" && echo ran || echo "DID-NOT-RUN: $(tail -1 "$SANDBOX/dlx.log")")" "ran"
ok "nubx leaves no registry record behind" "$(count_reg)" "$REG_PRE"
cd "$SANDBOX/projA" && "$NUB" store prune > "$SANDBOX/prune5.log" 2>&1
ok "  ...and the sweep is unaffected by a nubx" \
   "$(grep -q 'not reachable right now' "$SANDBOX/prune5.log" && echo BLOCKED || echo runs)" "runs"

echo
echo "=== $PASS passed, $FAIL failed ==="
cd /; rm -rf "$SANDBOX"
[ "$FAIL" -eq 0 ]
