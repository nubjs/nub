#!/usr/bin/env bash
# Install benchmark: nub install vs pnpm / bun / npm.
# Full process wall-clock, frozen lockfile, warm + cold scenarios.
#
# Usage:
#   bash tests/bench/install/run.sh [--cold-only | --warm-only] [--materialized]
#                           [--fixture <name>]
#
#   --warm-only / --cold-only   run just one scenario
#   --materialized              warm leg: pin nub's linking to per-project
#                               materialization (CI=1, GVS off) instead of the
#                               default global-virtual-store symlink farm. Use
#                               this to reproduce the CI-leg's linking path.
#   --fixture <name>            run a single fixture (simple|monorepo|t3|large)
#
# WARM (headline): warm CAS store + lockfile present, node_modules WIPED, then a
# full OFFLINE reinstall — the apples-to-apples repeated-checkout number vs
# pnpm/bun/npm. Teardown is via rename-aside in hyperfine --prepare and is
# EXCLUDED from timing. nub's GVS state is pinned (not inherited from $CI). See
# the long comment above run_warm() and tests/bench/install/README.md for the full
# methodology and the teardown-vs-install separation.
#
# Requires: hyperfine, pnpm, perl; bun + npm optional; target/release/nub built.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
NUB="$REPO_ROOT/target/release/nub"
FIXTURE_DIR="$REPO_ROOT/tests/bench/install/fixtures"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"

# Trash dir for rename-aside teardown. node_modules is mv'd here (fast) and a
# detached rm reaps it off the timed path. Cleaned on exit.
TRASH_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bench-trash-$$-XXXXXX")"
cleanup_trash() { rm -rf "$TRASH_DIR" 2>/dev/null || true; }
trap cleanup_trash EXIT

# Empty npmrc so a host ~/.npmrc can't redirect fetches (e.g. custom registry=).
EMPTY_NPMRC="$TRASH_DIR/empty.npmrc"
touch "$EMPTY_NPMRC"
export NPM_CONFIG_USERCONFIG="$EMPTY_NPMRC"
export NPM_CONFIG_GLOBALCONFIG="$EMPTY_NPMRC"

RUN_WARM=1
RUN_COLD=1
MATERIALIZED=0
FIXTURE_FILTER=""
SAVE_RESULTS=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cold-only)    RUN_WARM=0 ;;
    --warm-only)    RUN_COLD=0 ;;
    --materialized) MATERIALIZED=1 ;;
    --fixture)      shift; FIXTURE_FILTER="${1:-}" ;;
    --save)         SAVE_RESULTS=1 ;;
    *) echo "WARN: unknown arg '$1'" >&2 ;;
  esac
  shift
done
if [[ "$SAVE_RESULTS" -eq 1 ]]; then
  RESULTS_DIR="$REPO_ROOT/tests/bench/install/results"
else
  RESULTS_DIR="$(mktemp -d /tmp/nub-bench-results-XXXXXX)"
fi

HAS_BUN=0
if command -v bun &>/dev/null; then HAS_BUN=1; fi

# ── Preflight ──────────────────────────────────────────────────────────────
if [[ ! -x "$NUB" ]]; then
  echo "ERROR: $NUB not found or not executable. Run 'cargo build --release' first." >&2
  exit 1
fi
if ! command -v hyperfine &>/dev/null; then
  echo "ERROR: hyperfine not found. Install with: brew install hyperfine" >&2
  exit 1
fi
if ! command -v pnpm &>/dev/null; then
  echo "ERROR: pnpm not found." >&2
  exit 1
fi
if ! command -v perl &>/dev/null; then
  echo "ERROR: perl not found (used for sub-ms timing)." >&2
  exit 1
fi

NUB_VERSION="$("$NUB" --version 2>&1 | { read -r line; echo "$line"; cat > /dev/null; })"
PNPM_VERSION="$(pnpm --version)"
BUN_VERSION="$(bun --version 2>/dev/null || echo "(not installed)")"
NPM_VERSION="$(npm --version 2>/dev/null || echo "(not installed)")"

echo "================================================================"
echo "  Install benchmark: nub vs pnpm / bun / npm"
echo "  nub:  $NUB_VERSION  ($NUB)"
echo "  pnpm: $PNPM_VERSION  ($(command -v pnpm))"
echo "  bun:  $BUN_VERSION"
echo "  npm:  $NPM_VERSION"
echo "  date: $(date)"
echo "  warm-leg linking: $([[ $MATERIALIZED -eq 1 ]] && echo 'materialized (CI=1, GVS off)' || echo 'GVS on (symlink farm, default)')"
echo "================================================================"
echo ""

mkdir -p "$RESULTS_DIR"

# ── Helper: copy fixture to a fresh workdir ────────────────────────────────
setup_workdir() {
  local fixture="$1"
  local workdir="$2"
  local tool="${3:-}"   # optional: pnpm | nub | bun | npm — prunes foreign lockfiles
  rm -rf "$workdir"
  cp -r "$FIXTURE_DIR/$fixture" "$workdir"
  rm -rf "$workdir/node_modules" "$workdir"/packages/*/node_modules 2>/dev/null || true
  # A fixture may ship multiple lockfiles (pnpm-lock.yaml for nub+pnpm, bun.lock for
  # bun, package-lock.json for npm). Each tool installs from its OWN lockfile; a
  # foreign lockfile alongside makes nub refuse to infer a PM ("two competing
  # lockfiles") and lets a tool read a stale lock. Resolution stays identical within
  # a tool's own lockfile family; we only strip the OTHER tools' lockfiles from this
  # tool's isolated workdir.
  case "$tool" in
    pnpm|nub) rm -f "$workdir/bun.lock" "$workdir/bun.lockb" "$workdir/package-lock.json" 2>/dev/null || true ;;
    bun)      rm -f "$workdir/pnpm-lock.yaml" "$workdir/pnpm-workspace.yaml" "$workdir/package-lock.json" 2>/dev/null || true ;;
    npm)      rm -f "$workdir/bun.lock" "$workdir/bun.lockb" "$workdir/pnpm-lock.yaml" "$workdir/pnpm-workspace.yaml" 2>/dev/null || true ;;
  esac
}

# ── Warm benchmark ──────────────────────────────────────────────────────────
#
# WARM definition: warm CAS store + lockfile present,
# node_modules WIPED, then a full OFFLINE reinstall. This is the apples-to-apples
# repeated-checkout / CI-restore number vs pnpm/bun/npm.
#
# TEARDOWN IS NOT TIMED. hyperfine's --prepare runs BEFORE each timed run and is
# excluded from the measurement (only the benchmarked command's wall-clock is
# recorded). So wiping node_modules between iterations never dilutes the install
# number — confirmed against `hyperfine --help` (1.15.0): "--prepare ... Execute
# CMD before each timing run." We give EACH command its own --prepare so each
# tool's teardown is isolated to that tool and excluded from that tool's timing.
#
# FAST RESET (the reason this matters): a tool's node_modules teardown cost is
# wildly asymmetric. With GVS on, nub's node_modules is a ~260K–780K symlink farm
# and `rm -rf` is ~100–240ms. pnpm/npm materialize REAL files into the project
# (`node_modules/.pnpm/`, 140–540MB) and `rm -rf` takes 1–12 SECONDS. Even though
# teardown is untimed, paying 12s × 12 runs of pure deletion per fixture makes the
# suite impractical — repeated install/delete cycles would let deletion cost
# dominate. The fix: rename-aside + background delete.
# The reset moves node_modules to a trash dir (an atomic `mv`, ~50ms even for the
# materialized case) and a detached `rm -rf` reaps it later. The install never sees
# stale state, and wall-clock stays bounded. The teardown cost itself is measured
# and reported SEPARATELY below so it stays transparent.
#
# GVS STATE IS PINNED, NOT INHERITED. aube's global virtual store defaults to ON
# outside CI and OFF inside CI (`!aube_util::env::is_ci()`, and is_ci() is just
# `CI` being set in the env). An ambient `CI` var would silently flip nub from the
# symlink-farm path to the materialized path — a different scenario wearing the
# same "warm" label. We therefore pin it explicitly:
#   warm (default):  env -u CI   → GVS on  → symlink farm (the headline number)
#   --materialized:  CI=1        → GVS off → real files (matches CI-leg behavior)
# NOTE: `CI=` (empty but set) still counts as CI; only `env -u CI` turns GVS on.
# We do NOT clear the CAS store or the GVS itself — clearing those would make the
# run COLD, not warm.

# Pin GVS via the CI env var. Usage: nub_env <args...>
nub_install() {
  local wd="$1"; shift
  if [[ $MATERIALIZED -eq 1 ]]; then
    CI=1 "$NUB" install --frozen-lockfile --cwd "$wd" "$@"
  else
    env -u CI "$NUB" install --frozen-lockfile --cwd "$wd" "$@"
  fi
}

# Build a prepare command that renames node_modules aside (fast) and schedules a
# detached background delete. Echoes a shell snippet for hyperfine --prepare.
# The `mv` is the only synchronous cost; the rm is reaped off the critical path.
reset_cmd() {
  local wd="$1"
  local trash="$TRASH_DIR/$(basename "$wd")"
  # mv each node_modules (root + workspace packages) into a uniquely-named trash
  # slot, then fire a detached rm. `true` keeps --prepare's exit status 0.
  echo "for nm in '$wd/node_modules' '$wd'/packages/*/node_modules; do [ -e \"\$nm\" ] && mv \"\$nm\" '$trash'-\$RANDOM-\$RANDOM 2>/dev/null; done; rm -rf '$trash'-* 2>/dev/null & true"
}

# Measure (untimed-but-reported) the teardown cost for a tool's materialized
# node_modules, so the rename-aside saving is transparent. Prints one line.
report_teardown_cost() {
  local label="$1" wd="$2"
  [[ -d "$wd/node_modules" ]] || return 0
  local t0 t1
  t0=$(perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000000')
  mv "$wd/node_modules" "$TRASH_DIR/td-$label-$RANDOM" 2>/dev/null || true
  t1=$(perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000000')
  printf "    teardown(%s): rename-aside %dms (full rm -rf deferred to background)\n" \
    "$label" "$(( (t1 - t0) / 1000 ))"
}

run_warm() {
  local fixture="$1"
  local label="$2"
  local WD_PNPM="/tmp/bench-warm-pnpm-$$-$fixture"
  local WD_NUB="/tmp/bench-warm-nub-$$-$fixture"
  local WD_BUN="/tmp/bench-warm-bun-$$-$fixture"
  local WD_NPM="/tmp/bench-warm-npm-$$-$fixture"

  local mode_label="GVS on (symlink farm)"
  [[ $MATERIALIZED -eq 1 ]] && mode_label="materialized (real files, CI-leg parity)"

  echo "────────────────────────────────────────────────────────────────"
  echo "  WARM — $label"
  echo "  nub linking: $mode_label"
  echo "  (CAS store + lockfile warm; node_modules wiped between runs;"
  echo "   teardown via rename-aside, EXCLUDED from timing)"
  echo "────────────────────────────────────────────────────────────────"

  # Set up working dirs
  setup_workdir "$fixture" "$WD_PNPM" pnpm
  setup_workdir "$fixture" "$WD_NUB" nub

  # Pre-populate pnpm store (run once, discard result)
  echo "[setup] Pre-populating pnpm store..."
  pnpm install --frozen-lockfile --dir "$WD_PNPM" --silent 2>/dev/null \
    || pnpm install --frozen-lockfile --dir "$WD_PNPM" 2>&1 | tail -2

  # Pre-populate nub store (pinned GVS state)
  echo "[setup] Pre-populating nub CAS store + GVS..."
  nub_install "$WD_NUB" -s 2>/dev/null \
    || nub_install "$WD_NUB" 2>&1 | tail -2

  local outfile_nub="$RESULTS_DIR/warm-${fixture}-${TIMESTAMP}.json"

  # nub install command, GVS pinned via CI env. Single-quote-safe.
  local nub_cmd
  if [[ $MATERIALIZED -eq 1 ]]; then
    nub_cmd="CI=1 '$NUB' install --frozen-lockfile --cwd '$WD_NUB' -s"
  else
    nub_cmd="env -u CI '$NUB' install --frozen-lockfile --cwd '$WD_NUB' -s"
  fi

  # Run nub and pnpm together for direct comparison. Per-command --prepare so
  # each tool's (untimed) teardown is isolated and rename-aside fast.
  local HYPERFINE_ARGS=(
    --warmup 3
    --runs 12
    --prepare "$(reset_cmd "$WD_PNPM")"
    --command-name "pnpm install"
    "pnpm install --frozen-lockfile --dir '$WD_PNPM' --silent"
    --prepare "$(reset_cmd "$WD_NUB")"
    --command-name "nub install"
    "$nub_cmd"
  )

  # npm leg: npm ci --offline. Requires package-lock.json; npm has no symlink
  # mode (always materialized) and does not speak the `workspace:*` protocol, so
  # the monorepo fixture is skipped for npm.
  local has_npm_lock=0
  if command -v npm &>/dev/null && [[ -f "$FIXTURE_DIR/$fixture/package-lock.json" ]]; then
    has_npm_lock=1
    setup_workdir "$fixture" "$WD_NPM" npm
    echo "[setup] Pre-populating npm cache..."
    ( cd "$WD_NPM" && npm ci --ignore-scripts 2>/dev/null ) \
      || ( cd "$WD_NPM" && npm ci --ignore-scripts 2>&1 | tail -2 )
    # npm ci ignores --prefix/--dir; it installs in the process cwd, so cd in.
    HYPERFINE_ARGS+=(
      --prepare "$(reset_cmd "$WD_NPM")"
      --command-name "npm ci (offline)"
      "cd '$WD_NPM' && npm ci --offline --ignore-scripts"
    )
  fi

  # Add bun if available and fixture has bun.lock
  if [[ $HAS_BUN -eq 1 && -f "$FIXTURE_DIR/$fixture/bun.lock" ]]; then
    setup_workdir "$fixture" "$WD_BUN" bun
    echo "[setup] Pre-populating bun cache..."
    bun install --frozen-lockfile --cwd "$WD_BUN" 2>/dev/null \
      || bun install --frozen-lockfile --cwd "$WD_BUN" 2>&1 | tail -2
    HYPERFINE_ARGS+=(
      --prepare "$(reset_cmd "$WD_BUN")"
      --command-name "bun install (ref)"
      "bun install --frozen-lockfile --cwd '$WD_BUN'"
    )
  fi

  # Transparency: report each tool's teardown cost (rename-aside) once, BEFORE
  # the timed runs consume the populated node_modules. These are NOT part of any
  # timed number; they document why rename-aside matters for wall-clock.
  echo "  [teardown cost — untimed, reported for transparency]"
  report_teardown_cost "nub"  "$WD_NUB"
  report_teardown_cost "pnpm" "$WD_PNPM"
  [[ $has_npm_lock -eq 1 ]] && report_teardown_cost "npm" "$WD_NPM"
  [[ $HAS_BUN -eq 1 && -f "$FIXTURE_DIR/$fixture/bun.lock" ]] && report_teardown_cost "bun" "$WD_BUN"
  # Re-populate node_modules that report_teardown_cost just renamed away, so the
  # first --warmup run starts from a consistent state (hyperfine's --prepare will
  # reset anyway, but keep the warmup honest).
  nub_install "$WD_NUB" -s 2>/dev/null || true

  hyperfine "${HYPERFINE_ARGS[@]}" --export-json "$outfile_nub"

  echo ""
  echo "  [results saved → $outfile_nub]"
  echo ""

  # Move workdirs aside fast; reap in background (materialized dirs are large).
  for wd in "$WD_PNPM" "$WD_NUB" "$WD_BUN" "$WD_NPM"; do
    [[ -e "$wd" ]] && mv "$wd" "$TRASH_DIR/wd-$(basename "$wd")-$RANDOM" 2>/dev/null || true
  done
  rm -rf "$TRASH_DIR"/wd-* 2>/dev/null & true
}

# ── Cold benchmark ──────────────────────────────────────────────────────────
# Each run uses completely fresh stores. We manage the loop manually since
# hyperfine's --prepare can't create per-run isolated directories portably.

compute_stats_ms() {
  # args: list of integer millisecond values
  local arr=("$@")
  local n=${#arr[@]}
  local sum=0
  for v in "${arr[@]}"; do sum=$((sum + v)); done
  local mean=$((sum / n))
  local sq_sum=0
  for v in "${arr[@]}"; do
    local diff=$(( v - mean ))
    sq_sum=$(( sq_sum + diff * diff ))
  done
  local variance=$(( sq_sum / n ))
  local stddev=0
  if [[ $variance -gt 0 ]]; then
    local x=$variance
    local y=$(( (x + 1) / 2 ))
    while [[ $y -lt $x ]]; do x=$y; y=$(( (variance / y + y) / 2 )); done
    stddev=$x
  fi
  echo "${mean} ${stddev}"
}

run_cold() {
  local fixture="$1"
  local label="$2"
  local RUNS=5

  echo "────────────────────────────────────────────────────────────────"
  echo "  COLD — $label  (empty stores cleared between each run)"
  echo "────────────────────────────────────────────────────────────────"

  local pnpm_times=()
  local nub_times=()
  local bun_times=()
  local has_bun_lock=0
  [[ $HAS_BUN -eq 1 && -f "$FIXTURE_DIR/$fixture/bun.lock" ]] && has_bun_lock=1

  # Portable millisecond timer: use perl (available on macOS and Linux)
  ms_now() { perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000'; }

  echo "[pnpm cold — $RUNS runs]"
  for i in $(seq 1 $RUNS); do
    local store="/tmp/bench-cold-pnpm-store-$$-$i"
    local wd="/tmp/bench-cold-pnpm-wd-$$-$i"
    setup_workdir "$fixture" "$wd" pnpm
    mkdir -p "$store"
    local t0; t0=$(ms_now)
    pnpm install \
      --frozen-lockfile \
      --dir "$wd" \
      --silent \
      --store-dir "$store" \
      2>/dev/null \
      || pnpm install \
           --frozen-lockfile \
           --dir "$wd" \
           --store-dir "$store" \
           2>&1 | tail -2
    local t1; t1=$(ms_now)
    local ms=$(( t1 - t0 ))
    pnpm_times+=("$ms")
    echo "  run $i: ${ms}ms"
    rm -rf "$wd" "$store"
  done

  echo "[nub cold — $RUNS runs]"
  for i in $(seq 1 $RUNS); do
    # XDG_DATA_HOME controls the CAS store; XDG_CACHE_HOME controls the packument/engine cache.
    local data="/tmp/bench-cold-nub-data-$$-$i"
    local cache="/tmp/bench-cold-nub-cache-$$-$i"
    local wd="/tmp/bench-cold-nub-wd-$$-$i"
    setup_workdir "$fixture" "$wd" nub
    mkdir -p "$data" "$cache"
    local t0; t0=$(ms_now)
    XDG_DATA_HOME="$data" XDG_CACHE_HOME="$cache" \
      NPM_CONFIG_USERCONFIG="$EMPTY_NPMRC" NPM_CONFIG_GLOBALCONFIG="$EMPTY_NPMRC" \
      "$NUB" install \
      --frozen-lockfile \
      --cwd "$wd" \
      -s \
      2>/dev/null \
      || XDG_DATA_HOME="$data" XDG_CACHE_HOME="$cache" \
           NPM_CONFIG_USERCONFIG="$EMPTY_NPMRC" NPM_CONFIG_GLOBALCONFIG="$EMPTY_NPMRC" \
           "$NUB" install \
           --frozen-lockfile \
           --cwd "$wd" \
           2>&1 | tail -2
    local t1; t1=$(ms_now)
    local ms=$(( t1 - t0 ))
    nub_times+=("$ms")
    echo "  run $i: ${ms}ms"
    rm -rf "$wd" "$data" "$cache"
  done

  if [[ $has_bun_lock -eq 1 ]]; then
    echo "[bun cold — $RUNS runs]"
    for i in $(seq 1 $RUNS); do
      local bun_cache="/tmp/bench-cold-bun-cache-$$-$i"
      local wd="/tmp/bench-cold-bun-wd-$$-$i"
      setup_workdir "$fixture" "$wd" bun
      mkdir -p "$bun_cache"
      local t0; t0=$(ms_now)
      BUN_INSTALL_CACHE_DIR="$bun_cache" bun install --frozen-lockfile --cwd "$wd" 2>/dev/null \
        || BUN_INSTALL_CACHE_DIR="$bun_cache" bun install --frozen-lockfile --cwd "$wd" 2>&1 | tail -2
      local t1; t1=$(ms_now)
      local ms=$(( t1 - t0 ))
      bun_times+=("$ms")
      echo "  run $i: ${ms}ms"
      rm -rf "$wd" "$bun_cache"
    done
  fi

  read -r pnpm_mean pnpm_sd <<< "$(compute_stats_ms "${pnpm_times[@]}")"
  read -r nub_mean  nub_sd  <<< "$(compute_stats_ms "${nub_times[@]}")"

  echo ""
  echo "  COLD summary — $label:"
  printf "    pnpm: %dms ± %dms\n" "$pnpm_mean" "$pnpm_sd"
  printf "    nub:  %dms ± %dms\n" "$nub_mean"  "$nub_sd"
  if [[ $has_bun_lock -eq 1 ]]; then
    read -r bun_mean bun_sd <<< "$(compute_stats_ms "${bun_times[@]}")"
    printf "    bun:  %dms ± %dms (reference)\n" "$bun_mean" "$bun_sd"
  fi
  echo ""

  local outfile="$RESULTS_DIR/cold-${fixture}-${TIMESTAMP}.json"
  local bun_json='null'
  if [[ $has_bun_lock -eq 1 ]]; then
    read -r bun_mean bun_sd <<< "$(compute_stats_ms "${bun_times[@]}")"
    bun_json="$(printf '{"times_ms":[%s],"mean_ms":%d,"stddev_ms":%d}' \
      "$(IFS=,; echo "${bun_times[*]}")" "$bun_mean" "$bun_sd")"
  fi
  printf '{"scenario":"cold","fixture":"%s","runs":%d,"pnpm":{"times_ms":[%s],"mean_ms":%d,"stddev_ms":%d},"nub":{"times_ms":[%s],"mean_ms":%d,"stddev_ms":%d},"bun":%s}\n' \
    "$fixture" "$RUNS" \
    "$(IFS=,; echo "${pnpm_times[*]}")" "$pnpm_mean" "$pnpm_sd" \
    "$(IFS=,; echo "${nub_times[*]}")"  "$nub_mean"  "$nub_sd" \
    "$bun_json" \
    > "$outfile"
  echo "  [results saved → $outfile]"
  echo ""
}

# ── Fixture registry ────────────────────────────────────────────────────────
# Each entry: "name|label"
ALL_FIXTURES=(
  "simple|simple (~435 pkgs)"
  "monorepo|monorepo (~407 pkgs, 4 workspaces)"
  "t3|t3-app (~222 pkgs, Next16/tRPC11/Drizzle — Bun's benchmark fixture)"
  "large|large (~1168 pkgs, react+MUI+webpack+babel+ts+eslint)"
)

# ── Run benchmarks ──────────────────────────────────────────────────────────

if [[ $RUN_WARM -eq 1 ]]; then
  echo ""
  echo "════════════════════════════════════════════════════════════════"
  echo "  WARM benchmarks (stores pre-populated, node_modules deleted)"
  echo "════════════════════════════════════════════════════════════════"
  for entry in "${ALL_FIXTURES[@]}"; do
    IFS='|' read -r name label <<< "$entry"
    [[ -n "$FIXTURE_FILTER" && "$name" != "$FIXTURE_FILTER" ]] && continue
    run_warm "$name" "$label"
  done
fi

if [[ $RUN_COLD -eq 1 ]]; then
  echo ""
  echo "════════════════════════════════════════════════════════════════"
  echo "  COLD benchmarks (empty stores, cleared between each run)"
  echo "════════════════════════════════════════════════════════════════"
  for entry in "${ALL_FIXTURES[@]}"; do
    IFS='|' read -r name label <<< "$entry"
    [[ -n "$FIXTURE_FILTER" && "$name" != "$FIXTURE_FILTER" ]] && continue
    run_cold "$name" "$label"
  done
fi

echo "================================================================"
echo "  All benchmarks complete. Results: $RESULTS_DIR/"
echo "================================================================"
