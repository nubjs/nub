---
**Status:** research, 2026-07-08. Landscape survey — **is there a standard cross-platform (fs+net+env, macOS+Linux+Windows) sandboxing library, why/why-not, and is there anything nub should build on rather than reimplement.** Investigation-scope, recommend-only. No code proposed.

**Question (maintainer):** Is it really true there's no standard cross-platform sandboxing library covering fs + net + env on all three of macOS/Linux/Windows? Why isn't there one? Is there anything nub should DEPEND ON instead of hand-rolling? What genuine gap does nub's sandbox fill?

**Informs:** [`../sandbox-architecture.md`](../sandbox-architecture.md) (the built nub-sandbox engine), the engineering plan), `sandboxed-execution.md` (the original 2026-06-04 defer decision, now superseded by the active sandbox epic).
---

# Cross-platform sandboxing — prior art

## TL;DR (the four answers)

1. **No — nothing does OS-enforced fs + net + env confinement across all three of macOS + Linux + Windows from one maintained library.** The closest single crate that spans all three OSes, `cap-std`, is *by its own documentation not a sandbox* (a language-level capability discipline, not an OS boundary). The closest OS-enforced embeddable crates — `landlock` (Linux) and `birdcage` (Linux+macOS) — stop at two platforms, and `birdcage` was **archived 2026-07-06**, two days before this survey. Windows OS-level sandboxing in Rust is essentially a green field (one real crate, `rappct`).
2. **There's no de-facto standard because the three OSes solved process confinement with structurally unrelated primitives** — Linux *namespaces + Landlock LSM + seccomp-BPF* (a virtualization/allowlist model), macOS *Seatbelt/SBPL* (an in-kernel policy-language model, deprecated-with-no-replacement), Windows *AppContainer/Job-Object/Restricted-Token* (an identity/ACL model with no single "run under a profile" verb). No shared policy language, API shape, or even a shared notion of "confined." And **env-var confinement is almost universally absent** — most tools leave env to normal process inheritance. So every general-purpose tool picked one OS and went deep.
3. **Yes — nub should (and already does) build on the maintained OS primitives rather than raw syscalls.** nub's engine depends on `landlock` (11.8M downloads, actively maintained, the canonical binding) + `seccompiler` (Firecracker's, 15.5M downloads) on Linux, `windows-sys` FFI for the AppContainer backend, and hand-rolled SBPL for macOS Seatbelt. It correctly *disavows* `birdcage` (now archived) and `cap-std` (not an OS boundary) as the confinement layer. The realistic architecture is exactly what nub built: compose per-OS primitives behind one policy IR — there is no drop-in to adopt above that layer.
4. **The genuine gap nub fills:** OS-enforced, whole-descendant-tree confinement of **stock Node and its subprocesses**, on **all three OSes**, with **env as a first-class axis** and a **per-host egress proxy** — built into the launcher the user already runs. Every existing thing misses at least one of those: sandboxed runtimes (Deno/workerd) are app-level and don't survive subprocesses; process wrappers (bubblewrap, Anthropic's srt, OpenAI Codex) are separate tools and mostly two-OS; no mature tool treats env as a peer axis. That union is empty — which is what makes nub's `nub-sandbox` engine potentially the missing standard rather than a reimplementation of one.

## Landscape table

Axes legend: **fs** = filesystem read/write confinement · **net** = network egress control · **env** = environment-variable confinement as a first-class policy axis · **proc** = subprocess/exec control. "✓" = first-class; "~" = partial/coarse; "—" = absent.

### Rust, OS-enforced (embeddable)

| Project | Platforms | fs | net | env | proc | Mechanism | Maintenance | Shape |
|---|---|---|---|---|---|---|---|---|
| **landlock** (rust-landlock, official) | Linux | ✓ | ~ (ABI4+/6.7, TCP bind/connect only) | — | — | Landlock LSM | **Active** — v0.4.5 2026-05-22; 11.8M dl; canonical | Library |
| **seccompiler** (rust-vmm/Firecracker) | Linux | — | — | — | ~ (syscall gate) | seccomp-BPF, JSON→BPF AOT | **Active** — v0.5.0 2025-03; 15.5M dl | Library |
| **libseccomp** (libseccomp-rs) | Linux | — | — | — | ~ (syscall gate) | libseccomp C wrapper | **Active** — 2026-06 | Library |
| **birdcage** (Phylum) | Linux, macOS | ✓ | ✓ | — | — | Linux namespaces; macOS `sandbox_init` | **ARCHIVED 2026-07-06** — v0.8.1 2024-04; 119K dl | Library + example CLI |
| **extrasafe** | Linux | ✓ (Landlock) | — | — | ~ (seccomp) | seccomp-BPF + optional Landlock | Stale-ish — 2024-07; 27K dl | Library |
| **gaol** (Servo) | Linux, macOS, FreeBSD | ~ (read-only; write permanently denied) | ~ (outbound by port) | — | ✗ (exec denied) | Linux seccomp+ns; macOS Seatbelt | **Unpublished since 2019** (v0.2.1); repo touched 2024-12; "not mature" | Library (multiprocess) |
| **rappct** | Windows | ✓ (ACL) | ~ (capability SID) | — | ~ (Job/token) | AppContainer / LPAC | Small — 7K dl, 2025-10 | Library |
| **syscallz / seccomp-sys** | Linux | — | — | — | ~ | seccomp-BPF | Stale (2023 / 2019) | Library |
| **island** (landlock-lsm-org) | Linux | ✓ | ~ | — | — | wraps `landlock` | Active — 2026-05 | CLI |
| **sandlock-core** | Linux | ✓ | ~ | — | ~ | Landlock + seccomp + user-notify | New/small — 2026-06 | Library + C ABI |
| **yule-sandbox / skarn-sandbox** | Linux+macOS+Windows *(claimed)* | ~ | ~ | — | ~ | seccomp+Seatbelt+AppContainer behind one API | **Brand-new, unproven** — 166 / 46 dl | Library |
| **ai-sandbox** | Linux+macOS+Windows+FreeBSD+OpenBSD *(claimed)* | ✓ | ✓ | — | — | bwrap+seccomp+Landlock / Seatbelt / Restricted-Token / Capsicum / pledge | New, unvetted | Library |
| **nanosandbox** | Linux, macOS, Windows *(claimed)* | ✓ | ✓ | — | ✓ | Windows Job+Token+AppContainer; Landlock/Seatbelt elsewhere | v0.1.0 2026-01, unproven | Library |
| **wardstone / zerobox / sbe / ai-jail** | mac+Linux (Win "planned") | ✓ | ✓/~ | zerobox: ~ | ~ | auto SBPL / Landlock / bwrap; zerobox proxy-based net | New, AI-agent-tooling wave | Library/CLI |

### Rust, cross-platform but NOT an OS sandbox

| Project | Platforms | Covers | Why it's not a sandbox | Maintenance |
|---|---|---|---|---|
| **cap-std / cap-primitives** | Linux, macOS, Windows, FreeBSD | fs (capability `Dir`), coarse net `Pool` | Its own docs: *"cap-std is not a sandbox for untrusted Rust code… untrusted Rust could use `unsafe` or the unsandboxed APIs in `std::fs`."* A capability **discipline** for cooperating code (no `..`/symlink escapes), not an OS boundary. | **Active**, 13–17M dl, Bytecode Alliance |
| **wasmtime + WASI p2 / Extism / wasm-sandbox** | all | fs/net via capability handles | Requires compiling the untrusted code **to wasm** — inapplicable to native lifecycle scripts (shell/node) | Active |

### Non-Rust OS-level tools (the landscape context)

| Tool | Platforms | fs | net | env | proc | Mechanism | Shape |
|---|---|---|---|---|---|---|---|
| **bubblewrap** (bwrap) | Linux | ✓ | ~ (netns on/off) | ~ (`--clearenv`/`--setenv`) | ✓ (ns) | unprivileged user namespaces | CLI (Flatpak/srt/Codex engine) |
| **nsjail** (Google) | Linux | ✓ | ~ | **✓ (`--env`/`keep_env`)** | ✓ | namespaces + cgroups + seccomp (Kafel DSL) | CLI (+lib core) |
| **firejail** | Linux | ✓ | ~ | ~ | ✓ | SUID helper + ns + seccomp | CLI (SUID = own CVE surface) |
| **sandbox2 / Sandboxed-API** (Google) | Linux | ~ | ~ | — | ✓ | seccomp-BPF + ptrace broker | **Embeddable C++ lib** |
| **minijail** (ChromeOS) | Linux | ✓ | ~ | — | ✓ | namespaces + seccomp | CLI + lib |
| **gVisor / runsc** | Linux host | ✓ | ✓ | ~ | ✓ | userspace application kernel (Sentry), systrap/KVM/ptrace | OCI runtime |
| **Deno permission model** | mac+Linux+Win | ✓ | ✓ | ✓ | ✓ | **app-level** allow/deny at the Rust ops layer — NOT an OS boundary | runtime pattern |
| **workerd** (Cloudflare) | mac+Linux+Win | — | — | — | — | V8 isolates (JS-heap isolation); its README says use a VM for untrusted code | runtime |
| **macOS `sandbox_init`/`sandbox-exec`** (Seatbelt) | macOS | ✓ | ✓ | — | ✓ (exec gate) | TrustedBSD MAC hooks, SBPL profile | C API + CLI, **deprecated, no replacement** |
| **Windows AppContainer / Job / Restricted Token** | Windows | ✓ (ACL, allow-polarity) | ~ (capability SID) | — | ✓ (Job) | LowBox token + capability SIDs + Job Objects | Win32 APIs (compose yourself; Chromium `//sandbox` is the reference) |

### Reference architectures (nub's exact shape — study, don't necessarily depend on)

| Project | Platforms | Notes |
|---|---|---|
| **OpenAI Codex `codex-rs`** | mac + Linux + **Windows** | Production Rust CLI sandboxing LLM-generated commands. macOS Seatbelt, Linux bwrap+Landlock+seccomp, Windows restricted-tokens+ACLs+synthetic SIDs (two modes: AppContainer-style elevated + non-admin restricted-token). **The best concrete cross-platform-incl-Windows Rust reference found.** MIT/Apache, monorepo (not a crate). |
| **Anthropic `sandbox-runtime` (srt)** | mac + Linux | TypeScript CLI. Seatbelt + bwrap; net axis via **HTTP/SOCKS5 proxy + domain allowlist** (no MITM) — the pattern nub's egress proxy mirrors. Rust port `sandbox-runtime-rs` tracks its schema. |
| **Microsoft `mxc`** | multi | Rust core + TS SDK, one JSON policy schema over many backends (ProcessContainer/WindowsSandbox/LXC/bwrap/Seatbelt/microVM). Heavier; the policy-schema-over-backends design is the studyable part. |
| **Cackle** (cargo) | Linux | The *cargo* analog of nub's problem — `build.rs`/proc-macros run unsandboxed; Cackle is an API-capability ACL over them. Same supply-chain failure mode, different ecosystem. |
| **Ringfence** (pnpm-referenced) | — | The one PM-ecosystem-native attempt at real postinstall confinement (pnpm docs link it) rather than allowlisting. |

## Answering the four questions in full

### Q1 — Does anything do fs + net + env across all three OSes?

**No.** Split the field three ways:

- **Cross-platform but not OS-enforced.** `cap-std` is the only actively-maintained, high-adoption library that genuinely spans Linux/macOS/Windows/FreeBSD — but it is a *capability discipline*, not a sandbox, and says so in its own README. It stops cooperating code from traversing out of a directory; it does nothing against `unsafe`, direct `std::fs`, or a malicious native addon in the same process. Deno's permission model is likewise uniformly cross-platform and covers all four axes (it's the only thing that treats env as first-class *and* is cross-platform) — but it too is app-level: enforced by Deno's own Rust cooperating at the ops layer, bypassable by any native/FFI call or a runtime bug. Neither is an OS boundary.
- **OS-enforced but one/two platforms.** `landlock` (Linux, fs + narrow net) is the most maintained and highest-adoption *real* sandboxing primitive in Rust. `birdcage` (Linux+macOS, fs+net) was the best-known embeddable two-OS option — and it was **archived on 2026-07-06**, read-only, no successor named. Neither reaches Windows; neither does env.
- **Windows OS-level Rust sandboxing is a green field.** The only credible native entry is `rappct` (AppContainer/LPAC), single-platform, separately maintained. The two crates that *claim* all three OSes (`yule-sandbox`, `skarn-sandbox`) are days/weeks old with double-digit download counts — not a credible standard.

The env axis specifically: across the entire landscape, only **nsjail** (`--env`/`keep_env`), **bubblewrap** (coarse `--clearenv`/`--setenv`), **firejail** (coarse), and **Deno** (app-level) treat environment variables as a policy axis at all. sandbox2, minijail, gVisor, Seatbelt, AppContainer, `landlock`, `cap-std` — none do. Env-scrubbing is nub's headline control (see [`../sandbox-architecture.md`](../sandbox-architecture.md) §5), and essentially nothing off-the-shelf provides it.

### Q2 — Why is there no de-facto standard?

Three real reasons, in order of weight:

1. **The OS primitives are structurally unrelated — there is nothing to wrap once.** Linux models confinement as *virtualization + allowlist*: namespaces give a different view of the same kernel (unprivileged since ~2013), Landlock is an allow-only filesystem LSM, seccomp-BPF is an orthogonal syscall filter. macOS models it as an *in-kernel policy language*: Seatbelt evaluates an SBPL (a Scheme dialect) profile per-operation via TrustedBSD MAC hooks — no namespace concept, and the public entry point (`sandbox_init`/`sandbox-exec`) has been *deprecated since 10.8 with no replacement offered*, so you're building on officially-legacy API. Windows models it as *identity + ACL*: an AppContainer/LowBox token carries a package SID + capability SIDs, and every object access is checked against DACLs of *opposite polarity* from Linux (allow-by-ACL, not default-deny) — with **no single "run under a profile" verb**; you compose Restricted Tokens + Job Objects + AppContainer + integrity levels yourself (Chromium's `//sandbox` is the canonical example of how much composition that takes). Three unrelated enforcement substrates, three policy models, no shared abstraction → a cross-platform tool must reimplement the *policy* three times. Every general-purpose tool (bwrap/nsjail/firejail/minijail/sandbox2/gVisor) responded by picking one OS and going deep.
2. **No env axis in the OS primitives.** The one axis that matters most for supply-chain exfiltration (ambient cloud creds/tokens handed to every dependency) is simply *not expressed* by Seatbelt, AppContainer, or Landlock — env is an exec-time concern outside their vocabulary. So even a would-be standard has to invent the env story itself, which fragments designs further.
3. **Abandonment + recency churn.** The space is littered with stale/abandoned crates (`gaol` unpublished since 2019, `seccomp-sys` 2019, `syscallz` 2023) and the best two-OS option (`birdcage`) just archived. Meanwhile a 2024–2026 *wave* of AI-agent sandboxing tools (Codex, srt, mxc, zerobox, wardstone, ai-sandbox, nanosandbox, …) is re-attacking the same problem in parallel with no convergence yet. The need is finally acute (agents running untrusted LLM code; supply-chain attacks), but the field is pre-consolidation. Even Rust's *own* `build.rs` supply-chain problem is unsolved (the 2024H2 Rust Project Goal "Explore sandboxed build scripts"; Cackle is the lone real attempt) — a strong tell that this is genuinely hard, not merely neglected.

### Q3 — What should nub depend on vs hand-roll?

**nub already gets this right.** Verified in-tree: the vendored aube lifecycle-script jail (`vendor/aube/Cargo.toml`) depends on `landlock = "0.4"` + `seccompiler = "0.5"`, and the newer `nub-sandbox` engine (on the `sandbox-primitives` branch) depends on `landlock` + `seccompiler` + `windows-sys` (AppContainer FFI) with hand-rolled SBPL for macOS. It explicitly *disavows* `birdcage` (the sandbox design record calls it "the disavowed crate" — the `Degradation` fail-safe concept was ported, the engine rewritten). The triage:

- **`landlock` (the crate) — DEPEND. Already does.** This is the standout: canonical, kernel-team-adjacent, 11.8M downloads, actively pushed (2026-06). Hand-rolling the raw Landlock ABI (as some code does for the not-yet-created-path probe) buys nothing over it for the common rules. Tradeoff: none material; MSRV 1.71 is fine. It replaces raw Landlock syscalls in the Linux fs backend.
- **`seccompiler` — DEPEND. Already does.** Firecracker's, 15.5M downloads (mostly transitive), maintained. Replaces raw seccomp-BPF assembly for the net/ptrace deny filter. Tradeoff: AOT JSON→BPF model is slightly awkward for a few dynamic rules, but proven at scale. (Alternative `libseccomp` needs the C lib linked; `seccompiler` is pure-Rust and the better fit.)
- **`windows-sys` — DEPEND for the AppContainer backend. Already does.** There is no higher-level Rust AppContainer library worth adopting (`rappct` is small/single-maintainer, and pulling it in wouldn't cover nub's env/proxy integration). Raw FFI is the right call here; the composition burden is inherent to Windows (Q2). Tradeoff: most code to own, but unavoidable — study `codex-rs`'s Windows restricted-token path as the reference.
- **macOS Seatbelt — hand-roll SBPL. Correct.** No maintained Rust Seatbelt crate exists now that `birdcage` is archived; `sandbox_init` is a thin C call and SBPL generation is nub's real value (the deny-default base + scratch grants + firmlink canonicalization). Nothing to depend on above the libc call.
- **`birdcage` — do NOT adopt (correctly disavowed). Archived 2026-07-06.** Even before archival it was Linux+macOS-only with no env axis and no Windows; adopting it would have been a dead-ended two-OS dependency. nub's decision to port the one good idea (`Degradation`) and write its own engine is vindicated by the archival.
- **`cap-std` — do NOT use as the confinement layer (it is not one).** It *could* be useful narrowly for hardening nub's *own* path handling against `..`/symlink bugs in the compiler/launcher, but it is not a substitute for any OS backend and must not be presented as confinement. Low priority; the canonicalization traps in `../sandbox-architecture.md` §7 are the real path-safety concern and are handled explicitly.
- **`yule-sandbox`/`skarn-sandbox`/`ai-sandbox`/`nanosandbox` — do NOT adopt.** They claim exactly nub's three-OS story, which makes them tempting, but all are days/weeks old, single-maintainer, double/triple-digit downloads, unproven. Depending on an unvetted crate for the *security boundary* is the worst place to take that risk. Worth watching as convergence signals; not dependencies.

Net: nub depends on the two mature, maintained OS *primitives* (`landlock`, `seccompiler`) plus platform FFI, and hand-rolls exactly the layers where no maintained abstraction exists (SBPL, AppContainer composition, the policy IR, the env axis, the egress proxy). That is the correct dependency posture — and it aligns with the PM-purity/no-heavy-deps constraint, since `landlock`/`seccompiler`/`windows-sys` are lightweight, widely-used, and platform-gated.

### Q4 — The genuine gap nub fills

nub sits in an empty quadrant that `../sandbox-architecture.md` §1 already names, and this survey confirms is genuinely empty:

- **Sandboxed runtimes** (Deno, workerd) bake permissions into a *new engine* and enforce at the *app layer* — so (a) they don't run stock Node, and (b) Deno's permissions famously don't survive a subprocess: the moment a build tool shells out, the sandbox evaporates. workerd is V8-isolate isolation only and its own README says to wrap it in a VM for untrusted code.
- **Process wrappers** (bubblewrap, Anthropic srt, OpenAI Codex, firejail) enforce at the right layer (OS, whole tree) but are *separate tools you must remember to invoke*, and most are one- or two-OS (bwrap/srt/firejail are Linux/macOS only; only Codex reaches Windows). None treat env as a first-class axis with a curated build-hint allowlist.
- **Package managers** (npm, pnpm, bun) don't OS-sandbox at all — all three are *allowlist/consent* models (`allowScripts` / `allowBuilds` / `trustedDependencies`); a compromised *trusted* dep's postinstall runs with full user privileges. cargo's `build.rs` has the identical unsolved gap.

nub's union — **OS-level enforcement that survives subprocesses, built into the launcher the user already runs, over stock Node, on all three OSes, with env as a first-class axis and a per-host egress proxy** — is filled by nothing else. Two properties fall out of owning the spawn: whole-tree confinement inherited by every descendant for free, and one engine covering both PM lifecycle scripts and app runtime. The env-scrub in particular is close to unique in the entire landscape.

**Framing consequence:** because the field is pre-consolidation (Q2 reason 3) and the three-OS + env + subprocess-survival combination is unoccupied, nub's `nub-sandbox` engine is a plausible candidate for *the* missing cross-platform standard rather than a reimplementation of an existing one — modulo the honest threat-model tier (srt/Codex-class: OS primitives vs semi-trusted same-user code, not a hardware-VM boundary; see `../sandbox-architecture.md` §1). The main external validation to track is whether `codex-rs`, `mxc`, or one of the new unifying crates consolidates the three-OS story first.

## Follow-ups

1. **Confirm `birdcage`'s archival cause** (2026-07-06, no reason in-repo) before citing it as a cautionary precedent externally — check the Phylum issue tracker / whether it folded into another tool. Low effort, improves the provenance.
2. **Study `codex-rs`'s Windows backend** (restricted tokens + synthetic SIDs + ACLs, non-admin mode) directly against nub's AppContainer approach — it's the best cross-platform-incl-Windows Rust reference and may de-risk the still-⚠ Windows non-elevated file-confinement question in `../sandbox-architecture.md` §7. Clone into `` per repo convention.
3. **Study Anthropic srt's proxy-based net filtering** against nub's SNI-proxy design (§6) — same no-MITM domain-allowlist pattern; worth a differential read for edge cases (malformed ClientHello, SOCKS5 handling).
4. **No code changed; recommend-only.** Any dependency-adoption or architecture change implied here is a security-posture call the maintainer owns — this doc profiles and recommends, it does not decide. The current dependency posture (`landlock` + `seccompiler` + platform FFI + hand-rolled SBPL, `birdcage`/`cap-std` disavowed) is assessed as **correct** and needs no change.

Follow-ups: none blocking.

## Changelog
- 2026-07-08 — Initial write-up. Surveyed the Rust ecosystem (gaol, birdcage [archived 2026-07-06], extrasafe, landlock, seccompiler, cap-std, rappct, and the 2024–2026 AI-agent-sandbox crate wave), the non-Rust landscape (bwrap, nsjail, firejail, sandbox2, minijail, gVisor, Deno, workerd, Seatbelt, Windows AppContainer), and reference architectures (OpenAI codex-rs, Anthropic srt, Microsoft mxc, Cackle, Ringfence). Answered the four questions: no cross-platform OS-enforced fs+net+env library exists; the reason is structurally-unrelated per-OS primitives + a near-universal missing env axis + pre-consolidation churn; nub correctly depends on `landlock`+`seccompiler`+platform FFI and hand-rolls where no abstraction exists (`birdcage`/`cap-std` rightly disavowed); the genuine gap is OS-enforced, subprocess-surviving, three-OS, env-first-class confinement of stock Node built into the launcher.
</content>
</invoke>
