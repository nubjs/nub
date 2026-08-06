# What granting `/proc` to a Landlock-confined process actually exposes

nub's Linux build jail grants a short allowlist of global `/proc` files and refuses the tree. The refusal is argued in `linux_landlock.rs`, above `PROC_READ_PATHS`, on the grounds that a grant on `/proc` "would expose every same-uid process's `/proc/<pid>/environ` and `cmdline` — the user's shell, editor, and other tools, environment variables included," and that this is "a strictly worse trade than the build breakage avoided."

**That premise is half wrong, and the wrong half is the severe one.** `environ` is not exposed. `cmdline` is.

## Why the question came up

`dotnet-2.0.0@1.4.4` is one of eight Linux packages whose measured minimum grant is `write:"disk"` — a rung that clears the rule set and flips the filesystem default to `Allow`, i.e. no confinement at all. Tracing resolved it to a single refused syscall: a bundled yarn, three processes down, calling `openat("/proc/self/stat")`, being refused, and exiting 1. It is `process.memoryUsage()` reaching libuv's `uv_resident_set_memory`. A positive control confirmed causation — adding `/proc` to the read floor with the grant otherwise unchanged produced a successful 151 MB install.

So the trade is not "`/proc` exposure versus nothing". It is "`/proc` exposure versus a confinement release that hands the same script `~/.ssh` and `~/.npmrc` to read and write". Getting the exposure right decides whether that class of package can be confined at all.

## Measurements

All on Ubuntu 24.04, kernel 6.17.0-1021-gcp, Landlock ABI v7, as an unprivileged user. Each probe carries an unconfined control and an ungranted-path negative control, so a result cannot be produced by a ruleset that silently failed to enforce.

### 1. A per-process `/proc/self` grant is inexpressible

A rule was added on the parent's resolved `/proc/self/stat`, then the ruleset was enforced and a child forked.

| reader | path | result |
| --- | --- | --- |
| parent | `/proc/self/stat` | OK |
| **child** | **`/proc/self/stat`** | **EACCES** |
| child | `/proc/<parent-pid>/stat` | OK |
| parent | `/etc/passwd` (ungranted) | EACCES — the ruleset bites |

The third row is what makes this a mechanism rather than an outcome: the child *can* read the parent's stat file by explicit pid, because that is the granted inode, and *cannot* read its own. A Landlock rule is added by passing an open fd, which pins the resolved inode at ruleset-build time, and `/proc/self` is a kernel-resolved per-process symlink. No fixed rule set covers an unbounded descendant tree, and the failing process in the motivating case is a grandchild.

This confirms the existing comment's last clause, and deepens it. Building the ruleset after `fork` would not help either — it would cover the direct child only.

### 2. `environ` is refused; `cmdline` is not

With `/proc` granted in full (`READ_FILE` + `READ_DIR`), a confined process reading an **out-of-domain** same-uid process:

| leaf | result | leaf | result |
| --- | --- | --- | --- |
| `environ` | **EACCES** | `stat` | OK |
| `maps` | EACCES | **`cmdline`** | **OK** |
| `smaps` | EACCES | `status` | OK |
| `cwd` | EACCES | `comm` | OK |
| `exe` | EACCES | `limits` | OK |
| `fdinfo` | EACCES | `mountinfo` | OK |
| `root` | EACCES | `sched`, `wchan` | OK |

Everything gated by `ptrace_may_access` is refused. `cmdline` is not gated and reads out in full.

### 3. The gate is Landlock, not yama — established by a discriminating test

The obvious explanation for row `environ` is yama's `ptrace_scope`, which was 1 on the box. It is not:

| arm | reader | `ptrace_scope` | out-of-domain `environ` |
| --- | --- | --- | --- |
| control | unconfined | 0 | **OK** |
| confined | Landlock domain | 0 | **EACCES** |
| confined | Landlock domain | 1 | EACCES |

One variable, opposite results. Landlock's own `hook_ptrace_access_check` denies `ptrace_may_access` unless the requester's domain is an ancestor of the target's, and `/proc/<pid>/environ` opens through exactly that check.

The prediction that distinguishes this hypothesis from every other was tested and held, at both `ptrace_scope` settings:

| target of the read | result |
| --- | --- |
| the confined process's own `environ` | OK |
| a process **inside** its Landlock domain (its own child) | OK |
| a process **outside** the domain | EACCES |

This is a property of the confinement itself, so it holds on any host regardless of the distribution's `ptrace_scope` default — which matters, because Debian and RHEL ship 0 where Ubuntu ships 1.

### 4. Withholding `READ_DIR` blocks pid enumeration at no cost

| `/proc` granted as | own `/proc/self/stat` | `/proc/stat` | `readdir(/proc)` | out-of-domain `cmdline` |
| --- | --- | --- | --- | --- |
| `READ_FILE` only | OK | OK | **EACCES** | OK |
| `READ_FILE` + `READ_DIR` | OK | OK | OK | OK |

The `READ_FILE`-only form fixes the motivating case and denies the script any way to enumerate which pids exist, so reaching another process's `cmdline` requires guessing a pid. It does not close that channel, only narrows the path to it.

## What this means for the design

The residual exposure of a `READ_FILE`-only `/proc` grant is: the command lines, scheduling counters and mount table of other same-uid processes, reachable only by guessing pids. Command lines are the part that can carry a secret, since a credential passed as an argv element is visible there — the same information `ps` shows to any same-uid process on an unconfined machine.

Weighed against it: eight Linux packages currently sit at `write:"disk"`, which grants read and write on the real `~/.ssh`, `~/.npmrc`, keychains and browser profiles, plus a shared `/tmp`. At least one of those eight is caused by nothing but the refused `/proc/self/stat` read.

Choosing between them is a product call about security posture, not a mechanism question, and the mechanism no longer forecloses it. The options are:

1. **Grant `/proc` with `READ_FILE` only.** Fixes the `/proc/self` class. Exposes out-of-domain `cmdline` to a pid-guessing script.
2. **Keep the enumerated list and extend it.** Cannot reach `/proc/self/*` at all — measurement 1 is definitive — so it does not fix this class, whatever is added to it.
3. **Leave the affected packages at `write:"disk"`.** Strictly the largest disclosure of the three.

## Changelog

- 2026-08-06 — Initial write-up. Four probes on kernel 6.17 / ABI v7, each with unconfined and ungranted-path controls, plus a discriminating test separating Landlock's ptrace hook from yama.
