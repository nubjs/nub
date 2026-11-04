---
name: corpus-sweep
description: Run a large sharded measurement sweep over npm packages (the build-jail catalog probe, or any harness that installs thousands of package-versions and records a verdict per run). Invoke BEFORE launching a sweep, before believing any standalone reproduction of a sweep failure, and before reporting that a failure is or is not a real defect. Carries the self-deception patterns that each cost hours: an install exiting 0 while the build was silently skipped, `find` not following store symlinks, a restricted PATH swapping the toolchain out from under a probe, sequential arms sharing a warm store, and a batch query whose zero hits were never validated against a known positive. Also the shard mechanics, the disk and OS-indexing overheads, artifact cleanup, and the rule that a malicious package must never be executed.
---

# Running a corpus sweep

A sweep installs thousands of third-party package-versions and records a verdict for each. The
verdicts feed something real — for the build jail, the capability catalog that decides what actual
user installs are permitted. So a wrong verdict is not a flaky test, it is a shipped defect.

**The single most important thing in this skill: nearly every wrong conclusion comes from a
STANDALONE REPRODUCTION THAT SILENTLY TESTED NOTHING.** The harness says a package fails. You run it
by hand, it passes, and you conclude the harness is wrong. It is almost always the reverse.

## ⛔ Before you believe any standalone reproduction

Run this checklist. Each line is a measured failure that produced a confident wrong answer.

| Check | Why |
| --- | --- |
| **Did the script actually RUN?** | `nub install` exits **0** while the trust policy skips the build entirely — `WARN ignored build scripts for N package(s)`. You must run `nub approve-builds --all` after the install. An exit code proves nothing about whether a lifecycle script executed. |
| **Did you look for the artifact with `find -L`?** | Under the isolated linker every `node_modules` entry is a SYMLINK into the global store. Plain `find` does not follow symlinks and reports **zero** addons where `ls` shows them present. |
| **What is on PATH?** | Restricting `PATH` to `/usr/bin` silently hands node-gyp Xcode's Python 3.9 instead of the host's 3.14 — so the Python-dependent failure you are chasing cannot reproduce. Print the resolved `python3`, `node`, and `node-gyp` versions in the probe output, not just the exit code. |
| **Is each arm getting a FRESH home AND a fresh store?** | Sequential arms against a shared store make the second one warm. That confounds every A/B, and it is how four consecutive wrong answers about one package were reached. |
| **Are you varying exactly ONE thing?** | A separate tool (`classify-broken.sh`) differing from the harness in fixture, env scrubbing, pinned Node/Python AND jail state is not a control for concurrency. |

**A clean result where you expected a messy one is a signal to distrust the instrument, not to
conclude.** Three passes in a row after a real failure means you probably are not running the thing
you think you are running.

## ⛔ Validate any query or filter against a KNOWN POSITIVE

A screen that returns zero hits has two explanations and you cannot tell them apart without a
control. Measured: an OSV screen over 2,250 entries returned 0 MAL-* hits. That happened to be
correct — but the only way to know was to re-run it against a package known to be flagged
(`@ctrl/tinycolor@4.1.2` → `MAL-2025-47141`) and confirm the instrument fires. Do this for every
batch API call, grep filter, and classifier before reading a conclusion off it.

Batch APIs deserve a second control: put the known positive in the MIDDLE of a full-size batch and
check it is still found at the right index. Silent truncation is real.

## ⛔ Malicious packages: never execute, never catalog

A package with a `MAL-*` advisory must never have its scripts run, on this machine or any machine.

- The PM under test may refuse it correctly — but the **reference arms (npm, pnpm) have no OSV
  screen** and typically run with `--dangerously-allow-all-scripts`. If a refusal verdict is
  computed *after* the oracle arms, the harness executes the malicious script itself.
- So: detect the refusal and **return before any oracle arm runs.** Verify the ordering in code,
  not by assumption — the guarantee must not be incidental to line order.
- Better still, screen the WORKLIST against OSV before the sweep starts and drop hits entirely, so
  the tarball is never even fetched.
- A refusal is its OWN verdict. Scoring it as a defect of the PM under test blames the tool for
  working correctly, and lands the package in the wrong bucket of the final report.

## Sharding

Round-robin the worklist so each shard gets a mix of heavy head and cheap tail:

```sh
python3 -c "
lines=[l.strip() for l in open('worklist.txt') if l.strip()]
for i in range(3):
    open(f'shard{i}.txt','w').write('\n'.join(lines[i::3])+'\n')"
```

Then launch each shard as its OWN harness-tracked background task — never `&`, which the harness
rejects because a detached job cannot report completion.

- **Check fixture isolation first.** Concurrent shards are only safe if each process roots its
  fixtures uniquely (`fs.mkdtempSync`). Verify before launching, not after.
- 3 shards took throughput from ~0.7 to ~2 runs/min on a 10-core box. More is not obviously better:
  contention makes packages whose scripts run their OWN installer fail transiently.
- **Records must be per-package files** so shards never write the same path.

## The host will fight you

- **Spotlight and Time Machine index the churn.** Each cell installs a full dependency tree into a
  fresh `$HOME`, so a sweep generates millions of file events. Measured: `fseventsd` 60%,
  `backupd` 30%, `mds` + `spotlightknowledged` 47% — about 1.7 cores of pure overhead. Fix with
  `touch <cache>/.metadata_never_index` and `tmutil addexclusion <cache>`, both idempotent.
- **High load with slow progress is not always your processes.** Check `ps -Ao pid,%cpu,comm -r`
  before assuming contention; the answer may be an OS daemon.
- **Fixture roots leak on SIGKILL.** A harness that cleans up on normal exit does not clean up when
  you `pkill` it — and stop-fix-restart is the normal loop when triaging. Measured: 31 orphans,
  18.9 GB, on a disk already at 98%. Sweep anything untouched for >30 min at startup; a live shard
  touches its root continuously so the threshold cannot catch one in use.
- **Wall-clock numbers taken during a sweep are untrustworthy.** Use load-independent evidence.

## Clean up when you are done

A sweep and its debugging leave several distinct piles. Delete all of them:

- the harness's own fixture roots (`mkdtemp` dirs under the cache);
- any ad-hoc reproduction trees you created while triaging — these are the biggest surprise,
  measured at **12 GB** from one night of hand-testing;
- temp catalogs and worklists in `/tmp`;
- per-cell logs for records you have finished analyzing, if the verdict is recorded.

Keep the `results.json` records — they are small (~39 KB each, ~0.1 GB for a 2,250 corpus) and they
are the actual output.

## Stopping to fix the harness

Every harness fix invalidates the records taken before it, because a record means something
different under a changed instrument. That is what provenance hashes exist to expose.

- **Purge exactly the records the fix could change**, not all of them, when you can characterise the
  set (e.g. "only runs pinned to Node ≤15"). Purge everything when the change is corpus-wide.
- **Never edit the harness while a batch runs.** The driver script is re-read on every package
  spawn; editing it mid-run silently corrupted 54 of 100 packages once. Stop, fix, restart.
- A binary rebuild mid-run is safe IF the runner snapshots the binary at startup. Confirm it does.
- Weigh the trade honestly: a correct instrument with partial coverage beats a complete corpus
  measured by an instrument you already know is wrong. But say the coverage number plainly.

## Bringing the harness up on a NEW PLATFORM

**Load the `probe-platforms` skill.** It owns the bring-up ladder (debug with the SMALLEST payload —
six Windows faults were found using a 25-minute package when seconds would have done), the Windows
spawn/path/disk faults, the Linux Landlock and node-layout traps, and the remote-shell mechanics.

Two rules from it that bite during a sweep specifically:

- **Shuffle the worklist before sharding.** It is name-sorted, so a contiguous slice hands every
  shard the same heavy family at once — four shards once sat 12 minutes on one family and produced
  zero records. A seeded shuffle fixed it in seconds.
- **Confirm the override actually ENGAGED before believing any cell.** The catalog parser rejects a
  malformed catalog (e.g. `read` alongside `write: "disk"`, which is redundant) and falls back to the
  compiled-in one *silently* — so a run you believe grants network may have none.

## Reading results

- **Coverage first, intersected with the worklist.** Counting every record on disk inflates it with
  earlier runs. Packages with NO record mean the recorded set is a biased sample — the heavy native
  builds are exactly the ones that fail, so survivors are not the corpus.
- **Read the FIRST error, never the tail.** These logs end in a stack trace and a summary; the cause
  is ~40 lines earlier.
- **Group by CAUSE before concluding.** A cluster of similar-looking strings is not evidence of a
  shared cause; clustering by an error substring once merged two unrelated bugs.
- **Frequency tables must count DISTINCT PACKAGES, not records.** One package measured at four
  versions looks like ecosystem-wide leakage otherwise.
