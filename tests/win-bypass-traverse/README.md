# AppContainer bypass-traverse probe

Settles one question with a real AppContainer launch: **does bypass-traverse let a LowBox child read deep into `%USERPROFILE%` when only a directory *beneath* the profile carries an ACE, with `C:\` and `C:\Users` left untouched?**

**Answer: yes.** Findings, numbers and run ids in [`results.md`](results.md).

## Why it needed measuring

nub's Windows build jail has two candidate unprivileged mechanisms, and this decides between them.

| mechanism | filesystem reads | egress lever |
| --- | --- | --- |
| restricted token + low integrity | work everywhere, no ACE written anywhere | **none unprivileged** — the network tier has to be a userland preload allowlist |
| AppContainer (LowBox) | need a per-path ACE — and `C:\` / `C:\Users` cannot be ACE'd unprivileged, which was assumed fatal and [is not](results.md) | **coarse deny for free** — withhold the `internetClient` capability |

The AppContainer route was believed dead because every prior measurement found `C:\` and `C:\Users` DENIED. Those measurements used `AccessCheck`, which evaluates **one** descriptor. A LowBox token retains `SeChangeNotifyPrivilege` enabled, and that privilege makes the object manager skip the access check on every **intermediate** path component — so a DENIED row on `C:\` establishes only that `lstat` / `readdir` / `chdir` **on `C:\` itself** fails, not that a deep open **through** it fails. `AccessCheck` cannot model bypass-traverse by construction. Only a real launch can.

If deep reads work, the route is alive: ACE the subtree the user owns and let bypass-traverse cover the two roots nub cannot touch, buying OS-enforced egress denial instead of a userland gate.

## Why CI and not the standing VM

An AppContainer cannot be launched over OpenSSH. sshd lands you in services session 0, which has no window station a LowBox token can attach to, and every launch returns `0xC0000142 STATUS_DLL_INIT_FAILED`. A *restricted* token is exempt from this; a LowBox token is not. So the venue is a branch-scoped workflow with no PR (`.claude/skills/ci-adhoc-test/SKILL.md`), on `windows-latest` and `windows-11-arm`.

## What it does

`probe.ps1` mirrors `crates/nub-sandbox/src/backend/windows.rs`'s `run()` step for step — `CreateAppContainerProfile` for a per-run AC SID, inheritable `GRANT_ACCESS` ACEs on the allowed leaves, then `CreateProcessW` with `EXTENDED_STARTUPINFO_PRESENT` and `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` at capability count **zero**, so `internetClient` is withheld and egress is denied by construction. It writes **no** ACE, label, or DACL of any kind on `C:\` or `C:\Users`; both are read with `icacls` and reported as facts, and otherwise left exactly as the image ships them.

Eight launches, each one variable from its neighbour:

| arm | AppContainer | grants | what it isolates |
| --- | --- | --- | --- |
| `plain` | no | none | **control** — identical child, identical paths, no `SECURITY_CAPABILITIES`. Must pass everything. |
| `ac-root-grant` | yes | one inheritable grant at a project root under `%USERPROFILE%` | the realistic shape |
| `ac-leaf-grants` | yes | leaf-only grants on `runtime` + `data` | the shipping model; the test root is ungranted too |
| `ac-data-ungranted` | yes | `runtime` only | **control** — the data grant withheld. The deep read must FAIL. |
| `ac-cwd-deep` | yes | leaf grants | launch-time cwd five components below the last grant |
| `ac-entry-deep` | yes | leaf grants | `node <deep file>` as the entry point, so `resolveMainPath`'s realpath runs before user code |
| `ac-noflags` | yes | leaf grants | **control** — the two realpath-skipping flags withheld. The defect must reproduce here, or the flagged arms prove nothing about the flags. |
| `ac-derive-only` | yes | leaf grants | the SID hash-derived with no profile registered, so the zero-setup answer is structural rather than a property of the teardown |

## The controls, and why a result without them is worthless

This effort has twice produced tables where every arm failed identically for a harness reason — six launch arms all failing `CreateFileW err=2` because a P/Invoke lacked `CharSet.Unicode`, and an `AccessCheck` sweep whose denials could not be told from a gate that denies unconditionally. So:

- **`plain`** must pass everything. A red cell there is a harness or host defect and invalidates the run.
- **gate-is-live** — every AppContainer arm must be DENIED on `C:\`. Granted would mean the token is not confined and every pass is vacuous.
- **positive control** — a confined arm must still read `C:\Windows\System32`, which carries an `ALL APPLICATION PACKAGES` ACE. Without it, a column of denials cannot be told from a child that fails at everything.
- **ace-absent** — the decisive deep read with the data grant withheld must FAIL, or the ACE is doing nothing.
- **ungranted-sibling** — a path under `%USERPROFILE%` that never got a grant must FAIL, or the grant scopes nothing.
- **egress differential** — denied in every AppContainer arm *and* permitted in `plain`. A deny with no matching allow is not evidence.

Any pre-existing `S-1-15-*` ACE reaching the test tree would make the ace-absent control pass for the wrong reason, so the probe detects and strips them from the test root (an ordinary owner operation inside the user's own profile — it touches no ancestor) and asserts none remain.

## `selftest.ps1` — the verdict is itself tested

The verdict block lives in `verdict.ps1` so `selftest.ps1` can drive it with **synthetic** cells across six worlds and require a different answer from each:

| world | what it models | required answer |
| --- | --- | --- |
| `works` | bypass-traverse works | every property passes |
| `denied` | bypass-traverse fails | the decisive properties fail, every control still passes — what a *clean negative* must look like |
| `harness-dead` | every arm fails, `plain` included | the baseline properties fail, so a broken harness can never be reported as a clean negative |
| `ace-inert` | everything passes everywhere | the controls fail, so a grant that scopes nothing can never be reported as a pass |
| `grant-never-landed` | the ACE never propagated into the deep file | the grant-reached control fails. Read-cell for read-cell identical to `denied`, which is the point — without that control a harness slip is indistinguishable from a real kernel denial |
| `defect-absent` | the no-flag arm ran fine | the flag differential fails, so the flags are never credited with fixing something that was not broken |

It needs no Windows APIs and runs anywhere:

```
pwsh -NoLogo -NonInteractive -File tests/win-bypass-traverse/selftest.ps1
```

CI runs it before the real launch.

## Reproducing

Push to the `sandbox/win-bypass-traverse` branch; the workflow is `push`-triggered on that branch, so no PR is needed (`gh workflow run` will not work until the file reaches the default branch). To re-run unchanged: `git commit --allow-empty -m rerun && git push`.

On a real Windows box in an **interactive** session (not over SSH — see above):

```
pwsh -NoLogo -NonInteractive -File tests\win-bypass-traverse\probe.ps1
```

Read the operations table and the `prop:` lines, not the exit status. A clean negative — controls green, bypass-traverse red — is a good outcome for the effort and still exits non-zero.
