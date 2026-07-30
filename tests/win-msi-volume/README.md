# `tests/win-msi-volume` — three measurements against the AppContainer build jail

Both questions here are premised on `.fray/sandbox-MECHANISM-FACTS.md` §5h's bypass-traverse result
holding in conditions nobody had tested. Read §5h §5d–§5h first; nothing below re-derives it.

| | question | why it can invalidate the route |
| --- | --- | --- |
| 1 | Can an AppContainer child EXEC `node.exe` on a stock **MSI-installed** Node? | [`nodejs/node#63590`](https://github.com/nodejs/node/issues/63590): the MSI's `SetInstallDirPermission` writes a **protected** DACL on `C:\Program Files\nodejs` naming only Users / Authenticated Users / Administrators / SYSTEM. If no `S-1-15-2-*` sid holds rights there, the jail cannot start the user's Node at all and every §5h property is moot for that user. |
| 2 | Does the traverse skip hold on a **non-local volume**? | §5h measured only local `C:` NTFS and left the mechanism unresolved between `SeChangeNotifyPrivilege` and `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL`. The volume-flag reading predicts traverse **would** be enforced on a device lacking the flag — a mounted VHD, a mapped drive, a UNC path — which is where developers keep projects. |
| 3 | Does a **nub-owned, ACE-able bin dir plus a sanitized `PATH`** close the gap question 1 found? | Questions 1 and 2 are *can this invalidate the route*. Question 3 is the first one here that proposes a **fix**. §5j settled that the jail execs the MSI `node.exe` but cannot read that tree, so the bundled `npm` is unusable and `C:\Program Files\nodejs` cannot be ACE'd unprivileged. If a directory nub owns can hold what a jailed script needs and `PATH` can point only there, the gap closes on nub's side with no elevation and no upstream fix. |

## Why §5h could not have caught question 1

Not the reason it looks like. The suspicion is that a CI-provisioned Node carries different ACLs than
an MSI one, which is true and is measured here. But that is not what hid it:
`tests/win-bypass-traverse/probe.ps1:360` **copies** `node.exe` into a granted directory inside
`%USERPROFILE%` and launches the copy, with an in-place comment explaining that granting
`C:\Program Files\nodejs` would need `WRITE_DAC` on a path the user does not own. No prior arm ever
executed an ambient Node under the jail. The question was side-stepped, not answered.

## Layout

| file | role |
| --- | --- |
| `acjail.ps1` | The AppContainer launcher, a superset of `tests/win-bypass-traverse/probe.ps1`'s validated `Bt` class. Adds capability SIDs, `CreateProcessAsUserW` from a `CreateRestrictedToken` with named privileges deleted, a child-token read-back (`isAC` + privilege list), `NtQueryVolumeInformationFile` for the device Characteristics bitmask, and the ACE / DACL helpers. Duplicated rather than refactored so a shared edit cannot put both probes on untested code. |
| `child.js` | The in-child operations table for both probes, driven entirely by `AJ_TARGETS` (`name\|kind\|path` triples). Never spawns and never touches the network — a piped spawn under an AppContainer hangs indefinitely (§5h) and has already destroyed a run. |
| `msi-node-acl.ps1` | Question 1. Surveys every Node on the image, installs a real arch-correct MSI from `nodejs.org`, reads the DACL it writes, then runs the exec and read arms. |
| `volume-traverse.ps1` | Question 2. Provisions a VHD volume, a `subst` drive and an SMB share, runs the identical deep-read shape on each plus local `C:`, reads each volume's device Characteristics, and runs the privilege differential. |
| `jail-bin.ps1` | Question 3. Installs a real MSI to get a genuinely un-ACE-able ambient tree, grants an `S-1-15-2-1` ace on an EMPTY nub-owned dir and then populates it (so children inherit at creation), and runs thirteen arms over a **sanitized environment block** — which is the one thing `acjail.ps1` gained for this probe, behind a new `Launch` overload so every pre-existing arm's launch is byte-identical. |
| `msi-verdict.ps1` / `vol-verdict.ps1` / `jailbin-verdict.ps1` | The verdicts, factored out so the selftests can drive them with synthetic cells. |
| `selftest.ps1` | Fourteen synthetic worlds for questions 1 and 2, each asserting the **exact** set of failing property names. Runs anywhere `pwsh` does. |
| `jailbin-selftest.ps1` | Five synthetic worlds for question 3, including one where every decisive cell is green **and** the negative control is green too — the world a verdict without attribution waves through. |

## The controls, and what each rules out

A table of failures is only interpretable because of these. Every one is required, not decorative.

- **Unconfined baseline** per arm/volume — the same child, same paths, no `SECURITY_CAPABILITIES`.
  Must succeed everywhere. Rules out a broken fixture or an absent volume.
- **DACL read-back** per arm — the decisive target's own descriptor must name the AC sid where granted
  and **not** where withheld. Without it a propagation slip is indistinguishable from a kernel denial.
- **Gate-is-live / positive control** — every confined arm denied on `C:\`, and `System32` readable
  (it carries an `ALL APPLICATION PACKAGES` ace). Together they prove the token is confined *and* that
  the gate is passable, so a column of `ERR` is about DACLs rather than a dead child.
- **Question 1's one-variable repair arm** — grant the AC sid `ReadAndExecute` on
  `C:\Program Files\nodejs` (needs elevation) and re-launch the *identical* command line. This is what
  attributes a refusal to the DACL rather than to anything else about `C:\Program Files`.
- **Question 1's sibling control** — sampled `C:\Program Files\*` directories must carry the ace, or
  "nodejs lacks it" is not a defect.
- **Question 1's impersonation control** — under the admin-stripped token, an ACE write *inside*
  `%USERPROFILE%` must succeed, or "nub cannot repair it unprivileged" is unproven rather than shown.
- **Question 2's reachability control** (`<vol>-rootgrant`) — an inheritable ace at the volume root so
  no ancestor is ungranted. This is what separates "the volume is unreachable under an AppContainer
  for some other reason" — network isolation on a UNC path, a per-session device map on `subst` — from
  "traverse is enforced here". A red treatment row with a red reachability control says nothing.
- **Question 2's anchor** — local `C:` in the *same run*, so a failure elsewhere is attributable to the
  volume rather than the harness.
- **Question 2's privilege differential control** (`cn-kept`) — the identical
  `CreateProcessAsUserW` + `CreateRestrictedToken` path deleting nothing, plus a read-back of the
  privilege list off the process that actually ran.
- **Question 3's mechanism attribution** (`ac-jailbin-ungranted`) — the *identical* command line and
  `PATH` with the jail-bin ace **revoked**, and the revoke read back at both the directory and the
  five-levels-down target. Without it a green jail-bin arm could be bypass-traverse, an inherited ace,
  or the runner image. Its sibling `ac-ambient` re-measures §5j's own failure inside the same run.
- **Question 3's interpreter read-back** — the hybrid arm differs from the jail-bin arm only in `PATH`
  ORDER, so the child reports `process.execPath` and the two must DISAGREE. Otherwise "the ambient
  `node.exe` ran" is an assumption and the arm says nothing about whether the binary needs copying.
- **Question 3's environment control** — the child reports its own `PATH` and it must equal the block
  handed to it exactly. Every pre-existing arm in this directory passes `lpEnvironment = NULL` and so
  INHERITS the runner's `PATH`; a cell measured that way cannot tell "resolved from the nub-owned dir"
  from "resolved from the ambient install", and would confirm whatever the reader expected.
- **Question 3's inverted properties are gated on a real `ERR`, never on `-ne 'OK'`** — which would
  accept `MISSING-ARM`/`MISSING-OP` and report a treatment arm that never executed the operation as a
  clean negative. That is the bug §5k's run 1 shipped.
- **Elevation is reported per run** (`fact:admin`, the access check — never `TokenIsElevated`, which
  `CreateRestrictedToken` copies). Creating a VHD and an SMB share need admin; `subst` needs nothing;
  installing the MSI needs it and a user consents to the same thing. Question 3's *own* setup claim is
  measured separately, under an impersonated token whose privileges are DELETED rather than disabled —
  `DISABLE_MAX_PRIVILEGE` leaves them held and .NET's `Set-Acl` re-enables what it needs, which
  produced a false positive in `msi-node-acl.ps1`'s run 1.

## Reading the badge

Several properties are named for the **claim they measure**, not for the outcome anyone wanted, so a
clean and fully-controlled informative run still exits non-zero:
`msi-node-cannot-be-executed-by-an-appcontainer` PASSing means the route is broken for MSI users,
`<vol>-bypass-traverse-holds` FAILing is the compat finding, and
`jb-hardlink-ace-leaks-to-the-original-path` PASSing means hard-linking is DISQUALIFIED as a way to
populate the nub-owned dir. Read the table, not the badge.

## Running it

Branch-scoped, no pull request (`.claude/skills/ci-adhoc-test/SKILL.md`). Questions 1 and 2: push to
`sandbox/win-msi-acl-volumes` and `.github/workflows/win-msi-volume-probe.yml` fires. Question 3: push
to `sandbox/win-jail-bin` and `.github/workflows/win-jail-bin-probe.yml` fires. AppContainer cannot
launch over OpenSSH (session 0 has no window station; every launch returns `0xC0000142`, §5e), so CI is
the only venue.

Locally, on any platform:

```sh
pwsh -File tests/win-msi-volume/selftest.ps1          # questions 1 and 2
pwsh -File tests/win-msi-volume/jailbin-selftest.ps1  # question 3
```
