#!/usr/bin/env bash
# pnpm conformance harness: run pnpm 12's OWN black-box CLI suite against nub.
#
# pnpm 12 is the Rust `pnpm-cli` crate. Its end-to-end suite is one integration
# test binary, `pnpm/crates/cli/tests/suite` (~2,000 tests), and every test
# reaches the CLI through assert_cmd's `Command::cargo_bin("pnpm")`, which
# resolves `CARGO_BIN_EXE_pnpm` = `<target>/debug/pnpm`. nextest sets that
# variable itself at run time, so an exported override is discarded; the seam
# is the FILE at that path. The harness builds the suite, records nextest's
# build metadata, replaces `target/debug/pnpm` with a shim that execs nub, and
# reruns the recorded binaries without letting cargo rebuild (and so restore)
# the original. Full notes: tests/pnpm-conformance/README.md.
#
# Usage:
#   tests/pnpm-conformance/run.sh <nub-binary> [nextest-args...]
#
#   nextest-args  passed through, e.g. a filter: `-- add::` or `-E 'test(/^root::/)'`.
#                 Any extra argument marks the run partial: stale allowlist
#                 entries are not reported.
#
# Env:
#   PNPM_TAG            pnpm/pnpm tag to test against (default v12.4.1)
#   CONF_WORK_DIR       holds the clone, tools and the sandboxed HOME (default: temp dir)
#   KEEP_WORK=1         keep a temp CONF_WORK_DIR on exit
#   CONF_BUILD_JOBS     cargo -j for the suite build (default: cargo's own)
#
# Exit: the classifier's code. 0 iff every failing test is allowlisted.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PNPM_TAG="${PNPM_TAG:-v12.4.1}"
PNPM_VERSION="${PNPM_TAG#v}"

NUB_BIN_ARG="${1:-}"
if [ -z "$NUB_BIN_ARG" ] || [ ! -f "$NUB_BIN_ARG" ]; then
  echo "usage: $0 <nub-binary> [nextest-args...]" >&2
  exit 2
fi
shift
NUB_BIN="$(cd "$(dirname "$NUB_BIN_ARG")" && pwd)/$(basename "$NUB_BIN_ARG")"
NEXTEST_EXTRA=("$@")

WORK="${CONF_WORK_DIR:-}"
CLEANUP=0
if [ -z "$WORK" ]; then
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/nub-pnpm-conf.XXXXXX")"
  CLEANUP=1
fi
mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
trap '[ "$CLEANUP" = 1 ] && [ "${KEEP_WORK:-0}" != 1 ] && rm -rf "$WORK"' EXIT
CLONE="$WORK/pnpm"
TOOLS="$WORK/tools"
SANDBOX="$WORK/sandbox"

# ── Hermetic environment ─────────────────────────────────────────────────────
# A developer shell exports npm/pnpm config (credentials included) that would
# reconfigure both nub and the reference pnpm. Strip it, and give every tool a
# private HOME/XDG tree. cargo and rustup keep their real homes, resolved first.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
# The seam path and pnpr's fixture storage both assume `<clone>/target`.
unset CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR
while IFS= read -r name; do
  unset "$name"
done < <(env | grep -oE '^(npm_|NPM_CONFIG_|pnpm_config_|PNPM_)[^=]*')
mkdir -p "$SANDBOX"/{home,cache,data,config,state} "$TOOLS"
export HOME="$SANDBOX/home"
export XDG_CACHE_HOME="$SANDBOX/cache" XDG_DATA_HOME="$SANDBOX/data"
export XDG_CONFIG_HOME="$SANDBOX/config" XDG_STATE_HOME="$SANDBOX/state"
export GIT_AUTHOR_NAME=conformance GIT_AUTHOR_EMAIL=conformance@example.invalid
export GIT_COMMITTER_NAME=conformance GIT_COMMITTER_EMAIL=conformance@example.invalid
export PATH="$CARGO_HOME/bin:$TOOLS/bin:$TOOLS/node_modules/.bin:$PATH"
export NUB_NO_UPDATE=1

echo "==> nub binary:  $NUB_BIN ($("$NUB_BIN" --version))"
echo "==> pnpm tag:    $PNPM_TAG"
echo "==> work dir:    $WORK"

# ── Clone at the tag, and prove it is the version we mean ────────────────────
if [ ! -d "$CLONE/.git" ]; then
  git clone --quiet --depth 1 --branch "$PNPM_TAG" https://github.com/pnpm/pnpm.git "$CLONE"
fi
# Compare commits, not `describe` output: the release commit carries more than
# one tag (a pnpr tag sits on v12.4.1's commit).
CLONE_HEAD="$(git -C "$CLONE" rev-parse HEAD)"
TAG_COMMIT="$(git -C "$CLONE" rev-parse --verify --quiet "refs/tags/$PNPM_TAG^{commit}" || true)"
CLONE_VERSION="$(node -p 'require(process.argv[1]).version' "$CLONE/pnpm/npm/pnpm/package.json")"
if [ "$CLONE_HEAD" != "$TAG_COMMIT" ] || [ "$CLONE_VERSION" != "$PNPM_VERSION" ]; then
  echo "error: clone HEAD $CLONE_HEAD (package version $CLONE_VERSION) is not $PNPM_TAG" >&2
  exit 2
fi

# ── Tools: nextest, and the real pnpm the compatibility tests compare against ─
# Several tests spawn a bare `pnpm` from PATH to diff nub's output against it;
# upstream CI provides one with pnpm/setup. It must be the pinned version.
if ! command -v cargo-nextest >/dev/null; then
  case "$(uname -s)" in
    Linux) nextest_url=https://get.nexte.st/latest/linux ;;
    Darwin) nextest_url=https://get.nexte.st/latest/mac ;;
    *) echo "error: unsupported OS for nextest download" >&2; exit 2 ;;
  esac
  mkdir -p "$TOOLS/bin"
  curl -LsSf "$nextest_url" | tar zxf - -C "$TOOLS/bin"
fi
# Every version probe runs from the sandbox HOME: inside a project with a
# `packageManager` pin (pnpm/pnpm's own root pins an earlier 12.x) pnpm switches
# to the pinned version and reports that instead.
if [ "$(cd "$HOME" && pnpm --version 2>/dev/null || true)" != "$PNPM_VERSION" ]; then
  npm install --silent --no-audit --no-fund --prefix "$TOOLS" "pnpm@$PNPM_VERSION"
fi
REF_VERSION="$(cd "$HOME" && pnpm --version)"
if [ "$REF_VERSION" != "$PNPM_VERSION" ]; then
  echo "error: reference pnpm on PATH is $REF_VERSION, want $PNPM_VERSION" >&2
  exit 2
fi

cd "$CLONE"
# Upstream `just install`: the root workspace's node_modules, which the hook
# and pnpmfile tests load.
if [ ! -d node_modules ]; then
  pnpm install --frozen-lockfile --prefer-offline
fi

# ── Build the suite once and record nextest's view of it ─────────────────────
# Debug info is dropped (upstream CI keeps line tables only) to keep the link
# and the disk footprint down; no assertion reads it.
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
BUILD_ARGS=(-p pnpm-cli --test suite)
[ -n "${CONF_BUILD_JOBS:-}" ] && BUILD_ARGS+=(-j "$CONF_BUILD_JOBS")
SEAM="$CLONE/target/debug/pnpm"
if [ -f "$SEAM.pnpm-original" ]; then
  mv -f "$SEAM.pnpm-original" "$SEAM"
fi
cargo nextest list "${BUILD_ARGS[@]}" --list-type binaries-only --message-format json > "$WORK/binaries-metadata.json"
cargo metadata --format-version 1 > "$WORK/cargo-metadata.json"

# Positive control: before the swap the seam is pnpm itself.
SEAM_BEFORE="$(cd "$HOME" && "$SEAM" --version)"
if [ "$SEAM_BEFORE" != "$PNPM_VERSION" ]; then
  echo "error: $SEAM reports '$SEAM_BEFORE' before the swap, want $PNPM_VERSION" >&2
  exit 2
fi

# ── Swap the seam ────────────────────────────────────────────────────────────
# CONF_BASELINE=1 leaves pnpm in place: the same suite, environment and runner
# against pnpm itself, which is how a failure is told apart from one this
# environment causes for every binary (gen-allowlist.mjs takes that report).
SHIM_LOG="$WORK/shim-invocations.log"
if [ "${CONF_BASELINE:-0}" = 1 ]; then
  echo "==> baseline: running the suite against pnpm $SEAM_BEFORE itself"
else
  : > "$SHIM_LOG"
  mv "$SEAM" "$SEAM.pnpm-original"
  sed -e "s#__NUB_BIN__#${NUB_BIN}#" -e "s#__SHIM_LOG__#${SHIM_LOG}#" "$HERE/nub-as-pnpm.sh" > "$SEAM"
  chmod +x "$SEAM"
  SEAM_AFTER="$(cd "$HOME" && "$SEAM" --version)"
  NUB_VERSION="$(cd "$HOME" && "$NUB_BIN" --version)"
  if [ "$SEAM_AFTER" != "$NUB_VERSION" ]; then
    echo "error: swapped seam answered '$SEAM_AFTER', want nub's '$NUB_VERSION'" >&2
    exit 2
  fi
  : > "$SHIM_LOG"
  echo "==> seam swapped: target/debug/pnpm -> nub (control: $SEAM_BEFORE before, $SEAM_AFTER after)"
fi

# ── Run ──────────────────────────────────────────────────────────────────────
if ! grep -q '^\[profile\.nub-conformance\]' .config/nextest.toml; then
  cat >> .config/nextest.toml <<'TOML'

[profile.nub-conformance]
fail-fast = false

[profile.nub-conformance.junit]
path = "junit.xml"
store-success-output = false
store-failure-output = true
TOML
fi

# The environment upstream's run-rust-tests.mjs gives the suite.
: > "$SANDBOX/npmrc-auth"
export PNPM_CONFIG_CI=false PNPM_CONFIG_NPMRC_AUTH_FILE="$SANDBOX/npmrc-auth"
unset XDG_DATA_HOME

set +e
cargo nextest run --profile nub-conformance --no-fail-fast \
  --binaries-metadata "$WORK/binaries-metadata.json" \
  --cargo-metadata "$WORK/cargo-metadata.json" \
  "${NEXTEST_EXTRA[@]+"${NEXTEST_EXTRA[@]}"}"
NEXTEST_EXIT=$?
set -e
echo "==> nextest exit: $NEXTEST_EXIT"

JUNIT="$CLONE/target/nextest/nub-conformance/junit.xml"
if [ ! -f "$JUNIT" ]; then
  echo "error: nextest wrote no JUnit report" >&2
  exit 2
fi
if [ "${CONF_BASELINE:-0}" = 1 ]; then
  cp "$JUNIT" "$WORK/junit-baseline.xml"
  echo "==> baseline report: $WORK/junit-baseline.xml"
  exit 0
fi
cp "$JUNIT" "$WORK/junit.xml"

# A run in which nub was never spawned is a run of nothing.
INVOCATIONS="$(wc -l < "$SHIM_LOG" | tr -d ' ')"
echo "==> nub invocations through the seam: $INVOCATIONS"
if [ "$INVOCATIONS" -eq 0 ]; then
  echo "error: the suite never reached the seam" >&2
  exit 2
fi

CLASSIFY_ARGS=()
[ "${#NEXTEST_EXTRA[@]}" -eq 0 ] && CLASSIFY_ARGS+=(--full)
node "$HERE/classify.mjs" "${CLASSIFY_ARGS[@]+"${CLASSIFY_ARGS[@]}"}" "$WORK/junit.xml" "$HERE/allowlist.txt"
