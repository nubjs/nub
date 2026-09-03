#!/usr/bin/env bash
# Standalone-runner matrix: pack the runner npm packages from this checkout,
# install them into a throwaway project, and run every fixture under
# `node --import <pkg>` (and `--require <pkg>` where the Node supports it) on
# each requested Node, comparing stdout to fixtures/expected.txt.
#
#   tests/runner/run-matrix.sh                       # host node only
#   NODE_VERSIONS="18.19.0 22.14.0 26.7.0" tests/runner/run-matrix.sh
#                                                    # nvm-installed versions
#   TSX=1 tests/runner/run-matrix.sh                 # also run each fixture under tsx
#
# Needs a built addon at runtime/addons/nub-native.node (`make addon-fast`, or
# `cd crates/nub-native && cargo build --release` + copy). See README.md.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixtures="$repo/tests/runner/fixtures"
addon="$repo/runtime/addons/nub-native.node"
[[ -f "$addon" ]] || { echo "missing $addon — build the addon first"; exit 2; }

pkg_name="$(node -p "require('$repo/npm/runner/package.json').name")"
pkg_version="$(node -p "require('$repo/npm/runner/package.json').version")"

echo "== packing $pkg_name@$pkg_version"
node "$repo/scripts/build-runner-npm.mjs" --addon "$addon" --pack >/dev/null
platform="$(node -p "require('$repo/runtime/loader-platform.cjs').platformKey()")"
root_tgz="$repo/npm/runner/$(echo "$pkg_name" | tr -d '@' | tr '/' '-')-$pkg_version.tgz"
# Derive the platform tarball from its own manifest rather than spelling the
# package name a second time — a rename silently broke the hardcoded form once.
plat_name="$(node -p "require('$repo/npm/runner-$platform/package.json').name")"
plat_tgz="$repo/npm/runner-$platform/$(echo "$plat_name" | tr -d '@' | tr '/' '-')-$pkg_version.tgz"
[[ -f "$root_tgz" && -f "$plat_tgz" ]] || { echo "pack produced no tarballs ($root_tgz, $plat_tgz)"; exit 2; }

work="$(mktemp -d "${TMPDIR:-/tmp}/nub-loader-matrix.XXXXXX")"
trap 'rm -rf "$work"' EXIT
cp -R "$fixtures/." "$work/"
(cd "$work" && npm install --no-audit --no-fund --silent "$root_tgz" "$plat_tgz" @js-temporal/polyfill)
if [[ "${TSX:-0}" == "1" ]]; then
  (cd "$work" && npm install --no-audit --no-fund --silent tsx)
fi

# --require delivery goes through require(esm), which needs 20.19+ / 22.12+.
supports_require() {
  node -e '
    const [a, b] = process.versions.node.split(".").map(Number);
    process.exit((a > 22 || (a === 22 && b >= 12) || (a === 20 && b >= 19)) ? 0 : 1);
  '
}

fail=0
run_one() {  # <label> <argv...>
  local label="$1"; shift
  local fixture="${@: -1}"
  local want got
  want="$(grep "^${fixture}=" "$fixtures/expected.txt" | cut -d= -f2- | sed 's/\\n/\n/g')"
  got="$(cd "$work" && "$@" 2>&1 || true)"
  if [[ "$got" == "$want" ]]; then
    printf "  ok   %-10s %s\n" "$label" "$fixture"
  elif [[ "$label" == "tsx" ]]; then
    # The tsx column is the differential reference, not a gate: a tsx miss is a
    # divergence to read (e.g. tsx does not reach worker threads on Node < 26),
    # never a loader failure.
    printf "  diff %-10s %s\n       got:  %s\n" "$label" "$fixture" "$(echo "$got" | head -c 160 | tr '\n' ' ')"
  else
    printf "  FAIL %-10s %s\n       want: %s\n       got:  %s\n" "$label" "$fixture" "${want//$'\n'/\\n}" "${got//$'\n'/\\n}"
    fail=1
  fi
}

check_eq() {  # <label> <want> <got>
  if [[ "$3" == "$2" ]]; then
    printf "  ok   %-10s %s\n" "nubr" "$1"
  else
    printf "  FAIL %-10s %s\n       want: %s\n       got:  %s\n" \
      "nubr" "$1" "$2" "${3//$'\n'/\\n}"
    fail=1
  fi
}

# Command-surface behavior the fixture table cannot express, each pinned to the
# exact string npm or node produces. All three of these regressed once.
nubr_command_checks() {
  local nubr="./node_modules/.bin/nubr"

  # Forwarded arguments must survive the script shell as literals. A raw join
  # splits "a b", runs `y` out of `x;y`, and expands $HOME.
  check_eq "args-literal" '["a b","x;y","$HOME",""]' \
    "$(cd "$work" && "$nubr" args -- "a b" 'x;y' '$HOME' "" 2>&1)"

  # Node's split-value option form. Enumerating which options take a value is
  # what got this wrong: `--conditions development` swallowed the entrypoint.
  check_eq "opt-split" "hello world red 42" \
    "$(cd "$work" && "$nubr" --conditions development main.ts 2>&1)"
  check_eq "opt-equals" "hello world red 42" \
    "$(cd "$work" && "$nubr" --conditions=development main.ts 2>&1)"

  # The manifest-derived lifecycle environment npm guarantees a script.
  check_eq "lifecycle-env" "nub-run-fixture 9.9.9 json lifecycle" \
    "$(cd "$work" && "$nubr" lifecycle 2>&1)"
}

run_version() {
  echo "== node $(node --version)"
  for f in main.ts paths.ts req.cts using.ts worker-main.ts clobber.ts; do
    run_one "--import" node --import "$pkg_name" "$f"
  done
  if supports_require; then
    for f in main.ts req.cts; do
      run_one "--require" node --require "$pkg_name" "$f"
    done
  else
    echo "  skip --require (no require(esm) on this Node)"
  fi
  # The `nubr` command over the same fixtures. This is the column that catches an
  # entry-dispatch regression: the fixture package is `type: module`, so a `.ts`
  # entry is ESM and must NOT go through Module.runMain, which cannot load an ES
  # module on Node below 22.15. `greet` covers script mode and proves the command
  # is on PATH for a script's own children.
  for f in main.ts paths.ts req.cts using.ts worker-main.ts clobber.ts greet; do
    run_one "nubr" ./node_modules/.bin/nubr "$f"
  done
  nubr_command_checks
  if [[ "${TSX:-0}" == "1" ]]; then
    # paths.ts is excluded: its YAML import is a Nub feature tsx does not have.
    for f in main.ts req.cts using.ts worker-main.ts; do
      run_one "tsx" node --import tsx "$f"
    done
  fi
}

if [[ -n "${NODE_VERSIONS:-}" ]]; then
  for v in $NODE_VERSIONS; do
    bin="$HOME/.nvm/versions/node/v$v/bin"
    [[ -x "$bin/node" ]] || { echo "== node $v: not installed under ~/.nvm — skipping"; continue; }
    PATH="$bin:$PATH" run_version
  done
else
  run_version
fi

exit $fail
