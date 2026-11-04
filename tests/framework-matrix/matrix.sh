#!/usr/bin/env bash
# matrix.sh — the committed framework GVS acceptance matrix. For each framework:
# scaffold it via its OFFICIAL create-* generator (frameworks.sh), then drive the
# full lifecycle through run.sh UNDER nub's global virtual store:
#
#     install → dev-serve + page-load 200 → production build → build-serve + 200
#
# and record, per framework: the pass/fail of each stage, the resolved
# framework/bundler versions, the on-disk linking layout, and — the key GVS
# finding — WHICH deps were INJECTED (disk-materialized / ejected project-local
# while the rest of the tree stays symlinked into the machine-global store).
#
# Usage: matrix.sh [-b <nub>] [--force-gvs] [--keep] [--isolate-store] [framework ...]
#   -b <nub>         nub binary (default: target/fast/nub, then the shared build)
#   --force-gvs      set NPM_CONFIG_ENABLE_GLOBAL_VIRTUAL_STORE=true for every
#                    non-excluded framework (proves GVS specifically; the default
#                    already engages GVS but respects triggers). NEVER applied to a
#                    GVS=exclude framework (next/react-native) — that is an
#                    unsupported config for them.
#   --isolate-store  give each framework a fresh XDG_CACHE_HOME/XDG_DATA_HOME so
#                    the shared machine-global store can't mask a result (slower —
#                    a genuine cold fetch per framework). Default reuses the real
#                    warm store (the honest "what a user gets", and fast).
#   --keep           keep scaffolded fixtures under $OUT on success.
#   [framework ...]  subset to run (default: all — `frameworks.sh list`).
#
# Emits per-framework ROW/VERDICT lines (from run.sh) plus a final summary table
# and an INJECTED| line per framework. Exit 0 iff every non-excluded framework
# PASSes (an excluded framework is asserted to land PROJECT-LOCAL, not GVS).
set -u

# Needs bash 4.4+: associative arrays + mapfile are 4.0, but the "empty array +
# ${arr[@]} under set -u = unbound variable" bug (which bites the default
# no-flag path where env_prefix=()) was only fixed in 4.4. macOS ships 3.2 —
# re-exec under a newer bash if one is on PATH, else error clearly.
_bash_ge_44() { [ "${BASH_VERSINFO[0]}" -gt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -ge 4 ]; }; }
if ! _bash_ge_44; then
  for b in /opt/homebrew/bin/bash /usr/local/bin/bash bash; do
    if command -v "$b" >/dev/null 2>&1 && \
       "$b" -c '[ "${BASH_VERSINFO[0]}" -gt 4 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -ge 4 ]; }' 2>/dev/null; then
      exec "$b" "$0" "$@"
    fi
  done
  echo "error: matrix.sh needs bash 4.4+ (macOS system bash is 3.2 — 'brew install bash')" >&2
  exit 2
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── args ──────────────────────────────────────────────────────────────────────
NUB=""; FORCE_GVS=0; KEEP=0; ISOLATE=0; SEL=()
while [ $# -gt 0 ]; do
  case "$1" in
    -b) NUB="$2"; shift 2 ;;
    --force-gvs) FORCE_GVS=1; shift ;;
    --keep) KEEP=1; shift ;;
    --isolate-store) ISOLATE=1; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) SEL+=("$1"); shift ;;
  esac
done

if [ -z "$NUB" ]; then
  for c in "$HERE/../../target/fast/nub" "$HOME/.cache/nub/shared-target/fast/nub"; do
    [ -x "$c" ] && { NUB="$c"; break; }
  done
fi
[ -n "$NUB" ] && [ -x "$NUB" ] || { echo "error: no nub binary (pass -b <path>)" >&2; exit 2; }
NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")"

[ ${#SEL[@]} -eq 0 ] && mapfile -t SEL < <(bash "$HERE/frameworks.sh" list)

OUT="${OUT:-$(mktemp -d "${TMPDIR:-/tmp}/nub-fw-matrix.XXXXXX")}"
echo "== framework GVS matrix =="
echo "nub:   $NUB ($("$NUB" --version 2>/dev/null | head -1))"
echo "out:   $OUT"
echo "force-gvs=$FORCE_GVS  isolate-store=$ISOLATE  frameworks: ${SEL[*]}"
echo

declare -A VERDICT INJECTED LAYOUT
port=5180
fail=0

for fw in "${SEL[@]}"; do
  echo "── $fw ───────────────────────────────────────────────────────────────"
  dest="$OUT/$fw"
  meta="$(bash "$HERE/frameworks.sh" scaffold "$fw" "$dest")" || {
    echo "SCAFFOLD FAIL: $fw"; VERDICT[$fw]="SCAFFOLD-FAIL"; fail=1; continue; }
  # Pull the five keys out of the scaffolder's stdout.
  dev=$(sed -n 's/^DEV=//p'         <<<"$meta")
  build=$(sed -n 's/^BUILD=//p'     <<<"$meta")
  preview=$(sed -n 's/^PREVIEW=//p' <<<"$meta")
  probe=$(sed -n 's/^PROBE=//p'     <<<"$meta")
  gvs=$(sed -n 's/^GVS=//p'         <<<"$meta")

  # Assign a distinct dev/preview port pair per framework (run.sh reads the REAL
  # bound port from the server log, but distinct requests avoid a same-run clash).
  dport=$port; pport=$((port+5)); port=$((port+10))
  dev="${dev//\$PORT/$dport}"; dev="${dev//\$PPORT/$pport}"
  preview="${preview//\$PPORT/$pport}"; preview="${preview//\$PORT/$dport}"

  # GVS policy. Excluded frameworks run default (trigger → project-local); a
  # GVS=require fixture always forces the store on (its config is what makes the
  # framework store-aware); the rest optionally force GVS on via --force-gvs to
  # prove the shared store specifically.
  env_prefix=()
  if [ "$gvs" = "require" ] || { [ "$gvs" != "exclude" ] && [ "$FORCE_GVS" -eq 1 ]; }; then
    env_prefix=(env NPM_CONFIG_ENABLE_GLOBAL_VIRTUAL_STORE=true)
  fi
  if [ "$ISOLATE" -eq 1 ]; then
    iso="$OUT/.store-$fw"; mkdir -p "$iso/cache" "$iso/data"
    env_prefix=(env XDG_CACHE_HOME="$iso/cache" XDG_DATA_HOME="$iso/data" "${env_prefix[@]}")
  fi

  out="$OUT/$fw.runlog"
  NUB="$NUB" "${env_prefix[@]}" bash "$HERE/run.sh" "$fw" "$dest" "$dev" "$build" "$preview" "$probe" 2>&1 | tee "$out"

  v=$(sed -n 's/^VERDICT|'"$fw"'|\([A-Z]*\).*/\1/p' "$out" | head -1)
  lay=$(sed -n 's/.*layout=\([a-z-]*\).*/\1/p' "$out" | head -1)
  inj=$(sed -n 's/.*step=linking .*locals=\([^ ]*\).*/\1/p' "$out" | head -1)
  VERDICT[$fw]="${v:-NO-VERDICT}"; LAYOUT[$fw]="${lay:-?}"; INJECTED[$fw]="${inj:-?}"

  # Verdict interpretation per GVS policy.
  if [ "$gvs" = "exclude" ]; then
    # Excluded frameworks: success = builds/serves AND landed project-local.
    if [ "${VERDICT[$fw]}" = "PASS" ] && [ "${LAYOUT[$fw]}" = "project-local" ]; then
      echo "  → $fw PASS (project-local by trigger, as expected — GVS-excluded)"
    else
      echo "  → $fw check: verdict=${VERDICT[$fw]} layout=${LAYOUT[$fw]} (expected PASS+project-local)"
      [ "${VERDICT[$fw]}" != "PASS" ] && fail=1
    fi
    INJECTED[$fw]="n/a (GVS-excluded → project-local)"
  elif [ "$gvs" = "require" ]; then
    # Required-GVS fixtures: success = builds/serves AND the store layout is in
    # use — a project-local majority here means the store was silently bypassed.
    if [ "${VERDICT[$fw]}" = "PASS" ] && [ "${LAYOUT[$fw]}" = "gvs-store" ]; then
      echo "  → $fw PASS (shared store, as required)"
    else
      echo "  → $fw check: verdict=${VERDICT[$fw]} layout=${LAYOUT[$fw]} (expected PASS+gvs-store)"
      fail=1
    fi
  else
    [ "${VERDICT[$fw]}" != "PASS" ] && fail=1
  fi
  echo
done

# ── summary ───────────────────────────────────────────────────────────────────
echo "================================ SUMMARY ================================"
printf "%-12s %-8s %-14s %s\n" FRAMEWORK VERDICT LAYOUT "INJECTED DEPS (project-local under GVS)"
for fw in "${SEL[@]}"; do
  printf "%-12s %-8s %-14s %s\n" "$fw" "${VERDICT[$fw]:-?}" "${LAYOUT[$fw]:-?}" "${INJECTED[$fw]:-?}"
done
echo
if [ "$KEEP" -eq 1 ]; then echo "fixtures kept under $OUT"; else rm -rf "$OUT"; fi
[ "$fail" -eq 0 ] && echo "MATRIX: ALL PASS" || echo "MATRIX: FAILURES (see rows above)"
exit "$fail"
