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
`net-per-host`), never silently to unrestricted egress. The known network-equivalent daemon
sockets (`docker.sock`, container-runtime and D-Bus sockets) are force-masked at the fs layer
under net-confinement, so a generous filesystem policy cannot become a way around the netns.

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

### Private tmp (`<tmp>: "rw"`/`false`) — macOS/Linux enforced; Windows reported

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
- **Linux — ENFORCED.** `Private` bind-mounts the fresh per-run directory at `/tmp`;
  `Deny` installs an empty traverse-only tmpfs and remounts it read-only after any
  explicitly allowed descendant mounts are layered.
- **Windows — REPORTED, not enforced (fail-safe).** The child's temp env is pointed at
  the fresh dir best-effort, but the shared temp path is not yet hidden, so the backend
  reports `tmp-private`/`tmp-deny` instead of silently claiming the axis.

## Linux namespace and syscall boundary

Stock Bubblewrap creates fresh user, PID, IPC, device, and process-filesystem views.
Network-deny policy also creates a private network namespace and installs a small seccomp
filter for socket families that cross that namespace plus `io_uring_setup`. Unrestricted
network policy installs no network filter. Nested namespace inheritance and conditional
keyring hardening are separate unfinished work and are not claimed here.

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
