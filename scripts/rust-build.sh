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
# the ~3 workspace crates, instead of paying a ~3-min cold build. Cargo can only
# reuse artifacts across worktrees when the target-dir PATH is byte-identical —
# rustc bakes the target path into its fingerprints, so a private dir or a
# CoW-cloned dir gets a 0% cross-worktree hit (measured; this is also why sccache
# does nothing here). Sharing one path is the only mechanism that actually reuses.
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
# THE RULE (what this script enforces). Share the dir while this worktree's
# depended-on crate sources match origin/main — every sharer then agrees on those
# crates, so nothing clobbers. The moment this worktree diverges a depended-on crate
# from that baseline, fall back to a PRIVATE per-worktree target dir: one cold build,
# then stable and contamination-proof. Editing only nub-cli, tests, docs, or non-Rust
# files keeps you on the fast shared path — which is the overwhelming common case.
#
# Depended-on crates = every workspace/vendored crate EXCEPT the leaf binary
# (crates/nub-cli). See .claude/skills/rust-build/SKILL.md for the full model.

set -eu

root=$(git rev-parse --show-toplevel)
shared="${NUB_SHARED_TARGET:-$HOME/.cache/nub/shared-target}"

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
  diverged=$(git -C "$root" diff --name-only "$base" -- \
    vendor/aube crates ':(exclude)crates/nub-cli' 2>/dev/null || true)
fi
untracked=$(git -C "$root" ls-files --others --exclude-standard -- \
  vendor/aube crates ':(exclude)crates/nub-cli' 2>/dev/null || true)

if [ -n "$diverged" ] || [ -n "$untracked" ]; then
  target="$root/target"   # private, worktree-local; removed with the worktree
  why="isolated — this worktree diverges a depended-on crate from origin/main"
else
  target="$shared"        # shared fast path
  why="shared — depended-on crates match origin/main"
fi

mkdir -p "$target"
printf 'rust-build: %s\n  CARGO_TARGET_DIR=%s\n' "$why" "$target" >&2
# NUB_SHARED_TARGET is routing input for this wrapper, not part of the command's
# environment. In particular, tests must not expose it to spawned user code.
unset NUB_SHARED_TARGET
exec env CARGO_TARGET_DIR="$target" cargo "$@"
