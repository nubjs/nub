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

Orphaned Rust builds outlive their launcher, hold target-dir locks (stalling other builds 30+ min), burn cores, and leave tens of GB of stale `target/` behind. Every instance traces to the same root cause: a build launched in a way that survives the process that started it. (`cpu-reduction` is the mop; this is "don't spill.")

## The one rule: NEVER detach a build

A detached build **reparents to PID 1** the moment its launcher exits, so it outlives the agent/session/turn, keeps holding the cargo target-dir lock, and **`TaskStop` does NOT reap it** (TaskStop kills the agent, not its background bash jobs). This is the most common orphan and the usual cause of `Blocking waiting for file lock on artifact directory` on the next build.

```sh
# BANNED — these orphan to PID 1 and survive TaskStop:
setsid cargo build ... &
nohup cargo build ... &
cargo build ... & disown
```

## How to run a build correctly, by situation

| Situation | Do this | Why |
| --- | --- | --- |
| Quick interactive build/test (< a few min) | Foreground `Bash` call (through `scripts/rust-build.sh`) | Dies with the turn; the harness caps foreground at ~10 min |
| A build you want to keep working alongside | `Bash` with `run_in_background: true` | Harness-TRACKED — reaped when the session ends, shows in the background-jobs list; NOT detached |
| A long build you will REST on until it finishes | Dispatch a **sub-agent** that runs the build in ITS OWN foreground and returns the result | The sub-agent's liveness is what the harness tracks. Never rest on a bare background shell |
| A long build in CI / on a VM | `scripts/ci-watch.ts` (the `ci-watch` skill) or a sub-agent owning the watch | Own the wait in a tracked process, never a detached poll loop |

Never fake-wait on a build with a detached shell + a `sleep`/poll loop.

## The fleet, not your build, is what saturates the host

**A per-build cap cannot bound N builds.** Every build can be individually blameless — `--profile fast`, QoS-clamped, `jobs = 6` — and the machine still dies, because the caps multiply instead of adding. Measured 2026-08-19: 13 concurrent agent builds, every one of them already on `--profile fast`, produced a 78-way oversubscription of 10 cores — load 464, 0% idle, 36% sys, and a `reqwest` compile that normally takes ~30s taking 28 minutes. Nothing was misconfigured. There was simply no cap on the SUM.

- **`make qos-global` installs the global governor and is what actually bounds the fleet.** It registers `scripts/rustc-qos.sh` as the machine-wide rustc wrapper, where it does three jobs: clamp QoS; **let at most TWO builds compile at a time** (`NUB_BUILD_SLOTS`, default 2 — every other build's first rustc waits in a first-come-first-served queue until a holder's cargo exits, dies, or goes idle for `NUB_BUILD_IDLE`=120s, so a `cargo test` running its tests or a cargo blocked on a target lock does not hold the machine); and across the compiling builds, hold one of `NUB_RUSTC_LIMIT` (default 6) tokens for the life of each rustc. Two builds over six tokens bounds the memory peak to ~6 big-crate compiles (~12 GiB) and keeps a second build's worth of cores busy; strict one-at-a-time was the first cut (2026-08-28) and was measured idling nine cores behind one starved compile while eight builds queued 25 minutes. It needs no cooperation from the caller — which is the point, since the measured failure was builds bypassing the launcher script. A build that queues is not stuck — after 20s it prints one `rustc-qos: this build is queued …` line on its cargo's stderr, and `make build-status` shows the holders and the queue. `rust-analyzer` is exempt so the editor never waits behind agent builds.
- **Never blank `RUSTC_WRAPPER`, and never set `NUB_BUILD_FG=1` from an agent.** Blanking is cargo's documented "no wrapper", and it opts the build out of the global cap; `scripts/rust-build.sh` used to do exactly this, which is why 10 of those 13 builds were ungoverned. `NUB_BUILD_FG=1` opts a build out of the QoS clamp and the build-slot queue (a bare `cargo` still takes rustc tokens; through `rust-build.sh` it blanks both wrapper keys, so out of the tokens too) — it exists for a HUMAN at a terminal whose build must not wait behind the fleet, and an agent that sets it recreates the 2026-08-19 incident. `NUB_BUILD_SLOTS=0` disables only the slot layer and `NUB_BUILD_SLOTS=1` restores strict one-at-a-time — both are PER-PROCESS environment knobs, read by the wrapper of the cargo that inherits them, not host-wide settings. The host-wide switch is `make build-slots-off` / `build-slots-on` (a file every wrapper checks each second, so it also releases builds already queued); it is for an emergency, and it leaves the QoS clamp and the tokens in place.
- **`make build-status` answers "why is this machine saturated?" and "why is my build not starting?"** It prints the sum no single session can see: load, which builds hold the compile slots and who is queued behind them (each tagged with its worktree), token occupancy, a STALE WRAPPER line when an older checkout's `make install-dev` downgraded the governor, and which builds are outside the cap. Run it before concluding your own build is slow or hung — a build whose rustc sits at `Compiling` for minutes with no CPU is queued, not broken. A foreground Bash call whose cargo goes silent at `Compiling` is the same thing: relaunching it puts the new cargo at the BACK of the queue.

## Performance — reuse the cache, clamp the QoS, cap the jobs

- **Build through `scripts/rust-build.sh`** (drop-in for `cargo`). It picks the right target dir (shared by default, auto-isolates when a worktree diverges a depended-on crate) and applies a darwin QoS clamp (`taskpolicy -c utility`) plus a job cap on big hosts (`CARGO_BUILD_JOBS = ncpu-4`). That job cap bounds ONE build; `make qos-global` is what bounds the fleet.
- **Use the `fast` profile to iterate** (`--profile fast` → `target/fast/nub`, ~5s incremental), never `release` (its `lto=thin` + `codegen-units=1` re-LTOs the whole binary every change).
- **One target dir per CONCURRENTLY-building tree.** Two builds on one target dir serialize on cargo's lock — that IS the contention. A serial multi-phase epic reuses ONE dedicated warm target across its phases; never point two concurrent builds at it. See `rust-build`.
- **sccache does nothing here** (measured 0% cross-worktree hit — it keys on the rustc command line, which embeds the absolute target path). A stable per-tree target dir is the whole answer.

## Self-cleaning

- **A worktree owns its target.** `git worktree remove <path> --force` drops the worktree; `rm -rf <path>-target` drops its private target dir. Do both when the work lands. The shared `~/.cache/nub/shared-target` is intentionally left for the next worktree.
- **A sub-agent that built in an isolated target cleans it up on completion** — unless a serial chain will reuse it (then hand the warm target forward explicitly). Say which in the dispatch prompt.
- **Prune stale worktrees periodically.** `git worktree list` → remove dead ones → `git worktree prune`. The `worktree` skill owns the lifecycle; `cpu-reduction` §2b has the disk-pressure sweep.
- **Never `cp -r` the repo to isolate a build** — the tree carries multi-GB `target/`/`.repos/`/ `node_modules`. Use `git worktree add` or `git clone --depth 1 file://$PWD` + a private `CARGO_TARGET_DIR`.

## If it already orphaned

`cpu-reduction` §2: `ps ... | grep -Ei 'rustc|cargo|lld'`, find the detached build holding the lock, `pkill -f '<target-dir>'` (artifacts persist = still warm), hand the contention-free target to ONE fresh foreground build.
