#!/bin/sh
# Drives scripts/target-gc.sh against a fake cache root and fake git worktrees,
# so every collection rule can be asserted in seconds without touching the
# machine's real ~/.cache/nub. Ages are planted with touch -t; worktree
# resolution goes through a stub scripts/rust-build.sh in each fake worktree,
# exactly the interface the real collector uses.
#
#   tests/target-gc/run.sh            # all scenarios, ~10s
#   tests/target-gc/run.sh orphan age # a subset
#
# shellcheck disable=SC2016  # check() exprs are eval'd later; single quotes are the point
set -u

here=$(cd "$(dirname "$0")" && pwd)
GC="$here/../../scripts/target-gc.sh"
T=$(mktemp -d "${TMPDIR:-/tmp}/target-gc-test.XXXXXX") || exit 1
trap 'rm -rf "$T"' EXIT

fails=0
check() {
  if eval "$2" 2>/dev/null; then echo "  ok   $1"; else echo "  FAIL $1"; fails=$((fails + 1)); fi
}
want=" $* "
run() { [ "$want" = "  " ] || case $want in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# touch -t stamps: N hours / days in the past, both date dialects.
ago_h() { date -v-"$1"H +%Y%m%d%H%M 2>/dev/null || date -d "$1 hours ago" +%Y%m%d%H%M; }
ago_d() { date -v-"$1"d +%Y%m%d%H%M 2>/dev/null || date -d "$1 days ago" +%Y%m%d%H%M; }

# A bucket/target whose top level AND contents carry the given stamp — the
# in-use probe reads three levels deep, so a fresh inner file would pin it.
# `find -exec touch` stamps only what exists; a bare `touch -t list...` would
# CREATE any missing name, silently planting an rlib in every bucket.
plant() { # path stamp [rlib]
  mkdir -p "$1/fast"
  : > "$1/fast/artifact"
  [ "${3:-}" = rlib ] && : > "$1/fast/libx.rlib"
  find "$1" -exec touch -t "$2" {} + 2>/dev/null
}

# One fake repo with two worktrees, both of whose stub wrappers resolve to the
# `live` bucket, so the collector sees one referenced bucket and two real
# checkouts whose target/ dirs it may scan. Recreated per scenario.
reset() {
  rm -rf "$T/cache" "$T/repo" "$T/wt1" "$T/wt2"
  mkdir -p "$T/cache"
  git init -q "$T/repo" && (cd "$T/repo" && git commit -q --allow-empty -m x \
    && git worktree add -q --detach "$T/wt1" && git worktree add -q --detach "$T/wt2") || exit 1
  for _w in wt1 wt2; do
    mkdir -p "$T/$_w/scripts"
    printf '#!/bin/sh\necho "%s"\n' "$T/cache/shared-target-live" > "$T/$_w/scripts/rust-build.sh"
  done
}
gc() { (cd "$T/repo" && NUB_GC_CACHE_ROOT="$T/cache" NUB_GC_FREE_MIN_GIB=0 sh "$GC" "$@" 2>"$T/gc.err"); }

if run orphan; then
  echo "orphan: an unreferenced bucket is collected after a day; a referenced one is kept"
  reset
  plant "$T/cache/shared-target-live" "$(ago_h 30)"
  plant "$T/cache/shared-target-dead" "$(ago_h 30)"
  plant "$T/cache/shared-target-young" "$(ago_h 3)"
  gc
  check "the 30h unreferenced bucket is gone" '[ ! -d "$T/cache/shared-target-dead" ]'
  check "the bucket a worktree resolves to survives" '[ -d "$T/cache/shared-target-live" ]'
  check "a 3h unreferenced bucket survives (under NUB_GC_ORPHAN_HOURS)" '[ -d "$T/cache/shared-target-young" ]'
fi

if run seed; then
  echo "seed: the newest rlib-bearing bucket survives even as an orphan"
  reset
  plant "$T/cache/shared-target-seed" "$(ago_h 30)" rlib
  gc
  check "the rlib seed bucket survives" '[ -d "$T/cache/shared-target-seed" ]'
fi

if run inuse; then
  echo "inuse: a fresh write inside pins a bucket whose top level looks old"
  reset
  plant "$T/cache/shared-target-busy" "$(ago_h 30)"
  : > "$T/cache/shared-target-busy/fast/fresh"
  gc
  check "the bucket with a fresh inner write survives" '[ -d "$T/cache/shared-target-busy" ]'
fi

if run age; then
  echo "age: a worktree target idle 14 days is collected; a fresh one is kept"
  reset
  plant "$T/wt1/target" "$(ago_d 15)"
  gc
  check "the 15d-idle worktree target is gone" '[ ! -d "$T/wt1/target" ]'
  reset
  plant "$T/wt1/target" "$(ago_d 3)"
  gc
  check "a 3d-idle worktree target survives" '[ -d "$T/wt1/target" ]'
fi

if run keep; then
  echo "keep: NUB_GC_KEEP shields a directory from every pass"
  reset
  plant "$T/cache/shared-target-dead" "$(ago_h 30)"
  (cd "$T/repo" && NUB_GC_CACHE_ROOT="$T/cache" NUB_GC_FREE_MIN_GIB=0 \
    NUB_GC_KEEP="$T/cache/shared-target-dead" sh "$GC" 2>/dev/null)
  check "the kept orphan survives" '[ -d "$T/cache/shared-target-dead" ]'
fi

if run pressure; then
  echo "pressure: orphans go at any age, idle dirs oldest-first, the newest non-orphan is spared"
  reset
  plant "$T/cache/shared-target-live" "$(ago_h 3)"
  plant "$T/cache/shared-target-young" "$(ago_h 3)"
  plant "$T/wt1/target" "$(ago_h 40)"
  plant "$T/wt2/target" "$(ago_h 26)"
  (cd "$T/repo" && NUB_GC_CACHE_ROOT="$T/cache" NUB_GC_FREE_MIN_GIB=0 \
    NUB_GC_PRESSURE=1 sh "$GC" 2>/dev/null)
  check "a young orphan bucket is collected under pressure (never spared)" '[ ! -d "$T/cache/shared-target-young" ]'
  check "the oldest idle worktree target is collected under pressure" '[ ! -d "$T/wt1/target" ]'
  check "the newest at-risk non-orphan is spared" '[ -d "$T/wt2/target" ]'
  check "a recently touched referenced bucket is untouched by pressure" '[ -d "$T/cache/shared-target-live" ]'
fi

if run noroots; then
  echo "noroots: with no worktree resolutions the orphan rule disables itself"
  reset
  plant "$T/cache/shared-target-dead" "$(ago_h 30)"
  mkdir -p "$T/nowhere"
  (cd "$T/nowhere" && NUB_GC_CACHE_ROOT="$T/cache" NUB_GC_FREE_MIN_GIB=0 sh "$GC" 2>/dev/null)
  check "an unreferenced bucket survives when references are unknowable" '[ -d "$T/cache/shared-target-dead" ]'
fi

if run dryrun; then
  echo "dryrun: --dry-run reports and deletes nothing"
  reset
  plant "$T/cache/shared-target-dead" "$(ago_h 30)"
  gc --dry-run
  check "the candidate is reported" 'grep -q "would collect .*shared-target-dead" "$T/gc.err"'
  check "nothing was deleted" '[ -d "$T/cache/shared-target-dead" ]'
fi

if run off; then
  echo "off: NUB_TARGET_GC=0 disables collection"
  reset
  plant "$T/cache/shared-target-dead" "$(ago_h 30)"
  (cd "$T/repo" && NUB_GC_CACHE_ROOT="$T/cache" NUB_TARGET_GC=0 sh "$GC" 2>/dev/null)
  check "the orphan survives with collection off" '[ -d "$T/cache/shared-target-dead" ]'
fi

if run lock; then
  echo "lock: a live sibling collector wins; a stale lock is taken over"
  reset
  plant "$T/cache/shared-target-dead" "$(ago_h 30)"
  mkdir -p "$T/cache/target-gc.lock"
  gc
  check "a fresh lock blocks collection" '[ -d "$T/cache/shared-target-dead" ]'
  touch -t "$(ago_h 1)" "$T/cache/target-gc.lock"
  gc
  check "a stale lock is taken over and collection proceeds" '[ ! -d "$T/cache/shared-target-dead" ]'
  check "the lock is released afterwards" '[ ! -d "$T/cache/target-gc.lock" ]'
fi

echo
if [ "$fails" = 0 ]; then echo "target-gc: all scenarios passed"; else echo "target-gc: $fails assertion(s) FAILED"; exit 1; fi
