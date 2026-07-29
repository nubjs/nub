#!/usr/bin/env sh
# rust-build — pick the correct CARGO_TARGET_DIR for this worktree, then exec cargo.
#
# Usage (drop-in for cargo, from inside any worktree or the main tree):
#   scripts/rust-build.sh build -p nub-cli --profile fast
#   scripts/rust-build.sh test  -p nub-cli
#   scripts/rust-build.sh clippy --all-targets --all-features -- -D warnings
#
# WHY A SHARED TARGET DIR AT ALL. All worktrees default to ONE cargo target dir
# (~/.cache/nub/shared-target) so a fresh worktree reuses the crates.io dependency
# rlibs another worktree already compiled (the bulk of a build) and recompiles only
# the ~3 workspace crates, instead of paying a ~3-min cold build. Sharing one live
# path is what lets concurrent worktrees keep converging on one warm cache.
#
# WHAT DOES *NOT* DEFEAT REUSE: RELOCATION. This script used to claim that "rustc
# bakes the target path into its fingerprints, so a private or CoW-cloned dir gets
# a 0% hit." That is FALSE, and it was expensive — it is why isolation was treated
# as synonymous with a cold build. Measured on this workspace with controls in both
# directions: clone a warm target dir to a NEW path and build there → 0 crates
# rebuilt; a genuinely empty dir → all 13; touch one source in the clone → exactly 1.
# Cargo revalidates a relocated target dir in place. The true statement is about
# SCCACHE, which keys on the rustc command line — that embeds absolute --out-dir /
# -L dependency= paths, so a different target dir is a guaranteed miss there. The
# two mechanisms were conflated. Consequence: an isolated worktree can be SEEDED
# from the shared cache and skip the cold build entirely (see SEEDING below).
#
# THE HAZARD THAT SHARING CREATES. Cargo names a crate's output by package id
# (name + version), NOT by source content. Two worktrees whose source for the SAME
# depended-on crate differs — classically vendor/aube on divergent branches — write
# the same output slot and clobber each other. A dependent crate then links the
# stale rlib and fails to compile against source that is actually correct: the
# phantom "E0063: missing field" class of error, pointing at a field that exists
# nowhere in your checkout. It only bites crates that OTHER crates link; a divergent
# leaf binary (nub-cli) just rebuilds cleanly and is safe to share.
#
# THE RULE (what this script enforces). Share while this worktree's depended-on
# crate CONTENT matches the other sharers', and isolate otherwise. The bucket is
# keyed by a hash of that content, so "same path" IMPLIES "same content" by
# construction rather than by inference. A per-worktree merge-base cannot carry
# that weight: it proves only that THIS worktree made no local changes vs ITS OWN
# base, so two worktrees whose bases straddle a nub-core/aube commit both read
# "shared" while disagreeing on content — measured at 4 distinct contents among 6
# nominal sharers, i.e. live rebuild ping-pong plus the phantom-E0063 tail risk.
#
# Hashing the INDEX (not the working tree) is what makes this stable: we only reach
# the shared branch when there are no local changes under these paths, so the key
# moves only on rebase — never mid-edit, which would mean a cold build per save.
#
# SEEDING. Isolation no longer implies a cold build. A fresh private dir is cloned
# from the matching shared bucket (CoW: `cp -c` on APFS, `--reflink` on btrfs/XFS),
# which costs ~0 bytes and ~0 time, and cargo then rebuilds only the crates whose
# source actually differs. Cloning happens ONLY on first creation — never over an
# existing dir, which would clobber live artifacts.
#
# Depended-on crates = every workspace/vendored crate EXCEPT the leaf artifacts
# nothing links: crates/nub-cli (bin), crates/nub-native (cdylib, own workspace),
# crates/nub-phantom (bin, own workspace). NOTE nub-phantom-core and
# nub-phantom-scan are NOT leaves — nub-cli depends on both — and the git pathspec
# ':(exclude)crates/nub-phantom' matches only that directory, not those siblings
# (verified). See .claude/skills/rust-build/SKILL.md for the full model.

set -eu

root=$(git rev-parse --show-toplevel)
shared="${NUB_SHARED_TARGET:-$HOME/.cache/nub/shared-target}"

# Leaf artifacts nothing links, so a divergence here cannot clobber a sharer.
# Kept in one variable because the same set must apply to every query below —
# they drifting apart is exactly how an unsound share would sneak back in.
leaves=":(exclude)crates/nub-cli :(exclude)crates/nub-native :(exclude)crates/nub-phantom"

# Baseline all sharers agree on: the merge-base with origin/main. This worktree
# diverges a depended-on crate if, restricted to those crate dirs, EITHER:
#   - `git diff` vs the base is non-empty — committed branch work, uncommitted
#     edits, or deletions to TRACKED files (base is an ancestor of origin/main, so
#     origin/main advancing past it adds nothing — only THIS worktree's changes);
#   - there is an UNTRACKED file — a new module/source `git diff` can't see (it
#     compares tracked content only). `--exclude-standard` respects .gitignore, so
#     build output never counts.
# Both checks are deliberately broad (any path under a depended-on crate, not just
# *.rs): over-isolating on an irrelevant file costs one cold build; under-isolating
# risks the clobber. Depended-on = every workspace/vendored crate except nub-cli.
# `-C "$root"` on the git queries so the pathspecs resolve from the repo root
# regardless of the CWD the wrapper was invoked from (a subdir would otherwise
# misread them). The final `exec cargo` still runs in the original CWD.
base=$(git -C "$root" merge-base HEAD origin/main 2>/dev/null || true)
diverged=""
if [ -n "$base" ]; then
  # shellcheck disable=SC2086  # $leaves must word-split into separate pathspecs
  diverged=$(git -C "$root" diff --name-only "$base" -- \
    vendor/aube crates $leaves 2>/dev/null || true)
fi
# shellcheck disable=SC2086
untracked=$(git -C "$root" ls-files --others --exclude-standard -- \
  vendor/aube crates $leaves 2>/dev/null || true)

# The content key names the bucket AND, when isolating, names the seed to clone
# from — so it is computed unconditionally. `ls-files -s` emits the staged blob
# OIDs, so this is a pure content hash of the depended-on crates. ~0.2s.
# shellcheck disable=SC2086
key=$(git -C "$root" ls-files -s -- vendor/aube crates $leaves 2>/dev/null \
  | shasum 2>/dev/null | cut -c1-12 || true)
bucket="$shared${key:+-$key}"

# CoW-clone $1 into $2, PUBLISHING BY RENAME so the destination is never visible
# in a partial state. This is the whole point: the caller assigns $target and
# `exec cargo` runs against it regardless of what this returns, so a destination
# that exists-but-is-half-copied would hand cargo a target dir with fingerprints
# whose artifacts are missing — precisely the confusing artifact-level failure
# this script exists to prevent. A claim-then-fill scheme (mkdir, then copy)
# cannot give that guarantee: it serializes the copiers but not the consumers,
# and an interrupted copy leaves a permanently half-populated dir that looks
# claimed forever. Agent builds here are killed routinely, so that is a real
# case, not a theoretical one.
#
# Copy into a temp sibling, then rename into place only once the copy has
# returned 0. The exposure window shrinks from minutes of tree-walking to a
# single rename, and a killed copy leaves only an unreferenced temp (swept by
# the GC below). Clone-only: `cp -c` (APFS) and `--reflink=always` (btrfs/XFS)
# FAIL rather than silently falling back to a real multi-GB copy, which would
# trade minutes of CPU for gigabytes of disk.
seed_from() {
  [ -d "$1" ] || return 1
  [ -e "$2" ] && return 1
  _tmp="$2.seeding.$$"
  rm -rf "$_tmp" 2>/dev/null
  if cp -c -a "$1" "$_tmp" 2>/dev/null \
    || cp -a --reflink=always "$1" "$_tmp" 2>/dev/null; then
    # `mv` onto an EXISTING dir moves the source inside it, so re-check first.
    # A loser here wastes a clone; it can never publish a partial tree.
    if [ ! -e "$2" ] && mv "$_tmp" "$2" 2>/dev/null; then
      return 0
    fi
  fi
  rm -rf "$_tmp" 2>/dev/null
  return 1
}

if [ -n "$diverged" ] || [ -n "$untracked" ]; then
  target="$root/target"   # private, worktree-local; removed with the worktree
  why="isolated — this worktree diverges a depended-on crate from origin/main"
  # Seed from the shared cache on FIRST creation only. A relocated target dir
  # keeps its dependency artifacts (see the header), so this turns a ~3-min cold
  # build into a rebuild of just the diverged crates.
  if seed_from "$bucket" "$target"; then
    why="$why (seeded from $(basename "$bucket"))"
  fi
else
  target="$bucket"        # shared fast path, content-keyed
  why="shared bucket ${key:-none} — depended-on crate content"
  # MIGRATION: content-keying renames the bucket, so on the first run after this
  # change lands the warm legacy dir would be orphaned and every worktree would
  # eat one cold build. Seed the new bucket from it (same CoW clone, ~0 cost).
  # Self-retiring: the legacy dir is covered by the GC below, so once every live
  # worktree has moved to a keyed bucket it ages out on its own.
  if [ "$target" != "$shared" ] && seed_from "$shared" "$target"; then
    why="$why (migrated from the legacy shared dir)"
  fi
fi

mkdir -p "$target"
# `touch` is what makes the GC's mtime rule a real liveness signal, and it is NOT
# redundant with the mkdir above. Verified: `mkdir -p` on an existing dir does not
# update its mtime, and neither does a nested write — a cargo target dir's top
# level goes quiescent after the first build, so a bucket in continuous use would
# otherwise age past the window and be collected mid-build. That is a FAILED
# build, not a cold one.
touch "$target" 2>/dev/null || true

# Buckets are content-addressed caches, so a stale-mtime sweep is enough — no
# liveness check beyond the `touch` above, which every invocation applies to the
# dir it is about to use. Cheap, silent, and bounded to this script's own dirs.
#
# Matched by EXACT names, not a prefix glob: the keyed buckets plus the bare
# legacy dir (which is what makes the migration genuinely self-retiring — a `-*`
# glob alone never matches the legacy name, and a `*` glob would also swallow an
# unrelated prefix-sharing sibling such as a hand-made `shared-target.bak`).
# Half-written `.seeding.<pid>` temps from a killed clone are swept on the same
# pass; they are never referenced by anything.
_base=$(basename "$shared")
find "$(dirname "$shared")" -maxdepth 1 -type d \
  \( -name "$_base" -o -name "$_base-*" \) -mtime +14 \
  -exec rm -rf {} + 2>/dev/null || true
find "$(dirname "$shared")" -maxdepth 1 -type d -name "*.seeding.*" -mtime +1 \
  -exec rm -rf {} + 2>/dev/null || true

# CONTENTION CONTROL. Many agent worktrees build concurrently, and every cargo
# assumes it owns the machine — ~20 parallel builds drove the 10-core dev host to
# load ~190 and starved the UI (2026-07-23). Two default-on levers, both no-ops
# where they don't apply:
#   - QoS clamp (darwin only): run cargo at 'utility' QoS so interactive work
#     always preempts builds. An uncontended build still gets all cores — the
#     clamp only yields under pressure. NUB_BUILD_FG=1 opts out for a
#     latency-sensitive foreground build.
#   - Default job cap on big hosts (>8 cores): CARGO_BUILD_JOBS = ncpu-4 leaves
#     scheduler/memory headroom when N builds overlap. Any caller choice wins:
#     a pre-set CARGO_BUILD_JOBS, NUB_BUILD_JOBS, or an explicit -j/--jobs flag
#     (cargo's CLI flag outranks the env var).
ncpu=$( { sysctl -n hw.ncpu || nproc; } 2>/dev/null || echo 4 )
if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
  if [ -n "${NUB_BUILD_JOBS:-}" ]; then
    CARGO_BUILD_JOBS="$NUB_BUILD_JOBS" && export CARGO_BUILD_JOBS
  elif [ "$ncpu" -gt 8 ]; then
    CARGO_BUILD_JOBS=$((ncpu - 4)) && export CARGO_BUILD_JOBS
  fi
fi
qos=""
if [ "${NUB_BUILD_FG:-}" != "1" ] && [ "$(uname)" = "Darwin" ] \
  && command -v taskpolicy >/dev/null 2>&1; then
  qos="taskpolicy -c utility"
fi

printf 'rust-build: %s\n  CARGO_TARGET_DIR=%s jobs=%s qos=%s\n' \
  "$why" "$target" "${CARGO_BUILD_JOBS:-default}" "${qos:-none}" >&2
# NUB_* vars are routing input for this wrapper, not part of the command's
# environment. In particular, tests must not expose them to spawned user code.
unset NUB_SHARED_TARGET NUB_BUILD_JOBS NUB_BUILD_FG
# RUSTC_WRAPPER= disables the machine-global rustc-qos wrapper (make qos-global)
# for this invocation: QoS is already applied at the cargo level here, and with
# NUB_BUILD_FG unset above, a foreground (NUB_BUILD_FG=1) build would otherwise
# be re-clamped at the rustc level. Cargo treats the empty value as "no wrapper".
# $qos word-splits deliberately (empty, or "taskpolicy -c utility").
exec env CARGO_TARGET_DIR="$target" RUSTC_WRAPPER= $qos cargo "$@"
