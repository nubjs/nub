#!/usr/bin/env bash
# Portable Unix launcher for driver.mjs. The candidate runs the driver so this
# corpus uses Nub's normal Node interface rather than assuming a system npm.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ORIGINAL_ARGS=("$@")
NUB="${NUB_NATIVE_ISLAND_NUB:-}"
if [ -z "$NUB" ]; then
  for ((i = 0; i < ${#ORIGINAL_ARGS[@]}; i += 1)); do
    if [ "${ORIGINAL_ARGS[$i]}" = "--nub" ]; then
      (( i + 1 < ${#ORIGINAL_ARGS[@]} )) || { echo "error: --nub needs a value" >&2; exit 2; }
      NUB="${ORIGINAL_ARGS[$((i + 1))]}"
      break
    fi
  done
fi
[ -n "$NUB" ] || { echo "usage: $0 --nub <candidate> --launcher <matching-launcher> --target <node-version> [--platform <triple>] [--mode prebuilt]" >&2; exit 2; }
"$NUB" "$HERE/driver.mjs" "${ORIGINAL_ARGS[@]}"
