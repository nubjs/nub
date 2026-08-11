# The sandbox engine

`crates/nub-sandbox` is the confinement engine that enforces the build jail. It has no command grammar, reads no configuration file, and knows nothing about the package manager. A *front end* is the embedder: it discovers configuration, parses it, resolves the host's paths and environment, and then drives the engine.

This document is canonical for the engine's structure — the two calls that cross its boundary, the policy IR they move, how a backend is selected, and what happens when one cannot enforce. What the jail grants and how a grant is decided is [`build-jail.md`](build-jail.md); the per-OS enforcement mechanics and the approaches rejected on the way to them are the platform ledgers.

> **Status: unshipped.** Everything here exists on a feature branch. No release contains it.

## The two-boundary seam

One engine serves two products — the build jail, and the general `nub sandbox` scope a user authors policy for — and both reach it through the same two calls.

```
  EMBEDDER — nub-cli, driving the aube lifecycle interposition
  discovers config · resolves host paths + env · assigns trust capabilities
        │                                          │
   ─────┼──── Boundary A ────────────────────┬─────┼──── Boundary B ──────────────
        ▼                                    │     ▼
  ┌──────────────────────┐   SandboxPolicy   │  ┌──────────────────────────────┐
  │ COMPILER             │  ────────────────▶│  │ BACKEND    pure IR → OS call │
  │ the only code that   │      (the IR)     │  │                              │
  │ knows surface syntax │                   │  │  Linux   → Landlock, or bwrap│
  │ presets · reuse ptrs │                   │  │  macOS   → Seatbelt          │
  │ glob order · env DSL │                   │  │  Windows → AppContainer, or  │
  └──────────────────────┘                   │  │            a local account   │
                                             │  │  other   → env-scrub only    │
                          Linux only:        │  └───────────────┬──────────────┘
                          earliest_bootstrap ┘                  │
                          (first main action)   Prepared ───────┴──▶ spawn
                                                Degradation ─────────▶ warn · or refuse
```

| Call | Signature | Owns |
| --- | --- | --- |
| Boundary A | `compile(&Value, &CompileCtx) -> Result<SandboxPolicy, CompileError>` | Every piece of surface syntax — presets, `...:#/pointer` list reuse, glob ordering, the environment grammar. A backend never sees any of it. |
| Boundary B | `apply_with_runtime(&SandboxPolicy, CommandSpec, &RuntimeCapability) -> Result<Prepared, Degradation>` | Translating the IR into OS primitives, or refusing. |
| Linux only | `earliest_bootstrap() -> io::Result<RuntimeCapability>` | The embedder's **first** main action. Linux confinement fails closed without it; bare `apply` stays valid only for an unconfined Linux command. |

The model is **compile then apply**. The IR is compiled once and consumed in process. It round-trips through serde for fixtures and for a debug dump, but it is never deserialized on the enforcement path, so no configuration re-read sits between the two calls. One policy can drive many applies.

### The engine holds no package-manager type

`nub-sandbox` declares no dependency on `nub-cli`, `nub-core` or `vendor/aube`, and no type from any of them crosses either boundary. Everything the seam moves is plain data the engine owns: a `serde_json::Value` in, the IR through, `Prepared` / `Degradation` / `CompileError` out. That is what lets aube's lifecycle wire to two functions without dragging a package-manager type across the line, and it is asserted rather than left to convention.

## The policy IR

`SandboxPolicy` is fully resolved plain data: no presets, no reuse pointers, no glob-of-globs, no sentinels. Four axes compose independently.

| Axis | Shape | Enforced by |
| --- | --- | --- |
| `fs` | One ordered rule list plus a `default_effect` base. Each Allow carries `Read` or `ReadWrite`. | The OS filesystem primitive. |
| `net` | Ordered host, CIDR and `<private>` rules. The proxy posture and inspection tier are derived by the compiler, never authored. | The OS. For the build jail this collapses to a per-package boolean. |
| `env` | `constructed` — the literal child environment — plus a schema, the names deliberately `withheld`, and the concrete `sensitive_keys` a redactor must scrub. | Construction, not interception. |
| `pid` | An `isolate` flag. | A Linux PID namespace, where one is available. |

Both rule-bearing axes evaluate identically: walk the entries, the **last match wins**, and anything unmatched falls back to `default_effect`. There is no deny priority and no magic floor — the secret denies the compiler injects are ordinary entries under the same rule. An empty ruleset denies everything.

Two properties are structural rather than validated. **Write without read is unrepresentable**, because `FsAccess` has no write-only variant and the surface has no spelling for one. And **environment confinement is construction**: reading a variable is a plain memory read of the populated environ rather than a syscall, so nothing can intercept it, and a withheld variable is simply absent from the map the child is launched with.

## Why the build jail compiles to a pure allowlist

A `build_jail` marker on the policy — skipped in serde, because it is provenance for backend selection rather than confinement — flags a build-jail compile, and `enforce_pure_allowlist` then strips every deny rule from it.

The reason is that deny-inside-allow is inexpressible on the zero-privilege mechanisms. Landlock unions its rules and has no deny primitive at any ABI, and an explicit deny ACE naming a Windows AppContainer's own SID is inert against that AppContainer's own child. A secret is therefore protected by never being granted, and a deny rule surviving into a build-jail policy is either redundant or a sign that a grant is too broad — which is fixed by narrowing the grant.

**The invariant binds the backends, not only the compiler.** Stripping every deny is worth nothing if a backend then synthesizes one out of an allow, which is what the Seatbelt write loop once did by rendering a read-only Allow as `(deny file-write* …)`, so the jail's own grants cancelled each other while the IR still looked deny-free. A backend may render an Allow only as permission.

## Backend selection

The policy's shape picks the backend, and the two products diverge on two of the three platforms.

| Platform | Build jail — pure allowlist, no elevation | `nub sandbox` — deny inside allow, private tmp, PID isolation |
| --- | --- | --- |
| Linux | Landlock plus a seccomp socket-family filter, with no namespace | Bubblewrap, with user, mount, PID and network namespaces |
| macOS | Seatbelt, via `sandbox-exec` | Seatbelt, plus a loopback egress proxy |
| Windows | An AppContainer LowBox token | A dedicated local account plus WFP |
| anything else | An environment-scrub skeleton that reports `fs` and `net` as not enforced | Same |

The Linux split is the load-bearing one. Bubblewrap needs an unprivileged user namespace, which stock Ubuntu 24.04 denies by default, and universal unprivileged operation is what defines the build jail — so the jail takes Landlock or nothing, and that decision is made in one place. The reverse gate holds as well: a policy that is not the build jail is refused Landlock, because Landlock has neither a deny primitive nor any namespace and so cannot carry that shape at all.

## Failing closed

The enforcement contract is fail-safe with degradation. A backend never silently drops an axis it claimed to enforce.

| Outcome | Representation | What the caller does |
| --- | --- | --- |
| Full enforcement | `Ok(Prepared)`, with `degradation.is_full()` | Launch. |
| An axis could not be enforced | `Ok(Prepared)`, with a non-empty `lost` | Launch, and surface a warning naming the lost axes and the reason. |
| A required axis is unenforceable | `Err(Degradation)` | Refuse. The engine produces no launchable plan. |

A backend also distinguishes over-confinement from a hole, so a policy that ends up stricter than authored is not reported in the same terms as one with a gap in it.

Every backend returns a `Prepared` plan whose command is **private**. Callers launch through `spawn`, `status` or `output`, which is what keeps Linux's startup verification, Windows' enforcement, and per-launch resource ownership impossible to bypass. Windows owns its full synchronous spawn lifecycle, because neither of its launches can be a pre-built `std::process::Command` and both need per-run ACL grants torn down after the child exits.

## The launcher-handoff contract

For some guarantees the engine constructs the child's confinement correctly, but a complete guarantee needs the launcher — which owns the parent process and the working-directory layout — to satisfy something a front-end-less engine cannot. These define the seam rather than describing engine defects.

| Obligation | Why the engine cannot close it |
| --- | --- |
| macOS toolchain read-confine | A non-system interpreter needs its toolchain directory in the read-allow set. The engine grants the program file alone and does not probe the host for the rest. |
| Windows loopback exemption | Per-host egress needs a registered exemption so the child can reach a proxy. The build jail has neither, so this binds `nub sandbox` only. |
| The trust boundary | The engine cannot detect trust. The caller assigns each compile its capabilities: approved user configuration gets `$(…)` substitution and credential brokering, and dependency-controlled configuration gets neither. |

Capability is decided by scope **identity**, never inferred from a repository or checkout heuristic, and nub never tries to guess whether a checkout is trustworthy. Filesystem `$(…)` substitution sits outside the gate entirely and is unconditional in every scope, because a filesystem path is inert data rather than a credential or an injected header.

## Cross-references

- [`build-jail.md`](build-jail.md) — what the jail grants, and how a grant is decided
- [`build-jail-architecture.md`](build-jail-architecture.md) — the candidate architectures, each with a verdict
- [`build-jail-linux.md`](build-jail-linux.md), [`build-jail-macos.md`](build-jail-macos.md), [`build-jail-windows.md`](build-jail-windows.md) — per-OS enforcement mechanics and every approach tried
- `crates/nub-sandbox/LIMITATIONS.md` — the residuals each backend does not close, and the runtime signals that report them
- [`../research/sandbox-crate-structure.md`](../research/sandbox-crate-structure.md) — whether the crate is publishable standalone, and what a split would cost

## Changelog

- 2026-08-11 — Initial write-up. Records the engine structure the surrounding documents assumed but never stated: the two-boundary seam and the Linux bootstrap call, the IR and its two structural properties, the pure-allowlist invariant as an obligation on backends, backend selection across both products, the degradation contract, and the launcher-handoff items.
