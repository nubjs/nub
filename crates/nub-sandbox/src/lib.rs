//! OS-enforced sandbox engine with no package-manager dependency.
//!
//! Embedders provide parsed configuration, host paths, an ambient environment snapshot,
//! and per-source [`ScopeCapabilities`]. The engine does not discover project configuration.
//!
//! # Compile and apply
//!
//! [`compile`] resolves a surface and [`CompileCtx`] into a [`SandboxPolicy`].
//! [`compile_build_jail`] resolves the catalog-driven dependency-build profile.
//! [`apply`] combines that policy with a [`CommandSpec`] and returns a [`Prepared`]
//! launch, or a [`Degradation`] error when a required guarantee cannot be enforced.
//! Launch through [`Prepared::spawn`], [`Prepared::status`], or [`Prepared::output`]
//! so process cleanup, proxies and temporary directories retain their owners.
//!
//! Linux uses Landlock and seccomp, macOS uses Seatbelt, and Windows uses AppContainer.
//! None requires an elevated helper, account creation, or an early bootstrap capability.
//! Environment filtering constructs the child's environment rather than editing the parent.
//! Unsupported platforms report filesystem and network enforcement as unavailable.
//!
//! # Embedder obligations and limits
//!
//! Supply toolchain read paths for interpreters installed outside system directories.
//! Assign capabilities per configuration source; dependency-authored policy must not gain
//! the dynamic environment or credential-broker capabilities of root-authored policy.
//! Windows per-host networking requires a registered co-package egress helper through
//! [`set_windows_egress_helper_command`], not a machine-wide loopback exemption.
//! Its helper supports connection rules; unsupported TLS-inspection/broker policies fail closed.
//!
//! The build jail is a compatibility-oriented profile. In particular, its Windows full-disk
//! catalog tier omits the AppContainer token and therefore has no OS filesystem/network
//! boundary. Environment filtering still applies. Other reported losses are carried by
//! [`Prepared::degradation`]; an embedder must surface them rather than claiming enforcement.
//! Windows also rejects an already-shared working root that would defeat its allowlist.
//!
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
pub mod proxy;

/// What the kernel refused a confined launch, keyed by [`CommandSpec::audit_label`]. macOS
/// answers from the unified log; every other host answers with an empty list. Failure path only.
pub use backend::macos_denials;
#[cfg(target_os = "windows")]
pub use backend::windows_publish_appcontainer_read;
pub use backend::{
    CommandArgs, CommandSpec, Degradation, Prepared, PreparedChild, PreparedSignalTarget, apply,
};
// The Windows zero-privilege per-host egress FUNNEL seam: an embedder registers HOW to launch nub
// as the co-package egress-proxy helper (`set_...`, OS-agnostic so nub-cli registers with no
// `cfg`), and nub-cli dispatches its hidden re-entry to `serve_...` (Windows-only).
pub use backend::set_windows_egress_helper_command;
#[cfg(target_os = "windows")]
pub use backend::{serve_windows_egress_helper, windows_token_report};
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub use backend::{windows_leaf_grant_redundant, windows_object_traverse_ace};

/// The Linux enforcement suites' Landlock ABI skip gate. Test support, not an embedder API.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod host_probe {
    pub use crate::backend::landlock_abi;
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

// `linux_admin` / `macos_admin` re-exported the dropped `linux_setup` / `macos_setup` host-setup
// modules (the `nub setup-sandbox` surface for the privileged Linux helper tier; the macOS half
// was a no-op readiness reporter). Removed with the curated zero-privilege import (epic row 0.3);
// the zero-privilege enforcement tier defines its own readiness surface in a later phase.
pub use policy::SandboxPolicy;
pub use proxy::{Decision, EgressProxy, GrantDecider, Host, StaticDecider};

/// Relax a compiled policy's READ axis to the whole disk MINUS secret subtrees — the
/// sandbox-sanctioned expression of "read almost everything". A whole-root `/` read grant is
/// deliberately DROPPED by the Landlock lowering (`backend::linux_grants::compile_mount_plan`) as
/// an unclawable credential leak, so a raw `{"fs": {"/": "r"}}` surface silently collapses to
/// system-floor reads. This front-inserts the disk-minus-secrets read allows so pre-existing (more
/// specific) write grants still win under last-match-wins.
///
/// The build-jail embedder seam uses this to reproduce the generous read posture dependency
/// lifecycle scripts need — a `node` interpreter must read its own runtime/preload, its module
/// tree, and system libs — while keeping WRITES allow-only. It is stricter than a bare read-`/`:
/// `$HOME`-anchored secret SUBTREES are excluded. `.env*` basename reads remain a per-backend
/// residual on Landlock (which has no deny primitive).
pub fn relax_reads_to_disk_minus_secrets(policy: &mut SandboxPolicy, homes: &Homes) {
    compiler::relax_fs_read_to_disk_minus_secrets(policy, homes);
}

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
