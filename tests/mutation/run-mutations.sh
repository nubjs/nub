#!/usr/bin/env bash
# Lockfile MUTATION differential harness — the write-path counterpart to the
# static round trips in tests/conformance/ and tests/lockfile-conformance/. A
# static install can pass while `nub add` / `nub remove` / `nub update` churns
# the untouched part of a lockfile, de-dups a shared transitive differently, or
# over/under-prunes. See README.md for the full design.
#
# Two legs per fixture, each judged by REAL pnpm (pinned via npx):
#
#   pnpm — a pnpm project (`packageManager: pnpm@<pin>`). Real pnpm installs in
#          two copies, nub mutates one and real pnpm the other. Real pnpm must
#          frozen-accept nub's mutated pnpm-lock.yaml without rewriting it, and
#          both mutated lockfiles must describe the same graph.
#   nub  — a nub project. nub installs and mutates, writing nub.lock; the
#          reference copy is a pnpm project that real pnpm installs and mutates.
#          A frozen nub install must leave nub.lock unchanged, real pnpm must
#          frozen-accept it renamed into a pnpm-declaring copy, and its graph
#          must equal real pnpm's.
#
# Graphs are compared semantically (extract-graph.mjs + compare-graphs.mjs):
# the same direct-spec map and resolved-version multiset, ignoring order and
# formatting, because `add` ordering legitimately differs run to run.
#
# Usage:  run-mutations.sh [<path-to-nub>] [fixture ...]
# Env:    LEGS="pnpm nub"      subset of legs to run
#         SANDBOX_ROOT=<dir>   reuse/inspect the sandbox (implies KEEP)
#         KEEP=1               keep the sandbox on success
# Exit:   0 = every leg passes or is an expected red;
#         1 = at least one unexpected FAIL or stale expected-failure entry.
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
NUB_VERSION="$("$NUB" --version 2>/dev/null | head -1 || echo '?')"

EXTRACT="$HERE/extract-graph.mjs"
COMPARE="$HERE/compare-graphs.mjs"
[ -f "$EXTRACT" ] && [ -f "$COMPARE" ] || { echo "error: extract/compare scripts missing in $HERE" >&2; exit 2; }

# The judge, fetched per run via npx into the sandbox HOME, so the pin is exact
# on every machine. It is the pnpm the engine tracks.
PNPM_PIN=12.4.1

ALL_FIXTURES=(m1-add-noconflict m3-add-dedup m5-remove-prune)
FIXTURES=("$@")
[ ${#FIXTURES[@]} -gt 0 ] || FIXTURES=("${ALL_FIXTURES[@]}")
LEGS="${LEGS:-pnpm nub}"

# Hermetic sandbox — redirect HOME + XDG so no dev-box config leaks in or out.
CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-mutation.XXXXXX")"
  CREATED_SANDBOX=1
fi
mkdir -p "$SANDBOX_ROOT/home" "$SANDBOX_ROOT/runs" "$SANDBOX_ROOT/logs"
export HOME="$SANDBOX_ROOT/home"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

run_pnpm() { npx -y "pnpm@$PNPM_PIN" "$@"; }

echo "=== nub lockfile-mutation differential ==="
echo "nub:      $NUB ($NUB_VERSION)"
echo "pnpm:     $PNPM_PIN (pinned via npx)"
echo "sandbox:  $SANDBOX_ROOT"
echo ""

step() {
  local log="$1" label="$2"; shift 2
  { echo; echo "### $label"; echo "### \$ $*"; } >>"$log"
  "$@" >>"$log" 2>&1
}

wipe_node_modules() { find "$1" -name node_modules -type d -prune -exec rm -rf {} + 2>/dev/null || true; }

# declare_pnpm <proj> — make the project a pnpm project by declaring the pin.
declare_pnpm() {
  (cd "$1" && node -e '
    const fs = require("fs");
    const manifest = JSON.parse(fs.readFileSync("package.json", "utf8"));
    manifest.packageManager = process.argv[1];
    fs.writeFileSync("package.json", JSON.stringify(manifest, null, 2) + "\n");
  ' "pnpm@$PNPM_PIN")
}

# stage_fixture <fixture> <proj> <identity> — copy the manifest (never the
# `mutation` spec, which is harness metadata) as the kind of project needed.
stage_fixture() {
  local fixture="$1" proj="$2" identity="$3"
  rm -rf "$proj"; mkdir -p "$proj"
  cp "$HERE/fixtures/$fixture/package.json" "$proj/package.json"
  if [ "$identity" = pnpm ]; then
    if [ -f "$HERE/fixtures/$fixture/pnpm-workspace.yaml" ]; then
      cp "$HERE/fixtures/$fixture/pnpm-workspace.yaml" "$proj/"
    fi
    declare_pnpm "$proj"
  fi
}

# Read a `<verb>: <args>` line from the fixture's mutation spec.
mutation_field() {
  local fixture="$1" verb="$2"
  awk -F': *' -v v="$verb" '!/^#/ && $1==v { print $2; exit }' "$HERE/fixtures/$fixture/mutation"
}

# baseline <who> <proj> <log> — the pre-mutation install, by nub or real pnpm.
baseline() {
  local who="$1" proj="$2" log="$3"
  case "$who" in
    nub)  ( cd "$proj" && step "$log" "nub install (baseline)" "$NUB" install ) ;;
    pnpm) ( cd "$proj" && step "$log" "pnpm install (baseline)" run_pnpm install ) ;;
  esac
}

# mutate <who> <proj> <log> <verb> <args> — nub and pnpm share the verb names.
mutate() {
  local who="$1" proj="$2" log="$3" verb="$4" args="$5"
  # shellcheck disable=SC2086
  case "$who" in
    nub)  ( cd "$proj" && step "$log" "nub $verb $args" "$NUB" "$verb" $args ) ;;
    pnpm) ( cd "$proj" && step "$log" "pnpm $verb $args" run_pnpm "$verb" $args ) ;;
  esac
}

# pnpm_accepts <proj> <log> <label> — real pnpm frozen-installs the project's
# pnpm-lock.yaml and leaves it byte-identical.
pnpm_accepts() {
  local proj="$1" log="$2" label="$3"
  [ -f "$proj/pnpm-lock.yaml" ] || { echo "FAILED: no pnpm-lock.yaml to judge ($label)" >>"$log"; return 1; }
  cp "$proj/pnpm-lock.yaml" "$log.frozen-before"
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "real pnpm frozen accept ($label)" run_pnpm install --frozen-lockfile ) \
    || { echo "FAILED: real pnpm rejected the mutated lockfile ($label, --frozen-lockfile)" >>"$log"; return 1; }
  cmp -s "$log.frozen-before" "$proj/pnpm-lock.yaml" || {
    echo "FAILED: real pnpm rewrote the mutated lockfile during a frozen install ($label)" >>"$log"
    diff -u "$log.frozen-before" "$proj/pnpm-lock.yaml" >>"$log" 2>&1 || true
    return 1
  }
}

# semantic_equal <nub_proj> <ref_proj> <log> — nub's mutated graph equals real pnpm's.
semantic_equal() {
  local nub_proj="$1" ref_proj="$2" log="$3"
  local ga="$log.graph-nub.json" gb="$log.graph-ref.json"
  node "$EXTRACT" "$nub_proj" >"$ga" 2>>"$log" \
    || { echo "FAILED: could not extract the graph from nub's lockfile" >>"$log"; return 1; }
  node "$EXTRACT" "$ref_proj" >"$gb" 2>>"$log" \
    || { echo "FAILED: could not extract the graph from real pnpm's lockfile" >>"$log"; return 1; }
  { echo; echo "### semantic compare (nub vs real pnpm)"; } >>"$log"
  node "$COMPARE" "$ga" "$gb" --label-a nub --label-b real-pnpm >>"$log" 2>&1 && return 0
  echo "FAILED: nub's mutated graph diverges from real pnpm's (see compare output above)" >>"$log"
  return 1
}

leg_pnpm() {
  local fixture="$1" log="$2" verb="$3" args="$4"
  local nub_proj="$SANDBOX_ROOT/runs/$fixture--pnpm/nub" ref_proj="$SANDBOX_ROOT/runs/$fixture--pnpm/ref"
  stage_fixture "$fixture" "$nub_proj" pnpm
  stage_fixture "$fixture" "$ref_proj" pnpm
  baseline pnpm "$nub_proj" "$log" || { echo "FAILED: baseline pnpm install (nub copy)" >>"$log"; return 1; }
  baseline pnpm "$ref_proj" "$log" || { echo "FAILED: baseline pnpm install (reference copy)" >>"$log"; return 1; }
  mutate nub "$nub_proj" "$log" "$verb" "$args" || { echo "FAILED: nub $verb $args errored" >>"$log"; return 1; }
  mutate pnpm "$ref_proj" "$log" "$verb" "$args" || { echo "FAILED: real pnpm $verb $args errored" >>"$log"; return 1; }
  pnpm_accepts "$nub_proj" "$log" "pnpm project" || return 1
  semantic_equal "$nub_proj" "$ref_proj" "$log"
}

leg_nub() {
  local fixture="$1" log="$2" verb="$3" args="$4"
  local base="$SANDBOX_ROOT/runs/$fixture--nub"
  local nub_proj="$base/nub" ref_proj="$base/ref" judge="$base/judge"
  stage_fixture "$fixture" "$nub_proj" nub
  stage_fixture "$fixture" "$ref_proj" pnpm
  baseline nub "$nub_proj" "$log" || { echo "FAILED: baseline nub install" >>"$log"; return 1; }
  baseline pnpm "$ref_proj" "$log" || { echo "FAILED: baseline pnpm install (reference copy)" >>"$log"; return 1; }
  mutate nub "$nub_proj" "$log" "$verb" "$args" || { echo "FAILED: nub $verb $args errored" >>"$log"; return 1; }
  mutate pnpm "$ref_proj" "$log" "$verb" "$args" || { echo "FAILED: real pnpm $verb $args errored" >>"$log"; return 1; }
  [ -f "$nub_proj/nub.lock" ] || { echo "FAILED: nub wrote no nub.lock" >>"$log"; return 1; }
  cp "$nub_proj/nub.lock" "$log.nub-before"
  wipe_node_modules "$nub_proj"
  ( cd "$nub_proj" && step "$log" "nub install --frozen-lockfile" "$NUB" install --frozen-lockfile ) \
    || { echo "FAILED: a frozen nub install rejected the mutated nub.lock" >>"$log"; return 1; }
  cmp -s "$log.nub-before" "$nub_proj/nub.lock" \
    || { echo "FAILED: a frozen nub install rewrote the mutated nub.lock" >>"$log"; return 1; }
  # nub.lock is pnpm's lockfile format, so the same file in a pnpm-declaring
  # copy is real pnpm's to judge.
  rm -rf "$judge"; mkdir -p "$judge"
  cp "$nub_proj/package.json" "$judge/package.json"
  cp "$nub_proj/nub.lock" "$judge/pnpm-lock.yaml"
  if [ -f "$HERE/fixtures/$fixture/pnpm-workspace.yaml" ]; then
    cp "$HERE/fixtures/$fixture/pnpm-workspace.yaml" "$judge/"
  fi
  declare_pnpm "$judge"
  pnpm_accepts "$judge" "$log" "nub.lock as pnpm-lock.yaml" || return 1
  semantic_equal "$nub_proj" "$ref_proj" "$log"
}

expected_reason() {
  awk -v f="$1" -v l="$2" \
    '!/^#/ && $1==f && $2==l { $1=""; $2=""; sub(/^  */,""); print; exit }' \
    "$HERE/expected-failures.txt" 2>/dev/null
}

RESULTS=(); FAILS=0; XPASSES=0

for fixture in "${FIXTURES[@]}"; do
  [ -d "$HERE/fixtures/$fixture" ] || { echo "error: unknown fixture '$fixture'" >&2; exit 2; }
  verb=""; args=""
  for v in add remove update; do
    args="$(mutation_field "$fixture" "$v")"
    [ -n "$args" ] && { verb="$v"; break; }
  done
  [ -n "$verb" ] || { echo "error: fixture $fixture has no mutation spec" >&2; exit 2; }

  for leg in $LEGS; do
    echo "--- $fixture × $leg ($verb $args)"
    log="$SANDBOX_ROOT/logs/$fixture--$leg.log"; : >"$log"
    ok=0
    case "$leg" in
      pnpm) leg_pnpm "$fixture" "$log" "$verb" "$args" || ok=$? ;;
      nub)  leg_nub  "$fixture" "$log" "$verb" "$args" || ok=$? ;;
      *) echo "error: unknown leg '$leg'" >&2; exit 2 ;;
    esac

    reason="$(expected_reason "$fixture" "$leg")"
    if [ "$ok" -eq 0 ] && [ -z "$reason" ]; then
      echo "    PASS"
      RESULTS+=("$fixture|$leg|PASS")
    elif [ "$ok" -eq 0 ] && [ -n "$reason" ]; then
      echo "    XPASS-STALE: now passes — remove from expected-failures.txt: $reason"
      XPASSES=$((XPASSES + 1))
      RESULTS+=("$fixture|$leg|XPASS-STALE")
    elif [ -n "$reason" ]; then
      echo "    expected red: $reason"
      RESULTS+=("$fixture|$leg|RED (expected)")
    else
      FAILS=$((FAILS + 1))
      echo "    FAIL — log: $log"
      tail -n 24 "$log" | sed 's/^/    | /'
      RESULTS+=("$fixture|$leg|FAIL")
    fi
  done
done

echo ""
echo "=== results ==="
printf '%-22s %-6s %s\n' "fixture" "leg" "result"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r f l s <<<"$row"
  printf '%-22s %-6s %s\n' "$f" "$l" "$s"
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
