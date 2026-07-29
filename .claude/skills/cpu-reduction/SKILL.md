---
name: cpu-reduction
description: >-
  Diagnose and clear CPU/memory/DISK contention on the maintainer's dev host so
  iteration stays fast. Invoke (via the Skill tool) whenever the machine feels
  slow, load average is high, swap is filling, disk is filling, a build hangs on
  a file lock, a benchmark needs a quiet box, or someone suspects "orphaned
  builds". Two residue families: (1) OLD RUST BUILDS — detached/orphaned
  `cargo`/`rustc` that outlived their launcher (setsid/nohup, or a TaskStop'd
  agent's surviving background build) holding target-dir LOCKS and burning cores,
  plus stale per-worktree `target/` dirs eating tens of GB of disk; (2) orphaned
  NODE test processes from dead `cargo test`/fixture harnesses, which are the
  bigger CPU contributor; (3) orphaned SYNTHETIC LOAD GENERATORS — `while :; do
  :; done` shells a probe spawned to make a contended measurement and never
  reaped, which drive load into the hundreds. Encodes the
  measure→identify→preserve→sweep→verify loop, what is safe to kill vs must be
  kept, and the gotchas (kill silently no-ops, reparenting needs multiple passes,
  load average lags, `pgrep -f` can silently see nothing). For PREVENTING orphan
  builds in the first place, see the `rust-build-hygiene` skill.
metadata:
  internal: true
---

# CPU / disk reduction — clear build & test residue

Clearing contention on the maintainer's Mac (10 CPUs, 64 GiB) so the dev loop stays fast. There are
**two distinct residue families**, and they need different treatment. Diagnose which one you have
before sweeping — do not assume.

| Family | Symptom | Cost | Cause |
| --- | --- | --- | --- |
| **Old Rust builds** | a build hangs on `Blocking waiting for file lock`; a few `rustc`/`cargo`/`lld` pegged; disk filling | LOCK contention (stalls other builds) + CPU + tens of GB of `target/` disk | detached/orphaned builds that outlived their launcher; abandoned worktree `target/` dirs |
| **Orphaned node tests** | load floor sits ~20 with nothing building | CPU (the load) + swap (memory ballast) | `node`/`tsx`/`esbuild`/`tsc -w` reparented to launchd when their `cargo test`/fixture harness died |
| **Orphaned load generators** | load in the **hundreds**; N identical `/bin/zsh` at 25–70% each, consecutive PIDs, same start second | CPU — starves every build on the box | a probe spawned `(while :; do :; done) &` to force a contended measurement, then died before its own cleanup line ran |

The headline lesson (verified 2026-07-26): **a high load FLOOR with nothing building is almost never
active compilation — it is orphaned node test processes.** A single sweep that day killed 219
processes and took load `28 → 9.4`, swap `5.6 GB → 2.8 GB`. So for a high *load average*, check node
residue first (§3). For a *hung build* or *filling disk*, it's the Rust family (§2). The instinct
"it must be orphaned `rustc`" has been wrong for CPU every time — but orphaned Rust builds are exactly
the LOCK-contention and disk problem, so both are real; pick by symptom.

> Prevention beats cleanup. The recurring root cause of orphaned Rust builds is launching them in a
> way that survives the launcher — the `rust-build-hygiene` skill is how to spin builds up so they
> die with you and clean themselves up. This skill is the mop; that one is the "don't spill."

## 1. Measure

```sh
uptime                                   # load; the 1-min figure LAGS — see "verify" below
sysctl -n hw.ncpu vm.swapusage
memory_pressure | grep -i "free percentage"
df -h ~/.cache                            # disk — worktree target/ dirs live here
ps -Ao pid,ppid,pgid,%cpu,%mem,rss,etime,state,comm -r | head -40
```

Load meaningfully above `hw.ncpu` with nothing building → §3 (node). A build stuck on a lock, or
`~/.cache` near full → §2 (Rust).

## 2. Old Rust builds — locks, orphans, and disk

### 2a. Orphaned / detached builds holding a target lock (the hang)
The most damaging Rust residue: a build that outlived its launcher and still holds a target-dir lock,
so a live build blocks behind it — often for 30+ minutes reading as "the build is just slow."

```sh
# builds still running, with age + which target dir
ps -Ao pid,ppid,%cpu,etime,command | grep -Ei 'rustc|cargo|ld64|lld|cc1' | grep -v grep
# is a build blocked on a lock right now? (check the build log)
grep -l 'Blocking waiting for file lock on' /tmp/*build*.log 2>/dev/null
```

Cause is almost always TWO builds on ONE target dir — most insidiously a STOPPED/zombie agent's
**detached** cargo build that `TaskStop` did NOT reap (TaskStop kills the agent, not its background
bash jobs; setsid/nohup-detached builds reparent to PID 1 and run on). Fix:

```sh
pkill -f '<target-dir-path>'              # kill the orphaned builds for that target
```

The target's **artifacts persist** (still warm) — hand the contention-free target to ONE fresh
foreground build. Never point two concurrently-building trees at one target dir (cargo serializes on
its lock; that IS the contention). This is the exact hang diagnosed 2026-07-26; the `no-detached-orphan-builds`
memory + `rust-build-hygiene` skill prevent it.

### 2b. Stale worktree `target/` dirs eating disk
Each abandoned worktree leaves a private `target/` behind. 2026-07-26: **149 GB across 27 worktrees**
in `~/.cache/nub/worktrees`, on a data volume 95% full. Prune stale worktrees (the `worktree` skill
owns their lifecycle):

```sh
git worktree list                                            # what's live
du -sh ~/.cache/nub/worktrees/* 2>/dev/null | sort -h | tail  # biggest offenders
git worktree remove <path> --force && rm -rf <path>-target   # drop a dead one + its target
git worktree prune                                           # clean the admin records
```

Never delete a target dir of a worktree with a build in flight or uncommitted WIP. Confirm the
worktree is abandoned (merged/dead branch, no running build) first.

### 2c. Orphaned `rustc` at high CPU
Rare (Rust builds are noisy but they *finish*), but if `ps` shows a long-`etime` `rustc`/`cc1plus`/
`lld` at high %cpu with no parent cargo, it's an orphan from a killed build — safe to `kill -9` once
you've confirmed it's not part of a live build tree (check its `ppid` chain).

## 2d. Orphaned synthetic load generators (load in the hundreds)

A probe that needs a *contended* measurement sometimes manufactures the contention:

```sh
for c in 1 2 3 4 5 6 7 8 9 10; do (while :; do :; done) & done
LOADPIDS=$(jobs -p)
... measure ...
kill $LOADPIDS 2>/dev/null      # <-- never runs if the agent dies first
```

When the agent is interrupted before that last line, the busy-loops reparent to launchd and spin
**forever**. Observed 2026-07-28: ten of them at ~65% CPU each — **234% of a 10-core box** — for
3h55m, driving load to **277** and starving ~28 worktrees' builds. Every timing taken during that
window was garbage.

Signature (all four together — do not kill on `/bin/zsh` alone):
```sh
ps -Ao pid=,ppid=,%cpu=,etime=,comm= | awk '$2==1 && $3>20 && $5=="/bin/zsh"'
ps -o command= -p <pid>          # full argv reveals the `while :; do :; done` and its dead cleanup
pgrep -P <pid>                   # NO children — it is not wrapping a real build
```
Consecutive PIDs + an identical start second + `PPID=1` + no children + a spinning loop in the argv
= safe to `kill -9`. **Read the full argv first**: an agent's own background `cargo` also runs as
`/bin/zsh -c source <snapshot> && …`, and that one *does* have children and must be left alone.

**Diagnostic trap that wasted a cycle here:** `pgrep -f` returned **0 for everything** — including
`claude`, which was certainly running — because it could not read other processes' argv under the
sandbox. It reports absence, not an error. Cross-check with `ps aux | grep` before concluding
"nothing is running"; a `pgrep` zero is not evidence.

**Prevention (belongs in the probe, not here):** any script spawning load generators must trap its
own cleanup the moment it captures the PIDs, so an interrupted agent still reaps them:
```sh
LOADPIDS=$(jobs -p)
trap 'kill $LOADPIDS 2>/dev/null' EXIT INT TERM
```

## 3. Orphaned node test processes (the load floor)

Orphans are `PPID=1` (reparented to launchd when their harness died). That predicate finds nearly all
of it:

```sh
# every orphaned dev process, with what it costs
ps -Ao pid=,ppid=,%cpu=,rss=,etime=,command= | awk '$2==1' \
  | grep -Ei 'nvm/versions/node|/bin/tsx|esbuild --service|cache/nub/worktrees|tsc -b -w'
# what it totals
ps -Ao ppid=,user=,rss=,command= | awk '$1==1 && $2=="colinmcd94"' \
  | grep -Ei 'nvm/versions/node|/bin/tsx|esbuild --service|cache/nub/worktrees|tsc -b -w' \
  | awk '{s+=$3; n++} END {printf "count=%d  RSS=%.2f GB\n", n, s/1048576}'
```

`PPID=1` also matches ~445 legitimate macOS agents (`/System`, `/usr/*`, `/Applications`) — those are
launchd-managed and normal. Always narrow by command pattern; never sweep on `PPID=1` alone. Two cost
shapes: **CPU burners** (a handful pegged at 60–90% — the load) and **memory ballast** (hundreds idle
at ~0% CPU, ~52 MB RSS each — the swap).

## 4. Capture evidence before you kill

A process spinning for days is a **reproducible bug**; killing it destroys the only live instance.

```sh
sample <pid> 3 -mayDie                     # writes /tmp/<comm>_<ts>.sample.txt
ps -Ao pid,ppid,pgid,%cpu,rss,etime,state,lstart,command > ps-snapshot.txt
```

The known nub fixture-hang signature (a crash that became a multi-day CPU burn):
```
v8::internal::Isolate::StackOverflow
v8::internal::ErrorUtils::Construct
v8::internal::Isolate::CaptureAndSetErrorStack
```
= infinite recursion → `RangeError: Maximum call stack size exceeded` → source-mapped over a maximally
deep stack (nub passes `--enable-source-maps`) → something catches the RangeError and retries → never
dies. If you see this, the fix belongs in the recursion guard + catch site, not the sweep.

## 5. Preserve list — check before killing

Maintainer's standing instruction: *"kill anything not doing productive work, but have high
confidence it is not in fact productive."* Confidence comes from checking. **Never kill:**

- Anything holding a listening socket — `lsof -nP -iTCP -sTCP:LISTEN | awk 'NR>1{print $2}' | sort -u`, intersect with candidates (dev servers, `fray-ui`, `agent-browser`).
- `tmux` sessions (they host live `claude`/fray workers, maybe yours).
- `claude` processes and anything whose PPID chains to one (active agent work).
- `node --inspect-brk` (a debugger someone may be attached to).
- Any `cargo`/`rustc` in a LIVE build tree (check the `ppid` chain to a live cargo/agent).

Everything else in the residue set — orphaned `tsx` runs, `node --watch`, `tsc -b -w`,
`esbuild --service --ping`, `nub` fixture runs from `~/.cache/nub/worktrees`, and detached
`cargo`/`rustc` whose launcher is gone — is safe once it's `PPID=1` and more than a few hours old.

## 6. Sweep — four gotchas that make naive attempts fail

- **`kill $PIDS` silently no-ops with a large PID list** (returns 0, kills nothing). Since `SIGKILL` can't be ignored, "survived SIGKILL" always means it was never delivered. Use `xargs`: `awk '{print $1}' candidates.txt | xargs -n 20 kill -9`.
- **Killing parents reparents their children — sweep in passes.** Re-run identify until the count hits zero (2026-07-26 took four: 16 → 133 → 60 → 10).
- **A `%cpu == 0` filter misses idlers that aren't quite zero** (10 `esbuild --service --ping` at ~2% survived three passes). Filter by `PPID=1` + command + age; treat CPU as info, not the predicate.
- **`SIGTERM` won't be serviced by a process stuck in a JS recursion loop** (event loop unreachable). `SIGTERM` the sleeping Rust parents (temp files to clean); `SIGKILL` straight to spinning node children.

## 7. Verify

```sh
uptime; sysctl -n vm.swapusage; df -h ~/.cache; ps -Ao pid,%cpu,%mem,etime,comm -r | head -8
```

**Load average is a lagging exponential average — do not judge the sweep immediately.** It reads
*higher* right after the kills (briefly hit 28 during the successful sweep) and decays over minutes.
Swap + `memory_pressure` respond faster; trust those first. A `mds_stores` spike right after a large
sweep is a transient reaction to hundreds of exits, not a Spotlight problem — re-check a minute later.

## 8. Standing risks worth reporting
- `~/.cache/nub/worktrees` disk growth (149 GB / 27 worktrees, 2026-07-26) — prune stale worktrees.
- Each abandoned fixture run leaks a `nub` + `node` pair. Real fix is upstream (nub should kill its node child on exit; the harness should signal the whole process group). Until then this is recurring maintenance — and `rust-build-hygiene` is how to stop CREATING the residue.
