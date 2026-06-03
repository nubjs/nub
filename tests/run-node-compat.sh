#!/bin/bash
# Node.js compatibility test runner for Nub.
# Runs tests from tests/node-suite/test/ through both `node` and `nub`,
# comparing exit codes and output.
#
# Usage: ./tests/run-node-compat.sh [--mode nub|node|both] [--filter pattern]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SUITE_DIR="$REPO_DIR/tests/node-suite/test"
CONFIG="$REPO_DIR/tests/node-compat-config.jsonc"
NUB="$REPO_DIR/target/release/nub"

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
const stripped = src.replace(/\/\/.*$/gm, '').replace(/\/\*[\s\S]*?\*\//g, '');
const config = JSON.parse(stripped);
for (const [path, opts] of Object.entries(config)) {
  if (opts.ignore) continue;
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
    if (cd "$SUITE_DIR" && timeout 10 node "$test" >/dev/null 2>&1); then
      passed_node=$((passed_node + 1))
    else
      echo "FAIL (node) $test"
      failed_node=$((failed_node + 1))
    fi
  fi

  if [ "$MODE" = "nub" ] || [ "$MODE" = "both" ]; then
    if (cd "$SUITE_DIR" && NODE_TEST_KNOWN_GLOBALS=0 timeout 10 "$NUB" "$test" >/dev/null 2>&1); then
      passed_nub=$((passed_nub + 1))
    else
      echo "FAIL (nub)  $test"
      failed_nub=$((failed_nub + 1))
    fi
  fi
done

echo ""
echo "=== Results ==="
if [ "$MODE" = "node" ] || [ "$MODE" = "both" ]; then
  total_node=$((passed_node + failed_node))
  echo "node: $passed_node/$total_node passed ($failed_node failed)"
fi
if [ "$MODE" = "nub" ] || [ "$MODE" = "both" ]; then
  total_nub=$((passed_nub + failed_nub))
  echo "nub:  $passed_nub/$total_nub passed ($failed_nub failed)"
fi
echo "skipped: $skipped"
