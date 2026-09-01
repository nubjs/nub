//! nub-sandbox — the OS-enforced sandbox ENGINE, PM-pure by construction.
//!
//! This crate is the frontend-less confinement engine. It has NO command grammar,
//! reads NO config file, and knows nothing about the package manager. A *front-end*
//! (the build-jail, a runtime profile, `nub sandbox -- <cmd>`) is the EMBEDDER: it
//! discovers config, parses it, resolves the host's paths/env, then drives this
//! engine through the two-call data seam below. Linux additionally installs an
//! earliest process hook. The companion `EMBEDDER.md` is the full
//! integration guide (usage sketch, boundary tables, launcher-handoff contract);
//! this module doc is the authoritative summary that lives with the code.
//!
//! # The embedder seam — two calls over already-parsed data
//!
//! The data path has two calls over the plain-data types below (the two boundaries
//! of design.md §2). On Linux, the embedder also calls [`earliest_bootstrap`] as its
//! first main action and passes that capability to [`apply_with_runtime`]:
//!
//!   - [`compile`]`(surface: &Value, ctx: &`[`CompileCtx`]`) -> Result<`[`SandboxPolicy`]`, `[`CompileError`]`>`
//!     — **Boundary A**: the surface `sandbox` JSON (a parsed `serde_json::Value`)
//!     plus host context (homes/cwd/trust/ambient-env) resolve to the flat policy
//!     IR. This is the ONLY code that understands surface syntax (presets, `...:#/pointer`
//!     list reuse, glob ordering, the env grammar); a backend never sees any of it.
//!     Use [`compile_with_warnings`] to also surface non-fatal [`CompileWarning`]s.
//!   - [`apply_with_runtime`]`(policy: &`[`SandboxPolicy`]`, spec: `[`CommandSpec`]`, runtime: &`[`RuntimeCapability`]`) -> Result<`[`Prepared`]`, `[`Degradation`]`>`
//!     — **Boundary B**: a resolved policy plus a host-provided command produce a
//!     launch-ready child, or a fail-closed [`Degradation`] when a required axis is
//!     unenforceable. The embedder then surfaces [`Prepared::degradation`] and
//!     launches through [`Prepared::spawn`], [`Prepared::status`], or
//!     [`Prepared::output`]. The backend command is private so Linux verification,
//!     Windows enforcement, and per-launch resource ownership cannot be bypassed.
//!
//! Other platforms retain [`apply`], and bare `apply` remains valid for an unconfined
//! Linux command; Linux confinement fails closed without the early capability.
//!
//! The model is COMPILE-THEN-APPLY: the IR is compiled once and consumed in-process
//! by the apply seam; it is `serde`-round-trippable for fixtures/debug-dump but is NEVER
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
//!   - **Windows loopback exemption** — per-host egress (and the MITM tier) need a
//!     registered loopback exemption so the child can reach the proxy. The sibling
//!     clean-DACL work-root obligation is retired: the engine checks the work root for
//!     `ALL APPLICATION PACKAGES` reach itself, and FAILS CLOSED on `fs-root` when it finds
//!     reach it did not put there — an ACE the engine published on its own
//!     [`policy::FsOrigin::NubOwnedPublic`] caches is excused, since the child already holds
//!     a read grant on that subtree. "Degrades" here previously read as a soft loss; it is
//!     an `Err`, and the embedder refuses the launch. See `LIMITATIONS.md`.
//!   - **Per-host proxy wiring** — the launcher provisions/exempts the loopback
//!     proxy path per OS as above.
//!   - **Untrusted-config trust boundary** — the engine CANNOT detect trust; the
//!     CALLER assigns the compile its [`ScopeCapabilities`] (via [`CompileCtx::caps`])
//!     — the `env_substitution` / `credential_broker` gates — and secures untrusted-config
//!     usage (e.g. PR-CI). A `dependenciesMeta` scope compiles with no capabilities.
//!
//! # Net axis — the per-host egress proxy and the MITM tier
//!
//! When a policy enforces per-host net, [`apply`] starts a loopback [`EgressProxy`]
//! and stashes it on [`Prepared`] so it outlives the child. The connection tier gates
//! the CONNECT/SOCKS target + cleartext TLS SNI, then blind-forwards. The
//! capability-derived MITM tier terminates only exact brokered hosts (or all allowed
//! hosts in explicit terminate mode), verifies the real upstream, and replaces opaque
//! markers only in HTTP/1.1 request-header values. The per-host decision remains the
//! same [`GrantDecider`] seam ([`StaticDecider`] here).
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

pub mod arm;
pub mod backend;
// The catalog PARSER is compiled into the crate only for the dev-only override; `build.rs`
// pulls the same file in with `#[path]` and always runs it. A shipped build therefore
// contains no catalog-parsing code at all — the strongest form of "the dev path is absent,
// not merely inert". See `catalog_override`.
#[cfg(feature = "build-jail-catalog-override")]
pub mod catalog;
pub mod catalog_override;
/// The SHIPPED catalog-update path, compiled into every build — unlike [`catalog_override`]'s loader,
/// which is dev-only because it takes its path from an env var. This one takes no path from anyone: the
/// location is fixed under nub's data directory, so there is no input that can redirect it, which is
/// exactly what makes it shippable. Without it, a package measured after a release stays uncatalogued
/// until the next release, by construction.
pub mod catalog_update;
/// The v2 catalog parser, compiled into EVERY build — unlike [`catalog`] above, which stays
/// dev-only. It has to be: a shipped build now embeds `data/build-jail-catalog-v2.json` and parses
/// it once at first use (`catalog_override::baked_v2`), so the v2 grants are the ones the jail
/// actually runs on rather than an override-only path. `build.rs` pulls this same file in with
/// `#[path]` and parses the same bytes at BUILD time, which is what keeps a malformed catalog from
/// ever reaching a user.
pub mod catalog_v2;
pub mod compiler;

/// The v2 grant for one package AT ONE VERSION, for an embedder that must act on it AFTER a
/// lifecycle script has run — today the `writePaths` move. Exposed here rather than making the
/// override module public, so the seam stays one function wide. Version selection lives behind
/// this call, so an embedder cannot resolve a different band than the compiler did.
/// ⛔ NOT FEATURE-GATED, AND GATING IT WAS THE BUG. This was `#[cfg(feature =
/// "build-jail-catalog-override")]`, which forced its only caller — the `writePaths` promotion in
/// nub-cli — to be gated too, so promotion was compiled out of every shipped build. The effect was
/// silent: the jail confined the write correctly and then the cached artefact was discarded, so every
/// install re-downloaded it. The catalog this reads is BAKED IN and always present; nothing about
/// resolving a grant needs the dev-only override path.
pub fn catalog_override_v2_grant(
    package: &str,
    version: Option<&str>,
) -> Option<&'static catalog_v2::Grant> {
    catalog_override::v2_grant_for(package, version)
}

pub mod conformance;
pub mod matcher;
pub mod policy;
pub mod preflight;
pub mod proxy;

/// What the kernel refused a confined launch, keyed by [`CommandSpec::audit_label`]. macOS
/// answers from the unified log; every other host answers with an empty list. Failure path only.
pub use backend::macos_denials;
#[cfg(target_os = "windows")]
pub use backend::windows_publish_appcontainer_read;
pub use backend::{
    CommandArgs, CommandSpec, Degradation, Prepared, PreparedChild, PreparedSignalTarget,
    RuntimeCapability, StatusReport, apply, apply_with_runtime, earliest_bootstrap,
    validate_adjacent_resource_bundle,
};
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub use backend::{
    PreparedSignalCallback, exercise_monitor_state_6, exercise_monitor_state_7,
    exercise_monitor_state_8, exercise_monitor_states_1_to_5,
};
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub use backend::{windows_leaf_grant_redundant, windows_object_traverse_ace};

/// The Linux enforcement suites' skip gate, resolving Bubblewrap candidates the way
/// production does. Test support, not an embedder API.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod host_probe {
    pub use crate::backend::landlock_abi;
    pub use crate::backend::linux_probe::{
        BwrapProbe, cached_probe, probe, skip_without_bwrap, skip_without_bwrap_with, usable_bwrap,
    };
}

/// The Windows dedicated-account backend's machine administration, surfaced for the CLI.
///
/// These are the ONLY operations that need administrator, and they are the reason the
/// per-run launch does not: one elevated `setup` installs the account and the SID-keyed WFP
/// egress fence, after which every sandboxed run is unelevated. `clean` is unelevated and
/// exists for crash residue. Windows-only by construction — no other OS needs a second
/// principal to express the grammar.
#[cfg(target_os = "windows")]
pub mod windows_admin {
    pub use crate::backend::windows_account::{SETUP_COMMAND, clean, setup, status, teardown};
}
pub use compiler::jail_private_home;
pub use compiler::{
    CommandRunner, CompileCtx, CompileError, CompileWarning, DOWNLOAD_HOSTS,
    PACKAGE_NETWORK_ALLOWED, PROJECT_VIRTUAL_STORE_LEAF, ScopeCapabilities, build_jail_net_allowed,
    build_jail_net_allowed_for, build_jail_node_options, build_jail_stdio_preload_js, compile,
    compile_build_jail, compile_with_warnings, download_hosts, net_gate_node_options,
    package_network_allowed, realpath_shim_node_options,
};
#[cfg(windows)]
pub use compiler::{
    windows_build_jail_node_options, windows_native_realpath_shim_node_options,
    windows_realpath_node_options,
};
pub use matcher::Homes;

/// One-time privileged host setup for the Linux agent-sandbox — the implementation behind
/// `nub setup-sandbox`. See `.fray/sandbox-escalation-ux.md`.
#[cfg(target_os = "linux")]
pub mod linux_admin {
    pub use crate::backend::linux_setup::{
        HelperAccess, SETUP_COMMAND, SETUP_COMMAND_ALL_USERS, SetupReport, setup, status, teardown,
    };
}

/// The macOS half of `nub setup-sandbox`.
///
/// Seatbelt is unprivileged, so there is no host setup to perform and these carry no privileged
/// operation at all — the module exists so the CLI can answer the same three modes everywhere
/// with a real per-platform readiness answer rather than a hardcoded "nothing to do".
#[cfg(target_os = "macos")]
pub mod macos_admin {
    pub use crate::backend::macos_setup::{
        SANDBOX_EXEC_PATH, enforceable, setup, status, teardown,
    };
}

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
