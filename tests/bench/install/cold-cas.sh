#!/usr/bin/env bash
# Cold-CAS install benchmark for nub's package manager.
#
# Measures the un-benched fetch → gzip-decode → tar-unpack → CAS-store-write →
# link-into-node_modules path against a HERMETIC registry, with the
# content-addressed store reset to COLD before each timed run. This is the path
# OPP-2 (eager clonedir tree-build on macOS) and OPP-5 (CAS rayon chunk size for
# fat packages) target, and the gate for measuring those changes.
#
# Two things are reported per fixture:
#   • install wall-clock (hyperfine, median ± σ over N runs)
#   • peak RSS (a separate `/usr/bin/time -l` run; macOS reports "maximum
#     resident set size" + "peak memory footprint", Linux `-v` reports
#     "Maximum resident set size")
#
# COLD vs WARM:
#   COLD — XDG_DATA_HOME (CAS store at $XDG_DATA_HOME/nub/store/v1/files) and
#          XDG_CACHE_HOME (packument/index cache at $XDG_CACHE_HOME/nub/pm) both
#          point at FRESH empty dirs for the timed run, so the install pays the
#          full fetch+decode+unpack+link cost. hyperfine's --prepare wipes and
#          re-creates those dirs before EACH timed run (excluded from timing).
#   WARM — the store + cache are populated once, then REUSED across runs (only
#          node_modules is wiped between runs). Contrast number for the
#          relink-against-warm-store path.
#
# HERMETIC REGISTRY:
#   Sources vendor/aube/benchmarks/hermetic.bash, which brings up a no-uplink
#   Verdaccio on port 4874 (BENCH_VERDACCIO_PORT) serving a one-time-warmed
#   local storage so every tarball fetch hits localhost, not npmjs. nub is
#   pointed at it via `nub install --registry $BENCH_REGISTRY_URL`. Falls back
#   to the public npm registry (clearly labelled) if --no-hermetic is passed or
#   Verdaccio can't start.
#
# Usage:
#   bash tests/bench/install/cold-cas.sh [options]
#     --fixture <name>     simple | fat   (default: both)
#     --cold-only          skip the warm contrast leg
#     --warm-only          skip the cold leg
#     --runs <n>           timed runs per leg (default: cold 5, warm 8)
#     --no-hermetic        skip Verdaccio; install against the public npm
#                          registry (numbers carry CDN jitter — labelled)
#     --no-rss             skip the peak-RSS measurement pass
#     --save               write JSON results under results/ (default: temp dir)
#
# Requires: hyperfine, /usr/bin/time; target/release/nub built. Verdaccio is
# auto-installed by hermetic.bash on first hermetic run (npm i -g verdaccio@6).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
NUB="${NUB:-$REPO_ROOT/target/release/nub}"
FIXTURE_DIR="$REPO_ROOT/tests/bench/install/fixtures"
HERMETIC_BASH="$REPO_ROOT/vendor/aube/benchmarks/hermetic.bash"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"

RUN_COLD=1
RUN_WARM=1
FIXTURE_FILTER=""
COLD_RUNS=5
WARM_RUNS=8
USE_HERMETIC=1
MEASURE_RSS=1
SAVE_RESULTS=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cold-only)   RUN_WARM=0 ;;
    --warm-only)   RUN_COLD=0 ;;
    --fixture)     shift; FIXTURE_FILTER="${1:-}" ;;
    --runs)        shift; COLD_RUNS="${1:-5}"; WARM_RUNS="${1:-8}" ;;
    --no-hermetic) USE_HERMETIC=0 ;;
    --no-rss)      MEASURE_RSS=0 ;;
    --save)        SAVE_RESULTS=1 ;;
    *) echo "WARN: unknown arg '$1'" >&2 ;;
  esac
  shift
done

if [[ "$SAVE_RESULTS" -eq 1 ]]; then
  RESULTS_DIR="$REPO_ROOT/tests/bench/install/results"
else
  RESULTS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nub-coldcas-results-XXXXXX")"
fi
mkdir -p "$RESULTS_DIR"

# Scratch root for per-run isolated XDG dirs + workdirs. Reaped on exit.
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/nub-coldcas-$$-XXXXXX")"
cleanup() {
  [[ -n "${HERMETIC_BASH:-}" && "$USE_HERMETIC" -eq 1 ]] && hermetic_stop 2>/dev/null || true
  rm -rf "$SCRATCH" 2>/dev/null || true
}
trap cleanup EXIT

# Empty npmrc so a host ~/.npmrc (custom registry=, https-proxy=, etc.) can't
# route tarball fetches away from the hermetic Verdaccio and break the run.
EMPTY_NPMRC="$SCRATCH/empty.npmrc"
touch "$EMPTY_NPMRC"
export NPM_CONFIG_USERCONFIG="$EMPTY_NPMRC"
export NPM_CONFIG_GLOBALCONFIG="$EMPTY_NPMRC"

# ── Preflight ────────────────────────────────────────────────────────────────
if [[ ! -x "$NUB" ]]; then
  echo "ERROR: $NUB not found or not executable. Run 'cargo build --release' first." >&2
  exit 1
fi
if ! command -v hyperfine &>/dev/null; then
  echo "ERROR: hyperfine not found. Install with: brew install hyperfine" >&2
  exit 1
fi

# Peak-RSS measurement tool. macOS /usr/bin/time -l, GNU /usr/bin/time -v.
# Capture the probe output into a var first — piping into `grep -q` would
# SIGPIPE /usr/bin/time and, under `set -o pipefail`, misreport the probe.
RSS_KIND=""
if [[ "$MEASURE_RSS" -eq 1 ]]; then
  _probe_l="$(/usr/bin/time -l true 2>&1 || true)"
  _probe_v="$(/usr/bin/time -v true 2>&1 || true)"
  if [[ "$_probe_l" == *"maximum resident set size"* ]]; then
    RSS_KIND="macos"
  elif [[ "$_probe_v" == *"Maximum resident set size"* ]]; then
    RSS_KIND="gnu"
  else
    echo "WARN: no /usr/bin/time peak-RSS support detected; disabling RSS pass." >&2
    MEASURE_RSS=0
  fi
fi

# `nub --version | head -1` would SIGPIPE nub when head closes the pipe early;
# under `set -o pipefail` that non-zero status kills the script. Read the first
# line without a pipe instead.
NUB_VERSION="$({ "$NUB" --version 2>&1 || true; } | { IFS= read -r l; echo "$l"; })"

# ── Hermetic registry ────────────────────────────────────────────────────────
REGISTRY_LABEL=""
REGISTRY_ARGS=()
if [[ "$USE_HERMETIC" -eq 1 ]]; then
  if [[ ! -f "$HERMETIC_BASH" ]]; then
    echo "WARN: $HERMETIC_BASH not found; falling back to public registry." >&2
    USE_HERMETIC=0
  else
    # hermetic.bash resolves its own dir from BASH_SOURCE; SCRIPT_DIR helps it.
    SCRIPT_DIR="$(dirname "$HERMETIC_BASH")"
    # Scope the one-time registry warm to the tools nub actually needs cached
    # (pnpm-family resolution). The default aube warm also runs yarn/deno/bun
    # legs; those are slow and, in some host setups (e.g. a package-manager
    # shim that prompts), can stall the warm. nub only needs the npm/pnpm/aube
    # tarballs, and populate_registry pulls each fixture's exact tarballs via
    # uplink regardless, so dropping yarn/deno/bun here is safe. Override with
    # BENCH_TOOLS=... in the environment if you want the full warm set.
    export BENCH_TOOLS="${BENCH_TOOLS:-aube,pnpm,npm}"
    # shellcheck disable=SC1090
    source "$HERMETIC_BASH"
    export AUBE_BIN="$NUB"   # warm step skips aube if unset; nub serves the same role
    echo "[hermetic] bringing up Verdaccio (port ${BENCH_VERDACCIO_PORT:-4874})..." >&2
    if hermetic_start; then
      REGISTRY_LABEL="hermetic Verdaccio ($BENCH_REGISTRY_URL)"
      REGISTRY_ARGS=(--registry "$BENCH_REGISTRY_URL")
    else
      echo "WARN: hermetic_start failed; falling back to public registry." >&2
      USE_HERMETIC=0
    fi
  fi
fi
if [[ "$USE_HERMETIC" -eq 0 ]]; then
  REGISTRY_LABEL="public npm registry (registry.npmjs.org — CDN jitter present)"
fi

echo "================================================================"
echo "  Cold-CAS install benchmark — nub package manager"
echo "  nub:      $NUB_VERSION  ($NUB)"
echo "  registry: $REGISTRY_LABEL"
echo "  RSS:      $([[ $MEASURE_RSS -eq 1 ]] && echo "$RSS_KIND /usr/bin/time" || echo "disabled")"
echo "  date:     $(date)"
echo "================================================================"
echo ""

# ── Helpers ──────────────────────────────────────────────────────────────────

# Copy a fixture to a fresh workdir, stripping foreign lockfiles so nub installs
# from its own pnpm-lock family without a competing-lockfile refusal.
setup_workdir() {
  local fixture="$1" workdir="$2"
  rm -rf "$workdir"
  cp -r "$FIXTURE_DIR/$fixture" "$workdir"
  rm -rf "$workdir/node_modules" 2>/dev/null || true
  rm -f "$workdir/bun.lock" "$workdir/bun.lockb" "$workdir/package-lock.json" 2>/dev/null || true
}

# Pre-populate the hermetic Verdaccio storage with THIS fixture's own tarballs.
#
# hermetic.bash's one-time warm only caches the packages in aube's
# benchmarks/fixture.package.json — not our `simple`/`fat` fixtures. Against the
# no-uplink (cold) config those tarballs would 404. So before the timed loop we
# swap Verdaccio to the warm (uplink-enabled) config, run a throwaway nub install
# (its own scratch store, discarded) to pull+cache the fixture's tarballs from
# npmjs into Verdaccio's storage, then swap back to no-uplink. Subsequent timed
# installs are then fully offline against localhost. Mirrors bench.sh's per-tool
# populate step (hermetic_use_warm_uplink → install → hermetic_use_no_uplink).
populate_registry() {
  local fixture="$1"
  [[ "$USE_HERMETIC" -eq 1 ]] || return 0
  echo "[populate] caching $fixture tarballs into Verdaccio (uplink on)..." >&2
  hermetic_use_warm_uplink
  local pwd_data="$SCRATCH/populate-data-$fixture"
  local pwd_cache="$SCRATCH/populate-cache-$fixture"
  local pwd_wd="$SCRATCH/populate-wd-$fixture"
  mkdir -p "$pwd_data" "$pwd_cache"
  setup_workdir "$fixture" "$pwd_wd"
  if ! CI=1 XDG_DATA_HOME="$pwd_data" XDG_CACHE_HOME="$pwd_cache" \
      NPM_CONFIG_USERCONFIG="$EMPTY_NPMRC" NPM_CONFIG_GLOBALCONFIG="$EMPTY_NPMRC" \
      "$NUB" install --frozen-lockfile --dir "$pwd_wd" --registry "$BENCH_REGISTRY_URL" -s \
      >"$SCRATCH/populate-$fixture.log" 2>&1; then
    echo "  WARN: populate install for $fixture had errors (see $SCRATCH/populate-$fixture.log)" >&2
    tail -5 "$SCRATCH/populate-$fixture.log" >&2 || true
  fi
  rm -rf "$pwd_data" "$pwd_cache" "$pwd_wd"
  hermetic_use_no_uplink
  echo "[populate] $fixture cached; Verdaccio back to no-uplink." >&2
}

# nub install invocation, GVS pinned OFF via CI=1 so the timed path is the
# materialized fetch+decode+unpack+link path the CAS work lives on (GVS-on would
# relink from the warm GVS and hide the unpack cost we want cold). --frozen-lockfile
# installs strictly from the committed lockfile (deterministic resolution).
# NPM_CONFIG_USERCONFIG/GLOBALCONFIG are inherited from the exported env (set
# above), so no need to re-specify here; listed explicitly for documentation.
nub_install() {
  local data="$1" cache="$2" wd="$3"; shift 3
  CI=1 XDG_DATA_HOME="$data" XDG_CACHE_HOME="$cache" \
    "$NUB" install --frozen-lockfile --dir "$wd" "${REGISTRY_ARGS[@]+${REGISTRY_ARGS[@]}}" -s "$@"
}

# Portable millisecond timer (perl is on macOS + Linux).
ms_now() { perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000'; }

# Parse peak RSS (bytes) out of a /usr/bin/time log file.
parse_peak_rss_bytes() {
  local logf="$1"
  if [[ "$RSS_KIND" == "macos" ]]; then
    # macOS reports bytes for "maximum resident set size".
    grep "maximum resident set size" "$logf" | awk '{print $1}'
  else
    # GNU reports kilobytes for "Maximum resident set size (kbytes)".
    local kb
    kb="$(grep "Maximum resident set size" "$logf" | awk -F': ' '{print $2}')"
    echo $(( kb * 1024 ))
  fi
}

human_mb() { awk "BEGIN{printf \"%.1f\", $1/1048576}"; }

# ── COLD benchmark ───────────────────────────────────────────────────────────
# Each timed run gets a fresh empty store + cache (cold CAS). hyperfine's
# --prepare wipes+recreates them before each run and is EXCLUDED from timing.
run_cold() {
  local fixture="$1" label="$2"
  echo "────────────────────────────────────────────────────────────────"
  echo "  COLD-CAS — $label"
  echo "  (empty CAS store + cache per run; full fetch+decode+unpack+link)"
  echo "────────────────────────────────────────────────────────────────"

  local wd="$SCRATCH/cold-$fixture"
  local data="$SCRATCH/cold-data-$fixture"
  local cache="$SCRATCH/cold-cache-$fixture"
  setup_workdir "$fixture" "$wd"

  # --prepare runs before EACH timed run (untimed): wipe node_modules + the cold
  # store/cache so the timed install starts fully cold every iteration.
  local prepare="rm -rf '$wd/node_modules' '$data' '$cache'; mkdir -p '$data' '$cache'"
  local reg_flag=""
  [[ ${#REGISTRY_ARGS[@]} -gt 0 ]] && reg_flag="${REGISTRY_ARGS[*]}"
  local cmd="CI=1 XDG_DATA_HOME='$data' XDG_CACHE_HOME='$cache' NPM_CONFIG_USERCONFIG='$EMPTY_NPMRC' NPM_CONFIG_GLOBALCONFIG='$EMPTY_NPMRC' '$NUB' install --frozen-lockfile --dir '$wd' $reg_flag -s"

  local outfile="$RESULTS_DIR/coldcas-${fixture}-${TIMESTAMP}.json"
  hyperfine \
    --warmup 0 \
    --runs "$COLD_RUNS" \
    --prepare "$prepare" \
    --command-name "nub install (cold-CAS, $fixture)" \
    "$cmd" \
    --export-json "$outfile"

  echo "  [wall-clock results → $outfile]"

  # Peak-RSS pass: one cold install under /usr/bin/time. Separate from hyperfine
  # because hyperfine does not capture RSS.
  if [[ "$MEASURE_RSS" -eq 1 ]]; then
    rm -rf "$wd/node_modules" "$data" "$cache"; mkdir -p "$data" "$cache"
    local rss_log="$SCRATCH/rss-cold-$fixture.log"
    if [[ "$RSS_KIND" == "macos" ]]; then
      CI=1 XDG_DATA_HOME="$data" XDG_CACHE_HOME="$cache" \
        /usr/bin/time -l "$NUB" install --frozen-lockfile --dir "$wd" "${REGISTRY_ARGS[@]+${REGISTRY_ARGS[@]}}" -s \
        >/dev/null 2>"$rss_log" || true
    else
      CI=1 XDG_DATA_HOME="$data" XDG_CACHE_HOME="$cache" \
        /usr/bin/time -v "$NUB" install --frozen-lockfile --dir "$wd" "${REGISTRY_ARGS[@]+${REGISTRY_ARGS[@]}}" -s \
        >/dev/null 2>"$rss_log" || true
    fi
    local rss_bytes; rss_bytes="$(parse_peak_rss_bytes "$rss_log")"
    if [[ -n "$rss_bytes" ]]; then
      printf "  peak RSS (cold install): %s MB (%s bytes)\n" "$(human_mb "$rss_bytes")" "$rss_bytes"
    fi
  fi
  echo ""
}

# ── WARM contrast benchmark ──────────────────────────────────────────────────
# Store + cache populated ONCE, then reused; only node_modules wiped between
# runs. The relink-against-warm-store contrast number.
run_warm() {
  local fixture="$1" label="$2"
  echo "────────────────────────────────────────────────────────────────"
  echo "  WARM (contrast) — $label"
  echo "  (CAS store + cache warm + reused; only node_modules wiped per run)"
  echo "────────────────────────────────────────────────────────────────"

  local wd="$SCRATCH/warm-$fixture"
  local data="$SCRATCH/warm-data-$fixture"
  local cache="$SCRATCH/warm-cache-$fixture"
  setup_workdir "$fixture" "$wd"
  mkdir -p "$data" "$cache"

  echo "[setup] warming CAS store + cache once..."
  nub_install "$data" "$cache" "$wd" 2>/dev/null \
    || nub_install "$data" "$cache" "$wd" 2>&1 | tail -3

  local prepare="rm -rf '$wd/node_modules'"
  local reg_flag=""
  [[ ${#REGISTRY_ARGS[@]} -gt 0 ]] && reg_flag="${REGISTRY_ARGS[*]}"
  local cmd="CI=1 XDG_DATA_HOME='$data' XDG_CACHE_HOME='$cache' NPM_CONFIG_USERCONFIG='$EMPTY_NPMRC' NPM_CONFIG_GLOBALCONFIG='$EMPTY_NPMRC' '$NUB' install --frozen-lockfile --dir '$wd' $reg_flag -s"

  local outfile="$RESULTS_DIR/warmcas-${fixture}-${TIMESTAMP}.json"
  hyperfine \
    --warmup 2 \
    --runs "$WARM_RUNS" \
    --prepare "$prepare" \
    --command-name "nub install (warm-store, $fixture)" \
    "$cmd" \
    --export-json "$outfile"

  echo "  [wall-clock results → $outfile]"

  if [[ "$MEASURE_RSS" -eq 1 ]]; then
    rm -rf "$wd/node_modules"
    local rss_log="$SCRATCH/rss-warm-$fixture.log"
    if [[ "$RSS_KIND" == "macos" ]]; then
      CI=1 XDG_DATA_HOME="$data" XDG_CACHE_HOME="$cache" \
        /usr/bin/time -l "$NUB" install --frozen-lockfile --dir "$wd" "${REGISTRY_ARGS[@]+${REGISTRY_ARGS[@]}}" -s \
        >/dev/null 2>"$rss_log" || true
    else
      CI=1 XDG_DATA_HOME="$data" XDG_CACHE_HOME="$cache" \
        /usr/bin/time -v "$NUB" install --frozen-lockfile --dir "$wd" "${REGISTRY_ARGS[@]+${REGISTRY_ARGS[@]}}" -s \
        >/dev/null 2>"$rss_log" || true
    fi
    local rss_bytes; rss_bytes="$(parse_peak_rss_bytes "$rss_log")"
    if [[ -n "$rss_bytes" ]]; then
      printf "  peak RSS (warm install): %s MB (%s bytes)\n" "$(human_mb "$rss_bytes")" "$rss_bytes"
    fi
  fi
  echo ""
}

# ── Fixture registry ─────────────────────────────────────────────────────────
# Each entry: "name|label"
#   simple — multi-dep clonedir tree-build (OPP-2: eager clonedir on macOS)
#   fat    — a few thousand-file packages (OPP-5: CAS rayon chunk size)
ALL_FIXTURES=(
  "simple|simple multi-dep (~435 pkgs — OPP-2 clonedir tree-build)"
  "fat|fat packages (next + typescript + @swc/core — OPP-5 CAS chunk size)"
)

# ── Run ──────────────────────────────────────────────────────────────────────
for entry in "${ALL_FIXTURES[@]}"; do
  IFS='|' read -r name label <<< "$entry"
  [[ -n "$FIXTURE_FILTER" && "$name" != "$FIXTURE_FILTER" ]] && continue
  if [[ ! -d "$FIXTURE_DIR/$name" ]]; then
    echo "WARN: fixture '$name' not found at $FIXTURE_DIR/$name — skipping." >&2
    continue
  fi
  if [[ ! -f "$FIXTURE_DIR/$name/pnpm-lock.yaml" ]]; then
    echo "WARN: fixture '$name' has no pnpm-lock.yaml — generate it first (see COLD-CAS.md). Skipping." >&2
    continue
  fi
  populate_registry "$name"
  [[ "$RUN_COLD" -eq 1 ]] && run_cold "$name" "$label"
  [[ "$RUN_WARM" -eq 1 ]] && run_warm "$name" "$label"
done

echo "================================================================"
echo "  Cold-CAS benchmark complete. Results: $RESULTS_DIR/"
echo "================================================================"
