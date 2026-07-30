# `tests/win-msi-volume` — two measurements that can each invalidate the AppContainer build jail

Both questions here are premised on `.fray/sandbox-MECHANISM-FACTS.md` §5h's bypass-traverse result
holding in conditions nobody had tested. Read §5h §5d–§5h first; nothing below re-derives it.

| | question | why it can invalidate the route |
| --- | --- | --- |
| 1 | Can an AppContainer child EXEC `node.exe` on a stock **MSI-installed** Node? | [`nodejs/node#63590`](https://github.com/nodejs/node/issues/63590): the MSI's `SetInstallDirPermission` writes a **protected** DACL on `C:\Program Files\nodejs` naming only Users / Authenticated Users / Administrators / SYSTEM. If no `S-1-15-2-*` sid holds rights there, the jail cannot start the user's Node at all and every §5h property is moot for that user. |
| 2 | Does the traverse skip hold on a **non-local volume**? | §5h measured only local `C:` NTFS and left the mechanism unresolved between `SeChangeNotifyPrivilege` and `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL`. The volume-flag reading predicts traverse **would** be enforced on a device lacking the flag — a mounted VHD, a mapped drive, a UNC path — which is where developers keep projects. |

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
| `msi-verdict.ps1` / `vol-verdict.ps1` | The verdicts, factored out so the selftest can drive them with synthetic cells. |
| `selftest.ps1` | Fourteen synthetic worlds, each asserting the **exact** set of failing property names. Runs anywhere `pwsh` does, and gates both Windows jobs. |

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
- **Elevation is reported per run** (`fact:admin`, the access check — never `TokenIsElevated`, which
  `CreateRestrictedToken` copies). Creating a VHD and an SMB share need admin; `subst` needs nothing.
  That is a fact about provisioning a test volume, not about nub's own setup requirement.

## Reading the badge

Several properties are named for the **claim they measure**, not for the outcome anyone wanted, so a
clean and fully-controlled informative run still exits non-zero:
`msi-node-cannot-be-executed-by-an-appcontainer` PASSing means the route is broken for MSI users, and
`<vol>-bypass-traverse-holds` FAILing is the compat finding. Read the table, not the badge.

## Running it

Branch-scoped, no pull request (`.claude/skills/ci-adhoc-test/SKILL.md`): push to
`sandbox/win-msi-acl-volumes` and `.github/workflows/win-msi-volume-probe.yml` fires. AppContainer
cannot launch over OpenSSH (session 0 has no window station; every launch returns `0xC0000142`, §5e),
so CI is the only venue. Locally, `pwsh -File tests/win-msi-volume/selftest.ps1` validates both
verdicts on any platform.
