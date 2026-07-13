# nub-sandbox — known limitations

> **Linux Bubblewrap transition.** The active Linux backend now uses a private
> mount/PID/network view. Older Landlock/seccomp sections below describe the baseline
> being replaced and are not claims about the active backend. The current Linux bounds
> are: unprivileged user namespaces must be usable; `CLOSE_RANGE_CLOEXEC` requires Linux
> 5.11; UID 0 is rejected because capability-free nested sandboxing cannot be preserved;
> per-host proxy bridging is not wired yet and therefore fails safe to no network;
> denied globs are expanded only across caller-supplied workspace/package roots at
> startup; pre-existing hardlink aliases remain readable; and separately writable bind
> roots create `EXDEV` boundaries for raw cross-root `rename`/`link` calls.

An honest record of what the engine does NOT close, why each residual is bounded, and
where the fix lives. The sandbox fails safe, not silent: an **axis-level** degradation a
policy reaches (a per-host net policy with no proxy → coarse deny; per-host Windows egress
without elevation → a fail-CLOSED `Degradation` error, never a silent coarse-degrade) is
surfaced via `Degradation`. The **within-axis over-grant**
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

### Egress SSRF: cloud-metadata / link-local AND RFC1918 blocked by default; `<private>` opt-in

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

### Windows per-host egress + MITM: opt-in elevated "strict Windows" tier

Per-host net (Q21) and the MITM/credential-brokering tier (Q22) enforce on Windows the
same way as macOS/Linux — the confined child's sole egress is nub's loopback proxy — but
reaching that proxy needs a step the other platforms don't. An AppContainer child is
WFP-blocked from ALL loopback regardless of capability, and the only lift
(`NetworkIsolationSetAppContainerConfig`, a per-run AC-SID loopback exemption) requires
administrator. So per-host/MITM on Windows is an **opt-in elevated tier**; coarse on/off
(allow-all or deny-all, which need no proxy) stays the unprivileged default, unchanged.

- **How it enforces (elevated):** before spawn the backend registers the per-run unique AC
  SID in the machine-wide loopback-exemption list (a read-modify-write that never clobbers
  other apps' entries), keeps `internetClient` WITHHELD so the exemption opens loopback
  ONLY — nub's proxy is the child's sole egress — and tears the exemption down when the
  child exits (RAII, alongside the ACE/profile teardown). MITM rides the same proxy: the
  ephemeral CA reaches the child through the CA-env bundle, exactly as on mac/Linux.
- **The widening tradeoff (bounded).** A loopback exemption is not scoped to the proxy
  port — for the run's lifetime the exempted child can reach EVERY loopback listener (a
  local DB on `127.0.0.1:5432`, a Docker daemon, an SSH-agent pipe, …), not just nub's
  proxy. Narrowing it to only the proxy port would need admin WFP filters, which nub does
  not install. The widening is BOUNDED to the ephemeral per-run AC SID and removed on exit,
  so it never persists past the sandboxed child's own lifetime. One consequence to note:
  if a loopback listener is itself an OPEN FORWARDER with external reach (a user's own
  local proxy, an SSRF-able localhost service), a hostile child could relay egress through
  it and sidestep the per-host allowlist — the same local-forwarder caveat that applies to
  any localhost-reachable sandbox, now in scope on the elevated Windows tier because the
  child can reach all of loopback (macOS/Linux keep loopback closed except the proxy port).
- **Fail-CLOSED, never silent.** A policy that REQUIRES the proxy (any per-host rule, or a
  MITM/`inject` broker) on a host where the exemption cannot be registered — nub not
  elevated, or the write fails — surfaces a clear error naming the elevation requirement
  and does NOT coarse-degrade an allow-list into a deny-all. A coarse-only policy needs no
  elevation and is unaffected.
- **Crash-leak (bounded).** A nub that dies without running teardown — including a hard
  kill via `TerminateProcess`, where the `ProfileGuard` RAII `Drop` also doesn't run, so
  the AppContainer profile leaks alongside it — leaks one orphaned exemption entry for its
  per-run AC SID. The SID is unique per run (`nub_sbx_{pid}_{nonce}_{ctr}`), so no future
  child is ever created under the orphaned exemption — the stale entry exempts no live
  process, it only accretes an unused list row. (A subsequent nub run re-reads the list
  and would preserve, not reuse, the orphan; it is inert until the machine's exemption list
  is manually pruned.)
- **Prior art:** Codex and SRT hit the same wall and answer it the same way — per-host net
  on Windows is an elevated setup with unprivileged reuse, never unprivileged outright.
  (`backend/windows.rs` `plan_net` / `WindowsLaunch::run`; `tests/windows_enforcement.rs`
  `net_tier`.)

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
- **Port-agnostic broker scoping.** A broker host matches regardless of port —
  brokering configured for `api.example.com` applies to that host on any port.
- **Wildcard broker scoping is the user's own risk.** A broker host accepts the same
  universal host-glob syntax as any net rule (`*.example.com`, bare `*`); it brokers to
  the client-supplied SNI of every matching host. Pointing a broker at too broad a
  wildcard can hand the credential to an attacker-owned subdomain that presents a valid
  real cert — identical exposure to any over-broad wildcard net allow, out of the threat
  model and un-warned (maintainer decision). Scope the wildcard to hosts you trust.

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
- **macOS hardlink-to-secret (host-verified).** Seatbelt file-read rules are path-pattern
  based, like Landlock's, so the same alias holds: a pre-existing same-uid hardlink to a
  secret, at a name the deny never targets, is readable, and reading it reads the SHARED
  inode — so the path-denied secret leaks through the alias. Host-verified: with the alias
  present the secret leaks; without it the path-deny holds. Unlike Landlock's inode-keyed
  grant, Seatbelt matches the PATH, so the denied path itself stays denied — only the alias
  name reads. Bounded: requires a hardlink created *outside* the sandbox beforehand. Fix:
  none clean at the Seatbelt layer (the inode was legitimately named twice). Regression
  test: `hardlink_to_denied_secret_leaks_via_alias` (`tests/macos_enforcement.rs`).
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
- **Windows `.env*` read-deny inside a granted read subtree — REPORTED, not enforced.**
  The default `.env*` READ-deny (injected on every read-granting fs policy — see
  `compiler::fold::finalize_env_deny`) is a deny that lands INSIDE the granted read
  subtree, which the AppContainer allowlist model cannot carve (an inheritable read-allow
  ACE on the grant defeats a nested deny — the same AAP-class trap). So a `.env*` file
  under a granted dir stays readable on Windows, and the backend HONESTLY reports it via
  the `fs-read-deny` `Degradation` (`deny_shadows_grant` in `backend/windows.rs`), never
  silently. macOS (Seatbelt deny-regex) and Linux (allow-only enumeration carve) enforce
  it fully. Fix (future): the DACL inheritance-break mechanism (a PROTECTED DACL on the
  confined root that strips inherited ACEs and re-grants only intended principals) can
  carve the deny and remove this degradation — not yet built. Consequence today: every
  read-granting Windows policy reports reduced mode for the `.env*` carve while the
  read-CONFINE itself (deny everything outside the allow-set) is fully enforced.

### Private tmp (`<tmp>: "rw"`/`false`) — macOS ENFORCED; Linux/Windows REPORTED, not enforced

`<tmp>` is a SENTINEL that always denotes a specially-provisioned per-run PRIVATE dir — never
the shared system tmp — so its value is a plain fs permission on that dir. `{ "fs": { "<tmp>":
"rw" } }` (or `true`) gives the child a fresh per-run temp dir (its `TMPDIR`/`TMP`/`TEMP` point
there) with the SHARED system tmp hidden; `false` hides the shared tmp with no private dir. The
shared system tmp is a SEPARATE literal path — reach it only by granting `/tmp` (`{ "fs": {
"/tmp": "r" } }`), which leaves the tmp mode unconfined. Per-OS state:

- **macOS — ENFORCED (real-kernel verified).** The Seatbelt profile denies read+write on the
  shared tmp roots (the confstr `$TMPDIR` scratch `/private/var/folders/<uid>/T` and the
  world-shared `/private/tmp`) after the fs grants, and — for `Private` — grants the fresh
  per-run dir `(allow file* (subpath …))`. The deny is last-match-wins, so it hides the shared
  tmp even under a generous `(subpath "/")` read. Verified: a file in `/private/tmp` is DENIED
  under `<tmp>: "rw"`/`false` and readable without, and reachable via a literal `/tmp` grant
  (`tests/macos_enforcement.rs` `private_tmp_hides_the_shared_system_tmp` /
  `deny_tmp_hides_the_shared_system_tmp_too` / `literal_tmp_path_is_the_only_way_to_the_shared_system_tmp`).
  - **Tradeoff (forced, documented):** the shared-tmp deny INCLUDES the confstr scratch that
    the backend otherwise write-grants for the Apple toolchain (`xcrun_db`), so a from-source
    native compile that needs it fails under `Private`/`Deny`. You cannot both hide the shared
    tmp and keep a grant into it; the mode is opt-in, and a native-build run stays on Shared.
- **Linux / Windows — REPORTED, not enforced (fail-safe).** The child's temp env is pointed at
  the fresh dir best-effort, but the shared-tmp is NOT yet hidden, so the backend reports a
  `tmp-private`/`tmp-deny` `Degradation` (reduced mode) rather than silently running the child
  on the visible shared tmp. Fix is a MAINTAINER-DECISION follow-up: Linux needs a Landlock
  allow-only carve that excludes the shared tmp from the read/write grants (allow-only has no
  deny primitive); Windows needs a decision on redirecting `TEMP`/`TMP` while satisfying the
  OS-essential temp floor the child needs to start. Wired through the IR + reported so the axis
  is honest today; enforcement lands per-OS later. (`backend/{linux,windows}.rs` `apply`,
  `backend::tmp_lost_axis`.)

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
