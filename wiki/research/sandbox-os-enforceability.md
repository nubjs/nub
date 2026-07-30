# Sandbox OS enforceability — the capability × platform analysis

**Status:** living research, consolidated 2026-07-09 and corrected through 2026-07-13. The early design-phase feasibility record remains below, but the Windows column and verdict now distinguish an available OS primitive from the guarantee the current backend actually establishes.

> **Linux mechanism superseded 2026-07-12.** The Landlock analysis below remains the historical comparison, but the implemented Linux backend now uses Bubblewrap mount masks. See [`wiki/sandbox-architecture.md`](../sandbox-architecture.md) and [`crates/nub-sandbox/LIMITATIONS.md`](../../crates/nub-sandbox/LIMITATIONS.md).

## Dotfile-manager aliases and the deny guarantee

The common dotfile-manager shapes do not justify scanning every inode or rejecting hardlinked dependency files:

- [GNU Stow](https://www.gnu.org/software/stow/manual/stow.html) and [Homeshick](https://github.com/andsens/homeshick) manage dotfiles through symlinks.
- [chezmoi](https://www.chezmoi.io/reference/source-state-attributes/) normally materializes managed files and supports explicit symbolic-link entries.
- [yadm](https://yadm.io/docs/overview) uses the home directory as its Git worktree, so the managed file is already at its final pathname.
- [Dotbot](https://github.com/anishathalye/dotbot) normally creates symlinks but also offers an explicit `type: hardlink` option. Hardlink-managed secrets therefore exist, even though they are not the common default.

The Linux guarantee is deliberately path-based. Masking a project `.env` symlink follows the link and masks its resolved source throughout that sandbox. Masking one hardlink pathname cannot hide another allowed pathname for the same inode; the source or alias must be listed separately if it is also sensitive. Nub does not reject `st_nlink > 1`, warn on ordinary dependency-store hardlinks, or scan `node_modules` for aliases because none of those actions would prove that all names or copies of the content were found.

This is the design-phase enforceability groundwork; where later on-real-OS verification refined a mechanism, [`wiki/sandbox-architecture.md`](../sandbox-architecture.md) §7 carries current thinking. The important Windows correction is that neither per-file deny ACEs nor the proposed protected-DACL inheritance break is implemented by the current backend for arbitrary projects. This doc is the compact cross-OS capability matrix + verdicts.

## Historical expressiveness target — Landlock ∩ Seatbelt

The config schema's expressiveness target is the **intersection of what Landlock and Seatbelt can enforce** — confirmed sufficient for the threat model. Per-host egress is enforced in a **userspace localhost proxy** (works on all OSes; bridges Landlock's port-only limit — the kernel can deny/allow by port/IP but not hostname).

Two capabilities are **deliberately NOT exposed** (coarser-but-enforceable; leave out unless real demand appears):

- **Seatbelt regex path filters** — nub exposes subtree + glob, not arbitrary regex.
- **Landlock per-sub-right write bitmask** — nub collapses the write sub-rights to one `write_allow`.

## Capability × OS table — historical Linux/macOS columns, current Windows correction

| Capability | Linux | macOS | Windows |
|---|---|---|---|
| **FS write-confine** | FULL — Landlock ABI v2 `from_write` subtree | FULL — Seatbelt `(deny file-write*)` + re-allow + move-blocking | **Available, with production preconditions.** AppContainer SID grants work unprivileged on controlled NTFS roots. The current backend temporarily edits host DACLs; its lock is process-local, so concurrent Nub processes can race the read-modify-write restoration path. |
| **FS read-confine** (deny secrets / `.env*`) | FULL — recursive Landlock read walk (allow-only; carve by enumeration, no deny primitive) | FULL — one Seatbelt `(deny file-read* (regex …))`, one rule, all depths | **Default-deny allowlists work only on a controlled clean-DACL root.** The probe proved that case. The current backend neither establishes nor verifies that precondition for an arbitrary project, and it does not enforce a deny inside a broad project grant; it reports `fs-read-deny` and runs under warning-capable front-ends. |
| **NET egress — coarse on/off** | FULL — seccomp `AF_INET`-deny + loopback carve-out | FULL — Seatbelt loopback-only `network-outbound` | **No-network is full. Full-network is not.** Withholding AppContainer network capabilities blocks egress. Granting only `internetClient` does not restore localhost, private-LAN, or listener/server access, so current `net: true` is not equivalent to host networking. |
| **NET egress — per-host allowlist** | Userspace proxy + seccomp deny + loopback carve-out (Landlock-v4 net rules where k6.7+) | Userspace proxy + Seatbelt loopback-only carve-out | **Current mode is reduced.** The elevated loopback exemption lets the child reach Nub's proxy but grants access to every loopback listener. A local forwarding service can bypass the hostname allowlist. Port-scoped WFP or an equivalent trusted boundary is required for a complete guarantee. |
| **ENV scrub** (child's own env) | FULL (spawn-layer allowlist, pure Rust) | FULL | **FULL** — `lpEnvironment` of `CreateProcessAsUser`. Zero privilege. (Case-insensitive-key footgun on Windows.) |
| **ENV-READ isolation from ascendants** (the PID mechanism) | FULL — read-confine never grants `/proc` + seccomp deny of the ptrace family; PID-ns (`unshare --pid`) opportunistic where userns available | PARTIAL — no `/proc`; `task_for_pid` SIP-gated; the `KERN_PROCARGS2` exec-time-env residual (arch §5) | **FULL for an AppContainer child launched by Nub.** The de-elevated real-backend probe observed `OpenProcess(PROCESS_VM_READ)` fail with `ACCESS_DENIED`; its unconfined negative control recovered the parent secret. |
| **Process containment** (reap tree) | FULL — process-group + `setrlimit` / cgroup v2 `cgroup.kill` | FULL — process-group + `setrlimit` | **FULL for one launch** — Job Object `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` + active-process/memory caps. **Nested boundary incomplete:** a Low IL child cannot reproduce the current fresh-SID/DACL launch without a trusted outer broker or stable-identity redesign. |

TLS = **CONNECT host-allowlist, NO MITM** by default (read the host from the cleartext CONNECT line / SNI, blind-pipe the tunnel). MITM is rejected as the default (breaks cert-pinning, needs a CA install in every tree, buys nothing against the residual); the capability-derived per-host termination tier is architecture §6.

**Linux mechanism, EMPIRICALLY CONFIRMED:** `rust-landlock` (BestEffort, ABI v2) + `PR_SET_NO_NEW_PRIVS` + `restrict_self`: grant `/` read+exec, write only to pkg-dir + private scratch → **`FullyEnforced` as a non-root user, zero sudo/caps/flags.** Cross-package tamper blocked; bypass surface closed (symlink-escape, `..`, hardlink, REFER-move all denied). Floor = ABI v1 / kernel 5.13; the allow-only / no-deny model is confirmed PERMANENT (no deny-rules on the kernel roadmap). Secret read-deny is NOT expressible in allow-only Landlock — the one Linux-vs-macOS deficit — closed instead by confining reads to own-dir + toolchain (no secret deny-LIST needed). **Test-env gotcha: Docker Desktop CANNOT run Landlock** (its LinuxKit kernel ships `CONFIG_SECURITY_LANDLOCK` unset, unfixable by any flag) — use a real VM (e.g. Lima Ubuntu 24.04). (Grounding: the design-time Linux Landlock write-confine research, referenced from the design record; findings inlined here.)

**Backend crates:** Linux = `rust-landlock` BestEffort; macOS = nub's own SBPL generator; Windows = AppContainer (validated). **birdcage REJECTED** — GPL-3.0 (license-incompatible) AND its Linux backend is a userns/mount-namespace jail (NOT Landlock) that fails on stock Ubuntu-24.04/RHEL/restricted-CI, the exact targets nub needs; borrow its API shape only. **bubblewrap + network namespace both ruled out** generally — both need unprivileged user namespaces that fail on stock Ubuntu 24.04+/RHEL/CI. **DAC separate-uid REJECTED** (default, optional tier, AND fallback): can't get a separate uid unprivileged on the targets (needs subuid + setuid-root helpers or root; userns-create blocked by Docker-default-seccomp / `user.max_user_namespaces=0` / Ubuntu-24.04 AppArmor — the same wall as bwrap, heavier deps); where rootless works it corrupts artifact ownership; and the create-not-modify semantic never fires (real native builds are create-only). (Grounding: the design-time DAC-uid prototype research; findings inlined here.)

## Windows — current feasibility and limits

Dropping a child into an AppContainer, constructing its environment, assigning a Job Object, and withholding public-network capability are unprivileged operations on supported Windows versions. That makes a useful native backend feasible, but it does not make every configured policy enforceable by the implementation now in the tree.

- **Filesystem allowlists:** the controlled-root probe is valid. A LowBox child could read the AppContainer-SID-granted root and could not read an ungranted secret. The result depends on the object not already granting access through `ALL APPLICATION PACKAGES` or another capability SID. Production does not currently create or validate a protected root for arbitrary projects.
- **In-project denies:** a per-file deny ACE does not subtract an inherited AppContainer allow. The proposed inheritance-break approach could create a controlled tree, but it is not implemented. Current deny entries such as `.env` and `*.sandbox.json` are omitted from Windows grants and reported as `fs-read-deny`; a security-boundary front-end must reject instead of treating the warning as enforcement.
- **Network:** no-network is enforceable without admin. `internetClient` is only public-internet client access, not full host/local/listener access. Per-host currently requires an elevated loopback exemption, and that exemption exposes all localhost services; it is not a port-scoped sole-egress boundary.
- **Environment and process tree:** child environment construction, ascendant-environment read isolation, and one-launch Job Object containment are verified.
- **Nesting:** the current per-run unique SID and temporary DACL-grant model does not stack from Low IL. A trusted medium-integrity outer broker must retain the parent ceiling and apply descendant requests, or a stable-identity alternative must prove the same no-widen property. Until then, nested Windows sandbox requests reject before the target starts.

The resulting posture is not “Windows complete” or “a strict superset of Codex/SRT.” AppContainer supplies useful general isolation, while Codex's elevated backend currently handles explicit file read denies and port-scoped network policy that Nub does not yet match. Each policy must be compared by restriction, not by backend label.

### Prior-art Windows postures (from source, not docs)

| Tool | Windows FS-write | Windows FS-read-deny | Windows NET | Native Windows support |
|---|---|---|---|---|
| Codex (RestrictedToken, unelevated) | YES — restricted token | NO — routes to elevated backend | NO OS block (env-rewrite only) | Ships but OFF by default; documented = WSL2 |
| Codex (Elevated, dedicated accounts) | YES | YES — per-path ACL deny-read ACEs | YES — WFP per-account SID | One-time UAC setup; `CodexSandboxOffline`/`Online` accounts |
| SRT / Claude Code | ACL-based configured-path restrictions | Existing-file deny ACLs, while SRT explicitly does not present its Windows filesystem layer as an adversarial boundary | YES — WFP + restricted token + proxy | Native implementation exists; its stated boundary and prerequisites differ from Nub's AppContainer model |
| Claude Code (docs) | WSL2 only | WSL2 only | WSL2 only | Explicitly unsupported native; WSL2 required |

## GOVERNING CONSTRAINT — three-OS security parity (maintainer, 2026-06-23)

Complete parity across Linux + macOS + Windows for the sandbox's SECURITY GUARANTEES is mandatory (*"otherwise this doesn't really work"*; *"also needs to work on macOS!!"*; *"I'd be surprised if Windows made this literally impossible"*). A confinement guarantee that's hard on one OS is NOT "defer that OS" — it's "find the unprivileged mechanism there." Scopes to security-confinement parity, not every behavior (documented behavioral divergences like the shell-emulator stand).

## The `.env*` deny-list — per-OS mechanism

The motivating case: prevent code from reading the contents of any `.env*` files while allowing access to sibling directories. The decided posture: **default-deny `.env*`** read (a default-on read-deny in the runtime profile, overridable), **recursive at EVERY depth** (monorepo `packages/*/.env`, nested `.env.local`), read-only (the file stays writable — the threat is read-exfil, not overwrite). Near-zero-breakage: legitimate code reads secrets via the **process env** nub injects pre-spawn, not via `fs.read()` of the file. Scope = read-exfil of `.env*` files PRESENT ON DISK; out of scope = secrets already in `process.env`, and a `.env*` appearing on disk *after* spawn.

The load-bearing insight: Landlock read and write are INDEPENDENT access rights granted per-rule (`from_read` / `from_write` are distinct flag sets), so the two axes need not be enumerated the same way — write is subtree-wide on the project root; read is recursive, carving out `.env*`.

- **Linux = recursive Landlock read walk.** Landlock is allow-only with no deny/subtract primitive, so `.env*` is protected by *not granting* it: grant write subtree-wide on the root in one rule; grant read recursively, where a dir with no `.env*` gets ONE broad subtree read grant (walk stops) and a dir containing a `.env*` is granted per-child + descend (the `.env*` is never read-granted). A one-time pre-scan records which subtrees are clean; cost is proportional to `.env*` count × depth, not total file count.
- **macOS = one native deny regex.** `(allow file-write* (subpath proj))` + `(allow file-read* (subpath proj))` + `(deny file-read* (regex #"/\.env($|\.)"))` — matches `.env`/`.env.*` at any depth, no enumeration (Seatbelt is last-specific-match-wins; deny after broad-allow wins).
- **Windows = not currently enforced for an in-project carve-out.** The design-phase draft used per-file deny ACEs; the probe proved that an inherited `ALL APPLICATION PACKAGES` allow defeats that approach. A protected-DACL root with only deliberate grants is a feasible future design for startup-existing paths, but the current backend does not materialize that tree or apply an inheritance break. It grants the broader project path, skips the deny entry, and reports `fs-read-deny`. A load-bearing `.env*` or `*.sandbox.json` policy must fail before launch on Windows until a controlled-root or equivalent mechanism lands.

The Windows post-spawn-created-`.env*` future-file guarantee needs an admin-only minifilter — explicitly out of scope unless a stronger threat model demands it.

## Cross-references

- [`wiki/sandbox-architecture.md`](../sandbox-architecture.md) §5–§7 — the current per-OS enforcement model (env-read boundary, network, backends).
- [`wiki/sandbox-build-jail.md`](../sandbox-build-jail.md) — the build-jail front-end that consumes these mechanisms.
- Prior art across the other sandboxes: [`sandbox-prior-art.md`](sandbox-prior-art.md).
- Linux depth: [`sandbox-linux-confinement-audit.md`](sandbox-linux-confinement-audit.md), [`sandbox-linux-userns-backend.md`](sandbox-linux-userns-backend.md). (The design-time `linux-landlock-write-confine`, `dac-uid-build-sandbox-prototype`, and `macos-seatbelt-write-confine` research docs referenced from the earlier design record are not in the current tree; their findings are inlined above.)

## Changelog
- 2026-07-13 — **REVERSAL:** separated Windows primitive feasibility from current production enforcement. Corrected the broad claims of complete read confinement, full `net: true`, per-host proxy parity, and clean nesting. The current backend does not enforce in-project deny carve-outs, does not guarantee a clean-DACL arbitrary project root, grants only internet-client networking for `net: true`, exposes all loopback in per-host mode, and cannot create its fresh-SID/DACL boundary from Low IL. Also corrected ascendant environment read isolation to the verified AppContainer-closed result and updated the SRT prior-art row.
- 2026-07-09 — Initial write-up, consolidated from the design-phase enforceability analysis: expressiveness target + two deliberately-unexposed capabilities, the capability × OS table, the Windows elevation verdict + CI-validation + Codex-dedicated-account pricing, the prior-art Windows-postures table, the three-OS-parity governing constraint, and the per-OS `.env*` deny mechanism (with the Windows deny-ACE→inheritance-break supersession noted against architecture §7).
