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
ok "both projects registered" "$(ls "$GVS/.projects" | wc -l | tr -d ' ')" "2"

# ---------- CASE 2: prune keeps live entries, drops the planted orphan ----------
cd "$SANDBOX/projA" && "$NUB" store prune > "$SANDBOX/prune1.log" 2>&1
ok "orphan removed, live entries kept" "$(entries)" "$((LIVE-1))"
ok "planted orphan is gone" "$([ -e "$GVS/orphan@9.9.9-deadbeefdeadbeef" ] && echo present || echo gone)" "gone"

# ---------- CASE 3: the projects STILL RESOLVE after the prune ----------
# The check a code reviewer structurally cannot make.
for p in projA projB; do
  cd "$SANDBOX/$p"
  out=$(node -e 'const d=require("debug"); console.log(typeof d)' 2>&1)
  ok "$p still resolves debug after prune" "$out" "function"
  real=$(node -e 'console.log(require.resolve("debug"))' 2>&1)
  case "$real" in "$GVS"/*) echo "      resolved through the store: ok";; *) echo "      NOTE resolved elsewhere: $real";; esac
done

# ---------- CASE 4: deleting a project makes its entries collectable ----------
rm -rf "$SANDBOX/projB"
cd "$SANDBOX/projA" && "$NUB" store prune > "$SANDBOX/prune2.log" 2>&1
ok "deleted project deregistered" "$(ls "$GVS/.projects" | wc -l | tr -d ' ')" "1"
ok "projA survives its sibling's deletion" "$(node -e 'require("debug");console.log("ok")' 2>&1)" "ok"

# ---------- CASE 5: a second prune is a no-op (idempotent) ----------
BEFORE=$(entries)
"$NUB" store prune > "$SANDBOX/prune3.log" 2>&1
ok "re-prune is idempotent" "$(entries)" "$BEFORE"

# ---------- CASE 6: the registry dir itself is never swept ----------
ok ".projects survives the sweep" "$([ -d "$GVS/.projects" ] && echo present || echo gone)" "present"

echo
echo "=== $PASS passed, $FAIL failed ==="
cd /; rm -rf "$SANDBOX"
[ "$FAIL" -eq 0 ]
