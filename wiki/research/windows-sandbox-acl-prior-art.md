# Prior art: Windows sandbox ACL evaluation and AppContainer nesting

Research compiled 2026-08-06 for nub's Windows build jail. Scope: how production Windows sandboxes and build jails decide whether a directory is safe to confine into, which Win32 API they use to answer that question, whether anyone uses `GetEffectiveRightsFromAclW`, and whether AppContainer nesting is a supported thing to do.

Sources are read from source checkouts, not blog posts. Every claim is labelled:

- **MEASURED** — an empirical result. Where the measurement is nub's own (from the dispatch brief for this survey), it is marked *MEASURED (nub)*; this document did not re-run it.
- **CITED-FROM-SOURCE** — read out of a named file at a named line.
- **INFERRED** — a conclusion drawn from the above, not directly observed.

## TL;DR

| Question | Answer |
| --- | --- |
| How do production sandboxes decide a directory is safe to confine into? | They ask a narrow question about one specific path, not "is this tree clean". Chromium builds the real sandbox token and calls `AccessCheck`; others probe for a capability or hand-roll an ACE walk. |
| Does anyone use `GetEffectiveRightsFromAclW`? | **No one.** Zero uses across a 250-repo corpus including every Windows sandbox in it. |
| Is the 1336 defect documented? | **Partly.** One of the two measured conditions is a documented limitation. The other — alternating explicit/inherited ACEs — appears to be undocumented anywhere public. |
| Is AppContainer nesting supported? | Child AppContainers exist as a kernel concept, but no surveyed sandbox nests them. The documented default is that a child process *inherits* the parent's token rather than getting a new container. |
| What should nub do? | Stop asking the effective-rights question. Either ask the kernel with the real token, or drop the pre-flight check and let the launch fail with a good error. |

## 1. How production sandboxes decide a directory is safe to confine into

The strongest finding is a framing one: **none of the surveyed projects asks "is this directory clean enough to confine into."** That question — scan a tree, judge whether the ACEs on it are acceptable — is not one any of them poses. They ask a narrower, decidable question about one specific path, and they ask it in one of four ways.

### Chromium — build the real token, ask the kernel, hard-fail

Chromium's is the most rigorous, and it is the pattern worth copying. `AppContainerBase::AccessCheck` (`sandbox/win/src/app_container_base.cc:261`) does the following, CITED-FROM-SOURCE:

1. Reads the security descriptor for the named object with `SecurityDescriptor::FromName`, pulling owner, group, DACL, and label.
2. If low-privilege AC is enabled, edits the in-memory DACL to *revoke* `ALL_APPLICATION_PACKAGES`, because — in its own words — "We can't create a LPAC token directly, so modify the DACL to simulate it."
3. Builds the actual lowbox primary token it intends to launch with (`BuildPrimaryToken`), then duplicates it to an **identification-level impersonation token**.
4. Calls `SecurityDescriptor::AccessCheck` with that token.

That last call lands on the Win32 `::AccessCheck` — CITED-FROM-SOURCE, `base/win/security_descriptor.cc`, which runs `::MapGenericMask` on the desired access, sizes a `PRIVILEGE_SET` from the token's privileges, converts the descriptor to absolute form, and calls `::AccessCheck(&sd, token.get(), …)`.

The production caller is `SandboxWin::AddAppContainerProfileToConfig` (`sandbox/policy/win/sandbox_win.cc:798`), and its shape is instructive. It checks **exactly one path** — the executable it is about to launch — for **exactly one access mask**, `GENERIC_READ | GENERIC_EXECUTE`. On failure it does not repair the ACL, does not warn and continue, and does not fall back. It aborts the launch with a dedicated error code and a message naming the file:

> Sandbox cannot access executable … Check filesystem permissions are valid.

INFERRED: Chromium treats reachability as a launch precondition with a binary answer, deliberately scoped to the one object whose inaccessibility would produce an unexplainable failure later. It never forms an opinion about the surrounding tree.

**Firefox is covered by the above, not a separate data point.** Mozilla vendors Chromium's sandbox into `security/sandbox/chromium/` in mozilla-central and periodically re-syncs it from upstream Chromium, tracked in a long series of Bugzilla "Update `security/sandbox/chromium/` to Chromium …" bugs. Its Windows process sandbox is Chromium's, so the mechanism above is Firefox's mechanism. Caveat on confidence: this was established from Bugzilla and the `security/sandbox/moz.build` layout, not by reading a mozilla-central checkout, and a vendored snapshot can lag upstream by a lot.

### Microsoft mxc — cheap capability probe first, hand-rolled canonical walk second

`microsoft/mxc` runs a three-tier isolation fallback (`src/backends/appcontainer/common/src/fallback_detector.rs`), where tier 3 is "AppContainer + DACL" and needs to add ACEs to host paths. Before committing to that tier it calls `ensure_path_grantable_for_ac` (`:376`), CITED-FROM-SOURCE, which is a two-step:

1. Try to open the path for `WRITE_DAC` (`check_write_dac_path`). If that succeeds, stop — it can rewrite the ACL, so the current ACL does not matter.
2. Only if `WRITE_DAC` is unavailable, fall through to `appcontainer_already_grants` (`:358`), which does the expensive walk.

The comment above it explains the ordering as a performance decision: the `WRITE_DAC` probe is a single `CreateFileW`, where the walk costs a `GetNamedSecurityInfoW` plus a DACL walk plus three SID allocations.

The walk itself is `compute_appcontainer_effective_access` (`src/core/wxc_common/src/filesystem_dacl.rs:1617`) — a hand-rolled, ordering-faithful accumulation over `GetAce`:

```rust
AceType::Deny  => { denied  |= ace_mask & !allowed; }
AceType::Allow => { allowed |= ace_mask & !denied;  }
```

Three details in it are directly relevant to nub. CITED-FROM-SOURCE from the doc comment at `:1603` and the constant at `:1601`:

- It matches only three SIDs: `["S-1-15-2-1", "S-1-15-2-2", "S-1-1-0"]` — `ALL APPLICATION PACKAGES`, `ALL RESTRICTED APPLICATION PACKAGES`, and `Everyone`. Per-container explicit grants on a *specific* AppContainer SID are deliberately excluded, "the caller is presumably deciding whether such a grant is needed."
- Inherited ACEs are deliberately included.
- A NULL DACL means full access for everyone, but mxc returns 0 anyway and forces the `WRITE_DAC`-and-apply path, on the stated grounds of treating it as "trust nothing about it."

The first of those is the direct answer to nub's question 1 about other AppContainers' package SIDs in the tree. **mxc simply does not look at them.** It filters to the well-known membership SIDs that any AppContainer carries, so a stray `S-1-15-2-<hash>` ACE left by an unrelated Store app is invisible to the decision and cannot influence it.

### OpenAI Codex — allow-only ACE walk, ordering-blind

`openai/codex` reaches the same place independently. `codex-rs/windows-sandbox-rs/src/acl.rs` walks ACEs by hand in `dacl_mask_allows_with_scope` (`:123`), CITED-FROM-SOURCE: `GetAclInformation` for the count, then `GetAce` in a loop, skipping any ACE that is not `ACCESS_ALLOWED_ACE_TYPE`, skipping `INHERIT_ONLY_ACE`, optionally skipping `INHERITED_ACE` when the caller wants explicit-only scope, matching the SID with `EqualSid`, and normalising the mask through `MapGenericMask` before testing bits.

INFERRED: this walk is deny-blind — it only ever inspects allow ACEs — so it is an approximation of a real access check, not a reimplementation of one. It is also completely ordering-insensitive, which is exactly why it cannot fail the way the effective-rights API does.

Codex also carries a comment that independently corroborates the canonical-ordering rule (`:672`), CITED-FROM-SOURCE:

> `SetEntriesInAclW` places newly-created deny ACEs before allow ACEs, which keeps the resulting DACL in the order Windows expects for denies to win.

### Anthropic sandbox-runtime — never query, always rebuild

`anthropic-experimental/sandbox-runtime` sidesteps the question. Its `srt-win-src/src/acl.rs` has no effective-rights concept at all; it has `rebuild_acl` (`:275`) and `filter_aces`, and it composes a new ACL head-and-tail around kept ACEs. Its own section comment (`:305`) frames the primitives as "shared low-level wrappers so the recompose callers don't each open-code `GetNamedSecurityInfoW` + `GetAce` loops."

Worth noting for nub, CITED-FROM-SOURCE from that same comment: the two callers make *opposite* choices about inherited ACEs — `winsta.rs::recompose_dacl` keeps them, the file caller drops them. INFERRED: inherited-ACE handling is a per-call-site policy decision in a shipped sandbox, not a global truth to be derived.

### Microsoft BuildXL — does not use ACLs or AppContainer at all

BuildXL is Microsoft's own build sandbox and the nearest match to nub's problem domain, so its absence from the ACL discussion is the finding. MEASURED: grepping the whole BuildXL tree for `AppContainer` or `LowBox` returns exactly one file, `Public/Src/Utilities/Utilities.Core/Interop/Windows/Process.cs`, and the hits at `:72`–`:75` are members of a transcribed `TOKEN_INFORMATION_CLASS` enum (`TokenIsAppContainer`, `TokenAppContainerSid`, `TokenAppContainerNumber`) — boilerplate, with no call site.

BuildXL's Windows sandbox is Detours-based API interception (`Public/Src/Sandbox/Windows/Detours/`). INFERRED: Microsoft's production build sandbox confines by intercepting file-system calls in-process, not by constructing a restricted token and reasoning about DACLs, and therefore never has to answer "is this directory safe to confine into."

## 2. Does anyone use `GetEffectiveRightsFromAclW`?

**No.**

MEASURED: an unfiltered grep for `GetEffectiveRightsFromAcl` across every text file in a ~250-repository reference corpus — including Chromium's sandbox, BuildXL, hcsshim, gVisor, Bazel, Nix, podman, buildkit, go-winio, mxc, sandbox-runtime, and Codex — matches **three files, none of which is a call site**:

| File | What it is |
| --- | --- |
| `wine/dlls/advapi32/advapi32.spec:322` | Wine's export declaration for the symbol |
| `wine/dlls/advapi32/security.c:266` | Wine's stub implementation (see below) |
| `node/deps/LIEF/src/PE/utils/ordinals_lookup_tables/advapi32_dll_lookup.hpp:259` | an ordinal-to-name lookup table in the LIEF PE parser |

An export declaration, a stub, and a symbol-name table are not uses. No project in the corpus calls this function.

That instrument was validated against known positives before the negative was trusted: the same grep for `GetNamedSecurityInfo` returns 21 files and for `SetEntriesInAcl` returns 20, across the same repos. A pattern that finds those two and returns only non-call-site matches for the third is discriminating, not broken.

What they use instead, by name:

| Project | Function | Location | Mechanism |
| --- | --- | --- | --- |
| Chromium | `AppContainerBase::AccessCheck` | `sandbox/win/src/app_container_base.cc:261` | real lowbox token → `::AccessCheck` |
| Chromium | `SecurityDescriptor::AccessCheck` | `base/win/security_descriptor.cc` | `::MapGenericMask` + `::AccessCheck` |
| microsoft/mxc | `compute_appcontainer_effective_access` | `src/core/wxc_common/src/filesystem_dacl.rs:1617` | `GetAce` walk, canonical accumulation |
| microsoft/mxc | `ensure_path_grantable_for_ac` | `src/backends/appcontainer/common/src/fallback_detector.rs:376` | `WRITE_DAC` open probe, then the walk |
| openai/codex | `dacl_mask_allows_with_scope` | `codex-rs/windows-sandbox-rs/src/acl.rs:123` | `GetAce` walk, allow-only |
| anthropic sandbox-runtime | `rebuild_acl` / `filter_aces` | `srt-win-src/src/acl.rs:275` | rebuild, never query |
| microsoft/BuildXL | — | — | Detours interception; no ACL evaluation |

INFERRED: there are exactly two viable strategies in production. Ask the kernel with a real token (`AccessCheck`, or `AuthzAccessCheck` for a token you do not hold), or walk ACEs yourself with `GetAce`. Nobody computes effective rights from a bare ACL.

## 3. Is the 1336 defect documented anywhere public?

Partly. One of the two measured conditions is documented; the other is not.

### Condition 1 — deny after allow — is a documented limitation, in weaker form

CITED-FROM-SOURCE, the Remarks section of the [`GetEffectiveRightsFromAclW` reference](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-geteffectiverightsfromaclw):

> The **GetEffectiveRightsFromAcl** function fails and returns **ERROR_INVALID_ACL** if the specified ACL contains an inherited access-denied ACE.

nub's MEASURED (nub) condition is broader: *any* deny ACE positioned after an allow ACE fails, inherited or not. INFERRED: the documented statement is a special case of the measured one. In a canonically-ordered DACL, inherited ACEs always follow explicit ones, so an inherited access-denied ACE is necessarily positioned after any explicit allow ACE — it satisfies "deny after allow" by construction. The documentation describes the instance Microsoft noticed; the measurement describes the rule.

The practical consequence is worse than the documentation suggests. An inherited deny is not exotic — it is what any deny ACE set on a parent directory with `OI`/`CI` propagates as. INFERRED: on a machine where any ancestor of the working directory carries an inheritable deny, this API can never succeed, and no amount of ACL hygiene on the directory itself will fix it.

### Condition 2 — alternating explicit/inherited allows — appears undocumented

I could not find this described anywhere: not in the Microsoft reference, not in Microsoft Q&A or the archived MSDN forum threads on this error, not in an issue tracker, and not in any source in the corpus. **Treat this as a novel finding.**

The measured data, MEASURED (nub): `EIEIEI` fails, `EIEIE` passes, `EEEIII` passes, where `E` is an explicit allow ACE and `I` an inherited one.

The obvious unifying theory is that the API rejects non-canonical DACLs — canonical order being explicit-deny, explicit-allow, inherited-deny, inherited-allow. It explains condition 1 and it explains `EIEIEI` failing and `EEEIII` passing. **It is falsified by `EIEIE` passing**, which is equally non-canonical. So the API does not implement a clean canonicality check.

INFERRED, and offered as a testable hypothesis rather than a conclusion: the trigger is the **number of explicit→inherited transitions** in the ACE sequence, failing at three or more.

| Sequence | E→I transitions | Measured |
| --- | --- | --- |
| `EEEIII` | 1 | passes |
| `EIEIE` | 2 | passes |
| `EIEIEI` | 3 | fails |

That fits all three data points exactly, where a raw ACE count does not (`EIEIEI` and `EEEIII` are both six ACEs, and 48 canonical ACEs were measured to pass). It predicts that `EIEIEIEI` fails and that `IEIEIE` — which has two E→I transitions — passes. Neither prediction has been run; both are cheap, and running them is the way to promote or kill this hypothesis. INFERRED further: a transition-count threshold is the signature of a bounded internal buffer or a fixed-size scratch array in the implementation, not of a semantic rule.

### Corroborating evidence that the API is abandoned, not merely quirky

Three independent signals, all CITED-FROM-SOURCE:

- **Microsoft deprecates it in its own reference page,** and the replacement it names is Authz. The page opens with "It may be altered or unavailable in subsequent versions. Instead, use the method demonstrated in the example below," and the example that follows is a complete `AuthzInitializeResourceManager` / `AuthzInitializeContextFromSid` / `AuthzAccessCheck` program.
- **Wine never implemented it.** `dlls/advapi32/security.c:275` is a stub in the current tree: it emits `FIXME("%p %p %p - stub\n", …)`, writes `STANDARD_RIGHTS_ALL | SPECIFIC_RIGHTS_ALL` into the out-parameter, and returns 0. INFERRED: in roughly 25 years no Wine application forced the issue, which is a reasonable proxy for how little real software calls this. It also means Wine's source cannot explain the failure mode — a line of enquiry worth closing off explicitly, since a readable reimplementation would have been the cheapest possible answer.
- **A project shipped it and migrated off it.** The `NTFSSecurity` PowerShell module rewrote `Get-NTFSEffectivePermission` to use `AuthzAccessCheck` instead of `GetEffectiveRightsFromAcl` in version 3.1, keeping the old implementation renamed as `Get-NTFSEffectivePermissionOld`. Its version history states the change but gives no rationale, so the *reason* is uncited — only the migration itself is evidence.

## 4. AppContainer nesting

CITED-FROM-SOURCE, [AppContainer for legacy apps](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-for-legacy-applications-):

> When an unpackaged process running in an app container calls **CreateProcess**, the child process typically inherits the parent's token. That token includes the integrity level (IL) and app container info.

and:

> being or not being in an app container is a second and orthogonal property. That said, if you *are* in an app container, then the integrity level (IL) is always *low*.

So the documented default for a child process is **inheritance, not nesting**.

Child AppContainers exist as a kernel concept — a child AppContainer SID carries four additional RIDs beyond the parent's eight, which is how it is distinguished — and `NtCreateLowBoxToken` is the primitive. But the documentation for that primitive does not state whether a lowbox token may be derived from a token that is already a lowbox token; it documents an impersonation-level restriction (`STATUS_BAD_IMPERSONATION_LEVEL` for anonymous or identification level) and a caller-integrity restriction (medium IL or higher), neither of which is about nesting. **Confirmed absence: I could not find a public statement of whether AppContainer nesting is supported, and no surveyed sandbox does it.**

MEASURED: none of Chromium, mxc, Codex, sandbox-runtime, or BuildXL creates an AppContainer from inside an AppContainer. Chromium's `AppContainerType` enum (`sandbox/win/src/app_container.h`) has `kNone`, `kDerived`, `kProfile`, `kLowbox` — four ways to *obtain* a container, all exercised from a normal medium-IL broker process, not from within a container.

Two related mechanics worth carrying over regardless, CITED-FROM-SOURCE from `app_container_base.cc`:

- **Package SIDs are derived, not random.** `DerivePackageSid` (`:30`) calls `DeriveAppContainerSidFromAppContainerName`, so the SID is a deterministic function of the profile name. A stable name gives a stable SID across runs.
- **Profile creation is globally serialised.** `ProfileLock` (`:148`) takes a named machine-wide mutex (`_app_container_profile_lock_0278d671-…`) around registration, and `CreateProfile` treats `ERROR_ALREADY_EXISTS` from `AppContainerRegisterSid` as success rather than as an error. INFERRED: profile registration races between concurrent launches on one machine, and Chromium hit it hard enough to add a machine-global mutex. Any design that creates a fresh profile per launch inherits that race.

## 5. What nub should do

**Recommendation: delete the effective-rights pre-flight check rather than fixing it.**

The reasoning, in order of strength:

1. **The question nub is asking is not one anybody else asks, and not one this API can answer.** Effective rights *from an ACL* deliberately excludes the things that decide real access — the docs list implicitly-granted owner rights, privileges, logon-session group rights, and resource-manager policy as all out of scope. An answer that omits those is not the answer to "can the sandboxed process read this."
2. **Both failure conditions are properties of ACLs nub does not control.** An inheritable deny on any ancestor, or an interleaving of explicit and inherited ACEs, is produced by installers, Group Policy, and Store apps. A machine can be in a state where this check cannot succeed and nothing nub does to its own directory will change that.
3. **The API is deprecated by its own vendor in favour of Authz, unimplemented in Wine, and used by nobody in a 250-repo corpus.** Building on it is building on something already abandoned.

If a reachability check is wanted, the shape to copy is Chromium's, narrowed to nub's case: build the lowbox token nub is actually going to launch with, duplicate it to an identification-level impersonation token, and call `::AccessCheck` against the security descriptor of the **one** path whose inaccessibility would otherwise produce a confusing failure — then hard-fail the launch with a message naming that path. This is ordering-agnostic by construction because it is the kernel's own evaluator, so neither measured condition can arise.

If a check is wanted but building the token early is inconvenient, mxc's cheaper pattern applies: probe for the capability directly (open the path for the access you need, or for `WRITE_DAC` if you intend to grant) and treat the open's success as the answer. A capability probe answers the real question with one syscall and no ACL reasoning at all.

### A third option nobody in the corpus uses: permissive learning mode

Windows 11 and Windows Server 2022 added a mode in which the kernel performs the normal LowBox access check but, on failure, **logs the denial instead of returning access-denied**. It is enabled by adding the `permissiveLearningMode` capability SID to the lowbox token at creation, or by declaring it in a packaged app's manifest, and it reports through ETW on the `Microsoft-Windows-Kernel-General` provider under the `KERNEL_GENERAL_SECURITY_ACCESSCHECK` keyword (`0x20`). This is documented publicly only in James Forshaw's write-up, [LowBox Token Permissive Learning Mode](https://www.tiraniddo.dev/2021/09/lowbox-token-permissive-learning-mode.html); whether Microsoft will document it is, in his words, "remains to be seen."

This inverts the problem. Rather than predicting ahead of launch which paths the jail will fail to reach, it runs the jail and records every access the sandbox would have denied — using the kernel's own evaluator, on the real token, against the real objects.

Two caveats before treating it as a solution, both CITED-FROM-SOURCE from that write-up. **The logged events do not contain the name of the resource being opened** — they carry the requested access mask, the security descriptor, the object type, and token information, which makes attribution to a specific path indirect. And it is undocumented by the vendor, so it is not something to make load-bearing. INFERRED: its realistic value to nub is as a **diagnostic** for building the build-jail grant catalog — answering "what did this package's install script actually need?" — rather than as a runtime mechanism. That is a different job from the pre-flight check and does not replace it.

**The strongest counter-argument to deleting the check**, stated fairly: a pre-flight check exists to turn a late, incomprehensible failure into an early, precise one. Install scripts fail in ways that are extremely hard to attribute — a Node process inside a jail that cannot read its own working directory produces an `ENOENT` or an `EPERM` from deep inside a package's `postinstall`, with nothing pointing at confinement as the cause. Removing the check does not remove that failure mode, it removes the diagnosis. Chromium's own behaviour is the evidence for this side: it did not delete its check, it built the expensive token to do it properly, and it spends a full token construction on every sandboxed launch to get a good error message.

INFERRED: that counter-argument argues against deleting the *diagnosis*, not for keeping *this API*. The synthesis is to keep a pre-flight check and re-implement it as a token-based `AccessCheck` or a capability probe. Deleting the effective-rights call is not in tension with it. What would be a real trade-off is the cost — Chromium pays a token construction per launch — and whether nub wants to pay that on an install-script hot path where many jails are created in sequence.

## Confirmed absences

Things searched for and not found. Each is a result.

- **No use of `GetEffectiveRightsFromAclW` in any surveyed project.** Three corpus-wide matches, all non-call-sites (an export declaration, a stub, a symbol table); instrument validated against two known positives.
- **No public documentation of the alternating explicit/inherited failure condition.** Not in the Microsoft reference, Microsoft Q&A, the archived MSDN forum threads on this error, or any issue tracker reached.
- **No public statement on whether AppContainer nesting is supported.** `NtCreateLowBoxToken` documents impersonation-level and caller-IL restrictions, and is silent on deriving a lowbox token from a lowbox token.
- **No project that treats "an unrelated AppContainer's package SID appears in this tree" as a signal.** mxc explicitly filters to well-known membership SIDs and ignores per-container SIDs; nobody else looks at package SIDs on directories at all.
- **No rationale recorded for the NTFSSecurity migration** off `GetEffectiveRightsFromAcl`. The change is documented in the version history; the reason is not.
- **Wine cannot explain the failure mode.** Its implementation is a stub, so the hoped-for readable reimplementation does not exist.

## Changelog

- 2026-08-06 — Initial write-up.
