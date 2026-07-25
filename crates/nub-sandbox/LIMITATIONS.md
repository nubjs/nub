# nub-sandbox — known limitations

An honest record of what the engine does NOT close, why each residual is bounded, and
where the fix lives. The sandbox fails safe, not silent: an **axis-level** degradation a
policy reaches (a per-host net policy with no proxy → coarse deny; per-host Windows egress
→ coarse deny) is surfaced at runtime via `Degradation`. The **within-axis over-grant**
residuals below are a different class — documented here, NOT signalled: hardlink-to-secret,
the `/etc` no-deny-carve edge, derive→open TOCTOU, bind-mounted procfs, the macOS
floating-name move-block shapes, Linux `ConnectTcp` at `ptrace_scope≥2`, and NAT64/6to4.
This file is the durable "what's-not-covered" record the final PR and the build-jail thread
depend on.

Two kinds of residual appear here:

- **Engine residuals** — a bound the OS primitive itself imposes (Landlock has no
  address filter; an inheritable Windows allow-ACE defeats a nested deny). The engine
  reports these and does not claim them closed.
- **Launcher-handoff items** — the engine constructs the child's confinement correctly,
  but a complete guarantee needs the *launcher* (the future build-jail/embedder that
  owns the parent process and the work-dir layout) to satisfy a contract the
  frontend-less engine cannot. These are NOT engine defects; they define the launcher
  contract.

## Network

### Egress SSRF: cloud-metadata / link-local blocked; broad RFC1918 is a posture call (partly open by design)

The loopback egress proxy resolves an allowed host and connects to the resolved IP, so an
allowed hostname whose DNS points at an internal address — or an attacker DNS-*rebinding*
an allowed domain to one between validation and connect — could reach an off-limits
address. Two halves:

- **CLOSED — cloud-metadata / link-local + rebinding.** The proxy fails closed at the
  outbound connect on the IMDS / link-local surface: IPv4 `169.254.0.0/16` (incl. the
  `169.254.169.254` metadata endpoint), IPv6 link-local `fe80::/10`, and the AWS IPv6 IMDS
  `fd00:ec2::254` — regardless of what the policy admits. IPv4-in-IPv6 encodings
  (`::ffff:169.254.169.254`, `::169.254.169.254`) are unmapped before classification, and
  integer/octal/hex host forms are moot because classification runs on the RESOLVED
  `IpAddr`, not the child's token. Rebinding is pinned out: the host is resolved exactly
  once and the connect targets that same address — no re-resolution between check and
  connect. See `proxy/mod.rs` (`is_blocked_egress_ip`, `connect_upstream`).
- **OPEN BY DESIGN (maintainer posture call) — broad RFC1918 private ranges.** `10/8`,
  `172.16/12`, and `192.168/16` are NOT blocked by default. Blocking them wholesale breaks
  legitimate private-host allowlisting and collides with the deliberate loopback carve
  (the proxy's own listener and loopback upstreams must stay reachable), so it is a
  separate posture decision rather than folded into this guard. The seam is clean: a
  policy/config toggle can extend `is_blocked_egress_ip` to the private ranges when the
  maintainer decides the default. Until then, private-range egress is admitted iff the
  active policy admits the host.
- **OPEN residual (impractical) — NAT64 / 6to4 IPv6 embeddings of link-local.** A
  link-local address wrapped in the NAT64 well-known prefix (`64:ff9b::169.254.169.254`)
  or 6to4 (`2002:a9fe:a9fe::`) is NOT unwrapped, so it dodges the block. Reaching IMDS this
  way needs a NAT64/6to4 *translating gateway* on-path routing to a link-local target —
  absent in a normal cloud environment — so it is not a practical metadata reach. Left
  unblocked rather than partly-covered because only the well-known prefixes are detectable
  (a network-specific NAT64 `/96` is not), and partial coverage would misrepresent the
  guarantee. Same `is_blocked_egress_ip` seam if the threat model later wants it.

### Linux per-host egress: the port-scoped `ConnectTcp` residual (CLOSED via seccomp user_notify)

Under a per-host allowlist the child is forced through the loopback egress proxy, and
Landlock ABI-v4 `ConnectTcp` pins `connect()` to the proxy's port. Landlock has **no
address filter**, so historically a direct TCP `connect()` to an *external* host on the
(random, per-run) proxy port bypassed the per-host gate. **This is now closed** by a
seccomp `USER_NOTIF` supervisor over `connect()`: a filter installed in the child's
pre_exec (raw `seccomp(…, NEW_LISTENER)`, unprivileged under `no_new_privs`) routes every
`connect()` to a notification the nub PARENT services — it reads the child's destination
sockaddr from `/proc/<pid>/mem`, re-validates the notif id, and permits ONLY
`127.0.0.1:<proxy_port>`. On ALLOW the supervisor OWNS the connect (connects a fresh socket
to the FIXED proxy address and injects it over the child's fd via `NOTIF_ADDFD` SETFD),
which makes the allow path TOCTOU-robust against a post-read sockaddr rewrite; DENY returns
EPERM. Two sibling bypasses of the same gate are closed alongside it, both pure seccomp
(so they hold even where the supervisor is skipped, below): (1) the **TCP-Fast-Open**
variant — `sendto`/`sendmsg(MSG_FASTOPEN)`, which initiates a connection *without*
`connect()` — via a `MSG_FASTOPEN`-flag deny on the send syscalls; (2) **non-TCP stream
protocols** — Landlock's `ConnectTcp` governs only `IPPROTO_TCP`, so an `AF_INET`
`SOCK_STREAM` socket over SCTP (`IPPROTO_SCTP`) or MPTCP (`IPPROTO_MPTCP`, which is
default-on and transparently falls back to TCP against any server) would pass the type
narrowing yet dodge the hook — closed by narrowing the `socket()` protocol (arg2) to TCP
only. See `backend/linux_connect_notify.rs` and `backend/linux.rs` (`build_seccomp`).

- **Residual bound (narrow, hardened-host only):** the supervisor reads the child's memory
  via `/proc/<pid>/mem`, which yama `ptrace_scope >= 2` forbids even for a parent. On such
  hosts the supervisor is skipped (`viable()` is false) and the pre-existing port-scoped
  `ConnectTcp` residual remains — per-host egress keeps working rather than breaking. The
  default `ptrace_scope <= 1` closes it fully. No capability-floor change: `USER_NOTIF`
  (≥5.0) and `NOTIF_ADDFD` (≥5.9) sit below the Landlock-v4 (6.7) kernel floor proxy mode
  already requires. macOS (address+port carve) and Windows have no equivalent gap.
- **The same supervisor pattern extends to inbound** (`listen()`/`bind()` — the bind-less
  `listen()` autobind residual below): a future hardening slot, not wired now.

### Linux bind-less `listen()` autobind (P3, strictly dominated)

Landlock hooks `bind()` and `connect()` but has **no `socket_listen` hook**, so a
`listen()` issued WITHOUT an explicit `bind()` still autobinds a random ephemeral port —
an inbound listener remains creatable on an unpredictable port. Explicit `bind()`
(including `bind(port = 0)`) IS denied and VM-proven; only the bind-less path is open.

- **Why it adds nothing:** strictly weaker than the `ConnectTcp` residual above — the
  port is unpredictable, the listener needs inbound reachability, and the child has no
  outbound channel to signal the drawn port under net-deny. It confers no capability the
  port-scoped connect edge doesn't already bound; noted only so it is not later mistaken
  for full inbound closure. Same full-close as above (seccomp `user_notify` / netns).
  Documented in `backend/linux.rs` (`apply_landlock`).

### Windows per-host egress is not wired (coarse deny holds)

Windows has proven coarse egress-deny (no `internetClient` capability blocks all egress,
loopback included), but not per-host. An AppContainer child cannot reach the loopback
proxy without a registered loopback exemption (`NetworkIsolationSetAppContainerConfig`),
which this phase does not wire.

- **Why bounded:** fail-safe — a per-host allow policy degrades to coarse **deny**, not
  to open egress, and reports a `net-per-host` `Degradation`. Nothing is silently
  allowed.
- **Where fixed:** wire the loopback exemption so the child can reach the proxy —
  build-jail thread or a later hardening slot.

### MITM tier: credential-brokering residuals (INFO, doc-only)

The capability-derived MITM tier (see
[`EMBEDDER.md`](EMBEDDER.md#net-axis--proxy-and-the-mitm-tier)) injects a secret into
an allowed upstream request server-side, so the sandboxed child never holds it. Two
residuals:

- **Reflection-endpoint residual.** If the brokered upstream reflects request headers
  back into its response body — a debug/echo endpoint, or a compromised/malicious
  upstream — the injected secret comes back in a response the child CAN read. This is
  inherent to header-injection credential brokering, not a nub-specific bug: `op run`,
  corporate auth proxies, and every inject-at-the-proxy design carry the same residual.
  Brokering protects the secret from the child's environment and its outbound view, not
  from a reflecting upstream — only broker to upstreams trusted not to reflect
  credentials back.
- **Port-agnostic broker scoping.** A literal broker host matches regardless of port —
  brokering configured for `api.example.com` applies to that host on any port.

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

For the granular object form an omitted axis is **relaxed** (the "boolean is the de-nesting
mechanism" contract — you confine what you name). An *untrusted* tier would want the opposite
default (omitted axis fail-closed / tighten-only), but the engine does **not** detect trust —
it applies whatever config it is given. Securing an untrusted-config run is the caller's
responsibility, not an engine mechanism nub supplies.

- **Status:** decided — nub does not detect untrusted config; the caller owns that trust
  boundary (the standalone "untrusted-tier tighten-only launcher" item is dropped). Distinct
  from the cross-layer tighten-only *intersection* (CLI > user-global > project), which IS
  enforced — a lower-trust layer may only add restrictions, never widen.

## Filesystem (bounded P2s, threat-model-mitigated)

Each is documented in code at the site noted; none is silently mis-reported.

- **Linux `/etc` granted wholesale for the loader — a user deny now carves it (FIXED).**
  Build toolchains read `/etc` (resolv.conf, CA bundles), so the essential-read set grants
  `/etc` (and `/usr`,`/lib`,…) whole for the dynamic loader. Previously an explicit
  `!/etc/secret` was NOT honored — the wholesale essential-base grant overrode the carve,
  so a secret under `/etc` stayed readable despite the deny (VM-verified: `!/etc/**`,
  `!/etc/secret`, `!/etc/*`, `!/etc` all failed to deny). Now the essential-base grant
  CARVES an essential dir when a user deny reaches inside it (an implicit-allow walk
  excluding the denied path), so the deny is honored while the loader's own files stay
  readable — VM-verified surgical (wholesale vs carved `/etc` differ in exactly the denied
  file; `ld.so.cache`, `ca-certificates.crt`, dynamic linking all unaffected). Residual:
  with NO user deny, `/etc` is still granted whole (a secret under it is readable — user
  secrets do not live in `/etc`, and net-deny blocks exfil). (`backend/linux.rs` +
  `backend/linux_grants.rs` `essential_dir_needs_carve`/`derive_essential_dir_carve`.)
- **Linux dangerous-write-root over-grant — CLOSED (cross-OS consistency, F2).** A write
  grant that resolves to a dangerous top-level root (`/`, `/etc`, `/usr`, `/home`, …) was
  HONORED on Linux while macOS/Windows dropped it, so a `..`-collapsed surface path
  (`<proj>/../../..` → `/`, collapsed lexically at compile time) or an explicit `/`/`**`
  rw grant became a filesystem-wide write hole. The Linux grant derivation now drops such
  a grant fail-safe, mirroring the macOS/Windows `is_dangerous_write_root` guard. VM-verified:
  pre-fix an explicit `/` rw and a `..`-collapse-to-`/` both wrote OUTSIDE the project;
  post-fix both are denied while a legitimate scoped rw grant (`./writable`) still writes,
  and a co-listed scoped grant survives alongside the dropped dangerous root. Reads are
  exempt (a generous `(subpath "/")` read is a legitimate posture); `/tmp` is excluded (the
  legitimate broad temp target). (`backend/linux_grants.rs` `is_dangerous_write_root` /
  `derive_write_grants`; `tests/linux_enforcement.rs` `dangerous_write_root_grant_is_dropped`.)
- **Linux write-target for a not-yet-existing file becomes a DIRECTORY (not a parent
  widen).** Landlock cannot grant write to a file that does not yet exist, so the backend
  pre-creates the target and grants that subtree. VM-verified: `pre_create` uses
  `create_dir_all`, so the requested leaf is created as a DIRECTORY and its subtree is
  write-granted — the containing PARENT dir is NOT widened (a sibling in the parent stays
  denied). Consequences: (a) a tool expecting to write a FILE at that path finds a
  directory; (b) the over-grant is the target-as-dir subtree, bounded within the named
  path. Fix: none clean at the Landlock layer. (`backend/linux.rs` `pre_create`.)
- **Linux hardlink-to-secret (VM-verified).** A pre-existing same-uid hardlink to a
  secret, at a name the deny never targets, is granted `ReadFile`, which grants read on
  the SHARED inode — so the path-denied secret is reachable through the alias (and, since
  the grant is inode-keyed, the denied path itself then leaks too). VM-verified: with the
  alias present the secret leaks; without it the path-deny holds. Bounded: requires a
  hardlink created *outside* the sandbox beforehand. Fix: none clean at the Landlock layer
  (the inode was legitimately named twice). Regression test:
  `hardlink_to_denied_secret_leaks_via_alias`.
- **Linux derive→open TOCTOU.** Grant derivation canonicalizes paths on the host, then
  the kernel enforces at `open()` later; a path swapped in between could shift a target.
  Bounded: a same-uid local race within the confined tree. (Inherent to a canonicalize-
  then-enforce split; not deterministically reproducible.)
- **Linux bind-mounted procfs at a non-standard path.** The `/proc` filter (the
  ascendant-env boundary) is path-literal (`starts_with("/proc")`), so a procfs
  bind-mounted at a non-standard path is not filtered and IS grantable when a covering fs
  grant reaches it — VM-verified: under `{fs:["/tmp"]}` a `/proc` bind-mounted at
  `/tmp/altproc` was readable (`/tmp/altproc/version` leaked). The environ-secret read
  itself (`/proc/<pid>/environ`) is additionally gated by `ptrace_may_access`, which held
  in every VM topology tried (ancestor non-dumpable / sibling non-attachable under
  `ptrace_scope>=1`), so the specific ascendant-env leak was not reproduced — but the
  filter bypass is real. Bounded: requires the ability to bind-mount procfs (prior
  privilege/setup) before entering the sandbox. Fix: a mount namespace, or resolve the
  mount type rather than the path prefix.
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

## Linux syscall-boundary hardening (defense-in-depth, seccomp/`/dev`)

Three former residuals now closed at the syscall boundary so FS/env confinement no longer
leans on a host-policy chain. All VM-verified (kernel 6.17, Ubuntu 24.04) with differential
probes carrying positive + negative controls; each has a committed regression test.

- **Linux userns + mount family — the FS-escape now denied in seccomp (O1, CLOSED).** An
  unprivileged user namespace (`unshare`/`clone(CLONE_NEWUSER)`) plus a bind-mount under a
  granted dir is the strongest FS-confinement escape. It was previously blocked only by a
  CHAIN — Landlock never grants `/proc` (so a `uid_map` write fails), the kernel refuses a
  mount from an unmapped userns, and host AppArmor — none of which the engine owns. The
  seccomp filter now denies `unshare`, `mount`, `umount2`, `pivot_root`, `move_mount`,
  `fsopen`, `fsmount`, `fsconfig`, `fspick`, `mount_setattr`, `open_tree` wholesale (the
  classic + new-mount-API surface), and `clone` when its flags register carries
  `CLONE_NEWUSER`/`CLONE_NEWNS`. `clone3` hides its flags behind a pointer (seccomp cannot
  inspect them), so a companion filter returns `ENOSYS` for `clone3` — glibc then falls back
  to the flag-filtered `clone` (the same technique Docker's default profile uses), closing the
  `clone3(CLONE_NEWUSER)` path without breaking threading. VM-verified: `unshare` and
  `clone(CLONE_NEWUSER)` return EPERM and `clone3` ENOSYS under sandbox, while the same
  unprivileged forms succeed unsandboxed; normal `sh`/`python3`/PTY children AND a full Node
  run (Worker-thread pthread_create → clone3→ENOSYS→clone fallback, plus crypto) are
  unaffected. Two bounded trade-offs, both accepted: (a) a binary that calls `clone3`
  DIRECTLY and does not fall back on ENOSYS loses threading — rare, and the same trade Docker's
  default profile makes; (b) a nub-sandboxed child that builds its OWN user+mount-namespace
  sandbox (an `unshare`/`bwrap`-based tool, a browser/Electron zygote) is denied under ANY
  sandboxing axis, env-scrub-only included — the intended defense-in-depth posture, not a bug.
  Residual: a procfs *bind-mounted before* entering the sandbox is still not created via these
  syscalls (that residual is the "bind-mounted procfs at a non-standard path" item above); O1
  removes the in-sandbox mount-creation route to it.
  (`backend/linux.rs` `build_seccomp`/`clone_userns_newns_rules`/`build_clone3_enosys`;
  `seccomp_denies_userns_and_mount_family`.)
- **Linux pidfd fd-theft — `pidfd_getfd` now denied (O2, CLOSED).** `pidfd_getfd` steals an
  open fd out of another process (needs `PTRACE_MODE_ATTACH`; no legitimate confined use) and
  was not in the denylist. It is now denied in seccomp. `pidfd_open` stays ALLOWED — it has
  legitimate self-child uses (`pidfd_send_signal`/`waitid`) and cannot itself steal an fd.
  VM-verified: under sandbox `pidfd_open` still returns a valid fd while `pidfd_getfd` is
  EPERM; both go through unsandboxed. (`backend/linux.rs` `build_seccomp`;
  `seccomp_denies_pidfd_getfd_keeps_pidfd_open`.)
- **Linux `/dev` narrowed to a least-privilege allowlist (O3, CLOSED).** A read-confined
  child was granted wholesale `/dev` rw. It is now granted per-node:
  `null`/`zero`/`full`/`random`/`urandom`/`tty` (standard sink/source/entropy/tty) plus
  `ptmx` + the `pts` subtree (PTYs). Everything else under `/dev` — and listing `/dev`
  itself — is denied and fails CLOSED (EACCES, visible), not leaked. Landlock does no
  directory-traverse check, so a leaf grant suffices to open the node by path without
  granting the `/dev` directory. VM-verified: `/dev/null` + `/dev/urandom` and a full PTY
  (ptmx → pts slave) work under read-confine, `python3` and a full Node run still spawn, and a
  non-allowlisted node (`/dev/shm` write) is denied while relaxed-fs admits it. Bounded
  trade-offs of the narrowing (each fails CLOSED + visible, never a silent leak): a
  read-confined child can no longer open `/dev/shm` (POSIX shared memory —
  `multiprocessing.shared_memory`, some native addons), `/dev/fd` / `/dev/stdin|out|err`
  (these symlink into `/proc/self/fd`, deliberately ungranted — breaks `bash <(…)` process
  substitution and `fs.read('/dev/stdin')`), or any device node outside the set; add the node
  to the policy allow-set if a workload needs it. The granted `/dev/pts` subtree is rw, so a
  child can reach ANOTHER same-uid process's PTY (Landlock + DAC do not scope by owner within
  the subtree); on kernels older than the `dev.tty.legacy_tiocsti=0` default this permits
  TIOCSTI keystroke injection into that terminal — bounded to same-uid, which already shares a
  trust domain. Residual: the env-scrub-only relaxed path (fs deliberately relaxed) still
  grants `/dev` among the top-levels — narrowing it there would contradict "fs relaxed"; the
  allowlist governs the read-confine path where `/dev` is explicitly granted.
  (`backend/linux.rs` `DEV_ALLOWLIST`; `dev_allowlist_permits_pty_and_nodes_denies_rest`.)
