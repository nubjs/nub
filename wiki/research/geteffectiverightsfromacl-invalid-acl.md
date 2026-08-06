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
| `EIEIE`, `IE`, `EIE`, `EIEE`, `IIIEEE` | 0 | fewer than three inherited blocks |
| `EIEIEI`, `IEIEI`, `EEIIEEIIEEII` | **1336** | three or more inherited blocks |
| `EEEIII` | 0 | **the same six ACEs as `EIEIEI`, regrouped into one inherited block** |
| `E24I24` (48 ACEs) | 0 | count alone never trips it |

**Trigger 1 — a deny ACE after an allow ACE.** Microsoft documents a narrower version of this: "The `GetEffectiveRightsFromAcl` function fails and returns `ERROR_INVALID_ACL` if the specified ACL contains an inherited access-denied ACE." The inherited case is just the common way to reach it, since inherited ACEs sort after explicit ones. An entirely explicit `Ed` fails identically.

**Trigger 2 — three or more maximal blocks of inherited ACEs.** Count the maximal runs of consecutive `INHERITED_ACE` entries. Three or more, and the call fails. Equivalently: the number of explicit→inherited transitions, plus one if the ACL begins with an inherited ACE.

Verified on 22 sequences with no counterexample, 12 of which were predicted before being run. The rule is independent of every quantity that plausibly correlates with it:

| discriminating pair | | |
| --- | --- | --- |
| `EIEIE` → 0 vs `IEIEI` → **1336** | identical length, run count and alternation | differ only in whether the FIRST ACE is inherited, which moves the block count 2 → 3 |
| `EEEIIIEEEIII` (12 ACEs) → 0 vs `EEEIIIEEEIIIEEEIII` (18 ACEs) → **1336** | both neatly grouped, neither interleaved | 2 blocks vs 3 |
| `EEEEEEIIEEEEEEII` (16 ACEs) → 0 | 2 blocks | size is irrelevant |
| `IIEEIIEEIIEEII` → **1336** vs `IIEEIIEE` → 0 | same motif, extended | 4 blocks vs 2 |

It is also **asymmetric**: the number of EXPLICIT blocks does not matter. `EIEIE` has three explicit blocks and passes. Only inherited blocks count, which is why the `INHERITED_ACE` flag is required — clearing it from the second ACE of each pair makes the same ACL pass.

⚠️ **A superseded reading is recorded here deliberately.** This was first written up as "alternating past ~3 pairs", derived from sequences that all happened to begin with `E`. Two theories fit that data — a threshold on explicit→inherited transitions, and one on total runs — and `IEIEIE` was run precisely because it is where they diverge. It killed both: it fails with only 2 transitions, and `IEIEI` fails at 5 runs. The block-count rule is what survived, and it was then confirmed against fresh predictions rather than refitted. The earlier "~3 pairs" figure was imprecise and should not be cited.

This appears to be undocumented. A survey of the Microsoft reference, Microsoft Q&A, archived MSDN threads on `ERROR_INVALID_ACL`, and the issue trackers of projects that walk Windows ACLs found no statement of it; trigger 1 is documented, in a weaker form. The threshold was located on one Windows Server build, so re-check the constant before relying on it elsewhere — but the *shape* of the rule is established rather than guessed.

The real jail-home DACL carries 12 inherited blocks, four times the threshold.

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

## The API reports group-expanded rights, and that invented a finding

This is a separate defect from the 1336 failure, and it did more damage, because it did not error — it answered, plausibly, and wrongly.

`GetEffectiveRightsFromAclW` expands group membership. `ALL APPLICATION PACKAGES` is a member of `Everyone`, so querying AAP against an ACL whose only relevant ACE grants Everyone returns **Everyone's mask under AAP's name**. Measured: an ACL containing only `allow Everyone 0x1200A9` reports `rights=0x1200A9` for trustee `S-1-15-2-1`; substituting `BUILTIN\Users` reports `0`.

Two consequences, one historical and one architectural.

**It manufactured a documented finding that was never true.** [`build-jail-windows.md`](../design/build-jail-windows.md) recorded a survey showing `C:\Users` granting AAP rights `0x001200a9`, alongside a later descriptor read finding no AAP ACE there at all. The record concluded the two "do not reconcile as stated" and reached for image-dependence. Both readings were correct. `C:\Users` carries `Everyone:(RX)` — exactly `0x1200A9` — and no AAP ACE. The reported grant never existed; the instrument invented it, and the contradiction was the only symptom.

**It is also the wrong question.** A LowBox token reaches an object only where its ACL names an AppContainer SID, a capability SID, or AAP; an `Everyone` grant confers nothing on it. So group expansion does not merely mislead here, it answers a question about a principal the check is not asking about. Confirmed directly rather than argued: a secret behind an `Everyone`-only DACL outside the allow-set is `Access is denied.` to the confined child, while a granted file read in the same launch succeeds ([`windows_noncanonical_dacl.rs`](../../crates/nub-sandbox/tests/windows_noncanonical_dacl.rs)).

### The class, worth naming

Both of this document's corrections have the same shape: **a recorded conclusion that was an artefact of the instrument that produced it, not a fact about the system.** The "`C:\Users` grants AAP" entry above, and this document's own first draft of trigger 2 ("alternating past ~3 pairs"), which was an artefact of only ever testing sequences that began with an explicit ACE.

Neither looked like a mistake. Both were quantified, both were reproducible on demand, and each survived precisely as long as nobody derived it a second way. The cheap defence is the one that caught both: before believing a measurement, run the instrument against a case whose answer is already known, and prefer a prediction made *before* the run to a rule fitted *after* it.

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

Everything in this section comes from [`windows-sandbox-acl-prior-art.md`](windows-sandbox-acl-prior-art.md) rather than from the measurements above, which is worth flagging because the two have different standing: the triggers were reproduced here on a real machine, whereas these are second-hand and were not independently re-run.

No production project surveyed calls `GetEffectiveRightsFromAcl` at all. Across a ~250-repository corpus including Chromium's sandbox, BuildXL, hcsshim, gVisor, Bazel, Nix, podman and buildkit it matches three files, none of them a call site: Wine's export declaration, Wine's stub, and a symbol table. The search instrument was validated against known positives first (`GetNamedSecurityInfo` matched 21 files, `SetEntriesInAcl` 20), so the negative discriminates rather than reflecting a broken query. Wine's implementation being a stub also means there is no readable reimplementation to explain the failure mode from.

Microsoft deprecates the function in favour of the Authz API, and documents that it disregards implicitly granted owner rights, privileges, logon-session group rights, and resource-manager policy — most of what actually decides access.

Nor does any surveyed project treat an unrelated AppContainer's package SID appearing in a tree as a signal; mxc explicitly filters to well-known membership SIDs and ignores per-container ones. That is worth recording because the intuitive reading of this bug — "the profile carries Store-app package SIDs, so the root is compromised" — is wrong twice over: those SIDs are not the API's trigger, and a specific package SID confers nothing on a *different* AppContainer.

## Known gap, and the next step

The walk is scoped to `ALL APPLICATION PACKAGES` only. It does not consider `ALL RESTRICTED APPLICATION PACKAGES` (`S-1-15-2-2`) or capability SIDs (`S-1-15-3-*`), either of which could in principle make a root reachable by the confined child. **This gap is pre-existing and was not widened** — the replaced code was equally AAP-only.

The next step, when it matters, is one fixture on a Windows VM: grant a directory `S-1-15-2-2` and nothing else, launch a confined child, and see whether it can read. That settles whether nub's LowBox token carries the restricted SID, and so whether the scope needs widening. Not run, and deliberately not chased here.

## Changelog

- 2026-08-06 — Initial write-up.
- 2026-08-06 — **Trigger 2's mechanism identified and the first statement of it corrected.** It is the number of maximal blocks of consecutive inherited ACEs (≥3 fails), not "alternating past ~3 pairs". The earlier reading was an artefact of testing only sequences beginning with an explicit ACE; `IEIEIE` was run because it discriminates the two theories that fit that data, and it refuted both. Confirmed on 12 fresh predictions. Added the group-expansion finding as its own section and named the recurring class it belongs to.
