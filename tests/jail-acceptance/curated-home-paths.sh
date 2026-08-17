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
#
# ⛔⛔ THE SECOND SOURCE, AND IT IS NOT A CURATED GRANT: PROMOTION'S DESTINATIONS. `writePaths`
# promotion moves whatever a script left in the baseline write paths out of its throwaway `$HOME` and
# into the real one AFTER the scripts finish, so those directories legitimately gain files on every
# jailed install. Measured, and it read exactly like a confinement hole: a native-heavy project left 693
# files under the real `~/Library/Caches/Homebrew/...`, because a lifecycle script invoked Homebrew and
# `Library/Caches` is a BASELINE write path.
#
# ⛔ THE FALSE "nub's own machinery did it" READING WAS WRONG, AND SO WAS THE FIRST FIX. The premise was
# never tested; the control that settled it was `--ignore-scripts` (693 files with scripts, ZERO
# without), which proved the writes were script-driven, and a probe writing three markers proved
# confinement was nevertheless intact: `Library/Caches` reached the real home, `.ssh` and `Documents`
# did not, all three succeeded inside the private home. Promotion is the mover; the script never held a
# handle on the real home. So the answer is to exclude promotion's DESTINATIONS and keep the check a
# GATE — the earlier instinct to downgrade it to a report would have blinded it to every real escape in
# exchange for silencing one understood one.
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

# The baseline write paths — promotion's designed destinations — read from the constant itself.
#
# Restricted to the `&[ … ]` body of BASELINE_WRITE_PATHS so an unrelated string literal elsewhere in
# the file cannot widen the exclusion set. Widening it silently is the dangerous direction: every path
# added here is a place a real escape could hide.
baseline_write_paths () {
  local src="$1"
  [ -f "$src" ] || { echo "curated-home-paths: cannot read $src" >&2; return 1; }
  # ⛔ ANCHORED TO THE DECLARATION, AND A `sed` RANGE IS NOT ENOUGH. `sed -n '/NAME/,/^];/p'` RESTARTS
  # its range at every later mention of the name — the constant is referenced from several doc comments
  # in this file — so it swept in unrelated literals and produced 69 exclusions including `"APIKEY` and
  # a fragment of an error message. Every one of those is a directory a real escape could then hide in,
  # which is the silent-false-negative direction. awk exits at the first `];` instead, so exactly one
  # range is ever read.
  awk '/^pub const BASELINE_WRITE_PATHS/ {f=1; next} f && /^\];/ {exit} f' "$src" \
    | grep -oE '^[[:space:]]*"[^"]+"' | sed -E 's/^[[:space:]]*"//; s/"$//' | sort -u
}
BASELINE_WRITE_PATHS="$(baseline_write_paths "$_CHP_HERE/../../crates/nub-sandbox/src/catalog_v2.rs")"

# ⛔ POSITIVE CONTROL ON THE DERIVER, because a broken one fails SILENTLY and in the unsafe direction.
# A parser that returns nothing leaves the check flagging designed behaviour; one that over-matches
# blinds it. Both were observed. This refuses to be sourced rather than hand back a polluted set.
_bwp_n="$(printf '%s\n' "$BASELINE_WRITE_PATHS" | grep -c .)"
if [ "$_bwp_n" -lt 4 ] || [ "$_bwp_n" -gt 12 ]; then
  echo "curated-home-paths: BASELINE_WRITE_PATHS parse returned $_bwp_n entries, refusing" >&2
  return 1 2>/dev/null || exit 1
fi
# VALIDATED POSITIVELY, not by blacklisting junk. The blacklist tried first rejected
# `Library/Application Support` for containing a space, which is a real macOS path — so the guard was
# refusing a correct parse. Each entry must LOOK like a relative path instead, which rejects the
# observed pollution (`"APIKEY`, `"}}}}`, a fragment of an error message) without banning a legal name.
while IFS= read -r _bwp; do
  [ -n "$_bwp" ] || continue
  case "$_bwp" in
    /*|*..*) _bwp_bad="$_bwp" ;;
    *[!A-Za-z0-9._\ /-]*) _bwp_bad="$_bwp" ;;
  esac
done <<< "$BASELINE_WRITE_PATHS"
if [ -n "${_bwp_bad:-}" ]; then
  echo "curated-home-paths: BASELINE_WRITE_PATHS parse yielded a non-path entry: $_bwp_bad" >&2
  return 1 2>/dev/null || exit 1
fi

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
  # Promotion's destinations. Excluded as whole subtrees: promotion relocates whatever the script left
  # there, so the specific filenames are not predictable and only the root is.
  while IFS= read -r rel; do
    [ -n "$rel" ] || continue
    CURATED_FIND_ARGS+=(-not -path "./$rel/*")
  done <<< "$BASELINE_WRITE_PATHS"
}
curated_find_args
