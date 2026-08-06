# Tracing a process tree on macOS: what each instrument can and cannot see

Notes on the two root-only macOS tracing instruments, written after both produced confidently wrong answers. Everything here is labelled by how it was established: **MEASURED** (we ran it), **SOURCE** (read out of published source), or **REPORTED** (measured by a collaborator on CI, not re-run locally).

The task throughout was: watch a short-lived `/bin/sh -c` and its descendants, and attribute every file and network event to the process that performed it.

## `eslogger` suppresses its own process group — the single biggest trap

From `man eslogger`, verbatim (**MEASURED**, read on macOS 26.5.2):

> To avoid feedback loops when filtering output using shell pipelines, eslogger automatically suppresses events for all processes that are part of its process group.

This is documented, deliberate, and it silently destroys the obvious test harness. A shell script that launches `eslogger` **and** the processes it means to observe puts them all in one process group, because **a non-interactive shell has no job control**, and a pgid is inherited across `fork` + `exec`. Every observed process is then discarded by design.

MEASURED on macOS 26.5.2, from a `#!/bin/bash` script:

| launched from the script | pgid |
| --- | --- |
| the script itself | 11838 |
| background job, no job control | **11838** — same, suppressed |
| background job after `set -m` | **11931** — its own group |

⛔ **`setsid(1)` does not exist on macOS** (MEASURED — it is util-linux, Linux-only), so the reflexive fix is unavailable. Working alternatives, all present on a stock system: **`set -m`** in the shell, `perl -e 'use POSIX; POSIX::setsid(); exec @ARGV' -- <cmd>`, `python3 -c 'import os,sys; os.setsid(); os.execvp(sys.argv[1], sys.argv[1:])' <cmd>`, or in Node `spawn(..., {detached: true})`.

**How this misleads, concretely.** With the tracers and the observed processes sharing a group, `eslogger` still emits a large stream — tens of thousands of records — because Apple's launchd daemons are in *other* process groups and remain visible. A count-only check therefore looks like healthy coverage. The tell only appears when you print the **reporting** pid: in one run, every record naming the fixture's file came from `mds` and `mdworker_shared`, i.e. **Spotlight indexing the newly written file**, and not one came from the process that wrote it (REPORTED).

⇒ **Any coverage claim about `eslogger` must assert the tracer's pgid differs from the observed processes', and must attribute by `audit_token`, not by counting records that mention a path.**

## `eslogger` also requires TCC Full Disk Access

MEASURED, on a Mac whose terminal had not been granted it — all output empty, this on stderr:

```
Failed to create ES client: Not permitted to create an ES Client, responsible process
needs TCC Full Disk Access authorization (ES_NEW_CLIENT_RESULT_ERR_NOT_PERMITTED)
```

Root is necessary but not sufficient: the **responsible process** (the terminal application hosting the shell) needs Full Disk Access. This is independent of SIP, and independent of the process-group issue above — the two produce completely different failures, which is why a result from one machine does not generalize to another.

## `dtrace` needs SIP off, and "SIP off" is narrower than it looks

`dtrace` is refused with SIP enabled. Its gate is a single CSR bit — `dtrace_is_restricted()` checks `CSR_ALLOW_UNRESTRICTED_DTRACE` (SOURCE: `bsd/dev/dtrace/dtrace_subr.c`).

**SOURCE**, verified against `bsd/sys/csr.h` in the published XNU tree:

```c
#define CSR_ALLOW_UNRESTRICTED_DTRACE           (1 << 5)
#define CSR_ALLOW_UNAUTHENTICATED_ROOT          (1 << 11)

/* Flags set by `csrutil disable`. */
#define CSR_DISABLE_FLAGS (CSR_ALLOW_UNTRUSTED_KEXTS | \
                           CSR_ALLOW_UNRESTRICTED_FS | \
                           CSR_ALLOW_TASK_FOR_PID | \
                           CSR_ALLOW_KERNEL_DEBUGGER | \
                           CSR_ALLOW_APPLE_INTERNAL | \
                           CSR_ALLOW_UNRESTRICTED_DTRACE | \
                           CSR_ALLOW_UNRESTRICTED_NVRAM)
```

So `csrutil disable` sets bits 0–6 — **`0x7f`** — and **deliberately excludes** `CSR_ALLOW_UNAUTHENTICATED_ROOT`. dtrace's bit is inside that set; unsealing the system volume is a separate action (`csrutil authenticated-root disable`).

⛔ **The practical consequence is a misreading worth knowing about.** A *fully successful* `csrutil disable` leaves `/` read-only, on every Mac, VM or metal. Reports of "SIP disable only partially worked in a VM — status says disabled but the root filesystem is still read-only" describe the expected outcome of a complete disable, not a failure. A tool reporting `sip0: 7f` is reporting success. If dtrace is what you need, `csrutil status` is the wrong thing to check — **run dtrace**.

Narrower alternative worth knowing: `csrutil enable --without dtrace` grants that one bit without a blanket disable.

## What each instrument structurally cannot give you

| | `eslogger` (Endpoint Security) | `fs_usage` | `dtrace` |
| --- | --- | --- | --- |
| pid / ppid | ✅ the only source that carries one | ❌ prints `command.threadid`, never a pid | ✅ |
| open flags (read vs write intent) | ✅ | partial | ✅ |
| **refusal errno** | ❌ NOTIFY events fire on operations that were ALLOWED | ✅ numeric | ✅ |
| **TCP peer address** | ❌ no TCP event exists (`uipc_connect` is UNIX-domain only) | ❌ formats connect as fd + errno, never the sockaddr | ✅ |
| gate | root + TCC Full Disk Access | root | root + SIP off |

⇒ Neither ES nor `fs_usage` alone satisfies "attribute every event to a pid **and** report refusals **and** report peers". dtrace does, at the cost of requiring SIP off.

## Two dtrace decoding traps (REPORTED, found by known-answer fixture)

- **A non-blocking `connect()` returns `EINPROGRESS` (36) on every arm**, including one aimed at a closed port. A connect refusal is not readable from the `connect` return — it needs the later `getsockopt`. Do not classify `EINPROGRESS` as denied.
- **Decode the address family before the address.** An `AF_LOCAL` sockaddr decoded as IPv4 yields a plausible-looking dotted quad that is really the socket path's characters read as octets. Gate on `af == 2`. Similarly, shifting a port out of a `uint8_t`-width operand truncates the high byte — `:443` decodes as `187`.

## `/bin/sh` is a stub that re-execs `/bin/bash`, and `dtrace -c` does not survive it

**MEASURED.** macOS `/bin/sh` is a 101 KB binary that immediately re-execs the real 1.29 MB `/bin/bash` — two different files reporting the same `3.2.57` version string. `dtrace -c <target>` cannot follow its target through that re-exec: the child dies without running its body, **dtrace still exits 0**, and any sentinel the wrapper was supposed to write never appears.

The symptom is maximally misleading. The tracer is alive, the D script compiled, the exit status is clean, and the wrapper's own `sh -x` narration is simply *absent* — so every plausible story (a SIP-restricted binary, `sudo` inside a traced child, dtrace killing its child, a bad D script) fits the evidence equally well. All four are wrong.

The tell is in the trace itself: an `EXEC` record at the **target's own pid** whose execname is `bash`. `/bin/bash` invoked directly produces no such record.

A one-variable matrix settles it and exonerates everything else (each row differs from its neighbour in exactly one thing):

| arm | wrapper sentinel |
| --- | --- |
| no dtrace, `/bin/sh -x`, no sudo | **present** — positive control |
| no dtrace, `/bin/sh -x` + sudo | **present** — `sudo` is fine unjailed |
| `dtrace -c /usr/bin/true` | fires — `-c` itself works |
| `dtrace -c "/bin/sh w.sh"` (no `-x`) | absent — **`-x` exonerated** |
| `dtrace -c "/bin/sh -x w.sh"`, no sudo | absent — **`sudo` exonerated** |
| `dtrace -c "/bin/sh -x w.sh"` + sudo | absent, 0/8 repeats |
| **unsigned `cp /bin/sh`** + sudo | absent — **code signature and SIP exonerated** |
| **`dtrace -c "/bin/bash -x w.sh"`** + sudo | **present**, full exec tree |
| the real adapter, `/bin/sh` | absent, 0/8 — **the D script exonerated** |

⇒ **Name `/bin/bash` explicitly for any `dtrace -c` target.** Intermittent partial output — a line or two of narration on some runs — is the race between the re-exec'd shell making progress and dtrace tearing the child down, not tracer flakiness; do not chase it as nondeterminism.

⛔ **The same fact breaks process ATTRIBUTION, one layer up.** A decoder that identifies a lifecycle script as "the only `sh -c` in the subtree" fails on macOS for this reason: npm's `sh -c` re-execs too, so its `EXEC` record also carries execname `bash`. Match on `psargs` rather than the binary name.

## `copyinstr` at syscall ENTRY drops records silently

**MEASURED, and the damage is total rather than partial.** `copyinstr()` runs in probe context and cannot take a page fault, so a path string not yet resident at syscall entry does not merely produce the visible `dtrace: error on enabled probe … invalid address … at DIF offset 12` — it **aborts the whole clause**. Any `self->` variable the clause was going to set stays unset, the matching `:return` probe therefore never fires, and the event vanishes with no further diagnostic.

Known-answer fixture: 200 files whose path strings each sit in their own untouched, file-backed page, so non-residency at entry is deterministic rather than hoped-for. Scored by NAME against the known set, never by count.

| adapter | known paths seen | faults |
| --- | --- | --- |
| `copyinstr(arg0)` at `:entry` | **0 / 200** | 200 |
| pointer saved at `:entry`, `copyinstr` at `:return` | **200 / 200** | 0 |

The old form reported **none** of 200 opens the fixture provably performed. For a capability-measurement harness that is a silent under-report of everything, which is the one direction that matters.

⇒ **Save the POINTER at entry and `copyinstr` it at `:return`**, when the kernel has already faulted the string in. This is what Apple's shipped `/usr/bin/opensnoop` does — `self->pathp = arg0` at entry, `copyinstr(self->pathp)` at return, commented *"checked on return to ensure pathp is mapped"* — and what the DTrace guide's "Avoiding Errors" prescribes.

## Method note

Every wrong answer in this area came from a check that could not fail: counting records rather than attributing them, polling a tracer whose file-backed stdout had not flushed, or reading `csrutil status` instead of running the tool whose behaviour was in question. **Assert the instrument is working with a positive control that names a known process, and require the control to go red when the fix is absent.**

Both findings above were reached the same way and neither was reachable by reasoning: a matrix varying ONE thing per row, and a fixture whose correct answer was known in advance. In both cases the most plausible hypothesis — SIP for the first, "a few dropped records" for the second — was wrong, and wrong in a way no amount of further theorising would have exposed.

## Changelog

- 2026-08-06 — Two mechanisms added, both measured while unblocking a dtrace-based measurement harness: `/bin/sh` is a stub that re-execs `/bin/bash` and `dtrace -c` cannot follow it (also the reason `sh -c` process attribution fails on macOS), and `copyinstr` at syscall entry aborts its whole clause on a page fault, dropping records silently — 0 of 200 known opens reported.
- 2026-08-06 — Initial write-up.
