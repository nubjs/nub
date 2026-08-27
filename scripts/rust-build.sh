#!/usr/bin/env sh
# rust-build — pick the correct CARGO_TARGET_DIR for this worktree, then exec cargo.
#
# Usage (drop-in for cargo, from inside any worktree or the main tree):
#   scripts/rust-build.sh build -p nub-cli --profile fast
#   scripts/rust-build.sh test  -p nub-cli
#   NUB_ALLOW_INCOMPLETE_RUNTIME=1 scripts/rust-build.sh clippy --all-targets --all-features -- -D warnings
#
# The grant on that third line is what CI's clippy job sets: `--all-features` turns on
# `embed-runtime`, whose build script requires a fully staged runtime/ (the addon plus the
# vendored node_modules), and a lint ships nothing so it opts out. It is written per-invocation
# rather than exported here on purpose — this wrapper also execs `build` and `test`, which CAN
# produce a binary someone runs, and the check must stay armed for those.
#
# WHY A SHARED TARGET DIR AT ALL. All worktrees default to ONE cargo target dir
# (~/.cache/nub/shared-target) so a fresh worktree reuses the crates.io dependency
# rlibs another worktree already compiled (the bulk of a build) and recompiles only
# the ~3 workspace crates, instead of paying a ~3-min cold build. Sharing one live
# path is what lets concurrent worktrees keep converging on one warm cache.
#
# RELOCATION DOES NOT DEFEAT REUSE. This script used to claim a private or
# CoW-cloned dir gets "a 0% hit" because rustc bakes the target path into its
# fingerprints. That is FALSE, and it cost real time — it is why isolation was
# treated as synonymous with a cold build. Measured with controls both ways: a
# warm dir cloned to a new path rebuilt 0 crates, an empty dir rebuilt all 13,
# touching one source rebuilt 1. Cargo revalidates a relocated dir in place. The
# claim is true of SCCACHE, which keys on the rustc command line (absolute
# --out-dir / -L paths); the two mechanisms were conflated.
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
# An EXPLICIT NUB_SHARED_TARGET is honored verbatim — no content key appended.
# Keying exists to make SHARING sound (same path implies same content across the
# worktrees that all converge on the one default cache); a caller naming a
# specific path is not sharing, and silently redirecting the path it named breaks
# every consumer that knows where its artifacts land. `make verify` is exactly
# that caller: it points at a repo-local $(CURDIR)/target precisely to be
# isolated from the shared cache, and its addon recipe then copies from
# target/<profile>/.
if [ -n "${NUB_SHARED_TARGET:-}" ]; then
  shared="$NUB_SHARED_TARGET"
  keyed=0
else
  shared="$HOME/.cache/nub/shared-target"
  keyed=1
fi

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
# `runtime/` is in the set for a different reason than the crates: the binary
# resolves `runtime/*.cjs` at run time from the tree that compiled nub-core (its
# baked CARGO_MANIFEST_DIR), so a shared-bucket binary loads whichever SHARER
# compiled last — a worktree with edited runtime files would test a sibling's
# copy, silently. Isolating on runtime divergence keeps every shared-bucket
# binary pointed at base-identical runtime content. (Observed: three worktrees'
# red/green verdicts flipped as siblings rebuilt the common bucket.)
# `-C "$root"` on the git queries so the pathspecs resolve from the repo root
# regardless of the CWD the wrapper was invoked from (a subdir would otherwise
# misread them). The final `exec cargo` still runs in the original CWD.
base=$(git -C "$root" merge-base HEAD origin/main 2>/dev/null || true)
diverged=""
if [ -n "$base" ]; then
  # shellcheck disable=SC2086  # $leaves must word-split into separate pathspecs
  diverged=$(git -C "$root" diff --name-only "$base" -- \
    vendor/aube crates runtime $leaves 2>/dev/null || true)
fi
# shellcheck disable=SC2086
untracked=$(git -C "$root" ls-files --others --exclude-standard -- \
  vendor/aube crates runtime $leaves 2>/dev/null || true)

# The content key names the bucket AND, when isolating, names the seed to clone
# from — so it is computed unconditionally. `ls-files -s` emits the staged blob
# OIDs, so this is a pure content hash of the depended-on crates. ~0.2s.
# shellcheck disable=SC2086
key=$(git -C "$root" ls-files -s -- vendor/aube crates runtime $leaves 2>/dev/null \
  | shasum 2>/dev/null | cut -c1-12 || true)
if [ "$keyed" = 1 ]; then
  bucket="$shared${key:+-$key}"
else
  bucket="$shared"
fi

# CoW-clone $1 into $2. The caller runs cargo against $2 no matter what this
# returns, so $2 must never be visible half-copied — cargo would find fingerprints
# whose artifacts are missing, the exact failure this script exists to prevent.
# Staging under a fixed-name claim dir and renaming on success gives that: the
# claim is also the mutual-exclusion token, so one racer clones instead of all of
# them. Clone-only — `cp -c` (APFS) and `--reflink=always` (btrfs/XFS) fail rather
# than silently falling back to a real multi-GB copy.
seed_from() {
  [ -d "$1" ] || return 1
  _claim="$2.seeding"
  # Retire an abandoned claim (killed clone — routine here) BEFORE the
  # destination-exists return: that return fires on every run once $2 is
  # published, so a retire below it would be dead code and a claim in a worktree
  # root — which neither sweep scans — would leak untracked forever.
  if [ -d "$_claim" ] && [ -z "$(find "$_claim" -maxdepth 0 -mmin -120 2>/dev/null)" ]; then
    rm -rf "$_claim" 2>/dev/null
  fi
  [ -e "$2" ] && return 1
  mkdir "$_claim" 2>/dev/null || return 1
  if cp -c -a "$1/." "$_claim/" 2>/dev/null \
    || cp -a --reflink=always "$1/." "$_claim/" 2>/dev/null; then
    # A racer that lost the claim returns 1, and the caller's `mkdir -p "$target"`
    # then creates the destination EMPTY — which would otherwise make this publish
    # fail and leave BOTH processes cold, worse than no claim at all. `rmdir` only
    # succeeds on an empty dir, so it clears that placeholder and never touches a
    # destination something is already building in.
    [ -d "$2" ] && rmdir "$2" 2>/dev/null
    if [ ! -e "$2" ] && mv "$_claim" "$2" 2>/dev/null; then
      return 0
    fi
  fi
  rm -rf "$_claim" 2>/dev/null
  return 1
}

# The newest COMPLETE bucket on disk, or nothing. Buckets are only ever created
# by non-diverged worktrees, so every bucket name is a base-content key. The
# `.seeding` filter is load-bearing: a claim dir shares the `$shared-` prefix and
# `-t` sorts newest-first, so an actively-filling claim would be the MOST likely
# pick — seeding from a half-copied tree pairs a fresh-looking fingerprint with a
# truncated rlib, the exact failure this design exists to prevent.
newest_bucket() {
  # shellcheck disable=SC2012  # names are ours and contain no newlines
  ls -dt "$shared"-* 2>/dev/null | grep -v '\.seeding$' | head -1
}

if [ -n "$diverged" ] || [ -n "$untracked" ]; then
  target="$root/target"   # private, worktree-local; removed with the worktree
  why="isolated — this worktree diverges a depended-on crate from origin/main"
  # ANY bucket will do, and the fallback is what makes seeding fire at all for
  # the ordinary case: $bucket is keyed by this worktree's INDEX, but buckets are
  # only created by non-diverged worktrees, so a committed or staged divergence
  # (i.e. any feature branch) matches no bucket name. Seeding from base content is
  # sound here only because $target is PRIVATE — a stale workspace artifact can't
  # clobber a sibling, cargo just rebuilds it, and the crates.io rlibs we're
  # after are identical across buckets.
  _seed="$bucket"
  [ -d "$_seed" ] || _seed=$(newest_bucket)
  [ -n "$_seed" ] && [ -d "$_seed" ] || _seed="$shared"
  if seed_from "$_seed" "$target"; then
    why="$why (seeded from $(basename "$_seed"))"
  fi
else
  target="$bucket"        # shared fast path, content-keyed
  why="shared bucket ${key:-none} — depended-on crate content"
  # RE-KEY MIGRATION: any change that moves the content key — origin/main
  # advancing a hashed path, or a pathspec addition like runtime/ — renames the
  # bucket, and without a seed every worktree eats one cold build. Seed from the
  # newest existing keyed bucket, falling back to the legacy un-keyed dir
  # (pre-keying installs). Sound for the same reason the isolated-branch seed is:
  # sharers of the NEW bucket agree on current content, and cargo rebuilds any
  # seeded artifact whose fingerprint no longer matches. Self-retiring: orphaned
  # sources age out via the GC below.
  if [ "$target" != "$shared" ]; then
    # An EMPTY bucket blocks seeding as hard as a warm one: `seed_from` refuses an
    # existing destination, and the unconditional `mkdir -p` below (also reached
    # via --print-target) creates exactly such a placeholder. `rmdir` clears it
    # and cannot touch a bucket with contents.
    rmdir "$target" 2>/dev/null || true
    if [ ! -e "$target" ]; then
      _seed=$(newest_bucket)
      [ -n "$_seed" ] && [ -d "$_seed" ] || _seed="$shared"
      if seed_from "$_seed" "$target"; then
        why="$why (seeded from $(basename "$_seed"))"
      fi
    fi
  fi
fi

mkdir -p "$target"
# NOT redundant with the mkdir: verified that `mkdir -p` on an existing dir does
# not update its mtime, and neither does a nested write. A target dir's top level
# goes quiescent after the first build, so without this an in-use bucket ages past
# the GC window and is collected mid-build — a failed build, not a cold one.
touch "$target" 2>/dev/null || true

# Stale-mtime sweep; the `touch` above is the liveness signal. Exact names rather
# than a prefix glob so an unrelated sibling (a hand-made `shared-target.bak`)
# can't be caught, while the bare legacy dir still is — which is what lets the
# migration retire itself. Second pass: abandoned claims from a killed clone,
# bucket-side only; a claim in a worktree root is retired by `seed_from`.
_base=$(basename "$shared")
find "$(dirname "$shared")" -maxdepth 1 -type d \
  \( -name "$_base" -o -name "$_base-*" \) -mtime +14 \
  -exec rm -rf {} + 2>/dev/null || true
find "$(dirname "$shared")" -maxdepth 1 -type d -name "$_base*.seeding" -mmin +120 \
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

# `--print-target` resolves the target dir the SAME way a real build would and
# prints it, so callers that must locate the artifact afterwards (make
# install-dev symlinking nub-dev) follow the shared/isolated decision instead of
# hardcoding a path that is only right in one of the two cases.
if [ "${1:-}" = "--print-target" ]; then
  printf '%s\n' "$target"
  exit 0
fi

# The machine-global rustc wrapper (make qos-global) now carries the GLOBAL
# CONCURRENCY SEMAPHORE, not just a QoS clamp, so this invocation must let it
# run. It previously blanked RUSTC_WRAPPER — sound when the wrapper only
# re-applied a clamp cargo had already applied, and the direct cause of the
# 2026-08-19 saturation once the semaphore moved there: 10 of the 13 concurrent
# builds went through this script and every one of them opted itself out of the
# only machine-wide cap that existed. The per-cargo CARGO_BUILD_JOBS above stays
# — it is blind to sibling builds, so it bounds ONE build, never the fleet.
#
# NUB_BUILD_FG=1 still opts fully out of both, which is the documented meaning of
# a latency-sensitive foreground build; it is passed by blanking RUSTC_WRAPPER
# rather than by exporting the var, so the unset below keeps holding.
wrapper_off=""
[ "${NUB_BUILD_FG:-}" = "1" ] && wrapper_off="RUSTC_WRAPPER="

printf 'rust-build: %s\n  CARGO_TARGET_DIR=%s jobs=%s qos=%s sem=%s\n' \
  "$why" "$target" "${CARGO_BUILD_JOBS:-default}" "${qos:-none}" \
  "$([ -n "$wrapper_off" ] && echo bypassed || echo global)" >&2
# NUB_* vars are routing input for this wrapper, not part of the command's
# environment. In particular, tests must not expose them to spawned user code.
unset NUB_SHARED_TARGET NUB_BUILD_JOBS NUB_BUILD_FG
# $qos and $wrapper_off word-split deliberately (each empty, or one assignment).
# shellcheck disable=SC2086
exec env CARGO_TARGET_DIR="$target" $wrapper_off $qos cargo "$@"
