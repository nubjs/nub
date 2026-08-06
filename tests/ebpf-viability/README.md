# eBPF viability probe — is a ring-buffer tracer a sound replacement for `strace` in the corpus harness?

A branch-scoped ad-hoc probe (no PR). It exists to settle one question with evidence: an earlier survey rejected eBPF for the Linux god-mode observation adapter after measuring 37–41% silent, non-deterministic event loss from `bpftrace`'s per-event `printf`. That measurement showed the *probe* fired for every event (an in-kernel `count()` returned the full number) while the *transport* dropped them. Perf buffer, not eBPF. This probe tests the configuration the survey named but never ran: `BPF_MAP_TYPE_RINGBUF` with an in-kernel drop counter.

## What it measures

| | question | how it is answered |
| --- | --- | --- |
| Q1–Q4 | BTF, hook availability, `BPF_PROG_TYPE_TRACING`, `BPF_MAP_TYPE_RINGBUF` | by attempting each operation, never by inferring from a kernel version |
| Q5 | does the program load and attach? | a real load + attach |
| Q6 | **completeness** | delivered vs an in-kernel oracle, five runs |
| Q7 | **honesty** | deliberate starvation must make the drop counter fire, with a positive control that must not |
| Q8 | overlayfs double-firing | a controlled overlay-vs-tmpfs pair |
| Q9 | overhead | same workload, wall clock, against `strace` |
| Q10 | coverage | a known-answer fixture scored by unique token against `strace -e trace=%file,%desc,%network,%process` |

## The design that makes the completeness number trustworthy

`fileprobe.bpf.c` increments **two** in-kernel counters from the *same* probe invocation:

- `seen` — incremented before `bpf_ringbuf_reserve()`. This is the oracle: the exact number of times the hook fired for the traced process tree. It needs no separate run and no cross-run denominator, so it cannot disagree with the delivered count for any reason except real loss.
- `dropped` — incremented when `bpf_ringbuf_reserve()` returns `NULL`.

A run is only believable when `delivered + dropped == seen`. The consumer additionally writes every event to a file, so the line count is an independent third check on `delivered`.

## Controls, and why each is there

Every one of these caught a real defect while the probe was being built:

- **The tmpfs-vs-overlayfs pair.** On overlayfs each VFS hook fires twice. 1000 opens gave `seen=2011` on overlayfs and `seen=1011` on tmpfs. Any completeness number taken on overlayfs without this control looks like a 2x over-count.
- **`pick-bpftool.sh`.** Ubuntu ships `/usr/sbin/bpftool` as a version-matching *wrapper* that prints an advisory and does nothing. Selecting it produced an empty BTF symbol list, so every hook read "absent" — which looks exactly like a real finding of "this kernel has no LSM hooks." The picker validates each candidate by making it do the real job, and Q2 hard-flags a symbol count under 50.
- **The `nub_this_symbol_does_not_exist` row.** `bpftrace -lv` exits 0 for a nonexistent function, so a presence check needs a negative control.
- **The positive control in Q7.** A drop counter that always reports loss is as useless as one that never does.
- **`t01_created` in `coverage-score.py`.** A mechanism that misses the positive control is misconfigured, and its other rows mean nothing.
- **The fixture's own exit code.** `fixture.c` returns non-zero unless every operation succeeded, so a missing token means the tracer missed it rather than the operation never happening.

## Reproducing locally

Docker gives the whole thing except the exact runner kernel:

```sh
docker run --rm -d --name ebpfbox --privileged -v "$PWD:/work" -w /work ubuntu:24.04 sleep infinity
docker exec ebpfbox apt-get update -qq
docker exec ebpfbox apt-get install -y -qq clang llvm libbpf-dev libelf-dev zlib1g-dev linux-tools-generic strace sudo
docker exec ebpfbox bash /work/probe.sh
docker rm -f ebpfbox
```

The container's kernel is Docker Desktop's LinuxKit, not the runner's, so trust the *order* of the overhead numbers there and the *digits* only from CI.
