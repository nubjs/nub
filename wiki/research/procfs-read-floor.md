# The procfs read floor for the Landlock build jail

What `/proc` and `/sys` paths ordinary Node and libuv code reads, which of them the build jail's Landlock allowlist currently denies, and what it costs to grant them.

All measurements are from a Landlock-enforcing host — Ubuntu, kernel 6.17, Landlock ABI 7 — against Node v22.23.2 and v18.20.8 (the support floor). Probes are a standalone C jail that grants the rest of the filesystem and varies only the procfs grant, so every result isolates one variable. Claims are labelled MEASURED or INFERRED throughout.

## The finding in one line

The jail's `PROC_READ_PATHS` allowlist breaks exactly one public Node API — `process.memoryUsage()` — and the security rationale recorded against widening it is measurably wrong.

## Which APIs the current allowlist breaks

Twenty-four public Node APIs were run under a jail granting exactly the eight paths in `PROC_READ_PATHS`. Only three probes failed, and all three are the same call:

| API | Result under the current allowlist | Path it needs |
| --- | --- | --- |
| `process.memoryUsage()` | **throws** `EACCES: permission denied, uv_resident_set_memory`, exit 1 | `/proc/self/stat` |
| `process.memoryUsage.rss()` | **throws**, exit 1 | `/proc/self/stat` |
| Worker startup, any child process reaching either | **throws**, exit 1 | `/proc/self/stat` |
| `os.cpus()`, `os.loadavg()`, `os.uptime()`, `os.freemem()`, `os.totalmem()` | exit 0 | already granted |
| `os.availableParallelism()`, `os.networkInterfaces()`, `os.userInfo()` | exit 0 | — |
| `process.resourceUsage()`, `process.cpuUsage()`, `process.uptime()` | exit 0 | — |
| `process.report.getReport()`, `v8.getHeapStatistics()` | exit 0 | — |
| `dns.lookup`, `fs.watch`, `child_process.spawnSync`, CommonJS `require` | exit 0 | — |

MEASURED, and identical on Node 18.20.8 and 22.23.2. The failure is a hard throw with no `/proc` in the message, which is why it surfaces as a bare exit 1 several processes away from the call.

The caller is libuv's `uv_resident_set_memory` (`deps/uv/src/unix/linux.c`), which slurps `/proc/self/stat` and has no fallback. Nothing else in the Node or libuv tree reaches a denied per-process procfs path on a fatal path.

### One lead that dissolved

On Node 18.20.8, `process.availableMemory()` also exits 1 — but the API does not exist before Node 20.13. Unjailed, `typeof process.availableMemory` is `undefined`. It is a `TypeError`, not a denial.

## Which denials are tolerated, and what they silently cost

Every jailed Node process already takes four refusals at startup and survives all of them. Two change a returned value:

| Denied path | Reached by | Consequence |
| --- | --- | --- |
| `/proc/self/cgroup` | `uv_get_constrained_memory`, `uv_get_available_memory` | `process.constrainedMemory()` and `process.availableMemory()` return **0** instead of the real figure |
| `/sys/fs/cgroup/**/memory.max`, `memory.high` | same | same |
| `/proc/self/maps` | V8 | no observed effect |
| `/proc/version_signature` | `uv__kernel_version` | no observed effect |

MEASURED by diffing returned values against an unconfined run. So the cgroup denials are quiet rather than harmless: inside a memory-limited container a jailed script sees `0` for both limit APIs and cannot tell that apart from "unlimited". No probe turned this into a failure, but a build script sizing a worker pool from `constrainedMemory()` would divide by it.

Separately, `/proc/version_signature`, `/proc/version`, `/proc/filesystems`, `/proc/mounts`, and `/proc/sys/kernel/pid_max` are all denied today. Each is global — no per-process content — so each is free to grant.

### The same startup set appears on every supported major

MEASURED under `strace -f -e trace=file` in per-version containers, each column a delta against that version's own bare `node -e 0` baseline:

| path | 20.20 | 22.23 | 24.18 | 26.6 | reached by |
| --- | --- | --- | --- | --- | --- |
| `/proc/self/cgroup` | ● | ● | ● | ● | startup |
| `/proc/self/exe` | ● | ● | ● | ● | startup |
| `/proc/self/maps` | ● | ● | ● | ● | startup |
| `/proc/meminfo` | ● | ● | ● | ● | startup |
| `/sys/fs/cgroup//memory.{high,max}` | ● | ● | ● | ● | startup |
| `/proc/version_signature` | | | ● | ● | startup |
| `/sys/devices/system/cpu/online` | | | | ● | startup |
| **`/proc/self/stat`** | ● | ● | ● | ● | **`process.memoryUsage()`** |
| `/proc/cpuinfo`, `/proc/stat`, `/sys/…/cpufreq/scaling_cur_freq` | ● | ● | ● | ● | `os.cpus()` |
| `/proc/loadavg` | ● | ● | ● | ● | `os.loadavg()` |
| `/proc/uptime` | ● | ● | ● | ● | `os.uptime()` |

Nothing beyond the baseline came from `process.resourceUsage()`, `constrainedMemory()`, `availableMemory()`, `os.totalmem()`, `os.freemem()`, `os.networkInterfaces()`, `os.userInfo()`, `v8.getHeapStatistics()`, `child_process.spawnSync`, `worker_threads`, a TCP listener, or a file read. Two newer majors add a startup path each, both global and therefore grantable.

**Three of the startup paths are per-process and no allowlist can name them** — yet the jail works today for 1759 of 1767 Linux packages, every one of which ran Node. That is what makes "tolerated versus fatal" the distinction that governs this question, rather than "touched versus untouched".

## Landlock cannot express a per-process grant

A Landlock rule is added by passing an open file descriptor, which pins the resolved inode at ruleset-build time. Because `/proc/self` is a kernel-resolved per-process symlink, a rule built on it names the *builder's* entry and nothing else.

MEASURED. A parent adds a rule on `/proc/self/stat`, enforces, then forks:

```
parent pid=48326
  [setup] rule added on /proc/self/stat  (resolved inode 57316)
  [setup] rule added on /proc/cpuinfo    (resolved inode 4026532019)
--- ruleset ENFORCED ---
PARENT (the process that BUILT the ruleset):
  parent   read /proc/self/stat    => OK      (inode 57316)
  parent   read /proc/48326/stat   => OK      (inode 57316)
  parent   read /proc/cpuinfo      => OK      (inode 4026532019)
  parent   read /proc/self/status  => DENIED  Permission denied (inode 57317)
CHILD (pid=48327, inherits the ruleset across fork):
  child    read /proc/self/stat    => DENIED  Permission denied (inode 59398)
  child    read /proc/48326/stat   => OK      (inode 57316)
  child    read /proc/cpuinfo      => OK      (inode 4026532019)
```

The child's `/proc/self/stat` is a different inode and is refused, while the same child can still read the parent's entry through its numeric path. The grant follows the inode, not the path string. `/proc/cpuinfo` passing in both processes is the positive control; `/proc/self/status` failing in the parent is the negative control that proves the ruleset is enforcing.

Three consequences:

- **No pattern grant exists.** The ABI offers two rule types, `LANDLOCK_RULE_PATH_BENEATH` and `LANDLOCK_RULE_NET_PORT`. There is no glob, so `/proc/*/stat` is not writable as a rule.
- **Every `/proc/<pid>` is a distinct inode**, so no fixed set of rules covers an unbounded descendant tree.
- **Building the ruleset after `fork` covers one process deep.** MEASURED: granting `/proc/self` before `execve` works for the exec'd process, because `execve` preserves the pid — but its own child fails. The motivating failure was a grandchild (`postinstall` → static loader → bundled yarn), so this shape would not have covered it.

Independently reproduced on a second host and kernel build, same result row for row — child refused its own `/proc/self/stat`, same child allowed the parent's through the numeric path, ungranted `/etc/passwd` refused as the negative control.

### Withholding `READ_DIR` blocks pid enumeration, and costs nothing

The grant's access mask is a second axis, separate from which paths it names. MEASURED, same host:

| `/proc` granted as | own `/proc/self/stat` | `/proc/stat` | `readdir(/proc)` | out-of-domain `cmdline` |
| --- | --- | --- | --- | --- |
| `READ_FILE` only | OK | OK | **EACCES** | OK |
| `READ_FILE` + `READ_DIR` | OK | OK | OK | OK |

Without `READ_DIR` a jailed script cannot list `/proc`, so reaching another process's `cmdline` requires guessing a pid rather than enumerating one. It narrows the path to that surface without closing it.

**Nothing in Node needs it.** MEASURED across Node 20.20, 22.23, 24.18 and 26.6: zero opens of `/proc` as a directory, at startup or under `process.memoryUsage()`. So the `READ_FILE`-only form is free.

## The security cost of granting `/proc` is smaller than recorded

The comment above `PROC_READ_PATHS` says a grant on `/proc` "would expose every same-uid process's `/proc/<pid>/environ` and `cmdline`". The first half is **MEASURED false**.

Against a same-uid process that is not a descendant of the jailed reader:

| Per-process file | Unconfined | Current allowlist | Whole `/proc` granted |
| --- | --- | --- | --- |
| `environ` | OK | denied | **denied** |
| `maps` | OK | denied | **denied** |
| `io` | OK | denied | **denied** |
| `cmdline` | OK | denied | OK |
| `status`, `cgroup`, `limits`, `mountinfo`, `comm`, `sched`, `wchan` | OK | denied | OK |

The three-way control is what makes this attributable. Under one identical grant, `cmdline` succeeds while `environ` fails, so the filesystem rule cannot be the discriminator; and unconfined, `environ` succeeds, so confinement is what flips it.

The mechanism is Landlock's own ptrace hook. Opening `environ`, `maps`, or `io` requires `PTRACE_MODE_READ`, which passes through the LSM `ptrace_access_check` hook; Landlock's implementation in `security/landlock/task.c` returns `-EPERM` unless the target is inside the reader's own domain hierarchy. The kernel documentation states this directly:

> However, thanks to the ptrace restrictions, access to such sensitive `/proc` files are automatically restricted according to domain hierarchies.

Two caveats. The restriction is described as implicit and is not gated on an ABI level, but it was measured only on kernel 6.17 — INFERRED for older kernels down to Landlock's 5.13 floor. And it protects only against processes *outside* the domain: a jailed script can read its own descendants' `environ` (MEASURED), which is expected, since it supplied that environment.

**It is not yama, and that was worth checking** — yama's `ptrace_scope` is the obvious alternative explanation, and it would have made the protection depend on a distribution default rather than on the confinement. It does not. MEASURED, varying only `ptrace_scope`:

| arm | reader | `ptrace_scope` | out-of-domain `environ` |
| --- | --- | --- | --- |
| control | **unconfined** | 0 | **OK** |
| confined | Landlock domain | 0 | **EACCES** |
| confined | Landlock domain | 1 | EACCES |

The unconfined row at `ptrace_scope=0` is the one that settles it: with yama permissive, an unconfined reader succeeds where a confined one is refused, so the confinement is the discriminator. The domain-hierarchy prediction was then tested directly and held at both settings — a confined reader reads the `environ` of a process inside its own domain and is refused one outside it. This matters in practice because Debian and RHEL ship `ptrace_scope=0` where Ubuntu ships 1; the protection holds on all of them.

What a whole-`/proc` grant does newly expose is `cmdline` and the status-shaped files of other same-uid processes. Command lines are already world-readable on a stock Linux — mode 0444 with no ptrace gate, visible to every user on the box — so this is surface the jail adds back rather than a same-uid-only secret channel. Secrets passed as command-line arguments are the real content at risk.

## Options

| Option | Removes the failure class? | Cost |
| --- | --- | --- |
| Add `/proc` to the read floor, **`READ_FILE` without `READ_DIR`** | Yes, at any descendant depth | Same as the row below, except the script cannot enumerate pids and must guess one. Free — no Node version needs `readdir(/proc)` |
| Add `/proc` to the read floor with `READ_DIR` too | Yes, at any descendant depth | Exposes other same-uid processes' `cmdline`, `status`, `cgroup`, `limits`, `mountinfo`, `comm`; `environ`, `maps`, `io` stay protected |
| Add the safe global files only (`/proc/version_signature`, `/proc/version`, `/proc/mounts`, `/proc/filesystems`, `/proc/sys/kernel/pid_max`) | No | None — all are global, no per-process content |
| Grant `/proc/self` after `fork`, before `exec` | Only one process deep | Does not cover the observed grandchild case |
| Pattern-match `/proc/*/stat` | — | Not expressible; Landlock has no glob rule type |
| Leave as-is | No | Any install script transitively calling `process.memoryUsage()` fails with a bare exit 1, several processes from the call |

Widening the floor to `/proc` is the only option that removes the class, and the safe-globals addition is free regardless of that decision. Both are security-posture calls.

## Reproducing

The probes are standalone C plus shell, needing only `gcc` and `strace` on a Landlock-capable kernel:

- A fork probe that adds a rule on `/proc/self/stat` and compares parent against child, with `/proc/cpuinfo` as a positive control and `/proc/self/status` as a negative one.
- A jail wrapper taking a mode — the current allowlist, the whole tree, or `/proc/self` — that grants the rest of the filesystem and execs a command, so the procfs grant is the only variable.
- An API matrix that runs each Node call under each mode and records exit code and stderr.

Two probe bugs are worth avoiding: a rule on a *file* must carry `LANDLOCK_ACCESS_FS_READ_FILE` alone, since a directory-only right returns `EINVAL` from `landlock_add_rule`; and `stdout` must be flushed before `fork`, or the child re-emits the parent's buffer and the output reads like a denial.

Two traps apply to reading the traces.

- Searching an `strace` log for `EACCES` matches the `AT_EACCESS` flag name in every `faccessat2` line. Only `= -1 EACCES` is a refusal.
- ⛔ **`strace` under cross-architecture emulation produces a trace containing no syscalls.** An attempt to gather the version table above inside Docker's amd64-on-arm64 emulation returned 430 trace lines with zero `openat` calls, which reads exactly like "Node touches no `/proc` paths at all". The known-answer control is what caught it: `process.memoryUsage()` must add `/proc/self/stat`, and it came back zero. Trace on the host's native architecture, and require `grep -c openat` to be non-zero before reading anything off a trace.

Landlock probes additionally need a Landlock-capable kernel, which Docker Desktop's `linuxkit` kernel is not — `landlock_create_ruleset` returns `ENOSYS` there. The probe's own ABI check reports that rather than producing a false negative; keep that check in any probe built from these.

## Changelog

- 2026-08-06 — Merged in a second, independent investigation of the same question: the `READ_DIR` axis and its zero cost to Node, the Node 20/24/26 startup sweep alongside the original 18/22, the yama control establishing that the `environ` protection is Landlock's rather than `ptrace_scope`'s, and the emulation trap. Its separate write-up is superseded and removed. Both investigations reached the same conclusion about the recorded security rationale being wrong, independently.
- 2026-08-06 — Initial write-up.
