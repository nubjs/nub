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

use std::sync::OnceLock;

use aube_lockfile::LockfileGraph;

/// Match one package-name pattern using the linker's existing `glob` primitive.
/// Invalid glob syntax falls back to a literal comparison, so adding wildcard
/// support never makes a previously exact-matchable string disappear.
pub fn package_name_matches(pattern: &str, package_name: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|compiled| compiled.matches(package_name))
        .unwrap_or_else(|_| pattern == package_name)
}

#[cfg(test)]
mod tests {
    use super::package_name_matches;

    #[test]
    fn package_name_patterns_cover_wildcards_and_literals() {
        assert!(package_name_matches("is-*", "is-number"));
        assert!(!package_name_matches("is-*", "number-is"));
        assert!(package_name_matches("@corp/tool-*", "@corp/tool-cli"));
        assert!(!package_name_matches("@corp/tool-*", "@other/tool-cli"));
        assert!(package_name_matches("is-number", "is-number"));
        assert!(!package_name_matches("is-number", "is-positive"));
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
