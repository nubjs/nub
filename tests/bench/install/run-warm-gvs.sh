#!/usr/bin/env bash
# Warm-install benchmark, GVS-eligibility split.
#
# Measures warm install (warm CAS store + lockfile present, node_modules wiped,
# full offline reinstall) for nub vs pnpm (vs bun, conditionally) on two fixtures:
#
#   gvs-eligible   — realistic backend/library project with NO
#                    next/nuxt/parcel dep. nub's global virtual
#                    store (GVS) stays ON → node_modules is a symlink farm into
#                    one shared store.
#   gvs-ineligible — same deps PLUS `next`, which is on nub's
#                    disableGlobalVirtualStoreForPackages list
#                    (next,nuxt,parcel). GVS auto-disables → nub
#                    falls back to per-project materialize ≈ pnpm parity.
#                    The GVS-on speedup does not apply to Next/Nuxt/Parcel.
#
# IMPORTANT — nub's GVS trigger list is next,nuxt,parcel. vite, vitepress, and
# @sveltejs/kit are NOT triggers in nub (they are aube's standalone defaults,
# which nub overrides via the embedder-defaults seam). So those apps keep GVS ON.
# The auto-disable path is exercised with `next`, not vite.
#
# Teardown (wiping node_modules) is via rename-aside in hyperfine --prepare and
# is EXCLUDED from timing — see tests/bench/install/README.md for the full methodology.
#
# GVS state is pinned via the CI env var (env -u CI → on; CI=1 → off) so an
# ambient CI var can't silently flip nub between code paths. We measure at
# DEFAULTS: GVS on for nub, pnpm at its per-project-reflink default.
#
# Requires: hyperfine, pnpm, perl; bun optional; NUB env var or
# target/release/nub built.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
NUB="${NUB:-$REPO_ROOT/target/release/nub}"
# Resolve NUB to an absolute path — the timed install commands run with --cwd set
# to a fixture dir, so a relative NUB= override must still resolve.
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" 2>/dev/null && pwd)/$(basename "$NUB")" ;; esac
FIXTURE_DIR="$REPO_ROOT/tests/bench/install/fixtures"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"

TRASH_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bench-warm-gvs-trash-$$-XXXXXX")"
trap 'rm -rf "$TRASH_DIR" 2>/dev/null || true' EXIT

# Empty npmrc so a host ~/.npmrc can't route fetches away from the bench target.
EMPTY_NPMRC="$TRASH_DIR/empty.npmrc"
touch "$EMPTY_NPMRC"
export NPM_CONFIG_USERCONFIG="$EMPTY_NPMRC"
export NPM_CONFIG_GLOBALCONFIG="$EMPTY_NPMRC"

WARMUP=3
RUNS=12
FIXTURE_FILTER=""
SAVE_RESULTS=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --warmup) WARMUP="$2"; shift 2 ;;
    --runs)   RUNS="$2";   shift 2 ;;
    --fixture) FIXTURE_FILTER="$2"; shift 2 ;;
    --save) SAVE_RESULTS=1; shift ;;
    *) echo "WARN: unknown arg '$1'" >&2; shift ;;
  esac
done
if [[ "$SAVE_RESULTS" -eq 1 ]]; then
  RESULTS_DIR="$REPO_ROOT/tests/bench/install/results"
else
  RESULTS_DIR="$(mktemp -d /tmp/nub-bench-results-XXXXXX)"
fi

command -v hyperfine &>/dev/null || { echo "ERROR: hyperfine not found." >&2; exit 1; }
command -v pnpm &>/dev/null || { echo "ERROR: pnpm not found." >&2; exit 1; }
command -v perl &>/dev/null || { echo "ERROR: perl not found." >&2; exit 1; }
[[ -x "$NUB" ]] || { echo "ERROR: nub binary not found at $NUB" >&2; exit 1; }

HAS_BUN=0; command -v bun &>/dev/null && HAS_BUN=1

mkdir -p "$RESULTS_DIR"

setup_workdir() {
  local fixture="$1" workdir="$2" tool="$3"
  rm -rf "$workdir"
  cp -r "$FIXTURE_DIR/$fixture" "$workdir"
  rm -rf "$workdir/node_modules" 2>/dev/null || true
  case "$tool" in
    pnpm|nub) rm -f "$workdir/bun.lock" "$workdir/bun.lockb" "$workdir/package-lock.json" 2>/dev/null || true ;;
    bun)      rm -f "$workdir/pnpm-lock.yaml" "$workdir/pnpm-workspace.yaml" "$workdir/package-lock.json" 2>/dev/null || true ;;
  esac
}

# rename-aside + detached reap; echoes a --prepare snippet (untimed).
#
# CORRECTNESS over cleverness: the timed command MUST start every run with NO
# node_modules, or a tool that detects an already-installed tree no-ops (bun does
# this — it short-circuits a frozen install when node_modules is present, yielding
# bogus ~0ms "installs"). The earlier shared `rm -rf $TRASH/r-* &` glob raced the
# next iteration's mv and left node_modules half-present. Fix: rename to a UNIQUE
# per-iteration slot, reap ONLY that slot in the background, and — critically —
# busy-wait until node_modules is actually gone before --prepare returns. The mv
# is atomic (~50ms even for a materialized tree); the slow rm is still off the
# timed path.
reset_cmd() {
  local wd="$1"
  local base; base="$(basename "$wd")"
  # $$ here is the harness PID (constant); $RANDOM varies per --prepare eval.
  echo "slot='$TRASH_DIR/r-${base}-$$-'\$RANDOM\$RANDOM; if [ -e '$wd/node_modules' ]; then mv '$wd/node_modules' \"\$slot\" && (rm -rf \"\$slot\" 2>/dev/null &) ; fi; while [ -e '$wd/node_modules' ]; do rm -rf '$wd/node_modules' 2>/dev/null; done; true"
}

run_warm() {
  local fixture="$1" label="$2"
  local WD_PNPM="/tmp/bw-pnpm-$$-$fixture" WD_NUB="/tmp/bw-nub-$$-$fixture" WD_BUN="/tmp/bw-bun-$$-$fixture"

  echo "────────────────────────────────────────────────────────────────"
  echo "  WARM — $label"
  echo "  (warm CAS store + lockfile; node_modules wiped between runs;"
  echo "   teardown rename-aside, EXCLUDED from timing; nub at DEFAULTS, GVS on)"
  echo "────────────────────────────────────────────────────────────────"

  setup_workdir "$fixture" "$WD_PNPM" pnpm
  setup_workdir "$fixture" "$WD_NUB" nub

  echo "[setup] pre-populating pnpm store + node_modules..."
  pnpm install --frozen-lockfile --dir "$WD_PNPM" --silent 2>/dev/null \
    || pnpm install --frozen-lockfile --dir "$WD_PNPM" 2>&1 | tail -3

  echo "[setup] pre-populating nub CAS store + GVS (showing any GVS warning)..."
  # Capture nub's stderr so the GVS auto-disable warning is visible for the
  # ineligible fixture. env -u CI → GVS on (default).
  env -u CI "$NUB" install --frozen-lockfile --cwd "$WD_NUB" 2>&1 | tail -6 || true

  # Report which linking path nub actually took. Both GVS-on and GVS-off use the
  # node_modules/<pkg> -> .nub/<pkg>/node_modules/<pkg> symlink layout, so the
  # top-level symlink is NOT the signal. The real signal is INSIDE .nub: with GVS
  # ON the inner package is HARDLINKED from the shared global virtual store
  # (nlink>=2); with GVS OFF it is materialized fresh per-project (nlink==1).
  local inner="$WD_NUB/node_modules/.nub/lodash@4.18.1/node_modules/lodash/package.json"
  if [[ -f "$inner" ]]; then
    local nlink; nlink=$(stat -f '%l' "$inner" 2>/dev/null || stat -c '%h' "$inner" 2>/dev/null)
    if [[ "${nlink:-1}" -ge 2 ]]; then
      echo "  [nub linking: GVS ON — inner pkg hardlinked from shared global virtual store (nlink=$nlink)]"
    else
      echo "  [nub linking: GVS OFF — per-project materialize (nlink=$nlink, no cross-project sharing)]"
    fi
  fi

  local outfile="$RESULTS_DIR/warm-gvs-${fixture}-${TIMESTAMP}.json"
  local nub_cmd="env -u CI NPM_CONFIG_USERCONFIG='$EMPTY_NPMRC' NPM_CONFIG_GLOBALCONFIG='$EMPTY_NPMRC' '$NUB' install --frozen-lockfile --cwd '$WD_NUB' -s"

  local HF_ARGS=(
    --warmup "$WARMUP" --runs "$RUNS"
    --prepare "$(reset_cmd "$WD_NUB")"
    --command-name "nub install"
    "$nub_cmd"
    --prepare "$(reset_cmd "$WD_PNPM")"
    --command-name "pnpm install"
    "pnpm install --frozen-lockfile --dir '$WD_PNPM' --silent"
  )

  if [[ $HAS_BUN -eq 1 && -f "$FIXTURE_DIR/$fixture/bun.lock" ]]; then
    setup_workdir "$fixture" "$WD_BUN" bun
    echo "[setup] pre-populating bun cache..."
    bun install --frozen-lockfile --cwd "$WD_BUN" 2>/dev/null \
      || bun install --frozen-lockfile --cwd "$WD_BUN" 2>&1 | tail -3
    HF_ARGS+=(
      --prepare "$(reset_cmd "$WD_BUN")"
      --command-name "bun install (ref)"
      "bun install --frozen-lockfile --cwd '$WD_BUN'"
    )
  fi

  # re-populate nub node_modules consumed by the symlink check above
  env -u CI "$NUB" install --frozen-lockfile --cwd "$WD_NUB" -s 2>/dev/null || true

  hyperfine "${HF_ARGS[@]}" --export-json "$outfile"
  echo "  [results saved → $outfile]"
  echo ""

  for wd in "$WD_PNPM" "$WD_NUB" "$WD_BUN"; do
    [[ -e "$wd" ]] && mv "$wd" "$TRASH_DIR/wd-$(basename "$wd")-$RANDOM" 2>/dev/null || true
  done
  rm -rf "$TRASH_DIR"/wd-* 2>/dev/null & true
}

echo "================================================================"
echo "  Warm-install benchmark — GVS-eligibility split"
echo "  nub:  $("$NUB" --version 2>&1 | head -1)  ($NUB)"
echo "  pnpm: $(pnpm --version)"
echo "  bun:  $([[ $HAS_BUN -eq 1 ]] && bun --version || echo '(absent)')"
echo "  date: $(date)   load: $(uptime | sed 's/.*load averages*: //')"
echo "================================================================"

FIXTURES=(
  "gvs-eligible|gvs-eligible (~571 pkgs, NO next/nuxt/parcel — GVS ON)"
  "gvs-ineligible|gvs-ineligible (~619 pkgs, +next — GVS auto-disabled)"
)
for entry in "${FIXTURES[@]}"; do
  IFS='|' read -r name lbl <<< "$entry"
  [[ -n "$FIXTURE_FILTER" && "$name" != "$FIXTURE_FILTER" ]] && continue
  run_warm "$name" "$lbl"
done

echo "================================================================"
echo "  Done. Results in $RESULTS_DIR/"
echo "================================================================"
