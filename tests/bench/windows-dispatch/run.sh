#!/usr/bin/env bash
# Windows dispatch benchmark — the same nub command reached two ways from cmd.exe.
#
# Every `nub` call from cmd.exe used to be cmd.exe -> nub.cmd (npm's generated shim)
# -> node -> spawn nub.exe, and the node boot is most of the cost. Since 0.7.0 the
# launcher hardlinks a real nub.exe beside the shim (npm/nub/bin/launch.js,
# healWindowsBinDir), which PATHEXT resolves ahead of nub.cmd, so cmd.exe reaches the
# binary directly. This measures that gap on a real Windows host.
#
# Each cell is one command timed by hyperfine under its default Windows shell
# (cmd.exe, whose own spawn cost hyperfine subtracts). The cells run ROUND-ROBIN,
# a few runs each, for several rounds, so a drift on the box lands on every cell
# rather than on whichever ran last. Two commands are timed through both entry
# points; two more are controls:
#
#   version/cmd   nub.cmd --version      the shim path
#   version/exe   nub.exe --version      the direct path
#   run/cmd       nub.cmd run noop       a script run through the shim
#   run/exe       nub.exe run noop       the same through the .exe
#   bare          nub --version          what a user types; must land on version/exe,
#                                        which proves PATHEXT really picks the .exe
#   node          node --version         plain Node's own boot, for scale
#
# Windows only, under Git Bash. Needs hyperfine, jq, and @nubjs/nub installed
# globally with npm. The workflow of the same name runs it on windows-latest.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
case "$(uname -s)" in MINGW*|MSYS*) ;; *) echo "ERROR: Windows only (Git Bash); uname says $(uname -s)" >&2; exit 1 ;; esac
for tool in hyperfine jq npm cygpath; do command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: $tool not found" >&2; exit 1; }; done

ROUNDS=4
RUNS=10
WARMUP=3
SAVE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rounds) ROUNDS="$2"; shift 2 ;;
    --runs)   RUNS="$2";   shift 2 ;;
    --warmup) WARMUP="$2"; shift 2 ;;
    --save)   SAVE=1; shift ;;
    *) echo "Unknown arg: $1" >&2; exit 1 ;;
  esac
done
if [[ "$SAVE" -eq 1 ]]; then RESULTS_DIR="$REPO_ROOT/tests/bench/windows-dispatch/results"; else RESULTS_DIR="$(mktemp -d)"; fi
mkdir -p "$RESULTS_DIR"

# On Windows npm's global bin dir IS the prefix (no bin/ subdirectory).
BIN="$(cygpath -u "$(npm prefix -g)")"
BINW="$(cygpath -w "$BIN")"
[[ -f "$BIN/nub.cmd" ]] || { echo "ERROR: no nub.cmd in $BIN — run: npm install -g @nubjs/nub" >&2; exit 1; }
# The commands below name the shim and the .exe by unquoted absolute path, so that
# what cmd.exe receives is exactly what a shell prompt would; a prefix with a space
# would need quoting whose rules differ between cmd.exe and the spawner.
case "$BINW" in *" "*) echo "ERROR: npm prefix '$BINW' contains a space; set a plain prefix (npm config set prefix C:\\npm)" >&2; exit 1 ;; esac

# Start from the shim-only state and let the launcher heal it: the first call that
# goes through nub.cmd is what drops nub.exe beside it. Going through cmd.exe, not
# bash, so the shim path is the one exercised.
rm -f "$BIN/nub.exe"
MSYS_NO_PATHCONV=1 cmd.exe /C "nub --version" >/dev/null
[[ -f "$BIN/nub.exe" ]] || { echo "ERROR: the launcher did not place nub.exe beside nub.cmd in $BIN" >&2; exit 1; }
[[ -f "$BIN/nub-sh/busybox.exe" ]] || { echo "ERROR: the launcher did not stage nub-sh/busybox.exe in $BIN" >&2; exit 1; }

# A project with one script that spawns nothing, so `nub run` times the runner and
# its shell, not a payload. Installed once so the freshness check has a tree to read.
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT
printf '{ "name": "windows-dispatch-bench", "version": "1.0.0", "private": true, "scripts": { "noop": "exit 0" } }\n' > "$FIXTURE/package.json"
( cd "$FIXTURE" && MSYS_NO_PATHCONV=1 cmd.exe /C "nub install" >/dev/null )

declare -A CMD=(
  [version/cmd]="$BINW\\nub.cmd --version"
  [version/exe]="$BINW\\nub.exe --version"
  [run/cmd]="$BINW\\nub.cmd run noop"
  [run/exe]="$BINW\\nub.exe run noop"
  [bare]="nub --version"
  [node]="node --version"
)
ORDER=(version/cmd version/exe run/cmd run/exe bare node)

# Every cell must work before anything is timed; a non-zero exit inside hyperfine
# would be counted as a (fast) run.
for cell in "${ORDER[@]}"; do
  ( cd "$FIXTURE" && MSYS_NO_PATHCONV=1 cmd.exe /C "${CMD[$cell]}" >/dev/null 2>&1 ) || { echo "ERROR: smoke run failed for $cell: ${CMD[$cell]}" >&2; exit 1; }
done

NUB_VERSION="$(MSYS_NO_PATHCONV=1 cmd.exe /C "nub --version" 2>/dev/null | tr -d '\r' | head -1)"
NODE_VERSION="$(node --version | tr -d '\r')"
HF_VERSION="$(hyperfine --version | tr -d '\r' | awk '{print $2}')"
CPU="$(powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor).Name' 2>/dev/null | tr -d '\r' | head -1 | sed 's/ *$//')"
OS="$(powershell.exe -NoProfile -Command '[System.Environment]::OSVersion.VersionString' 2>/dev/null | tr -d '\r' | head -1)"
DATE_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "================================================================"
echo "  Windows dispatch benchmark — nub.cmd (npm shim) vs nub.exe, from cmd.exe"
echo "  nub: $NUB_VERSION   node: $NODE_VERSION   hyperfine: $HF_VERSION"
echo "  host: ${ImageOS:-?} ${ImageVersion:-} · $OS · $CPU · ${NUMBER_OF_PROCESSORS:-?} logical cores"
echo "  bin dir: $BINW"
echo "  rounds: $ROUNDS × runs: $RUNS per cell (warmup $WARMUP each), round-robin"
echo "  date: $DATE_UTC"
echo "================================================================"

TMP="$(mktemp -d)"
for ((round = 1; round <= ROUNDS; round++)); do
  for cell in "${ORDER[@]}"; do
    safe="${cell//\//-}"
    ( cd "$FIXTURE" && hyperfine --warmup "$WARMUP" --runs "$RUNS" --style none \
        --export-json "$TMP/$safe-$round.json" "${CMD[$cell]}" >/dev/null )
    echo "round $round  $cell  mean $(jq -r '.results[0].mean * 1000 | . * 10 | round / 10' "$TMP/$safe-$round.json") ms"
  done
done

# One object per cell: every per-run time across rounds, plus the statistics.
for cell in "${ORDER[@]}"; do
  safe="${cell//\//-}"
  jq -s --arg key "$cell" --arg command "${CMD[$cell]}" '
    ([.[].results[0].times[]] | sort) as $t
    | { key: $key, value: {
          command: $command,
          times: [.[].results[0].times[]],
          mean: ($t | add / length),
          min: $t[0],
          median: $t[($t | length / 2 | floor)],
          max: $t[-1],
          runs: ($t | length)
      } }' "$TMP/$safe"-*.json > "$TMP/$safe.cell.json"
done

OUT="$RESULTS_DIR/$(date -u +%Y-%m-%d)-windows-dispatch.json"
jq -n \
  --arg benchmark "windows-dispatch" \
  --arg date "$DATE_UTC" \
  --arg nub "$NUB_VERSION" --arg node "$NODE_VERSION" --arg hyperfine "$HF_VERSION" \
  --arg image "${ImageOS:-} ${ImageVersion:-}" --arg os "$OS" --arg cpu "$CPU" \
  --argjson cores "${NUMBER_OF_PROCESSORS:-0}" \
  --argjson rounds "$ROUNDS" --argjson runs "$RUNS" --argjson warmup "$WARMUP" \
  --arg binDir "$BINW" \
  '{
    benchmark: $benchmark, date: $date,
    versions: { nub: $nub, node: $node, hyperfine: $hyperfine },
    host: { image: ($image | gsub("^ +| +$"; "")), os: $os, cpu: $cpu, logicalCores: $cores },
    method: {
      shell: "cmd.exe (hyperfine default on Windows; its spawn cost is subtracted)",
      order: "round-robin over the cells, rounds × runs per cell",
      rounds: $rounds, runsPerRound: $runs, warmupPerRound: $warmup,
      binDir: $binDir, unit: "seconds"
    },
    cells: (reduce inputs as $c ({}; . + { ($c.key): $c.value }))
  }' "$TMP"/*.cell.json > "$OUT"

ms() { jq -r --arg c "$1" --arg s "$2" '.cells[$c][$s] * 1000 | . * 10 | round / 10' "$OUT"; }
echo
echo "cell           mean      min     median  (ms)"
for cell in "${ORDER[@]}"; do
  printf 'ROW {"cell":"%s","mean_ms":%s,"min_ms":%s,"median_ms":%s}\n' "$cell" "$(ms "$cell" mean)" "$(ms "$cell" min)" "$(ms "$cell" median)"
done
echo
echo "nub --version  via nub.cmd $(ms version/cmd mean) ms → via nub.exe $(ms version/exe mean) ms  ($(jq -r '.cells["version/cmd"].mean / .cells["version/exe"].mean * 10 | round / 10' "$OUT")×)"
echo "nub run noop   via nub.cmd $(ms run/cmd mean) ms → via nub.exe $(ms run/exe mean) ms  ($(jq -r '.cells["run/cmd"].mean / .cells["run/exe"].mean * 10 | round / 10' "$OUT")×)"
echo "control: bare 'nub --version' $(ms bare mean) ms · node --version $(ms node mean) ms"
echo "saved: $OUT"

# The control decides whether the run means anything: a bare `nub` must land on the
# .exe number, not the .cmd one. If it does not, PATHEXT did not pick the .exe on this
# host and every ratio above is a comparison nobody experiences.
jq -e '(.cells.bare.mean - .cells["version/exe"].mean | fabs) < (.cells.bare.mean - .cells["version/cmd"].mean | fabs)' "$OUT" >/dev/null \
  || { echo "ERROR: control failed — bare 'nub --version' timed closer to nub.cmd than to nub.exe" >&2; exit 1; }
jq -e '.cells["version/cmd"].mean > .cells["version/exe"].mean' "$OUT" >/dev/null \
  || { echo "ERROR: control failed — the shim path was not slower than the .exe path" >&2; exit 1; }
