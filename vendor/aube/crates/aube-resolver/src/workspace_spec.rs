//! Range semantics for the `workspace:` protocol.
//!
//! The grammar itself — how a tail splits into an alias, a path, or a
//! range — lives in [`aube_util::pkg::parse_workspace_spec`], because the
//! lockfile readers need the same split and cannot depend on this crate.
//! What stays here is the half that needs the resolver's semver cache.

use crate::semver_util::{is_semver_range, version_satisfies};

/// Whether the RANGE part of a `workspace:` tail binds to a member
/// sitting at `ws_version`.
///
/// Shared by the plain form (`workspace:^1`) and the alias form
/// (`workspace:pkg@^1`) so the two cannot drift apart. Note the
/// unparseable-range arm: it exists because a tail that is not a range is
/// a member DIRECTORY under bun/Yarn-v1 (`workspace:packages/x`), and
/// read as a range it would satisfy nothing. We only ever reach here once
/// a member has been identified, and `workspace:` never means "go to the
/// registry", so a locator binds outright.
pub(crate) fn workspace_range_binds(ws_version: &str, range: &str) -> bool {
    match range {
        // `*`/`^`/`~`/empty are pnpm's "don't pin me, just track local"
        // sigils.
        "" | "*" | "^" | "~" => true,
        r if !is_semver_range(r) => true,
        r => version_satisfies(ws_version, r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_binds_follows_the_sigils_then_semver() {
        for sigil in ["", "*", "^", "~"] {
            assert!(
                workspace_range_binds("0.0.0-0", sigil),
                "`{sigil}` must bind to any local version"
            );
        }
        assert!(workspace_range_binds("1.2.3", "^1.0.0"));
        assert!(!workspace_range_binds("1.2.3", "^2.0.0"));
        // A directory locator is not a range and binds outright.
        assert!(workspace_range_binds("1.2.3", "packages/plugin"));
    }
}
