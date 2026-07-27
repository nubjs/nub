# nub-sandbox — known limitations

> **Linux Bubblewrap backend.** Linux uses an unmodified stock Bubblewrap executable
> to construct a private mount/PID/network view. Current bounds: unprivileged user
> namespaces and a stock `setsid` launcher must be usable; each candidate is accepted
> only after a behavior probe, and each target is released only after its namespaces,
> zero capability sets, seccomp posture, and session group are verified; per-host egress
> rides an empty-netns + UDS bridge to the loopback proxy, which resolves and allow-lists
> hostnames; denied globs are expanded only across declared project/workspace/package roots
> at startup; and an existing denied regular file with multiple hard links fails setup.

An honest record of what the engine does NOT close, why each residual is bounded, and
where the fix lives. The sandbox fails safe, not silent: an **axis-level** degradation a
policy reaches (a per-host net policy with no proxy → coarse deny; per-host Windows egress
without elevation → a fail-CLOSED `Degradation` error, never a silent coarse-degrade) is
surfaced via `Degradation`. The **within-axis over-grant**
residuals below are a different class — documented here, NOT signalled: derive→mount TOCTOU, the macOS floating-name move-block shapes, and NAT64/6to4.
This file is the durable "what's-not-covered" record the final PR and the build-jail thread
depend on.

Two kinds of residual appear here:

- **Engine residuals** — a bound the OS primitive itself imposes (for example, an
  inheritable Windows allow-ACE defeats a nested deny). The engine
  reports these and does not claim them closed.
- **Launcher-handoff items** — the engine constructs the child's confinement correctly,
  but a complete guarantee needs the *launcher* (the future build-jail/embedder that
  owns the parent process and the work-dir layout) to satisfy a contract the
  frontend-less engine cannot. These are NOT engine defects; they define the launcher
  contract.

## Network

### Egress SSRF: cloud-metadata / link-local AND RFC1918 blocked by default; `<private>` opt-in

> **macOS and Linux only.** Every guarantee in this section lives in the egress proxy, and the
> proxy starts only for a fine-grained (per-host/CIDR) policy — which Windows rejects before
> launch (`backend/windows.rs:301-310`, `:350-364`). A Windows child therefore runs under one of
> two coarse postures: no network capability at all, or `internetClient` with **raw egress and
> none of the hardening below** — no IMDS/link-local block, no RFC1918 default-deny, no
> DNS-rebinding pin. Worse, under `internetClient` the reachable range is *machine-dependent*: a
> Public network profile permits RFC1918 and `169.254.169.254`, a Private/Domain profile does
> not. Do not read this section as a cross-platform guarantee.

The loopback egress proxy resolves an allowed host and connects to the resolved IP, so an
allowed hostname whose DNS points at an internal address — or an attacker DNS-*rebinding*
an allowed domain to one between validation and connect — could reach an off-limits
address. Three halves:

- **CLOSED (hard, no opt-out) — cloud-metadata / link-local + rebinding.** The proxy fails
  closed at the outbound connect on the IMDS / link-local surface: IPv4 `169.254.0.0/16`
  (incl. the `169.254.169.254` metadata endpoint), IPv6 link-local `fe80::/10`, and the AWS
  IPv6 IMDS `fd00:ec2::254` — regardless of what the policy admits, and NOT re-opened by the
  `<private>` opt-in below (the AWS IPv6 IMDS sits inside ULA but is caught by this hard tier
  first). IPv4-in-IPv6 encodings (`::ffff:169.254.169.254`, `::169.254.169.254`) are unmapped
  before classification, and integer/octal/hex host forms are moot because classification
  runs on the RESOLVED `IpAddr`, not the child's token. Rebinding is pinned out: the host is
  resolved exactly once and the connect targets that same address — no re-resolution between
  check and connect. See `proxy/mod.rs` (`is_hard_blocked_ip`, `connect_upstream`).
- **CLOSED by default, `<private>` opt-in — broad RFC1918 / IPv6 ULA.** `10/8`, `172.16/12`,
  `192.168/16`, and IPv6 ULA `fc00::/7` are BLOCKED by default at the outbound connect, even
  when the policy admits the host (SSRF fail-closed, following Codex's block-by-default
  posture for agent-driven code). A project re-permits them with the explicit symbolic net
  target `<private>` (alias `<local>`), e.g. `net: ["<private>", "10.0.0.5"]` to reach a
  local service. A bare wildcard `*` does NOT re-open the private ranges — only the explicit
  `<private>` target does (mirrors Codex's non-wildcard local-allowlist). The opt-in is a
  policy-level flag (`net_allows_private`) that lifts the private tier of the SSRF guard.
  A raw private-range CIDR (`net: ["192.168.0.0/16"]`) admits at gate 1 but does NOT by
  itself lift the SSRF tier — the `<private>` token is what unlocks it. To narrow WHICH
  private hosts are reachable, compose `<private>` (unlock) with last-match-wins denies at
  gate 1: `net: ["<private>", "!10.0.0.0/8"]` reaches all private ranges except `10/8`.
  Loopback (`127/8`, `::1`) is in NEITHER tier — the proxy's own listener + loopback
  upstreams stay reachable unconditionally. See `proxy/mod.rs` (`is_private_range`,
  `net_allows_private`) and `NetTarget::Private`.
- **OPEN residual (impractical) — NAT64 / 6to4 IPv6 embeddings of link-local AND private
  ranges.** A link-local or RFC1918/ULA address wrapped in the NAT64 well-known prefix
  (`64:ff9b::169.254.169.254`, `64:ff9b::10.0.0.1`) or 6to4 (`2002:a9fe:a9fe::`,
  `2002:0a00:0001::`) is NOT unwrapped, so it dodges both tiers of the block. Reaching an
  internal target this way needs a NAT64/6to4 *translating gateway* on-path routing to it —
  absent in a normal cloud environment — so it is not a practical reach. Left unblocked
  rather than partly-covered because only the well-known prefixes are detectable (a
  network-specific NAT64 `/96` is not), and partial coverage would misrepresent the
  guarantee. Same `is_blocked_egress_ip` seam if the threat model later wants it.

### Linux per-host egress: cooperative proxy-env redirect

Per-host egress is wired. The child runs in an empty network namespace (`--unshare-net`),
which is the boundary — nothing leaves the box except through nub's loopback egress proxy.
A per-run bridge carries the child's proxy traffic across the empty netns to that proxy: a
loopback listener inside the netns forwards to a filesystem UDS, and a host-side listener
forwards the UDS to the proxy port. The proxy resolves the hostname (so the allow-list
matches the name the proxy sees, not a child-resolved IP — no DNS-rebinding confusion),
enforces the host allow-list, and requires a per-session token. The guarantee: **the program
can reach allowed hostnames through the proxy, and nothing else leaves the box.** The default
tier is a CONNECT/SNI allow-list with NO decryption; payload inspection happens only under the
explicit MITM tier (credential brokering / `proxy: "terminate"`), which is opt-in and announced.

The redirect is the standard proxy environment (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`), so
the reach is **defined by cooperation, not confinement widening**:

- A program that honors the proxy env reaches allowed hostnames and is refused for the rest.
- A program that resolves names itself and dials directly IGNORES the proxy env, hits the
  empty netns, and fails — `ECONNREFUSED`/`ENETUNREACH`. This is the DEFINED, fail-CLOSED
  behavior of per-host: a non-cooperating program loses egress rather than escaping it. DNS
  is the same story — the empty netns has no resolver, so a child doing its own `getaddrinfo`
  fails; resolution is meant to happen at the proxy (proxy-side DNS), reached over the proxy env.

A per-host policy whose bridge cannot be established fails SAFE to coarse deny (reported as
`net-per-host`), never silently to unrestricted egress. Target-created AF_UNIX sockets are
seccomp-denied under net confinement, so a generous filesystem policy cannot become a way
around the netns through a host socket. The trusted bridge is outside that target filter, and
`socketpair(2)` remains available for target-local IPC. Known network-equivalent daemon sockets
(`docker.sock`, container-runtime and D-Bus sockets) are also force-masked at the fs layer.

**FUTURE (not shipped) — transparent redirect.** A robustness upgrade would make a
non-cooperating program's direct egress work rather than fail: nftables DNAT inside the
userns-created netns to steer arbitrary outbound TCP at the proxy, plus an in-netns resolver.
That removes the "must honor the proxy env" caveat but adds materially more machinery; it is
deferred, and the cooperative redirect above is the shipped tier.

### Windows per-host egress and MITM are unavailable

An AppContainer child is blocked from loopback destinations by default. The package-wide loopback exemption needed to reach a proxy exposes every local listener, not just the proxy port. The backend therefore rejects per-host and MITM policies before launch, rather than installing the exemption. Coarse `net: true` permits public outbound connections but is not full host networking; coarse deny remains available without elevation.

### MITM tier: credential-brokering residuals (INFO, doc-only)

The capability-derived MITM tier (see
[`EMBEDDER.md`](EMBEDDER.md#net-axis--proxy-and-the-mitm-tier)) injects a secret into
an allowed upstream request server-side, so the sandboxed child never holds it. Its
bounded residuals and scope are:

- **Reflection-endpoint residual.** If the brokered upstream reflects request headers
  back into its response body — a debug/echo endpoint, or a compromised/malicious
  upstream — the injected secret comes back in a response the child CAN read. This is
  inherent to header-injection credential brokering, not a nub-specific bug: `op run`,
  corporate auth proxies, and every inject-at-the-proxy design carry the same residual.
  Brokering protects the secret from the child's environment and its outbound view, not
  from a reflecting upstream — only broker to upstreams trusted not to reflect
  credentials back.
- **Port-agnostic broker scoping.** A broker host matches regardless of port —
  brokering configured for `api.example.com` applies to that host on any port.
- **Exact-host-only scoping.** Broker hosts reject wildcards, IP literals, CIDRs, and
  symbolic host classes. The brokered TLS SNI/upstream and HTTP Host boundary must equal
  that literal; the CONNECT/SOCKS authority is independently required to be admitted by
  the net policy.

## Windows dedicated-account backend (agent-sandbox) — bounded residuals

The account backend (`backend/windows_account/`) runs the child as a dedicated local account
fenced by SID-keyed WFP filters. It carries the residuals below. Everything here is a
deliberate spike bound or an inherited platform property, not an unknown.

### One-time elevation is required, and per-run is not

`nub run --sandbox-admin setup` needs administrator: it creates a local account and installs
WFP filters, and Windows gates both. Every sandboxed run afterwards is unelevated. This is the
whole reason the WFP permit covers a loopback PORT WINDOW rather than the run's exact proxy
port — a filter tracking an ephemeral port would need a WFP write, hence a UAC prompt, on
every run.

- **Cost of the window:** the permit admits any local principal to loopback on those ports for
  as long as the filters are installed, rather than admitting only nub's proxy on one port.
  Bounded by the window's width (10 ports by default, 64 max, enforced at install).
- **Where fixed:** an elevated helper service could add an exact-port filter per run. Not
  built; the window is the deliberate trade.
- **The sandbox belongs to whoever ran setup.** `%PROGRAMDATA%\nub\sandbox` holds the
  DPAPI-sealed account credential, and machine-scope DPAPI is explicitly not a boundary — any
  local principal that can READ the ciphertext can decrypt it and then hold the sandbox
  account's password. Setup therefore replaces that directory's DACL with a PROTECTED one
  naming only SYSTEM, `BUILTIN\Administrators`, and the account that ran setup. Another
  standard user on the same box reads neither the credential nor the provisioning marker and
  fails closed with "not provisioned" — including the ordinary user of a machine where the UAC
  prompt was satisfied with a DIFFERENT administrator account. Re-running setup as that user
  re-points the lock at them.

### Egress coverage is `ALE_AUTH_CONNECT` only

The fence blocks outbound connects for the account on IPv4 and IPv6. It does NOT filter
inbound (`ALE_AUTH_RECV_ACCEPT_*`) or bind/listen (`ALE_RESOURCE_ASSIGNMENT_*`), so the
account may still open a listening socket. Neither of the reference implementations nub
mirrors (SRT, Codex) covers those layers either.

- **Why bounded:** the policy grammar's net axis is about EGRESS. A listener the child opens
  is reachable only by something already on the box.

### DNS still resolves

`getaddrinfo` is serviced by the `Dnscache` service running as `NETWORK SERVICE`, so name
resolution succeeds under a different token even though the child's own `connect()` is
blocked. No filter set can close this while keying on the connecting token.

- **Why harmless here:** the child reaches nub's proxy by IP and the PROXY resolves. A blocked
  host is blocked at connect regardless of whether its name resolved.

### Per-user tool installs are unreachable without an explicit grant

The child is a DIFFERENT principal, so anything under the invoking user's profile — an
nvm/fnm-managed Node, a Scoop or per-user winget package, `pip install --user`,
`%LOCALAPPDATA%\Programs\…` — resolves on `PATH` but cannot be opened. This lands
particularly hard on nub, whose premise is running the user's installed Node.

- **What the engine does:** auto-grants the resolved program FILE (never its parent directory,
  which would sweep in a neighbouring secret).
- **Launcher contract:** a program that loads SIBLING DLLs from its own directory needs the
  front-end to put that toolchain directory in the read allow-set — the same contract the
  macOS "toolchain read-confine for a non-system interpreter" residual defines.

### Certificate revocation checks fail under schannel

CryptoAPI's CRL/OCSP fetch goes out via WinHTTP under the caller's token and ignores proxy
environment variables, so it is blocked. `curl`, `git`, and `cargo` surface
`CRYPT_E_REVOCATION_OFFLINE` (`0x80092013`) unless revocation is disabled
(`curl --ssl-no-revoke`, `git -c http.schannelCheckRevoke=false`,
`CARGO_HTTP_CHECK_REVOKE=false`). `Invoke-WebRequest`, .NET `HttpClient`, and `gh` are
unaffected.

### Glob denies are not enforceable as aces

A deny whose matcher carries glob metacharacters (`**/.env`, `C:/proj/*.pem`) cannot be one
ACE. The engine reports `fs-deny-glob` as a LOST axis — distinct from the over-confinement
degradations, because a missed deny is a hole, not extra confinement. Literal deny paths are
enforced exactly.

### A grant or deny target that does not exist at launch is skipped

An ACE needs an object, so a policy path absent when the run starts gets none. Skipping rather
than failing is deliberate: the flagship agent policy denies `~/.ssh`, `~/.aws`,
`~/.docker/config.json` and `<proj>/.env`, most of which are absent on any given machine, and
aborting on the first missing one would kill the backend on its own headline shape. Denying a
path that does not exist denies nothing, which is correct — but a file created LATER at that
path, INSIDE a granted tree, inherits the tree's grant with no explicit deny to outrank it.
Every error other than not-found stays fatal.

- **Why bounded:** it needs the denied path to sit under a grant AND to be created after the
  aces land. A denied path outside every grant is unreachable to the account regardless.
- **Where fixed:** pre-create the deny target (as the Linux backend pre-creates write targets),
  or re-apply denies when the child creates a matching path. Neither is built.

### Junction/symlink TOCTOU on an ace target

Every ace target is canonicalized before the ace is written, so a symlink or junction resolves
to its TARGET and the ace lands on the object an open actually reaches — a reparse point does
not gate content access. Nothing then re-checks that the resolved object is still inside the
path the policy named. A child holding DELETE on a nested grant target can replace it with a
junction between runs, so the NEXT run stamps an inheritable grant on wherever that junction
points.

- **Why bounded:** it needs a prior run that granted write inside the tree, and it MOVES a
  grant rather than widening one — the next run's confinement is wrong, the current run's is
  not.
- **Where fixed:** open the target with `FILE_FLAG_OPEN_REPARSE_POINT` and ace the HANDLE, or
  re-verify the canonicalized target against the policy's own prefix after resolution.

### The child inherits the parent's real console handles

`CreateProcessWithLogonW` has no `bInheritHandles` parameter and no `STARTUPINFOEX` overload, so
stdio must ride the three `STARTF_USESTDHANDLES` fields — and nub marks the parent's own
stdin/stdout/stderr permanently inheritable to supply them. A foreign-user child therefore holds
the parent shell's real console INPUT handle, which is a `WriteConsoleInput` keystroke-injection
path back into that shell.

- **Where fixed:** give the child three anonymous pipes and relay, flipping nub's own ends
  non-inheritable — what SRT does. Not built; it also costs the child a real console (no PTY
  semantics, no `isatty`).

### A policy that confines only the network still runs as a foreign principal

Backend selection is per-POLICY, not per-axis: a policy with per-host egress rules and no
filesystem confinement still routes here, and its child then runs as the sandbox account —
which reaches nothing under the invoking user's profile. The user's own project files become
unreachable although they asked for nothing about the filesystem.

- **Why it is a surprise, not a hole:** it is over-confinement, and the engine reports `fs-read`
  as a degradation. It is listed because the SIZE of the behavioral change is easy to miss when
  the policy names only the net axis.
- **Where fixed:** the launcher supplies the project directory in the read allow-set — the same
  launcher contract as the per-user tool installs item above.

### Window-station access is granted per run, and verified only in session 0

seclogon's window-station auto-grant covers `WinSta0` only. A NON-INTERACTIVE caller — SSH, a
service, a CI agent — runs on a per-logon `Service-0x0-…$` station instead, where the auto-grant
does not apply and the child dies in loader init with `0xC0000142 STATUS_DLL_INIT_FAILED`. The
launch therefore aces the caller's own window station and desktop for the sandbox SID before
spawning, and restores both DACLs when the run ends. `READ_CONTROL` in the station mask is
load-bearing: without it the child HANGS in loader init rather than failing. Setting `lpDesktop`
explicitly does not substitute for the aces.

- **Verified in session 0 only.** The failure and the fix were both reproduced from a
  non-interactive session. The interactive `WinSta0` path — where seclogon's auto-grant should
  make the aces redundant — is untested; the aces are written there too and are expected to be
  a no-op.
- **The granted rights are BROAD, and on an interactive session that matters.** The masks are
  `WINSTA_ALL_ACCESS | READ_CONTROL` and the full documented `DESKTOP_*` union — what the VM
  diagnosis established as working, not a bisected minimum. Against the caller's own
  `WinSta0\Default` that hands the confined account `DESKTOP_JOURNALRECORD` and
  `DESKTOP_HOOKCONTROL` (desktop-wide keystroke capture), `DESKTOP_JOURNALPLAYBACK` (input
  injection into the user's session), `WINSTA_READSCREEN` (screen capture) and
  `WINSTA_ACCESSCLIPBOARD` — for the run's duration, at the same integrity level as the user's
  own apps, so UIPI does not block hooks against them. On a `Service-0x0-…$` station there is
  no user session to reach and the cost is nil.
- **Where fixed:** bisect the real floor on a VM and narrow both masks — plausibly station
  `READ_CONTROL | ENUMDESKTOPS | READATTRIBUTES | CREATEDESKTOP | ACCESSGLOBALATOMS |
  EXITWINDOWS`, desktop `READ_CONTROL | READOBJECTS | WRITEOBJECTS | CREATEWINDOW`. Not done
  here: the broad set is the one actually verified to work, and `READ_CONTROL`'s hang-vs-fail
  behavior already shows this surface punishes guessing.
- **Cost while the child runs:** concurrent runs share the station too — the restore puts back
  the DACL the FIRST of them saw, so a second run's ace can be removed while its child is still
  alive, and that run's own restore then re-writes the first's ace permanently. Same shape as
  the shared-account bound below.

### Whole-tree kill is best-effort

`AssignProcessToJobObject` on a `CreateProcessWithLogonW` child commonly returns
`ERROR_NOT_SUPPORTED`: the Secondary Logon service already placed it in its own job and
current Windows refuses that nesting cross-session. The assignment is attempted and the
failure logged; a descendant that outlives the target may survive.

- **Where fixed:** the two-hop broker→runner design SRT and Codex use, where the runner owns a
  job it can assign into. Deferred.

### Spike bounds carried deliberately

- **Single-hop launch.** The child is started directly, not through a runner holding a
  restricted token. Confinement comes from the account's ACL reach plus SID-keyed WFP, neither
  of which needs that token — but the token would additionally strip privileges and groups.
- **`lpDesktop` is NULL**, so the child shares the caller's desktop instead of getting a
  private one. A private desktop is the hardening follow-up and would additionally need a
  session-`BaseNamedObjects` ace. The station and desktop aces themselves are NOT part of this
  bound — the launch writes them on every run (see the window-station item above).
- **Concurrent runs share one account.** Two simultaneous sandboxed runs grant and strip aces
  for the SAME SID, so one run's teardown can revoke a grant the other still needs.
- **Child-created files are owned by the sandbox account.** Access is preserved (the grant is
  written UNPROTECTED so the user's inherited aces survive on new children), but the OWNER
  field changes, which git reports as "dubious ownership" and which leaves an orphaned owner
  SID if the account is later deleted. Neither reference implementation solves this.

### A path with a NULL DACL cannot carry a deny

Windows reads a NULL DACL as UNRESTRICTED access, not as an empty allow-set. Merging a deny
into it would yield a DACL containing only that ace, replacing the object's permissive state
with one that locks out its own owner — so the engine refuses rather than writing it. A grant
on such a path is skipped instead (the account already has access).

- **Why bounded:** fail-closed and loud. The launch aborts with a message naming the path.
  Ordinary project and profile directories carry a real DACL and are unaffected; this was
  observed only under `C:\Windows\Temp`.
- **Where fixed:** synthesize the equivalent explicit DACL (a deny for the sandbox account plus
  an `Everyone` full-access allow preserving the NULL-DACL semantics) rather than refusing.

### Residue after a crash

A run killed between granting an ace and stripping it leaves the ace behind. The ledger at
`%PROGRAMDATA%\nub\sandbox\acl-ledger.txt` records every path so
`nub run --sandbox-admin clean` (unelevated) collects them. A leaked grant is over-permission
for a confined account, not a host compromise — hygiene, not a correctness boundary.

- **`clean` is a machine-wide sweep, so it can revoke a LIVE run's aces.** The ledger is
  machine-wide and `clean` is unelevated, so a sweep strips every path it lists — including
  ones a concurrent sandboxed run still needs. Same root as the shared-account bound above:
  every run's aces key on the one SID, and nothing distinguishes a live grant from residue.

## Launcher-handoff items (engine correct; launcher must complete the guarantee)

### macOS ascendant-env via `KERN_PROCARGS2` — CLOSED in-engine

`sysctl(KERN_PROCARGS2, <pid>)` returns any same-uid process's exec-time argv+environ, so
a confined child could recover a scrubbed secret from a co-resident process (`getppid()` →
nub, a sibling, a spawned kin). The read is a disjunction — the kernel permits it if EITHER
`sysctl-read` OR `process-info*` is allowed for the target — so every wrapped Seatbelt
profile denies both arms:

- `process-info*` is allowed-by-default even under `(deny default)`, so it is denied
  explicitly, with `(allow process-info* (target self))` restoring only self-introspection
  (node needs it); never `(target others)`/`(target same-sandbox)`, which re-open the hole
  (a confined child's own siblings/children ARE same-sandbox).
- the sysctl arm is already shut by `(deny default)` — the pid-parameterized procargs2
  sysctl is unnameable (queried by numeric MIB) and the base admits `kern.*` only by
  narrow name (specific names plus two `kern.proc.*` prefixes, neither covering procargs2),
  never a bare `kern.` prefix (which would re-admit it).

Emitted UNCONDITIONALLY on every wrapped profile — including an env-scrub-only policy (the
`env_needs_closure` gate) — so a confined child can read no procargs2 but its own. Sibling
and same-sandbox-child reads are both EPERM, verified with negative controls
(`tests/macos_envread.rs`; `emit_env_read_closure` in `backend/macos.rs`).

- **Residual (irreducible at same-uid, bounded):** the closure binds every process nub
  confines, but nub cannot scrub a secret out of a co-resident process it never launched. A
  secret held in the *own* exec-time env of such a process (a CI runner injecting job
  secrets, `env SECRET=x tool`) stays readable within the same-uid trust domain. Closed only
  by a privileged uid boundary — the dedicated-account tier (post-v0).

### Windows ascendant-env via same-user `PROCESS_VM_READ` — OS-CLOSED (not a residual)

Previously suspected as the Windows twin of the macOS ascendant-env read; **empirically
disproven** — the AppContainer closes it. A LowBox child CANNOT
`OpenProcess(PROCESS_VM_READ)` the parent to read nub's environ: the AppContainer access
check requires the target process object's DACL to grant the child's package SID, a
capability, or `ALL APPLICATION PACKAGES`, and a normal parent process grants only the user
SID — so the open is denied (`ERROR_ACCESS_DENIED`), **independent of integrity level**.

- **CI-proven on windows-latest (run 29043151805)** with the parent BOTH elevated AND
  de-elevated (Medium-IL standard user): the AppContainer child's `OpenProcess(PROCESS_VM_READ
  | PROCESS_QUERY_LIMITED_INFORMATION)` on the parent is DENIED (exit 5), while an unconfined
  control recovers the secret (exit 0 — negative control proving the read path is live). So
  **no dedicated-account backend is needed for this axis.**
- **Honest bound:** the `PROCESS_VM_READ`-inclusive `OpenProcess` is proven denied; a
  `PROCESS_QUERY_LIMITED_INFORMATION`-only handle was not separately probed, but it cannot
  read the environment block (that requires `PROCESS_VM_READ`), so it does not reopen the axis.
- **VM-reconfirmed (burn box, standard `nub` user, 2026-07):** an AppContainer child's
  `OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION)` AND `(… | QUERY_LIMITED)` on
  its same-user parent are BOTH denied `ERROR_ACCESS_DENIED` — independent of the CI run
  (`tests/windows_ascendant_env.rs`, which mounts the full PEB-walk attack, not just the
  reporting check).
- **Code state:** `backend/windows.rs` `apply` emits NO `env-read-ascendant` `Degradation`
  (the enforcement suite locks that in) — the axis is closed by the OS, not merely reported.

### macOS toolchain read-confine for a non-system interpreter

The program auto-grant exposes the program FILE only (never its parent dir — that F3
over-grant is deliberately closed). A non-system Node (Homebrew/nvm) then needs its
toolchain directory in the read-allow set to load its own libraries under a tight
read-confine; the engine does not discover that dir itself (Boundary B — it receives
paths, it does not probe the host).

- **Where fixed:** the launcher/front-end supplies the interpreter's toolchain dir in
  the allow-set. A system interpreter is covered by the essential base and never hits
  this.

### Windows confined work dirs need a CLEAN-DACL root (not a nub-owned store; not ancestor traverse grants)

Superseded — a LowBox token retains SeChangeNotifyPrivilege (Bypass Traverse Checking) and
standard NTFS volumes carry `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL`, so intermediate-dir
ACLs are NOT access-checked: a leaf-only AC-SID grant is reachable under an ORDINARY
`%TEMP%`/profile tree with no ancestor traverse grants and no `C:\`-owned store (VM-verified
under `%TEMP%`, `tests/windows_enforcement.rs` + `windows_residuals.rs`). nub never needs
`WRITE_DAC` on a shared ancestor.

- **Real launcher contract:** the confined root must carry a CLEAN DACL — no inherited
  `ALL APPLICATION PACKAGES` allow-ACE. Where a work dir inherits an AAP grant (some
  `%TEMP%`/profile trees), an ungranted secret UNDER it is readable regardless of the
  allow-set (the AAP grant satisfies the LowBox check before default-deny). Demonstrated by
  the `windows_residuals.rs` RT-B probe; the fixtures strip inherited ACEs
  (`icacls /inheritance:r`) to model the clean root the launcher provides.

### Untrusted-tier tighten-only layering — by design, the caller's responsibility

For the granular object form an omitted axis **FLOORS** (fs deny-all, net deny-all *enforcing*,
env strip-all) — a present block is a COMPLETE STATEMENT, so `{}` is deny-all and
`{ "fs": [...] }` also denies net and strips env. There is NO implicit inheritance: the
object-level `{ "...": true }` opt-out was removed in Phase 4 (a policy is a complete
statement; reuse another list explicitly with a `...:#/pointer` array entry). (Corrected
2026-07-24: this paragraph previously said an omitted axis was *relaxed*, which contradicted the
shipped compiler — see `floor_fs`/`floor_net`/`floor_env` in `compiler/mod.rs`, the unit test
`empty_object_is_deny_all` in `tests/compiler.rs`, and the e2e fixture
`empty-object-default-deny.json`.)

Flooring is what makes a partial block from a less-trusted source fail closed rather than widen,
which was the original concern here. The engine still does **not** detect trust — it applies
whatever config it is given. Securing an untrusted-config run is the caller's responsibility, not
an engine mechanism nub supplies.

- **Status:** decided — nub does not detect untrusted config; the caller owns that trust
  boundary (the standalone "untrusted-tier tighten-only launcher" item is dropped). Distinct
  from the cross-layer tighten-only *intersection* (CLI > user-global > project), which IS
  enforced — a lower-trust layer may only add restrictions, never widen.

## Filesystem

- **Linux mount plans are current-path and literal.** Non-whole wildcard allows,
  `/proc`/`/sys`/`/dev` allows, unsafe broad write roots, and missing literal sources are
  rejected before launch. Missing deny targets are skipped. The engine never creates a
  host path to satisfy policy.
- **Linux deny-glob inventory is bounded.** Only current cwd, the nearest project,
  the containing workspace, and declared current-existing package roots are enumerated.
  Enumeration is immediate per root; project contents are never recursively scanned.
- **The `.env*` secret floor masks the monorepo, not the tree.** The deny mask covers
  env files at the workspace root and at every sub-project (member) root — exactly the
  packages the package manager resolves for the monorepo (`workspaces` for npm/yarn,
  `pnpm-workspace.yaml` when a `pnpm-lock.yaml` marks pnpm the incumbent). It does not
  mask `.env` in an arbitrary non-project subdirectory: a `.env` under a member that is
  not itself a workspace package stays readable inside the sandbox. The member inventory
  uses a bounded pattern grammar — a `*` segment matches one directory level, literal
  segments match exactly. A workspace declared with an unbounded pattern (`packages/**`,
  a partial-segment glob like `pkg-*`, or a brace group like `{apps,libs}/*`) is rejected:
  the run fails closed rather than proceeding with a partial mask.
- **Linux denied hardlinks fail setup.** A currently existing denied regular file with more than one hard link aborts setup rather than masking one pathname while an allowed alias can read the same bytes. The check only examines denied startup objects; it does not scan for aliases or reject unrelated dependency-store hardlinks.
- **Linux derive→mount TOCTOU.** Mount and mask planning canonicalizes paths before
  Bubblewrap installs the view. A same-uid local process that can replace a path during
  setup could shift a target. This is bounded to a local race and is not silently claimed
  closed.
- **Alternate procfs mounts are masked.** Existing procfs mountpoints outside `/proc`
  are discovered from `/proc/self/mountinfo` and layered unreadable in the child view.
- **macOS hardlink-to-secret (host-verified).** Seatbelt file-read rules are path-pattern
  based, so a pre-existing same-uid hardlink to a secret at an un-denied name remains
  readable. The denied path itself stays denied. Regression test:
  `hardlink_to_denied_secret_leaks_via_alias` (`tests/macos_enforcement.rs`).
- **macOS move/rename secret-relocation — literal AND regex directory-pinning denies CLOSED.**
  A write-deny keyed to a secret's path is defeatable by renaming a container dir out from
  under the deny. Both deny shapes now pin the container: a literal `(subpath)` deny pins its
  ancestor-dir chain, and a regex directory-pinning deny (`!secrets/*.key` → `/proj/secrets/*.key`)
  pins its literal directory prefix (`/proj/secrets`) and up to the write-grant root, so
  `mv secrets secretz` can no longer relocate the matched leaves. VM/host-verified: the
  ancestor rename is blocked while a legit write under the pinned dir still succeeds
  (`tests/macos_moveblock.rs`, `emit_move_block` in `backend/macos.rs`). **Residual (bounded):**
  the pin covers a secret whose container is the deny's literal directory prefix or a FULL glob
  component below it (`packages/*/.env`). Two shapes stay open: a floating-name deny with no
  fixed prefix (`!**/secrets/**` — the `secrets` component floats to any depth), and a PARTIAL
  glob in a non-leaf component (`!sec*/x.key` — the relocation-sensitive `secrets/` dir is
  matched by `sec*`, not literal, so it sits below the pinned prefix and renaming it to a
  non-`sec*` name escapes; a literal `}`/`]` in a dir name hits the same corner). Both need a
  user-authored glob-directory deny AND a writable container; the file-level deny still blocks
  renaming the matched leaves themselves.
- **Windows program grant is file-only (neighbor-read leak CLOSED).** The engine grants
  read+execute on the program FILE ITSELF, not its parent dir (traverse-bypass makes the
  leaf-object ACL sufficient to exec), so a `.env` next to a binary is no longer swept
  into the allow-set. Mirrors the macOS file-only program grant. VM-verified: the neighbor
  `.env` is DENIED while the child still execs and reads its granted dirs
  (`tests/windows_residuals.rs` R1, `tests/windows_enforcement.rs`). Residual launcher
  contract (identical to the macOS "toolchain read-confine" item above): a program that
  loads SIBLING DLLs from its own dir needs the front-end to supply that toolchain dir in
  the read allow-set — the engine no longer auto-widens. A self-contained build-jail
  toolchain (`node.exe`) needs nothing more. (`backend/windows.rs` `apply`.)
- **Windows `.env*` read-deny inside a granted read subtree — REJECTED before launch.** The compiler injects a default `.env*` read deny into every read-granting filesystem policy (`compiler::fold::finalize_env_deny`). The AppContainer allowlist cannot carve that deny from an inheritable read grant, so the backend returns an `fs-read-deny` `Degradation` error before producing a launchable `Prepared` (`deny_shadows_grant` in `backend/windows.rs`). Direct embedders and the Nub CLI fail closed identically. The macOS Seatbelt and Linux Bubblewrap backends enforce the deny. A future DACL inheritance-break mechanism could make the policy enforceable on Windows; until then, Windows read confinement works only when no deny is nested inside a granted subtree.

### Private tmp (`$tmp: "rw"`/`false`) — macOS/Linux enforced; Windows reported

`$tmp` is a SENTINEL that always denotes a specially-provisioned per-run PRIVATE dir — never
the shared system tmp — so its value is a plain fs permission on that dir. `{ "fs": { "$tmp":
"rw" } }` (or `true`) gives the child a fresh per-run temp dir (its `TMPDIR`/`TMP`/`TEMP` point
there) with the SHARED system tmp hidden; `false` hides the shared tmp with no private dir. The
shared system tmp is a SEPARATE literal path — reach it only by granting `/tmp` (`{ "fs": {
"/tmp": "r" } }`), which leaves the tmp mode unconfined. Per-OS state:

- **macOS — ENFORCED (real-kernel verified).** The Seatbelt profile denies read+write on the
  shared tmp roots (the confstr `$TMPDIR` scratch `/private/var/folders/<uid>/T` and the
  world-shared `/private/tmp`) after the fs grants, and — for `Private` — grants the fresh
  per-run dir `(allow file* (subpath …))`. The deny is last-match-wins, so it hides the shared
  tmp even under a generous `(subpath "/")` read. Verified: a file in `/private/tmp` is DENIED
  under `$tmp: "rw"`/`false` and readable without, and reachable via a literal `/tmp` grant
  (`tests/macos_enforcement.rs` `private_tmp_hides_the_shared_system_tmp` /
  `deny_tmp_hides_the_shared_system_tmp_too` / `literal_tmp_path_is_the_only_way_to_the_shared_system_tmp`).
  - **Native-build carve-out (`Private` only):** under `Private` the confstr TEMP scratch
    (`/var/folders/<uid>/T` — the Apple toolchain's fixed, non-TMPDIR-redirectable `xcrun_db`
    lookup cache) is EXCLUDED from the shared-tmp deny, so it stays granted and a from-source
    native compile keeps the same toolchain scratch it has under `Shared`; only the
    world-shared `/private/tmp` is hidden. `Deny` (`<tmp>: false`, no tmp at all) carves
    nothing — it hides the confstr scratch too, so a native compile that needs it fails under
    `Deny` (a native-build run uses `Private`, not `Deny`).
- **Linux — ENFORCED.** `Private` bind-mounts the fresh per-run directory at `/tmp`;
  `Deny` installs an empty traverse-only tmpfs and remounts it read-only after any
  explicitly allowed descendant mounts are layered.
- **Windows — REPORTED, not enforced (fail-safe).** The backend reports `tmp-private`/`tmp-deny`
  instead of silently claiming the axis. Two corrections to what this file previously claimed
  (both verified in source 2026-07-24):
  - The child's temp env is **NOT** pointed at the fresh dir. `set_tmp_env` is called only on the
    non-confining fast path (`backend/windows.rs:401-420`, guarded by
    `!sandboxing && tmp_lost.is_none()`); the enforcing path builds
    `WindowsLaunch { env: Some(policy.env.constructed.clone()) }` (`:512-522`) with no tmp
    injection. The per-run private dir is created, never handed to the child, then deleted at
    drop. The earlier "pointed at the fresh dir best-effort" wording was true only of the fast path.
  - Under `$tmp: false` (Deny) the child is actively pointed at the SHARED temp, because
    `OS_ESSENTIAL_ENV` unconditionally injects the ambient `TEMP`/`TMP` (`compiler/defaults.rs:351`)
    — the inverse of what was requested. The keys cannot simply be dropped: `CreateProcessW` into
    an AppContainer fails without them, so `Deny` must point at an empty granted dir instead.

## Linux namespace and syscall boundary

Stock Bubblewrap creates fresh user, PID, IPC, device, and process-filesystem views.
Network-deny policy also creates a private network namespace and installs a small seccomp
filter that denies target-created AF_UNIX sockets and non-bridge IP families plus
`io_uring_setup`. Unrestricted network policy installs no network filter. Nested namespace
inheritance and conditional keyring hardening are separate unfinished work and are not claimed
here.

Before a Bubblewrap executable is used, a bounded probe verifies the required stock
mount operations, read-only remounting, private `/proc` and `/dev`, network isolation,
and seccomp effect. The probe runs for each launch so acceptance cannot outlive a changed
host confinement context. The production target then stops at an in-sandbox gate after
Bubblewrap setup. Nub verifies that exact process has zero inheritable, permitted,
effective, bounding, and ambient capabilities; occupies the expected namespaces; has
the required seccomp mode; and is its own session and process-group leader before it is
resumed. This permits UID 0 only when the resulting process has no Linux capabilities.

Inherited descriptors are marked close-on-exec with `close_range` where available. On
older kernels, Nub enumerates a verified procfs `/proc/self/fd` directory with raw
syscalls and marks every non-setup descriptor close-on-exec; it does not rely on the
current `RLIMIT_NOFILE` as an enumeration bound.
