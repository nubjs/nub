# The Linux `write:"disk"` residual is one mechanism, reached two ways

**Status: closed.** Every Linux package that could only install at `write:"disk"` fails for the same structural reason — a process needs to read its own `/proc/self/<file>`, and Landlock cannot express that. Two different runtimes reach it by two different routes, and one package turned out not to belong at all.

## The population

Eight records carried `write:"disk"` on Linux (0.5% of ~2,196 MINIMUM records). Measured individually:

| package | fatal refusal | how it was established |
| --- | --- | --- |
| `dotnet-2.0.0@1.4.4` | `/proc/self/stat` | strace, last syscall before `exit_group(1)` |
| `@nuxt/components@2.1.0` | `/proc/self/stat` | the runtime printed its own stack |
| `@tensorflow/tfjs-backend-wasm@1.4.0-alpha2` | `/proc/self/stat` | same error, verbatim |
| `codeceptjs@1.1.3` | `/proc/self/stat` | same stack; dies in **preinstall**, not postinstall |
| `postman-code-generators@2.1.1` | `/proc/self/stat` ×2 | same error; survives two levels of nesting |
| `react-native-purchases@1.5.4` | `/proc/self/stat` | same stack |
| `@opencode-ai/cli@0.0.0-next-16573` | `/proc/self/maps` | single-variable Landlock arms (below) |
| `iedriver@4.0.0` | **none** | **installs completely under the narrow grant; the record was stale** |

`iedriver@4.0.0` exits 0 with 2,717 files and `Success! IEDriverServer binary available at …` under `{write:{deps,project,userHome},network:true}`. Its postinstall downloads a binary, so a transient fetch failure during the original sweep would have walked the ladder to `write:"disk"` with no confinement involved. **A pass/fail ladder can only ever report that a grant was insufficient, never what was missing — so it cannot distinguish a real capability need from a bad network minute.** Check for staleness before treating any such count as a population.

## Route A — `process.memoryUsage()`, six packages

The runtime prints the whole chain:

```
Error: EACCES: permission denied, uv_resident_set_memory
    at process.memoryUsage (node:internal/process/per_thread:221:5)
    at ConsoleReporter.checkPeakMemory (/usr/lib/node_modules/yarn/lib/cli.js:33423:40)
    at ConsoleReporter.initPeakMemoryCounter (/usr/lib/node_modules/yarn/lib/cli.js:33414:10)
```

libuv's `uv_resident_set_memory` reads `/proc/self/stat` to get RSS. yarn v1 calls it from `initPeakMemoryCounter` on **every invocation, before any package work**, so any lifecycle script that shells out to yarn v1 dies identically regardless of what the package does. Confirmed by isolation: `process.memoryUsage()` opens `/proc/self/stat` once, while `process.cpuUsage()`, `os.cpus()`, `os.freemem()`, `os.loadavg()` and a bare `node -e 0` open it zero times.

Control, on `@nuxt/components@2.1.0`: the narrow arm shows 7 refusals and rc=1; the `write:"disk"` arm shows **zero** refusals and rc=0.

## Route B — glibc's `pthread_getattr_np()`, one package

`@opencode-ai/cli` ships a Bun binary that dies by `SIGABRT` during pre-main init, printing nothing — it aborts before its own panic printer is reachable, so no diagnostic is recoverable. The stack names the caller:

```
openat(AT_FDCWD, "/proc/self/maps", O_RDONLY|O_CLOEXEC) = -1 EACCES
 > libc.so.6(fopen64) > libc.so.6(pthread_getattr_np+0x266)
 > opencode2(...)  [under __libc_start_main → __libc_init_first]
```

glibc's `pthread_getattr_np()` parses `/proc/self/maps` to find the main thread's stack bounds. **This is a glibc dependency, not a Bun bug** — any glibc-linked runtime doing the same at startup has it. (The two musl candidates in the same postinstall fail `execve … ENOENT` for the unrelated reason that a musl loader is absent on a glibc host; that happens unjailed too and is not a denial.)

Isolated with a standalone Landlock harness that enforces a ruleset on itself and then `execve`s the target — enforce-then-exec is what makes `/proc/self/*` probing possible at all, since exec preserves the pid. Base grant is everything except `/proc`:

| extra grant | exit | reading |
| --- | --- | --- |
| none | 134 | aborts |
| `/proc` (whole) | 0 | superset control |
| **`/proc/self/maps`** | **0** | **sufficient alone, 3/3 runs** |
| `/proc/self/cgroup` | 134 | aborts |
| `/proc/sys/vm/mmap_min_addr` | 134 | aborts |
| cgroup + mmap_min_addr + version_signature | 134 | aborts — so `maps` is **necessary**, not merely sufficient |

`/sys` was granted in full in every arm, which exonerates all three sysfs candidates including the `O_WRONLY` write to `/sys/kernel/debug/tracing/trace_marker`.

⛔ **The "last refusal before the abort" heuristic was wrong here, and the error is instructive.** strace confirms the last refusal before `tgkill(SIGABRT)` is `/proc/self/cgroup`, 1.3 ms prior — but granting cgroup alone still aborts. Last-before-death is correlation. Only a single-variable arm settles which refusal is fatal, and only a negative control granting all the others proves it necessary.

**Not version-scoped.** The current `@opencode-ai/cli-linux-x64@1.18.14` behaves identically: unconfined 0, no-`/proc` 134, `+maps` 0. This will not age out.

No upstream issue describes it. The nearest, Bun [PR #28801](https://github.com/oven-sh/bun/pull/28801) (cgroup-aware `availableParallelism`), explains why `/proc/self/cgroup` is read at startup but is unrelated to the abort.

## Why no grant fixes it

A Landlock rule pins the **inode** resolved when the ruleset is built, and `/proc/self` is a kernel-resolved per-process symlink. So a rule naming `/proc/self/<x>` grants the *builder's* file, and every descendant is denied its own. Five expressible forms, all measured dead:

| attempted form | result |
| --- | --- |
| `/proc/self/stat` rule built pre-`fork` | child denied its own; reads the parent's fine |
| `/proc/<parent-pid>` directory rule | child still denied its own — **and it leaks the parent's `environ`** |
| `open("/proc/*/stat", O_PATH)` | `ENOENT` — the ABI has only `PATH_BENEATH` and `NET_PORT`, no glob |
| grant a not-yet-existing pid dir | `ENOENT` — cannot name a pid that does not exist yet |
| second `restrict_self` after `fork` | rule **adds successfully (rc=0)**, read still denied — domains only ever intersect |

The failing process is routinely a grandchild (postinstall → shell → bundled tool), so even building the ruleset after `fork` would cover only the direct child.

⇒ **"Each process may read its own `/proc/self/<x>`" is inexpressible in Landlock.** Only a whole-`/proc` grant reaches it, and that is refused on security grounds — see [`../design/build-jail-linux.md`](../design/build-jail-linux.md) for the four-direction `environ` gate showing a `/proc` subtree exposes sibling jailed scripts' environments.

## What this is not

`write:"disk"` does not repair these packages by granting a write. `preset::relax_fs_to_full_disk` **clears** the ruleset and flips the default effect to `Allow`, which incidentally makes `/proc` readable. Recording these packages as needing whole-disk write reads the symptom for the cause: not one of them writes anything unusual, and the narrow arm of `@nuxt/components` opens **zero sockets** — it dies before reaching the network at all.

## Changelog

- 2026-08-06 — Initial write-up. Census completed across all eight packages; Route B isolated single-variable; `iedriver@4.0.0` found stale.
