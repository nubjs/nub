# Sizing libuv's threadpool

What Nub's cores-sized libuv threadpool protects and costs: which mechanism keeps a bigger pool off other work, why the CPU objection against it fails for an isolated container, and where it is paid for in memory.

Measured 2026-09-08 on GCE (the pool benchmarks under `tests/bench/runtime/`) and 2026-09-15 on a 4-CPU cgroup v1 box (the scheduling and memory probes below). Written after a Node maintainer objected publicly to the shipped default.

## The question

Node runs libuv's pool at four threads on every machine, and its own `max(4, cores)` proposal was closed. Nub ships that size on augmented runs, so the question is which objections to it survive contact with a measurement.

libuv hands async `fs`, `dns.lookup`, async `zlib` and `crypto`, and any addon using `uv_queue_work`, to a process-wide pool. It reads `UV_THREADPOOL_SIZE` once, at first use, so only the environment can size it. nodejs/node#61533 proposed `max(4, available parallelism)` for Node itself, collected eight approvals, and was closed in August 2026 with: "Just because there are many CPUs in a host, it doesn't mean they are free. Saturating the CPU is worse than queuing I/O tasks." The same position was restated publicly after Nub shipped the default, adding that it causes slowdowns and OOMs on a busy or dense server and is mostly safe on local development.

## What the bigger pool gains

Routes bound by the pool gain in proportion to physical cores; routes that touch it lightly are unchanged.

On a c3d-standard-16 (16 vCPU, 8 physical cores), Node 26.8.1, Fastify, autocannon at 64 connections, pool 4 against 16: bcrypt 3.7x, scrypt 2.6x, pbkdf2 2.8x, sharp thumbnails 3.2x, gzip of 1 MB 1.5x. On a c3d-standard-8 (4 physical cores) the same routes gain 1.2x to 1.8x. A 270 KB file read, a `stat` and a 55 KB gzip read within noise of the four-thread pool. Full rows: `tests/bench/runtime/results/`.

## Two mechanisms, each covering what the other cannot

A `nice` value bounds the pool against threads in its own cgroup; a cgroup's CPU weight bounds it against every other cgroup. Neither covers the other's case, and together they cover both.

`sched(7)`: "Under group scheduling, a thread's nice value has an effect for scheduling decisions only relative to other threads in the same task group." Four normal-priority spinners against four at nice 10 on 4 CPUs: in one cgroup the nice-10 side takes 11% of the CPU, across two cgroups of equal weight it takes 51%. So the demotion Nub applies to the workers beyond Node's four does nothing across a container boundary.

It does not need to. Weight bounds a group's share independently of its thread count. One cgroup fixed at four threads while the other varies:

| Other cgroup's threads | Fixed cgroup's throughput | Its share |
| --- | --- | --- |
| 4 | 195 Mloop/s | 51% |
| 16 | 192 Mloop/s | 49% |
| 64 | 191 Mloop/s | 47% |
| 16, both in one cgroup (control) | 58 Mloop/s | 19% |

Sixteen times the threads costs a neighbouring cgroup about 2%; the same threads inside one cgroup cost it 70%. The control rules out a failed cgroup assignment, which would have read 19% throughout.

**So the CPU objection does not hold for a cgroup-isolated deployment**, which is what a container or a systemd service is. Do not "fix" this by standing the sizing down there: it would disable the feature for Docker without `--cpus`, Kubernetes without `limits.cpu`, systemd services and CI, and buy no CPU safety. That stand-down was built and measured wrong before this page existed; see nubjs/nub#945.

## Where it is genuinely paid for

Pool size is the concurrency limit for pool-bound work, so peak memory tracks it — and in a cgroup whose CPU is bounded by weight rather than quota, that memory buys no throughput at all.

A cgroup weighted to roughly a ninth of the box, 24 `sharp` resizes of a 12 MP PNG, four busy neighbours in another cgroup:

| Pool | Throughput | Peak RSS over baseline |
| --- | --- | --- |
| 4 | 2.02 ops/s | +157 MB |
| 16 | 1.87 ops/s | +314 MB |
| 64 | 1.90 ops/s | +455 MB |

Throughput is flat, memory is 2.9x. The extra threads cannot run, because weight fixes the group's share, but each in-flight task still holds its working set. Uncontended the memory half holds alone: 32 queued PNG resizes peak at +137 MB, +386 MB and +585 MB for pools of 4, 16 and 64. The same count of JPEG resizes is far cheaper (30 / 67 / 142 MB) because libvips shrinks a JPEG on load and cannot for a PNG, so the per-task working set is a property of the workload, not a constant to size a pool against. glibc's per-thread arenas are a minor term: `MALLOC_ARENA_MAX=2` moved a pool-64 JPEG run from 213 MB to 188 MB.

Thread stacks are NOT the mechanism, and an OOM claim resting on them is wrong. libuv reserves 8 MB per worker; on Linux it stays virtual, so 128 workers add 2.4 GB of address space and nothing resident. Windows commits it, which is why the pool stops at [[crates/nub-core/src/node/spawn.rs#WINDOWS_THREADPOOL_CAP]] threads there.

## The gap that remains

[[crates/nub-core/src/node/spawn.rs#threadpool_size]] sizes from `available_parallelism`, which reports the cores the process can SEE. Under a CPU quota that equals what it can get; under a weight it can be an order of magnitude more, and the difference is the memory above. Tracked in nubjs/nub#947.

`available_parallelism` takes the minimum of the affinity mask and the cgroup CPU quota, so a quota'd container is already sized correctly. A memory limit constrains neither, so a pod with `limits.memory` and no `limits.cpu` — a common shape, because a CPU limit throttles — gets a pool sized to every core on the host. No clamp is applied today: the candidate rules all need a megabytes-per-thread budget or an entitlement estimate, and the per-task working set above spans 4x between two inputs to one library, so no constant here is defensible yet. The escape is an explicit `UV_THREADPOOL_SIZE`, which every launcher uses as is.

Two runtimes have already ruled on the entitlement estimate. The JDK derived its processor count from `cpu.shares` and removed that in JDK 19 ([JDK-8281181](https://bugs.openjdk.org/browse/JDK-8281181)): a share is a ratio against sibling groups that come and go, so a value read at start-up cannot say how much CPU the process will get, and the default of 1024 was read as one CPU on hosts that limited nothing. Go 1.25 made `GOMAXPROCS` container-aware from the CPU quota alone and left weight out for the same reason ([container-aware GOMAXPROCS](https://go.dev/blog/container-aware-gomaxprocs)). The memory-limit rule fails on reach rather than principle. Nearly every Kubernetes pod carries a memory limit, so a pool capped whenever one exists is the stand-down above under another name, and it gives up the throughput on every node that is not saturated. That leaves a fixed cap with no measurement behind it, or the explicit variable.

## Sources

The upstream proposal and its closing comment, the scheduler and libuv references, and the operator-facing guidance from the same maintainer, which recommends raising the pool for a deployment the operator sizes themselves.

- [nodejs/node#61533](https://github.com/nodejs/node/pull/61533), under [nodejs/performance#193](https://github.com/nodejs/performance/issues/193); [nodejs/node#57911](https://github.com/nodejs/node/issues/57911) for the Windows stack commit
- [sched(7)](https://man7.org/linux/man-pages/man7/sched.7.html), "The nice value and group scheduling"
- [libuv `threadpool.c`](https://github.com/libuv/libuv/blob/v1.52.1/src/threadpool.c), `init_threads`
- [sharp performance docs](https://github.com/lovell/sharp/blob/main/docs/src/content/docs/performance.md)
- [mcollina/skills, libuv-thread-pool rule](https://github.com/mcollina/skills/blob/main/skills/nodejs-core/rules/libuv-thread-pool.md)

## Changelog

One dated bullet per revision, with a `REVERSAL:` marker where a later finding overturned an earlier one.

- 2026-09-19 — Prior art on the entitlement estimate: the JDK removed `cpu.shares` sizing in JDK 19 and Go 1.25 sizes from the quota alone, so that candidate is closed; the memory-limit candidate is rejected on reach.
- 2026-09-15 — REVERSAL: an earlier draft of this page concluded that because `nice` does not cross a cgroup boundary, the pool must stand down to Node's four in a non-root cgroup with no quota. Measuring the neighbour rather than the nice disproved it — cgroup weight already bounds the share regardless of thread count, so that stand-down protected nothing and would have disabled the sizing across most Linux server deployments. The real cost is memory, and it is now the only open item.
- 2026-09-15 — First version: the cross-cgroup scheduling probes, the contended and uncontended memory measurements, and the premise check against a live cgroup v1 hierarchy. The 2026-09-08 benchmarks behind nubjs/nub#919 are summarised, not re-run.
