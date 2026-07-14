# nub-sandbox — known limitations

> **Linux Bubblewrap backend.** Linux uses an unmodified stock Bubblewrap executable
> to construct a private mount/PID/network view. Current bounds: unprivileged user
> namespaces and a stock `setsid` launcher must be usable; each candidate is accepted
> only after a behavior probe, and each target is released only after its namespaces,
> zero capability sets, seccomp posture, and session group are verified; per-host proxy
> bridging is not wired and therefore fails safe to no network; denied globs are expanded
> only across declared project/workspace/package roots at startup; and an existing denied regular file with multiple hard links fails setup.

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

### Linux per-host egress is not yet wired

The stock-Bubblewrap backend currently provides full network or a private network
namespace for deny/restricted policy. The authenticated proxy/control plane required for
per-host egress is deferred. A per-host policy therefore tightens to no network and is
reported as `net-per-host`; it never silently widens to unrestricted egress.

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

## Filesystem

- **Linux mount plans are current-path and literal.** Non-whole wildcard allows,
  `/proc`/`/sys`/`/dev` allows, unsafe broad write roots, and missing literal sources are
  rejected before launch. Missing deny targets are skipped. The engine never creates a
  host path to satisfy policy.
- **Linux deny-glob inventory is bounded.** Only current cwd, the nearest project,
  the containing workspace, and declared current-existing package roots are enumerated.
  Enumeration is immediate per root; project contents are never recursively scanned.
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
- **Windows `.env*` read-deny inside a granted read subtree — REPORTED, not enforced.**
  The default `.env*` READ-deny (injected on every read-granting fs policy — see
  `compiler::fold::finalize_env_deny`) is a deny that lands INSIDE the granted read
  subtree, which the AppContainer allowlist model cannot carve (an inheritable read-allow
  ACE on the grant defeats a nested deny — the same AAP-class trap). So a `.env*` file
  under a granted dir stays readable on Windows, and the backend HONESTLY reports it via
  the `fs-read-deny` `Degradation` (`deny_shadows_grant` in `backend/windows.rs`), never
  silently. macOS (Seatbelt deny-regex) and Linux (Bubblewrap masks) enforce
  it fully. Fix (future): the DACL inheritance-break mechanism (a PROTECTED DACL on the
  confined root that strips inherited ACEs and re-grants only intended principals) can
  carve the deny and remove this degradation — not yet built. Consequence today: every
  read-granting Windows policy reports reduced mode for the `.env*` carve while the
  read-CONFINE itself (deny everything outside the allow-set) is fully enforced.

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
