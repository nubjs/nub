# Windows — blocking egress for a NON-AppContainer child, without elevation

**The question.** nub's Windows build jail confines a lifecycle script inside an AppContainer (LowBox) token, where egress is withheld by not granting the `internetClient` capability. When a package needs full disk access (`write:"disk"`), the AppContainer allowlist has no spelling for "the whole filesystem", so [`backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs) declines the LowBox token entirely and runs the child as a plain process. Because egress is an *AppContainer capability*, declining the token declines the network axis with it: 52 of 96 full-disk Windows packages get unconfined network they were never granted.

So: **with no AppContainer and no elevation, can a parent launch a child that has full filesystem access but cannot make outbound connections?** Coarse on/off is enough; per-host is not needed; losing loopback is acceptable.

This is a different question from [`sandbox-windows-net-parity.md`](sandbox-windows-net-parity.md), which asks about *per-host* policy and concludes that needs an admin-gated loopback exemption or admin WFP. Nothing here contradicts that document; it answers a question that one did not ask.

---

## Verdict

**No — there is no unprivileged mechanism that truly blocks all egress for a non-AppContainer child. But one mechanism gets most of the way there and is worth shipping.**

| | |
| --- | --- |
| **A true, total egress block** | **Not available.** Every candidate is either admin-gated, or structurally incompatible with keeping full filesystem access. |
| **A strong partial block** | **Available and unprivileged:** a job object with `JobObjectNetRateControlInformation` set to `MaxBandwidth = 1` byte/sec. Measured: the child's own TCP connections to external hosts do not establish, its UDP does not get out, surrogate processes are covered, the child cannot escape the job, and full filesystem access is untouched. |
| **What that partial block leaks** | Two named channels: **DNS resolution through the `dnscache` service still works** (a full bidirectional covert channel), and **loopback is unaffected**. It is also starvation-based rather than a policy deny, so a patient sender can dribble bytes. |

The honest label for the job-object mechanism is **"blocks the child's own outbound sockets"**, not "blocks all network egress". It is a large improvement on today's *unconfined* state and should be reported to users as exactly what it is.

---

## How this was measured

Everything below was run on `nub-win2` (Windows Server 2022 Datacenter, 21H2, build 10.0.20348).

**The measurement account is a genuine standard user.** The VM's `nub` account is a **full administrator with a High-integrity, already-elevated token** (`IsInRole(Administrator) = True`, `SeDebugPrivilege`/`SeTakeOwnershipPrivilege`/`SeLoadDriverPrivilege` all enabled) — not a valid context for this question. A separate local user `nubstd` was created, confirmed absent from the Administrators group, and every result below was produced as `NUB-WIN2\nubstd` at Medium integrity. Where a *tool* needed admin (reading an adapter binding), that is called out as an instrument, never as the subject.

**Every arm has a positive control.** The harness is a C# launcher/child (`Jail.cs`) compiled in-box with `csc.exe`, driving native probes (`cmd.exe`, `curl.exe`) so results are not an artifact of the CLR. A `plain` arm — same launcher, same child, no mechanism — is run alongside each measurement and must show egress *working*. It does: `NET-TCP-8.8.8.8:53=OK`, `NET-TCP-1.1.1.1:443=OK`, `curl http://1.1.1.1/ -> 301`, `NET-UDP-SENDTO=OK`, DNS resolving.

**Uncacheable DNS test.** DNS claims use the `nip.io` wildcard, which resolves `<label>.9.8.7.6.nip.io` to `9.8.7.6`. A freshly-generated random label cannot be answered from cache, so a correct answer proves the query reached a recursive resolver on the wire and came back.

---

## The structural fact everything else follows from

Dumping the real security descriptors of the NT device objects (via `NtOpenFile` + `GetSecurityInfo`, as `nubstd`) is what settles this question:

```
\Device\Afd     O:BAG:SYD:(A;;0x1201bf;;;WD)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;RC)S:AI(ML;;NW;;;LW)
\Device\Null    O:BAG:SYD:(A;;0x1201bf;;;WD)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;RC)S:AI(ML;;NW;;;LW)
\Device\KsecDD  O:BAG:SYD:(A;;0x1201bf;;;WD)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;RC)(A;;0x1201bf;;;AC)(A;;0x1201bf;;;S-1-15-2-2)S:(ML;;NW;;;LW)
\Device\CNG     O:BAG:SYD:(A;;0x1201bf;;;WD)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;RC)(A;;0x1201bf;;;AC)(A;;0x1201bf;;;S-1-15-2-2)S:(ML;;NW;;;LW)
```

Reading the ACEs that matter:

- **`WD` (Everyone)** gets `0x1201bf`, which includes `FILE_WRITE_DATA`. Creating a socket opens `\Device\Afd\Endpoint` for read **and write**, so Everyone is what lets an ordinary user make sockets.
- **`RC` (`RESTRICTED`, S-1-5-12)** gets `0x1200a9` — read, execute, read-attributes, **but no `FILE_WRITE_DATA`**. Microsoft deliberately gives restricted tokens read-only AFD access.
- **There is no `BUILTIN\Users` ACE and no `Authenticated Users` ACE on any of them.**
- The label is `(ML;;NW;;;LW)` — **Low** integrity, no-write-up.

That last group of facts is the whole story. The **filesystem** grants ordinary users through `BUILTIN\Users`, `Authenticated Users`, `CREATOR OWNER` and the user's own SID (measured: `C:\` is `BUILTIN\Users:(OI)(CI)(RX)` + `(CI)(AD)` + `(CI)(IO)(WD)`; `C:\Windows` adds `ALL APPLICATION PACKAGES:(RX)`). The **device namespace** grants ordinary users *only* through `Everyone`. So a token-based mechanism that removes `Everyone` in order to kill AFD also kills `\Device\Null`, `\Device\KsecDD`, `\Device\CNG` and the console — because they share AFD's DACL byte for byte.

---

## Mechanisms tried

### 1. Restricted tokens — `CreateRestrictedToken` with restricting SIDs

**What it is.** A restricted token makes the kernel run the access check twice: once against the token's normal groups, once against *only* the restricting SID list. Access needs both to pass. Needs no privilege, and `CreateProcessAsUser` does not need `SeAssignPrimaryTokenPrivilege` when the token is a restricted version of the caller's own.

**Elevation:** none required — verified by running it as `nubstd`.

**Result: disqualified.** The theory is sound and the DACL gap above is real — restricting to `{Users, Authenticated Users, <user SID>, <logon SID>}` excludes `Everyone` and therefore excludes AFD. But the child cannot start:

| restricting SID set | child (`cmd.exe /c echo`) |
| --- | --- |
| `plain` (control) | starts, egress works |
| `nopriv` (`DISABLE_MAX_PRIVILEGE`) alone | starts |
| `inert` (`SANDBOX_INERT`) alone | starts |
| `{Everyone}` | starts, egress works |
| `{Everyone, Users, AuthUsers, user, logon}` | starts, egress works |
| `{Users, AuthUsers, user, logon}` — no `Everyone` | **`0xC0000022` STATUS_ACCESS_DENIED at init** |
| deny-only `{Everyone}` | **`0xC0000022`** |

The logon SID was added to every restricting set (so a window-station failure could not be misread as a network result), and `SANDBOX_INERT` (the documented AppLocker/SRP escape hatch) was tried. Neither helps. Process startup itself needs write access to objects granted only via `Everyone`.

### 2. Write-restricted tokens — `CreateRestrictedToken(WRITE_RESTRICTED)`

**What it is.** The `WRITE_RESTRICTED` flag applies the second access check **only to write requests**. That is exactly the shape this problem wants: reads stay unrestricted so process startup works, while the AFD open — which needs `FILE_WRITE_DATA` — gets checked against a SID set that excludes `Everyone`.

**Elevation:** none required.

**Result: disqualified, and this is the closest any token mechanism came.** It fixes the *first* failure — a write-restricted token without `Everyone` now starts `cmd.exe` — but a real executable still dies:

| child | write-restricted, no `Everyone` |
| --- | --- |
| `cmd.exe /c echo HELLO` (builtin only, no new process) | **works** |
| `cmd.exe /c whoami` | `0xC0000142` STATUS_DLL_INIT_FAILED |
| `curl.exe` directly | `0xC0000142` |
| `cmd.exe /c <batch file>` | exit 1, no output |
| `cmd.exe /c type <file> > NUL` | `Access is denied.` — `\Device\Null` needs write, and its DACL is AFD's |
| the .NET probe | `0xC06D007E`, CLR fails to load |
| `wrestrict:{Everyone}` (control) | **works, `curl -> 301`** |

Also tried and did not rescue it: adding the logon SID, `SANDBOX_INERT`, `DETACHED_PROCESS` (to rule out the inherited console), and granting the restricting SIDs full access on the process window station and desktop via `SetUserObjectSecurity` (`GRANTUI=OK` for every SID, child still `0xC0000142`).

The reason is the structural fact: any executable that loads DLLs and touches `\Device\Null`, `KsecDD` or `CNG` needs device writes, and those are granted to a normal user only through `Everyone` — the same ACE that re-opens AFD. There is no SID that separates "the network device" from "the other devices a process needs to run".

### 3. Integrity level — `SetTokenInformation(TokenIntegrityLevel)`

**What it is.** AFD carries `S:AI(ML;;NW;;;LW)` — a Low label with no-write-up. A process below Low cannot write to it.

**Elevation:** none required (lowering your own IL is always allowed).

**Result: disqualified — it is exactly backwards.** Measured:

| level | filesystem | network |
| --- | --- | --- |
| `il:low` (S-1-16-4096) | **all writes DENIED** (scratch, `%USERPROFILE%`, `%TEMP%`, `C:\ProgramData`); reads OK | **fully works** — `curl -> 301`, DNS resolves, surrogate `curl -> 301` |
| `il:untrusted` (S-1-16-0) | child cannot run at all (exit 1, no output) | n/a |

Low IL *equals* AFD's label, so write-up is permitted and the network is untouched; meanwhile ordinary files have no explicit label, so their implicit Medium label blocks every write. The integrity axis removes filesystem access strictly before it removes network access. Turning the token's mandatory policy off (`TOKEN_MANDATORY_POLICY_OFF`) would restore filesystem writes but disables the IL check entirely, re-opening AFD — self-defeating.

*(Harness note: with `il:` alone the launcher was initially lowering **its own** token, killing itself. Fixed by duplicating the token first; the numbers above are post-fix.)*

### 4. Job object — `JobObjectSecurityLimitInformation`

**What it is.** `JOBOBJECT_SECURITY_LIMIT_INFORMATION` would apply restricted-token semantics to *every* process in a job — which would have solved surrogate spawn elegantly.

**Result: disqualified — removed from modern Windows.** `SetInformationJobObject(JobObjectSecurityLimitInformation)` returns **`GLE=50` (`ERROR_NOT_SUPPORTED`)** as a standard user on Server 2022. Measured, not inferred.

### 5. Job object — `JobObjectNetRateControlInformation` ← **the one that works**

**What it is.** `JOBOBJECT_NET_RATE_CONTROL_INFORMATION` sets `MaxBandwidth`, documented as "the maximum bandwidth for **outgoing** network traffic for the job, in bytes". Windows 10 / Server 2016+.

**Elevation:** **none.** `SetInformationJobObject` returned success as `nubstd` (`JOB-NETRATE=SET bps=1`, `JOB-ASSIGN=OK`).

**Result — with `MaxBandwidth = 1` byte/sec:**

| axis | outcome |
| --- | --- |
| **Filesystem** | **fully preserved.** Read System32, list `C:\`, write scratch / `%USERPROFILE%` / `%TEMP%` / `C:\ProgramData` — all OK, identical to the unconfined control. |
| TCP to external hosts | **does not establish.** `8.8.8.8:53` and `1.1.1.1:443` both time out; `curl -m 60` fails with `Failed to connect to 1.1.1.1 port 80 after 21032 ms: Could not connect to server`. |
| UDP egress | blocked — `nslookup` (which sends its own UDP) gets `No response from server`. |
| HTTP/HTTPS | `curl` returns `000` on both. |
| **Surrogate spawn** | **covered.** A grandchild `cmd.exe /c curl.exe` is blocked too. |
| **Escape** | **blocked, by design.** See below. |
| Loopback | **NOT blocked** — `127.0.0.1:135` connects and accepts a 64-byte send. |
| **DNS via `dnscache`** | **NOT blocked** — see the leak section. |

**Dose-response proves causation.** The block is caused by the bandwidth value, not by the presence of a job or by anything incidental:

| `MaxBandwidth` | `curl http://1.1.1.1/` |
| --- | --- |
| `0` | **301 in 0.036 s** — zero means *unlimited*, not blocked |
| `1000` (1 KB/s) | 301 in 0.126 s |
| `100000` | 301 in 0.025 s |
| `10000000` | 301 in 0.029 s |
| **`1`** | **fails to connect** |

**Escape resistance, with a control.** The confined child was given code that actively tries to shed the limit. Both orderings fail, and the `plain` control proves the failure is caused by the mechanism rather than being an intrinsic API limitation:

| attempt | under `netrate:1` | under `plain` (control) |
| --- | --- | --- |
| create own job, set high rate, assign self | `set=OK assign=FAILED GLE=50` | `set=OK assign=OK` |
| create own job, assign self, then set high rate | `assign=OK set=FAILED GLE=50` | `set=FAILED GLE=50` |
| spawn child with `CREATE_BREAKAWAY_FROM_JOB` | `FAILED GLE=5` | `FAILED GLE=5` |
| TCP connect after all of the above | `TIMEOUT` | `OK connected` |

`ERROR_NOT_SUPPORTED` here is the documented rule doing the work: *"You can only set the control of the network traffic on one job in a hierarchy of nested jobs, and the settings that you specify apply to that job and the child jobs in the hierarchy."* An inner job cannot override an outer one, and breakaway is already refused because nub's existing job sets `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`.

### 6. AppContainer with unrestricted filesystem

The brief flagged this as the most promising direction. It was tested directly, and it is **structurally closed**.

A real LowBox child was launched via `CreateAppContainerProfile` + `UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES)`, as `nubstd`:

| test | result |
| --- | --- |
| AC reads `C:\probe\probe.bat` — DACL grants `Users:F`, **`Everyone:F`**, and the user's own SID, but no AppContainer ACE | **`Access is denied.`** |
| AC reads `C:\Windows\System32\drivers\etc\hosts` — inherits `ALL APPLICATION PACKAGES:(RX)` (positive control) | **file contents printed** |
| AC egress, no capabilities | `Failed to connect to 1.1.1.1 port 80 after 0 ms` — instant hard denial |
| AC egress, with `internetClient` (S-1-15-3-1) | `HTTP=301` |

The first row is decisive: **`Everyone` does not satisfy the AppContainer access check.** A LowBox token reaches an object only where that object's own ACL names the package SID, a capability SID, or `ALL APPLICATION PACKAGES`. Being granted to Everyone, to Users, or to the invoking user's own SID buys an AppContainer nothing. So "give the AppContainer effectively unrestricted filesystem access by a route the user is already entitled to" has no implementation — there is no group membership, capability, privilege or token attribute that substitutes for the ACE. It would require an `ALL APPLICATION PACKAGES` or per-run-AC-SID ACE propagated across the whole volume, which is precisely what [`backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs) already rejects as a filesystem-wide write hole. The existing decision to decline the token stands.

### 7. Mechanisms disqualified on elevation — verified empirically, not from docs

The brief asked twice for the privilege requirement to be established by running it as a standard user. As `nubstd`:

| mechanism | measured result |
| --- | --- |
| Windows Firewall rule (`netsh advfirewall firewall add rule`) | `The requested operation requires elevation (Run as administrator).` |
| Windows Firewall rule (`New-NetFirewallRule`) | `Cannot connect to CIM server. Access denied` |
| **WFP filter add** | `FwpmEngineOpen0` succeeds (`0x00000000`) — the engine opens read-only — but **`FwpmTransactionBegin0` returns `0x00000005` (`ERROR_ACCESS_DENIED`)**, and a transaction is required to add a filter. |
| Winsock catalog / LSP (`HKLM\SYSTEM\CurrentControlSet\Services\WinSock2\Parameters`) | `Requested registry access is not allowed.` |
| **`CheckNetIsolation LoopbackExempt -a`** | **`Error: Access Denied, run the command as an administrator`** |

That last row closes a gap [`sandbox-windows-net-parity.md`](sandbox-windows-net-parity.md) §3a explicitly admitted: it grounded the admin requirement in docs and the API contract because "the write path `-a` returns before the ACL check on an unregistered name, so admin-denial wasn't reproduced against a real profile." It is now reproduced.

### 8. Mechanisms disqualified on inspection

- **`SetProcessMitigationPolicy`.** The `PROCESS_MITIGATION_POLICY` enumeration has 20 values (DEP, ASLR, dynamic code, strict handle check, system call disable, mitigation options mask, extension point disable, CFG, signature, font disable, image load, system call filter, payload restriction, child process, side channel isolation, user shadow stack, redirection trust, user pointer auth, SEHOP, activation context trust). **None is network-related.** The nearest miss, `ProcessImageLoadPolicy`, blocks *loading images* from remote devices — not egress.
- **Proxy environment variables** (`HTTP_PROXY` and friends pointed at a dead port). Cooperative only. A package that opens a socket directly ignores them entirely, and the threat model here is a package that does not cooperate. Not a boundary; adds nothing on top of the job object.
- **DLL injection / API hooking of `ws2_32`.** In-process, so trivially bypassable by anything issuing syscalls directly, and the machine-wide hooks (`AppInit_DLLs`, IFEO) are both admin-gated and machine-wide persistent state — two hard constraints violated at once.
- **Server silos / Windows Sandbox.** `NtCreateServerSilo` needs `SeTcbPrivilege`; Windows Sandbox needs Hyper-V and admin.

### 9. What comparable sandboxes do

Consistent with the above, and with `sandbox-windows-net-parity.md` §4: nobody ships unprivileged network confinement on Windows. Chromium's renderer combines a lockdown restricted token, Untrusted integrity and an AppContainer — it can afford the restricted token precisely because the renderer is a *cooperating* process whose resources are brokered in as pre-opened handles by the browser process, which is the one thing a hostile npm lifecycle script will not do for us. Codex's `windows-sandbox-rs` does write-confinement only when unelevated and needs a one-time UAC for its WFP path; SRT installs a machine-wide WFP filter set under a one-time UAC.

---

## The two leaks in the job-object mechanism

**DNS resolution still works, and it is a full covert channel.** Under `netrate:1`, a freshly-generated name that cannot be cached resolved correctly: `p668db11464a3.9.8.7.6.nip.io -> 9.8.7.6`. The query reached a public recursive resolver and the answer came back. The reason is that the `dnscache` service does the network I/O on the child's behalf, in `svchost` as SYSTEM, **outside the job** — so the job's bandwidth limit never applies to it. Arbitrary data can be encoded into DNS labels outbound and into responses inbound.

**This is a genuine regression against the AppContainer path, and the comparison was measured directly:**

| arm | uncacheable hostname fetch |
| --- | --- |
| AppContainer, no `internetClient` | `curl: (28) **Resolving** timed out` — DNS blocked |
| AppContainer, with `internetClient` | `curl: (28) **Connection** timed out` — DNS resolved |
| **job `netrate:1`** | `curl: (28) **Connection** timed out` — **DNS resolved** |

AppContainer network isolation covers the resolver service; the job-object bandwidth limit does not. There is no unprivileged way to close this: denying the child the `dnscache` RPC endpoint needs the restricted-token avenue (mechanism 1, dead), and changing DNS configuration is machine-wide and admin-gated.

**Loopback is unaffected.** `127.0.0.1:135` connects and accepts data under `netrate:1`. Any local listener — a Docker daemon, a database, an SSH agent, an MCP server — remains reachable. For nub this is not a new exposure (a full-disk child today has *everything*), but it must not be described as blocked.

**It is starvation, not a deny.** `MaxBandwidth=1` blocks because a token bucket refilling at 1 byte/sec never accumulates enough for a TCP SYN in any practical window — not because the kernel refuses the operation. Over a long-running build a very patient sender could dribble bytes out. Contrast the AppContainer path, which denies the connect in 0 ms.

---

## Recommended implementation shape

Ship the job-object limit for the full-disk tier, and describe it accurately.

**Where it goes.** [`backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs) already builds a confinement job in `create_confinement_job()` with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS`. This is one additional `SetInformationJobObject` call on that existing handle, gated on `policy.net.enforce && !confine_fs` — the branch that currently returns a `Degradation` reporting the whole net axis lost:

```rust
let mut nrc: JOBOBJECT_NET_RATE_CONTROL_INFORMATION = zeroed();
nrc.MaxBandwidth = 1;
nrc.ControlFlags = JOB_OBJECT_NET_RATE_CONTROL_ENABLE | JOB_OBJECT_NET_RATE_CONTROL_MAX_BANDWIDTH;
SetInformationJobObject(job, JobObjectNetRateControlInformation, ..., size_of::<...>());
```

The full-disk branch must stop taking the early `plain_command` return that skips the job entirely, and instead keep the job (it already has the process-count ceiling) while declining only the LowBox token.

**Three things that must come with it:**

1. **Assign the job before the child runs.** The existing pattern — `CREATE_SUSPENDED`, `AssignProcessToJobObject`, `ResumeThread` — is already correct and closes the race. Better still, `PROC_THREAD_ATTRIBUTE_JOB_LIST` assigns at creation.
2. **Verify enforcement rather than assuming it.** The limiter rides the QoS Packet Scheduler (`ms_pacer`, confirmed bound and enabled on the test host, and on by default). If it were ever unbound, `SetInformationJobObject` could plausibly still return success while nothing is enforced — the exact silent-no-op shape this repo keeps getting burned by. **This was not tested with `ms_pacer` unbound.** Either probe enforcement once at launch or degrade honestly if it cannot be confirmed.
3. **Report it honestly.** The `Degradation` message must change from "network access is not confined" to something that names what holds and what does not: outbound sockets are blocked, **DNS resolution and loopback are not**. Do not let this be labelled a coarse egress-deny equal to the AppContainer path — it is not, and the DNS comparison above is the proof.

**What it does not change.** Packages that do *not* need full disk keep the AppContainer path and its genuine, instant, DNS-covering egress deny. Nothing here weakens that. A true block for the full-disk tier still requires the elevated, dedicated-account + WFP route already described in [`sandbox-windows-net-parity.md`](sandbox-windows-net-parity.md) §4 and implemented in [`backend/windows_account`](../../crates/nub-sandbox/src/backend/windows_account/mod.rs).

---

## Sources

- [JOBOBJECT_NET_RATE_CONTROL_INFORMATION — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_net_rate_control_information) — `MaxBandwidth` is outgoing traffic in bytes; only one job in a nested hierarchy may set net rate control, and it applies to that job and its child jobs.
- [PROCESS_MITIGATION_POLICY — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-process_mitigation_policy) — the complete 20-value enumeration; no network policy exists.
- [CreateRestrictedToken — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-createrestrictedtoken) — two-pass access check; `WRITE_RESTRICTED` applies the restricting-SID check to write requests only.
- [Chromium sandbox design](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/docs/design/sandbox.md) — restricted token + Untrusted integrity + AppContainer; resources are brokered in as pre-opened handles by the browser process.
- [Understanding Network Access in Windows AppContainers — Google Project Zero](https://projectzero.google/2021/08/understanding-network-access-windows-app.html) — AppContainer egress is capability-coarse; loopback is blocked by a separate `IsLoopback` mechanism.
- Prior nub research: [`sandbox-windows-net-parity.md`](sandbox-windows-net-parity.md) — the per-host question and the elevated loopback exemption.
- Code: [`crates/nub-sandbox/src/backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs) (`create_confinement_job`, the full-disk tier branch), [`crates/nub-sandbox/src/backend/windows_account/wfp.rs`](../../crates/nub-sandbox/src/backend/windows_account/wfp.rs).
- Empirical: `nub-win2`, Windows Server 2022 21H2 build 10.0.20348, standard user `NUB-WIN2\nubstd` (verified non-administrator), C# harness `Jail.cs` compiled in-box, native `cmd.exe`/`curl.exe` probes, every arm run against a `plain` positive control.

## Changelog

- 2026-08-05 — Initial write-up. Answers a question [`sandbox-windows-net-parity.md`](sandbox-windows-net-parity.md) did not ask: a *total* egress block for a non-AppContainer child, unprivileged. Verdict: no true block exists, but `JobObjectNetRateControlInformation` at `MaxBandwidth=1` blocks the child's own outbound sockets, survives surrogate spawn, resists escape, and preserves full filesystem access — leaking DNS-via-`dnscache` and loopback. Establishes the structural reason the token avenues fail (`\Device\Afd`, `\Device\Null`, `\Device\KsecDD` and `\Device\CNG` share one DACL that grants ordinary users only through `Everyone`, while the filesystem grants through `Users`/`Authenticated Users`/user SID). Confirms empirically that `Everyone` does not satisfy the AppContainer access check, closing the "AppContainer with unrestricted filesystem" direction. Reproduces the admin requirement for `CheckNetIsolation LoopbackExempt -a`, which the prior document had grounded only in docs.
