# Results — is eBPF workable if configured properly, and should we switch?

**Short answer: the transport defect is real and fully fixable, and the ring buffer is exactly as good as hoped — but the performance case that motivated the question does not survive contact with the real workload, and the VFS hook has a specific blind spot the harness's own filter policy depends on. Recommendation: do not switch the Linux adapter now.**

Everything below is MEASURED unless labelled INFERRED.

## Venue

| | |
| --- | --- |
| primary | `nub-corpus-linux`, Ubuntu 24.04, **kernel 6.17.0-1021-gcp, x86_64**, 8 vCPU. Same kernel family as GitHub's `6.17.0-1020-azure`, one patch release apart. |
| secondary | Docker Desktop LinuxKit **6.10.14 arm64**, `ubuntu:24.04 --privileged` |

The primary box was shared with another agent running corpus measurements, so absolute wall-clock digits are contended. Every ratio below is from arms run back-to-back on the same box.

## 1. The ring buffer does everything the survey said it would

The program increments `seen` before `bpf_ringbuf_reserve()` and `dropped` when it returns `NULL`, both from the same probe invocation, so the denominator needs no second run.

**Completeness — 5/5 runs, 8 MB ring, N=300,000 on tmpfs:**

```
RESULT seen=300007 submitted=300007 dropped=0 delivered=300007   (x5, identical)
   independent check: lines written by consumer = 300007         (x5, identical)
```

Three instruments agree: the in-kernel counter, the userspace delivered count, and the line count of the file the consumer wrote. Deterministic across runs, which `bpftrace`'s perf path was not.

**Honesty — the drop counter fires on demand and stays silent when it should:**

| arm | seen | delivered | dropped | sums? |
| --- | --- | --- | --- | --- |
| 4 KB ring, N=300,000 | 300,007 | 299,626 | 381 | exact |
| 8 MB ring, consumer stalls 5 ms/event, N=20,000 | 20,007 | 15,435 | 4,572 | exact |
| **positive control** — 8 MB ring, no stall, N=20,000 | 20,007 | 20,007 | **0** | exact |

`delivered + dropped == seen` in every arm. The counter also fired unprompted the very first time the ring was undersized, which is how the sizing question got answered at all.

**The perf-transport defect reproduces, and the ring buffer is the fix.** Same box, same workload, same hook, same event content (path + pid):

| transport | run 1 | run 2 | run 3 | loss reported |
| --- | --- | --- | --- | --- |
| `bpftrace` per-event `printf` (perf) | 228,281 | 241,418 | 233,977 | **none — stderr silent** |
| libbpf `BPF_MAP_TYPE_RINGBUF` (8 MB) | 300,011 | 300,011 | 300,011 | 0 drops, exact when starved |

⇒ **The survey's rejection was correct about `bpftrace` and wrong as a verdict on eBPF.** The transport was the defect; the ring buffer removes it and makes loss accountable to the event.

## 2. But the performance case does not survive the real workload

The microbenchmark reproduces. On `nub-corpus-linux`, 200,000 `open`/`close` pairs:

| arm | median | x baseline |
| --- | --- | --- |
| baseline | 714.7 ms | 1.0x |
| eBPF ringbuf, paths on, every event persisted | 1,055 ms | **1.44x** |
| `strace -f --seccomp-bpf -e trace=%file` | 12,706 ms | **17.8x** |
| `strace -f -e trace=%file` | 23,044 ms | **32x** |

**On a real install workload it collapses.** `npm rebuild sqlite3 --build-from-source`, artifact presence asserted each run:

| arm | wall | x baseline | events captured |
| --- | --- | --- | --- |
| baseline | 126.1 s / 127.5 s | 1.00x | — |
| eBPF ringbuf, every event persisted | 128.9 s | **1.02x** | 8,796, zero drops |
| `strace -f -y -e trace=%file,%desc,%network,%process` | 136.4 s | **1.08x** | 122,029 lines |

A native build is CPU-bound in `cc1plus`. strace costs about **8%** there, not 250%. The whole premise of the question — *"its performance is so much faster"* — is true of a syscall microbenchmark and very nearly irrelevant to the workload the corpus actually runs.

⇒ Switching buys roughly **6 percentage points** of wall clock on a real package build, against a rewrite of the Linux adapter.

## 3. The blind spot that decides it

`security_file_open` never fires for an open that was refused. Known-answer fixture, three opens in one process:

| open | result | eBPF VFS hook | `strace -e trace=%file` |
| --- | --- | --- | --- |
| `DENIED_EACCES.txt`, mode 000 | `-1 EACCES` | **MISSED** | SEEN, with errno |
| `MISSING_ENOENT.txt` | `-1 ENOENT` | **MISSED** | SEEN, with errno |
| `ALLOWED_CONTROL.txt` | `= 3` | **SEEN** | SEEN |

The control proves the probe was live in that same run. The mechanism: DAC permission is decided in `inode_permission()`, which returns before `do_dentry_open()` reaches the LSM hook.

**This collides with the harness's stated filter policy**, which keeps `-1 EACCES`/`EPERM` precisely because a refusal is the signal that a grant is missing. Missing ENOENT is harmless and in fact desirable — the policy drops those anyway, and it explains most of the 122,029 → 8,796 gap as pure noise reduction. Missing EACCES is a real regression against the design.

**The repair exists and was verified**, so this is a cost rather than a dead end: `kretprobe:do_sys_openat2` tallied `-13` (EACCES) and `-2` (ENOENT) alongside the three successful fds on the same fixture. A complete eBPF tracer therefore needs a VFS hook *and* a correlated syscall-return hook — bounded work, but not a drop-in, and it re-introduces a syscall-layer component to the design whose absence was one of the arguments for the VFS layer.

## 4. Coverage is a tie

Known-answer fixture, scored by unique token, on the 6.17 box:

| | eBPF VFS | `strace` with `%file,%desc,%network,%process` |
| --- | --- | --- |
| score | **9/9** | **9/9** |

Both see the rename destination, the hard-link and `linkat(dirfd)` destinations, the symlink, `mkdir`, the `MAP_SHARED|PROT_WRITE` target, and the writes of both a forked child and an exec'd grandchild.

⛔ **A correction to the earlier survey's framing.** The arm64 finding that `sys_enter_renameat2` fired zero times is a liability of subscribing to raw *tracepoints* by name, not of `strace`. On x86_64 glibc issues plain `rename`, and strace's `%file` class renders both ends: `rename("…/t01_created.txt", "…/t04_renamedest.txt") = 0`. An earlier reading of mine said strace showed no rename at all — that was a broken grep in this repo's own probe script, caught by the token scorer disagreeing with it.

What the eBPF output *is* genuinely nicer at is shape. One event carries both ends with no fd table, and fd-only operations arrive path-resolved by the kernel:

```
RENAME  pid=10471 t01_created.txt -> t04_renamedest.txt
LINK    pid=10471 t04_renamedest.txt -> t06_linkatdest.txt
SETATTR pid=10471 t01_created.txt          <- from ftruncate/fchmod, no path in the syscall
MMAPW   pid=10471 t11_mmapdest.bin
```

That is a decoder-simplicity argument, not a completeness one.

## 5. Environment — the deployment question, answered by attempting each operation

On kernel 6.17.0-1021-gcp:

| question | answer |
| --- | --- |
| BTF present | yes, `/sys/kernel/btf/vmlinux`, 7,074,017 bytes |
| `BPF_PROG_TYPE_TRACING` | available, and a real program loaded and attached |
| `BPF_MAP_TYPE_RINGBUF` | available |
| `security_inode_*` / `vfs_*` / `wake_up_new_task` | all present |
| **`security_path_*`** | **13 symbols present** (vs **1** on LinuxKit) |
| active LSMs | `lockdown,capability,landlock,yama,apparmor,ima,evm` |
| `unprivileged_bpf_disabled` | 2 — irrelevant, the probe runs as root |

⇒ The `CONFIG_SECURITY_PATH` concern is **resolved for the deployment kernel**: AppArmor and Landlock are both active and BuildXL's entire hook set is reachable. LinuxKit was the outlier, and any hook-availability conclusion drawn there does not transfer.

**Overlayfs doubling confirmed on the real box**, so it must be accounted for in any completeness number: 1,000 opens gave `seen=2007` on overlayfs and `seen=1007` on tmpfs.

## Verdict

**Is eBPF workable if configured properly? Yes — the ring buffer delivers 100% and reports loss exactly, and the survey's rejection was a verdict on `bpftrace`'s perf transport rather than on eBPF.**

**Should we switch? No, not now.** The two reasons the question was worth asking both weakened under measurement:

1. The **~250x** performance argument is a microbenchmark artefact. On a real native package build the gap is **1.02x vs 1.08x** — about 6 points.
2. The claim that strace *perturbs what it measures* rests on the same artefact. At 1.08x on the real workload, strace is not meaningfully deforming the timing of an install, so harness anomalies should be re-attributed rather than blamed on the instrument.

And switching now would trade a tracer that structurally cannot lose an event for one that needs a second, syscall-layer hook bolted on to recover refusals it cannot see.

**What is worth keeping from this:** the ring-buffer + drop-counter design is sound and this harness is a working reference implementation of it. If a future workload is genuinely syscall-bound — a very large pure-JS install with no compile step, where the ratio would be closer to the microbenchmark than to the sqlite3 build — re-open with that workload as the benchmark. The remaining measurement that would move this verdict is the same three-arm comparison on an install-heavy, compile-free package.

## Reproducing

`bash tests/ebpf-viability/probe.sh` on any Linux box with `clang`, `libbpf-dev`, `linux-tools-generic` and `strace`; `README.md` has the Docker recipe and describes each control and the defect it caught.

⚠️ The branch-scoped workflow in `.github/workflows/ebpf-viability.yml` **never fired a run** on push — only a Vercel check appeared on the commit. It is left in place unfixed; the VM was the better venue and the question was answered there.
