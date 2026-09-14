#!/usr/bin/env bash
# Foreign-lockfile conversion harness — proves nub takes a project off npm, yarn
# or bun once, and that what it writes is a lockfile the receiving tool accepts.
#
# nub writes no npm, yarn or bun lockfile, so there is nothing to convert TO
# those formats. A project arriving with one has exactly two destinations:
#
#   → nub   `nub pm migrate` converts the foreign lockfile to nub.lock and
#           removes the source. nub must then frozen-install from it, and — since
#           nub.lock is pnpm v9 format — real pnpm must frozen-accept the same
#           bytes renamed into a pnpm-declaring copy. The rename judge is what
#           makes the format claim testable by something other than nub.
#   → pnpm  `nub pm use pnpm@<pin>` converts it to pnpm-lock.yaml and declares
#           pnpm. Real pnpm must frozen-install from it.
#
# Real pnpm is fetched through npx at PNPM_PIN so the judge is deterministic; the
# SOURCE package managers are driven off PATH, because the point is whatever
# lockfile a real project actually arrives carrying.
#
# Usage:  run.sh [<path-to-nub>] [fixture ...]
# Env:
#   SANDBOX_ROOT=<dir>    reuse/inspect the sandbox (implies KEEP)
#   KEEP=1                keep the sandbox on success
#   SKIP_YARN=1           skip yarn sources even if yarn is on PATH
#   SKIP_BUN=1            skip bun sources even if bun is on PATH
#   PNPM_PIN=<ver>        the pnpm nub embeds; the judge and the `pm use` pin
#   TARGETS="nub pnpm"    subset of targets to run
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

NUB_VERSION="$("$NUB" --version 2>/dev/null || echo '?')"
PNPM_PIN="${PNPM_PIN:-12.4.1}"

ALL_FIXTURES=(simple peers empty-root-importer)
FIXTURES=("$@")
[ ${#FIXTURES[@]} -gt 0 ] || FIXTURES=("${ALL_FIXTURES[@]}")
read -r -a TARGET_LIST <<<"${TARGETS:-nub pnpm}"

# The sources are the three FOREIGN formats. pnpm is not one of them: a
# pnpm-lock.yaml is already the format nub.lock is, so `pm migrate` refuses it
# outright ("there is nothing to migrate") and the pnpm-to-nub hand-over is
# `pm use nub`, which tests/lockfile-conformance/ owns.
HAVE_NPM=0;  command -v npm  >/dev/null 2>&1 && HAVE_NPM=1
HAVE_YARN=0; command -v yarn >/dev/null 2>&1 && [ "${SKIP_YARN:-0}" != "1" ] && HAVE_YARN=1
HAVE_BUN=0;  command -v bun  >/dev/null 2>&1 && [ "${SKIP_BUN:-0}"  != "1" ] && HAVE_BUN=1
NPM_VERSION="$(npm  --version 2>/dev/null || echo MISSING)"
YARN_VERSION="$(yarn --version 2>/dev/null || echo MISSING)"
BUN_VERSION="$(bun  --version 2>/dev/null || echo MISSING)"

# Hermetic sandbox.
CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-conversion.XXXXXX")"
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
# npm's audit endpoint is a separate service with its own outages and nothing to
# say about a lockfile; a degraded one stalled every npm case for 100-300 s.
export npm_config_audit=false npm_config_fund=false npm_config_update_notifier=false

run_pnpm() { npx -y "pnpm@$PNPM_PIN" "$@"; }

echo "=== nub foreign-lockfile conversion ==="
echo "nub:      $NUB ($NUB_VERSION)"
echo "node:     $(node --version)"
echo "pnpm:     $PNPM_PIN (pinned via npx — judge and pm-use pin)"
echo "npm:      $NPM_VERSION  (HAVE=$HAVE_NPM)"
echo "yarn:     $YARN_VERSION  (HAVE=$HAVE_YARN)"
echo "bun:      $BUN_VERSION  (HAVE=$HAVE_BUN)"
echo "sandbox:  $SANDBOX_ROOT"
echo ""

step() {
  local log="$1" label="$2"; shift 2
  { echo; echo "### $label"; echo "### \$ $*"; } >>"$log"
  "$@" >>"$log" 2>&1
}

wipe_node_modules() {
  find "$1" -name node_modules -type d -prune -exec rm -rf {} +
}

# A `pnpm-workspace.yaml` is one of the markers that makes a project
# pnpm-incumbent, so a fixture carrying one is a pnpm project and `pm migrate`
# correctly targets pnpm's format there. The nub destination stages without it;
# the workspace still resolves, from the neutral `workspaces` field.
stage_fixture() {
  local fixture="$1" proj="$2" target="$3"
  rm -rf "$proj"
  mkdir -p "$proj"
  cp -R "$HERE/fixtures/$fixture/." "$proj/"
  [ "$target" = nub ] && rm -f "$proj/pnpm-workspace.yaml"
  return 0
}

# assert_node_modules <proj> <log> — every direct dep of every package.json in
# the project must exist, so a workspace fixture is checked member by member.
assert_node_modules() {
  local proj="$1" log="$2" failed=0 manifest dir dep
  while IFS= read -r manifest; do
    dir="$(dirname "$manifest")"
    while IFS= read -r dep; do
      [ -z "$dep" ] && continue
      [ -d "$dir/node_modules/$dep" ] || [ -d "$proj/node_modules/$dep" ] || {
        echo "FAILED: node_modules/$dep missing for $manifest" >>"$log"; failed=1
      }
    done < <(node -e '
      const p = require(process.argv[1]);
      Object.keys({ ...p.dependencies, ...p.devDependencies }).forEach((d) => console.log(d));
    ' "$manifest" 2>/dev/null)
  done < <(find "$proj" -name package.json -not -path '*/node_modules/*')
  return $failed
}

# write_source <pm> <proj> <log> — the lockfile the project arrives carrying.
write_source() {
  local pm="$1" proj="$2" log="$3" lockfile
  case "$pm" in
    npm)  ( cd "$proj" && step "$log" "npm install" npm install ) ; lockfile=package-lock.json ;;
    yarn) ( cd "$proj" && step "$log" "yarn install" yarn install ) ; lockfile=yarn.lock ;;
    bun)  ( cd "$proj" && step "$log" "bun install" bun install ) ; lockfile=bun.lock ;;
  esac || { echo "FAILED: $pm install failed" >>"$log"; return 1; }
  [ -f "$proj/$lockfile" ] || { echo "FAILED: $pm wrote no $lockfile" >>"$log"; return 1; }
  wipe_node_modules "$proj"
}

# to_nub — `nub pm migrate` converts the foreign lockfile once, nub frozen-reads
# it, and real pnpm frozen-accepts the same bytes renamed into a pnpm copy.
to_nub() {
  local proj="$1" log="$2"
  ( cd "$proj" && step "$log" "nub pm migrate" "$NUB" pm migrate ) \
    || { echo "FAILED: nub pm migrate failed" >>"$log"; return 1; }
  [ -f "$proj/nub.lock" ] || { echo "FAILED: pm migrate wrote no nub.lock" >>"$log"; return 1; }
  for stale in package-lock.json yarn.lock bun.lock pnpm-lock.yaml; do
    [ -e "$proj/$stale" ] && { echo "FAILED: pm migrate left $stale behind" >>"$log"; return 1; }
  done
  ( cd "$proj" && step "$log" "nub install --frozen-lockfile" "$NUB" install --frozen-lockfile ) \
    || { echo "FAILED: nub rejected the lockfile it just migrated" >>"$log"; return 1; }
  assert_node_modules "$proj" "$log" || return 1

  # nub.lock is pnpm v9 format, so real pnpm is the judge of that claim: the same
  # bytes under pnpm's name, in a copy that declares pnpm, must frozen-install.
  local judge="$proj.judge"
  rm -rf "$judge"; mkdir -p "$judge"
  cp -R "$proj/." "$judge/"
  wipe_node_modules "$judge"
  mv "$judge/nub.lock" "$judge/pnpm-lock.yaml"
  # The judge copy declares NO package manager. A manifest that pins pnpm makes
  # pnpm demand its own env lockfile document, which nub.lock legitimately does
  # not carry — nub manages no package-manager versions — and pnpm then refuses
  # the frozen install with ERR_PNPM_FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE
  # before it ever reads the project graph. Real pnpm needs no declaration to
  # install; the lockfile under its own name is the whole input being judged.
  ( cd "$judge" && node -e '
    const fs = require("fs");
    const manifest = JSON.parse(fs.readFileSync("package.json", "utf8"));
    delete manifest.packageManager;
    delete manifest.devEngines;
    fs.writeFileSync("package.json", JSON.stringify(manifest, null, 2) + "\n");
  ' )
  ( cd "$judge" && step "$log" "real pnpm frozen-accepts nub.lock renamed" run_pnpm install --frozen-lockfile ) \
    || { echo "FAILED: real pnpm rejected nub.lock renamed to pnpm-lock.yaml" >>"$log"; return 1; }
  assert_node_modules "$judge" "$log"
}

# to_pnpm — `nub pm use pnpm@<pin>` hands the project to pnpm, which must
# frozen-install from what nub wrote.
to_pnpm() {
  local proj="$1" log="$2"
  ( cd "$proj" && step "$log" "nub pm use pnpm@$PNPM_PIN" "$NUB" pm use "pnpm@$PNPM_PIN" ) \
    || { echo "FAILED: nub pm use pnpm failed" >>"$log"; return 1; }
  [ -f "$proj/pnpm-lock.yaml" ] || { echo "FAILED: pm use pnpm wrote no pnpm-lock.yaml" >>"$log"; return 1; }
  for stale in package-lock.json yarn.lock bun.lock nub.lock; do
    [ -e "$proj/$stale" ] && { echo "FAILED: pm use pnpm left $stale behind" >>"$log"; return 1; }
  done
  ( cd "$proj" && step "$log" "real pnpm frozen accept" run_pnpm install --frozen-lockfile ) \
    || { echo "FAILED: real pnpm rejected nub's converted lockfile" >>"$log"; return 1; }
  assert_node_modules "$proj" "$log"
}

# expected_reason <fixture> <source> <target>
expected_reason() {
  awk -v f="$1" -v s="$2" -v t="$3" \
    '$1==f && $2==s && $3==t { $1=""; $2=""; $3=""; sub(/^  */,""); print; exit }' \
    "$HERE/expected-failures.txt" 2>/dev/null
}

RESULTS=()
FAILS=0
XPASSES=0

for fixture in "${FIXTURES[@]}"; do
  [ -d "$HERE/fixtures/$fixture" ] || { echo "error: unknown fixture '$fixture'" >&2; exit 2; }

  declare -a sources=()
  [ "$HAVE_NPM"  -eq 1 ] && sources+=(npm)
  [ "$HAVE_YARN" -eq 1 ] && sources+=(yarn)
  [ "$HAVE_BUN"  -eq 1 ] && sources+=(bun)

  for source in "${sources[@]}"; do
    for target in "${TARGET_LIST[@]}"; do
      label="$fixture: $source → $target"
      echo "--- $label"
      proj="$SANDBOX_ROOT/runs/$fixture--$source--$target"
      log="$SANDBOX_ROOT/logs/$fixture--$source--$target.log"
      : >"$log"
      stage_fixture "$fixture" "$proj" "$target"

      ok=0
      if write_source "$source" "$proj" "$log"; then
        case "$target" in
          nub)  to_nub  "$proj" "$log" || ok=$? ;;
          pnpm) to_pnpm "$proj" "$log" || ok=$? ;;
        esac
      else
        ok=1
      fi

      reason="$(expected_reason "$fixture" "$source" "$target")"
      if [ "$ok" -eq 0 ] && [ -z "$reason" ]; then
        echo "    PASS"
        RESULTS+=("$fixture|$source → $target|PASS")
      elif [ "$ok" -eq 0 ] && [ -n "$reason" ]; then
        echo "    XPASS-STALE: now passes — remove from expected-failures.txt: $reason"
        XPASSES=$((XPASSES + 1))
        RESULTS+=("$fixture|$source → $target|XPASS-STALE")
      elif [ -n "$reason" ]; then
        echo "    expected red: $reason"
        RESULTS+=("$fixture|$source → $target|RED (expected)")
      else
        FAILS=$((FAILS + 1))
        echo "    FAIL — log: $log"
        tail -n 20 "$log" | sed 's/^/    | /'
        RESULTS+=("$fixture|$source → $target|FAIL")
      fi
    done
  done
done

[ "$HAVE_NPM"  -eq 0 ] && echo "NOTE: npm not on PATH — npm sources skipped"
[ "$HAVE_YARN" -eq 0 ] && echo "NOTE: yarn not on PATH (or SKIP_YARN=1) — yarn sources skipped"
[ "$HAVE_BUN"  -eq 0 ] && echo "NOTE: bun not on PATH (or SKIP_BUN=1) — bun sources skipped"

echo ""
echo "=== results ==="
printf '%-22s %-16s %s\n' "fixture" "conversion" "result"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r f c s <<<"$row"
  printf '%-22s %-16s %s\n' "$f" "$c" "$s"
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
