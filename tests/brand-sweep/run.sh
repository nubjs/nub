#!/usr/bin/env bash
# Brand-boundary sweep for the embedded PM engine (vendor/aube).
#
# Runs a REAL `nub install` in a sandboxed temp fixture (HOME + every XDG_* dir
# pointed inside the sandbox) and asserts the engine never leaks its upstream
# identity through nub's surface:
#
#   1. output is clean — no ERR_AUBE_* / WARN_AUBE_* codes, no aube.jdx.dev URLs
#      on stdout or stderr (nub's presentation layer must rewrite them);
#   2. AUBE_* env vars are dead — an AUBE_VIRTUAL_STORE_DIR canary pointing into
#      the sandbox must have zero effect (engine_preflight enables only the
#      NPM + EXTERNAL env families, never the engine's own AUBE family);
#   3. layout is nub-branded — the isolated virtual store lands at
#      node_modules/.nub, and no node_modules/.aube or ~/.local/share/aube
#      (or any other aube-named path) appears anywhere in the sandbox;
#   4. lifecycle identity is nub — npm_config_user_agent observed by a real
#      postinstall script starts with "nub/".
#
# Usage: tests/brand-sweep/run.sh <path-to-nub-binary>
# CI: a step on one ubuntu leg of the `test` job (see .github/workflows/ci.yml).
# Network: installs one tiny real package (left-pad) from registry.npmjs.org.
set -euo pipefail

NUB_ARG=${1:?usage: run.sh <path-to-nub>}
NUB=$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")
[ -x "$NUB" ] || { echo "FAIL: nub binary not executable: $NUB"; exit 1; }

SANDBOX=$(mktemp -d)
trap 'rm -rf "$SANDBOX"' EXIT

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "ok: $*"; }

# Everything user-dirs lands inside the sandbox so a leak is observable, not a
# write into the dev box / runner home.
export HOME="$SANDBOX/home"
export XDG_DATA_HOME="$SANDBOX/xdg/data"
export XDG_CACHE_HOME="$SANDBOX/xdg/cache"
export XDG_CONFIG_HOME="$SANDBOX/xdg/config"
export XDG_STATE_HOME="$SANDBOX/xdg/state"
mkdir -p "$HOME"

# Canary: if the engine's own AUBE_* env family were honored, this would
# relocate the virtual store into the sandbox at an aube-named path.
export AUBE_VIRTUAL_STORE_DIR="$SANDBOX/aube-canary-vsd"

PROJ="$SANDBOX/proj"
mkdir -p "$PROJ"
cd "$PROJ"
# postinstall writes the UA it observed to a file — file, not stdout, so the
# assertion doesn't depend on how install output is streamed/prefixed.
cat > package.json <<'EOF'
{
  "name": "brand-sweep-fixture",
  "private": true,
  "scripts": {
    "postinstall": "node -e \"require('fs').writeFileSync('ua-seen.txt', process.env.npm_config_user_agent || '<unset>')\""
  },
  "dependencies": {
    "left-pad": "1.3.0"
  }
}
EOF

out_file="$SANDBOX/install-output.txt"
if ! "$NUB" install >"$out_file" 2>&1; then
  echo "--- nub install output ---"
  cat "$out_file"
  fail "nub install exited non-zero"
fi

# 1. No engine-branded identity in combined stdout+stderr — not just the
# ERR_AUBE_/WARN_AUBE_ codes and aube.jdx.dev URLs, but ANY 'aube' token and
# the 'by jdx.dev' attribution (which caught the real leak this assertion is
# scar tissue from: the engine's no-op banner printed
# 'aube 1.18.2-DEBUG by jdx.dev · ✓ Already up to date' verbatim). The fixture
# is aube-free by construction, so zero occurrences is the right bar.
if grep -inE 'aube|jdx\.dev' "$out_file"; then
  fail "engine-branded identity reached nub's output (above)"
fi
pass "no aube/jdx.dev identity in install output"

# 2. The AUBE_* canary had no effect.
[ ! -e "$AUBE_VIRTUAL_STORE_DIR" ] || fail "AUBE_VIRTUAL_STORE_DIR was honored — AUBE_* env family is live"
pass "AUBE_VIRTUAL_STORE_DIR canary ignored"

# 3a. The install actually ran through the engine with nub's layout policy:
# no lockfile detected -> isolated linker, virtual store at node_modules/.nub.
[ -d node_modules/.nub ] || fail "expected isolated virtual store at node_modules/.nub"
[ -e node_modules/left-pad ] || fail "left-pad was not installed"
pass "isolated install landed at node_modules/.nub"

# 3b. No aube-named paths anywhere in the sandbox (covers node_modules/.aube,
# ~/.local/share/aube, XDG dirs, and anything else). Allowlist: KNOWN leaks
# whose fixes are tracked separately — keep this list SHRINKING, never growing.
#   - $XDG_CACHE_HOME/aube/**: the engine's cache BASE is hard-named in
#     aube-store/src/dirs.rs (+ independent join("aube") derivations in
#     aube-util/adaptive.rs, aube-resolver/primer.rs, settings_context.rs) with
#     no settings key — the half-done cacheDir embedder seam (the storeDir half
#     IS done: the content store lands at $XDG_DATA_HOME/nub/store). DELETE
#     this entry when the cacheDir seam lands in vendor/aube.
# (The former .aube-state / .aube-applied-patches.json entries are gone: the
# sidecar stems now follow the registered product identity in vendor/aube, so
# nub installs write .nub-state / .nub-applied-patches.json — asserted below.)
leaks=$(find "$SANDBOX" -name '*aube*' ! -path "$AUBE_VIRTUAL_STORE_DIR" ! -path "$XDG_CACHE_HOME/aube" ! -path "$XDG_CACHE_HOME/aube/*" 2>/dev/null || true)
if [ -n "$leaks" ]; then
  echo "$leaks"
  fail "aube-named paths created outside the documented residual allowlist (above)"
fi
[ ! -e "$HOME/.local/share/aube" ] || fail "engine wrote ~/.local/share/aube"
[ ! -e node_modules/.aube ] || fail "engine created node_modules/.aube"
# The engine's freshness sidecar must carry nub's stem (product-identity
# derivation in vendor/aube): .nub-state under the virtual store, and no
# .aube-state anywhere (covered by the find above).
[ -d node_modules/.nub/.nub-state ] || fail "expected install state at node_modules/.nub/.nub-state"
pass "no aube-named paths beyond the documented residual allowlist"

# 4. Lifecycle UA identity: first token of npm_config_user_agent is nub/<ver>.
[ -f ua-seen.txt ] || fail "postinstall did not run (ua-seen.txt missing)"
ua=$(cat ua-seen.txt)
case "$ua" in
  nub/*) pass "npm_config_user_agent starts with nub/: $ua" ;;
  *) fail "npm_config_user_agent first token is not nub/: '$ua'" ;;
esac

# 5. Second install pass with the CI mode INVERTED. The engine's linker takes
# a different path under CI (the global-virtual-store gate flips on `CI`),
# and the paths can leak independently: the original node_modules/.aube CI
# leak (probe linker missing the virtualStoreDir override in the non-GVS
# streaming materializer) reproduced ONLY with CI set. Run the on-disk
# assertions in both modes so local runs and CI runs each cover the other's
# mode.
PROJ2="$SANDBOX/proj2"
mkdir -p "$PROJ2"
cp "$PROJ/package.json" "$PROJ2/"
cd "$PROJ2"
if [ -n "${CI:-}" ]; then other_mode_env=(env -u CI); other_mode="CI unset"; else other_mode_env=(env CI=1); other_mode="CI=1"; fi
if ! "${other_mode_env[@]}" "$NUB" install >"$SANDBOX/install-output2.txt" 2>&1; then
  cat "$SANDBOX/install-output2.txt"
  fail "nub install ($other_mode) exited non-zero"
fi
if grep -inE 'aube|jdx\.dev' "$SANDBOX/install-output2.txt"; then
  fail "engine-branded identity reached nub's output under $other_mode (above)"
fi
[ -d node_modules/.nub ] || fail "($other_mode) expected isolated virtual store at node_modules/.nub"
[ ! -e node_modules/.aube ] || fail "($other_mode) engine created node_modules/.aube"
[ -d node_modules/.nub/.nub-state ] || fail "($other_mode) expected install state at node_modules/.nub/.nub-state"
leaks=$(find "$SANDBOX" -name '*aube*' ! -path "$AUBE_VIRTUAL_STORE_DIR" ! -path "$XDG_CACHE_HOME/aube" ! -path "$XDG_CACHE_HOME/aube/*" 2>/dev/null || true)
if [ -n "$leaks" ]; then
  echo "$leaks"
  fail "($other_mode) aube-named paths outside the allowlist (above)"
fi
pass "no engine identity leaks with $other_mode either"

# 6. The engine's WARNING CHANNEL surfaces, rewritten. Two leak classes that
# bypassed the report-path rewrite live here (both were real, found 2026-06-10):
#   - tracing::warn! events (ignored build scripts): swallowed entirely by the
#     old no-op subscriber, and leaked raw WARN_AUBE_*/`aube approve-builds`
#     under RUST_LOG=warn. The pm_engine::log bridge must surface them
#     rewritten BY DEFAULT.
#   - direct-stderr stream lines (the transitive-deprecation hint): printed
#     mid-install where no fd capture runs; the fork drives the product name
#     from the registered UA token (aube_util::ua::product_name).
# esbuild = unreviewed dep build (default-deny policy); request = transitively
# deprecated deps (har-validator, uuid@3). Both version-pinned.
PROJ3="$SANDBOX/proj3"
mkdir -p "$PROJ3"
cat > "$PROJ3/package.json" <<'EOF'
{
  "name": "brand-sweep-warnings",
  "private": true,
  "dependencies": {
    "esbuild": "0.28.0",
    "request": "2.88.2"
  }
}
EOF
cd "$PROJ3"
out3="$SANDBOX/install-output3.txt"
if ! "$NUB" install >"$out3" 2>&1; then
  cat "$out3"
  fail "nub install (warning fixture) exited non-zero"
fi
grep -q 'WARN ignored build scripts' "$out3" || fail "ignored-build-scripts warning was swallowed (tracing bridge dead)"
grep -q 'WARN_NUB_IGNORED_BUILD_SCRIPTS' "$out3" || fail "warning code not rewritten to WARN_NUB_*"
grep -q 'Run \`nub approve-builds\`' "$out3" || fail "approve-builds hint not rebranded"
grep -q 'deprecation warnings. Run \`nub deprecations' "$out3" || fail "transitive-deprecation hint not rebranded"
if grep -inE 'aube|jdx\.dev' "$out3"; then
  fail "engine-branded identity reached the warning channel (above)"
fi
pass "warning channel surfaces rewritten (ignored builds + deprecation hint)"

# 7. Help/usage text is leak-free for every wired engine verb. Help renders
# through present::rewrite_help (config-vocabulary pass + brand rewrite);
# this loop is the rot-guard for engine help drift after a pin bump.
for verb in add remove update import dedupe prune rebuild fetch link unlink \
  approve-builds ignored-builds dlx patch patch-commit patch-remove \
  create init recursive list la ll \
  outdated why licenses audit peers query view deprecations check bin root \
  search publish pack version deprecate undeprecate dist-tag unpublish \
  login logout whoami owner token stage \
  store cache cat-file cat-index find-hash config get set pkg set-script \
  install ci; do
  if "$NUB" "$verb" --help 2>&1 | grep -qiE 'aube|jdx\.dev'; then
    "$NUB" "$verb" --help 2>&1 | grep -inE 'aube|jdx\.dev' | head -3
    fail "engine branding in \`nub $verb --help\` (above)"
  fi
done
pass "all wired verb helps are leak-free"

echo "brand-sweep: all assertions passed"
