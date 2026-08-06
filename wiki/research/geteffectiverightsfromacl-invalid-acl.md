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

**Trigger 2 — alternating explicit and inherited allow ACEs.** Undocumented. `EIEIEI` fails while `EIEIE` passes, and `EEEIII` — the same six ACEs regrouped — passes. Nothing about the ACEs changes between the failing and passing forms except their order, which is what makes this a mechanism rather than a correlation. The `INHERITED_ACE` flag is required: dropping it from the second ACE of each pair makes the same ACL pass.

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

This resolves a contradiction previously recorded as unexplained in [`build-jail-windows.md`](../design/build-jail-windows.md). A survey there reported `C:\Users` as granting `ALL APPLICATION PACKAGES` rights `0x001200a9` while a later descriptor read found no AAP ACE on it at all. Both readings were correct: `C:\Users` carries `Everyone:(RX)` — which is `0x1200A9` — and no AAP ACE. The effective-rights survey was reporting Everyone's rights under AAP's name.

The expansion is also the wrong question for AppContainer reachability. A LowBox token reaches an object only where that object's ACL names an AppContainer SID, a capability SID, or `ALL APPLICATION PACKAGES`; an `Everyone` grant confers nothing on it. This project already has direct evidence: the user profile tree carries `Everyone:(OI)(CI)(IO)(GR,GE)` inheritably, and jailed probes against the real `%LOCALAPPDATA%` still read **0 bytes** against 42 MB unjailed.

## Why the corpus never caught it

The condition is a property of the machine, not of the harness. The AppContainer package SIDs on `%USERPROFILE%` are left by installed Store-app tooling; a fresh CI runner image does not carry them, and the pre-existing `windows_clean_root` suite passes on the affected VM because its own fixture roots happen to be evaluable. So a clean Windows corpus history is evidence about runner images, not about developer machines.

This is load-bearing rather than incidental: on Windows both the jail home and the package store resolve under `%LOCALAPPDATA%`, i.e. inside the user profile, so a developer whose profile carries these ACEs has both poisoned.

## The fix

Walk the ACEs directly — `GetNamedSecurityInfoW`, then `GetAce` per index — and compute the mask for `ALL APPLICATION PACKAGES` without asking for "effective rights", which is a strictly stronger question than the one being asked. The walk cannot fail on an ACL it merely finds awkward, does no group expansion, and can name the offending SID rather than reporting that the ACL structure is invalid. Unknown ACE types fail closed: an object or vendor ACE type places its trustee SID at a different offset, and guessing would risk under-reporting a grant.

Implementation: `for_each_ace_of_sid` in [`crates/nub-sandbox/src/backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs). Regression suite: [`crates/nub-sandbox/tests/windows_noncanonical_dacl.rs`](../../crates/nub-sandbox/tests/windows_noncanonical_dacl.rs), which builds the hostile DACL itself and asserts the legacy API rejects it before asserting anything else, so it cannot pass vacuously on a host that lacks the condition.

## Changelog

- 2026-08-06 — Initial write-up.
