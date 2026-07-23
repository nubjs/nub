#!/usr/bin/env bash
# One clean 4-way warm-install table: nub vs bun vs pnpm vs npm.
#
# Warm install = warm global store + frozen lockfile, node_modules wiped between
# runs (in hyperfine --prepare, excluded from timing), no network. This is the
# case the global store exists to make cheap. Each tool gets its own copy of the
# fixture so their node_modules/lockfiles never collide; each store is pre-warmed
# once (untimed) before the timed run.
#
# Usage:
#   cargo build --release -p nub-cli
#   bash tests/bench/install/run-4way.sh
#   NUB=/path/to/nub bash tests/bench/install/run-4way.sh --fixture tanstack-start --runs 12 --warmup 3
#
# Requires: hyperfine, pnpm, bun, npm on PATH; a release nub (NUB= or target/release/nub).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DEFAULT_NUB="$REPO_ROOT/target/release/nub"
NUB="${NUB:-$DEFAULT_NUB}"
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac

FIXTURE="tanstack-start"
RUNS=10
WARMUP=3
while [ $# -gt 0 ]; do
  case "$1" in
    --fixture) FIXTURE="$2"; shift 2 ;;
    --runs) RUNS="$2"; shift 2 ;;
    --warmup) WARMUP="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

FIX="$REPO_ROOT/tests/bench/install/fixtures/$FIXTURE"
[ -d "$FIX" ] || { echo "no fixture at $FIX" >&2; exit 1; }
command -v hyperfine >/dev/null || { echo "hyperfine not found (brew install hyperfine)" >&2; exit 1; }

# Never benchmark a stale local release binary. The default binary is rebuilt
# automatically; an explicit NUB= override must already match the workspace.
WS_VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
nub_semver() { "$1" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1; }
ensure_fresh_nub() {
  local is_default=0
  [ "$NUB" = "$DEFAULT_NUB" ] && is_default=1
  if [ ! -x "$NUB" ]; then
    if [ "$is_default" -eq 1 ]; then
      echo "== nub binary missing at $NUB — building release (cargo build --release -p nub-cli) =="
      ( cd "$REPO_ROOT" && cargo build --release -p nub-cli ) || { echo "ERROR: release build failed" >&2; exit 1; }
    else
      echo "ERROR: nub not found at NUB=$NUB. Build it: cargo build --release -p nub-cli" >&2
      exit 1
    fi
  fi
  local nv; nv="$(nub_semver "$NUB")"
  if [ "$nv" != "$WS_VERSION" ]; then
    if [ "$is_default" -eq 1 ]; then
      echo "== STALE nub binary: $NUB is v$nv but workspace is v$WS_VERSION — rebuilding release =="
      ( cd "$REPO_ROOT" && cargo build --release -p nub-cli ) || { echo "ERROR: release build failed" >&2; exit 1; }
      nv="$(nub_semver "$NUB")"
      [ "$nv" = "$WS_VERSION" ] || { echo "ERROR: after rebuild nub is still v$nv, expected v$WS_VERSION" >&2; exit 1; }
    else
      echo "ERROR: stale/mismatched nub binary: NUB=$NUB is v$nv but workspace is v$WS_VERSION." >&2
      echo "       Rebuild release and re-run: cargo build --release -p nub-cli" >&2
      exit 1
    fi
  fi
  echo "== nub binary OK: v$nv (matches workspace v$WS_VERSION) =="
}
ensure_fresh_nub

WORK="$(mktemp -d "${TMPDIR:-/tmp}/nub-4way-XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
for t in nub bun pnpm npm; do cp -R "$FIX" "$WORK/$t"; done

# A fixture ships every lockfile; each tool must install from its OWN. Strip the
# others so nub (which refuses an ambiguous multi-lockfile project) and the rest
# resolve the right one — same split as run.sh's setup_workdir. Nub keeps only
# nub.lock so this measures its native lockfile path.
rm -f "$WORK/nub/pnpm-lock.yaml" "$WORK/nub/pnpm-workspace.yaml" "$WORK/nub/bun.lock" "$WORK/nub/bun.lockb" "$WORK/nub/package-lock.json"
rm -f "$WORK/pnpm/nub.lock" "$WORK/pnpm/bun.lock" "$WORK/pnpm/bun.lockb" "$WORK/pnpm/package-lock.json"
rm -f "$WORK/bun/nub.lock" "$WORK/bun/pnpm-lock.yaml" "$WORK/bun/pnpm-workspace.yaml" "$WORK/bun/package-lock.json"
rm -f "$WORK/npm/nub.lock" "$WORK/npm/bun.lock" "$WORK/npm/bun.lockb" "$WORK/npm/pnpm-lock.yaml" "$WORK/npm/pnpm-workspace.yaml"

echo "== warming each tool's store (untimed) =="
( cd "$WORK/nub"  && env -u CI "$NUB" install --frozen-lockfile )
( cd "$WORK/bun"  && bun install --frozen-lockfile )
( cd "$WORK/pnpm" && pnpm install --frozen-lockfile --silent )
( cd "$WORK/npm"  && npm ci )

echo "== warm install: $FIXTURE =="
# -N (no intermediate shell) so a per-run shell spawn doesn't add a fixed cost to
# every command and compress the ratio. Commands are reshaped to be shell-free:
# env sets/unsets vars directly; npm (which must cd — it ignores --prefix) gets a
# single `sh -c`, which -N execs directly (only the slowest tool pays that, so the
# ratio is unaffected).
hyperfine -N --warmup "$WARMUP" --runs "$RUNS" \
  --command-name "nub install"  --prepare "rm -rf '$WORK/nub/node_modules'"  "env -u CI '$NUB' --cwd '$WORK/nub' install --frozen-lockfile -s" \
  --command-name "bun install"  --prepare "rm -rf '$WORK/bun/node_modules'"  "bun install --frozen-lockfile --cwd '$WORK/bun'" \
  --command-name "pnpm install" --prepare "rm -rf '$WORK/pnpm/node_modules'" "pnpm install --frozen-lockfile --dir '$WORK/pnpm' --silent" \
  --command-name "npm ci"       --prepare "rm -rf '$WORK/npm/node_modules'"  "sh -c \"cd '$WORK/npm' && npm ci --offline --ignore-scripts\""
