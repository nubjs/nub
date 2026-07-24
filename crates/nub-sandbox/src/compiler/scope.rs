//! Scope resolution — two mechanisms for how the future project-config frontend
//! (nub.jsonc + package.json metas) turns per-scope surfaces into one policy.
//!
//! - [`resolve`]: pick the most-specific PRESENT scope (the older most-specific-
//!   over-default view; retained for callers that want a single winner).
//! - [`resolve_chain`]: COMPOSE an inheritance chain (outermost→innermost) into
//!   one policy under the complete-statement model — cascade for a keyless scope,
//!   `"..."` inheriting the resolved parent per axis, and unlisted axes flooring.
//!
//! The frontend-less `--sandbox <file.json>` entry passes ONE explicit block via
//! [`crate::compile`] and bypasses both; these are the seam the frontend plugs
//! into. Implemented + tested here (against SYNTHETIC chains) so that frontend
//! lands on tested ground — it does not exist yet (design.md §2.2 "single-term
//! reality").

use super::{CompileCtx, CompileError, CompileWarning, SandboxPolicy, ScopeCapabilities};
use serde_json::Value;

/// A candidate scope, ordered least- to most-specific by construction: the caller
/// passes them in increasing specificity and the most-specific PRESENT one wins.
#[derive(Debug, Clone)]
pub struct ScopeCandidate<'a> {
    /// A human label for diagnostics (e.g. "nub.jsonc sandbox", "scriptsMeta.dev").
    pub label: &'a str,
    /// The surface value at this scope, if present.
    pub value: Option<&'a Value>,
}

/// Choose the most-specific present scope. Returns `(label, value)` or `None`
/// when no scope defines a policy (→ the caller's built-in default / unjailed).
pub fn resolve<'a>(candidates: &[ScopeCandidate<'a>]) -> Option<(&'a str, &'a Value)> {
    candidates
        .iter()
        .rev()
        .find_map(|c| c.value.map(|v| (c.label, v)))
}

/// One scope in an inheritance chain, OUTERMOST first (least specific → most).
/// `surface = None` is a KEYLESS scope (no `sandbox` key): it inherits its
/// parent's whole policy by CASCADE (precedence — once sandboxing is on it covers
/// every scope; a keyless scope can't escape by saying nothing).
#[derive(Debug, Clone)]
pub struct ChainScope<'a> {
    pub label: &'a str,
    pub surface: Option<&'a Value>,
    /// This scope's capabilities, decided by scope IDENTITY (approved `nub.jsonc` /
    /// `scriptsMeta` vs dependency-controlled `dependenciesMeta`) — never inferred.
    /// This is the point of the chain: each scope compiles against its OWN caps, so an
    /// outer approved scope may use `$(…)` / credential brokering while an inner dependency
    /// scope in the SAME compile is denied them.
    pub caps: ScopeCapabilities,
}

/// Compose an inheritance chain (outermost→innermost) into one resolved policy.
///
/// Each PRESENT scope resolves against the running parent (the resolved policy of
/// the scopes above it): its `"..."` inherits that parent per axis, and any axis
/// it does not list floors (complete statement) unless it carries an object-level
/// `"..."`. A KEYLESS scope leaves the running policy unchanged (cascade). The
/// outermost present scope resolves against the built-in base (no parent). If NO
/// scope is present, nothing sandboxes → the unjailed policy.
///
/// This lands the resolution LOGIC; wiring real scopes (discovery, phase defaults)
/// is the frontend's job. Warnings from every scope are accumulated.
pub fn resolve_chain(
    scopes: &[ChainScope],
    ctx: &CompileCtx,
) -> Result<(SandboxPolicy, Vec<CompileWarning>), CompileError> {
    let mut warnings = Vec::new();
    let mut resolved: Option<SandboxPolicy> = None;
    for scope in scopes {
        if let Some(surface) = scope.surface {
            // Each scope compiles against ITS OWN capabilities — the security core of
            // per-scope trust: a dependency-controlled scope's fold gates read its own
            // (denying) caps regardless of an outer approved scope in the same chain.
            let policy =
                super::compile_scope(surface, resolved.as_ref(), ctx, scope.caps, &mut warnings)?;
            resolved = Some(policy);
        }
        // A keyless scope keeps `resolved` as-is (cascade: inherit the parent).
    }
    let policy = match resolved {
        Some(p) => p,
        // No scope declared a policy → not sandboxed. (The frontend only invokes
        // the chain when a policy exists somewhere; this is the defensive base.)
        // `false` is a full unjail with no gated op, so the caps here are inert;
        // pass the least-privilege set as the defensive default regardless.
        None => super::compile_scope(
            &Value::Bool(false),
            None,
            ctx,
            ScopeCapabilities::dependency(),
            &mut warnings,
        )?,
    };
    Ok((policy, warnings))
}
