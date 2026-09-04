# `win-ac-loopback` — can two same-package AppContainers talk over loopback at zero privilege?

A branch-scoped CI probe (no pull request; see the `ci-adhoc-test` skill). It measures one thing, on real Windows, with controls: **W1**.

## The question

Withholding the `internetClient` capability from an AppContainer is the only unprivileged Windows egress lever, and it denies **loopback** as well as the internet ([`.frizz/sandbox-MECHANISM-FACTS.md`](../../.frizz/sandbox-MECHANISM-FACTS.md) §5l·4). That is what stops a nub-owned filtering proxy from ever being reachable from inside the jail — and therefore what keeps Windows at coarse on/off egress while macOS and Linux can do per-host.

James Forshaw's [*Understanding Network Access in Windows AppContainers*](https://googleprojectzero.blogspot.com/2021/08/understanding-network-access-windows-app.html) (Project Zero, 2021) describes the AppContainer loopback block as a WFP filter at `FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4/V6` keyed on `IsLoopback`, sitting beside a **permit** filter keyed on `IsAppContainerLoopback` — a condition that, on his testing, is set only when both endpoints share a **package SID**.

**W1**: does that permit fire for two AppContainer processes bearing the same nub-derived package SID, with no capability, no `CheckNetIsolation LoopbackExempt`, and no elevation anywhere?

**W2** rides along as two more rows: Forshaw's loopback-exemption capability SID — the package SID with its first RID changed from `2` to `3`.

### Why the existing data does not already answer it

§5l·4 measured `connect 127.0.0.1:135` → `ETIMEDOUT` from an AppContainer both **with** and **without** `internetClient`, while a real outbound denial (`connect 1.1.1.1:443`, no capability) was `EACCES`. Two different errors mean two different layers: the loopback failure is a **receive-side drop**, not an outbound denial. But the listener in that measurement was the ordinary RPC endpoint mapper — a **non**-AppContainer process, which is exactly the cell `IsAppContainerLoopback` can never fire for. The AppContainer-to-same-package-AppContainer cell has never been run.

## What runs

One de-elevated context creates two AppContainer profiles (two package SIDs), grants read+execute on a stage directory under `%USERPROFILE%`, and runs a table of two-child arms — one listener, one connector, one variable apart:

| arm | listener | connector | expected |
| --- | --- | --- | --- |
| `d-plain-to-plain` | plain | plain | **CONNECT** — positive control |
| `c-plain-to-ac` | plain | AC P1, no caps | **BLOCK** — negative control, reproduces §5l·4 |
| `b-diff-package-sid` | AC P1, no caps | AC P2, no caps | **BLOCK** |
| `a-same-package-sid` | AC P1, no caps | AC P1, no caps | **the question** |
| `a2-same-sid-conn-netcap` | AC P1, no caps | AC P1, `internetClient` | does the capability change loopback? |
| `a3-same-sid-listener-srvcap` | AC P1, `privateNetworkClientServer` | AC P1, no caps | fallback if a zero-cap `bind` cannot listen |
| `e-w2-selfcap-to-plain` | plain | AC P2, cap derived from P2 | W2, own package |
| `f-w2-peercap-cross-sid` | AC P1, no caps | AC P2, cap derived from P1 | W2, naming the peer |

`a` and `b` differ by exactly one thing: the listener's package SID. Same program, same base token, same grants, same (empty) capability set.

## What makes the answer trustworthy

A table of failures looks like confinement and is indistinguishable from a broken harness. These are the gates that separate the two, and every one of them prints a `CONTROL … PASS/FAIL` line:

- **Positive control** (`d`) — plain listener, plain connector, same de-elevated base. It must connect.
- **Negative control** (`c`) — plain listener, AppContainer connector. It must fail, reproducing §5l·4 on this image rather than importing it.
- **Egress gate live** — a zero-capability AppContainer child must not reach `1.1.1.1:443`, in this same run. Without it, a connecting arm `a` could be explained by "the AppContainer attribute was never applied". The unconfined half of that pair is informational only: a CI runner may sit behind its own egress filter, which says nothing about the token.
- **Child-token read-back, twice per launch.** The launcher reads `TokenIsAppContainer` / `TokenAppContainerSid` / `TokenCapabilities` / `TokenIntegrityLevel` off the child's process handle, and the child reads the same four values **from inside itself**. §5i: `tests/win-bypass-traverse/launcher.ps1` declared a capability parameter and passed `CapabilityCount = 0`, so every arm it ever ran was a zero-capability arm and nothing in its output could have said so.
- **No elevation, asserted by access check.** `CreateRestrictedToken` *copies* `TokenIsElevated` (§5h, run 30423750288), so that flag still reads `1` on a de-elevated token. `CheckTokenMembership(NULL, Administrators)` is the only honest assertion, and profiles and ACEs are both written under that impersonation.
- **No `LoopbackExempt`.** `CheckNetIsolation LoopbackExempt -s` is captured before and after and searched for both package SIDs, so "the admin exemption was not used" is evidence rather than a claim.
- **Sequencing on the listener's own ready line**, never a sleep — a connect fired before `listen()` fails for a reason that has nothing to do with the token.
- **Two bounds on every connect** (5 s, then 25 s more). An outbound capability denial returns in single-digit ms; a receive-side drop leaves the SYN unanswered until Windows' TCP retry budget runs out (~21 s). A single 5 s ceiling reports both as "no completion" and erases the very distinction the finding rests on.

`FAILURES = 0` in the log means the table can be read. **A `BLOCKED` arm is a result, not a job failure** — the job always exits 0 and only the `CONTROL` lines decide whether the arm table means anything.

## Files

| file | what it is |
| --- | --- |
| [`probe.ps1`](probe.ps1) | the orchestrator: profiles, ACEs, viability ladder, egress gate, arm table, controls, verdict |
| [`launcher.ps1`](launcher.ps1) | C# P/Invoke: profile create/derive/delete, de-elevated medium-IL token, async LowBox `Start`/`Wait`/`Kill`, child-token read-back, ACE write/read-back |
| [`child.cs`](child.cs) | the child, compiled on the runner by the .NET Framework `csc.exe`. Reports its own token and the raw Winsock number (`SocketException.ErrorCode`) |
| [`child.js`](child.js) | node fallback for the same three modes, used only if `child.exe` cannot run confined. Proven runtime, but it cannot read its own token and libuv hides the WSA number |
| [`results.md`](results.md) | the measured answer |

Both children are run when both are viable: the decisive cells (`a`, `b`, `c`, `d`) are measured twice, once through each runtime, so the answer does not rest on one implementation's error handling.

## Running it

```sh
git push origin probe/win-ac-loopback          # the push IS the trigger
gh run list --workflow win-ac-loopback-probe.yml --branch probe/win-ac-loopback
gh run view <id> --log                          # or download the acloop-* artifact
```

Re-run with `git commit --allow-empty -m rerun && git push`, or `gh run rerun <id>`.

CI is the only venue: an AppContainer cannot be launched over OpenSSH, because sshd lands in services session 0, which has no window station a LowBox token can attach to — every launch returns `0xC0000142` (§5e/§5h). The standing `nub-win` VM cannot answer this.
