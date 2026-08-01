# Results — runs 30688900451 / 30689267117 / 30689583039, both images, `FAILURES = 0`

`windows-latest` (Server 2025 Datacenter 10.0.26100, AMD64, Node 22.23.1) and `windows-11-arm`
(10.0.26200, ARM64, Node 22.23.1). **Every access cell is IDENTICAL on both images** — the only
diff across the two logs is which Cloudflare address `registry.npmjs.org` resolved to. Every
control passes on both, in all three runs.

## The answer

**Yes.** An AppContainer holding a broad filesystem grant and no `internetClient` capability writes
everywhere the invoking user can install an ACE, and reaches no network at all. What it costs
versus taking no token is four machine-wide directories the user can write but cannot re-ACL.

## 1. The write ceiling is WRITE_DAC, not `%USERPROFILE%`

Raw Win32, exact `GetLastError()`, elevated runner vs the same thread impersonating a restricted
**medium-integrity** token. `create` is a file; `mkdir` is a directory; `dacl` is
`SetNamedSecurityInfoW`.

| location | create | mkdir | read | list | **dacl** |
| --- | --- | --- | --- | --- | --- |
| `C:\` | **ERR 5** | OK | OK | OK | **ERR 5** |
| `C:\ProgramData` | OK | OK | OK | OK | **ERR 5** |
| `C:\Users` | **ERR 5** | **ERR 5** | OK | OK | **ERR 5** |
| `C:\Users\Public` | OK | OK | OK | OK | **ERR 5** |
| `C:\Windows\Temp` | OK | OK | OK | OK | OK |
| `C:\Program Files` | **ERR 5** | **ERR 5** | OK | OK | **ERR 5** |
| `C:\Windows` | **ERR 5** | **ERR 5** | OK | OK | **ERR 5** |
| `%USERPROFILE%` · `\.cache` · `%TEMP%` · `%LOCALAPPDATA%` | OK | OK | OK | OK | OK |
| **`C:\<dir the user made>`** | OK | OK | OK | OK | **OK** |
| **`C:\ProgramData\<dir the user made>`** | OK | OK | OK | OK | **OK** |

Elevated is the control: it succeeds on the `C:\` / `C:\Users` / `C:\ProgramData` DACL writes the
de-elevated pass refuses, so the refusal is privilege and not a bad path. It fails identically on
`C:\Program Files` and `C:\Windows` (owned by `TrustedInstaller`), so elevation is not a universal
key either.

**The rule the table states:** what an unprivileged caller may ACE is what it OWNS. That is its
whole profile — and, decisively, **anything it creates**, including directly under `C:\` and
`C:\ProgramData`, where `mkdir` succeeds and the creator owns the result.

## 2. The arms

One AppContainer profile, one sid, one installed ACE set backs all four broad arms; the DACL is
read back before each launch and is byte-identical across them. The only variable between
`ac-broad-net` and `ac-broad-nonet` is the capability array. Each launch's token is read back
through the child's process handle:

| arm | isAppContainer | capabilities | integrity |
| --- | --- | --- | --- |
| `plain-elev` | 0 | `[]` | `S-1-16-12288` (high) |
| `no-token` | 0 | `[]` | `S-1-16-8192` (medium) |
| `ac-bare` | 1 | `[]` | `S-1-16-4096` (low) |
| `ac-broad-net` | 1 | **`[S-1-15-3-1]`** | `S-1-16-4096` |
| `ac-broad-nonet` | 1 | `[]` | `S-1-16-4096` |
| `ac-broad-{net,nonet}-dv` | 1 | `[S-1-15-3-1]` / `[]` | `S-1-16-4096` |

## 3. What keeping the token costs — `no-token` vs `ac-broad-nonet`

Only the rows that differ. Everything else — the whole profile, `%TEMP%`, `%LOCALAPPDATA%`,
`C:\Windows\Temp`, any user-made directory anywhere, reads and listings of `C:\Program Files` and
`C:\Windows` — is identical in the two arms.

| location | op | `no-token` | `ac-broad-nonet` |
| --- | --- | --- | --- |
| `C:\` | mkdir · read · list | OK | `EPERM` |
| `C:\ProgramData` | create · mkdir · read · list | OK | `EPERM` |
| `C:\Users` | read · list | OK | `EPERM` |
| `C:\Users\Public` | create · mkdir · read · list | OK | `EPERM` |
| **network** | tcp · loopback · dns · https | **OK** | **denied** |

`C:\` file create and `C:\Users` create/mkdir are `EPERM` in **both** arms — the token is not what
denies those, privilege is.

## 4. Egress is fully separable, and loopback closes with it

Same sid, same ACEs, one capability apart:

| | `ac-broad-net` | `ac-broad-nonet` |
| --- | --- | --- |
| `connect 1.1.1.1:443` | OK | `EACCES` |
| `connect 127.0.0.1:135` | **`ETIMEDOUT`** | `ETIMEDOUT` |
| `dns.lookup` | OK | `ENOTFOUND` |
| `https.get registry.npmjs.org` | `200` | `ENOTFOUND` |

Loopback is refused **even with `internetClient`**, so relaying through a helper listening outside
the container is not an escape from the no-capability arm. `no-token` and `plain-elev` reach
loopback normally, which is what makes the refusal the AppContainer's doing.

## 5. Reads are NOT free under LowBox

A deep read *through* un-ACE'd ancestors works (bypass-traverse, MECHANISM-FACTS §5h) but a target
read does not. Every confined arm is `EPERM` reading a file in `C:\`, `C:\ProgramData`, `C:\Users`
or `C:\Users\Public`, and `EPERM` listing them. `C:\Program Files` and `C:\Windows` read and list
fine in every confined arm — those two carry `ALL APPLICATION PACKAGES:(OI)(CI)(RX)` and the four
above do not.

## 6. An AppContainer child is LOW integrity and writes to unlabeled objects anyway

`childtoken:integrity = S-1-16-4096` on every confined arm, including the ones launched from a
high-IL parent — LowBox forces Low. The same child creates files in `%USERPROFILE%`, which carries
no mandatory label and is therefore implicitly Medium. A restricted low-IL token is refused exactly
that write (MECHANISM-FACTS §5e), so the mandatory write-up barrier does not bind an AppContainer
the way it binds a restricted token. **Mechanism NOT established here** — recorded as measured
behaviour, and not to be reasoned from.

## Limits of this run

- **`C:\Windows\Temp` being DACL-writable de-elevated is surprising and is measured only on the two
  GitHub images.** It is identical on both, but they share a build pipeline, so this is not
  evidence about a stock Windows install. Nothing in the verdict depends on it: it is one extra
  grantable location, and removing it changes no row above.
- Only `C:` was tested. Another volume's root may have a different owner.
- The `-dv` arms show the answer is not an artefact of the runner's elevation (an AppContainer
  launched from a de-elevated medium-IL base via `CreateProcessAsUserW` produces a cell-for-cell
  identical table), but every arm still ran as `runneradmin` — a *de-elevated admin account*, not a
  separate standard-user account. `HKCU` and `%LOCALAPPDATA%` are still an admin's profile.
- Cost of the broad grant is measured only on a runner profile (5,779 entries → `%USERPROFILE%`
  grant 752–1,476 ms). MECHANISM-FACTS §5i has the slope; this run does not re-derive it.
- The first canary in each location is planted by the **elevated** parent. Inside a user-made
  directory under `C:\` that produces an Administrators-owned file the de-elevated grantor cannot
  re-ACL, so the propagating grant skips it and the confined read comes back `EPERM` — which reads
  as confinement and is not. The `read2` row (a canary the de-elevated user planted itself) is `OK`
  in the same arm at the same path, and the DACL read-back confirms it directly: `0x00000000` on
  the elevated-planted file, `0x001301bf` on the user-planted one.
