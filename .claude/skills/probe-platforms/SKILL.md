---
name: probe-platforms
description: Bring a test harness or probe up on a NEW operating system (Windows or Linux), or debug one that fails there. Invoke before the first run on a platform, and whenever a harness works on one OS but not another. Carries the bring-up ladder that avoids spending hours learning single booleans, the Windows spawn/path/disk faults that each present as something else entirely, the Linux Landlock and node-layout traps, and the remote-shell mechanics (PowerShell-over-SSH quoting, pkill matching your own command) that waste a loop each time they are rediscovered.
---

# Bringing a probe up on a new platform

A harness that works on one OS fails on another for reasons that are almost never the thing the
error names. Expect a CHAIN of faults, each hidden by the one in front of it.

## ⛔ Debug with the SMALLEST payload — the single most expensive mistake here

Measured, bringing the build-jail probe up on Windows: **six sequential faults, found using
`puppeteer` as the probe, at ~25 minutes per attempt.** Every one was answerable by a question as
small as *"does the binary spawn?"* — seconds with `is-odd`. About two hours bought six booleans.

**Climb this ladder in order. Do not skip to the end.**

| # | Step | Proves |
|---|---|---|
| 1 | `<binary> --version` | it exists, is executable, spawns |
| 2 | same, with the config/override env set | the feature is compiled in AND the override *engages* |
| 3 | install a trivial no-scripts package (`is-odd`) | fixture, store, linker |
| 4 | one tiny package that DOES run a script | a child process spawns under confinement |
| 5 | the real canary (large, real download) | end-to-end — a FINAL GATE, never a debugging instrument |

Each step is seconds. Each isolates one layer. **When one passes, expect the next to fail** — do not
read a green step as "the platform works."

**The top fault usually accuses the wrong subsystem.** On Windows the probe insisted the binary
lacked a cargo feature; the truth four layers down was an unspawnable path, with every per-cell log
at 0 bytes. When a diagnostic names a subsystem, verify that subsystem independently before acting
on the accusation — and when you fix a misleading diagnostic, it pays for itself immediately.

## Windows — all measured

- **`for /f` runs its command through `cmd /c`, which STRIPS the outer quote pair** when the string
  both starts and ends with a quote. So the natural spelling of a capture,
  `` for /f "usebackq delims=" %%i in (`"%EXE%" arg "%DIR%"`) ``, degrades to
  `C:\...\x.exe" arg "C:\...\dir` and dies with **"The filename, directory name, or volume label
  syntax is incorrect"** — with or without spaces in the path. Fix: wrap the whole command in one
  MORE quote pair (`` `""%EXE%" arg "%DIR%""` ``). Measured against a temp-file-redirect alternative,
  which did *not* work.
- **An undefined `%VAR%` expands to its own literal text**, so a missing variable is passed onward as
  the string `%VAR%` rather than being empty — silently handing a bogus path downstream. Guard with
  `if not defined VAR set "VAR=%CD%"`, under `setlocal` so the default does not leak to the caller.
- **A `.cmd` on `PATH` is invisible to whole classes of CI.** In this repo `native-deps.yml` is the
  only workflow touching node-gyp and it is ubuntu-only, so the Windows `.cmd` shim shipped broken and
  unnoticed for as long as another code path kept it from ever running. Green Windows jobs are not
  evidence for a Windows file nothing executes — check that some job actually runs it.
- **Node aborts at startup inside nub's build jail on Windows**: `Assertion failed:
  ncrypto::CSPRNG(nullptr, 0)`. Any jailed scenario whose script is `node` therefore cannot report on
  what it meant to test; skip it with the reason printed rather than recording a pass or a fail.

- **`spawnSync` cannot run `npm`/`npx`/`pnpm`.** They are `.cmd` shims: the bare name gives
  **ENOENT**, the `.cmd` spelling gives **EINVAL** (Node has refused to `CreateProcess` a batch file
  since CVE-2024-27980). `shell: true` works but is DEP0190 — args are concatenated rather than
  escaped, so cmd.exe re-parses a spec like `@scope/pkg@1.0.0`. **Run the bundled JS instead:
  `node <path>/node_modules/npm/bin/npm-cli.js`.**
- **Git Bash paths are not spawnable by native tools.** `cd $(dirname) && pwd` under Git Bash yields
  `/c/Users/…`, handed to CreateProcess verbatim → ENOENT. Convert to `C:\…`; for any path passed to
  a Windows binary use `cygpath -w`. `/tmp/x` is NOT where a Windows `node` will look.
- **A copied binary must keep its `.exe`.** Windows decides executability from the SUFFIX, so a
  content-addressed copy named by bare hash is ENOENT even at 1.2 GB and mode 0755.
- **Removing a `node_modules` tree fails with EPERM routinely** — a lifecycle child still holds a
  handle, or an indexer/AV opens files behind you. Use `maxRetries`/`retryDelay`, and **never let a
  cleanup failure discard a measurement that already succeeded** (a non-zero exit from tidy-up reads
  upstream as "no result").
- **Disk is a first-class hazard, and a full disk is SILENT AND EVIL.** A debug Rust binary can be
  ~1.2 GB with a `target/` of tens of GB; add leaked fixture trees and 80 GB vanishes. When it
  fills, `git fetch` fails and the probe runs **STALE CODE while you believe you are testing the
  fix**. Size 200 GB+, build `--profile fast` not `debug`, and check free space before trusting any
  run. Growing the GCE disk is not enough — the in-OS partition must be extended too, and
  `Resize-Partition` can simply fail.
- **Prefer a CI-built binary to provisioning MSVC.** A `windows-latest` workflow that uploads
  `target/debug/nub.exe` gives a real MSVC binary for one dispatch, versus installing the VS
  `VCTools` workload, `cmake`, and switching the rustup host triple by hand. A `windows-gnu`
  cross-compile is NOT a substitute when the thing under test is OS confinement behaviour.
- If you do build on the VM: rustup may default to `windows-gnu` (nub needs `-msvc`), VS Build Tools
  can be present with **no C++ workload** so there is no linker at all, and `/STACK:8388608` is an
  **MSVC-only** flag that breaks a GNU link — it is required under MSVC because Windows gives the
  main thread 1 MB against Linux's 8 MB.

### Windows filesystem speed — budget ~6x Linux, and beware the benchmark itself

MEASURED on matched GCE `e2-standard-8` / `pd-balanced` VMs, 3,000 small file creates:

| | time |
|---|---|
| Linux (bash redirect) | **181 ms** |
| Windows (`[System.IO.File]::WriteAllText`) | **1,142 ms** |
| Windows (PowerShell `Set-Content`) | 3,156 ms |

**~6x, not 17x.** The first Windows figure was `Set-Content`, whose cmdlet/pipeline overhead is
roughly two-thirds of it — comparing that against a bash redirect is not varying one thing. Use a
raw write on both sides before quoting any cross-OS I/O ratio.

⛔ **DO NOT EXTRAPOLATE THAT RATIO TO INSTALL TIME — I did, and it was wrong.** A microbenchmark of
raw file creation is not a workload. Measured with the SAME nub binary on matched VMs:

| install | Linux | Windows | ratio |
|---|---|---|---|
| trivial (`is-odd`) | 493 ms | 744 ms | **1.5x** |
| file-heavier (`typescript`) | 1,675 ms | 3,218 ms | **1.9x** |

So a real install is **~1.5–2x**, not 6x: installs are dominated by network and archive work, and
the file-create penalty is a minority of the total. I first wrote "budget an order of magnitude" here
off the microbenchmark alone; that was a wrong planning number in a durable doc. **Time the actual
workload before sizing anything.**

(Note the file COUNTS in that test are not comparable — `Get-ChildItem -Recurse` reported 9 where
`find -L` reported 264, because neither traverses junctions the same way. The TIMES are the
comparable part; see the junction-traversal trap above.)

## Linux — all measured

- **An AUTHORED Landlock grant naming a path that does not exist makes the whole policy
  uncompilable** — refused, not degraded (`PolicyNotExpressible`). Speculative grants skip a missing
  path; authored ones abort. One bad entry breaks every confined run.
- **Do not read "Landlock unavailable" as a kernel problem.** That error covers both an old kernel
  and *our own policy failing to compile*; it claimed a missing feature on a 6.17 kernel with ABI 4.
- **A fresh cloud box has no system `node`**, and the Rust build needs `cmake`. Provision Nodes with
  the tool under test where possible (dogfoods the real mechanism) — but its layout may be
  `<cache>/nub/node/22.23.1/bin`, with **no `v` prefix**, where nvm uses `v22.23.1`. Anything parsing
  a version out of a node path must accept both or it silently returns null.
- **Absent tooling is environmental, not a defect.** `pnpm` missing → a script shelling out to it
  exits 127; a `-musl` package on a glibc box cannot load. Neither is a bug in the thing under test.
- **Old Node pins bring old npm, and npm 6 races on its own `_cacache` under concurrency** —
  surfacing as `rimraf: missing path` + `Callback called more than once`. This manufactures FALSE
  defect verdicts that a double-control cannot catch, because both attempts sit in the same busy
  window. Re-verify any defect verdict serially once the batch drains.

## Remote-shell mechanics that cost a loop every time

- **SSH to Windows lands in PowerShell, not bash.** `bash.exe -lc "…"` must go through a `.ps1` or
  PowerShell parses `-lc` as an expression. Nested quoting through
  `ssh … powershell -Command "…"` breaks constantly — prefer a command containing **no double
  quotes at all**, or `scp` a script file (which fails on a full disk, so check that first).
- **Keep every PowerShell script ASCII.** One em-dash anywhere fails with `The string is missing the
  terminator`, pointing at the LAST line of the file rather than the offending one.
- **`pkill -f <pattern>` matches YOUR OWN command line.** Killing `-f "search.mjs"` from a shell
  whose command text contains `search.mjs` kills that shell — the remote command dies mid-way and
  returns no output, which reads like a hang. Split the kill into its own invocation, or break the
  literal (`"sea""rch.mjs"`).
- **Detach long remote runs with `tmux new-session -d`.** A bare `nohup … &` over SSH hangs the
  connection and can leave only the first job running.
- **Prefer native tools for bulk filesystem work.** `du -sh` over Git Bash on a large Windows tree
  is glacial and will time out; PowerShell equivalents finish.
