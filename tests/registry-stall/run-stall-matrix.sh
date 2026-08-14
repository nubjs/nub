#!/usr/bin/env bash
# Drives a real nub binary against a registry that never answers, and asserts
# on how long each configuration takes to give up. Elapsed time IS the
# assertion here: every case pins a different bound, so a bound that silently
# stops being read shows up as the wrong number rather than as a pass.
#
# Usage: run-stall-matrix.sh [path-to-nub] [port]
#
# Runs all five cases and exits non-zero if any of them missed its window.
# Deliberately not fail-fast: the whole matrix is about three minutes, and
# seeing which bounds moved together is what tells you where a regression is.
set -uo pipefail

NUB="${1:-target/fast/nub}"
PORT="${2:-4999}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nub-stall-matrix.XXXXXX")"
SERVER_PID=""
FAILURES=0

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

if [ ! -x "$NUB" ]; then
  echo "no nub binary at $NUB — build one first:" >&2
  echo "  scripts/rust-build.sh build -p nub-cli --profile fast" >&2
  echo "  then pass \"\$(scripts/rust-build.sh --print-target)/fast/nub\"" >&2
  exit 2
fi

# A binary that predates the stall bound is the failure mode this harness is
# most likely to hit in practice, because the wrapper's target dir is a
# content-hashed bucket that moves when a depended-on crate changes — so a
# stale artifact can sit at the expected path looking perfectly valid. It would
# not error; it would take ~300s on the first case and report a confusing
# out-of-window failure. Name it up front instead.
# `grep -c` rather than `grep -q`: under `pipefail`, a `-q` grep exits on the
# first match and SIGPIPEs `strings`, which fails the whole pipeline and makes
# this warn on a perfectly good binary. `-c` consumes all input; `|| true`
# absorbs grep's exit 1 when there is genuinely no match.
STALL_SYMBOLS=$(strings "$NUB" 2>/dev/null | grep -c fetchStallTimeout || true)
if [ "${STALL_SYMBOLS:-0}" -eq 0 ]; then
  echo "warning: $NUB has no fetchStallTimeout — it predates the stall bound." >&2
  echo "         Expect the first case to take ~300s (the old fetchTimeout) and fail." >&2
  echo "         Rebuild and re-check which artifact you are pointing at." >&2
fi

node "$HERE/stall-registry.mjs" --port "$PORT" --mode blackhole > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
sleep 1

# name | npmrc body | env prefix | low bound (s) | high bound (s) | expected "still waiting" lines
run_case() {
  local name="$1" npmrc="$2" envs="$3" lo="$4" hi="$5" want_lines="$6"
  local dir="$WORK/$name"
  mkdir -p "$dir"
  echo '{"name":"stall-case","private":true,"dependencies":{"react":"^19.0.0"}}' > "$dir/package.json"
  { echo "registry=http://127.0.0.1:$PORT/"; printf '%b\n' "$npmrc"; } > "$dir/.npmrc"

  # Scrub the ambient settings env. `env` outranks `project_npmrc` in the
  # precedence chain (aube-settings/src/values.rs), so a developer who happens
  # to export npm_config_registry or npm_config_fetch_stall_timeout would have
  # it silently beat the fixture's .npmrc and the case would measure something
  # other than what it claims. Each case's own env is applied after the -u
  # flags, so it still wins.
  local -a scrub=()
  local key
  while IFS='=' read -r key _; do
    case "$key" in
      npm_config_*|NPM_CONFIG_*|pnpm_config_*|PNPM_CONFIG_*|AUBE_*|NUB_*)
        scrub+=(-u "$key") ;;
      # The proxy family too: `NpmConfig` fills http_proxy from HTTPS_PROXY,
      # then HTTP_PROXY, then PROXY whenever the npmrc layer left it unset
      # (config/apply.rs), and the fixture npmrc deliberately sets none of
      # them. On a machine behind a corporate proxy every case would then be
      # routed away from the 127.0.0.1 stall server it is supposed to measure.
      HTTPS_PROXY|https_proxy|HTTP_PROXY|http_proxy|PROXY|proxy|NO_PROXY|no_proxy)
        scrub+=(-u "$key") ;;
    esac
  done < <(env)

  # An array rather than a word-split scalar. Not because an empty `$envs`
  # would produce a stray argument — unquoted, it expands to zero words, and
  # only the *quoted* spelling has that hazard. The reason is the opposite
  # end: unquoted, the content is subject to globbing and IFS splitting, so an
  # assignment whose value ever contained a space or a glob character would be
  # silently mangled. The array says exactly how many arguments there are.
  local -a case_env=()
  [ -n "$envs" ] && read -r -a case_env <<< "$envs"

  local start elapsed rc lines
  start=$(date +%s)
  ( cd "$dir" && env ${scrub[@]+"${scrub[@]}"} XDG_CACHE_HOME="$dir/cache" \
      ${case_env[@]+"${case_env[@]}"} timeout 700 "$NUB" install --lockfile-only ) \
      > "$dir/out.log" 2>&1
  rc=$?
  elapsed=$(( $(date +%s) - start ))
  lines=$(tr '\r' '\n' < "$dir/out.log" | grep -c "still waiting on")

  local verdict="ok"
  if [ "$elapsed" -lt "$lo" ] || [ "$elapsed" -gt "$hi" ]; then
    verdict="FAIL (wanted ${lo}-${hi}s)"
    FAILURES=$((FAILURES + 1))
  elif [ "$rc" -eq 0 ]; then
    verdict="FAIL (expected a non-zero exit; a stalled registry must not succeed)"
    FAILURES=$((FAILURES + 1))
  elif [ "$want_lines" = "some" ] && [ "$lines" -eq 0 ]; then
    verdict="FAIL (expected 'still waiting' output)"
    FAILURES=$((FAILURES + 1))
  elif [ "$want_lines" = "none" ] && [ "$lines" -ne 0 ]; then
    verdict="FAIL (expected silence, got $lines lines)"
    FAILURES=$((FAILURES + 1))
  fi
  printf '%-22s elapsed=%-5s rc=%-3s waiting-lines=%-3s %s\n' \
    "$name" "${elapsed}s" "$rc" "$lines" "$verdict"
}

echo "nub: $NUB"
echo

# The default idle bound. Without fetchStallTimeout this took the whole
# 300s fetchTimeout, which is what made #715 look like a permanent hang.
run_case "default-60s"        "fetch-retries=0"                                        "" 55  90  some
# Proves the .npmrc key reaches the client. Only a changed elapsed shows that.
run_case "npmrc-5s"           "fetch-retries=0\nfetch-stall-timeout=5000"               "" 3   15  none
# Same, via env. The AUBE_* spelling is inert under nub (env_prefix is None),
# so npm_config_* is the only env route and this is what guards it.
run_case "env-20s"            "fetch-retries=0"     "npm_config_fetch_stall_timeout=20000" 17  40  some
# 0 must DISABLE the idle bound and fall back to fetchTimeout alone.
run_case "disabled-falls-back" "fetch-retries=0\nfetch-stall-timeout=0\nfetch-timeout=30000" "" 27 55 some
# fetchWarnTimeoutMs=0 is documented as disabling the warning; it must silence
# the in-flight line too, without touching the bound.
run_case "warn-disabled"      "fetch-retries=0\nfetch-warn-timeout-ms=0"                "" 55  90  none

echo
if [ "$FAILURES" -ne 0 ]; then
  echo "$FAILURES case(s) failed"
  exit 1
fi
echo "all cases passed"
