# Results — run 30675254853, both images, `FAILURES = 0`

`windows-latest` (Server 2025 Datacenter 10.0.26100, AMD64, Node 22.23.1) and `windows-11-arm`
(ARM64). **Every access cell below is IDENTICAL on both images**; only the timings differ.
All 18 controls PASS on both, so the tables are the tokens' doing and not the harness's.

## 1. The DACL-write ceiling: the user's own profile, and nothing above it

Where an unelevated principal can install a grant at all. The elevated column is the control —
it succeeds on `C:\` on both images, so the de-elevated refusal is attributable to privilege
rather than to a bad path.

| root | elevated | **de-elevated** |
| --- | --- | --- |
| `C:\` | OK | **ERR 5 ACCESS_DENIED** |
| `C:\Users` | OK | **ERR 5** |
| `C:\ProgramData` | OK | **ERR 5** |
| `C:\Program Files` | ERR 5 | ERR 5 |
| `C:\Windows` | ERR 5 | ERR 5 |
| `%USERPROFILE%` | OK | **OK** |
| project dir | OK | **OK** |

## 2. Inheritance is static — so a broad grant is a tree walk, per launch

One variable apart, same tree size, same trustee, reading the mask off a **pre-existing** deep
file's own DACL:

| ACE written at the tree root | deep file's effective mask |
| --- | --- |
| inheritable, **non**-propagating | `0x00000000` |
| inheritable, propagating | `0x001301bf` |

So reaching content that already exists requires the propagating write. Cost is linear in entries
across two orders of magnitude:

| entries | windows-latest grant / revoke | windows-11-arm grant / revoke |
| --- | --- | --- |
| 220 | 33 / 26 ms | 19 / 17 ms |
| 4,100 | 533 / 680 ms | 318 / 297 ms |
| 30,300 | 4,072 / 3,820 ms | 2,604 / 2,675 ms |
| **per entry** | **0.134 ms** | **0.086 ms** |

Measured on the real runner profile: **1,004 ms** for 5,779 entries. Projected (a projection, said
plainly) for a developer profile, grant **plus** the revoke every launch must also pay:

| profile entries | round trip, x64 | round trip, arm64 |
| --- | --- | --- |
| 100,000 | 27 s | 17 s |
| 500,000 | 134 s | 86 s |
| 1,000,000 | 269 s | 172 s |

## 3. The write matrix — five real launches, one variable apart

`plain` = no token (what the full-disk tier does today) as the **elevated** runner.
`plain-deelev` = no token under a de-elevated restricted token — the honest baseline, since a real
user is not a CI admin. `ac-max` = the LowBox token plus an inheritable write grant on every root
section 1 proved reachable, i.e. the ceiling by construction.

| op | plain | plain-deelev | ac-bare | ac-leaf | **ac-max** | ac-max-net |
| --- | --- | --- | --- | --- | --- | --- |
| granted-write | OK | OK | EPERM | OK | OK | OK |
| ungranted sibling write | OK | OK | EPERM | EPERM | **OK** | OK |
| profile pre-existing write | OK | OK | EPERM | EPERM | **OK** | OK |
| profile new file | OK | OK | EPERM | EPERM | **OK** | OK |
| `C:\` mkdir | OK | **OK** | EPERM | EPERM | **EPERM** | EPERM |
| `C:\ProgramData` mkdir | OK | **OK** | EPERM | EPERM | **EPERM** | EPERM |
| `C:\Program Files` create | OK | EPERM | EPERM | EPERM | EPERM | EPERM |
| `C:\Windows` create | OK | EPERM | EPERM | EPERM | EPERM | EPERM |
| **`~` secret read** | OK | OK | EPERM | **EPERM** | **OK** | OK |
| `C:\` listing | OK | OK | EPERM | EPERM | EPERM | EPERM |
| **egress** | OK | OK | EACCES | EACCES | **EACCES** | **OK** |

## 4. The two answers that decide the design

**Egress is fully separable from the filesystem grant.** `ac-max` and `ac-max-net` carry the
byte-identical ACE set and differ only by the `internetClient` capability: egress is `EACCES` /
`ENOTFOUND` in one and `OK` in the other. A maximum-grantable tier keeps the network gated; a
no-token launch cannot, which is the whole argument for preferring one.

**What the ceiling still denies:** `C:\` and `C:\ProgramData` creation (both of which a standard
user CAN do without the token — see `plain-deelev`), `C:\Program Files`, `C:\Windows`, listing
`C:\`, and anything outside the granted roots. **What it stops denying:** the user's own secrets.
`~`-secret read goes `EPERM` at `ac-leaf` and `OK` at `ac-max`, because a profile-wide grant
necessarily covers `~/.ssh`, `~/.npmrc` and everything beside them — and an AppContainer deny ACE
is inert against its own child (MECHANISM-FACTS §5), so the exception cannot be carved back out.

## Limits of this run

- The projections in section 2 are projections from a measured linear slope, not measurements of a
  large profile. The largest tree actually walked was 30,300 entries.
- Only `C:` was tested. A second volume's root may have a different owner and so a different
  de-elevated answer.
- The ceiling was constructed one way — an inheritable grant on every de-elevated-reachable root.
  The other constructions are closed by prior measurement rather than re-measured here: capability
  SIDs (§5f, the kernel refuses the AppSilo class that `C:\` actually grants) and an
  `ALL APPLICATION PACKAGES` ACE on a shared root, which needs the same refused `WRITE_DAC`.
