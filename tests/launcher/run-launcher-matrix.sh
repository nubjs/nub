#!/usr/bin/env bash
# Exercise the POSIX self-heal launcher (npm/nub/bin/launch.js) against every on-PATH
# bin-entry shape, on the HOST. The heal is binary-agnostic, so a fake native
# (make-fixture.sh) stands in for a platform build. What each scenario and each style
# guards is documented in README.md.
#
# Styles: symlink (npm/bun/yarn) and the pnpm 10 / pnpm >=11 cmd-shims are what a real
# package manager writes. `scan`, `decl` and `declrel` are DERIVED — each withholds
# every route into leadsToUs but one, because leadsToUs returns on the trailer before
# it scans quotes, so a shape carrying both hazards cannot fail when either mechanism
# alone regresses. A style that cannot go red is not coverage; README.md carries the
# revert-to-red table that keeps that honest.
#
# Scenarios (all host-runnable; the non-owner staged-copy case needs Docker —
# docker-non-owner.sh):
#   heal           first call runs + rewrites the on-PATH entry to an sh trampoline
#   zero-node      second call spawns ZERO node (the shim-tax avoidance, ~50ms->~1ms)
#   polyglot       the healed entry, run AS a node script, still exec's native (race)
#   nubx-verb      nubx keeps its verb through the heal (bin/nubx, "nubx-mode")
#   ensure-chmod   a 0o644 native we OWN is chmod'd +x in place by ensureExecutable
#   foreign        `nub` files on PATH that do NOT resolve to us are left untouched —
#                  including ones that NAME our launcher without dispatching to it
#   concurrency    N concurrent first-calls -> 0 failures (the polyglot 0/600 claim)
#
# Usage: run-launcher-matrix.sh [node-bin-dir ...]
# With no node dirs it sweeps ~/.nvm/versions/node/* (so the heal is validated on
# both the fast tier 22.15+ and the compat tier 18.19-22.14); pass explicit bin dirs
# to target specific versions (or a container's /usr/local/bin). The launcher is the
# same JS on every Node, but the version sweep is cheap insurance.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FIXTURE_BASE="${NUB_LAUNCHER_FIXTURE:-/tmp/nub-launcher-fixture}"
CONCURRENCY_N="${NUB_LAUNCHER_CONCURRENCY:-200}"

PASS=0; FAIL=0
ok(){ echo "  ok: $1"; PASS=$((PASS+1)); }
no(){ echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

# Resolve which Node versions to sweep.
NODE_DIRS=("$@")
if [ "${#NODE_DIRS[@]}" -eq 0 ]; then
  for d in "$HOME"/.nvm/versions/node/*/bin; do
    [ -x "$d/node" ] && NODE_DIRS+=("$d")
  done
fi
if [ "${#NODE_DIRS[@]}" -eq 0 ]; then
  # No nvm: fall back to whatever `node` is on PATH.
  NDIR="$(dirname "$(command -v node)")"
  NODE_DIRS=("$NDIR")
fi

# --- per-(style,node) scenario block --------------------------------------------
run_block() {
  local style="$1" nodedir="$2"
  local dest="$FIXTURE_BASE-$style"
  bash "$SCRIPT_DIR/make-fixture.sh" "$dest" "$style" >/dev/null
  local BIN="$dest/bin"
  local HOST="$dest/node_modules/@nubjs/nub-host/bin"
  # R: invoke through the fixture's PATH (on-PATH entry first, then the logging node).
  R(){ PATH="$BIN:$dest/fakenode:$nodedir:$PATH" "$@" 2>&1; }

  echo "== $style shim · node $("$nodedir/node" --version) =="

  # heal: first call runs and rewrites the on-PATH entry.
  : > "$dest/node.log"
  local o; o=$(R "$BIN/nub" --version)
  case "$o" in *9.9.9-ci*) ok "first call runs ($style)";; *) no "first call: $o";; esac
  case "$(head -1 "$BIN/nub")" in
    "#!/bin/sh") : ;;
    *) no "healed entry not #!/bin/sh ($style): $(head -1 "$BIN/nub")";;
  esac
  case "$(cat "$BIN/nub")" in
    *nub-host/bin/nub\'*) ok "entry healed -> sh trampoline ($style)";;
    *) no "entry not healed ($style): $(tail -1 "$BIN/nub")";;
  esac

  # zero-node: the second call must spawn ZERO node.
  : > "$dest/node.log"
  local o2; o2=$(R "$BIN/nub" --version)
  case "$o2" in *9.9.9-ci*) ok "second call runs ($style)";; *) no "second call: $o2";; esac
  if [ -s "$dest/node.log" ]; then no "node spawned post-heal ($style): $(wc -l <"$dest/node.log") spawn(s)"
  else ok "zero node post-heal ($style)"; fi

  # polyglot: the healed entry run AS a node script still exec's native (the race the
  # 0/600 claim rests on — a concurrent node mid-heal reads the JS fallback, not sh).
  local pn; pn=$(PATH="$nodedir:$PATH" node "$BIN/nub" --version 2>&1)
  case "$pn" in *9.9.9-ci*) ok "healed entry runs via node too (polyglot) ($style)";;
    *) no "polyglot-as-node ($style): $pn";; esac

  # nubx-verb: nubx keeps its verb through the heal, off ONE shipped binary.
  # The platform package has no bin/nubx any more, so the healed entry must exec
  # bin/nub and carry the verb in __NUB_ARGV0. Assert the trampoline's SHAPE...
  : > "$dest/node.log"
  local ox; ox=$(R "$BIN/nubx" foo)
  case "$ox" in *nubx-mode*) ok "nubx verb dispatch, first call ($style)";; *) no "nubx verb ($style): $ox";; esac
  case "$(cat "$BIN/nubx")" in
    *__NUB_ARGV0=\'nubx\'*nub-host/bin/nub\'*) ok "nubx healed -> bin/nub + __NUB_ARGV0 ($style)";;
    *) no "nubx heal shape wrong ($style): $(sed -n 2p "$BIN/nubx")";;
  esac
  # ...and then that it still dispatches as nubx THROUGH that healed path. This second
  # call is the one that regressed when the duplicate was simply deleted: the first
  # call succeeds via the node fallback either way, and only call 2 exposes a
  # trampoline that lost the verb (exit 0, silently running `nub`).
  : > "$dest/node.log"
  local ox2; ox2=$(R "$BIN/nubx" foo)
  case "$ox2" in *nubx-mode*) ok "nubx verb survives the heal, second call ($style)";;
    *) no "nubx verb LOST post-heal ($style): $ox2";; esac
  if [ -s "$dest/node.log" ]; then no "node spawned post-heal for nubx ($style)"
  else ok "zero node post-heal for nubx ($style)"; fi

  # ensure-chmod: the fake native lands 0o644 (no +x). Because the FIRST call already
  # ran successfully above, ensureExecutable must have recovered it. We own the file
  # here, so the recovery is an in-place chmod — assert the native is now +x.
  if [ -x "$HOST/nub" ]; then ok "native chmod'd +x in place by ensureExecutable ($style)"
  else no "native still not executable after first call ($style)"; fi
}

# --- concurrency: N concurrent FIRST calls, 0 failures ---------------------------
# The headline claim: the sh/node polyglot heal needs no lock and survives a
# concurrent-first-call stampede with zero failures (pure-sh was ~6%/200). We fork N
# processes that all hit the UNHEALED on-PATH entry at once and assert every one
# prints the native's version line.
run_concurrency() {
  local style="$1" nodedir="$2"
  local dest="$FIXTURE_BASE-conc-$style"
  bash "$SCRIPT_DIR/make-fixture.sh" "$dest" "$style" >/dev/null
  local BIN="$dest/bin"
  local outdir="$dest/conc-out"; mkdir -p "$outdir"
  echo "== concurrency · $style · $CONCURRENCY_N first-calls · node $("$nodedir/node" --version) =="

  local i
  for i in $(seq 1 "$CONCURRENCY_N"); do
    ( PATH="$BIN:$dest/fakenode:$nodedir:$PATH" "$BIN/nub" --version >"$outdir/$i.out" 2>&1 ) &
  done
  wait

  local bad=0
  for i in $(seq 1 "$CONCURRENCY_N"); do
    if ! grep -q '9.9.9-ci' "$outdir/$i.out"; then
      bad=$((bad+1))
      [ "$bad" -le 3 ] && echo "    proc $i output: $(head -c 200 "$outdir/$i.out")"
    fi
  done
  if [ "$bad" -eq 0 ]; then ok "concurrency $CONCURRENCY_N/$CONCURRENCY_N first-calls OK ($style)"
  else no "concurrency: $bad/$CONCURRENCY_N first-calls FAILED ($style)"; fi
  # And the entry ended up healed (some winner rewrote it).
  case "$(cat "$BIN/nub")" in *nub-host/bin/nub\'*) ok "entry healed after stampede ($style)";;
    *) no "entry not healed after stampede ($style)";; esac
}

# --- foreign nub: verify-before-clobber (leadsToUs) ------------------------------
run_foreign() {
  local nodedir="$1"
  local dest="$FIXTURE_BASE-foreign"
  bash "$SCRIPT_DIR/make-fixture.sh" "$dest" symlink >/dev/null
  echo "== verify-before-clobber (foreign nub) =="
  mkdir -p "$dest/foreign"
  local ourreal
  ourreal=$("$nodedir/node" -e 'console.log(require("fs").realpathSync(process.argv[1]))' \
    "$dest/node_modules/@nubjs/nub/bin/nub")

  # Each case: a foreign `nub` that must survive byte-for-byte. healPathEntry renames
  # over its match with no backup, so a false positive is unrecoverable loss in a file
  # we do not own — and an unrelated `nub@1.0.0` really is on npm.
  probe_foreign() {
    local label="$1" body="$2" mode="$3"
    printf '%b' "$body" > "$dest/foreign/nub"
    chmod "$mode" "$dest/foreign/nub"
    local before; before=$(cat "$dest/foreign/nub")
    # Run OUR launcher with the foreign nub FIRST on PATH. Our launcher must heal only
    # an entry whose realpath leads to us; the foreign one must be left untouched.
    PATH="$dest/foreign:$dest/bin:$nodedir:$PATH" \
      "$dest/node_modules/@nubjs/nub/bin/nub" --version >/dev/null 2>&1
    if [ "$before" = "$(cat "$dest/foreign/nub")" ]; then ok "foreign untouched: $label"
    else no "foreign CLOBBERED: $label"; fi
  }

  probe_foreign "plain shim" '#!/bin/sh\necho foreign-untouched\n' 0755
  # A file that only MENTIONS our launcher is not a shim that dispatches to it. These
  # pin the two ways a `# cmd-shim-target=` line could be over-trusted: no `exec` in
  # the body at all, and a `\s`-class straddling the newline between `#` and the key.
  probe_foreign "declares our target, never execs" \
    "#!/bin/sh\necho foreign-untouched\n# cmd-shim-target=$ourreal\n" 0755
  probe_foreign "bare # then newline then key" \
    "#!/bin/sh\nexec echo foreign-untouched\n#\ncmd-shim-target=$ourreal\n" 0755
  # A file with no shebang is not a shim at all, whatever it declares. The `#!` check
  # is a clobber guard, not only the perf guard the size cap is — without this probe it
  # could be deleted with the matrix still green.
  probe_foreign "no shebang, declares our target" \
    "# cmd-shim-target=$ourreal\nexec echo foreign-untouched\n" 0644
}

# Sweep the matrix. Concurrency + foreign run once per Node (style varies inside).
#
# `scan`/`decl`/`declrel` are parse-isolating styles, so they run through run_block
# only. The concurrency scenario guards the polyglot WRITE race, which is identical
# whichever route matched in leadsToUs — running it per parse style bought nothing
# and cost 200 forks each.
for nodedir in "${NODE_DIRS[@]}"; do
  for style in symlink pnpm pnpm11 scan decl declrel; do
    run_block "$style" "$nodedir"
  done
  for style in symlink pnpm pnpm11; do
    run_concurrency "$style" "$nodedir"
  done
  run_foreign "$nodedir"
done

echo
echo "RESULT: $PASS ok, $FAIL failed"
[ "$FAIL" -eq 0 ]
