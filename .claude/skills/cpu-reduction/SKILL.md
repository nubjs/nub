---
name: cpu-reduction
description: >-
  Diagnose and clear CPU, memory, and disk contention on the maintainer's dev
  host. Invoke when the machine is slow, load or swap is high, disk is filling,
  a Rust build hangs on a target lock, a benchmark needs a quiet host, stale
  worktree targets have accumulated, or orphaned build/test processes are
  suspected. Covers safe worktree-target cleanup, detached Rust builds,
  orphaned Node tests and synthetic load generators, the preserve list, and the
  measure-to-verify loop. For preventing orphan builds, use
  `rust-build-hygiene`.
metadata:
  internal: true
---

# CPU / disk reduction — clear build & test residue

The maintainer's Mac is 10 CPUs / 64 GiB. Three residue families need different treatment — diagnose by symptom before sweeping.

| Family | Symptom | Cost | Cause |
| --- | --- | --- | --- |
| **Old Rust builds** | a build hangs on `Blocking waiting for file lock`; a few `rustc`/`cargo`/`lld` pegged; disk filling | LOCK contention + CPU + tens of GB of `target/` | detached/orphaned builds that outlived their launcher; abandoned worktree `target/` dirs |
| **Orphaned node tests** | load floor sits ~20 with nothing building | CPU + swap | `node`/`tsx`/`esbuild`/`tsc -w` reparented to launchd when their harness died |
| **Orphaned load generators** | load in the **hundreds**; N identical `/bin/zsh` at 25–70%, consecutive PIDs, same start second | CPU — starves every build | a probe spawned `(while :; do :; done) &` and died before its cleanup line ran |

**A high load FLOOR with nothing building is almost never active compilation — it is orphaned node processes** (§3). A *hung build* or *filling disk* is the Rust family (§2).

> Prevention beats cleanup: `rust-build-hygiene` is how to spin builds up so they die with you.

## 1. Measure

```sh
uptime                                   # load; the 1-min figure LAGS — see §7
sysctl -n hw.ncpu vm.swapusage
memory_pressure | grep -i "free percentage"
df -h ~/.cache                            # disk — worktree target/ dirs live here
ps -Ao pid,ppid,pgid,%cpu,%mem,rss,etime,state,comm -r | head -40
```

Load well above `hw.ncpu` with nothing building → §3. A build stuck on a lock, or `~/.cache` near full → §2.

## 2. Old Rust builds — locks, orphans, disk

### 2a. Orphaned builds holding a target lock (the hang)

A build that outlived its launcher still holds a target-dir lock, so a live build blocks behind it — often 30+ minutes, reading as "the build is just slow."

```sh
# builds still running, with age + which target dir
ps -Ao pid,ppid,%cpu,etime,command | grep -Ei 'rustc|cargo|ld64|lld|cc1' | grep -v grep
# is a build blocked on a lock right now? (check the build log)
grep -l 'Blocking waiting for file lock on' /tmp/*build*.log 2>/dev/null
pkill -f '<target-dir-path>'              # kill the orphaned builds for that target
```

Cause is almost always TWO builds on ONE target dir — most often a stopped agent's **detached** cargo build that `TaskStop` did not reap (TaskStop kills the agent, not its background bash jobs; setsid/nohup-detached builds reparent to PID 1). Artifacts persist and stay warm — hand the contention-free target to ONE fresh foreground build. Never point two concurrently-building trees at one target dir.

### 2b. Worktree-owned Rust targets eating disk

```sh
# Audit only. This is the default.
python3 .claude/skills/cpu-reduction/scripts/clean-worktree-targets.py

# Repeat every safety check, then delete eligible build output.
python3 .claude/skills/cpu-reduction/scripts/clean-worktree-targets.py --apply
```

It deletes no worktree, branch, source file, or shared cache. Layouts it covers:

```text
~/.cache/nub/worktrees/<slug>/target/
~/.cache/nub/worktrees/<slug>/aube-target/
~/.cache/nub/worktrees/<slug>/target-linux/
~/.cache/nub/worktrees/<slug>-target/
~/.cache/nub/worktrees/<slug>-launcher-target/
~/.cache/nub/worktrees/<slug>-target-native/
```

Spellings vary; the invariant is a top-level target-named directory inside a registered worktree, or a target-named sibling prefixed by that worktree's full basename.

Safety model: the cleaner protects an owner's entire target set when `git status --porcelain` reports any staged, unstaged, or untracked work — an uncommitted worktree is active state. It refuses symlinks, unmatched directories, targets owning the installed `nub-dev`/`nubx-dev` binary, and apply mode while any Rust build is running. Status and process state are rechecked at the deletion boundary.

**Do not remove clean worktree checkouts merely to recover disk.** Build output is regenerable; a checkout and branch are not interchangeable with cache. Remove a checkout only via the `worktree` skill after proving it abandoned and pushed.

**Shared targets stay warm** — they seed fresh worktrees and may own the installed `nub-dev` binary. Never include them in a worktree cleanup; prune only with explicit operator approval:

```text
~/.cache/nub/shared-target
~/.cache/nub/shared-target-<content-hash>
```

**Judge every reclaim by `df`, never `du`.** APFS clone/shared-block accounting means summed `du` sizes need not equal the `df` delta, and `du -sh` over the worktree root can take minutes.

```sh
df -h /System/Volumes/Data
```

### 2c. Orphaned `rustc` at high CPU

Rare — Rust builds are noisy but they finish. If `ps` shows a long-`etime` `rustc`/`cc1plus`/`lld` at high %cpu with no parent cargo, it's an orphan; `kill -9` after confirming its `ppid` chain has no live build.

### 2d. Orphaned synthetic load generators (load in the hundreds)

A probe that needs contended measurement sometimes manufactures the contention and then dies before cleanup, leaving busy-loops spinning forever under launchd.

Signature — **all four together**, never `/bin/zsh` alone:

```sh
ps -Ao pid=,ppid=,%cpu=,etime=,comm= | awk '$2==1 && $3>20 && $5=="/bin/zsh"'
ps -o command= -p <pid>          # full argv reveals the `while :; do :; done` and its dead cleanup
pgrep -P <pid>                   # NO children — it is not wrapping a real build
```

Consecutive PIDs + identical start second + `PPID=1` + no children + a spinning loop in the argv = safe to `kill -9`. **Read the full argv first** — an agent's own background `cargo` also runs as `/bin/zsh -c source <snapshot> && …`, and that one *has* children and must be left alone.

**`pgrep -f` can return 0 for everything** when it cannot read other processes' argv under the sandbox. It reports absence, not an error — cross-check with `ps aux | grep` before concluding "nothing is running."

Prevention, in the probe itself:

```sh
LOADPIDS=$(jobs -p)
trap 'kill $LOADPIDS 2>/dev/null' EXIT INT TERM
```

## 3. Orphaned node test processes (the load floor)

Orphans are `PPID=1`:

```sh
# every orphaned dev process, with what it costs
ps -Ao pid=,ppid=,%cpu=,rss=,etime=,command= | awk '$2==1' \
  | grep -Ei 'nvm/versions/node|/bin/tsx|esbuild --service|cache/nub/worktrees|tsc -b -w'
# what it totals
ps -Ao ppid=,user=,rss=,command= | awk '$1==1 && $2=="colinmcd94"' \
  | grep -Ei 'nvm/versions/node|/bin/tsx|esbuild --service|cache/nub/worktrees|tsc -b -w' \
  | awk '{s+=$3; n++} END {printf "count=%d  RSS=%.2f GB\n", n, s/1048576}'
```

`PPID=1` also matches ~445 legitimate macOS agents (`/System`, `/usr/*`, `/Applications`). **Always narrow by command pattern; never sweep on `PPID=1` alone.** Two cost shapes: CPU burners (a handful at 60–90%) and memory ballast (hundreds idle at ~0% CPU, ~52 MB RSS each).

## 4. Capture evidence before you kill

A process spinning for days is a reproducible bug; killing it destroys the only live instance.

```sh
sample <pid> 3 -mayDie                     # writes /tmp/<comm>_<ts>.sample.txt
ps -Ao pid,ppid,pgid,%cpu,rss,etime,state,lstart,command > ps-snapshot.txt
```

Known nub fixture-hang signature:

```
v8::internal::Isolate::StackOverflow
v8::internal::ErrorUtils::Construct
v8::internal::Isolate::CaptureAndSetErrorStack
```

= infinite recursion → `RangeError: Maximum call stack size exceeded` → source-mapped over a maximally deep stack (nub passes `--enable-source-maps`) → something catches the RangeError and retries → never dies. The fix belongs in the recursion guard + catch site, not the sweep.

## 5. Preserve list — check before killing

Standing instruction: kill anything not doing productive work, but have high confidence it is not in fact productive. **Never kill:**

- Anything holding a listening socket — `lsof -nP -iTCP -sTCP:LISTEN | awk 'NR>1{print $2}' | sort -u`, intersect with candidates (dev servers, `fray-ui`, `agent-browser`).
- `tmux` sessions (they host live `claude`/fray workers).
- `claude` processes and anything whose PPID chains to one.
- `node --inspect-brk` (a debugger may be attached).
- Any `cargo`/`rustc` in a LIVE build tree (check the `ppid` chain).

Everything else — orphaned `tsx` runs, `node --watch`, `tsc -b -w`, `esbuild --service --ping`, `nub` fixture runs from `~/.cache/nub/worktrees`, detached `cargo`/`rustc` whose launcher is gone — is safe once `PPID=1` and more than a few hours old.

## 6. Sweep — four gotchas

- **`kill $PIDS` silently no-ops with a large PID list** (returns 0, kills nothing). Since `SIGKILL` can't be ignored, "survived SIGKILL" always means it was never delivered. Use `awk '{print $1}' candidates.txt | xargs -n 20 kill -9`.
- **Killing parents reparents their children — sweep in passes.** Re-run identify until the count hits zero.
- **A `%cpu == 0` filter misses near-zero idlers.** Filter by `PPID=1` + command + age; treat CPU as info, not the predicate.
- **`SIGTERM` won't be serviced by a process stuck in a JS recursion loop** (event loop unreachable). `SIGTERM` the sleeping Rust parents (temp files to clean); `SIGKILL` straight to spinning node children.

## 7. Verify

```sh
uptime; sysctl -n vm.swapusage; df -h ~/.cache; ps -Ao pid,%cpu,%mem,etime,comm -r | head -8
```

**Load average lags** — it reads *higher* right after the kills and decays over minutes. Swap + `memory_pressure` respond faster; trust those first. An `mds_stores` spike after a large sweep is a transient reaction to hundreds of exits.

## 8. Standing risks

- `~/.cache/nub/worktrees` disk growth. Pruning the CHECKOUTS reclaims almost nothing — `scripts/rust-build.sh` CoW-clones with `cp -c`, so `du` bills shared APFS blocks to every referencing path. **The `<name>-target` dirs are the real consumers; hunt orphaned ones first.** Removing a CLEAN worktree is lossless (`git worktree remove` drops the directory; branch and commits stay), so `git status --porcelain` is the whole guard.
- **Keep stderr visible when a command's failure is what you are diagnosing** — a silenced `git worktree remove … 2>/dev/null` in a loop makes a failed removal look like a CoW accounting effect.
- Each abandoned fixture run leaks a `nub` + `node` pair. Real fix is upstream (nub should kill its node child on exit; the harness should signal the process group). Until then, `rust-build-hygiene` is how to stop creating the residue.
