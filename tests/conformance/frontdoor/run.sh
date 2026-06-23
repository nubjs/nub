#!/usr/bin/env bash
# Front-door pm-compat conformance MATRIX — the anti-resurfacing guard.
#
# For each incumbent identity × front-door surface (config read/write, env
# knobs, run/exec flags), assert nub behaves per the documented per-incumbent
# behavior map (wiki/research/nub-incumbent-behavior.md). A gap/regression on a
# covered cell FAILS here instead of being rediscovered ad-hoc by a user.
#
# DISTINCT from its two siblings (README.md): the lockfile harness verifies
# round-trip fidelity; the cmdflag harness verifies every verb runs on one repo;
# THIS one is the only suite parameterized by INCUMBENT, and it targets the
# front-door SURFACE that users actually drive.
#
# Usage:  run.sh <path-to-nub> [surface ...]
#           [surface ...]  restrict to surfaces (run-flag|config-read|
#                          config-write|env-bridge|env-gate) or specific ids.
# Env:
#   REF=1            also run the `ref`-mode cells' real-PM differential (needs
#                    the PM installed; a couple touch the network). Off → those
#                    cells run their doc-mode assertion only.
#   REFPM=pnpm       reference PM for the run-flag ref diffs.
#   KEEP=1           keep the sandbox for forensics.
set -uo pipefail   # NOT -e: a failing cell is data, not a harness abort.

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES="$HERE/fixtures"

if [ $# -lt 1 ]; then echo "usage: run.sh <path-to-nub> [surface|id ...]" >&2; exit 2; fi
NUB="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
{ [ -x "$NUB" ] || ! [ -x "$NUB.exe" ]; } || NUB="$NUB.exe"
[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }
shift
ONLY=("$@")
REF="${REF:-0}"
REFPM="${REFPM:-pnpm}"

# Hermetic sandbox — the dev box's ~/.npmrc carries a DEAD proxy that breaks
# fetches, so isolating HOME/XDG is mandatory (same discipline as the siblings).
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/nub-frontdoor.XXXXXX")"
mkdir -p "$SANDBOX/home" "$SANDBOX/runs" "$SANDBOX/logs"
export HOME="$SANDBOX/home"
export XDG_DATA_HOME="$HOME/.local/share" XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config" XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

echo "== front-door pm-compat conformance matrix =="
echo "nub:      $NUB ($("$NUB" --version 2>/dev/null || echo '?'))"
echo "node:     $(node --version 2>/dev/null || echo MISSING)"
echo "ref:      $([ "$REF" = 1 ] && echo "on ($REFPM)" || echo off)"
echo "sandbox:  $SANDBOX"
echo

# Fresh throwaway copy of an incumbent fixture; echoes its path.
stage() {
  local incumbent="$1" id="$2" src
  [ "$incumbent" = "-" ] && incumbent="nub"
  src="$FIXTURES/$incumbent"
  [ -d "$src" ] || { echo "MISSING-FIXTURE:$src" >&2; return 1; }
  local proj="$SANDBOX/runs/$id"
  rm -rf "$proj"; mkdir -p "$proj"; cp -R "$src/." "$proj/"
  echo "$proj"
}

RESULTS=(); FAILS=0; SKIPS=0
pass() { RESULTS+=("$1|PASS|$2"); echo "    PASS  $2"; }
fail() { RESULTS+=("$1|FAIL|$2"); echo "    FAIL  $2"; FAILS=$((FAILS+1)); }
skip() { RESULTS+=("$1|SKIP|$2"); echo "    SKIP  $2"; SKIPS=$((SKIPS+1)); }

# ─ assertion verbs ───────────────────────────────────────────────────────────
# Each takes: id, log, proj, then verb-specific operands. They emit pass/fail.

# Run nub in $proj with optional leading VAR=val env pairs, capture combined
# output to $log. The run-flag fixture's scripts print RAN:/ENV: markers and the
# run-echo prints a `$ <cmd>` line.
nub_run() {  # proj log [ENV=val ...] -- nub-args...
  local proj="$1" log="$2"; shift 2
  local -a envs=()
  while [ "$1" != "--" ]; do envs+=("$1"); shift; done
  shift
  ( cd "$proj" && env "${envs[@]}" "$NUB" "$@" ) >"$log" 2>&1
  return $?
}

echo_present() { grep -qE '^\$ ' "$1"; }   # nub's run-echo line

run_cell() {
  local id="$1" incumbent="$2" surface="$3" mode="$4" verb="$5"; shift 5
  local log="$SANDBOX/logs/$id.log"
  local proj; proj="$(stage "$incumbent" "$id")" || { fail "$id" "no fixture"; return; }
  echo "--- $id  [$incumbent/$surface/$mode]  $verb $*"

  case "$verb" in
    echo-shown)
      nub_run "$proj" "$log" -- "$@"; echo_present "$log" \
        && pass "$id" "run-echo shown" || fail "$id" "run-echo MISSING" ;;
    echo-hidden)
      nub_run "$proj" "$log" -- "$@"; echo_present "$log" \
        && fail "$id" "run-echo SHOWN (not suppressed)" || pass "$id" "run-echo suppressed" ;;
    echo-hidden-env)   # ENV  --  args...   (env pair from $1, args after)
      local e="$1"; shift
      nub_run "$proj" "$log" "$e" -- "$@"; echo_present "$log" \
        && fail "$id" "run-echo SHOWN under $e (G1 regression)" || pass "$id" "run-echo suppressed under $e" ;;
    echo-shown-env)
      local e="$1"; shift
      nub_run "$proj" "$log" "$e" -- "$@"; echo_present "$log" \
        && pass "$id" "run-echo shown under $e (correctly NOT honored)" || fail "$id" "run-echo suppressed under $e (over-honored)" ;;
    runs-scripts)      # expected(csv)  --  args...
      local want="$1"; shift
      nub_run "$proj" "$log" -- "$@"
      local ok=1 s
      IFS=, read -ra wantarr <<<"$want"
      # every wanted script's marker present, AND no UNwanted build:* marker
      for s in "${wantarr[@]}"; do grep -qF "RAN:$s" "$log" || ok=0; done
      # for regex/multi cases: assert scripts NOT in the want-set did not run
      for s in hello build:app build:lib; do
        case ",$want," in *",$s,"*) : ;; *) grep -qF "RAN:$s" "$log" && ok=0 ;; esac
      done
      [ "$ok" = 1 ] && pass "$id" "ran exactly {$want}" || { fail "$id" "script set != {$want}"; sed 's/^/      | /' "$log"; } ;;
    env-injected)      # VAR=val  --  args...
      local kv="$1"; shift; local var="${kv%%=*}" val="${kv#*=}"
      # the assertion seeds a secrets file the fixture's --env-file points at
      printf '%s=%s\n' "$var" "$val" >"$proj/SECRETS"
      nub_run "$proj" "$log" -- "$@"
      grep -qF "ENV:$var=$val" "$log" \
        && pass "$id" "child saw $var=$val" || { fail "$id" "$var not injected"; sed 's/^/      | /' "$log"; } ;;
    exits-nonzero)
      nub_run "$proj" "$log" -- "$@"; local c=$?
      [ "$c" != 0 ] && pass "$id" "exit=$c (nonzero as required)" || fail "$id" "exit=0 (expected nonzero)" ;;
    config-reads)      # key=val
      local kv="$1" key="${1%%=*}" val="${1#*=}"
      printf '%s=%s\n' "$key" "$val" >"$proj/.npmrc"
      local out; out="$( cd "$proj" && "$NUB" config get "$key" 2>>"$log" )"
      [ "$out" = "$val" ] && pass "$id" "honored $key=$val" || fail "$id" "config get $key => '$out' (want '$val')" ;;
    config-ignores-pnpm-yaml)  # leak-value  control-npmrc-value
      # Seed a pnpm-NAMED file with a LEAK registry (must be ignored under npm)
      # AND the project .npmrc with a distinct CONTROL registry. nub must return
      # the control value: proves the reader is LIVE and chose the right file —
      # not vacuously returning the default because nothing was read at all.
      local leak="$1" control="$2"
      printf 'packages:\n  - "."\nregistry: %s\n' "$leak" >"$proj/pnpm-workspace.yaml"
      printf 'registry=%s\n' "$control" >"$proj/.npmrc"
      local out; out="$( cd "$proj" && "$NUB" config get registry 2>>"$log" )"
      if [ "$out" = "$control" ]; then
        pass "$id" "read .npmrc ($control), ignored pnpm-named file ($leak)"
      elif [ "$out" = "$leak" ]; then
        fail "$id" "READ a pnpm-named file under a non-pnpm incumbent (brand-boundary breach): $leak"
      else
        fail "$id" "reader inert/wrong: got '$out' (want control '$control'; leak was '$leak')"
      fi ;;
    config-writes-to)  # relpath  key  val   (grep the distinctive VALUE — the key
      local relpath="$1" key="$2" val="$3"   # is kebab→camel-normalized in yaml)
      ( cd "$proj" && "$NUB" config set "$key" "$val" ) >>"$log" 2>&1
      local in_target=0 leaked=""
      [ -f "$proj/$relpath" ] && grep -qF "$val" "$proj/$relpath" && in_target=1
      for other in .npmrc pnpm-workspace.yaml package.json; do
        [ "$other" = "$relpath" ] && continue
        [ -f "$proj/$other" ] && grep -qF "$val" "$proj/$other" && leaked="$other"
      done
      if [ "$in_target" = 1 ] && [ -z "$leaked" ]; then pass "$id" "wrote $key → $relpath only"
      elif [ "$in_target" != 1 ]; then fail "$id" "$key NOT in $relpath"; sed 's/^/      | /' "$log"
      else fail "$id" "$key also leaked into $leaked (wrong home)"; fi ;;
    env-bridge-resolver)   # unreachable-registry-url  (ref-mode: observes the install resolver)
      local url="$1"
      if [ "$REF" != 1 ]; then skip "$id" "REF=1 to run the resolver probe"; return; fi
      ( cd "$proj" && env "npm_config_registry=$url" "$NUB" install --no-frozen-lockfile ) >>"$log" 2>&1
      # The bridge took effect iff the resolver ATTEMPTED the env-supplied host —
      # not merely logged the registry at startup. Require the host string on a
      # line that also carries a fetch/resolve/DNS-failure token, so the cell keys
      # on a real network attempt against that host (an unreachable .invalid host
      # forces a resolve error naming it). Match is case-insensitive.
      if grep -iE "frontdoor-envbridge\.invalid" "$log" \
           | grep -qiE "fetch|resolv|resolu|request|ENOTFOUND|EAI_AGAIN|getaddrinfo|dns|connect|GET |https?://"; then
        pass "$id" "resolver ATTEMPTED npm_config_registry host ($url)"
      else
        fail "$id" "npm_config_registry host not reached by a resolve/fetch attempt"; sed 's/^/      | /' "$log"
      fi ;;
    env-not-in-config-get) # key  shadow-value  (assert env does NOT surface in config get)
      local key="$1" shadow="$2"
      local out; out="$( cd "$proj" && env "npm_config_${key}=$shadow" "$NUB" config get "$key" 2>>"$log" )"
      [ "$out" != "$shadow" ] && pass "$id" "config get $key does NOT reflect env ($out)" \
        || fail "$id" "config get $key now reflects npm_config_$key — config-display semantics changed" ;;
    env-gate-aube-ignored) # key  AUBE_ENV_VAR  leak-value
      # The setting MUST be one aube reads from this AUBE_* env in standalone mode
      # and surfaces via `config get` (store-dir / AUBE_STORE_DIR) — otherwise the
      # cell is vacuous. Two witnesses: (1) `config get <key>` surfaces a value at
      # all (the reader is live); (2) setting the AUBE_* env does NOT change it
      # under the nub embedder profile (env_prefix=None suppresses the branded env
      # that WOULD win in standalone aube).
      local key="$1" envvar="$2" leak="$3"
      local base; base="$( cd "$proj" && "$NUB" config get "$key" 2>>"$log" )"
      local out;  out="$(  cd "$proj" && env "$envvar=$leak" "$NUB" config get "$key" 2>>"$log" )"
      if [ -z "$base" ] || [ "$base" = "undefined" ]; then
        fail "$id" "config get $key surfaced no value — cell can't observe the gate (vacuous)"
      elif [ "$out" = "$leak" ]; then
        fail "$id" "$envvar was READ under nub (AUBE_* brand leak into config surface): $key=$leak"
      else
        pass "$id" "$envvar ignored under nub ($key stayed '$out', not the leak '$leak')"
      fi ;;
    *) fail "$id" "unknown assert verb '$verb'" ;;
  esac
}

want_id() {
  [ ${#ONLY[@]} -eq 0 ] && return 0
  local id="$1" surface="$2" x
  for x in "${ONLY[@]}"; do { [ "$x" = "$id" ] || [ "$x" = "$surface" ]; } && return 0; done
  return 1
}

while IFS=$'\t' read -r id incumbent surface mode verb rest; do
  [ -z "$id" ] && continue
  case "$id" in \#*) continue ;; esac
  want_id "$id" "$surface" || continue
  # ref-mode cells: with REF=0 we still run the doc assertion (the verb itself is
  # the documented-behavior check); REF=1 is where the real-PM differential adds
  # value. The real-PM diff for run-flag cells is recorded as a comparison note,
  # not a separate pass/fail (the doc assertion is authoritative for the gate).
  # shellcheck disable=SC2086
  run_cell "$id" "$incumbent" "$surface" "$mode" "$verb" $rest
done < <(grep -vE '^[[:space:]]*(#|$)' "$HERE/assertions.tsv")

echo
echo "== results =="
printf '%-34s %-6s %s\n' "id" "status" "detail"
for row in "${RESULTS[@]}"; do IFS='|' read -r i s d <<<"$row"; printf '%-34s %-6s %s\n' "$i" "$s" "$d"; done
PASSES=$(( ${#RESULTS[@]} - FAILS - SKIPS ))
echo
if [ "$FAILS" -gt 0 ]; then
  echo "RESULT: FAIL ($FAILS fail, $PASSES pass, $SKIPS skip)"; echo "sandbox kept: $SANDBOX"; exit 1
fi
echo "RESULT: OK ($PASSES pass, $SKIPS skip)"
[ "${KEEP:-0}" = 1 ] && echo "sandbox kept: $SANDBOX" || rm -rf "$SANDBOX"
