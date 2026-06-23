//! The default-trust dependency list backing the `defaultTrust`
//! build-policy floor.
//!
//! The list is Bun's curated `default-trusted-dependencies.txt`,
//! vendored verbatim with provenance recorded in the data file's
//! header (`data/default-trusted-dependencies.txt`). Adopting the
//! list as-is keeps the trust root traceable to an existing,
//! widely-deployed curation instead of inventing a new one.
//!
//! Membership here is necessary but never sufficient: the floor that
//! consumes this list (`aube`'s install pipeline) ranks below every
//! explicit `allowBuilds` entry and additionally requires registry
//! provenance, an active OSV `MAL-*` advisory gate, and a satisfied
//! `minimumReleaseAge` window before a listed package's lifecycle
//! scripts run. This module only answers "is the name on the list".

use std::collections::HashSet;
use std::sync::LazyLock;

static DEFAULT_TRUSTED: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    include_str!("../data/default-trusted-dependencies.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
});

/// True when `name` is on the vendored default-trust list. Callers
/// must pass the *registry* name (the real package behind an npm
/// alias), mirroring [`crate::BuildPolicy::decide`]'s alias rule — an
/// alias must not be able to borrow a listed name's trust.
pub fn is_default_trusted(name: &str) -> bool {
    DEFAULT_TRUSTED.contains(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_loads_and_contains_known_native_packages() {
        // Spot-check entries that have been on bun's list since its
        // introduction; a parse regression (header not skipped, wrong
        // delimiter) would drop them.
        for name in ["esbuild", "sharp", "better-sqlite3", "@vscode/sqlite3"] {
            assert!(is_default_trusted(name), "{name} must be on the list");
        }
        assert!(!is_default_trusted("definitely-not-on-the-list"));
        assert!(
            !is_default_trusted("# Default-trusted dependency build allowlist."),
            "header comment lines must not parse as entries"
        );
    }
}
