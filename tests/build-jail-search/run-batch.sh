#!/usr/bin/env bash
# Run the grant search over a worklist. One JSON record per line to stdout, and one
# `results/runs/<pkg>@<version>.json` per package written by search.mjs itself.
#
# Usage:  ./run-batch.sh <nub> [--force] <pkg@ver> [pkg@ver ...]
#         ./run-batch.sh <nub> [--force] --file <worklist.txt>
#
# RESUMABLE BY DEFAULT. search.mjs skips a package whose run file already exists, so a
# re-run costs only what has not been measured — and a batch killed halfway can simply be
# started again. `--force` re-measures everything, which is what a harness or jail change
# calls for, since a run file records the answer AND the harness hash that produced it.
set -u
NUB="${1:-}"; shift || true
[ -x "$NUB" ] || { echo "usage: $0 <nub> [--force] <pkg@ver>... | --file <list>"; exit 2; }
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac

FORCE=""
[ "${1:-}" = "--force" ] && { FORCE="--force"; shift; }

if [ "${1:-}" = "--file" ]; then
  [ -r "${2:-}" ] || { echo "cannot read worklist ${2:-}"; exit 2; }
  set -- $(grep -vE '^\s*(#|$)' "$2")
fi

here="$(cd "$(dirname "$0")" && pwd)"
for spec in "$@"; do
  timeout 2400 node "$here/search.mjs" "$spec" --nub "$NUB" $FORCE 2>/dev/null \
    || echo "{\"pkg\":\"$spec\",\"verdict\":\"HARNESS-TIMEOUT-OR-CRASH\"}"
done
