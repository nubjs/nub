//! Disk-materialize plan expansion — the embedder-pluggable seam that turns a
//! flat seed list into a graph-aware selective-subtree materialization plan.
//!
//! Standalone aube installs NO hook, so [`expand_disk_materialize`] returns the
//! seed verbatim (names = the seed) and the linker's disk-materialize pass is
//! byte-for-byte unchanged. An embedder (nub) installs a hook via
//! [`set_disk_materialize_expand_hook`] that consults the resolved graph to expand
//! each seed to its ancestor-closure — every package that transitively imports it
//! — so a transitively-consumed package materializes together with its importers
//! (else a store-resident importer resolves the un-materialized copy, a silent
//! singleton split). Undeclared imports an ejected package makes are resolved by
//! the linker's collective project-local hidden hoist tree over the ejected set,
//! not a per-importer hoist.
//!
//! The hook receives only `&LockfileGraph` + the seed names and returns a pure
//! [`DiskMaterializePlan`], so all embedder-specific policy (which packages
//! seed, the flag gate, vite<8.1 detection) lives in the embedder; aube owns
//! only the neutral seam and the graph primitive ([`LockfileGraph::importer_closure`]).

use std::collections::HashSet;
use std::sync::OnceLock;

use aube_lockfile::LockfileGraph;

/// Match one package-name pattern using the linker's existing `glob` primitive.
/// Invalid glob syntax falls back to a literal comparison, so adding wildcard
/// support never makes a previously exact-matchable string disappear.
/// One-shot callers only — a pattern set consulted per package compiles once via
/// [`PackageNameMatcher`] instead.
pub fn package_name_matches(pattern: &str, package_name: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|compiled| compiled.matches(package_name))
        .unwrap_or_else(|_| pattern == package_name)
}

/// The `glob` crate's metacharacters. npm forbids all three in a package name, so
/// a pattern containing none of them can only ever match itself — which is what
/// makes the literal half of [`PackageNameMatcher`] an exact-lookup set rather
/// than a glob evaluation.
const GLOB_METACHARS: [char; 3] = ['*', '?', '['];

/// A package-name pattern set compiled once, up front.
///
/// `glob::Pattern::new` is a parse plus an allocation and the linker consults
/// this once per graph package inside the GVS link pass, so classification
/// happens at construction: metacharacter-free patterns (i.e. every real package
/// name) resolve by O(1) hash lookup, and only the globby minority is compiled.
/// A pattern that fails to compile degrades to a literal, preserving
/// [`package_name_matches`]'s fallback.
#[derive(Debug, Default, Clone)]
pub struct PackageNameMatcher {
    literals: HashSet<String>,
    globs: Vec<glob::Pattern>,
}

impl PackageNameMatcher {
    /// Generic over the element so a caller holding `&str`s need not materialize
    /// a `Vec<String>` purely to be read once.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        let mut literals = HashSet::new();
        let mut globs = Vec::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if !pattern.contains(GLOB_METACHARS) {
                literals.insert(pattern.to_owned());
                continue;
            }
            match glob::Pattern::new(pattern) {
                Ok(compiled) => globs.push(compiled),
                Err(_) => {
                    literals.insert(pattern.to_owned());
                }
            }
        }
        Self { literals, globs }
    }

    pub fn is_empty(&self) -> bool {
        self.literals.is_empty() && self.globs.is_empty()
    }

    pub fn matches(&self, package_name: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        self.literals.contains(package_name) || self.globs.iter().any(|p| p.matches(package_name))
    }
}

#[cfg(test)]
mod tests {
    use super::{PackageNameMatcher, package_name_matches};

    #[test]
    fn package_name_patterns_cover_wildcards_and_literals() {
        assert!(package_name_matches("is-*", "is-number"));
        assert!(!package_name_matches("is-*", "number-is"));
        assert!(package_name_matches("@corp/tool-*", "@corp/tool-cli"));
        assert!(!package_name_matches("@corp/tool-*", "@other/tool-cli"));
        assert!(package_name_matches("is-number", "is-number"));
        assert!(!package_name_matches("is-number", "is-positive"));
    }

    #[test]
    fn matcher_resolves_plain_names_by_lookup_and_still_honors_wildcards() {
        let matcher = PackageNameMatcher::new(&[
            "is-number".to_string(),
            "@corp/tool-*".to_string(),
            "bad-[".to_string(),
        ]);

        assert_eq!(
            matcher.globs.len(),
            1,
            "only the wildcard pattern should compile a glob; \
             the plain name and the uncompilable pattern are literals"
        );

        assert!(matcher.matches("is-number"));
        assert!(!matcher.matches("is-positive"));
        assert!(matcher.matches("@corp/tool-cli"));
        assert!(!matcher.matches("@other/tool-cli"));
        assert!(
            matcher.matches("bad-["),
            "an uncompilable pattern keeps the literal-comparison fallback"
        );
        assert!(!PackageNameMatcher::default().matches("is-number"));
    }
}

/// The materialization plan a [`DmExpandHook`] produces from the resolved graph.
#[derive(Debug, Default, Clone)]
pub struct DiskMaterializePlan {
    /// The expanded set of package NAMES to disk-materialize project-local (the
    /// seed UNION its ancestor-closure). Fed to
    /// [`Linker::with_disk_materialize`](crate::Linker::with_disk_materialize),
    /// matched by exact name, exactly as the raw seed was. Undeclared phantoms an
    /// ejected package imports are resolved by the linker's collective
    /// project-local hidden hoist tree over this set (see
    /// [`Linker::link_hidden_hoist`](crate::Linker)), not a per-importer hoist.
    pub names: Vec<String>,
    /// `(importer name, optional-dependency name)` pairs fed to
    /// [`Linker::with_nested_optional_deps`](crate::Linker::with_nested_optional_deps).
    /// Empty for standalone aube; see that field's docs for the mechanism.
    pub nested_optional_deps: Vec<(String, String)>,
}

/// A hook that expands a disk-materialize seed into a graph-aware plan. `Send +
/// Sync` because it is stored in a process-global consulted from the install
/// pipeline; `'static` because it outlives any single install.
pub type DmExpandHook =
    Box<dyn Fn(&LockfileGraph, &[String]) -> DiskMaterializePlan + Send + Sync + 'static>;

static DM_EXPAND_HOOK: OnceLock<DmExpandHook> = OnceLock::new();

/// Install the embedder's disk-materialize expansion hook. Set-once: a second
/// call is ignored (the first registration wins), matching aube's other
/// process-global embedder seams. Called once at engine-session build; standalone
/// aube never calls it, so the default path stays hook-free.
pub fn set_disk_materialize_expand_hook(hook: DmExpandHook) {
    let _ = DM_EXPAND_HOOK.set(hook);
}

/// Expand a disk-materialize `seed` (the resolved `diskMaterializePackages`
/// names) into a plan against `graph`. With no hook installed — standalone aube,
/// every test — returns the seed verbatim as the plan names, so the caller's
/// `with_disk_materialize(&plan.names)` is identical to the pre-existing
/// `with_disk_materialize(&seed)` and nothing else changes.
pub fn expand_disk_materialize(graph: &LockfileGraph, seed: &[String]) -> DiskMaterializePlan {
    match DM_EXPAND_HOOK.get() {
        Some(hook) => hook(graph, seed),
        None => DiskMaterializePlan {
            names: seed.to_vec(),
            ..DiskMaterializePlan::default()
        },
    }
}
