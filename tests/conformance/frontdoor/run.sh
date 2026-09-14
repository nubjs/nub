#!/usr/bin/env bash
# Front-door pm-compat conformance MATRIX — the anti-resurfacing guard.
#
# For each project identity × front-door surface (config read/write, env knobs,
# run/exec flags, a foreign lockfile), assert nub behaves as that identity
# requires. There are two: a pnpm project (a pnpm pin, `pnpm-lock.yaml` or
# `pnpm-workspace.yaml`, per crates/nub-core/src/pm/identity.rs) behaves exactly
# like pnpm 12.4.1, and every other project is nub's. A gap/regression on a
# covered cell FAILS here instead of being rediscovered ad-hoc by a user.
#
# DISTINCT from its two siblings (README.md): the lockfile harness verifies
# round-trip fidelity; the cmdflag harness verifies every verb runs on one repo;
# THIS one is the only suite parameterized by IDENTITY, and it targets the
# front-door SURFACE that users actually drive.
#
# Usage:  run.sh <path-to-nub> [surface ...]
#           [surface ...]  restrict to surfaces (run-flag|config-read|
#                          config-write|env-bridge|env-gate|lockfile) or ids.
# Env:
#   REF=1            also run the `ref`-mode cells, which install against the
#                    network. Off → they SKIP.
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

# Hermetic sandbox — the dev box's ~/.npmrc carries a DEAD proxy that breaks
# fetches, so isolating HOME/XDG is mandatory (same discipline as the siblings).
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/nub-frontdoor.XXXXXX")"
mkdir -p "$SANDBOX/home" "$SANDBOX/homes" "$SANDBOX/runs" "$SANDBOX/logs"
export HOME="$SANDBOX/home"
export XDG_DATA_HOME="$HOME/.local/share" XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config" XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"
# A config variable the caller exported is the same input the env cells set, so
# an inherited one decides a cell before it runs. Running the suite through a
# package manager's own `run` is enough to inherit a dozen.
# `PNPM_` and not `PNPM_CONFIG_`: a pnpm project honors PNPM_HOME, so a developer's
# own PNPM_HOME would move that cell's store out of the sandbox and into their
# real one.
for var in $(env | grep -oE '^(npm_config_|NPM_CONFIG_|pnpm_config_|PNPM_)[A-Za-z0-9_]*' || true); do
  unset "$var"
done

echo "== front-door pm-compat conformance matrix =="
echo "nub:      $NUB ($("$NUB" --version 2>/dev/null || echo '?'))"
echo "node:     $(node --version 2>/dev/null || echo MISSING)"
echo "ref:      $([ "$REF" = 1 ] && echo on || echo off)"
echo "sandbox:  $SANDBOX"
echo

# Fresh throwaway copy of a fixture; echoes its path.
stage() {
  local fixture="$1" id="$2" src
  [ "$fixture" = "-" ] && fixture="nub"
  src="$FIXTURES/$fixture"
  [ -d "$src" ] || { echo "MISSING-FIXTURE:$src" >&2; return 1; }
  local proj="$SANDBOX/runs/$id"
  rm -rf "$proj"; mkdir -p "$proj"; cp -R "$src/." "$proj/"
  echo "$proj"
}

# A listed cell is a known product divergence: it stays red without failing the
# run, and the moment it passes the run fails so the entry cannot outlive the bug.
expected_reason() {
  awk -v id="$1" '$1==id { $1=""; sub(/^[ \t]*/,""); print; exit }' "$HERE/expectations.txt" 2>/dev/null
}

RESULTS=(); FAILS=0; SKIPS=0; XFAILS=0
pass() {
  local reason; reason="$(expected_reason "$1")"
  if [ -n "$reason" ]; then
    RESULTS+=("$1|XPASS-STALE|$2"); echo "    XPASS-STALE  $2 — delete its expectations.txt entry"; FAILS=$((FAILS+1))
  else RESULTS+=("$1|PASS|$2"); echo "    PASS  $2"; fi
}
fail() {
  local reason; reason="$(expected_reason "$1")"
  if [ -n "$reason" ]; then
    RESULTS+=("$1|XFAIL|$2"); echo "    XFAIL $2 — expected: $reason"; XFAILS=$((XFAILS+1))
  else RESULTS+=("$1|FAIL|$2"); echo "    FAIL  $2"; FAILS=$((FAILS+1)); fi
}
skip() { RESULTS+=("$1|SKIP|$2"); echo "    SKIP  $2"; SKIPS=$((SKIPS+1)); }

# `~/x` names a file in the sandbox HOME, anything else a file in the project.
resolve_path() {  # proj relpath
  case "$2" in "~/"*) echo "$HOME/${2#\~/}" ;; *) echo "$1/$2" ;; esac
}

# Write `registry` = value in the syntax the named file speaks.
seed_registry() {  # path value
  mkdir -p "$(dirname "$1")"
  case "$(basename "$1")" in
    pnpm-workspace.yaml) printf 'packages:\n  - "."\nregistry: %s\n' "$2" >"$1" ;;
    config.yaml)         printf 'registry: %s\n' "$2" >"$1" ;;
    .yarnrc.yml)         printf 'npmRegistryServer: "%s"\n' "$2" >"$1" ;;
    bunfig.toml)         printf '[install]\nregistry = "%s"\n' "$2" >"$1" ;;
    *) return 1 ;;
  esac
}

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
  # The `+` form: bash 3.2 (macOS /bin/bash) calls an empty array unbound under
  # `set -u`, which killed the subshell before nub ran — and every echo-hidden
  # cell then passed on an empty log.
  ( cd "$proj" && env ${envs[@]+"${envs[@]}"} "$NUB" "$@" ) >"$log" 2>&1
  return $?
}

echo_present() { grep -qE '^\$ ' "$1"; }   # nub's run-echo line

run_cell() {
  local id="$1" fixture="$2" surface="$3" mode="$4" verb="$5"; shift 5
  local log="$SANDBOX/logs/$id.log"
  local proj; proj="$(stage "$fixture" "$id")" || { fail "$id" "no fixture"; return; }
  echo "--- $id  [$fixture/$surface/$mode]  $verb $*"
  # A HOME per cell: a shared one let an earlier cell's metadata cache or global
  # config decide a later cell, so a cell passed in the suite and failed alone.
  export HOME="$SANDBOX/homes/$id"
  export XDG_DATA_HOME="$HOME/.local/share" XDG_CACHE_HOME="$HOME/.cache"
  export XDG_CONFIG_HOME="$HOME/.config" XDG_STATE_HOME="$HOME/.local/state"
  mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

  case "$verb" in
    echo-shown)
      nub_run "$proj" "$log" -- "$@"; echo_present "$log" \
        && pass "$id" "run-echo shown" || fail "$id" "run-echo MISSING" ;;
    echo-hidden)
      nub_run "$proj" "$log" -- "$@"
      if ! grep -qF "RAN:" "$log"; then fail "$id" "script never ran — nothing to judge the echo against"; sed 's/^/      | /' "$log"
      elif echo_present "$log"; then fail "$id" "run-echo SHOWN (not suppressed)"
      else pass "$id" "run-echo suppressed"; fi ;;
    echo-hidden-env)   # ENV  --  args...   (env pair from $1, args after)
      local e="$1"; shift
      nub_run "$proj" "$log" "$e" -- "$@"
      if ! grep -qF "RAN:" "$log"; then fail "$id" "script never ran — nothing to judge the echo against"; sed 's/^/      | /' "$log"
      elif echo_present "$log"; then fail "$id" "run-echo SHOWN under $e (G1 regression)"
      else pass "$id" "run-echo suppressed under $e"; fi ;;
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
      local key="${1%%=*}" val="${1#*=}"
      printf '%s=%s\n' "$key" "$val" >"$proj/.npmrc"
      local out; out="$( cd "$proj" && "$NUB" config get "$key" 2>>"$log" )"
      [ "$out" = "$val" ] && pass "$id" "honored $key=$val" || fail "$id" "config get $key => '$out' (want '$val')" ;;
    config-file)       # file  value  control|-  honored|ignored
      # Seed `registry` in a config file one identity reads and the other must
      # not. With a control, the project .npmrc carries a second registry: an
      # `ignored` row must return THAT, which proves the reader is live rather
      # than returning a default because it read nothing, and an `honored` row
      # proves the file outranks .npmrc. With no control (a HOME file that a
      # project .npmrc would outrank anyway), `ignored` leans on its `honored`
      # twin in the other identity to keep the file live.
      local file="$1" val="$2" control="$3" want="$4" path
      path="$(resolve_path "$proj" "$file")"
      seed_registry "$path" "$val" || { fail "$id" "config-file: no registry syntax for $file"; return; }
      [ "$control" != "-" ] && printf 'registry=%s\n' "$control" >"$proj/.npmrc"
      local out; out="$( cd "$proj" && "$NUB" config get registry 2>>"$log" )"
      case "$want" in
        honored)
          [ "$out" = "$val" ] && pass "$id" "read $file ($val)" \
            || fail "$id" "$file NOT honored: config get registry => '$out' (want '$val')" ;;
        ignored)
          if [ "$out" = "$val" ]; then fail "$id" "READ $file, which this identity must not read: $val"
          elif [ "$control" != "-" ] && [ "$out" != "$control" ]; then fail "$id" "reader inert/wrong: got '$out' (want control '$control')"
          elif [ -z "$out" ] || [ "$out" = "undefined" ]; then fail "$id" "config get registry surfaced no value — cell can't observe the gate (vacuous)"
          else pass "$id" "ignored $file (registry stayed '$out')"; fi ;;
        *) fail "$id" "config-file: want must be honored|ignored, got '$want'" ;;
      esac ;;
    config-writes-to)  # target  key  val  [config-set flags...]
      # Grep the distinctive VALUE, not the key: pnpm's yaml homes store it
      # camelCased (store-dir → storeDir).
      local target="$1" key="$2" val="$3"; shift 3
      local tpath; tpath="$(resolve_path "$proj" "$target")"
      ( cd "$proj" && "$NUB" config set "$@" "$key" "$val" ) >>"$log" 2>&1
      local in_target=0 leaked="" other opath
      [ -f "$tpath" ] && grep -qF "$val" "$tpath" && in_target=1
      for other in .npmrc pnpm-workspace.yaml package.json nub.jsonc "~/.npmrc" "~/.config/pnpm/config.yaml" "~/.config/nub/nub.jsonc"; do
        opath="$(resolve_path "$proj" "$other")"
        [ "$opath" = "$tpath" ] && continue
        [ -f "$opath" ] && grep -qF "$val" "$opath" && leaked="$other"
      done
      if [ "$in_target" = 1 ] && [ -z "$leaked" ]; then pass "$id" "wrote $key → $target only"
      elif [ "$in_target" != 1 ]; then fail "$id" "$key NOT in $target${leaked:+ (landed in $leaked)}"; sed 's/^/      | /' "$log"
      else fail "$id" "$key also leaked into $leaked (wrong home)"; fi ;;
    env-bridge-resolver)   # unreachable-registry-url  honored|ignored
      local url="$1" want="$2"
      if [ "$REF" != 1 ]; then skip "$id" "REF=1 to run the resolver probe"; return; fi
      ( cd "$proj" && env "npm_config_registry=$url" "$NUB" install ) >>"$log" 2>&1; local c=$?
      # A real attempt, not a startup mention: the host must share a line with a
      # fetch/resolve/DNS-failure token (an unreachable .invalid host forces an
      # error naming it).
      local attempted=0
      grep -iE "frontdoor-envbridge\.invalid" "$log" \
        | grep -qiE "fetch|resolv|resolu|request|ENOTFOUND|EAI_AGAIN|getaddrinfo|dns|connect|GET |https?://" && attempted=1
      case "$want" in
        honored)
          [ "$attempted" = 1 ] && pass "$id" "resolver ATTEMPTED npm_config_registry host ($url)" \
            || { fail "$id" "npm_config_registry host not reached by a resolve/fetch attempt"; sed 's/^/      | /' "$log"; } ;;
        ignored)
          # exit 0 is the witness that the install fetched from somewhere else;
          # a failed install proves nothing about which registry it tried.
          if [ "$attempted" = 1 ]; then fail "$id" "resolver used npm_config_registry, which this identity must ignore"; sed 's/^/      | /' "$log"
          elif [ "$c" != 0 ]; then fail "$id" "install exit=$c — cannot tell which registry it used"; sed 's/^/      | /' "$log"
          else pass "$id" "install ignored npm_config_registry and succeeded"; fi ;;
        *) fail "$id" "env-bridge-resolver: want must be honored|ignored, got '$want'" ;;
      esac ;;
    env-gate)          # key  ENV_VAR  value  honored|ignored
      # Whether a config env variable is read is the identity's call, so each
      # `ignored` row has an `honored` twin differing only in the fixture — a
      # variable nothing reads would pass `ignored` however broken the gate, and
      # the twin is what keeps the variable (or at least the read path) live. The
      # key must also be one `config get` surfaces, or neither row observes anything.
      local key="$1" envvar="$2" val="$3" want="$4"
      local base; base="$( cd "$proj" && "$NUB" config get "$key" 2>>"$log" )"
      local out;  out="$(  cd "$proj" && env "$envvar=$val" "$NUB" config get "$key" 2>>"$log" )"
      case "$want" in
        honored)
          [ "$out" = "$val" ] \
            && pass "$id" "$envvar honored ($key=$out)" \
            || fail "$id" "$envvar NOT honored: config get $key => '$out' (want '$val') — nothing reads it, so its ignored twin proves nothing" ;;
        ignored)
          if [ -z "$base" ] || [ "$base" = "undefined" ]; then
            fail "$id" "config get $key surfaced no value — cell can't observe the gate (vacuous)"
          elif [ "$out" != "$base" ]; then
            fail "$id" "$envvar was READ here, which this identity must not do: $key moved '$base' → '$out'"
          else
            pass "$id" "$envvar ignored ($key stayed '$out', not the seeded '$val')"
          fi ;;
        *) fail "$id" "env-gate: want must be honored|ignored, got '$want'" ;;
      esac ;;
    foreign-lockfile)  # file  hint|quiet
      # npm, Yarn and Bun confer no identity: their lockfile is left unread and
      # untouched, and a nub project says once that `nub pm migrate` carries it
      # across. A pnpm project stays silent, as pnpm 12.4.1 does. Each branch
      # also demands the lockfile its identity writes, so the absence of a hint
      # is only accepted from an install that demonstrably ran.
      local file="$1" want="$2" src
      if [ ! -f "$proj/$file" ]; then
        case "$file" in package-lock.json) src=npm ;; yarn.lock) src=yarn ;; bun.lock) src=bun ;; *) src="" ;; esac
        [ -n "$src" ] && [ -f "$FIXTURES/$src/$file" ] || { fail "$id" "foreign-lockfile: no fixture carries $file"; return; }
        cp "$FIXTURES/$src/$file" "$proj/$file"
      fi
      cp "$proj/$file" "$SANDBOX/logs/$id.orig"
      ( cd "$proj" && "$NUB" install --offline ) >"$log" 2>&1; local c=$?
      local hint=0; grep -F "$file" "$log" | grep -qF "nub pm migrate" && hint=1
      local problems=""
      [ "$c" = 0 ] || problems="$problems install exit=$c;"
      cmp -s "$proj/$file" "$SANDBOX/logs/$id.orig" || problems="$problems $file was rewritten;"
      case "$want" in
        hint)
          [ "$hint" = 1 ] || problems="$problems no \`nub pm migrate\` hint naming $file;"
          [ -f "$proj/nub.lock" ] || problems="$problems no nub.lock written;"
          [ -f "$proj/pnpm-lock.yaml" ] && problems="$problems wrote pnpm-lock.yaml in a nub project;" ;;
        quiet)
          grep -qF "pm migrate" "$log" && problems="$problems printed nub's migrate hint in a pnpm project;"
          [ -f "$proj/pnpm-lock.yaml" ] || problems="$problems no pnpm-lock.yaml written;"
          [ -f "$proj/nub.lock" ] && problems="$problems wrote nub.lock in a pnpm project;" ;;
        *) problems="foreign-lockfile: want must be hint|quiet, got '$want'" ;;
      esac
      [ -z "$problems" ] && pass "$id" "$file unread and untouched ($want)" \
        || { fail "$id" "${problems# }"; sed 's/^/      | /' "$log"; } ;;
    *) fail "$id" "unknown assert verb '$verb'" ;;
  esac
}

want_id() {
  [ ${#ONLY[@]} -eq 0 ] && return 0
  local id="$1" surface="$2" x
  for x in "${ONLY[@]}"; do { [ "$x" = "$id" ] || [ "$x" = "$surface" ]; } && return 0; done
  return 1
}

while IFS=$'\t' read -r id fixture surface mode verb rest; do
  [ -z "$id" ] && continue
  case "$id" in \#*) continue ;; esac
  want_id "$id" "$surface" || continue
  # shellcheck disable=SC2086
  run_cell "$id" "$fixture" "$surface" "$mode" "$verb" $rest
done < <(grep -vE '^[[:space:]]*(#|$)' "$HERE/assertions.tsv")

echo
echo "== results =="
printf '%-38s %-11s %s\n' "id" "status" "detail"
for row in ${RESULTS[@]+"${RESULTS[@]}"}; do IFS='|' read -r i s d <<<"$row"; printf '%-38s %-11s %s\n' "$i" "$s" "$d"; done
PASSES=$(( ${#RESULTS[@]} - FAILS - SKIPS - XFAILS ))
echo
if [ "$FAILS" -gt 0 ]; then
  echo "RESULT: FAIL ($FAILS fail, $PASSES pass, $XFAILS expected-fail, $SKIPS skip)"; echo "sandbox kept: $SANDBOX"; exit 1
fi
echo "RESULT: OK ($PASSES pass, $XFAILS expected-fail, $SKIPS skip)"
[ "${KEEP:-0}" = 1 ] && echo "sandbox kept: $SANDBOX" || rm -rf "$SANDBOX"
