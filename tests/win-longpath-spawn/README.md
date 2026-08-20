# Windows long-path spawn probe

Settles one question, empirically, on a real `windows-latest` runner:

**Can a bash step execute an `.exe` whose path exceeds `MAX_PATH` (260 characters)?**

## Why it exists

The release pre-publish gate deliberately compiles to a >260-character `--out` and then runs the artifact from that path (`.github/workflows/release.yml`, the `Windows long-path compile replaced an existing executable` assertion). Microsoft documents that `CreateProcessW` cannot launch an image past `MAX_PATH`, that the `\\?\` prefix does not lift it, and that `LongPathsEnabled` does not cover process creation. If that is universally true, the gate's own execution step cannot pass either — and the gate has never reached that line, so nobody has observed which way it goes.

The answer decides whether fixing `nub compile`'s self-probe is sufficient, or whether the gate's execution step needs the same treatment.

## What it runs

One job, no compilation. It copies the runner's `node.exe` to a >260-character path and tries to execute it several ways, printing the exit status of each:

| candidate | what it tells us |
| --- | --- |
| `direct` | the gate's current form — **the control**. Expected to fail if the premise is universal. |
| `verbatim` | the same path with a `\\?\` prefix, the fix people reach for first. |
| `shortname` | the 8.3 short name, when the volume still generates one. |
| `subst` | a mapped drive letter that shortens the path. |
| `copy` | a copy at a short path — the shape `nub compile`'s self-probe now uses. |

A losing candidate is a result, not a failure. The job exits non-zero only if the control unexpectedly PASSES (which would mean the reproduction is wrong and every other row is meaningless) or if no candidate works at all.

## Running it

Push to the `probe-win-longpath-spawn` branch; the workflow is scoped to that branch and needs no pull request. Results land in the job log under `=== RESULTS ===`.
