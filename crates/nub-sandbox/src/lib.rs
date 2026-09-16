//! OS-enforced sandbox engine with no package-manager dependency.
//!
//! Embedders provide parsed configuration, host paths, an ambient environment snapshot,
//! and per-source [`ScopeCapabilities`]. The engine does not discover project configuration.
//!
//! # Compile and apply
//!
//! [`compile`] resolves a surface and [`CompileCtx`] into a [`SandboxPolicy`].
//! [`Sandbox::acquire`] retains resolved policy resources for reusable commands; combine it
//! with a [`CommandSpec`] through [`Sandbox::prepare`]. [`apply`] remains the one-shot
//! compatibility adapter returning a [`Prepared`] launch. Launch through [`Prepared::spawn`],
//! [`Prepared::status`], or [`Prepared::output`] so process cleanup and session resources
//! retain their owners.
//!
//! **Enforcement is Linux-only.** Linux uses Landlock plus a seccomp `USER_NOTIF` supervisor,
//! and needs no elevated helper, account creation, or early bootstrap capability. Every other
//! platform reports filesystem and network enforcement as unavailable and refuses the launch
//! rather than running the command unconfined. Environment filtering constructs the child's
//! environment rather than editing the parent.
//!
//! # Embedder obligations and limits
//!
//! Supply toolchain read paths for interpreters installed outside system directories.
//! Assign capabilities per configuration source; dependency-authored policy must not gain
//! the dynamic environment or credential-broker capabilities of root-authored policy.
//! Reported losses are carried by [`Prepared::degradation`]; an embedder must surface them
//! rather than claiming enforcement.
//!
#![cfg_attr(
    all(test, windows),
    expect(
        clippy::duplicate_mod,
        reason = "Integration fixtures own their output helper when compiled as standalone test crates"
    )
)]

// Reuse the integration fixtures in the test-only native adapter experiment.
#[cfg(all(test, windows))]
extern crate self as nub_sandbox;
pub mod backend;
pub mod compiler;
#[cfg(all(test, windows))]
#[path = "../tests/git_tool_functionality.rs"]
mod git_tool_functionality_probe;
#[cfg(all(test, windows))]
#[path = "../tests/tool_functionality.rs"]
mod js_tool_functionality_probe;
#[cfg(all(test, windows))]
#[path = "../tests/native_tool_functionality.rs"]
mod native_tool_functionality_probe;
#[cfg(all(test, windows))]
#[path = "../tests/python_tool_functionality.rs"]
mod python_tool_functionality_probe;

pub mod conformance;
pub mod matcher;
pub mod policy;
pub mod proxy;

pub use backend::{
    CommandArgs, CommandSpec, Degradation, Prepared, PreparedChild, PreparedSignalTarget, Sandbox,
    apply, cleanup,
};
/// The Linux enforcement suites' Landlock ABI skip gate. Test support, not an embedder API.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod host_probe {
    pub use crate::backend::landlock_abi;
}

pub use compiler::{
    CommandRunner, CompileCtx, CompileError, CompileWarning, DOWNLOAD_HOSTS, ScopeCapabilities,
    compile, compile_with_warnings, download_hosts,
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
