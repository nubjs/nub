---
name: rust-build
description: >-
  Use when building or testing the nub Rust workspace inside a git worktree —
  `cargo build`/`test`/`clippy` for nub-cli/nub-core/aube in a worktree off
  origin/main. Explains how worktrees share ONE cargo target dir for fast
  incremental builds, the cross-worktree artifact-contamination hazard that
  sharing creates (the phantom `E0063: missing field` on correct source), and the
  wrapper (`scripts/rust-build.sh`) that shares by default and auto-isolates the
  moment a worktree diverges a depended-on crate. Auto-triggers on a spurious
  cargo compile error that names a field/symbol absent from your checkout, on
  "worktree build contamination", and on setting CARGO_TARGET_DIR for a worktree.
metadata:
  internal: true
---

# rust-build

Building the nub Rust workspace across parallel worktrees has one tension: **cross-worktree cache reuse wants a shared target dir; correctness wants isolation.** `scripts/rust-build.sh` resolves it — share by default, isolate automatically only when a worktree diverges a crate other crates link.

## Use it

Drop-in for `cargo`, from any worktree (or the main tree):

```sh
scripts/rust-build.sh build -p nub-cli --profile fast
scripts/rust-build.sh test  -p nub-cli --test integration
scripts/rust-build.sh clippy --all-targets --all-features -- -D warnings
```

It prints which target dir it chose and why, then execs `cargo` with `CARGO_TARGET_DIR` set. Nothing else changes — same profiles, same args.

## Why one shared target dir

All worktrees default to `~/.cache/nub/shared-target`. A fresh worktree then reuses the crates.io dependency rlibs a sibling already compiled (the bulk of a build) and recompiles only the ~3 workspace crates — a ~5s incremental step instead of a ~3-min cold build.

This reuse works **only because the path is byte-identical.** rustc bakes the target-dir path into its fingerprints, so a private dir — or a CoW/APFS-cloned one — gets a **0% cross-worktree hit** (measured; it's also why sccache does nothing on this workspace). Sharing one path is the only mechanism that actually reuses artifacts across worktrees. There is no copy-on-write shortcut: cargo invalidates the cloned fingerprints and rebuilds from scratch.

## The hazard sharing creates

Cargo names a crate's output by **package id (name + version), not source content.** Two worktrees whose source for the *same depended-on crate* differs — classically `vendor/aube` on divergent branches — write the **same output slot** and clobber each other. A dependent crate then links the stale rlib and fails to compile against source that is actually correct:

```
error[E0063]: missing field `lockfile_legacy_basenames` in initializer of `aube_util::Embedder`
```

— a field that exists nowhere in your checkout. It's a ghost from a sibling's build. This only bites crates that **other crates link**: a divergent leaf binary (`nub-cli`) just rebuilds cleanly, so editing it is safe to share.

## The rule the wrapper enforces

Baseline all sharers agree on is **origin/main**. So:

- **Your depended-on crate sources match origin/main → share.** Every sharer agrees on those crates; nothing clobbers. This is the overwhelming common case: feature work in `nub-cli`, integration tests, docs, non-Rust files.
- **You've diverged a depended-on crate from origin/main → isolate.** A private per-worktree `target/` (removed with the worktree). One cold build, then stable and contamination-proof.

Depended-on crates = every workspace/vendored crate **except the leaf binary** `crates/nub-cli`: `crates/nub-core`, `crates/nub-cache-key`, `crates/nub-native`, and all of `vendor/aube`. The wrapper computes divergence with `git diff` against the merge-base with origin/main (committed branch work *and* uncommitted edits), so it adapts as you edit — start shared, and the first time you touch aube it flips to isolated on the next build.

## Trade-offs and edges

- **A worktree that edits aube pays a cold build even with no sibling diverging aube concurrently.** The invariant is "match origin/main," which doesn't depend on observing volatile sibling state — that's what makes it robust. Isolating-only-when-a-sibling-also-diverges would save a cold build for a lone aube editor but is racy; the simple rule wins.
- **Concurrent builds in two sharing worktrees serialize** on cargo's target-dir lock (one waits). That's a latency cost, never a correctness one — unrelated to the contamination above. Need two builds at once? An isolated worktree (diverge a lib crate, or set a private `CARGO_TARGET_DIR`) runs in parallel.
- **`NUB_SHARED_TARGET`** overrides the shared path if you need a different location.
- Cleanup is unchanged: `git worktree remove <path> --force` drops the worktree and its private `target/`; the shared dir is intentionally left in place for the next worktree.

## Relationship to the worktree + dev-loop skills

`new-worktree.ts` creates the worktree and points you at this wrapper. The `dev-loop` skill covers the fast-profile / incremental-build loop; this skill owns the target-dir decision specifically. When a build fails with a symbol/field that isn't in your source, this is almost always the cause — rebuild through `rust-build.sh` and it isolates you.
