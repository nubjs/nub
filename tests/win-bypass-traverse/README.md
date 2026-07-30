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

## The second question: `\Device\Null`, the pipe namespaces, and the piped-spawn hang

Bypass-traverse cleared the filesystem blocker; two Node/libuv interaction blockers remain, and one of them is the **piped-spawn hang** — a confined `child_process` spawn with piped stdio never returns, which every npm lifecycle script would hit. Five further arms (`obj-*`) settle where it comes from and whether an unprivileged repair exists.

A specific candidate motivated them. Microsoft's own mxc documents `\Device\Null` as a hard blocker for its AppContainer backends — the kernel resets that descriptor at every boot and the default names no AppContainer trustee, so a LowBox child opening `NUL` for stdio redirection gets `ERROR_ACCESS_DENIED` partway through startup. mxc's remedy (`prepare-null-device`) runs **elevated, once per boot**, which is disqualifying for nub; Codex performs the same repair **unprivileged and best-effort** (`windows-sandbox-rs/src/acl.rs`, `allow_null_device`). The two disagree about the privilege it takes, and that disagreement is measurable.

The same shape applies to a second device. Every `\\.\pipe\…` `CreateNamedPipeW` is refused to a LowBox token while `\\.\pipe\LOCAL\…` succeeds, and libuv's stdio path spells the global form — so if the NPFS root's DACL is unprivileged-writable, the hang is fixable with no libuv change at all. Both devices get the same three cells: read the descriptor, try to obtain `WRITE_DAC` (elevated **and** de-elevated), then launch with the repair applied and withheld.

| arm | AppContainer | device treatment | what it isolates |
| --- | --- | --- | --- |
| `obj-plain` | no | none | **control** — every object cell must pass, or a confined failure is the harness |
| `obj-ac-baseline` | yes | as shipped | the blocker in its as-is state |
| `obj-ac-nulfix` | yes | `\Device\Null` granted to this arm's SID | one variable |
| `obj-ac-npfsfix` | yes | NPFS root granted to this arm's SID | one variable |
| `obj-ac-baseline-again` | yes | as shipped, run **last** | a device DACL is machine-global, so a revoke that silently failed would make a later arm read as repaired |

Two design points worth not rediscovering. **The privilege answer is measured de-elevated, not just elevated**: a GitHub runner is `runneradmin` and elevated, so an elevated-only success would say nothing about nub's shipping case — the probe impersonates a restricted token (Administrators deny-only, every removable privilege dropped, Medium integrity) and requests `READ_CONTROL` and `WRITE_DAC` from that same context, so a `WRITE_DAC` refusal cannot be confused with an impersonation that never took effect. And **a hanging arm is identified by an absent op line, not an error**: the spawn does not fail, it spins inside `uv_spawn` before any timer arms, so `child:objects-done` is emitted before it and the launch timeout is the only bound. `GetProcessTimes` is sampled at the timeout, because cpu ≈ wall means a busy retry loop and cpu ≈ 0 means a blocking wait.

Per-run AppContainer SIDs are what make these arms inherently one-variable: each arm's device grant names a trustee no other arm holds, so a failed revoke cannot silently treat a later arm — and the read-back from the device's own DACL is asserted present where granted and absent where withheld.

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

The verdict block lives in `verdict.ps1` so `selftest.ps1` can drive it with **synthetic** cells across ten worlds and require a different answer from each:

| world | what it models | required answer |
| --- | --- | --- |
| `works` | bypass-traverse works | every property passes |
| `denied` | bypass-traverse fails | the decisive properties fail, every control still passes — what a *clean negative* must look like |
| `harness-dead` | every arm fails, `plain` included | the baseline properties fail, so a broken harness can never be reported as a clean negative |
| `ace-inert` | everything passes everywhere | the controls fail, so a grant that scopes nothing can never be reported as a pass |
| `grant-never-landed` | the ACE never propagated into the deep file | the grant-reached control fails. Read-cell for read-cell identical to `denied`, which is the point — without that control a harness slip is indistinguishable from a real kernel denial |
| `defect-absent` | the no-flag arm ran fine | the flag differential fails, so the flags are never credited with fixing something that was not broken |
| `obj-nul-fixes-the-hang-too` | the `\Device\Null` repair also unhangs the piped spawn | the "does not fix the hang" property fails, so a broader-than-predicted effect is loud rather than absorbed |
| `obj-repair-never-landed` | the device ACE never reached the object | the repair-reached control fails. Cell for cell identical to "neither repair works", which is the point |
| `obj-baseline-leaked` | a device revoke silently failed, so the repeat baseline reads as repaired | the baseline-repeat guard fails |
| `obj-harness-dead` | `obj-plain` fails every object cell | the unconfined control fails, so a confined table of failures is never reportable |

It needs no Windows APIs and runs anywhere:

```
pwsh -NoLogo -NonInteractive -File tests/win-bypass-traverse/selftest.ps1
```

CI runs it before the real launch.

## Reproducing

Push to `sandbox/win-bypass-traverse` or `sandbox/win-null-device`; the workflow is `push`-triggered on those branches, so no PR is needed (`gh workflow run` will not work until the file reaches the default branch). To re-run unchanged: `git commit --allow-empty -m rerun && git push`.

On a real Windows box in an **interactive** session (not over SSH — see above):

```
pwsh -NoLogo -NonInteractive -File tests\win-bypass-traverse\probe.ps1
```

Read the operations table and the `prop:` lines, not the exit status. A clean negative — controls green, bypass-traverse red — is a good outcome for the effort and still exits non-zero.
