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
#
# ⛔⛔ THIS USED TO BE `.config/evil`, AND THAT WAS WRONG ONCE PROMOTION SHIPPED. `.config` is a BASELINE
# write path, so a file a script leaves at `.config/evil` in its throwaway `$HOME` is relocated into the
# real home BY NUB, deliberately — the doc comment on `persist_declared_home_writes` says so outright:
# "nub copies whatever landed there". Asserting that path is an escape asserts that nub's own designed
# behaviour is a breach, and the only way to make it pass is to blind the check to a promotion
# destination. `.ssh` is outside every promoted and curated path, so it is rogue under any policy.
mkdir -p "$t/.ssh/evil"; : > "$t/.ssh/evil/marker"

found="$(cd "$t" && find . -type f "${CURATED_FIND_ARGS[@]}" 2>/dev/null | sort | tr '\n' ' ')"

case "$found" in
  *"./.ssh/evil/marker"*) echo "PASS  the rogue write is reported" ;;
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

# A write inside a PROMOTION DESTINATION is not an escape, and this pins it so nobody re-adds the
# `.config/evil` case above. What stops a script abusing this is not the path list: it is that the script
# holds NO live handle on the real home (an absolute-path write to the real `~/.ssh` returns EPERM,
# measured) plus `merge_into`'s only-ever-add invariant, which means promotion can add a file the user did
# not have but can never modify or replace one they did. That invariant is a SECURITY property, not
# merely a data-integrity one, and `promotion_never_overwrites_what_the_destination_already_has` in
# `build_jail.rs` is the test that holds it.
mkdir -p "$t/.config/vendor"; : > "$t/.config/vendor/settings.json"
found2="$(cd "$t" && find . -type f "${CURATED_FIND_ARGS[@]}" 2>/dev/null | sort | tr '\n' ' ')"
case "$found2" in
  *"./.config/vendor/settings.json"*) echo "FAIL  a promotion destination was reported as an escape — the suite would fail on designed behaviour"; fail=1 ;;
  *) echo "PASS  a write inside a promotion destination is not an escape" ;;
esac

RC="$fail"

# ── Promotion's destinations, and the deriver that reads them ────────────────────────────────────
#
# These exist because the escape check reported 693 real-home files as a confinement failure and the
# first fix was to downgrade the check to a report, on the untested premise that nub's own machinery
# had written them. It had not: `--ignore-scripts` produced ZERO of those files, and a three-marker
# probe showed `Library/Caches` reaching the real home while `.ssh` and `Documents` did not. Promotion
# was the mover. A check that cannot tell promotion from an escape gets talked into being toothless.

t_baseline_derived_from_the_constant () {
  local n; n="$(printf '%s\n' "$BASELINE_WRITE_PATHS" | grep -c .)"
  # Every entry of the constant must be here — a partial parse silently narrows the exclusion set and
  # the suite then fails on designed behaviour.
  for want in .cache .config .npm .electron "AppData/Local" "Library/Caches" "Library/Application Support"; do
    printf '%s\n' "$BASELINE_WRITE_PATHS" | grep -qxF "$want" || { echo "FAIL  baseline missing $want"; return 1; }
  done
  [ "$n" -eq 7 ] || { echo "FAIL  baseline derived $n entries, expected 7 — the constant moved, update this"; return 1; }
  echo "PASS  promotion destinations derived from BASELINE_WRITE_PATHS"
}

t_the_deriver_refuses_a_polluted_parse () {
  # A NEGATIVE CONTROL on the parser itself. The first implementation used a `sed` address range, which
  # restarts at every later mention of the constant's name and swept in 69 entries including `"APIKEY`
  # and `"}}}}`. Each of those is a directory a real escape could then hide inside, so this fails in the
  # direction that matters and must stay tested.
  local ctl; ctl="$(mktemp -d)"
  mkdir -p "$ctl/tests/jail-acceptance" "$ctl/crates/nub-sandbox/src/compiler"
  cp "$HERE/curated-home-paths.sh" "$ctl/tests/jail-acceptance/"
  cp "$HERE/../../crates/nub-sandbox/src/compiler/curated.rs" "$ctl/crates/nub-sandbox/src/compiler/"
  sed 's|pub const BASELINE_WRITE_PATHS: &\[&str\] = &\[|pub const BASELINE_WRITE_PATHS: \&[\&str] = \&[\n    "APIKEY{}",|' \
    "$HERE/../../crates/nub-sandbox/src/catalog_v2.rs" > "$ctl/crates/nub-sandbox/src/catalog_v2.rs"
  local out; out="$( cd "$ctl/tests/jail-acceptance" && bash -c '. ./curated-home-paths.sh' 2>&1 )"
  rm -rf "$ctl"
  case "$out" in
    *"non-path entry"*) echo "PASS  the deriver refuses a polluted constant" ;;
    *) echo "FAIL  a polluted constant was ACCEPTED — the exclusion set can be widened silently"; return 1 ;;
  esac
}

t_baseline_derived_from_the_constant || RC=1
t_the_deriver_refuses_a_polluted_parse || RC=1
exit "${RC:-0}"
