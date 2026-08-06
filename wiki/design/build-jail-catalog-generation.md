# Generating the build-jail catalog — how do we learn what a package needs?

The build jail grants each package a measured set of capabilities from a catalog. The sibling documents describe how that catalog is *enforced*. This one describes how it is *produced*, and it exists because the first answer was wrong in a way that took a long time to see.

## The verdict

**The measurement harness and the shipped jail are different systems with opposite constraints, and designing them as one is what made the first harness expensive and blind.**

| | generation harness | shipped build jail |
| --- | --- | --- |
| runs on | our machines and our CI | a stranger's laptop |
| privilege | root and full god-mode observability are expected | unprivileged, always, no exception |
| scripts assumed | well-behaved and non-malicious | hostile |
| job | *interrogate* what a script needs | *enforce* the narrowest grant that works |
| may use | ptrace, eBPF, ETW, dtrace, network namespaces, elevated APIs | only what an unprivileged process can do |
| output | exact paths, hosts and syscalls | a pass or a failure to launch |

Elevated observation produces ground truth; ground truth becomes catalog grants; the catalog feeds the unprivileged jail. Elevation lives entirely on the left of that arrow and never crosses it. Nothing an operator can set — no environment variable, no flag — may influence a grant at enforcement time, which is why the catalog is compiled in and why the override that makes measurement possible is a compile-time feature rather than a runtime one.

## What the first harness was, and why it was shaped that way

The original harness could observe exactly one thing: whether a jailed install passed or failed. From that single bit it had to recover a minimum capability set, so it searched — 55 states ordered by ascending cost, first pass wins, minimum by construction. The design is sound given the constraint, and it produced the corpus we have.

The constraint was self-imposed. Three consequences followed from it, and all three dissolve once the harness may watch:

- **Cost.** A search over 55 states runs the same install dozens of times: 13 minutes for one package, hours for a slice. One traced run answers the same question.
- **Silence about mechanism.** A failing rung says a grant was insufficient. It never says what was missing. Every "why does this package need the whole disk" question had to be re-investigated by hand, one package at a time.
- **A whole class of ambiguity.** A jailed script that exits 0 having written nothing is indistinguishable from success to a blind observer. To a trace it is a refusal with a path attached.

⛔ The corpus record fields do not close this gap and must not be read as though they do. `pathsBlockedWithoutGrant` and `pathsBlockedByPrefix` are both the delta against the **zero-grant** cell, not against the rung that mattered — they describe a *successful* install. That family was misread three separate times in one working session, once nearly landing a fix built on it.

## The v2 pipeline

**Observe → synthesize → verify → fall back.**

1. **Observe.** Run the package unjailed, under OS-level tracing, as an ordinary user. Read off the paths, hosts and syscalls it actually used.
2. **Synthesize.** Map the observed set to the narrowest grant covering it.
3. **Verify.** Run that grant in the real, unprivileged jail. This arm is the check, not the discovery.
4. **Fall back.** If the synthesized grant does not verify, walk the ladder *upward from it* — a handful of states rather than 55.

The ladder is retained deliberately. It is the test suite for the mapping: when the synthesized grant verifies, the mapping was right; when the ladder has to go wider, the observer missed something and the delta names it; when the ladder finds something narrower, the observer over-attributed. **Under-prediction and over-prediction rates are first-class metrics of the harness, not footnotes.**

The standing method that follows: to learn what a package needs, trace it — never infer the need from which jailed rung happens to pass.

## Observation is per-OS, and that is fine

Three adapters, one event contract. `strace` on Linux, ETW on Windows, `fs_usage` on macOS. They share nothing but their output shape:

```jsonc
{ "op": "read" | "write" | "connect" | "exec", "path": "/abs/path",
  "host": "1.2.3.4", "port": 443, "result": "ok" | "denied", "pid": 1234, "ppid": 1200 }
```

Everything downstream — normalization, scope classification, grant synthesis — is shared and identical. Three parsers is the right number; what has to be uniform is the mapping they feed, and it is a pure function tested against golden fixtures. Determinism rests on five rules, of which one is easy to violate and expensive: **a path that maps to no declared scope is an error, not a whole-disk grant.** Rounding an unclassifiable path up to `"disk"` is how a catalog silently inflates.

An OS-level boundary is also language-agnostic by construction, which is the property that settled a long detour. Two in-process seams were evaluated as network detectors and each is blind to real cases — an HTTP proxy misses a Node client that passes an explicit `agent`, and a `net.Socket` connect-hook misses every non-Node child, including the PowerShell and `curl` downloaders that are common in this corpus. The full comparison, which stands as a measurement even though its recommendation is superseded, is in [`../research/network-detection-proxy-vs-block.md`](../research/network-detection-proxy-vs-block.md).

## What this bought, concretely

`dotnet-2.0.0@1.4.4` sat at `write:"disk"` on Linux. Tracing resolved it to one syscall — a bundled yarn, three processes down, doing `openat("/proc/self/stat")`, being refused, and exiting 1. It is `process.memoryUsage()` reaching libuv's `uv_resident_set_memory`; the package writes nothing unusual and the whole-disk grant had nothing to do with writing. `write:"disk"` "repaired" it only because that rung flips the filesystem default to Allow and incidentally exposes `/proc`.

A positive control confirmed causation rather than correlation: adding `/proc` to the read floor with the grant otherwise unchanged made the install succeed and produced 151 MB of installed artifact. No amount of ladder-walking produces that sentence. One trace did.

## Two traps that survive the redesign

- **Elevation can silently change the answer.** On Windows an elevated token carries `SeBackupPrivilege`, and libuv sets `FILE_FLAG_BACKUP_SEMANTICS` on every file open, so **every** Node open bypasses the DACL. Measured one-variable: a write into a directory with an explicit Deny ACE succeeded as launched and was refused after the privilege was dropped. Untreated, a package that probes a location, is refused, and falls back elsewhere is observed taking the probe and never the fallback — yielding a grant both wider than needed and missing the real need. Every adapter must run the *target* unprivileged even when the *tracer* is not.
- **A refusal is only a refusal when the kernel says so.** On Linux `grep EACCES` matches the `AT_EACCESS` flag name in every `faccessat2` line; only `= -1 EACCES` is a denial. Raw counts of 26/13/1 were really 11/0/0. On Windows the refusal predicate is exactly four NTSTATUS values, and every other failure is omitted rather than called a denial — one fixture trace held 379 failed-but-not-refused operations against a single real refusal.

## Where the code lives

The harness is not in this repository. It lives in the corpus repository alongside the records it produces, under `harness/v2/`: `README.md` and `MAPPING.md` carry the pipeline contract and the five determinism rules, `measure.sh` is the driver, and `adapters/` holds the three per-OS observers with their known-answer fixtures and validation. The compile-time seam it drives is `build-jail-catalog-override` in `nub-sandbox`.

## Changelog

- 2026-08-06 — Initial write-up, recording the generation/enforcement split and the v2 pipeline that follows from it.
