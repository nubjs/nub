# Sandbox — Windows per-host net + MITM parity (Q21 / Q22)

**Historical question (maintainer).** Per-host network policy (Q21) and the U5 MITM/credential-brokering tier (Q22) were wired on macOS + Linux but unavailable on Windows. This document established the AppContainer loopback limitation, then recorded the elevated exemption implementation. The current correction below supersedes the earlier parity claim while preserving that mechanism history.

**Current verdict (2026-07-13).** The elevated exemption makes Nub's proxy reachable and can block direct public egress by withholding `internetClient`, but it grants the child access to **every** loopback listener. A local forwarder can relay around the hostname policy, so the implementation is not a sole-egress per-host boundary and must not be reported as parity. Complete enforcement needs port-scoped WFP or an equivalent trusted boundary. Separately, current `net: true` grants only `internetClient`, which omits localhost, private-LAN access, and server/listener capability; it is not full host networking.

---

## 1. How per-host net (and MITM) work on macOS + Linux today

The mechanism is identical on both: **force all child egress through a loopback proxy that owns the per-host and MITM decision.** Code: [`crates/nub-sandbox/src/proxy/mod.rs`](../../crates/nub-sandbox/src/proxy/mod.rs), the backends under `crates/nub-sandbox/src/backend/`.

- **The proxy** ([`proxy/mod.rs`](../../crates/nub-sandbox/src/proxy/mod.rs)) binds `127.0.0.1:<port>` in the nub **parent** process, speaks HTTP `CONNECT` + SOCKS5, and enforces per-host policy in two gates (CONNECT/SOCKS target host, then the TLS SNI read in the clear). It is started in `backend::apply` and stashed on `Prepared` so it outlives the child. MITM (U5, [`proxy/mitm.rs`](../../crates/nub-sandbox/src/proxy/mitm.rs) on the `mitm-tier` branch) is a *tier of this same proxy*: for a brokered/terminated host it terminates TLS with an ephemeral CA the child trusts, injects the real credential, and re-originates a verified upstream TLS leg. No new transport — it rides the exact same loopback tunnel.
- **The OS deny-layer makes the proxy the *sole* egress** — this is the load-bearing part. The child can reach `127.0.0.1:<port>` and **nothing else**; direct external egress is blocked in the kernel, so a malicious client cannot bypass the proxy.
  - **macOS** ([`backend/macos.rs`](../../crates/nub-sandbox/src/backend/macos.rs) `emit_net`): the Seatbelt profile is `(deny default)` + exactly `(allow network* (remote ip "localhost:<port>"))`. Only the proxy port is reachable; all other egress (incl. other loopback services and AF_UNIX) is denied.
  - **Linux** ([`backend/linux.rs`](../../crates/nub-sandbox/src/backend/linux.rs) `NetMode::Proxy`): seccomp denies `AF_INET` datagram/raw + non-TCP; Landlock ABI-v4 `ConnectTcp` pins `connect()` to the proxy port; and a seccomp `USER_NOTIF` `connect()` supervisor ([`backend/linux_connect_notify.rs`](../../crates/nub-sandbox/src/backend/linux_connect_notify.rs), thread `sandbox-linux-net-usernotify`) permits only `127.0.0.1:<port>`, closing the port-scoped external-connect residual.
- **The env proxy vars** (`HTTP_PROXY`/`HTTPS_PROXY`/…, `backend::mod::set_proxy_env`) are a *cooperative hint* so ordinary clients route through the proxy — **not** the boundary. The boundary is the OS deny-layer above.

The decisive property: **macOS and Linux can express "allow egress to exactly one loopback address:port, deny everything else" as an UNPRIVILEGED OS primitive.** No admin, no elevation. Per-host and MITM are then pure userspace policy inside the proxy.

---

## 2. Why Windows is different — the confirmed technical limitation

Windows' sandbox primitive is the **AppContainer / LowBox token** ([`backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs), thread `sandbox-windows-backend`, CI-proven on `windows-latest`). Its network model is fundamentally coarser than Seatbelt/Landlock, and it has a loopback-specific block that has no macOS/Linux analog.

### 2a. AppContainer network access is capability-coarse, not address-scoped
AppContainer egress is gated by **capabilities**, which are all-or-nothing address ranges, not per-address rules:
- `internetClient` → outbound to the internet. On a Public network profile this is the WFP "InternetClient Default Rule" permitting **`0.0.0.0`–`255.255.255.255`** — i.e. *everything*.
- `privateNetworkClientServer` → RFC1918 private ranges (only meaningful on a Private profile).

There is **no capability that means "only 127.0.0.1:<port>."** So the macOS/Linux "sole-egress-to-the-proxy" primitive is simply not expressible with AppContainer capabilities. (Source: [Project Zero, *Understanding Network Access in Windows AppContainers*](https://projectzero.google/2021/08/understanding-network-access-windows-app.html).)

### 2b. Loopback is blocked separately, at the firewall layer, regardless of capability
The crux. Windows enforces the AppContainer→localhost block with a **dedicated WFP mechanism** — the `FWP_CONDITION_FLAG_IS_LOOPBACK` condition at the receive/accept layer — that is **independent of the capability checks**. Granting `internetClient` or `privateNetworkClientServer` does **not** open loopback. Quoting the two authoritative reverse-engineerings:

- Project Zero: *"one of the specific restrictions imposed on AppContainer applications is blocking access to localhost … loopback blocking uses a separate mechanism — the `IsLoopback` condition flag — independent of capability checks. Only the admin-only `Add-AppModelLoopbackException` API can grant localhost access."* ([Project Zero](https://projectzero.google/2021/08/understanding-network-access-windows-app.html))
- James Forshaw (Tyranid's Lair): *"the Firewall checks for the capabilities and blocks connecting or accepting sockets"* — the block is architectural, not capability-derived. ([UWP Localhost Network Isolation and Edge](https://www.tiraniddo.dev/2018/07/uwp-localhost-network-isolation-and-edge.html))

So nub's loopback proxy — the entire mechanism per-host and MITM depend on — is **unreachable from the AppContainer child by default.** This is exactly what `backend/windows.rs` documents in the net branch:

> *"an AppContainer child cannot reach a loopback service without a registered loopback exemption (`NetworkIsolationSetAppContainerConfig`) — NOT wired in this phase — so per-host is honestly degraded and the coarse egress-deny (no `internetClient`) holds."*

The MITM path on the `mitm-tier` branch degrades identically and says so: *"the loopback exemption that per-host needs gates the MITM proxy the same way."*

### 2c. Confirmed: this IS the reason both were deferred
- Q21 (`net-per-host`) and Q22 (MITM/U5) are **one blocker, not two.** Both require the child to reach the loopback proxy; the AppContainer loopback block defeats both. Fix the loopback reachability and both light up (MITM additionally needs the ephemeral-CA env bundle, which mac/Linux already inject — see §5).
- The earlier ground-truth research (`sandbox-windows-research`, findings §2) concluded *"nub's userspace proxy is the non-admin per-host path on ALL OSes — Windows is no different."* **That conclusion was wrong on the Windows half, and this doc supersedes it.** It treated "the proxy runs unprivileged in the parent" as sufficient, but missed that the *child cannot reach the parent's loopback proxy* without an admin-gated exemption. Windows *is* different. The implementation hit this the moment the backend tried to wire the proxy — matching the pattern the `sandbox-windows-research` thread already flagged, where a "proven by CI probe" comment turned out never to have been run.

---

## 3. The loopback-exemption mechanism, its cost, and the security tradeoff

Three independent authorities agree on every point below.

### 3a. It requires administrator, and it is machine-wide persistent state
- `CheckNetIsolation LoopbackExempt -a -n=<AppContainer|PackageFamily>` and its programmatic twin `NetworkIsolationSetAppContainerConfig(numPublicAppCs, publicAppCs, …)` both write the firewall service's system-wide exemption list. **Both require admin**; a normal user gets access-denied. ([NetworkIsolationSetAppContainerConfig — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/netfw/nf-netfw-networkisolationsetappcontainerconfig); [Troubleshooting UWP Firewall — Microsoft Learn](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/troubleshooting-uwp-firewall).)
- MS labels the API **"for debugging purposes"** — it is not a supported production sandbox knob. Edge reaches localhost only via a **hardcoded whitelist in the firewall RPC service** (Forshaw) — a proprietary bypass nub cannot use.
- Empirically confirmed on the Windows VM (`nub-win`, standard non-admin user `NUB-WIN\nub`): `CheckNetIsolation LoopbackExempt -s` (read) succeeds as non-admin; the tool exists and the read path is open. (The write path `-a` returns before the ACL check on an unregistered name, so admin-denial wasn't reproduced against a real profile — but the three doc authorities plus the MS API contract settle it: the write is admin-gated.)
- The `Set…Config` API **replaces the entire exemption list** on each call — a naive add clobbers other apps' exemptions unless you read-modify-write. An operational hazard on a shared machine.

### 3b. It widens the sandbox's reach — a genuine security regression
This is the tradeoff the maintainer must weigh. A loopback exemption is **not scoped to nub's proxy port.** Both Forshaw and Project Zero confirm the exemption grants the AppContainer access to **all** localhost resources:

> *"Once exempted, a process can access localhost resources generally … not restricted to specific services."* (Forshaw)

So exempting the child's AC SID to let it reach nub's proxy on `127.0.0.1:<port>` **also** lets that child reach every other loopback listener: a local Docker daemon, a database on `127.0.0.1:5432`, a dev server, an SSH-agent named pipe, an MCP server, etc. For an agent-threat-model sandbox whose *point* is blast-radius containment, opening all of loopback is a real widening — and it partly re-opens the local-exfil surface that macOS's `remote ip "localhost:<port>"` carve was specifically written to keep closed ([`backend/macos.rs`](../../crates/nub-sandbox/src/backend/macos.rs) `emit_net`, and the `sandbox-pentest` loopback-exfil finding).
- Mitigation available: nub already uses a **unique per-run AppContainer SID**, so the exemption can be scoped to that ephemeral SID and torn down after the run — the widening is bounded to the sandboxed child's own lifetime, not machine-wide-forever. But *during* the run, that child sees all of loopback.
- Cleanup is mandatory and fragile: the exemption is machine-wide state; a crashed nub that never runs teardown leaks an exemption (harmless because the SID is orphaned, but it accretes list entries).

### 3c. Why binding the proxy off-loopback does NOT rescue an unprivileged path
The tempting non-admin idea: bind nub's proxy to the host's **non-loopback** IP (its LAN address) instead of `127.0.0.1`, since `internetClient` permits `0.0.0.0/0`. It fails as an *enforcement* boundary:
- With `internetClient` granted (needed to reach a non-loopback address), the child can **also connect directly to any external host** — the proxy becomes bypassable, so there is no per-host enforcement at all. Closing that direct-egress hole requires WFP filters scoped to the AC SID, which is **admin-gated** (`FWPM_ACTRL_ADD` is granted only to administrators — [WFP access control](https://learn.microsoft.com/en-us/windows/win32/fwp/access-control)).
- The `privateNetworkClientServer`-only variant (RFC1918 proxy bind, withhold `internetClient`) blocks the *public* internet but leaves **direct LAN egress uncontrolled** — reintroducing the SSRF-to-link-local/metadata surface the proxy's SSRF guard (commit `850ebe622f`) just closed, because direct-LAN traffic skips the proxy entirely. Leaky and fragile (needs a stable RFC1918 host IP; breaks offline). Rejected as a real boundary.

**Conclusion: there is no unprivileged path to OS-enforced per-host egress on Windows.** Either the child reaches a loopback proxy (needs an admin loopback exemption) or a non-loopback proxy with direct-egress closed (needs admin WFP). macOS/Linux avoid this because their primitives express a single-address egress allow directly; AppContainer cannot.

---

## 4. Prior art confirms the elevated-tier conclusion

Both comparable sandboxes reached the same wall and answer it the same way (from `sandbox-windows-research` findings §B, direct source reads):
- **Codex** (`windows-sandbox-rs`): an unelevated `RestrictedToken` backend does **write-confine only** and an **`Elevated`** backend that does a **one-time UAC** to create hidden accounts + persistent **WFP** filters; per-host egress is delegated to an external proxy reachable *because* the elevated setup carves it. Per-run is unelevated once the one-time setup exists.
- **SRT** (Claude Code's primitive stack, `srt-win.exe`): machine-wide **WFP filter set** keyed on a discriminator group SID, installed under a **one-time UAC**; child reaches the host only via the JS proxies. Default net = CONNECT-allowlist; an opt-in TLS-terminate (MITM) mode exists but is not default.

Neither ships unprivileged per-host on Windows. The pattern is universal: **per-host net on Windows = a one-time-elevated setup + per-run unelevated reuse.**

---

## 5. Historical implementation plan and current gap

The per-run elevated-exemption variant below was implemented and its direct-external negative control passed. That probe did not establish the stronger property required by §1: it did not prove that only Nub's proxy port was reachable. The exemption is intentionally all-loopback, so other local services and a local traffic forwarder remain reachable. This section is retained as implementation provenance, not as the current recommendation.

The blocker is single and well-understood, so the plan is concrete. It is gated on a **maintainer posture decision** (§6) because it introduces elevation/admin into the Windows sandbox — a security-posture call the maintainer owns.

### Historical recommended shape: coarse-on/off stays the unprivileged default; per-host + MITM use an elevated exemption

**Tier 0 (today, unprivileged default — keep):** enforced net with allow-rules ⇒ withhold `internetClient` ⇒ coarse egress-deny; report `net-per-host` degradation honestly. This already ships and is correct. Nothing to change for users who don't opt into strict.

**Tier 1 (implemented, elevated, but not parity):** make the loopback proxy reachable so ordinary proxy-aware clients can use per-host + MITM policy.
1. **Loopback exemption for the child's AC SID.** Before spawn, register the per-run unique AppContainer SID in the firewall exemption list via `NetworkIsolationSetAppContainerConfig` (read-modify-write the existing list; do not clobber). **Withhold `internetClient`** so the exempted child cannot connect directly to public addresses. This does **not** make Nub's proxy the sole egress: the exemption also exposes every other loopback listener. Tear down the exemption after the child exits (RAII, alongside the existing per-run ACE teardown in `WindowsLaunch::run`).
2. **Elevation acquisition.** The exemption write needs admin. Two sub-options (maintainer's call):
   - **One-time UAC + persistent helper** (Codex/SRT model): a first-run elevated step registers a small nub sandbox WFP/exemption facility keyed to a *stable* nub sandbox identity; per-run reuse is unelevated. Tension to resolve: a stable AC identity conflicts with the per-run-unique-SID fs-isolation choice — reconcile by keying the *net* exemption to a stable identity while keeping per-run fs grants, or by scoping the exemption via a WFP filter on the proxy port rather than the SID. Best UX (no per-run prompt) but the most engineering.
   - **Per-run elevation** (simplest): only enforce per-host when nub already runs elevated (or prompt per-run). Register/tear-down the exemption for the per-run SID under the held admin token. Simple, but a UAC prompt or admin shell per sandboxed run is poor UX.
3. **MITM (Q22) rides Tier 1 for free.** Once the child reaches the proxy, MITM needs only what mac/Linux already do: inject the ephemeral CA into the child's trust via the env bundle (`SSL_CERT_FILE`/`NODE_EXTRA_CA_CERTS`, already in the constructed env map) and run the existing `MitmEngine` in the proxy. No additional Windows-specific mechanism.

### Launcher-handoff contract additions
The Windows backend already owns spawn→wait→teardown (`Prepared::status()` → `WindowsLaunch::run`, the seam extension from `sandbox-windows-backend`). Additions are localized:
- `WindowsLaunch` gains: the proxy port (already threaded via `proxy_port`), an **exemption register step** before `CreateProcessW`, and an **exemption teardown** in the RAII drop path next to the ACE/profile teardown.
- The `allow_internet` decision stays `false` on the enforced-net path (unchanged — that is what makes the exemption safe: loopback-only reach).
- The CA-env injection for MITM reuses the existing constructed-env plumbing; no new field beyond what mac/Linux use.
- No change to the PM-pure boundary (Boundary B) — this is all inside `nub-sandbox`, no nub-cli/aube types cross.

### Effort estimate
- **Tier 1, per-run-elevation variant:** ~2–4 days. The exemption register/teardown is a bounded `netfw.h` FFI addition (`NetworkIsolationSetAppContainerConfig` read-modify-write + a `SECURITY_CAPABILITIES`-adjacent SID handoff nub already computes). The proxy + MITM engine already exist and are OS-agnostic. Most of the cost is the read-modify-write list safety, teardown-on-crash robustness, and the `windows-latest` enforcement probe (child-reaches-proxy positive + direct-external-still-denied negative + exemption-cleaned-up-after-exit).
- **Tier 1, one-time-UAC persistent variant:** ~1–2 weeks. The stable-identity-vs-per-run-SID reconciliation, the persistent facility, install/uninstall, and the elevation UX are the bulk.
- **MITM increment on top of Tier 1:** ~1 day (CA env bundle + engage the existing engine + a header-injection probe).

### Blockers needing the Windows VM
- The `nub-win` VM (`104.197.255.22`, user `nub`, key `~/.ssh/nub-vm`) is reachable and is the right box to prove the mechanism empirically before committing: (a) confirm a loopback-exempted AC child with `internetClient` **withheld** reaches `127.0.0.1:<proxy>` while direct external stays `WSAEACCES`; (b) confirm exemption register/teardown works and cleans up; (c) confirm the admin requirement for the write against a *real* registered profile (this doc grounded it in docs + the API contract, not a reproduced access-denied). These need admin on the VM (user `nubadmin`) for the exemption write. Not a research blocker — a validation step for the implementation.

---

## 6. Current decision boundary

The elevated all-loopback exemption is useful compatibility plumbing, but it is not the full per-host security mode. The current requirements are:

- A policy that requires a hostname boundary must reject before target launch on Windows while the all-loopback exemption is the only pinning mechanism. A reduced, explicitly supplemental mode may still use the proxy, but it cannot carry the same enforcement label as macOS/Linux.
- Complete Windows per-host enforcement needs an elevated, port-scoped WFP rule or an equivalent trusted network broker that prevents access to every other loopback endpoint. Codex and SRT's WFP designs are the relevant prior art.
- `net: true` needs a separate capability design that restores the host networking applications expect, including localhost, LAN, and listener use. Until then it must be reported as `internetClient` access rather than full network.
- The programmatic nested-launch design must keep the network setup in the trusted medium-integrity outer supervisor; a Low IL child cannot establish either the current exemption or a future WFP boundary for itself.

---

## Sources
- [Understanding Network Access in Windows AppContainers — Google Project Zero](https://projectzero.google/2021/08/understanding-network-access-windows-app.html) — loopback blocked by `IsLoopback`/`FWP_CONDITION_FLAG_IS_LOOPBACK` separate from capabilities; `privateNetworkClientServer` does not grant loopback; only admin `Add-AppModelLoopbackException`.
- [UWP Localhost Network Isolation and Edge — James Forshaw / Tyranid's Lair](https://www.tiraniddo.dev/2018/07/uwp-localhost-network-isolation-and-edge.html) — firewall enforces the block regardless of capability; exemption per-SID, admin-gated, grants all-loopback; Edge's proprietary bypass.
- [NetworkIsolationSetAppContainerConfig — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/netfw/nf-netfw-networkisolationsetappcontainerconfig) — `appContainerSids`, system-wide, admin, "for debugging."
- [Troubleshooting UWP App Connectivity Issues in Windows Firewall — Microsoft Learn](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/troubleshooting-uwp-firewall) — CheckNetIsolation must be run by an administrator.
- [WFP access control — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/fwp/access-control) — `FWPM_ACTRL_ADD` admin-only (the alternative egress-pinning path is also elevated).
- Code: [`crates/nub-sandbox/src/proxy/mod.rs`](../../crates/nub-sandbox/src/proxy/mod.rs), [`backend/macos.rs`](../../crates/nub-sandbox/src/backend/macos.rs), [`backend/linux.rs`](../../crates/nub-sandbox/src/backend/linux.rs), [`backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs) (branch `sandbox-primitives`); [`proxy/mitm.rs`](../../crates/nub-sandbox/src/proxy/mitm.rs) (branch `mitm-tier`).
- Threads: `sandbox-windows-backend`, `sandbox-windows-research` (findings §2, §B), `sandbox-linux-net-usernotify`, `sandbox.md` (§NET rows).
- Empirical: `nub-win` VM reachable as standard user `NUB-WIN\nub`; `CheckNetIsolation` present, read path open unprivileged.

## Changelog
- 2026-07-13 — **REVERSAL:** the implemented elevated loopback exemption is not per-host parity. It blocks direct public egress but exposes every loopback listener, so a local forwarder can bypass the proxy's hostname policy. Complete enforcement requires port-scoped WFP or an equivalent trusted boundary, and load-bearing per-host requests must reject until then. Also recorded that current `net: true` is `internetClient` only, not localhost/LAN/listener-capable full networking, and that nested setup belongs in the trusted outer supervisor.
- 2026-07-10 — Initial write-up. Establishes the AppContainer loopback block as the single confirmed technical limitation behind both Q21 (per-host net) and Q22 (MITM); documents the admin-gated, machine-wide, all-loopback-widening exemption tradeoff; **REVERSAL:** supersedes `sandbox-windows-research` findings §2's "userspace proxy is the non-admin per-host path on all OSes — Windows no different" — Windows is different because the child cannot reach the loopback proxy unprivileged. Recommends an opt-in elevated "strict Windows" tier (Codex/SRT model), coarse-on/off as the unprivileged default.
- 2026-07-10 — **Mechanism EMPIRICALLY VALIDATED + IMPLEMENTED** (per-run-elevation variant, PR to `sandbox-primitives`). On the `nub-win` VM (Windows Server 2022, GNU toolchain), driving the real backend under a standard user (Medium IL) and an admin (High IL) confirmed: (1) `NetworkIsolationSetAppContainerConfig` returns ACCESS_DENIED unelevated / succeeds elevated; (2) an elevated child reaches `127.0.0.1:<proxy>` while a direct public connect returns WSAEACCES; (3) RAII teardown returns the exemption list to baseline; and (4) unelevated per-host `apply()` fails closed. Implementation: `crates/nub-sandbox/src/backend/windows.rs` (`plan_net`/`WinNetPlan`/`is_elevated`/`set_loopback_exemption`/`ExemptionGuard`) with `net_tier` regression cases in `tests/windows_enforcement.rs`. The 2026-07-13 audit corrected the inference that this proved sole egress: no test scoped loopback access to Nub's proxy port, and the exemption cannot do so.
