#!/usr/bin/env bash
# Brand-boundary sweep for the embedded pnpm engine.
#
# Runs REAL installs in sandboxed temp fixtures (HOME + every XDG_* dir pointed
# inside the sandbox) and asserts the identity each kind of project gets:
#
#   a nub project (no pnpm marker)
#     1. output carries nub's identity — `using nub v…` and ERR_NUB_/WARN_NUB_
#        codes — and no ERR_PNPM_/WARN_PNPM_ code or pnpm.io link;
#     2. PNPM_* env is dead — store canaries change neither the install nor
#        `config get store-dir`;
#     3. the stores are nub's — the virtual store leaf is node_modules/.store,
#        packages resolve into $XDG_CACHE_HOME/nub/store outside CI and into the
#        project's own .store in CI, and no pnpm-named user dir appears;
#     4. lifecycle scripts see a nub-first npm_config_user_agent;
#     5. the ignored-builds gate and the deprecation warnings speak nub;
#   a pnpm project (a pnpm-workspace.yaml marker)
#     6. keeps pnpm's own user agent, as pnpm 12 reports it;
#   7. engine verb help renders under nub's program name.
#
# Usage: tests/brand-sweep/run.sh <path-to-nub-binary>
# CI: a step on one ubuntu leg of the `test` job (see .github/workflows/ci.yml).
# Network: installs left-pad, core-js and request from registry.npmjs.org.
set -euo pipefail

NUB_ARG=${1:?usage: run.sh <path-to-nub>}
NUB=$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")
[ -x "$NUB" ] || { echo "FAIL: nub binary not executable: $NUB"; exit 1; }

SANDBOX=$(mktemp -d)
trap 'rm -rf "$SANDBOX"' EXIT

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "ok: $*"; }
# fail_with <file> <message> — dump the captured output first, so a CI log
# shows which line fired.
fail_with() { echo "---- output ($1):"; cat "$1"; shift; fail "$@"; }

export HOME="$SANDBOX/home"
export XDG_DATA_HOME="$SANDBOX/xdg/data"
export XDG_CACHE_HOME="$SANDBOX/xdg/cache"
export XDG_CONFIG_HOME="$SANDBOX/xdg/config"
export XDG_STATE_HOME="$SANDBOX/xdg/state"
mkdir -p "$HOME"
# Settings the runner or a developer exported must not steer these installs.
for var in $(env | grep -oE '^(npm_config_|NPM_CONFIG_|pnpm_config_|PNPM_)[A-Za-z0-9_]*' || true); do
  unset "$var"
done

# Store canaries: pnpm reads both, so honoring either relocates a store into
# the sandbox. Set per invocation, because a pnpm project reads them by design.
CANARY=(env "PNPM_CONFIG_STORE_DIR=$SANDBOX/canary-store" "pnpm_config_virtual_store_dir=$SANDBOX/canary-vsd")

# write_ua_fixture <dir> <name> <extra-manifest-lines> — a project whose
# postinstall records the user agent it saw. A file, not stdout, so the
# assertion does not depend on how the install streams script output.
write_ua_fixture() {
  mkdir -p "$1"
  cat > "$1/package.json" <<EOF
{
  "name": "$2",
  "private": true,$3
  "scripts": {
    "postinstall": "node -e \"require('fs').writeFileSync('ua-seen.txt', process.env.npm_config_user_agent || '<unset>')\""
  },
  "dependencies": {
    "left-pad": "1.3.0"
  }
}
EOF
}

# assert_nub_identity <output-file> <label>
assert_nub_identity() {
  grep -q 'using nub v' "$1" || fail_with "$1" "the install summary does not name nub ($2)"
  if grep -nE 'ERR_PNPM_|WARN_PNPM_|pnpm\.io' "$1"; then
    fail_with "$1" "pnpm's identity reached a nub project's output ($2)"
  fi
}

# assert_nub_layout <project> <ci:0|1> <label>
assert_nub_layout() {
  local proj=$1 ci=$2 label=$3 real store
  [ -d "$proj/node_modules/.store" ] || fail "expected the virtual store at node_modules/.store ($label)"
  real=$(cd "$proj/node_modules/left-pad" 2>/dev/null && pwd -P) || fail "left-pad was not installed ($label)"
  if [ "$ci" = 1 ]; then
    store=$(cd "$proj/node_modules/.store" && pwd -P)
  else
    store=$(cd "$XDG_CACHE_HOME/nub/store" 2>/dev/null && pwd -P) || fail "no store at \$XDG_CACHE_HOME/nub/store ($label)"
  fi
  case "$real" in
    "$store"/*) ;;
    *) fail "left-pad resolves to $real, outside $store ($label)" ;;
  esac
  [ ! -e "$SANDBOX/canary-store" ] && [ ! -e "$SANDBOX/canary-vsd" ] \
    || fail "a PNPM_* store canary was honored in a nub project ($label)"
}

# assert_nub_ua <project> <label>
assert_nub_ua() {
  local ua
  [ -f "$1/ua-seen.txt" ] || fail "postinstall did not run ($2)"
  ua=$(cat "$1/ua-seen.txt")
  echo "$ua" | grep -qE '^nub/[0-9][^ ]* npm/\? node/[^ ]+ [a-z0-9]+ [a-z0-9]+$' \
    || fail "npm_config_user_agent is not nub-first ($2): '$ua'"
}

if [ -n "${CI:-}" ]; then this_ci=1; other_ci=0; other=(env -u CI); else this_ci=0; other_ci=1; other=(env CI=1); fi

# 1-4. A nub project, in this environment's CI mode and then in the other one:
# the global virtual store is on outside CI and off in it, and the two layouts
# reach the store through different paths.
PROJ="$SANDBOX/nub-project"
write_ua_fixture "$PROJ" brand-sweep-nub ""
out="$SANDBOX/nub-install.txt"
(cd "$PROJ" && "${CANARY[@]}" "$NUB" install) >"$out" 2>&1 || fail_with "$out" "nub install exited non-zero"
assert_nub_identity "$out" "CI=$this_ci"
assert_nub_layout "$PROJ" "$this_ci" "CI=$this_ci"
assert_nub_ua "$PROJ" "CI=$this_ci"

PROJ2="$SANDBOX/nub-project-other-mode"
write_ua_fixture "$PROJ2" brand-sweep-nub-2 ""
out2="$SANDBOX/nub-install-other-mode.txt"
(cd "$PROJ2" && "${other[@]}" "${CANARY[@]}" "$NUB" install) >"$out2" 2>&1 || fail_with "$out2" "nub install (CI=$other_ci) exited non-zero"
assert_nub_identity "$out2" "CI=$other_ci"
assert_nub_layout "$PROJ2" "$other_ci" "CI=$other_ci"
assert_nub_ua "$PROJ2" "CI=$other_ci"

store=$(cd "$PROJ" && "${CANARY[@]}" "$NUB" config get store-dir)
case "$store" in
  "$XDG_CACHE_HOME/nub/store"*) ;;
  *) fail "config get store-dir answered '$store' in a nub project, not nub's store" ;;
esac
for dir in "$XDG_DATA_HOME/pnpm" "$XDG_CACHE_HOME/pnpm" "$XDG_STATE_HOME/pnpm" "$HOME/Library/pnpm"; do
  [ ! -e "$dir" ] || fail "a nub project wrote pnpm's user dir $dir"
done
pass "nub projects carry nub's identity, stores and user agent in both CI modes; PNPM_* canaries ignored"

# 5. The ignored-builds gate and the deprecation warnings. core-js ships an
# install script nobody approved, so the install fails as pnpm 12's does;
# request is deprecated.
PROJ3="$SANDBOX/nub-warnings"
mkdir -p "$PROJ3"
printf '{\n  "name": "brand-sweep-warnings",\n  "private": true,\n  "dependencies": { "core-js": "3.40.0", "request": "2.88.2" }\n}\n' > "$PROJ3/package.json"
out3="$SANDBOX/nub-warnings.txt"
if (cd "$PROJ3" && "$NUB" install) >"$out3" 2>&1; then
  fail_with "$out3" "an install with an unapproved build script exited 0"
fi
grep -q 'ERR_NUB_IGNORED_BUILDS' "$out3" || fail_with "$out3" "the ignored-builds error does not carry nub's code"
grep -q 'core-js@3.40.0' "$out3" || fail_with "$out3" "the ignored-builds error does not name core-js"
grep -q 'Run "nub approve-builds"' "$out3" || fail_with "$out3" "the ignored-builds hint does not name nub approve-builds"
grep -q 'deprecated request@2.88.2' "$out3" || fail_with "$out3" "the deprecation warning is missing"
if grep -nE 'ERR_PNPM_|WARN_PNPM_|pnpm approve-builds|pnpm\.io' "$out3"; then
  fail_with "$out3" "pnpm's identity reached the warning channel"
fi
pass "the ignored-builds gate and deprecation warnings speak nub"

# 6. A pnpm project keeps pnpm's identity: its user agent leads with pnpm's own
# token, as pnpm 12 reports it.
PROJ4="$SANDBOX/pnpm-project"
write_ua_fixture "$PROJ4" brand-sweep-pnpm ""
: > "$PROJ4/pnpm-workspace.yaml"
out4="$SANDBOX/pnpm-install.txt"
(cd "$PROJ4" && "$NUB" install) >"$out4" 2>&1 || fail_with "$out4" "nub install (pnpm project) exited non-zero"
[ -f "$PROJ4/ua-seen.txt" ] || fail_with "$out4" "postinstall did not run (pnpm project)"
ua=$(cat "$PROJ4/ua-seen.txt")
echo "$ua" | grep -qE '^pnpm/[0-9][^ ]* npm/\? node/[^ ]+ [a-z0-9]+ [a-z0-9]+$' \
  || fail "a pnpm project's npm_config_user_agent is not pnpm's: '$ua'"
pass "a pnpm project keeps pnpm's user agent: $ua"

# 7. Engine verb help renders under nub's program name, with no pnpm code or
# link. Help comes from the engine's own command definitions, so this is the
# guard against help drift after a pin move.
help_out="$SANDBOX/help.txt"
for verb in add remove update import dedupe prune rebuild fetch link unlink \
  approve-builds ignored-builds patch patch-commit patch-remove \
  list ls la ll outdated why licenses audit peers view bin root search \
  publish pack version deprecate undeprecate dist-tag unpublish \
  login logout whoami owner store cache cat-file cat-index find-hash pkg set-script \
  install ci sbom deploy; do
  (cd "$PROJ" && "$NUB" "$verb" --help) >"$help_out" 2>&1 || fail_with "$help_out" "\`nub $verb --help\` exited non-zero"
  grep -q 'Usage: nub ' "$help_out" || fail_with "$help_out" "\`nub $verb --help\` does not render under nub's program name"
  if grep -nE 'ERR_PNPM_|pnpm\.io' "$help_out"; then
    fail_with "$help_out" "\`nub $verb --help\` carries a pnpm code or link"
  fi
done
pass "engine verb help renders under nub's program name"

echo "brand-sweep: all assertions passed"
