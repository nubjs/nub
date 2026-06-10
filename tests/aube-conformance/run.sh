#!/usr/bin/env bash
# Lockfile conformance harness — nub's embedded aube engine writes each
# foreign lockfile format, and the REAL package manager is the judge.
# See README.md for the full loop; expected-failures.txt for the red list.
#
# Usage:  run.sh <path-to-nub> [fixture ...]
# Env:    FORMATS="pnpm npm bun yarn"   subset of legs to run
#         SANDBOX_ROOT=<dir>           reuse/inspect the sandbox (implies KEEP)
#         KEEP=1                       keep the sandbox on success
#
# Per-format gates (a scenario passes only if every step holds):
#   pnpm — nub writes pnpm-lock.yaml; real pnpm `install --frozen-lockfile`
#          accepts it; a follow-up real-pnpm mutable install leaves the
#          lockfile byte-identical (zero churn).
#   npm  — nub writes package-lock.json; real `npm ci` accepts; `npm install`
#          rewrite leaves it byte-identical.
#   bun  — nub writes bun.lock; real `bun install --frozen-lockfile` accepts.
#   yarn — nub must REFUSE to mutate a detected yarn.lock (write fidelity is
#          unproven, so the refusal IS the pass): non-zero exit, the error
#          names yarn.lock, the file is untouched, no other lockfile appears.
#   nub  — the two-mode round trip: drive the mutation in compat (pnpm) mode,
#          `nub pm use nub` (zero pnpm-named files, lock.yaml present, a
#          frozen nub-mode install works from it), `nub pm use pnpm@<pin>`
#          back, and the REAL pnpm judge accepts the regenerated state frozen.
#
# Every nub invocation's output is also swept for the string "aube" — the
# conformance projects double as brand-leak canaries (cheap local complement
# to tests/brand-sweep/run.sh).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ $# -lt 1 ]; then
  echo "usage: run.sh <path-to-nub> [fixture ...]" >&2
  exit 2
fi
NUB="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
# Windows (Git Bash): tolerate a path given without the .exe suffix.
{ [ -x "$NUB" ] || ! [ -x "$NUB.exe" ]; } || NUB="$NUB.exe"
[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }
shift

# Pinned judges. pnpm and npm are fetched per-run via npx into the sandbox
# HOME, so the pin is exact on every machine. bun has no npx channel; it
# comes from PATH and CI pins it via oven-sh/setup-bun (BUN_PIN below is the
# version CI installs — locally a different bun only produces a warning).
PNPM_PIN=10.15.1
NPM_PIN=11.13.0
BUN_PIN=1.3.14

ALL_FIXTURES=(simple workspace peer-heavy overrides platform-optional scoped git-dep patched)
FIXTURES=("$@")
[ ${#FIXTURES[@]} -gt 0 ] || FIXTURES=("${ALL_FIXTURES[@]}")
FORMATS="${FORMATS:-pnpm npm bun yarn nub}"

# Hermetic sandbox: absolute HOME + XDG so neither the dev box's ~/.npmrc,
# caches, or stores leak in, nor the run leaves residue behind. (Absolute
# paths are load-bearing — aube mis-handles relative XDG_DATA_HOME.)
# The mktemp template deliberately avoids the string "aube": nub may print
# sandbox paths, and the brand sweep below would false-positive on them.
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
# Windows (Git Bash): Node tools resolve os.homedir() from USERPROFILE, and
# npm roots its cache/userconfig in LOCALAPPDATA/APPDATA — HOME alone doesn't
# sandbox them on this OS, so point all three into the sandbox home too.
# (nub itself follows the XDG_* overrides above on every OS: those are plain
# env reads, checked before any platform known-folder fallback.)
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    mkdir -p "$HOME/AppData/Roaming" "$HOME/AppData/Local"
    USERPROFILE="$(cygpath -w "$HOME")"
    APPDATA="$(cygpath -w "$HOME/AppData/Roaming")"
    LOCALAPPDATA="$(cygpath -w "$HOME/AppData/Local")"
    export USERPROFILE APPDATA LOCALAPPDATA
    ;;
esac
# Format steering must come only from the per-leg env below.
unset npm_config_default_lockfile_format NPM_CONFIG_DEFAULT_LOCKFILE_FORMAT 2>/dev/null || true

run_pnpm() { npx -y "pnpm@$PNPM_PIN" "$@"; }
run_npm()  { npx -y "npm@$NPM_PIN" "$@"; }

echo "== nub lockfile conformance =="
echo "nub:     $NUB ($("$NUB" --version 2>/dev/null || echo '?'))"
echo "node:    $(node --version)"
echo "pnpm:    $PNPM_PIN (pinned via npx)"
echo "npm:     $NPM_PIN (pinned via npx)"
BUN_ACTUAL="$(bun --version 2>/dev/null || echo MISSING)"
echo "bun:     $BUN_ACTUAL (CI pin: $BUN_PIN)"
[ "$BUN_ACTUAL" = "$BUN_PIN" ] || echo "warning: local bun $BUN_ACTUAL != CI pin $BUN_PIN" >&2
echo "sandbox: $SANDBOX_ROOT"
echo

# The mutation each fixture drives through nub. peer-heavy is the adversarial
# reviewer's exact blocker repro (clean project, `nub add`), everything else
# is a fresh `nub install` against the committed manifest.
fixture_cmd() {
  case "$1" in
    peer-heavy) echo "add react-dom@18.3.1 chokidar@3.6.0 react-redux@9.2.0" ;;
    *) echo "install" ;;
  esac
}

# nub_mutation <log> <proj> <format> <fixture> — drive the fixture's nub
# mutation. `patched` is the only multi-step fixture: the full patch
# workflow (install → patch → edit → patch-commit, whose chained install
# is what must land the patch entry in the lockfile). Everything else is
# the single fixture_cmd verb.
nub_mutation() {
  local log="$1" proj="$2" format="$3" fixture="$4"
  if [ "$fixture" = patched ]; then
    local edit="$proj/.patch-edit"
    nub_step "$log" "$proj" "$format" install || return $?
    nub_step "$log" "$proj" "$format" patch ms@2.1.3 --edit-dir "$edit" || return $?
    printf '\nmodule.exports.NUB_PATCHED = true;\n' >>"$edit/user/index.js" || return 1
    nub_step "$log" "$proj" "$format" patch-commit "$edit/user" || return $?
  else
    # shellcheck disable=SC2046
    nub_step "$log" "$proj" "$format" $(fixture_cmd "$fixture")
  fi
}

# fixture_post_check <fixture> <proj> <log> — fixture-specific assertion on
# the tree the REAL package manager just linked. patched: the real PM must
# have applied the committed patch from the lockfile entry — an install that
# silently drops the patch is exactly the failure this fixture exists for.
fixture_post_check() {
  case "$1" in
    patched)
      grep -q NUB_PATCHED "$2/node_modules/ms/index.js" 2>/dev/null \
        || { echo "FAILED: real PM install did not apply the patch" >>"$3"; return 1; } ;;
  esac
  return 0
}

expected_reason() {
  # expected-failures.txt lines: "<fixture> <format> <reason...>"
  awk -v f="$1" -v m="$2" '$1==f && $2==m { $1=""; $2=""; sub(/^  */,""); print; exit }' \
    "$HERE/expected-failures.txt" 2>/dev/null
}

# Scenarios that can never pass because the ECOSYSTEM lacks the construct —
# permanently skipped by design, unlike expected-failures.txt entries which
# are fork-fixable and must shrink.
skip_reason() {
  case "$1--$2" in
    # Verified 2026-06-10 (npm 11.13.0): npm errors EUNSUPPORTEDPROTOCOL
    # parsing the member manifests themselves — no lockfile nub could write
    # changes that.
    workspace--npm) echo "npm has no workspace: protocol support" ;;
    # npm has no patched-dependency construct at all: package-lock.json
    # cannot carry a patch entry and `npm ci` will never apply one, so no
    # lockfile nub writes can make the post-check pass.
    patched--npm) echo "npm has no patched-dependency construct" ;;
    *) echo "" ;;
  esac
}

wipe_node_modules() {
  find "$1" -name node_modules -type d -prune -exec rm -rf {} +
}

# step <log> <label> <cmd...> — append the command's output to the log,
# return its exit code without tripping -e at the call site (callers use if).
step() {
  local log="$1" label="$2"; shift 2
  {
    echo
    echo "### $label"
    echo "### \$ $*"
  } >>"$log"
  "$@" >>"$log" 2>&1
}

# nub_step <log> <proj> <format> <cmd...> — run nub with the leg's lockfile
# format steering, then brand-sweep its captured output.
nub_step() {
  local log="$1" proj="$2" format="$3"; shift 3
  local out="$log.nub-out"
  {
    echo
    echo "### nub $*  (defaultLockfileFormat=$format)"
  } >>"$log"
  local code=0
  (cd "$proj" && npm_config_default_lockfile_format="$format" "$NUB" "$@") >"$out" 2>&1 || code=$?
  cat "$out" >>"$log"
  if grep -qi 'aube' "$out"; then
    echo "### BRAND LEAK: nub output contains 'aube'" >>"$log"
    return 99
  fi
  return $code
}

stage_fixture() {
  local fixture="$1" proj="$2"
  rm -rf "$proj"
  mkdir -p "$proj"
  cp -R "$HERE/fixtures/$fixture/." "$proj/"
}

leg_pnpm() {
  local proj="$1" log="$2" fixture="$3"
  nub_mutation "$log" "$proj" pnpm "$fixture" || { echo "FAILED: nub step (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/pnpm-lock.yaml" ] || { echo "FAILED: nub wrote no pnpm-lock.yaml" >>"$log"; return 1; }
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "real pnpm frozen accept" run_pnpm install --frozen-lockfile ) \
    || { echo "FAILED: real pnpm rejected the lockfile (--frozen-lockfile)" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  cp "$proj/pnpm-lock.yaml" "$log.lock-before"
  ( cd "$proj" && step "$log" "real pnpm zero-churn rewrite" run_pnpm install ) \
    || { echo "FAILED: real pnpm mutable install errored" >>"$log"; return 1; }
  cmp -s "$log.lock-before" "$proj/pnpm-lock.yaml" \
    || { echo "FAILED: real pnpm rewrote the lockfile (churn):" >>"$log"; diff -u "$log.lock-before" "$proj/pnpm-lock.yaml" >>"$log" || true; return 1; }
  return 0
}

leg_npm() {
  local proj="$1" log="$2" fixture="$3"
  nub_mutation "$log" "$proj" npm "$fixture" || { echo "FAILED: nub step (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/package-lock.json" ] || { echo "FAILED: nub wrote no package-lock.json" >>"$log"; return 1; }
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "real npm ci accept" run_npm ci ) \
    || { echo "FAILED: real npm ci rejected the lockfile" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  cp "$proj/package-lock.json" "$log.lock-before"
  ( cd "$proj" && step "$log" "real npm zero-churn rewrite" run_npm install --no-audit --no-fund ) \
    || { echo "FAILED: real npm mutable install errored" >>"$log"; return 1; }
  cmp -s "$log.lock-before" "$proj/package-lock.json" \
    || { echo "FAILED: real npm rewrote the lockfile (churn):" >>"$log"; diff -u "$log.lock-before" "$proj/package-lock.json" >>"$log" || true; return 1; }
  return 0
}

leg_bun() {
  local proj="$1" log="$2" fixture="$3"
  command -v bun >/dev/null || { echo "FAILED: bun not on PATH" >>"$log"; return 1; }
  nub_mutation "$log" "$proj" bun "$fixture" || { echo "FAILED: nub step (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/bun.lock" ] || { echo "FAILED: nub wrote no bun.lock" >>"$log"; return 1; }
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "real bun frozen accept" bun install --frozen-lockfile ) \
    || { echo "FAILED: real bun rejected the lockfile (--frozen-lockfile)" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  return 0
}

# The nub leg: the bidirectional identity switch over real resolution state.
# Compat-mode mutation first (pnpm artifacts), then `pm use nub` — the full
# switch — proving the nub-mode invariants; an actual frozen install from
# lock.yaml (the engine must resolve+link from the renamed lockfile and the
# migrated config homes); then `pm use pnpm@<pin>` reverses everything and
# the REAL pnpm is the judge of the regenerated state. The pin matters:
# pnpm errors on a packageManager naming a different major.
leg_nub() {
  local proj="$1" log="$2" fixture="$3"
  nub_mutation "$log" "$proj" pnpm "$fixture" || { echo "FAILED: nub step (exit $?)" >>"$log"; return 1; }
  nub_step "$log" "$proj" pnpm pm use nub || { echo "FAILED: pm use nub (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/lock.yaml" ] || { echo "FAILED: use nub left no lock.yaml" >>"$log"; return 1; }
  local pnpm_named
  pnpm_named=$(find "$proj" -name '*pnpm*' -not -path '*/node_modules/*' 2>/dev/null || true)
  [ -z "$pnpm_named" ] || { echo "FAILED: pnpm-named files survived use nub: $pnpm_named" >>"$log"; return 1; }
  wipe_node_modules "$proj"
  nub_step "$log" "$proj" pnpm install --frozen-lockfile \
    || { echo "FAILED: nub-mode frozen install from lock.yaml (exit $?)" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  nub_step "$log" "$proj" pnpm pm use "pnpm@$PNPM_PIN" || { echo "FAILED: pm use pnpm (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/pnpm-lock.yaml" ] && [ ! -f "$proj/lock.yaml" ] \
    || { echo "FAILED: use pnpm did not rename lock.yaml back" >>"$log"; return 1; }
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "real pnpm frozen accept (post round-trip)" run_pnpm install --frozen-lockfile ) \
    || { echo "FAILED: real pnpm rejected the post-round-trip state (--frozen-lockfile)" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  return 0
}

# The yarn pass is the refusal: seed a classic yarn.lock, attempt a mutation,
# and demand a clean refusal that leaves every byte alone.
leg_yarn() {
  local proj="$1" log="$2" fixture="$3"
  printf '# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n# yarn lockfile v1\n' >"$proj/yarn.lock"
  cp "$proj/yarn.lock" "$log.yarn-before"
  local code=0
  nub_step "$log" "$proj" pnpm add kleur@4.1.5 || code=$?
  [ "$code" -eq 99 ] && { echo "FAILED: brand leak in refusal output" >>"$log"; return 1; }
  [ "$code" -ne 0 ] || { echo "FAILED: nub add succeeded despite yarn.lock (gate did not fire)" >>"$log"; return 1; }
  grep -q 'yarn\.lock' "$log.nub-out" \
    || { echo "FAILED: refusal does not name yarn.lock (wrong error?)" >>"$log"; return 1; }
  cmp -s "$log.yarn-before" "$proj/yarn.lock" \
    || { echo "FAILED: yarn.lock was modified" >>"$log"; return 1; }
  for stray in pnpm-lock.yaml package-lock.json bun.lock; do
    [ ! -f "$proj/$stray" ] || { echo "FAILED: stray $stray written during refusal" >>"$log"; return 1; }
  done
  return 0
}

RESULTS=()
FAILS=0
XPASSES=0

for fixture in "${FIXTURES[@]}"; do
  [ -d "$HERE/fixtures/$fixture" ] || { echo "error: unknown fixture '$fixture'" >&2; exit 2; }
  for format in $FORMATS; do
    echo "--- $fixture × $format"
    skip="$(skip_reason "$fixture" "$format")"
    if [ -n "$skip" ]; then
      echo "    skip (by design): $skip"
      RESULTS+=("$fixture|$format|SKIP (by design)")
      continue
    fi
    proj="$SANDBOX_ROOT/runs/$fixture--$format"
    log="$SANDBOX_ROOT/logs/$fixture--$format.log"
    : >"$log"
    stage_fixture "$fixture" "$proj"
    ok=0
    case "$format" in
      pnpm) leg_pnpm "$proj" "$log" "$fixture" || ok=$? ;;
      npm)  leg_npm  "$proj" "$log" "$fixture" || ok=$? ;;
      bun)  leg_bun  "$proj" "$log" "$fixture" || ok=$? ;;
      yarn) leg_yarn "$proj" "$log" "$fixture" || ok=$? ;;
      nub)  leg_nub  "$proj" "$log" "$fixture" || ok=$? ;;
      *) echo "error: unknown format '$format'" >&2; exit 2 ;;
    esac
    reason="$(expected_reason "$fixture" "$format")"
    if [ "$ok" -eq 0 ] && [ -z "$reason" ]; then
      status="PASS"
    elif [ "$ok" -eq 0 ] && [ -n "$reason" ]; then
      # The red list must shrink the moment a fix lands — a passing entry is
      # stale and fails the run so the green flip can't go unrecorded.
      status="XPASS-STALE"
      XPASSES=$((XPASSES + 1))
      echo "    XPASS: now passes — delete its expected-failures.txt entry: $reason"
    elif [ -n "$reason" ]; then
      status="RED (expected)"
      echo "    expected red: $reason"
    else
      status="FAIL"
      FAILS=$((FAILS + 1))
      echo "    FAIL — log: $log"
      tail -n 15 "$log" | sed 's/^/    | /'
    fi
    RESULTS+=("$fixture|$format|$status")
  done
done

echo
echo "== results =="
printf '%-18s %-6s %s\n' "fixture" "format" "result"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r f m s <<<"$row"
  printf '%-18s %-6s %s\n' "$f" "$m" "$s"
done
echo

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
