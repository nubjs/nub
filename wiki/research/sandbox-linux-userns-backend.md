# Optional Linux namespace sandbox backend (design)

> **DROPPED 2026-07-10 — this backend is not being built.** Landlock+seccomp is the sole Linux backend. It needs zero host configuration, is purpose-built for this confinement, is augmenter-aligned (no external tool, no privilege), and is enforcement-proven (18/18); a userns-based backend requires unprivileged-userns to be enabled, which hardened hosts and much of CI disable, so it cannot be a dependable floor. The only genuine niche — a pre-Landlock kernel (< 5.13) — is covered honestly by degrade-with-warning rather than by carrying a second, host-config-dependent backend. This document is retained below as a historical record of the design that was considered and declined; the decision lives in [`wiki/sandbox-architecture.md`](../sandbox-architecture.md) §7.

*2026-07-09. Design/planning, recommend-only. Proposes an OPTIONAL second Linux
enforcement backend for `crates/nub-sandbox` that uses unprivileged Linux namespaces
(user + mount + net + pid) to give STRONGER confinement than the current Landlock+seccomp
path where unprivileged user namespaces are available, falling back to Landlock+seccomp
otherwise. No code lands from this doc. It surfaces ONE load-bearing decision (§1) the
maintainer ratifies before implementation; everything else is design.*

Sources read directly (file:line grounded, not memory):
- **nub** — `crates/nub-sandbox/` on the unmerged `sandbox-primitives` branch:
  `src/backend/{mod,linux,linux_grants,linux_connect_notify}.rs`, `LIMITATIONS.md`,
  `src/conformance.rs`, `tests/linux_enforcement.rs`.
- **Prior art** — `codex/codex-rs/linux-sandbox/` (`bundled_bwrap.rs`,
  `proxy_routing.rs`, `bwrap.rs`, `landlock.rs`), `sandbox-runtime/src/sandbox/`
  (`linux-sandbox-utils.ts` bwrap+socat bridge). The reproduced-on-VM
  `sandbox-linux-confinement-audit.md`.

## TL;DR

- The namespace tier is **additive hardening on permissive hosts only**. Landlock+seccomp
  stays the shipped baseline and the contracted floor; the namespace layer engages
  opportunistically when unprivileged userns is available and **layers OVER** Landlock+seccomp
  (defense in depth), it does not replace them. Its absence is NOT a `Degradation`.
- It closes the specific FS residuals `LIMITATIONS.md` documents as open under Landlock —
  `/etc`-wholesale, write-target-widening / new-path over-grant, symlink-replacement,
  derive→open TOCTOU, bind-mounted-procfs — and (with a pid-ns) implements the
  PID-namespace process-hiding the arch doc claims but the audit found unimplemented
  (`sandbox-linux-confinement-audit.md` FINDING 2). For **net deny-all**, an empty netns
  is strictly stronger and simpler than the current seccomp socket-family deny.
- **THE decision to sign off (§1): hand-roll the namespaces in Rust (libc
  `unshare`/`mount`/`pivot_root`) vs shell out to `bubblewrap`.** Recommendation:
  **hand-roll**, because "unprivileged, dependency-free, no external binaries" is the
  documented identity of nub's Linux backend across every comparison doc, and shelling to
  bwrap (or bundling it, as Codex must) regresses exactly that. Both options are laid out
  for the call.
- **Scope: a POST-primitives expansion. It does NOT gate the `sandbox-primitives` done-gate.**
  A pre_exec-compatible first cut (user+mount ns for the FS closes, empty-netns deny-all)
  is ~2–3 weeks incl. a VM conformance matrix + adversarial review; the pid-ns and the
  netns-per-host bridge are a nub-owned-spawn follow-on.

---

## What the namespace tier actually buys (per-residual, honest)

Grounded against `LIMITATIONS.md` "Filesystem (bounded P2s)" and the confinement audit.
"CLOSED" means the constructed-view mechanism removes the residual; caveats are stated.

| `LIMITATIONS.md` residual | Landlock today | Namespace tier | Mechanism |
|---|---|---|---|
| `/etc` granted wholesale, no deny-inside carve | open (P2) | **CLOSED** | over-mount the specific secret path with `/dev/null` (or a tmpfs), or construct a tmpfs `/etc` with only needed files bound — Claude Code's `--ro-bind /dev/null <deny>` model |
| write-target widening for a not-yet-existing file → parent DIR granted (new-path over-grant) | open (P2) | **CLOSED** | `--bind` a single-file writable target; read-only binds elsewhere make creation impossible outside a writable bind |
| symlink-replacement of a deny target into an allowed subtree | documented residual | **CLOSED** | an over-mount pins the path — it resolves to the mounted object regardless of a later symlink swap at that path |
| derive→open TOCTOU (path swapped between canonicalize and open) | open (same-uid race) | **CLOSED** | the mount view is constructed ONCE at setup; the child sees a fixed root, no re-derivation window |
| bind-mounted procfs at a non-standard path re-exposes `/proc/<ppid>/environ` | open (needs prior privilege) | **CLOSED** | pid-ns + fresh `/proc` mount: the parent isn't in the child's pid-ns, so its `environ` doesn't exist in the child's `/proc`; the child can't remount host procfs |
| hardlink-to-secret (pre-existing alt-path bypasses a path deny; Landlock keys on inode) | documented residual | **PARTIAL** | closed ONLY under a minimal-root plan where the secret's fs region is not bound in at all; an over-mount masking the canonical path does NOT mask the inode via the hardlink — state this honestly |
| PID-ns process hiding "opportunistic where userns available" (arch §5) | **claimed, not implemented** (audit FINDING 2) | **CLOSED** | this backend is where that claim becomes real |

Net (from the network-axis comparison):
- **deny-all** → an empty netns (only `lo`, or nothing) is a structurally airtight route-less
  isolation — strictly stronger than, and replacing, the seccomp `AF_INET`/exotic-family
  deny for this mode. Trivial (`unshare(CLONE_NEWNET)`).
- **per-host** → a netns has no external route, so the SCTP/MPTCP/TFO/`ConnectTcp`-port and
  the yama-`ptrace_scope≥2` supervisor residuals (`LIMITATIONS.md` Network, audit FINDING 1)
  are all mooted — BUT the child must then be bridged to the parent's proxy, which is the
  hard, external-binary-tempting part (§3-net). Deferred to a follow-on; first cut keeps
  per-host on the proven Landlock `ConnectTcp` + connect-notify path.

Env: **unchanged** — see §3-env. The scrub is by construction, backend-independent.

---

## §1 — THE load-bearing decision: hand-roll vs bubblewrap (SURFACE, do not decide)

The namespace mechanism is a security-critical mount/pivot_root/propagation dance. Two ways
to obtain it. This is the one call the maintainer owns before any implementation.

### Option A — hand-roll in Rust (libc `unshare`/`mount`/`pivot_root`/`umount2`) — RECOMMENDED

nub calls the namespace syscalls itself: `unshare(CLONE_NEWUSER|CLONE_NEWNS|…)`, write
`/proc/self/uid_map`+`setgroups`+`gid_map`, set `MS_REC|MS_PRIVATE` root propagation, build
the bind-mount plan from the SAME `DerivedGrants` Landlock already consumes, `pivot_root`,
`umount2(MNT_DETACH)` the old root, mount fresh `/proc`+`/dev`, then Landlock+seccomp+exec.

- **For:**
  - **Preserves the documented identity.** Every comparison doc leads with nub's Linux
    differentiator being "unprivileged, no external binaries — no `bwrap`, no `socat`, no
    `ripgrep`" (established in the prior-art survey). Shelling to bwrap directly negates the single
    thing that makes nub's Linux story distinct from SRT/Codex/Claude-Code.
  - **Same discipline nub already exercises.** nub already hand-rolls unprivileged
    kernel-primitive code of comparable delicacy: the raw `landlock_create_ruleset` ABI
    probe (`linux.rs:443`), raw seccomp filters, and the `seccomp NEW_LISTENER` `connect()`
    supervisor that reads `/proc/<pid>/mem` and injects fds via `NOTIF_ADDFD`
    (`linux_connect_notify.rs`). A hand-rolled userns backend is the same class of work, not
    a new capability.
  - **Bounded blast radius by layering.** The namespace layer sits OVER Landlock+seccomp
    (still installed): a mount-plan bug is backstopped by Landlock still enforcing
    inode-keyed fs rules and seccomp still denying ptrace/UDP/etc. A hand-roll mistake
    degrades toward the current baseline, it does not open a hole below it.
  - **No runtime dependency, no version-skew, static single binary.** Nothing to detect on
    `PATH`, no bwrap-flag drift across distro versions, consistent behavior everywhere.
  - **Precedent that the alternative is fragile.** Codex does NOT trust host bwrap — it
    **bundles** a bwrap binary (`bundled_bwrap.rs`, `BundledBwrapLauncher` opening a shipped,
    SHA-pinned binary) precisely because host versions vary. Bundling is the only way to make
    "shell to bwrap" reliable, and it reintroduces a shipped platform binary — a distribution
    + brand regression nub would be adopting to dodge a hand-roll it is already equipped for.
- **Against (honest):**
  - Mount propagation + `pivot_root` ordering is genuinely security-critical; a wrong
    propagation flag or a missed `MS_PRIVATE` is a latent escape. bwrap has years of
    hardening and a large bug-finding user base that a fresh hand-roll lacks.
  - More nub-owned code on a UB/escape-adjacent surface → mandatory VM conformance matrix +
    multi-lens adversarial review (which this doc already scopes in §5).
  - The `uid_map`/`setgroups`/`gid_map` write ordering and the AppArmor-restricted-userns
    landscape (Ubuntu 24.04) have real footguns.

### Option B — shell out to `bubblewrap`

nub spawns `bwrap --unshare-user --unshare-ns --ro-bind … --bind … --dev /dev --proc /proc …
-- <program>`, as SRT and Claude Code do.

- **For:** battle-tested mount/propagation handling; far less nub code; a known-good
  reference model; faster to ship.
- **Against:**
  - **Regresses the headline "no external binaries" claim** — the exact differentiator
    called out in all three comparison docs. A user without bwrap gets no namespace tier
    (adds a THIRD availability axis: bwrap → Landlock → seccomp-only).
  - **Version-skew + supply chain:** bwrap flags shift across versions; on older distros
    bwrap is setuid (a different privilege model). Mitigating skew means **bundling** bwrap
    (per Codex) — a shipped platform binary in the nub distribution, itself an "external
    binary" in spirit and a size/brand cost.
  - Weaker layering story: shelling out hands the whole confinement to an opaque child;
    composing it under nub's own Landlock+seccomp is more awkward than an in-process hand-roll.

### Recommendation

**Hand-roll (Option A).** The namespace tier exists to strengthen the ONE thing nub markets
as its Linux edge — dependency-free unprivileged confinement — so obtaining it by adding a
dependency is self-defeating, and bundling to hide the dependency reintroduces the shipped
binary. nub already owns comparably-delicate unprivileged-primitive code; the layering over
Landlock+seccomp bounds the risk of a hand-roll bug to "degrade to today's baseline"; and the
cost (careful mount code + VM matrix + adversarial review) is exactly the cost this crate
already pays for its seccomp/Landlock surfaces. **This is recommend-only — the maintainer
ratifies A vs B before implementation.**

---

## §2 — Runtime detection + backend selection + surfacing

### Detecting unprivileged-userns availability

Sysctl reads are necessary-but-not-sufficient on modern hosts, so the AUTHORITATIVE signal is
an empirical probe (mirroring nub's existing "test the real primitive, never infer" discipline
— `landlock_abi()` is documented as "immune to `Ruleset::create`'s degrade-to-dummy false
positive"):

1. **Fast negative (optional short-circuit):** read `/proc/sys/user/max_user_namespaces`
   (== `0` → blocked), `/proc/sys/kernel/unprivileged_userns_clone` (Debian/Ubuntu downstream
   knob, `0` → blocked), and note `kernel.apparmor_restrict_unprivileged_userns` (Ubuntu 24.04
   AppArmor can deny even when the clone knob is `1` — this is exactly why the sysctl is not
   authoritative).
2. **Authoritative probe:** `fork()` a throwaway child that calls `unshare(CLONE_NEWUSER)` and
   reports success via exit code; the parent reaps it. Microsecond cost, no side effects,
   fail-safe (any error → treat userns as unavailable). This is the only signal that accounts
   for AppArmor + seccomp-of-the-caller + the sysctls together.

Cache the result per-process (like the Landlock ABI query).

### Selection

```
apply(policy) →
  probe userns:
    available   → NAMESPACE tier: user+mount(+pid/net per §3) ns  ⊕  Landlock+seccomp underneath
    unavailable → LANDLOCK tier:  Landlock+seccomp (today's path, unchanged)
    (no Landlock either) → SECCOMP-ONLY tier (today's honest-degradation path, unchanged)
```

The namespace tier does not remove Landlock+seccomp — it adds a mount/pid/net-ns layer on
top and Landlock still runs (defense in depth). So "selection" is really "does the extra
layer engage," not "which of two mutually-exclusive backends."

### Surfacing the chosen tier

The existing `Degradation` reports **losses** (fail-safe: axes that could not be enforced).
The namespace tier is a STRENGTHENING, not a loss, so folding it into `Degradation` would
muddy that contract. Design:

- Add an **informational** `enforced_backend: LinuxBackend { Namespace, Landlock, SeccompOnly }`
  to `Prepared` (a new field, not a `Degradation.lost` entry). A caller (the build-jail) can
  log/assert which tier ran; a stock-Ubuntu run does NOT emit a warning for "namespace
  unavailable" because Landlock+seccomp is the promised floor.
- **Future strict posture (out of scope, note the seam):** a policy flag like
  `require_namespace` could elevate "userns unavailable" to a fail-closed `Degradation`
  (`lost: ["fs-namespace"]`) for a caller that demands the stronger tier. Default posture
  keeps it opportunistic. This preserves the current fail-safe-with-degradation semantics
  (`backend/mod.rs:57-86`) untouched for everyone who does not opt into strict.

---

## §3 — Per-axis mapping in the namespace model

### fs — bind-mount allowlist + `pivot_root`

The mount plan is DERIVED FROM THE SAME `DerivedGrants` Landlock consumes
(`linux_grants::derive_read_grants` / `derive_write_grants`), so the policy → grant-set
mapping is shared and the two tiers cannot diverge on WHAT is allowed (this is the backbone of
the §4 consistency contract). The grant kinds map:

| Grant | Landlock rule | Namespace mount |
|---|---|---|
| `ReadSubtree` / `ReadDir` / `ReadFile` | `PathBeneath(ReadFile\|ReadDir)` | `--ro-bind <path> <path>` |
| `WriteSubtree` | `PathBeneath(rw)` on a pre-created dir | `--bind <path> <path>` (file target bound as a file — no dir-widening) |
| essential read set (`/usr /bin /lib …`) | wholesale read grant | `--ro-bind` each |
| a deny landing inside a granted subtree (e.g. `!/etc/secret`) | **cannot carve** (residual) | **over-mount `/dev/null`** at the deny path (closes the `/etc`-wholesale + symlink-replacement residuals) |
| `/proc`, `/sys` | never granted | fresh `--proc /proc`; `/sys` not mounted |

Construction: `unshare(CLONE_NEWUSER|CLONE_NEWNS)`, map current uid→uid (single mapping; write
`setgroups=deny` before `gid_map` per the unprivileged rule), `mount(MS_REC|MS_PRIVATE)` on
`/`, build a fresh root (tmpfs), bind the plan in, `pivot_root` + `umount2(old, MNT_DETACH)`,
mount `/dev` (minimal) + `/proc`. **Landlock is STILL applied afterward** over the same grant
set — so even if a bind is wrong, the inode-keyed Landlock rules hold.

Residuals closed: `/etc`-wholesale, write-widening/new-path, symlink-replacement,
derive→open TOCTOU, bind-mounted-procfs (all per the table above). Honest non-close:
hardlink-to-secret closes only under a minimal-root plan (secret fs not bound), not by
over-mount masking.

### net — netns + routing to the loopback proxy WITHOUT external binaries

- **deny-all:** `unshare(CLONE_NEWNET)` and do not configure any route (optionally bring up
  `lo` for intra-process loopback). Route-less = airtight; replaces the seccomp AF_INET
  family-deny for this mode and is strictly stronger + simpler.
- **per-host (the hard part):** a netns child has its OWN `127.0.0.1`, so it cannot reach the
  parent's proxy on the host's loopback — this is why SRT/Claude Code bridge with **`socat`
  unix-socket bridges** across the netns boundary (`linux-sandbox-utils.ts:490-557`). nub must not add `socat`. Two std-only routes:
  - **(recommended follow-on) nub-self-reexec bridge.** nub re-execs its OWN binary as a tiny
    in-netns forwarder (`nub __net-bridge`) that listens on `127.0.0.1:<P>` inside the netns
    and forwards each connection over an inherited fd to the parent proxy — Rust std net only,
    no external binary (it is nub's own binary, not a dependency). Codex proves this shape is
    viable std-only: `proxy_routing.rs` hand-rolls exactly this (host `UnixListener` bridge +
    in-netns `TcpListener`→`UnixStream`, a `HOST_BRIDGE_READY` handshake, `lo` bring-up) in
    Rust std with NO socat — nub would do the same but re-exec its own binary rather than
    bundle one.
  - **(first cut) do NOT unshare net for per-host.** Keep user+mount(+pid) ns for the FS/env
    wins, but leave the net axis on the already-shipped Landlock `ConnectTcp` +
    connect-notify supervisor (which needs the child on the host netns to reach the proxy on
    real `127.0.0.1`). This gets every FS/PID benefit with zero net-bridge risk, and is the
    clean scoping seam: **the namespace layer unshares NET only for deny-all until the
    self-reexec bridge lands.**

### env — unchanged, with a structural bonus

Env-scrub is by CONSTRUCTION (`base_command`: `env_clear()` + the `constructed` map,
`linux.rs:324-329`) — entirely parent-side, independent of the OS confinement mechanism. It
applies **verbatim** in the namespace tier. What the namespace tier ADDS:

- With a **pid-ns + fresh `/proc`**, the parent is not visible in the child's `/proc`, so
  `/proc/<ppid>/environ` structurally does not exist (stronger than "Landlock does not grant
  `/proc`"), and the bind-mounted-procfs residual is closed. The seccomp ptrace-family deny
  still applies belt-and-braces.
- **Bonus:** in a pid+mount ns, a pure `{env:true}` passthrough could leave a FRESH `/proc`
  readable (child sees only its own ns) without re-exposing any ancestor — closing the
  "passthrough leaves `/proc` open" gap noted in the prior-art survey, which the
  Landlock tier cannot (it must hard-deny `/proc` wholesale to hold the boundary).

---

## §4 — Conformance for TWO Linux backends (no doubled maintenance)

The fixtures are already backend-agnostic: a `Fixture` is a surface `sandbox` block + expected
per-axis allow/deny verdicts (`conformance.rs`), and the OS-level tests
(`tests/linux_enforcement.rs`) drive the REAL backend against a probe program. The design:

1. **One corpus, one runner, tier-parametrized.** Add a test-only backend override (e.g.
   `NUB_SANDBOX_FORCE_BACKEND=namespace|landlock` — an internal test seam, brand-exempt) that
   forces auto-selection. The OS-level conformance runs the IDENTICAL fixture set under BOTH
   tiers: a `(fixture × tier)` matrix. No fixture is duplicated.
2. **The behavioral-consistency contract (the invariant to test):** for every fixture in the
   shared corpus, `namespace` and `landlock` MUST produce the SAME allow/deny outcome per
   axis. This is credible because both tiers consume the SAME `DerivedGrants` (§3-fs) — the
   policy→grant mapping is shared code; only the ENFORCEMENT primitive differs. A divergence
   in the matrix is a bug in one tier's primitive mapping, caught immediately.
3. **Residual fixtures become executable assertions (the extra value).** The residuals the
   namespace tier closes are exactly where the two tiers legitimately DIVERGE. Tag those
   fixtures with a `min_backend: namespace`: assert **expected-deny under namespace** AND
   **known-residual/expected-allow under landlock**. This pins each `LIMITATIONS.md` entry
   (`/etc` carve, symlink-replacement, hardlink minimal-root, new-path) as a differential
   test that PROVES the namespace tier closes it and PROVES the Landlock tier has the
   documented gap — turning the honest-limits doc into CI-enforced facts.
4. **Where it runs.** Both tiers need userns available, so the two-tier matrix runs on a
   userns-enabled Linux (the existing `landlock-vm` Lima box already has
   `unprivileged_userns_clone=1`, per the confinement audit's pinned env). A stock-CI job
   (userns blocked) runs the Landlock leg ONLY and skips the namespace leg (documented skip,
   not a failure) — which itself validates the fallback selection. Document the matrix + the
   VM/CI split in `tests/<feature>/README.md` per the repo's harness convention.

Maintenance stays single-corpus: adding a fixture covers both tiers automatically; only the
`min_backend`-tagged residual fixtures carry a per-tier expectation, and those are the whole
point.

---

## §5 — Scope, effort, and gating

**Gating (confirm): a POST-primitives expansion that does NOT gate the `sandbox-primitives`
done-gate.** The Landlock+seccomp backend is the complete, shipped baseline with honestly
documented residuals; the namespace tier is purely additive hardening for permissive hosts.
Nothing here blocks landing `sandbox-primitives`.

Rough effort (hand-roll option, the recommended path):

| Piece | Fits pre_exec? | Effort |
|---|---|---|
| userns detection probe | yes | ~0.5 day (mirrors `landlock_abi`) |
| user+mount ns: uid/gid map, propagation, bind-plan-from-`DerivedGrants`, `pivot_root`, `/dev`+`/proc`, `/dev/null` over-mount denies | yes | **1–2 wk incl. review** (the bulk; security-critical) |
| netns deny-all (empty netns) | yes | ~1 day |
| conformance two-tier parametrization + residual fixtures + VM/CI matrix | n/a | ~2–3 days |
| **First-cut subtotal** | | **~2–3 wk** |
| pid-ns process hiding (implements audit FINDING 2) | **no** — needs a fork AFTER `unshare(CLONE_NEWPID)`, so nub must OWN the spawn (fork-after-unshare) rather than use `Command::pre_exec`. The `Prepared::status` seam already lets a backend own the launch (Windows does; Linux connect-notify already routes through it) — graduate to a `clone`-based nub-owned spawn | +~1 wk (follow-on) |
| netns per-host self-reexec bridge | no (own-spawn + a helper process) | +~1 wk (follow-on, the riskiest/optional piece) |

**Two clean scoping seams keep the first cut low-risk:** (1) user+mount ns fits
`Command::pre_exec` and delivers the headline FS residual closes with Landlock still
underneath; pid-ns (which needs the fork-after-unshare + nub-owned spawn) is deferred and is
NOT required for the env boundary (Landlock-not-granting-`/proc` + seccomp already hold it).
(2) per-host net stays on the proven `ConnectTcp`+supervisor path until the self-reexec bridge
is built, so the first cut never has to solve the bridge.

Because this is escape/UB-adjacent, the implementation MUST carry: a VM conformance matrix
(both tiers), and multi-lens adversarial review (a correctness lens + an impact-analysis lens
tracing the new `enforced_backend` field's readers + the `Prepared::status` seam change +
mount-propagation review) — the audit-thread treatment this crate already applies to its
seccomp/Landlock surfaces.

---

## Open questions for the maintainer (recommend-only)

1. **§1 — hand-roll vs bwrap (THE call).** Recommendation: hand-roll. Ratify before any
   implementation.
2. **Strict posture?** Should a future policy flag be able to REQUIRE the namespace tier and
   fail-closed (`Degradation`) when userns is unavailable — or is opportunistic-only correct
   for the foreseeable build-jail use? (Design keeps opportunistic default; strict is a noted
   seam.)
3. **First-cut net scope.** Confirm per-host net stays on the current Landlock path in the
   first cut (namespace unshares NET only for deny-all), with the self-reexec bridge as an
   explicit follow-on — vs wanting the full netns-per-host bridge in the first cut.

## Follow-ups

- If §1 is ratified as hand-roll: cut a `plan-thread` to sequence the first-cut implementation
  (detection probe → user+mount ns fs → netns deny-all → two-tier conformance), each landing
  as its own reviewed PR off `sandbox-primitives`, with the VM matrix as the gate.
- The residual-fixture idea (§4.3) is worth doing EVEN IF the namespace backend is deferred:
  tagging the current `LIMITATIONS.md` FS residuals as expected-allow-under-landlock fixtures
  pins them as executable facts now and pre-builds the corpus the namespace tier will diff
  against.

## Changelog

- 2026-07-09 — Initial write-up. Design for an optional unprivileged-namespace Linux backend
  layered over Landlock+seccomp: per-residual close table, the hand-roll-vs-bwrap decision
  (recommend hand-roll) surfaced for maintainer sign-off, detection/selection/surfacing,
  per-axis (fs bind-mount+pivot_root / net netns+std-only-bridge / env unchanged) mapping,
  a single-corpus two-tier conformance contract with residual fixtures as executable
  assertions, and effort/gating (~2–3 wk first cut, POST-primitives, non-gating). Grounded in
  `sandbox-primitives` code, the reproduced-on-VM confinement audit, and Codex/SRT/Claude-Code
  prior art (``).
