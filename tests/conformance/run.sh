#!/usr/bin/env bash
# Drop-in conformance harness — nub and real pnpm 12 must agree on the same
# project, in both directions:
#
#   Direction A (nub READS pnpm's lockfile): real pnpm writes pnpm-lock.yaml →
#     nub frozen-installs from it → node_modules carries every direct dep.
#
#   Direction B (pnpm READS nub's lockfile): nub writes pnpm-lock.yaml → real
#     pnpm frozen-installs from it without rewriting a byte.
#
# Every fixture is staged as a PNPM project (`packageManager` pins the version
# below), so nub serves it under pnpm's own identity — which is the identity
# whose byte-for-byte parity direction A and B exist to prove. The round trip
# through NUB's identity — nub.lock, `pm use nub` / `pm use pnpm` — is a
# different contract and lives in tests/lockfile-conformance/.
#
# Real pnpm is pinned through npx rather than taken off PATH: a PATH pnpm is
# whatever the box happens to carry, and a different major has a different
# store layout and a different settings home, so it judges a different product.
#
# See README.md for the full loop and design rationale.
#
# Usage:  run.sh [<path-to-nub>] [fixture ...]
# Env:    SANDBOX_ROOT=<dir>    reuse/inspect the sandbox (implies KEEP)
#         KEEP=1                keep the sandbox on success
#         DIRECTIONS="A B"      subset of directions to run
#
# Exit: 0 = every leg passes or is an expected red;
#       1 = at least one unexpected FAIL or stale expected-failure entry.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NUB="${1:-}"
if [ -z "$NUB" ]; then
  for candidate in \
    "$(cd "$HERE/../.." && pwd)/target/release/nub" \
    "$(cd "$HERE/../.." && pwd)/target/debug/nub"; do
    [ -x "$candidate" ] && { NUB="$candidate"; break; }
  done
fi
shift 2>/dev/null || true
NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")"
[ -x "$NUB" ] || { echo "error: nub binary not found/executable: $NUB" >&2; exit 2; }

PNPM_PIN="${PNPM_PIN:-12.4.1}"
NUB_VERSION="$("$NUB" --version 2>/dev/null || echo '?')"

# Fixture list — each is a subdirectory of fixtures/
ALL_FIXTURES=(simple peers scoped optional-deps alias file-dep peer-meta deep-graph postinstall overrides-ref overrides-nested patched-deps patched-deps-no-newline catalog workspace workspace-dedup empty-root-importer git-dep platform-optional dist-tag-spec range-forms alias-scoped injected-deps)
FIXTURES=("$@")
[ ${#FIXTURES[@]} -gt 0 ] || FIXTURES=("${ALL_FIXTURES[@]}")
read -r -a DIRECTION_LIST <<<"${DIRECTIONS:-A B}"

# Hermetic sandbox: redirect HOME + XDG so no dev-box config leaks in or out.
CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-conformance.XXXXXX")"
  CREATED_SANDBOX=1
fi
mkdir -p "$SANDBOX_ROOT/home" "$SANDBOX_ROOT/runs" "$SANDBOX_ROOT/logs"
export HOME="$SANDBOX_ROOT/home"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

# Settings a runner or a developer exported must not steer these installs.
for var in $(env | grep -oE '^(npm_config_|NPM_CONFIG_|pnpm_config_|PNPM_)[A-Za-z0-9_]*' || true); do
  unset "$var"
done
# npm's audit report is a separate registry service with its own outages and no
# bearing on the lockfile: a degraded advisory endpoint stalled every case for
# 100-300 s (npm's fetch timeout) and timed the CI job out on every branch for an
# evening (2026-09-03), while the same fixture without it took 1 s. fund and the
# update notifier are the other two network side trips with nothing to say about
# the result. These reach npx, which is npm.
export npm_config_audit=false npm_config_fund=false npm_config_update_notifier=false

run_pnpm() { npx -y "pnpm@$PNPM_PIN" "$@"; }

echo "=== nub drop-in conformance ==="
echo "nub:      $NUB ($NUB_VERSION)"
echo "node:     $(node --version)"
echo "pnpm:     $PNPM_PIN (pinned via npx)"
echo "sandbox:  $SANDBOX_ROOT"
echo ""

# step <log> <label> <cmd...> — append output to log, return exit code.
# Must be called from inside a ( cd "$proj" && ... ) subshell.
step() {
  local log="$1" label="$2"; shift 2
  { echo; echo "### $label"; echo "### \$ $*"; } >>"$log"
  "$@" >>"$log" 2>&1
}

wipe_node_modules() {
  find "$1" -name node_modules -type d -prune -exec rm -rf {} +
}

# Both sides must serve the project under pnpm's identity, so the manifest pins
# the pnpm version nub embeds: a pin naming any OTHER version is a different
# path entirely, where nub provisions that pnpm and delegates the command to it.
stage_fixture() {
  local fixture="$1" proj="$2"
  rm -rf "$proj"
  mkdir -p "$proj"
  cp -R "$HERE/fixtures/$fixture/." "$proj/"
  (cd "$proj" && node -e '
    const fs = require("fs");
    const manifest = JSON.parse(fs.readFileSync("package.json", "utf8"));
    manifest.packageManager = process.argv[1];
    fs.writeFileSync("package.json", JSON.stringify(manifest, null, 2) + "\n");
  ' "pnpm@$PNPM_PIN")
}

# assert_node_modules <proj> <log> — every direct dep from package.json must exist.
# Checks dependencies + devDependencies; skips optionalDependencies (platform-
# conditional) since their presence is legitimately OS-dependent.
assert_node_modules() {
  local proj="$1" log="$2"
  local pkg="$proj/package.json"
  local failed=0
  local deps
  deps=$(node -e "
    const p = require('$pkg');
    const all = Object.keys({...p.dependencies, ...p.devDependencies});
    all.forEach(d => console.log(d));
  " 2>/dev/null) || { echo "FAILED: could not parse package.json" >>"$log"; return 1; }
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    if [ ! -d "$proj/node_modules/$dep" ]; then
      echo "FAILED: node_modules/$dep missing after frozen install" >>"$log"
      failed=1
    fi
  done <<< "$deps"
  return $failed
}

# skip_reason <fixture> <direction> — a permanent ecosystem-level impossibility,
# not a fixable nub bug. Empty output means "run it." These live here rather
# than in expected-failures.txt because no lockfile nub could write would ever
# make them pass.
skip_reason() {
  case "$1--$2" in
    # pnpm 12 writes a lockfile whose importer block omits `dependenciesMeta`,
    # then on frozen-verify demands it back and self-rejects. nub's lockfile is
    # byte-identical here, so no lockfile nub could write would frozen-pass.
    # Direction A — nub frozen-READS pnpm's injected-workspace lockfile and
    # materializes the dep — is the meaningful guard and runs.
    injected-deps--B) echo "pnpm self-rejects injected dependenciesMeta under --frozen-lockfile, from its own lockfile" ;;
  esac
}

# expected_reason <fixture> <direction> — look up a known-red entry.
# Lines in expected-failures.txt: "<fixture> <dir> <reason...>"
expected_reason() {
  awk -v f="$1" -v d="$2" \
    '$1==f && $2==d { $1=""; $2=""; sub(/^  */,""); print; exit }' \
    "$HERE/expected-failures.txt" 2>/dev/null
}

RESULTS=()
FAILS=0
XPASSES=0

# ── Direction A (pnpm → nub): real pnpm writes the lockfile, nub frozen-installs
dir_a() {
  local proj="$1" log="$2"
  ( cd "$proj" && step "$log" "pnpm install (write lockfile)" run_pnpm install --no-frozen-lockfile ) \
    || { echo "FAILED: pnpm install failed" >>"$log"; return 1; }
  [ -f "$proj/pnpm-lock.yaml" ] || { echo "FAILED: no pnpm-lock.yaml written" >>"$log"; return 1; }
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "nub install --frozen-lockfile" "$NUB" install --frozen-lockfile ) \
    || { echo "FAILED: nub install --frozen-lockfile failed" >>"$log"; return 1; }
  assert_node_modules "$proj" "$log"
}

# ── Direction B (nub → pnpm): nub writes the lockfile, real pnpm frozen-installs
dir_b() {
  local proj="$1" log="$2"
  ( cd "$proj" && step "$log" "nub install (write lockfile)" "$NUB" install ) \
    || { echo "FAILED: nub install failed" >>"$log"; return 1; }
  [ -f "$proj/pnpm-lock.yaml" ] || { echo "FAILED: nub wrote no pnpm-lock.yaml" >>"$log"; return 1; }
  cp "$proj/pnpm-lock.yaml" "$log.lock-before"
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "pnpm frozen accept" run_pnpm install --frozen-lockfile ) \
    || { echo "FAILED: pnpm rejected nub's lockfile (--frozen-lockfile)" >>"$log"; return 1; }
  cmp -s "$log.lock-before" "$proj/pnpm-lock.yaml" || {
    echo "FAILED: pnpm rewrote the lockfile after a frozen install (churn)" >>"$log"
    diff -u "$log.lock-before" "$proj/pnpm-lock.yaml" >>"$log" || true
    return 1
  }
  assert_node_modules "$proj" "$log"
}

for fixture in "${FIXTURES[@]}"; do
  [ -d "$HERE/fixtures/$fixture" ] || { echo "error: unknown fixture '$fixture'" >&2; exit 2; }

  for direction in "${DIRECTION_LIST[@]}"; do
    label="$fixture × dir-$direction"
    echo "--- $label"

    skip="$(skip_reason "$fixture" "$direction")"
    if [ -n "$skip" ]; then
      echo "    skip (by design): $skip"
      RESULTS+=("$fixture|dir-$direction|SKIP (by design)")
      continue
    fi

    proj="$SANDBOX_ROOT/runs/$fixture--$direction"
    log="$SANDBOX_ROOT/logs/$fixture--$direction.log"
    : >"$log"
    stage_fixture "$fixture" "$proj"

    ok=0
    if [ "$direction" = "A" ]; then
      dir_a "$proj" "$log" || ok=$?
    else
      dir_b "$proj" "$log" || ok=$?
    fi

    reason="$(expected_reason "$fixture" "$direction")"
    if [ "$ok" -eq 0 ] && [ -z "$reason" ]; then
      echo "    PASS"
      RESULTS+=("$fixture|dir-$direction|PASS")
    elif [ "$ok" -eq 0 ] && [ -n "$reason" ]; then
      echo "    XPASS-STALE: now passes — remove from expected-failures.txt: $reason"
      XPASSES=$((XPASSES + 1))
      RESULTS+=("$fixture|dir-$direction|XPASS-STALE")
    elif [ -n "$reason" ]; then
      echo "    expected red: $reason"
      RESULTS+=("$fixture|dir-$direction|RED (expected)")
    else
      FAILS=$((FAILS + 1))
      echo "    FAIL — log: $log"
      tail -n 20 "$log" | sed 's/^/    | /'
      RESULTS+=("$fixture|dir-$direction|FAIL")
    fi
  done
done

echo ""
echo "=== results ==="
printf '%-26s %-8s %s\n' "fixture" "dir" "result"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r f d s <<<"$row"
  printf '%-26s %-8s %s\n' "$f" "$d" "$s"
done
echo ""

if [ "$FAILS" -gt 0 ] || [ "$XPASSES" -gt 0 ]; then
  echo "RESULT: FAIL ($FAILS unexpected failure(s), $XPASSES stale expected-failure entry/entries)"
  echo "sandbox kept for forensics: $SANDBOX_ROOT"
  exit 1
fi

echo "RESULT: OK (expected reds, if any, are listed above and tracked in expected-failures.txt)"
if [ "$CREATED_SANDBOX" -eq 1 ] && [ "${KEEP:-0}" != "1" ]; then
  rm -rf "$SANDBOX_ROOT"
else
  echo "sandbox kept: $SANDBOX_ROOT"
fi
