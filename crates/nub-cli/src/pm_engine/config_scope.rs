//! Which package manager a project's config speaks for.
//!
//! The active PM is the [`Role`] resolved from the `packageManager`
//! declaration (preferred) or the lockfile kind (fallback) — the same
//! precedence identity resolution and the lifecycle UA use, which is why the
//! mapping lives here once rather than at each caller.
//!
//! This module resolves the role and stops there. The per-dialect scoping it
//! used to drive — filtering `overrides` / `resolutions` / `pnpm.overrides`
//! down to what the active PM honors — belonged to the vendored engine and
//! went with it; the pnpm engine's embedder profile owns that boundary now.

use aube_lockfile::LockfileKind;

/// The active package manager whose config dialect nub mirrors. Resolved
/// declaration-first (the `packageManager`/`devEngines` pin names the
/// owner) then lockfile-kind, exactly like identity resolution. `Nub` is
/// nub's own brand-symmetric identity: it honors un-branded cross-tool
/// fields only, never another PM's branded config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Nub,
}

/// Resolve the active-PM [`Role`] from the declared `packageManager` name
/// (if it names a PM nub recognizes) then the detected lockfile kind.
///
/// `declared` is the raw `(name, version)` from `packageManager` /
/// `devEngines`; `kind` is the resolved lockfile kind. An unknown declared
/// name (vlt, deno, …) falls through to the lockfile kind, mirroring
/// identity resolution. Returns `None` only when neither a recognized
/// declaration nor a lockfile kind is present (a truly fresh project) — the
/// caller treats that as "fresh = nub identity" for scoping purposes.
pub(crate) fn role_of(declared: Option<&str>, kind: Option<LockfileKind>) -> Option<Role> {
    if let Some(name) = declared {
        match name {
            "npm" => return Some(Role::Npm),
            "pnpm" => return Some(Role::Pnpm),
            "yarn" => return Some(Role::Yarn),
            "bun" => return Some(Role::Bun),
            "nub" => return Some(Role::Nub),
            // Unknown declared tool: fall through to the lockfile kind.
            _ => {}
        }
    }
    kind.map(|k| match k {
        LockfileKind::Pnpm => Role::Pnpm,
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => Role::Npm,
        LockfileKind::Yarn | LockfileKind::YarnBerry => Role::Yarn,
        LockfileKind::Bun => Role::Bun,
        // The generic nub.lock (aube's `Aube` slot under nub's filename
        // toggle) is nub identity.
        LockfileKind::Aube => Role::Nub,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_of_prefers_declaration_then_lockfile_kind() {
        assert_eq!(
            role_of(Some("pnpm"), Some(LockfileKind::Npm)),
            Some(Role::Pnpm)
        );
        assert_eq!(
            role_of(Some("vlt"), Some(LockfileKind::Bun)),
            Some(Role::Bun)
        );
        assert_eq!(role_of(None, Some(LockfileKind::Aube)), Some(Role::Nub));
        assert_eq!(role_of(None, None), None);
    }
}
