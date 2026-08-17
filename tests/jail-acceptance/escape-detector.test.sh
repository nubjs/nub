#!/usr/bin/env bash
# Does the escape detector fire on a rogue write and stay silent on a curated one?
#
# ⛔⛔ WHY THIS EXISTS. The detector excludes nub's own trees AND the curated `home_paths` that nub grants
# on purpose — so after adding the curated exclusion, the one package that had ever tripped it
# (`puppeteer`, whose browser cache is a DESIGNED real-home write) no longer does. That left a detector
# with no demonstrated failing case, which is a detector nobody has seen work. This gives it one without
# needing an install: build a home by hand and assert which files the predicate reports.
#
# It also pins the polarity that matters. A false NEGATIVE here is silent and permanent — a real escape
# scrolls past as a green run — so the rogue case is asserted first and by name.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/curated-home-paths.sh"

fail=0
t="$(mktemp -d "${TMPDIR:-/tmp}/escape-det-XXXXXX")"
trap 'rm -rf "$t"' EXIT

# nub's own bookkeeping — must be ignored on all three of its trees.
mkdir -p "$t/.cache/nub/pm" "$t/.local/share/nub" "$t/Library/Caches/nub"
: > "$t/.cache/nub/pm/store-index.json"; : > "$t/.local/share/nub/x"; : > "$t/Library/Caches/nub/y"
# A CURATED home path — a real-home write nub grants deliberately. Must be ignored.
mkdir -p "$t/.cache/puppeteer/chrome"; : > "$t/.cache/puppeteer/chrome/big.zip"
# A ROGUE write — nobody granted this. MUST be reported.
mkdir -p "$t/.config/evil"; : > "$t/.config/evil/marker"

found="$(cd "$t" && find . -type f "${CURATED_FIND_ARGS[@]}" 2>/dev/null | sort | tr '\n' ' ')"

case "$found" in
  *"./.config/evil/marker"*) echo "PASS  the rogue write is reported" ;;
  *) echo "FAIL  the rogue write was NOT reported — the detector cannot catch an escape: [$found]"; fail=1 ;;
esac
case "$found" in
  *puppeteer*) echo "FAIL  a CURATED home path was reported as an escape — the suite would fail on designed behaviour"; fail=1 ;;
  *) echo "PASS  the curated puppeteer cache is ignored" ;;
esac
case "$found" in
  *"/nub/"*) echo "FAIL  nub's own bookkeeping was reported: [$found]"; fail=1 ;;
  *) echo "PASS  nub's own three trees are ignored" ;;
esac
# The derivation must actually have found something; an empty list would silently exclude nothing and
# make the curated assertion above pass for the wrong reason.
[ -n "$CURATED_HOME_PATHS" ] && echo "PASS  curated paths were derived from curated.rs" \
  || { echo "FAIL  no curated paths derived — the exclusion list is empty"; fail=1; }

exit "$fail"
