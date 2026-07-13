//! nub-sandbox — the OS-enforced sandbox ENGINE, PM-pure by construction.
//!
//! This crate is the frontend-less confinement engine. It has NO command grammar,
//! reads NO config file, and knows nothing about the package manager. A *front-end*
//! (the build-jail, a runtime profile, `nub sandbox -- <cmd>`) is the EMBEDDER: it
//! discovers config, parses it, resolves the host's paths/env, then drives this
//! engine through the two-call seam below. The companion `EMBEDDER.md` is the full
//! integration guide (usage sketch, boundary tables, launcher-handoff contract);
//! this module doc is the authoritative summary that lives with the code.
//!
//! # The embedder seam — two calls over already-parsed data
//!
//! The whole public surface is two functions and the plain-data types they move
//! (the two boundaries of design.md §2):
//!
//!   - [`compile`]`(surface: &Value, ctx: &`[`CompileCtx`]`) -> Result<`[`SandboxPolicy`]`, `[`CompileError`]`>`
//!     — **Boundary A**: the surface `sandbox` JSON (a parsed `serde_json::Value`)
//!     plus host context (homes/cwd/trust/ambient-env) resolve to the flat policy
//!     IR. This is the ONLY code that understands surface syntax (presets, `"..."`
//!     spread, glob ordering, the env grammar); a backend never sees any of it.
//!     Use [`compile_with_warnings`] to also surface non-fatal [`CompileWarning`]s.
//!   - [`apply`]`(policy: &`[`SandboxPolicy`]`, spec: `[`CommandSpec`]`) -> Result<`[`Prepared`]`, `[`Degradation`]`>`
//!     — **Boundary B**: a resolved policy plus a host-provided command produce a
//!     launch-ready child, or a fail-closed [`Degradation`] when a required axis is
//!     unenforceable. The embedder then surfaces [`Prepared::degradation`] and
//!     spawns via [`Prepared::status`] — the UNIFORM launch verb (do not call
//!     `command.status()` directly; Windows enforcement + the egress proxy ride the
//!     `status` seam, not the `command` field).
//!
//! The model is COMPILE-THEN-APPLY: the IR is compiled once and consumed in-process
//! by [`apply`]; it is `serde`-round-trippable for fixtures/debug-dump but is NEVER
//! deserialized on the enforcement path (no config re-read between compile and
//! apply). One policy can drive many [`apply`] calls.
//!
//! # PM-purity invariant (the Boundary-B guarantee — a done-gate assertion)
//!
//! NO `nub-cli` / `nub-core` / `vendor/aube` (PM) type crosses either boundary, and
//! this crate declares NO dependency on any of them (see `Cargo.toml`). Everything
//! the seam moves is plain data owned here — a `serde_json::Value` in, the IR
//! ([`SandboxPolicy`]) through, [`Prepared`]/[`Degradation`]/[`CompileError`] out.
//! That is what keeps the embedder seam clean: aube's lifecycle wires to these two
//! fns without dragging a PM type across the line. Do NOT add a PM dependency here;
//! an impact-analysis review leg asserts the dependency graph.
//!
//! # Launcher-handoff contract (the embedder's obligations)
//!
//! For some guarantees the engine constructs the child's confinement correctly but
//! a COMPLETE guarantee needs the launcher (which owns the parent process + the
//! work-dir layout) to satisfy a contract the frontend-less engine cannot. These
//! are NOT engine defects — they define the seam. The current set (full detail in
//! `LIMITATIONS.md` "Launcher-handoff items"):
//!
//!   - **macOS toolchain read-confine** — a non-system interpreter (Homebrew/nvm
//!     Node) needs its toolchain dir in the read-allow set; the engine grants the
//!     program file only and does not probe the host for it.
//!   - **macOS parent-env scrub** — the engine scrubs the CHILD's env, not nub's
//!     own; the launcher must not hold ambient secrets in nub's environ at spawn
//!     (co-resident `KERN_PROCARGS2` ascendant-env read).
//!   - **Windows loopback exemption + clean-DACL work root** — per-host egress (and
//!     the MITM tier) need a registered loopback exemption so the child can reach
//!     the proxy; confined work dirs must sit under a CLEAN-DACL root (no inherited
//!     `ALL APPLICATION PACKAGES` allow-ACE).
//!   - **Per-host proxy wiring** — the launcher provisions/exempts the loopback
//!     proxy path per OS as above.
//!   - **Untrusted-config trust boundary** — the engine CANNOT detect trust; the
//!     CALLER sets [`CompileCtx::trusted`] (gates `$(…)`) and secures untrusted-config
//!     usage (e.g. PR-CI). A `dependenciesMeta` grant is compiled untrusted.
//!
//! # Net axis — the per-host egress proxy and the MITM tier
//!
//! When a policy enforces per-host net, [`apply`] starts a loopback [`EgressProxy`]
//! (no MITM: it gates the CONNECT/SOCKS target + the cleartext TLS SNI, then
//! blind-forwards) and stashes it on [`Prepared`] so it outlives the child. The
//! per-host decision is a [`GrantDecider`] seam ([`StaticDecider`] here); a
//! capability-derived **MITM tier** (credential brokering via an ephemeral CA passed
//! to the child through an env bundle) is a landed-but-held extension that swaps in
//! through the same seam — see PR #414. The core [`compile`]/[`apply`] seam is
//! unchanged by it.
//!
//! # Backend status
//!
//! The compiler + IR + matcher are complete and exhaustively tested. [`apply`]
//! enforces fs/net/env on macOS (Seatbelt), real-kernel Linux (Bubblewrap),
//! and Windows (AppContainer LowBox), each proven by per-axis enforcement tests with
//! negative controls; any other OS runs an env-scrub-only skeleton that reports fs/net
//! as NOT enforced (never silent). The [`conformance`] harness evaluates
//! compiler/matcher verdicts against committed fixtures — the engine-pure half of the
//! cross-platform bar. Bounded residuals + the launcher-handoff contract are recorded
//! honestly in `LIMITATIONS.md` alongside the runtime [`Degradation`] signals; read it
//! before relying on any single-axis guarantee.

pub mod backend;
pub mod compiler;
pub mod conformance;
pub mod matcher;
pub mod policy;
pub mod proxy;

pub use backend::{CommandSpec, Degradation, Prepared, apply};
pub use compiler::{
    CommandRunner, CompileCtx, CompileError, CompileWarning, compile, compile_with_warnings,
};
pub use matcher::Homes;
pub use policy::SandboxPolicy;
pub use proxy::{Decision, EgressProxy, GrantDecider, Host, StaticDecider};

/// Whether applying this policy needs the embedder to supply bounded current-path
/// roots for wildcard deny inventory. Exact denies are enforced directly and need
/// no enumeration.
pub fn requires_deny_search_roots(policy: &SandboxPolicy) -> bool {
    policy.fs.rules.entries.iter().any(|rule| {
        if rule.effect != policy::Effect::Deny {
            return false;
        }
        let literal = rule
            .matcher
            .as_str()
            .strip_suffix("/**")
            .unwrap_or(rule.matcher.as_str());
        literal.is_empty() || literal.contains(['*', '?', '[', '{'])
    })
}
