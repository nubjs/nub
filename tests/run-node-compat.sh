#!/bin/bash
# Node.js compatibility test runner for Nub.
# Runs tests from tests/node-suite/test/ through both `node` and `nub`,
# comparing exit codes and output.
#
# Usage: ./tests/run-node-compat.sh [--mode nub|node|both] [--filter pattern]
#
# Measurement policy -- the number is only worth having if it is honest:
#   1. Test files are NEVER modified. The corpus is the pinned submodule,
#      verbatim. No commented-out assertion, no injected skip.
#   2. Exemptions are DECLARED, in node-compat-config.jsonc, with a reason --
#      never embedded in a test file where they read as a pass.
#   3. Both runtimes get the SAME environment. An exemption applied to nub but
#      not to node measures the exemption, not compatibility.
#   4. The headline rate counts EVERY test in the corpus. The curated rate,
#      which excludes declared divergences, is reported second and never alone.
#
# TEST_TIMEOUT (default 60s) matches the scale Node's own runner uses; a short
# timeout manufactures failures on tests that legitimately take 10-25s.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SUITE_DIR="$REPO_DIR/tests/node-suite/test"
CONFIG="$REPO_DIR/tests/node-compat-config.jsonc"
NUB="$REPO_DIR/target/release/nub"

TEST_TIMEOUT="${TEST_TIMEOUT:-60}"
MODE="${1:-both}"
FILTER="${2:-}"

if [ ! -d "$SUITE_DIR" ]; then
  echo "Error: Node test suite not found at $SUITE_DIR"
  echo "Run: git submodule update --init --depth 1"
  exit 1
fi

if [ ! -f "$NUB" ]; then
  echo "Building nub (release)..."
  cargo build --release -p nub-cli
fi

# Parse config.jsonc (strip comments, extract test paths)
TESTS=$(node -e "
const fs = require('fs');
const src = fs.readFileSync('$CONFIG', 'utf8');
// String-aware comment strip: a bare /\\/\\/.*\$/ also eats a '//' that occurs
// INSIDE a string, and reason strings legitimately contain one.
function strip(src) {
  let out = '', i = 0, inStr = false, esc = false;
  while (i < src.length) {
    const c = src[i];
    if (inStr) { out += c; if (esc) esc = false; else if (c === '\\\\') esc = true; else if (c === '\"') inStr = false; i++; continue; }
    if (c === '\"') { inStr = true; out += c; i++; continue; }
    if (c === '/' && src[i + 1] === '/') { while (i < src.length && src[i] !== '\n') i++; continue; }
    if (c === '/' && src[i + 1] === '*') { i += 2; while (i < src.length && !(src[i] === '*' && src[i + 1] === '/')) i++; i += 2; continue; }
    out += c; i++;
  }
  return out.replace(/,(\s*[}\]])/g, '\$1');
}
const config = JSON.parse(strip(src));
// RUN_IGNORED=1 → run the WHOLE corpus with zero skips (the raw, no-exclusions
// conformance number); default still honors the curated ignore list.
const runIgnored = process.env.RUN_IGNORED === '1';
for (const [path, opts] of Object.entries(config)) {
  if (opts.ignore && !runIgnored) continue;
  console.log(path);
}
")

passed_node=0
failed_node=0
passed_nub=0
failed_nub=0
skipped=0

for test in $TESTS; do
  if [ -n "$FILTER" ] && [[ "$test" != *"$FILTER"* ]]; then
    continue
  fi

  full="$SUITE_DIR/$test"
  if [ ! -f "$full" ]; then
    echo "SKIP $test (not found)"
    skipped=$((skipped + 1))
    continue
  fi

  if [ "$MODE" = "node" ] || [ "$MODE" = "both" ]; then
    if (cd "$SUITE_DIR" && timeout "$TEST_TIMEOUT" node "$test" >/dev/null 2>&1); then
      passed_node=$((passed_node + 1))
    else
      echo "FAIL (node) $test"
      failed_node=$((failed_node + 1))
    fi
  fi

  if [ "$MODE" = "nub" ] || [ "$MODE" = "both" ]; then
    # No NODE_TEST_KNOWN_GLOBALS=0 here. It disables test/common's global-leak
    # check, and setting it for nub but not for node suppressed exactly the
    # failures nub's preload causes -- an exemption that read as a pass.
    if (cd "$SUITE_DIR" && timeout "$TEST_TIMEOUT" "$NUB" "$test" >/dev/null 2>&1); then
      passed_nub=$((passed_nub + 1))
    else
      echo "FAIL (nub)  $test"
      failed_nub=$((failed_nub + 1))
    fi
  fi
done

# CORPUS_TOTAL is every test file in the tracked directories, independent of the
# config. Rate 1 below is measured against it, so a divergence moved into the
# ignore list can never raise the headline number.
CORPUS_TOTAL=$(cd "$SUITE_DIR" && ls parallel sequential es-module module-hooks 2>/dev/null \
  | grep -cE '\.(m?js|cjs)$' || echo 0)
IGNORED=$(( CORPUS_TOTAL - $(printf '%s\n' "$TESTS" | grep -c . ) ))

pct() { [ "$2" -eq 0 ] && { echo "n/a"; return; }; awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f%%", a*100/b}'; }

echo ""
echo "=== Results ==="
echo "corpus: $CORPUS_TOTAL tests | declared divergences (ignored): $IGNORED"
if [ "$MODE" = "node" ] || [ "$MODE" = "both" ]; then
  total_node=$((passed_node + failed_node))
  echo "node: $passed_node passed  $(pct $passed_node $CORPUS_TOTAL) of corpus  |  $(pct $passed_node $total_node) of run  ($failed_node failed)"
fi
if [ "$MODE" = "nub" ] || [ "$MODE" = "both" ]; then
  total_nub=$((passed_nub + failed_nub))
  echo "nub:  $passed_nub passed  $(pct $passed_nub $CORPUS_TOTAL) of corpus  |  $(pct $passed_nub $total_nub) of run  ($failed_nub failed)"
fi
echo "not found: $skipped"
echo ""
echo "The FIRST rate is the headline: it counts every test in the corpus, so"
echo "ignoring a test cannot improve it. The second excludes declared"
echo "divergences and is never to be quoted on its own."
