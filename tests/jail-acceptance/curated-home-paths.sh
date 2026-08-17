#!/usr/bin/env bash
# The real-home directories nub grants ON PURPOSE, derived from the source of truth.
#
# ⛔⛔ WHY THE ESCAPE CHECK NEEDS THIS AT ALL. `compiler/curated.rs` gives some packages a curated
# `home_paths` entry: nub sets an env var pointing the package at a directory in the user's REAL home
# and grants write to it, so the artefact persists across installs. puppeteer's browser cache
# (`PUPPETEER_CACHE_DIR` → `~/.cache/puppeteer`) is the case that proved this — a jailed install writes
# hundreds of megabytes there BY DESIGN, and an escape check that does not know it reports intended
# behaviour as a confinement failure. That misreading cost a whole investigation in this effort: five
# hypotheses were refuted before the curated entry was found.
#
# ⛔ DERIVED, NOT COPIED. A hardcoded list drifts the moment someone adds a curated package, and it
# drifts SILENTLY in the dangerous direction: a stale list makes the check flag a legitimate write and
# the suite goes red on designed behaviour, so the pressure is to loosen the check rather than update
# the list. Reading `curated.rs` means adding a curated package updates this automatically.
#
# ⛔ ONLY THE CONCRETE `~/…` FORMS ARE EXCLUDED, deliberately. Some entries are written against a
# `$cache` variable, and excluding a whole platform cache root would blind the check to real escapes —
# false NEGATIVES being the direction that matters here, since the check exists to catch writes nobody
# intended. If a `$cache`-based curated grant ever trips the check, resolve it by naming the concrete
# path, never by widening this.
#
# Usage: `. curated-home-paths.sh` then use `$CURATED_HOME_PATHS` (newline-separated, `~/` stripped).

curated_home_paths () {
  local src="$1"
  [ -f "$src" ] || { echo "curated-home-paths: cannot read $src" >&2; return 1; }
  # `macos:`/`linux:`/`windows:` each carry a per-OS spelling; take every concrete `~/…` value from all
  # three, because the same directory can be named on one platform and templated on another.
  grep -oE '(macos|linux|windows): Some\("~/[^"]+"\)' "$src" \
    | sed -E 's/^(macos|linux|windows): Some\("~\///; s/"\)$//' \
    | sort -u
}

# Resolve against the repo this script ships in, so a caller's cwd cannot change the answer.
_CHP_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CURATED_HOME_PATHS="$(curated_home_paths "$_CHP_HERE/../../crates/nub-sandbox/src/compiler/curated.rs")"

# The find(1) predicate that prunes nub's own trees plus every curated path, as an ARRAY.
#
# ⛔⛔ AN ARRAY, NOT A STRING, AND THE STRING VERSION WAS SILENTLY BROKEN. An unquoted command
# substitution undergoes PATHNAME EXPANSION as well as word splitting, so a pattern like
# `./.cache/puppeteer/*` was glob-expanded against the caller's cwd before `find` ever saw it — the
# predicate that reached find named one existing subdirectory instead of the whole subtree, and the
# exclusion quietly did not apply. Caught by `escape-detector.test.sh`, which reported a curated path as
# an escape; without that test this would have failed the suite on designed behaviour and the obvious
# "fix" would have been to loosen the check.
#
# Populates `CURATED_FIND_ARGS`; use it quoted, as `"${CURATED_FIND_ARGS[@]}"`.
curated_find_args () {
  # nub's own three trees first: `.cache/nub`, `.local/share/nub` and `Library/Caches/nub` all match
  # `*/nub/*`, and their bare container directories hold no files of their own.
  CURATED_FIND_ARGS=(-not -path '*/nub/*')
  local rel
  while IFS= read -r rel; do
    [ -n "$rel" ] || continue
    CURATED_FIND_ARGS+=(-not -path "./$rel/*")
  done <<< "$CURATED_HOME_PATHS"
}
curated_find_args
