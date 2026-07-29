---
name: rust-build-hygiene
description: >-
  Best practices for spinning up nub Rust builds so they are PERFORMANT and
  CLEAN THEMSELVES UP — the prevention side of the recurring orphaned-build
  problem on the maintainer's dev host. Invoke (via the Skill tool) before
  launching any `cargo build`/`test`/`clippy` you might background or leave
  running, when setting up a build in a sub-agent, or when deciding how to wait
  on a long build. Encodes the ONE rule that stops the bleeding — never DETACH a
  build (setsid/nohup/`& disown` reparent it to PID 1, it outlives its launcher,
  holds the target-dir lock for 30+ min, and `TaskStop` does NOT reap it) —
  plus how to background correctly (harness-tracked), how to wait on a long
  build (a sub-agent that owns the wait, never a detached shell), one-target-per-
  concurrent-build, the fast profile + QoS clamp, and cleanup-on-done. For
  clearing residue that already accumulated, see `cpu-reduction`; for the target-
  dir sharing/isolation decision, see `rust-build`; for the worktree loop, `dev-loop`.
metadata:
  internal: true
---

# rust-build-hygiene — launch builds that die with you and clean up after themselves

The maintainer's host has a **continuous orphaned-Rust-build problem**: builds that outlive their
launcher, hold target-dir locks (stalling other builds for 30+ min), burn cores, and leave tens of GB
of stale `target/` behind. Every instance traces to the SAME root cause — a build launched in a way
that survives the process that started it. This skill is how to launch so that never happens. (The
`cpu-reduction` skill is the mop for residue that already exists; this is "don't spill.")

## The one rule: NEVER detach a build

Do **not** launch a build with `setsid`, `nohup`, a trailing `&` + `disown`, or any other
detach-from-launcher form. A detached build **reparents to PID 1** the moment its launcher exits, so
it outlives the agent/session/turn entirely, keeps holding the cargo target-dir lock, and — the
killer — **`TaskStop` does NOT reap it** (TaskStop kills the agent, not its background bash jobs). A
stopped/abandoned agent's detached build is the single most common orphan, and the one that most often
produces the `Blocking waiting for file lock on artifact directory` hang on the NEXT build.

```sh
# BANNED — these orphan to PID 1 and survive TaskStop:
setsid cargo build ... &
nohup cargo build ... &
cargo build ... & disown
```

(This is the `no-detached-orphan-builds` memory, generalized to a launch discipline.)

## How to run a build correctly, by situation

| Situation | Do this | Why |
| --- | --- | --- |
| Quick interactive build/test (< a few min) | Foreground `Bash` call (through `scripts/rust-build.sh`) | Dies with the turn; the harness caps foreground at ~10 min |
| A build you want to keep working alongside (dev server aside) | `Bash` with `run_in_background: true` | Harness-TRACKED — it's reaped when the session ends and shows in the background-jobs list; NOT detached |
| A long build you will REST on until it finishes | Dispatch a **sub-agent** that runs the build in ITS OWN foreground and returns the result | The sub-agent's liveness is what the harness tracks; when it returns you continue. Never rest on a bare background shell (it can't be told from a dev server and won't hold you active) |
| A long build in CI / on a VM | `scripts/ci-watch.ts` (the `ci-watch` skill) or a sub-agent owning the watch | Same principle — own the wait in a tracked process, never a detached poll loop |

Never fake-wait on a build with a detached shell + a `sleep`/poll loop. If you need to wait, own the
wait in a tracked process (foreground, background-tracked, or a sub-agent).

## Performance — reuse the cache, clamp the QoS, cap the jobs

These live in the `rust-build` + `dev-loop` skills; the hygiene-relevant essentials:

- **Build through `scripts/rust-build.sh`** (drop-in for `cargo`). It picks the right target dir (shared by default, auto-isolates when a worktree diverges a depended-on crate), and applies two default-on contention controls: a darwin QoS clamp (`taskpolicy -c utility`, so interactive work always preempts fleet builds) and a job cap on big hosts (`CARGO_BUILD_JOBS = ncpu-4`). `make qos-global` additionally clamps EVERY rustc on the host (any entry point) to utility QoS.
- **Use the `fast` profile to iterate** (`--profile fast` → `target/fast/nub`, ~5s incremental), never `release` (its `lto=thin` + `codegen-units=1` re-LTOs the whole binary every change).
- **One target dir per CONCURRENTLY-building tree.** Two builds on one target dir serialize on cargo's lock — that IS the contention. A serial multi-phase epic should reuse ONE dedicated warm target across its phases (deps compile cold once); NEVER point two concurrent builds at it. Details + the sharing-vs-isolation rule: the `rust-build` skill.
- **sccache does nothing here** (measured 0% cross-worktree hit — rustc bakes the target path into fingerprints). Don't reach for it; a stable per-tree target dir is the whole answer.

## Self-cleaning — leave nothing behind

- **A worktree owns its target.** `git worktree remove <path> --force` drops the worktree; `rm -rf <path>-target` drops its private target dir. Do both when the work lands. The shared `~/.cache/nub/shared-target` is intentionally left for the next worktree.
- **A sub-agent that built in an isolated target cleans it up on completion** — UNLESS a serial chain will reuse it (then hand the warm target forward explicitly). Say which in the dispatch prompt.
- **Prune stale worktrees periodically** — they accumulate (149 GB / 27 worktrees, 2026-07-26). `git worktree list` → remove dead ones → `git worktree prune`. The `worktree` skill owns the lifecycle; `cpu-reduction` §2b has the disk-pressure sweep.
- **Never `cp -r` the repo to isolate a build** — the tree carries multi-GB `target/`/`.repos/`/`node_modules`; a recursive copy fills the disk. Use `git worktree add` or `git clone --depth 1 file://$PWD` + a private `CARGO_TARGET_DIR`.

## If it already orphaned
Diagnose + clear with `cpu-reduction` §2: `ps ... | grep -Ei 'rustc|cargo|lld'`, find the detached
build holding the lock, `pkill -f '<target-dir>'` (artifacts persist = still warm), hand the
contention-free target to ONE fresh foreground build.
