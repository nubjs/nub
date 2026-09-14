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
#   pnpm-tag      pnpm git tag to clone & pin (default: PNPM_PIN env or v11.3.0).
#                 PIN to nub's spoofed pnpm major to avoid version-skew false negs.
#   jest-args     extra args passed through to jest (e.g. a single test file:
#                 `test/root.ts` to run just one, or `-t 'pattern'`).
#
# Env:
#   PNPM_PIN          pnpm version to pin (without the leading v; default 11.3.0)
#   PNPM_CLONE_DIR    where to clone pnpm (default: a temp dir; reused if present)
#   KEEP_CLONE=1      do not delete a temp clone on exit (for debugging)
#   NUB_NO_UPDATE=1   gate nub's self-update check off (set automatically; B3 flake)
#
# Exit: 0 if every failing test is allowlisted (a known divergence/bug); non-zero
# ONLY on a SURPRISE failure (a new, un-allowlisted divergence — a regression).
# A stale allowlist entry is reported but non-fatal (see classify.mjs).
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
PNPM_TAG="${1:-v${PNPM_PIN:-11.3.0}}"
# Allow passing a bare jest-arg in $1 (only shift the tag if it looks like a tag).
case "$PNPM_TAG" in
  v[0-9]*|[0-9]*) [[ $# -gt 0 ]] && shift; [[ "$PNPM_TAG" == v* ]] || PNPM_TAG="v$PNPM_TAG" ;;
  *) PNPM_TAG="v${PNPM_PIN:-11.3.0}" ;;
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
  # Reusing a clone is the proximate cause of the historical fork bomb: a stale
  # v11 clone reused for a v10 pin exercised a version-specific re-entrant path,
  # AND it left the seam already swapped so the `.orig-pnpm` backup was a shim,
  # not real pnpm. Verify the checked-out tag matches the request; on mismatch,
  # fetch + hard-checkout the requested tag (which also restores the tracked
  # seam to pristine), then wipe node_modules/dist so the bootstrap, compile,
  # and seam-swap all re-run cleanly against the correct version.
  CURRENT_TAG="$(git -C "$CLONE_DIR" describe --tags --exact-match HEAD 2>/dev/null || echo '')"
  if [ "$CURRENT_TAG" = "$PNPM_TAG" ]; then
    echo "==> reusing existing clone at $CLONE_DIR (at $PNPM_TAG)"
  else
    echo "==> existing clone is at '${CURRENT_TAG:-unknown}', need '$PNPM_TAG' — re-checking out"
    if ! git -C "$CLONE_DIR" fetch --depth 1 origin "refs/tags/$PNPM_TAG:refs/tags/$PNPM_TAG" 2>/dev/null; then
      echo "error: cannot fetch $PNPM_TAG into existing clone at $CLONE_DIR" >&2
      echo "       remove that dir (or unset PNPM_CLONE_DIR) and re-run for a fresh clone." >&2
      exit 2
    fi
    git -C "$CLONE_DIR" checkout -f "$PNPM_TAG"
    rm -rf "$CLONE_DIR/node_modules" "$CLONE_DIR/pnpm/dist"
  fi
fi

# ── Process-cap safety net ───────────────────────────────────────────────────
# Belt-and-suspenders for the seam-recursion fork bomb (which hit ~10k pnpm
# processes and drove load to ~600). The shim re-entry guard is the real fix;
# this cap ensures any FUTURE recursion regression (e.g. via a jailed dep script
# whose env_clear strips the sentinel) still cannot melt the host. Sized as
# current-usage + headroom so it never trips this box's existing build-fleet
# load, yet kills an unbounded self-spawn. NUB_CONF_NO_ULIMIT=1 skips it (e.g.
# when relying on a container `--pids-limit` instead); NUB_CONF_PROC_HEADROOM
# tunes the headroom.
if [ "${NUB_CONF_NO_ULIMIT:-0}" != 1 ]; then
  HEADROOM="${NUB_CONF_PROC_HEADROOM:-800}"
  # `|| CUR_PROCS=200` is required: under `pipefail` a failing `ps` makes the
  # whole substitution non-zero, which would abort the script via `set -e`
  # before the empty-check fallback could run.
  CUR_PROCS="$(ps -U "$(id -u)" 2>/dev/null | wc -l | tr -d ' ')" || CUR_PROCS=200
  [ -z "$CUR_PROCS" ] && CUR_PROCS=200
  # The cap is a per-USER limit (RLIMIT_NPROC), enforced against the user's
  # TOTAL live processes at fork time — not just this run's tree. HEADROOM must
  # therefore exceed both jest's worker pool AND any concurrent fleet growth on
  # a busy box, or a legitimate fork gets EAGAIN. Bump NUB_CONF_PROC_HEADROOM if
  # so; the default 800 is generous for a normal run yet far below a 10k bomb.
  CAP=$((CUR_PROCS + HEADROOM))
  HARD="$(ulimit -Hu 2>/dev/null || echo unlimited)"
  if [ "$HARD" != unlimited ] && [ "$CAP" -gt "$HARD" ]; then CAP="$HARD"; fi
  if ulimit -u "$CAP" 2>/dev/null; then
    echo "==> process cap:  ulimit -u $CAP (current ~$CUR_PROCS + headroom $HEADROOM)"
  else
    echo "==> warning: could not set process cap (ulimit -u $CAP)" >&2
  fi
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
# The pristine-pnpm backup MUST keep the seam's original extension: the shim's
# nested-pnpm fallthrough execs it as `node <backup>`, and node picks its loader
# (ESM vs CJS) from the extension — a `.mjs` backup renamed to `.orig-pnpm` is
# rejected (ERR_UNKNOWN_FILE_EXTENSION). So `pnpm.mjs` → `pnpm.orig-pnpm.mjs`,
# `pnpm.cjs` → `pnpm.orig-pnpm.cjs`.
SEAM_EXT="${SEAM##*.}"
ORIG_BACKUP="${SEAM%.*}.orig-pnpm.${SEAM_EXT}"
# Back up the pristine seam ONLY when the seam is pristine (not already our
# shim). The fallthrough execs this backup, so overwriting it with an
# already-swapped seam (the stale-reuse failure mode) would poison the
# fork-bomb guard. The clone-tag re-checkout above restores a reused seam to
# pristine, so this only trips if a clone is hand-swapped.
if grep -q 'nub-pnpm-shim' "$SEAM" 2>/dev/null; then
  echo "==> seam already swapped; preserving existing backup"
  if [ ! -f "$ORIG_BACKUP" ] || grep -q 'nub-pnpm-shim' "$ORIG_BACKUP" 2>/dev/null; then
    echo "error: seam is swapped but no pristine backup ($ORIG_BACKUP) exists." >&2
    echo "       remove $CLONE_DIR (or unset PNPM_CLONE_DIR) and re-run for a clean clone." >&2
    exit 2
  fi
else
  cp "$SEAM" "$ORIG_BACKUP"
fi
# The shim body is CommonJS. A .cjs seam takes it verbatim; a .mjs seam would
# force ESM and reject `require`, so for .mjs we emit an ESM wrapper that defers
# to the CJS shim via createRequire. Either way nub is what actually runs.
# Bake the absolute nub path, the pristine-pnpm backup path, and the clone dir
# into the shim — pnpm's createEnv() rebuilds a clean env (keeps only
# PATH/COLORTERM/APPDATA), so exported env would not reach the spawned shim.
# Substituting them into the file is the robust seam. `#` is the sed delimiter
# and `&`/`\` are special in sed's replacement — a path containing any of them
# would corrupt the shim, but these paths (NUB_BIN, the clone/backup dirs) are
# controlled local input that never contains them.
SHIM_BODY="$(sed -e "s#__NUB_BIN__#${NUB_BIN}#" \
                 -e "s#__ORIG_PNPM__#${ORIG_BACKUP}#" \
                 -e "s#__CLONE_DIR__#${CLONE_DIR}#" \
                 "$HERE/nub-pnpm-shim.cjs")"
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
# B3: nub never checks for a newer pnpm, and CI=1 keeps any other update notice
# quiet, so no "Update available" banner pollutes stdout assertions.
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
  # jest 30 (pnpm 11.x) renamed --testPathPattern -> --testPathPatterns; jest 29
  # (pnpm 10.x) used the singular. Pick the flag the installed jest accepts.
  # First integer run of `jest --version` (robust to a leading v / extra text).
  JEST_MAJOR="$("$JEST_BIN" --version 2>/dev/null | grep -oE '[0-9]+' | head -1)"
  if [ "${JEST_MAJOR:-0}" -ge 30 ]; then
    JEST_ARGS+=(--testPathPatterns 'test/')
  else
    JEST_ARGS+=(--testPathPattern 'test/')
  fi
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
