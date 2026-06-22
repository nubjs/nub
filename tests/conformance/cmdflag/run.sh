#!/usr/bin/env bash
# Command×flag conformance harness — exercise nub's FULL CLI surface against a
# REAL repo and assert each command behaves (exits cleanly where it should,
# fails correctly where it should, and — where parity is claimed — agrees with
# the reference package manager). This is the durable backstop for the gap that
# let `nub audit` ship a real-machine failure: shallow happy-path probing.
#
# DISTINCT from the lockfile harness one level up (tests/conformance/run.sh),
# which verifies LOCKFILE round-trip fidelity. This one verifies the COMMAND ×
# FLAG surface — every wired verb + its major flags actually runs.
#
# See README.md for the loop; inventory.tsv for the canonical surface;
# expectations.txt for the known-failure red list.
#
# Usage:   run.sh <path-to-nub> <fixture-dir> [id ...]
#            <fixture-dir>  a real project checkout (has package.json). The
#                           harness operates on COPIES — the fixture is never
#                           dirtied.
#            [id ...]       restrict to specific inventory ids (default: all).
#
# Env:
#   REFPM=pnpm           reference PM for parity diffs (pnpm|npm|yarn|bun).
#   REF=1                run the reference PM for parity-tagged cells + compare
#                        exit-code agreement. Off by default (slow/network).
#   NET=1                also run the `net` cells (registry/network/TTY). Off by
#                        default so the core sweep is hermetic + offline-ish.
#   USER_NPMRC=<file>    seed the sandbox HOME's ~/.npmrc from this file BEFORE
#                        any cell runs — the real-world condition (a custom
#                        `registry=`) that broke `nub audit`. Empty = clean.
#   KEEP=1               keep the sandbox for forensics.
#   SANDBOX_ROOT=<dir>   reuse/inspect a sandbox (implies KEEP).
set -uo pipefail   # NOT -e: a failing cell is data, not a harness abort.

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ $# -lt 2 ]; then
  echo "usage: run.sh <path-to-nub> <fixture-dir> [id ...]" >&2
  exit 2
fi
NUB="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
{ [ -x "$NUB" ] || ! [ -x "$NUB.exe" ]; } || NUB="$NUB.exe"
[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }
FIXTURE="$(cd "$2" && pwd)"
[ -f "$FIXTURE/package.json" ] || { echo "error: fixture has no package.json: $FIXTURE" >&2; exit 2; }
shift 2
ONLY=("$@")

REFPM="${REFPM:-pnpm}"
REF="${REF:-0}"
NET="${NET:-0}"

# Hermetic sandbox HOME so the dev box's ~/.npmrc / caches / stores don't leak
# in or get clobbered. (Same discipline as tests/aube-conformance/run.sh.)
CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-cmdconf.XXXXXX")"
  CREATED_SANDBOX=1
fi
mkdir -p "$SANDBOX_ROOT/home" "$SANDBOX_ROOT/runs" "$SANDBOX_ROOT/logs"
export HOME="$SANDBOX_ROOT/home"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

# Seed the user ~/.npmrc — the real-world config that broke `nub audit`. The
# point of the harness is to cover REAL machine state, not a pristine void.
if [ -n "${USER_NPMRC:-}" ]; then
  [ -f "$USER_NPMRC" ] || { echo "error: USER_NPMRC not a file: $USER_NPMRC" >&2; exit 2; }
  cp "$USER_NPMRC" "$HOME/.npmrc"
  echo "seeded ~/.npmrc from $USER_NPMRC:"
  sed 's/^/    | /' "$HOME/.npmrc"
fi

echo "== nub command×flag conformance =="
echo "nub:      $NUB ($("$NUB" --version 2>/dev/null || echo '?'))"
echo "node:     $(node --version 2>/dev/null || echo MISSING)"
echo "fixture:  $FIXTURE"
echo "refpm:    $REFPM (parity diff: $([ "$REF" = 1 ] && echo on || echo off))"
echo "net:      $([ "$NET" = 1 ] && echo on || echo off)"
echo "sandbox:  $SANDBOX_ROOT"
echo

# A pristine copy for read-only cells, primed with one `install` so queries have
# a node_modules to read. mut/net cells each get their own throwaway copy.
RO_PROJ="$SANDBOX_ROOT/runs/_ro"
rm -rf "$RO_PROJ"; mkdir -p "$RO_PROJ"
cp -R "$FIXTURE/." "$RO_PROJ/"
if [ "${PRIME_RO:-1}" = 1 ]; then
  echo "priming RO copy: nub install (so read-only queries have a node_modules)…"
  (cd "$RO_PROJ" && "$NUB" install) >"$SANDBOX_ROOT/logs/_ro-prime.log" 2>&1 \
    && echo "  prime OK" || echo "  prime FAILED (read-only cells may degrade) — see logs/_ro-prime.log"
fi

expected_reason() {
  awk -v id="$1" '$1==id { $1=""; sub(/^[ \t]*/,""); print; exit }' \
    "$HERE/expectations.txt" 2>/dev/null
}

RESULTS=()
FAILS=0; XPASSES=0
run_cell() {
  local id="$1" kind="$2" parity="$3"; shift 3
  local -a cell_args=("$@")
  local log="$SANDBOX_ROOT/logs/$id.log"

  if [ "$kind" = net ] && [ "$NET" != 1 ]; then
    RESULTS+=("$id|SKIP-net|(NET=1 to run)"); echo "--- $id  SKIP-net"; return
  fi

  local proj
  case "$kind" in
    meta) proj="$SANDBOX_ROOT" ;;
    mut|net)
      proj="$SANDBOX_ROOT/runs/$id"
      rm -rf "$proj"; mkdir -p "$proj"; cp -R "$FIXTURE/." "$proj/" ;;
    ro|*) proj="$RO_PROJ" ;;
  esac

  echo "--- $id  ($kind)  \$ nub ${cell_args[*]}"
  { echo "### nub ${cell_args[*]}   (cwd=$proj, kind=$kind)"; } >"$log"
  local code=0
  (cd "$proj" && "$NUB" "${cell_args[@]}") >>"$log" 2>&1 || code=$?
  echo "### exit=$code" >>"$log"

  local leak=""
  grep -qiE '\baube\b' "$log" && leak=" [BRAND-LEAK:aube]"

  local parity_note=""
  if [ "$REF" = 1 ] && [ "$parity" != "-" ]; then
    local refproj="$SANDBOX_ROOT/runs/${id}__ref"
    rm -rf "$refproj"; mkdir -p "$refproj"; cp -R "$FIXTURE/." "$refproj/"
    local refcode=0
    (cd "$refproj" && "$REFPM" "$parity" "${cell_args[@]:1}") >"$log.ref" 2>&1 || refcode=$?
    echo "### refpm($REFPM $parity) exit=$refcode" >>"$log"
    if { [ "$code" = 0 ] && [ "$refcode" = 0 ]; } || { [ "$code" != 0 ] && [ "$refcode" != 0 ]; }; then
      parity_note=" parity:agree($code/$refcode)"
    else
      parity_note=" parity:DIVERGE(nub=$code ref=$refcode)"
    fi
  fi

  local reason; reason="$(expected_reason "$id")"
  local status
  if [ "$code" = 0 ] && [ -z "$leak" ]; then
    if [ -n "$reason" ]; then status="XPASS-STALE"; XPASSES=$((XPASSES+1));
      echo "    XPASS: now passes — delete its expectations.txt entry: $reason"
    else status="PASS"; fi
  else
    if [ -n "$reason" ]; then status="RED(expected)"; echo "    expected red: $reason";
    else status="FAIL"; FAILS=$((FAILS+1));
      echo "    FAIL (exit=$code$leak) — log: $log"; tail -n 12 "$log" | sed 's/^/    | /'; fi
  fi
  RESULTS+=("$id|$status$leak$parity_note|exit=$code")
}

want_id() {
  [ ${#ONLY[@]} -eq 0 ] && return 0
  local x; for x in "${ONLY[@]}"; do [ "$x" = "$1" ] && return 0; done; return 1
}

while IFS=$'\t' read -r id kind parity args; do
  [ -z "$id" ] && continue
  case "$id" in \#*) continue ;; esac
  want_id "$id" || continue
  # shellcheck disable=SC2206
  read -r -a argv <<<"$args"
  run_cell "$id" "$kind" "$parity" "${argv[@]}"
done < <(grep -vE '^[[:space:]]*(#|$)' "$HERE/inventory.tsv")

echo
echo "== results =="
printf '%-22s %-34s %s\n' "id" "status" "detail"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r i s d <<<"$row"
  printf '%-22s %-34s %s\n' "$i" "$s" "$d"
done
echo

if [ "$FAILS" -gt 0 ] || [ "$XPASSES" -gt 0 ]; then
  echo "RESULT: FAIL ($FAILS unexpected, $XPASSES stale expected-failure entries)"
  echo "sandbox kept for forensics: $SANDBOX_ROOT"
  exit 1
fi
echo "RESULT: OK"
if [ "$CREATED_SANDBOX" = 1 ] && [ "${KEEP:-0}" != 1 ]; then rm -rf "$SANDBOX_ROOT";
else echo "sandbox kept: $SANDBOX_ROOT"; fi
