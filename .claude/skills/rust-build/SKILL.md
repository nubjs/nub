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

It prints which target dir it chose and why, then execs `cargo` with `CARGO_TARGET_DIR` set. Same profiles, same args — plus two default-on contention controls (added 2026-07-23 after ~20 concurrent agent builds drove the 10-core host to load ~190):

- **QoS clamp (darwin only):** cargo runs under `taskpolicy -c utility`, so interactive work always preempts fleet builds; an uncontended build still gets all cores. `NUB_BUILD_FG=1` opts out for a latency-sensitive foreground build.
- **Default job cap on big hosts (>8 cores):** `CARGO_BUILD_JOBS = ncpu-4` unless the caller already chose (pre-set `CARGO_BUILD_JOBS`, `NUB_BUILD_JOBS`, or an explicit `-j`/`--jobs` flag — cargo's CLI flag outranks the env var).

These wrapper-level controls only cover builds that go THROUGH the wrapper (or make). Direct `cargo build`/`test` invocations are clamped by a third, machine-global control: `make qos-global` installs `scripts/rustc-qos.sh` as the cargo `rustc-wrapper` in `~/.cargo/config.toml`, so every rustc on the host — any worktree, any clone, any entry point — compiles at utility QoS (`install-dev` re-runs it, so it self-heals). Same `NUB_BUILD_FG=1` opt-out; toggling the wrapper does not invalidate cargo fingerprints, so mixed builds share a target dir without rebuild churn.

## Why one shared target dir

All worktrees default to `~/.cache/nub/shared-target`. A fresh worktree then reuses the crates.io dependency rlibs a sibling already compiled (the bulk of a build) and recompiles only the ~3 workspace crates — a ~5s incremental step instead of a ~3-min cold build.

**Relocation does NOT defeat reuse — this file used to claim it did, and the claim was false.** The retired wording said a private or CoW-cloned dir gets a "0% cross-worktree hit" because "rustc bakes the target-dir path into its fingerprints." Measured on this workspace with controls in both directions: cloning a warm target dir to a **new path** and building there rebuilt **0** crates; a genuinely empty dir rebuilt all 13; touching one source in the clone rebuilt exactly 1. Cargo revalidates a relocated target dir in place.

The true version of that statement is about **sccache**, which keys on the rustc command line — and that embeds absolute `--out-dir` / `-L dependency=` paths, so a different target dir *is* a guaranteed miss there. The two mechanisms were conflated, and the cost was real: isolation was treated as synonymous with a cold build, so ~79% of worktrees paid a full dependency rebuild they never needed.

What sharing a live path actually buys is **convergence** — concurrent worktrees keep one cache warm together. What a clone buys is a **warm start**, which is why `scripts/rust-build.sh` now seeds a fresh private dir from the matching shared bucket (CoW, ~0 cost) instead of starting cold.

## The hazard sharing creates

Cargo names a crate's output by **package id (name + version), not source content.** Two worktrees whose source for the *same depended-on crate* differs — classically `vendor/aube` on divergent branches — write the **same output slot** and clobber each other. A dependent crate then links the stale rlib and fails to compile against source that is actually correct:

```
error[E0063]: missing field `lockfile_legacy_basenames` in initializer of `aube_util::Embedder`
```

— a field that exists nowhere in your checkout. It's a ghost from a sibling's build. This only bites crates that **other crates link**: a divergent leaf binary (`nub-cli`) just rebuilds cleanly, so editing it is safe to share.

## The rule the wrapper enforces

Sharers are grouped by the **content** of their depended-on crates, hashed into the bucket name (`shared-target-<hash>`). So:

- **Your depended-on crate sources are unmodified → share** the bucket for that content. Everyone in it agrees by construction, so nothing can clobber. This is the overwhelming common case: feature work in `nub-cli`, integration tests, docs, non-Rust files.
- **You've diverged a depended-on crate → isolate** to a private per-worktree `target/` (removed with the worktree) — **seeded** from the matching bucket via CoW clone, so you start warm and rebuild only what actually differs, not the whole dependency graph.

**Why content and not a merge-base.** A per-worktree merge-base proves only that *this* worktree made no local changes against *its own* base — not that two sharers agree with each other. Two worktrees whose bases straddle a `nub-core`/`aube` commit both pass that test while disagreeing on content, then fight over the same output slots. Measured: 4 distinct depended-on-crate contents among 6 worktrees that all read "shared." Content-keying makes "same path implies same content" a theorem instead of an inference. The key hashes the git **index**, which only matters on the shared branch — where there are no local changes by definition — so it moves on rebase, never mid-edit.

Depended-on crates = every workspace/vendored crate **except the leaf artifacts nothing links**: `crates/nub-cli` (bin), `crates/nub-native` (cdylib, own workspace), and `crates/nub-phantom` (bin, own workspace). So: `crates/nub-core`, `crates/nub-cache-key`, `crates/nub-phantom-core`, `crates/nub-phantom-scan`, and all of `vendor/aube`. Note `nub-phantom-core`/`nub-phantom-scan` are **not** leaves — `nub-cli` depends on both — and the pathspec `:(exclude)crates/nub-phantom` matches only that directory, not those siblings (verified).

## Orchestrating a serial multi-phase epic — reuse ONE dedicated target, never fresh-cold-build per worktree

**Largely obsolete since seeding landed** — a fresh private `target/` is now CoW-cloned from the matching shared bucket, so isolation costs a rebuild of the diverged crates, not the ~700-crate dependency graph. The section is kept because the reasoning still applies whenever seeding can't fire (no matching bucket yet, or a filesystem without CoW).

Before seeding, the isolation rule handed each diverging worktree a **fresh private `target/` = one cold build (~40 min on a contended host)**. Correct for a lone worktree — but an agentic **serial chain** of diverging worktrees (a multi-phase epic where each phase edits `crates/nub-sandbox` or another depended-on crate in its own worktree, one after another, each reviewed + merge-verified) paid that cold build *per phase and per review* — pure waste, since the crates.io deps are identical across the phases and only the ~10 workspace crates change. (Burned 2026-07-25: ran a sandbox epic giving every phase/review a fresh private `CARGO_TARGET_DIR` — a 47-min cold review build for nothing, repeatedly, until the maintainer flagged the loop times.)

Fix — point the whole serial chain at **ONE dedicated private target** (not a fresh per-worktree one, and NOT the default shared one): `export CARGO_TARGET_DIR=~/.cache/nub/<epic>-target`, reused across every phase worktree + its review + its merge-verify. Deps compile cold **once**; each later build recompiles only the workspace crates (a few minutes). Two safety rules:
- **Serial only.** Cargo's target-dir lock serializes builds into one dir, so run them one at a time (a serial epic already does); never point two *concurrently-building* worktrees at it.
- **Dedicated, NOT `shared-target`.** `~/.cache/nub/shared-target` is **multi-tenant** — many concurrent Claude/dev sessions build against it — so a worktree diverging a depended-on crate would clobber *their* builds. Use a private-but-reused epic target instead.

Corollary: **a review/verify sub-agent must reuse the implementer's already-warm target, never a fresh one** — dispatching a reviewer into a new `CARGO_TARGET_DIR` re-pays the whole cold build for nothing.

## Trade-offs and edges

- **A worktree that edits aube pays a cold build even with no sibling diverging aube concurrently.** The invariant is "match origin/main," which doesn't depend on observing volatile sibling state — that's what makes it robust. Isolating-only-when-a-sibling-also-diverges would save a cold build for a lone aube editor but is racy; the simple rule wins.
- **Concurrent builds in two sharing worktrees serialize** on cargo's target-dir lock (one waits). That's a latency cost, never a correctness one — unrelated to the contamination above. Need two builds at once? An isolated worktree (diverge a lib crate, or set a private `CARGO_TARGET_DIR`) runs in parallel.
- **`NUB_SHARED_TARGET`** overrides the shared path if you need a different location.
- Cleanup is unchanged: `git worktree remove <path> --force` drops the worktree and its private `target/`; the shared dir is intentionally left in place for the next worktree.

## Relationship to the worktree + dev-loop skills

`new-worktree.ts` creates the worktree and points you at this wrapper. The `dev-loop` skill covers the fast-profile / incremental-build loop; this skill owns the target-dir decision specifically. When a build fails with a symbol/field that isn't in your source, this is almost always the cause — rebuild through `rust-build.sh` and it isolates you.
