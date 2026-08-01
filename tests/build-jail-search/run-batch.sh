#!/usr/bin/env bash
# Run the grant search over a worklist, one JSON record per line to stdout.
# Usage: ./run-batch.sh <nub> <pkg@ver> [pkg@ver ...]
NUB="$1"; shift
for spec in "$@"; do
  timeout 2400 node "$(dirname "$0")/search.mjs" "$spec" --nub "$NUB" 2>/dev/null \
    || echo "{\"pkg\":\"$spec\",\"verdict\":\"HARNESS-TIMEOUT-OR-CRASH\"}"
done
