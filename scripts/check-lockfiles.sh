#!/usr/bin/env sh
# check-lockfiles — verify every tracked Cargo.lock still satisfies its manifest.
#
# Usage:
#   scripts/check-lockfiles.sh          # verify; exit 1 if any lock is stale
#   scripts/check-lockfiles.sh --fix    # re-resolve the stale ones, then verify
#
# WHY THIS IS A SCRIPT AND NOT A SNIPPET. This repo has FIVE tracked lockfiles in
# four separate Cargo workspaces plus a path dependency, and only some of them have
# a `--locked` gate in CI — so a lock with no gate is exactly the one that rots
# (`crates/nub-phantom` did, until 2026-09-02). The honest enumeration is
# `git ls-files`, never the list of gates and never a hardcoded array, so this
# derives it and cannot drift as workspaces are added or removed.
#
# THE BUG THIS REPLACES. The previous recipe lived in AGENTS.md and ran
# `cargo metadata --locked --offline`, treating any non-zero exit as "STALE". That
# conflates two unrelated failures, and the wrong one is the common one:
#
#   stale lock   error: cannot update the lock file ... because --locked was passed
#   cold cache   error: failed to download `x` / attempting to make an HTTP request,
#                but --offline was specified
#
# A fresh checkout, a fresh container or a CI runner has no populated crate cache,
# so `--offline` fails on the FIRST uncached crate and the recipe reported all five
# locks stale on a tree where every one of them was clean. That is a false positive
# in the expensive direction: it invents a dependency problem that does not exist,
# and the reflex fix (regenerating locks that were fine) writes real churn into a
# commit. Measured 2026-09-15 in a fresh container, where it reported 5/5 stale and
# every lock verified clean once the network was allowed.
#
# So this distinguishes three outcomes rather than two, and the third is the point:
#   ok          the lock satisfies the manifest
#   STALE       cargo says it must update the lock — a real finding, exit 1
#   unverified  neither the cache nor the network could answer — say so, do not
#               claim staleness, and do not block (the same posture .githooks uses
#               when `lat check` cannot run)
#
# Offline is tried first because it is fast on a warm cache (~0.2s per workspace)
# and needs no network; the online retry only runs when offline could not answer.
set -u

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "check-lockfiles: not inside a git repository" >&2
  exit 2
}
cd "$root" || exit 2

fix=0
case "${1:-}" in
  --fix) fix=1 ;;
  "") ;;
  *) echo "check-lockfiles: unknown argument '$1' (expected --fix)" >&2; exit 2 ;;
esac

# The tracked locks, as paths to the directory that owns each one. `git ls-files`
# is the authoritative list; a lock that is untracked is not ours to verify.
dirs=$(git ls-files '*/Cargo.lock' 'Cargo.lock' | while IFS= read -r lock; do
  d=$(dirname "$lock")
  [ "$d" = "." ] && echo "." || echo "$d"
done)

if [ -z "$dirs" ]; then
  echo "check-lockfiles: no tracked Cargo.lock found" >&2
  exit 2
fi

err=$(mktemp) || exit 2
trap 'rm -f "$err"' EXIT INT TERM

# Cargo's own words for "the lock must change", which is the only failure that
# means the lock is stale. Matched on the message rather than the exit code
# because every other failure also exits 101.
stale_msg='cannot update the lock file'

# Verify one directory. Echoes the verdict; returns 0 ok, 1 stale, 2 unverified.
verify() {
  # `--format-version 1` silences cargo's "please specify" warning, which would
  # otherwise land in the captured stderr and muddy the match below.
  if (cd "$1" && cargo metadata --locked --offline --format-version 1) >/dev/null 2>"$err"; then
    return 0
  fi
  if grep -qF "$stale_msg" "$err"; then
    return 1
  fi
  # Offline could not answer — almost always an unpopulated crate cache. Ask
  # again with the network before saying anything about this lock.
  if (cd "$1" && cargo metadata --locked --format-version 1) >/dev/null 2>"$err"; then
    return 0
  fi
  if grep -qF "$stale_msg" "$err"; then
    return 1
  fi
  return 2
}

status=0
unverified=0
for d in $dirs; do
  verify "$d"
  case $? in
    0) printf '  ok          %s\n' "$d" ;;
    1)
      if [ "$fix" = 1 ]; then
        if (cd "$d" && cargo metadata --format-version 1) >/dev/null 2>"$err" && verify "$d"; then
          printf '  regenerated %s\n' "$d"
        else
          printf '  STALE       %s (could not re-resolve)\n' "$d"
          sed 's/^/                /' "$err" | head -4
          status=1
        fi
      else
        printf '  STALE       %s\n' "$d"
        status=1
      fi
      ;;
    *)
      printf '  unverified  %s (no crate cache and no network — not a staleness claim)\n' "$d"
      sed 's/^/                /' "$err" | head -3
      unverified=1
      ;;
  esac
done

if [ "$status" = 1 ]; then
  echo
  echo "A lock above does not satisfy its manifest. Re-resolve it without compiling:"
  echo "    scripts/check-lockfiles.sh --fix"
  echo "and commit the changed Cargo.lock — CI's --locked gates fail on it otherwise."
elif [ "$unverified" = 1 ]; then
  echo
  echo "Some locks could not be checked. That is not a failure of the tree; re-run"
  echo "with a crate cache or network access to get an answer."
fi

exit "$status"
