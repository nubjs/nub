#!/usr/bin/env bash
# Drives a real nub binary against a local registry that stalls, and asserts
# on how the package manager's request bounds end each stall. The bounds are
# time-shaped, so the assertions are too: how many times the stalled URL was
# requested, and how long from the first of those requests to the last socket
# closing. Both are read from the registry's own log, so the time a loaded
# machine spends starting the process is not part of the measurement.
#
# Usage: run-stall-matrix.sh [path-to-nub] [base-port]
#
#   STALL_ONLY=a,b                 run only the named cases
#   STALL_OMIT=key,key             drop these settings from every case
#   STALL_SET=key=value,key=value  force these settings into every case
#   STALL_TRICKLE_MS=n             override the trickle interval
#
# STALL_OMIT and STALL_SET are the break controls: removing the setting a case
# pins has to move it out of its window, or the case is not testing it.
#
# Cases run in parallel, each with its own registry and port, and the matrix
# exits non-zero if any case fails or if an expected failure starts passing.
set -uo pipefail

NUB="${1:-target/fast/nub}"
BASE_PORT="${2:-4990}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nub-stall-matrix.XXXXXX")"
PIDS=()

cleanup() {
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do kill "$pid" 2>/dev/null; done
  rm -rf "$WORK"
}
trap cleanup EXIT

if [ ! -x "$NUB" ]; then
  echo "no nub binary at $NUB — build one first:" >&2
  echo "  scripts/rust-build.sh build -p nub-cli --profile fast" >&2
  echo "  then pass \"\$(scripts/rust-build.sh --print-target)/fast/nub\"" >&2
  exit 2
fi
NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")"

# Every ambient package-manager setting is dropped: an exported
# npm_config_fetch_timeout, or a user .npmrc reached through HOME or
# npm_config_userconfig, outranks the fixture and would silently change what a
# case measures. Proxy variables go too, or a machine behind a proxy would
# route the fixture away from 127.0.0.1.
SCRUB=()
while IFS='=' read -r key _; do
  case "$key" in
    npm_config_*|NPM_CONFIG_*|pnpm_config_*|PNPM_*|NUB_*) SCRUB+=(-u "$key") ;;
    HTTPS_PROXY|https_proxy|HTTP_PROXY|http_proxy|ALL_PROXY|all_proxy|PROXY|proxy|NO_PROXY|no_proxy)
      SCRUB+=(-u "$key") ;;
  esac
done < <(env)

camel() { awk -F- '{ out = $1; for (i = 2; i <= NF; i++) out = out toupper(substr($i, 1, 1)) substr($i, 2); print out }' <<< "$1"; }

# Applies STALL_OMIT and STALL_SET to one case's kebab-case settings.
effective_settings() {
  local settings="$1" out=() item key forced omit
  IFS=',' read -r -a omit <<< "${STALL_OMIT:-}"
  IFS=',' read -r -a forced <<< "${STALL_SET:-}"
  for item in $settings; do
    key="${item%%=*}"
    [[ " ${omit[*]+${omit[*]}} " == *" $key "* ]] && continue
    [[ ",${STALL_SET:-}," == *",$key="* ]] && continue
    out+=("$item")
  done
  for item in ${forced[@]+"${forced[@]}"}; do out+=("$item"); done
  echo "${out[*]+${out[*]}}"
}

# name | project (nub, pnpm) | shape | settings | env setting | requests | span lo | span hi | cap | expect
run_case() {
  local name="$1" project="$2" shape="$3" settings="$4" env_setting="$5" want_reqs="$6" lo="$7" hi="$8" cap="$9" expect="${10}"
  local port="${11}" dir="$WORK/$name"
  mkdir -p "$dir/project" "$dir/home" || return
  settings="$(effective_settings "$settings")"
  local -a case_env=()
  if [ -n "$env_setting" ]; then
    local env_key="${env_setting%%=*}"
    if [[ ",${STALL_OMIT:-}," != *",$env_key,"* ]]; then
      case_env=("npm_config_${env_key//-/_}=${env_setting#*=}")
    fi
  fi

  node "$HERE/stall-registry.mjs" --port "$port" --shape "$shape" \
    --trickle-ms "${STALL_TRICKLE_MS:-1000}" > "$dir/server.log" 2>&1 &
  local server=$!
  local tries=0
  until grep -q "stall registry on" "$dir/server.log" 2>/dev/null; do
    tries=$((tries + 1)); [ "$tries" -gt 100 ] && break; sleep 0.1
  done

  echo "registry=http://127.0.0.1:$port/" > "$dir/project/.npmrc"
  local item
  if [ "$project" = pnpm ]; then
    echo '{"name":"stall-case","private":true,"packageManager":"pnpm@12.4.1","dependencies":{"stall-probe":"1.0.0"}}' > "$dir/project/package.json"
    : > "$dir/project/pnpm-workspace.yaml"
    for item in $settings; do echo "$(camel "${item%%=*}"): ${item#*=}" >> "$dir/project/pnpm-workspace.yaml"; done
  else
    echo '{"name":"stall-case","private":true,"dependencies":{"stall-probe":"1.0.0"}}' > "$dir/project/package.json"
    for item in $settings; do echo "$item" >> "$dir/project/.npmrc"; done
  fi

  local start rc
  start=$(date +%s)
  ( cd "$dir/project" && env ${SCRUB[@]+"${SCRUB[@]}"} HOME="$dir/home" \
      XDG_CACHE_HOME="$dir/home/.cache" XDG_DATA_HOME="$dir/home/.local/share" \
      XDG_CONFIG_HOME="$dir/home/.config" XDG_STATE_HOME="$dir/home/.local/state" \
      ${case_env[@]+"${case_env[@]}"} timeout "$cap" "$NUB" install ) > "$dir/out.log" 2>&1
  rc=$?
  local elapsed=$(( $(date +%s) - start ))
  kill "$server" 2>/dev/null

  local reqs span
  reqs=$(grep -c " STALL " "$dir/server.log")
  span=$(awk '
    / REQ .* STALL / { id = $3; t = substr($1, 2) + 0; if (first == "") first = t; stalled[id] = 1 }
    / CLOSED / { if (stalled[$3]) last = substr($1, 2) + 0 }
    END { if (first != "" && last != "") printf "%.1f", last - first; else print "-" }' "$dir/server.log")

  local problem=""
  if [ "$rc" -eq 124 ]; then
    problem="unbounded: still running when the ${cap}s cap killed it"
  elif [ "$rc" -eq 0 ]; then
    problem="exit 0: a stalled registry must not produce a successful install"
  elif [ "$reqs" -ne "$want_reqs" ]; then
    problem="$reqs request(s) to the stalled URL, wanted $want_reqs"
  elif [ "$span" = "-" ] || awk -v s="$span" -v lo="$lo" -v hi="$hi" 'BEGIN { exit !(s < lo || s > hi) }'; then
    problem="stall lasted ${span}s, wanted ${lo}-${hi}s"
  fi

  local verdict
  case "$expect:$problem" in
    pass:) verdict="ok" ;;
    pass:*) verdict="FAIL ($problem)" ;;
    xfail:) verdict="XPASS (expected failure now passes; make this case a pass)" ;;
    xfail:*) verdict="xfail ($problem)" ;;
  esac
  printf '%-26s span=%-7s elapsed=%-6s rc=%-4s requests=%-3s %s\n' \
    "$name" "${span}s" "${elapsed}s" "$rc" "$reqs" "$verdict" > "$dir/result"
  if [ "$verdict" != "ok" ] && [ "${verdict%% *}" != "xfail" ]; then
    { echo "--- $name: nub output"; tail -15 "$dir/out.log"; echo "--- $name: registry log"; cat "$dir/server.log"; } > "$dir/detail"
  fi
}

NAMES=()
case_def() {
  local name="$1"
  if [ -n "${STALL_ONLY:-}" ] && [[ ",$STALL_ONLY," != *",$name,"* ]]; then return; fi
  NAMES+=("$name")
  run_case "$@" "$((BASE_PORT + ${#NAMES[@]}))" &
  PIDS+=($!)
}

echo "nub: $NUB"
echo

# Each request may make no progress for fetch-timeout. Waiting for a response
# head and waiting inside a body are separate code paths behind the same bound.
case_def fetch-timeout          nub  meta-silent       "fetch-timeout=4000 fetch-retries=0" "" 1 3 12 120 pass
case_def fetch-timeout-body     nub  meta-partial-body "fetch-timeout=4000 fetch-retries=0" "" 1 3 12 120 pass
case_def fetch-timeout-env      nub  meta-silent       "fetch-retries=0" "fetch-timeout=4000" 1 3 12 120 pass
# The default bound: 60s per attempt. Without it a stalled registry is a hang.
case_def default-fetch-timeout  nub  meta-silent       "fetch-retries=0" "" 1 50 80 150 pass
# A pnpm project reads the same bounds from pnpm-workspace.yaml, not .npmrc.
case_def pnpm-workspace-yaml    pnpm meta-silent       "fetch-timeout=4000 fetch-retries=0" "" 1 3 12 120 pass
# Retry count and backoff: attempts are retries + 1, and the wait before
# attempt n+1 is min(mintimeout * factor^n, maxtimeout). Each case picks values
# where dropping its own setting moves the result by several seconds.
case_def fetch-retries          nub  meta-silent       "fetch-timeout=3000 fetch-retries=1 fetch-retry-mintimeout=1000" "" 2 6 12 120 pass
case_def fetch-retry-factor     nub  meta-silent       "fetch-timeout=3000 fetch-retries=2 fetch-retry-mintimeout=1000 fetch-retry-factor=3" "" 3 11 17 120 pass
case_def fetch-retry-maxtimeout nub  meta-silent       "fetch-timeout=3000 fetch-retries=2 fetch-retry-mintimeout=1000 fetch-retry-factor=10 fetch-retry-maxtimeout=3000" "" 3 11 17 120 pass
# A tarball is requested under two budgets in sequence: the download started
# during resolution, then the install's own fetch once that one has failed.
case_def fetch-timeout-tarball  nub  tarball-partial-body "fetch-timeout=4000 fetch-retries=0" "" 2 7 20 180 pass
# Known defect: fetch-retries=0 still requests a stalled tarball twice, so every
# tarball stall costs double the configured budget (500s at the defaults).
case_def fetch-retries-tarball  nub  tarball-partial-body "fetch-timeout=4000 fetch-retries=0" "" 1 3 12 180 xfail
# Known gap: fetch-timeout restarts on every byte received, and nothing bounds a
# response that keeps trickling, so a registry sending one byte a second is never
# cut off. fetchMinSpeedKiBps only warns, and only after a download succeeds.
case_def trickle                nub  meta-trickle      "fetch-timeout=3000 fetch-retries=0" "" 1 2 12 30 xfail

wait

FAILURES=0
for name in ${NAMES[@]+"${NAMES[@]}"}; do
  cat "$WORK/$name/result"
  line=$(cat "$WORK/$name/result")
  case "$line" in *" FAIL "*|*" XPASS "*) FAILURES=$((FAILURES + 1)) ;; esac
done
for name in ${NAMES[@]+"${NAMES[@]}"}; do
  [ -f "$WORK/$name/detail" ] && { echo; cat "$WORK/$name/detail"; }
done

echo
if [ "$FAILURES" -ne 0 ]; then
  echo "$FAILURES case(s) failed"
  exit 1
fi
echo "all cases passed"
