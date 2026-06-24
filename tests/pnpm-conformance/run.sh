#!/usr/bin/env bash
# pnpm conformance harness — run pnpm's OWN black-box CLI test suite against nub.
#
# pnpm's front-door package (`pnpm/` inside the monorepo) ships ~64 test files in
# pnpm/test/*.ts, 63 of which spawn the real binary through ONE seam:
# `pnpmBinLocation` in pnpm/test/utils/execPnpm.ts. We swap that bin for the nub
# binary (identified as pnpm via argv[0]) so the suite exercises nub's drop-in PM
# surface — stdout/stderr/exit-code/lockfile/node_modules — exactly where nub
# claims pnpm parity. Divergences are the findings.
#
# Usage:
#   tests/pnpm-conformance/run.sh <nub-binary> [pnpm-tag] [jest-args...]
#
#   <nub-binary>  absolute or repo-relative path to the built nub (e.g. target/debug/nub)
#   pnpm-tag      pnpm git tag to clone & pin (default: PNPM_PIN env or v10.15.1).
#                 PIN to nub's spoofed pnpm major to avoid version-skew false negs.
#   jest-args     extra args passed through to jest (e.g. a single test file:
#                 `test/root.ts` to run just one, or `-t 'pattern'`).
#
# Env:
#   PNPM_PIN          pnpm version to pin (without the leading v; default 10.15.1)
#   PNPM_CLONE_DIR    where to clone pnpm (default: a temp dir; reused if present)
#   KEEP_CLONE=1      do not delete a temp clone on exit (for debugging)
#   NUB_NO_UPDATE=1   gate nub's self-update check off (set automatically; B3 flake)
#
# Exit: 0 if every failing test is allowlisted (known divergence/bug); non-zero
# if any SURPRISE failure (an unexpected divergence) or stale allowlist entry.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

# ── Args ─────────────────────────────────────────────────────────────────────
NUB_BIN_ARG="${1:-}"
if [ -z "$NUB_BIN_ARG" ]; then
  echo "usage: $0 <nub-binary> [pnpm-tag] [jest-args...]" >&2
  exit 2
fi
shift
PNPM_TAG="${1:-v${PNPM_PIN:-10.15.1}}"
# Allow passing a bare jest-arg in $1 (only shift the tag if it looks like a tag).
case "$PNPM_TAG" in
  v[0-9]*|[0-9]*) [[ $# -gt 0 ]] && shift; [[ "$PNPM_TAG" == v* ]] || PNPM_TAG="v$PNPM_TAG" ;;
  *) PNPM_TAG="v${PNPM_PIN:-10.15.1}" ;;
esac
JEST_EXTRA=("$@")

# Resolve nub binary to an absolute path.
if [ -f "$NUB_BIN_ARG" ]; then
  NUB_BIN="$(cd "$(dirname "$NUB_BIN_ARG")" && pwd)/$(basename "$NUB_BIN_ARG")"
elif [ -f "$REPO_ROOT/$NUB_BIN_ARG" ]; then
  NUB_BIN="$REPO_ROOT/$NUB_BIN_ARG"
else
  echo "error: nub binary not found: $NUB_BIN_ARG" >&2
  exit 2
fi
export NUB_BIN

# ── Clone (pinned) ───────────────────────────────────────────────────────────
CLONE_DIR="${PNPM_CLONE_DIR:-}"
CLEANUP_CLONE=0
if [ -z "$CLONE_DIR" ]; then
  CLONE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nub-pnpm-conf.XXXXXX")"
  CLEANUP_CLONE=1
fi
cleanup() {
  if [ "$CLEANUP_CLONE" = 1 ] && [ "${KEEP_CLONE:-0}" != 1 ]; then
    rm -rf "$CLONE_DIR"
  fi
}
trap cleanup EXIT

echo "==> nub binary:   $NUB_BIN"
echo "==> nub version:  $("$NUB_BIN" --version 2>/dev/null || echo '?')"
echo "==> pnpm tag:     $PNPM_TAG"
echo "==> clone dir:    $CLONE_DIR"

if [ ! -d "$CLONE_DIR/.git" ]; then
  echo "==> cloning pnpm/pnpm @ $PNPM_TAG (shallow)"
  git clone --depth 1 --branch "$PNPM_TAG" https://github.com/pnpm/pnpm.git "$CLONE_DIR"
else
  echo "==> reusing existing clone at $CLONE_DIR"
fi

cd "$CLONE_DIR"

# ── Bootstrap (mirror pnpm's own CI: install + compile-only) ─────────────────
# pnpm's CI (.github/workflows/ci.yml) does `pn compile-only` then runs jest.
# We use Corepack-pinned pnpm to install the monorepo, then compile only what
# the `pnpm` front-door package and the registry-mock need.
if [ ! -d "$CLONE_DIR/node_modules" ]; then
  echo "==> enabling corepack + installing monorepo deps (this is the slow step)"
  corepack enable >/dev/null 2>&1 || true
  # The repo's own packageManager field pins the pnpm used to bootstrap.
  corepack pnpm install --frozen-lockfile
fi

# The front-door bin file the suite spawns is version-dependent: newer pnpm uses
# bin/pnpm.mjs, older (e.g. 10.15.x) uses bin/pnpm.cjs. Detect which the suite's
# own seam points at, so the swap targets the exact file execPnpm.ts spawns.
SEAM_BASENAME="$(node -e '
  const fs = require("fs");
  const src = fs.readFileSync(process.argv[1], "utf8");
  const m = src.match(/pnpmBinLocation\s*=.*?["'"'"']([^"'"'"']*pnpm\.(?:cjs|mjs))["'"'"']/);
  process.stdout.write(m ? m[1].split("/").pop() : "pnpm.cjs");
' "$CLONE_DIR/pnpm/test/utils/execPnpm.ts")"
SEAM="$CLONE_DIR/pnpm/bin/$SEAM_BASENAME"
echo "==> seam file: pnpm/bin/$SEAM_BASENAME"

if [ ! -f "$CLONE_DIR/pnpm/dist/pnpm.cjs" ] && [ ! -f "$CLONE_DIR/pnpm/dist/pnpm.mjs" ] || [ "${FORCE_COMPILE:-0}" = 1 ]; then
  echo "==> compiling pnpm front-door package"
  # The full `compile-only` script also typechecks + lints the ENTIRE monorepo
  # (many minutes, irrelevant to running the suite). We do only what produces a
  # runnable binary: tsc --build (the pnpm package's lib/) + bundle (dist/), then
  # copy the runtime assets the bundle expects. This is the lean compile path.
  corepack pnpm -F pnpm exec tsc --build
  corepack pnpm -F pnpm run bundle
  corepack pnpm -F pnpm exec shx cp -r node-gyp-bin dist/node-gyp-bin 2>/dev/null || true
  corepack pnpm -F pnpm exec shx cp -r node_modules/@pnpm/tabtab/lib/templates dist/templates 2>/dev/null || true
  corepack pnpm -F pnpm exec shx cp -r node_modules/ps-list/vendor dist/vendor 2>/dev/null || true
  corepack pnpm -F pnpm exec shx cp pnpmrc dist/pnpmrc 2>/dev/null || true
fi

# ── Seam swap ────────────────────────────────────────────────────────────────
if [ ! -f "$SEAM" ]; then
  echo "error: seam target not found after compile: $SEAM" >&2
  echo "       (the suite spawns this file; it must exist before swapping)" >&2
  exit 2
fi
echo "==> swapping seam: $SEAM -> nub"
cp "$SEAM" "$SEAM.orig-pnpm"
# The shim body is CommonJS. A .cjs seam takes it verbatim; a .mjs seam would
# force ESM and reject `require`, so for .mjs we emit an ESM wrapper that defers
# to the CJS shim via createRequire. Either way nub is what actually runs.
# Bake the absolute nub path into the shim — pnpm's createEnv() rebuilds a clean
# env (keeps only PATH/COLORTERM/APPDATA), so an exported NUB_BIN would not reach
# the spawned shim. Substituting it into the file is the robust seam.
SHIM_BODY="$(sed "s#__NUB_BIN__#${NUB_BIN}#" "$HERE/nub-pnpm-shim.cjs")"
case "$SEAM" in
  *.mjs)
    printf '%s\n' "$SHIM_BODY" > "$CLONE_DIR/pnpm/bin/nub-pnpm-shim.cjs"
    cat > "$SEAM" <<'ESM'
import { createRequire } from 'node:module'
createRequire(import.meta.url)('./nub-pnpm-shim.cjs')
ESM
    ;;
  *)
    printf '%s\n' "$SHIM_BODY" > "$SEAM"
    ;;
esac

# ── Flake mitigations ────────────────────────────────────────────────────────
# B3: gate nub/aube self-update check off so the "Update available" banner never
# pollutes stdout assertions.
export NUB_NO_UPDATE=1
export AUBE_NO_UPDATE_CHECK=1
export CI=1

# ── Run jest, scoped to the front-door suite (pnpm/test/) ────────────────────
echo "==> running jest over pnpm/test/ (front-door black-box suite)"
RESULTS_JSON="$CLONE_DIR/nub-conformance-results.json"
JEST_BIN="$CLONE_DIR/node_modules/.bin/jest"
if [ ! -x "$JEST_BIN" ]; then
  echo "error: jest not found at $JEST_BIN (did the install step run?)" >&2
  exit 2
fi

# Run from inside the pnpm package so its jest preset (registry-mock) is active.
# Build one jest arg list so empty cases are safe under `set -u` (bash 3.2 errors
# on "${empty[@]}"). Scope to the front-door suite (pnpm/test/) ONLY when no
# explicit jest args were given — a passed file/pattern would otherwise be OR'd
# with --testPathPattern and pull in extra suites.
JEST_ARGS=(--json --outputFile="$RESULTS_JSON" --ci)
if [ "${#JEST_EXTRA[@]}" -gt 0 ]; then
  JEST_ARGS+=("${JEST_EXTRA[@]}")
else
  JEST_ARGS+=(--testPathPattern 'test/')
fi
cd "$CLONE_DIR/pnpm"
set +e
NODE_OPTIONS="${NODE_OPTIONS:-} --experimental-vm-modules --disable-warning=ExperimentalWarning --disable-warning=DEP0169" \
  "$JEST_BIN" "${JEST_ARGS[@]}"
JEST_EXIT=$?
set -e

# ── Classify results against the allowlist ───────────────────────────────────
echo "==> classifying results against allowlist"
if [ ! -f "$RESULTS_JSON" ]; then
  echo "error: jest produced no results JSON (exit $JEST_EXIT) — bootstrap/run failed" >&2
  exit 2
fi

# Stale-allowlist detection only on a whole-suite run (no extra jest filters).
# Build the arg list as a single array so an empty case is safe under `set -u`
# (bash 3.2 errors on "${empty[@]}" without a default).
CLASSIFY_ARGS=()
if [ "${#JEST_EXTRA[@]}" -eq 0 ]; then
  CLASSIFY_ARGS+=(--full)
fi
CLASSIFY_ARGS+=("$RESULTS_JSON" "$HERE/allowlist.txt")
node "$HERE/classify.mjs" "${CLASSIFY_ARGS[@]}"
CLASSIFY_EXIT=$?
exit $CLASSIFY_EXIT
