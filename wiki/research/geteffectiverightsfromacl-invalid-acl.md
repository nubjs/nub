# `GetEffectiveRightsFromAcl` fails on ordinary DACLs

`GetEffectiveRightsFromAclW` returns `ERROR_INVALID_ACL` (1336, "The access control list (ACL) structure is invalid") on access control lists that are legal, that Windows itself created, and that every other ACL API reads without complaint. Two distinct ACE arrangements trigger it. Both occur on a normal developer machine.

This mattered here because the Windows build jail asked that API whether `ALL APPLICATION PACKAGES` could already reach a working root. On a machine carrying either arrangement the question could not be answered, the launch failed closed, and no lifecycle script could be confined at all. The jail is on by default, so that is a total failure rather than a degradation. Measured on the Windows VM: **552 real directories under a single `%LOCALAPPDATA%\nub` returned 1336**.

## The two triggers

Measured 2026-08-06 on Windows Server (`nub-win3`) by assembling ACLs in memory with `InitializeAcl` + `AddAccessAllowedAceEx`/`AddAccessDeniedAceEx` and calling `GetEffectiveRightsFromAclW` directly, so ACE type, flags, mask, order, count and trustee vary independently of any filesystem.

Notation: `E` = explicit allow ACE, `I` = inherited allow ACE (`INHERITED_ACE`, `0x10`), `d` = explicit deny, `D` = inherited deny. Each letter uses a distinct trustee SID. Trustee queried is `S-1-15-2-1`.

| ACL | rc | |
| --- | --- | --- |
| `E`, `I`, `EI`, `dE`, `ddEE`, `DE` | 0 | canonical, or too short to trip either rule |
| `Ed`, `EEd`, `EEdd` | **1336** | a deny ACE positioned after an allow ACE |
| `ED`, `EDE` | **1336** | same rule reached via an inherited deny |
| `IE`, `EIE`, `EIEE`, `EIEIE`, `IIIEEE` | 0 | interleaved, but under the threshold |
| `EIEIEI`, `EIEIEIEI` | **1336** | explicit/inherited allow ACEs alternating past ~3 pairs |
| `EEEIII` | 0 | **the same six ACEs as `EIEIEI`, regrouped** |
| `E24I24` (48 ACEs) | 0 | count alone never trips it |

**Trigger 1 — a deny ACE after an allow ACE.** Microsoft documents a narrower version of this: "The `GetEffectiveRightsFromAcl` function fails and returns `ERROR_INVALID_ACL` if the specified ACL contains an inherited access-denied ACE." The inherited case is just the common way to reach it, since inherited ACEs sort after explicit ones. An entirely explicit `Ed` fails identically.

**Trigger 2 — alternating explicit and inherited allow ACEs.** `EIEIEI` fails while `EIEIE` passes, and `EEEIII` — the same six ACEs regrouped — passes. Nothing about the ACEs changes between the failing and passing forms except their order, which is what makes this a mechanism rather than a correlation. The `INHERITED_ACE` flag is required: dropping it from the second ACE of each pair makes the same ACL pass.

This one appears to be undocumented. A survey of the Microsoft reference, Microsoft Q&A, archived MSDN threads on `ERROR_INVALID_ACL`, and the issue trackers of projects that walk Windows ACLs found no statement of it; trigger 1 is documented, in a weaker form. Treat it as a finding of this project rather than as a known limitation, and re-check before relying on the "~3 pairs" figure — the threshold was located by bisection on one Windows Server build, and only the ordering dependence itself was varied against a control.

## What is *not* the trigger

Each was varied on its own against a control that still returned 0:

| Suspected | Verdict |
| --- | --- |
| ACEs naming an unresolvable SID (AppContainer package SIDs, nonexistent domain SIDs) | **refuted** — well-known resolvable SIDs fail identically at the same pattern |
| `GENERIC_READ`/`GENERIC_WRITE`/`GENERIC_EXECUTE` bits in the mask | refuted |
| `OBJECT_INHERIT`/`CONTAINER_INHERIT`/`INHERIT_ONLY` flags | refuted |
| ACL revision 2 vs 4 | refuted |
| ACE count | refuted — 48 canonical ACEs pass |
| The `\\?\` verbatim path form from `std::fs::canonicalize` | refuted — identical both ways |

The unresolvable-SID theory is the intuitive one, because the machine that surfaced this carries seven AppContainer package SIDs on `%USERPROFILE%` and because MSDN warns that the function "returns an error if it cannot enumerate the members of a group". It is wrong. Substituting `BUILTIN\Users` reproduces the failure exactly.

## A second defect: the API reports group-expanded rights

`GetEffectiveRightsFromAclW` expands group membership, so querying `ALL APPLICATION PACKAGES` against an ACL whose only relevant ACE grants **Everyone** returns Everyone's mask. Measured: an ACL containing only `allow Everyone 0x1200A9` reports `rights=0x1200A9` for trustee `S-1-15-2-1`. An ACL containing only `allow BUILTIN\Users` reports `0`.

This retires a contradiction that [`build-jail-windows.md`](../design/build-jail-windows.md) had recorded as unexplained, and the correction is worth stating plainly so the next reader does not rediscover the same ghost: **the earlier entry was wrong, and it was wrong because the instrument was.** A survey there reported `C:\Users` as granting `ALL APPLICATION PACKAGES` rights `0x001200a9`, while a later descriptor read found no AAP ACE on it at all; the record concluded the two "do not reconcile as stated" and guessed at image-dependence. Both readings were in fact correct. `C:\Users` carries `Everyone:(RX)` — which is exactly `0x1200A9` — and no AAP ACE. The effective-rights survey was reporting Everyone's rights under AAP's name, so the "AAP grant" it discovered never existed.

A recorded conclusion can be an artefact of the instrument that produced it rather than a fact about the system, and it reads as evidence until someone derives it a second way.

The expansion is also the wrong question for AppContainer reachability. A LowBox token reaches an object only where that object's ACL names an AppContainer SID, a capability SID, or `ALL APPLICATION PACKAGES`; an `Everyone` grant confers nothing on it. This project already has direct evidence: the user profile tree carries `Everyone:(OI)(CI)(IO)(GR,GE)` inheritably, and jailed probes against the real `%LOCALAPPDATA%` still read **0 bytes** against 42 MB unjailed.

## Why the corpus never caught it

The condition is a property of the machine, not of the harness. The AppContainer package SIDs on `%USERPROFILE%` are left by installed Store-app tooling; a fresh CI runner image does not carry them, and the pre-existing `windows_clean_root` suite passes on the affected VM because its own fixture roots happen to be evaluable. So a clean Windows corpus history is evidence about runner images, not about developer machines.

This is load-bearing rather than incidental: on Windows both the jail home and the package store resolve under `%LOCALAPPDATA%`, i.e. inside the user profile, so a developer whose profile carries these ACEs has both poisoned.

## The fix

Walk the ACEs directly — `GetNamedSecurityInfoW`, then `GetAce` per index — and compute the mask for `ALL APPLICATION PACKAGES` without asking for "effective rights", which is a strictly stronger question than the one being asked. The walk cannot fail on an ACL it merely finds awkward, does no group expansion, and can name the offending SID rather than reporting that the ACL structure is invalid. Unknown ACE types fail closed: an object or vendor ACE type places its trustee SID at a different offset, and guessing would risk under-reporting a grant.

Implementation: `for_each_ace_of_sid` in [`crates/nub-sandbox/src/backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs). Regression suite: [`crates/nub-sandbox/tests/windows_noncanonical_dacl.rs`](../../crates/nub-sandbox/tests/windows_noncanonical_dacl.rs), which builds the hostile DACL itself and asserts the legacy API rejects it before asserting anything else, so it cannot pass vacuously on a host that lacks the condition.

The accumulation is canonical rather than allow-only: deny ACEs subtract, and a deny only removes rights an earlier allow has not already granted. An allow-only walk would ignore deny ACEs and under-refuse, which is the dangerous direction for this check.

### Alternatives considered

**Ask the kernel with a real LowBox token** — read the descriptor, build the token the launch will use, duplicate it to an identification-level impersonation token, call `AccessCheck`. This is what Chromium's sandbox does (`AppContainerBase::AccessCheck`). It is ordering-agnostic by construction, since the kernel's own evaluator decides, and it would also catch capability SIDs (`S-1-15-3-*`) and `ALL RESTRICTED APPLICATION PACKAGES` — which an AAP-scoped walk does not. Not adopted: `verify_clean_root` runs before any AppContainer profile exists, so it would mean creating and deleting a profile per check or reordering the launch. The capability-SID gap is pre-existing and unchanged by this fix. This is the direction to take if that gap ever matters.

**Probe by opening the path** — mxc's `ensure_path_grantable_for_ac` answers "can I open this for the access I need?" in one syscall. It does not apply here. That is a *sufficiency* question about the calling process; this check asks an *excess* question about a different principal. nub's process is not an AppContainer, so what it can open says nothing about what `ALL APPLICATION PACKAGES` can reach, and asking on AAP's behalf requires a LowBox token — at which point it is the previous option.

**Protecting the jail root instead was proposed and would have broken installs.** The alternative considered first was to give each working root a protected DACL, severing inheritance so it is clean by construction. Two things kill it. The premise was wrong — the ACEs a developer profile carries name *specific* package SIDs, and a specific package SID confers nothing on the freshly-created AppContainer nub launches, so a working check accepts such a machine rather than refusing it. And the existing suite already measures the damage it would do. `windows_clean_root`'s inheritance-severing probe reports:

> VERDICT inheritance-severing CONFIRMED: a protected directory does NOT receive a later inheritable grant on its parent, so protecting each working root would strand every previously-confined package dir

The build jail grants the dependency tree read on `<project>/node_modules` while each lifecycle script's cwd is `node_modules/<pkg>`. Protecting `<pkg>` blocks propagation from `node_modules`, so once package `a`'s script has run, every later package's script is granted `node_modules` and still cannot read `node_modules/a` — permanently, in that install and every install after it.

### Prior art

Everything in this section comes from a separate corpus survey rather than from the measurements above, which is worth flagging because the two have different standing: the triggers were reproduced here on a real machine, whereas these are second-hand and were not independently re-run.

No production project surveyed calls `GetEffectiveRightsFromAcl` at all. Across a ~250-repository corpus including Chromium's sandbox, BuildXL, hcsshim, gVisor, Bazel, Nix, podman and buildkit it matches three files, none of them a call site: Wine's export declaration, Wine's stub, and a symbol table. The search instrument was validated against known positives first (`GetNamedSecurityInfo` matched 21 files, `SetEntriesInAcl` 20), so the negative discriminates rather than reflecting a broken query. Wine's implementation being a stub also means there is no readable reimplementation to explain the failure mode from.

Microsoft deprecates the function in favour of the Authz API, and documents that it disregards implicitly granted owner rights, privileges, logon-session group rights, and resource-manager policy — most of what actually decides access.

Nor does any surveyed project treat an unrelated AppContainer's package SID appearing in a tree as a signal; mxc explicitly filters to well-known membership SIDs and ignores per-container ones. That is worth recording because the intuitive reading of this bug — "the profile carries Store-app package SIDs, so the root is compromised" — is wrong twice over: those SIDs are not the API's trigger, and a specific package SID confers nothing on a *different* AppContainer.

## Changelog

- 2026-08-06 — Initial write-up.
