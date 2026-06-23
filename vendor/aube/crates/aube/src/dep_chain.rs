//! Dependency chain lookup for error diagnostics.
//!
//! When a post-resolver error mentions a specific package
//! (`tarball integrity failed`, `failed to fetch`, `script exited`),
//! the user usually wants to know *why* their install pulled that
//! package in. The resolver already attaches `chain: a@1 > b@2 > leaf`
//! to its own diagnostics (`crates/aube-resolver/src/error.rs`), but
//! the rest of the install pipeline operates on a flat list of
//! `(name, version)` pairs and doesn't know which importer is
//! responsible for each entry.
//!
//! This module bridges the gap. After the resolver finishes — when a
//! `LockfileGraph` is available — call [`set_active`] to seed a
//! process-global chain index. Subsequent error wrappers consult it
//! via [`format_chain_for`] and embed a chain string in the message.
//!
//! The index is computed once via BFS from importer roots, recording
//! the *shortest* path back to an importer for each `(name, version)`
//! pair. When a package has multiple parents, the shortest chain
//! wins — that's the most informative one for users hunting down
//! transitive pulls. Multi-parent disambiguation isn't tracked; the
//! goal is "tell the user where this came from", not full ancestry.
//!
//! Storage is a `OnceLock<Mutex<Option<Arc<ChainIndex>>>>`. A single
//! install run sets it once; recursive installs (workspace fan-out)
//! reset it per-package. Outside an install, `format_chain_for` is a
//! no-op and returns an empty string, so error messages remain
//! stable when no install is active (e.g. during `aube view`).

use aube_lockfile::LockfileGraph;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// Maps `(name, version)` → shortest ancestor chain back to an
/// importer. Empty chain = direct importer dep (no ancestors above
/// the package itself).
#[derive(Debug, Default)]
pub struct ChainIndex {
    chains: HashMap<(String, String), Vec<(String, String)>>,
}

impl ChainIndex {
    /// Return the shortest chain to `(name, version)`, or `None` if
    /// the package isn't in the index. Direct importer deps return
    /// `Some(&[])`.
    pub fn lookup(&self, name: &str, version: &str) -> Option<&[(String, String)]> {
        self.chains
            .get(&(name.to_string(), version.to_string()))
            .map(Vec::as_slice)
    }

    /// Build a chain index from a resolved lockfile graph.
    ///
    /// BFS from each importer's direct deps, tracking the path taken
    /// to reach each `dep_path`. The first time a `(name, version)`
    /// pair is reached wins — that's the shortest chain because BFS
    /// expands by hop distance.
    pub fn from_graph(graph: &LockfileGraph) -> Self {
        let mut chains: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();

        // Seed BFS with importers' direct dependencies. Each importer
        // entry is a list of `DirectDep { name, version, dep_path }`
        // pointing into `graph.packages`.
        let mut queue: VecDeque<(String, Vec<(String, String)>)> = VecDeque::new();
        for deps in graph.importers.values() {
            for direct in deps {
                queue.push_back((direct.dep_path.clone(), Vec::new()));
            }
        }

        while let Some((dep_path, ancestors)) = queue.pop_front() {
            let Some(pkg) = graph.packages.get(&dep_path) else {
                continue;
            };
            // First-write-wins under BFS = shortest path. The alias
            // key gates both index entries (alias and `alias_of`);
            // skipping on alias-collision is the right invariant
            // because the alias is the unique identifier for an
            // installed entry — `(real_name, version)` may legitimately
            // appear under two distinct aliases (e.g. `h3` plus
            // `h3-v2: npm:h3@...`), and each alias gets its own row
            // in the queue with its own chain.
            let alias_key = (pkg.name.clone(), pkg.version.clone());
            if chains.contains_key(&alias_key) {
                continue;
            }
            chains.insert(alias_key, ancestors.clone());
            // Mirror the entry under `(alias_of, version)` for
            // aliased packages so call sites that key off the real
            // npm name (`registry_name` in the install pipeline)
            // also resolve. Conflicts here are tolerable: another
            // alias of the same real package would have its own
            // chain, and we keep the first-seen one — same
            // "shortest chain" semantics as the alias key.
            if let Some(real) = &pkg.alias_of {
                let real_key = (real.clone(), pkg.version.clone());
                chains.entry(real_key).or_insert_with(|| ancestors.clone());
            }

            // Enqueue children. `dependencies` holds the dep_path
            // tail (`<version>(<peer-context>)?`); the full child
            // dep_path is `<child-name>@<tail>`.
            let mut child_ancestors = ancestors;
            child_ancestors.push((pkg.name.clone(), pkg.version.clone()));
            push_children(&mut queue, &pkg.dependencies, &child_ancestors);
            push_children(&mut queue, &pkg.optional_dependencies, &child_ancestors);
        }

        Self { chains }
    }
}

fn push_children(
    queue: &mut VecDeque<(String, Vec<(String, String)>)>,
    children: &BTreeMap<String, String>,
    ancestors: &[(String, String)],
) {
    for (child_name, child_tail) in children {
        let child_dep_path = format!("{child_name}@{child_tail}");
        queue.push_back((child_dep_path, ancestors.to_vec()));
    }
}

/// Format an ancestor chain as `a@1 > b@2 > leaf@3`. Returns an
/// empty string when the chain is empty AND the leaf is a direct
/// importer dep (no chain to show).
pub fn format_chain(ancestors: &[(String, String)], leaf_name: &str, leaf_version: &str) -> String {
    if ancestors.is_empty() {
        return String::new();
    }
    let mut s = String::from("chain: ");
    for (i, (n, v)) in ancestors.iter().enumerate() {
        if i > 0 {
            s.push_str(" > ");
        }
        s.push_str(&format!("{n}@{v}"));
    }
    s.push_str(&format!(" > {leaf_name}@{leaf_version}"));
    s
}

/// Process-global active index. Set after the resolver finishes;
/// consulted by error wrappers in the install pipeline.
fn slot() -> &'static Mutex<Option<Arc<ChainIndex>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<ChainIndex>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Set the active chain index. Call once per install run, after
/// resolution settles. Idempotent — replacing an existing index is
/// fine (recursive installs reset between workspace packages).
pub fn set_active(graph: &LockfileGraph) {
    let idx = Arc::new(ChainIndex::from_graph(graph));
    *slot().lock().expect("chain index slot poisoned") = Some(idx);
}

/// Lookup the chain for `(name, version)` against the active index
/// and format it. Returns an empty string when no index is active or
/// the package isn't present — callers concatenate the result, so
/// the empty case must not insert separator characters.
pub fn format_chain_for(name: &str, version: &str) -> String {
    let guard = match slot().lock() {
        Ok(g) => g,
        Err(_) => return String::new(),
    };
    let Some(idx) = guard.as_ref() else {
        return String::new();
    };
    match idx.lookup(name, version) {
        Some(chain) if !chain.is_empty() => {
            // Prefix newline so the chain appears on its own line in
            // miette's rendered output. Empty-chain case (direct
            // importer dep) returns "" so nothing is appended.
            format!("\n{}", format_chain(chain, name, version))
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DirectDep, LockedPackage};

    fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> (String, LockedPackage) {
        let dep_path = format!("{name}@{version}");
        let dependencies: BTreeMap<String, String> = deps
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        (
            dep_path.clone(),
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                dep_path,
                dependencies,
                ..Default::default()
            },
        )
    }

    fn direct(name: &str, version: &str) -> DirectDep {
        DirectDep {
            name: name.to_string(),
            dep_path: format!("{name}@{version}"),
            dep_type: aube_lockfile::DepType::Production,
            specifier: None,
        }
    }

    #[test]
    fn shortest_chain_wins() {
        let mut graph = LockfileGraph::default();
        graph
            .importers
            .insert(".".to_string(), vec![direct("a", "1")]);
        graph.packages.extend([
            pkg("a", "1", &[("b", "1"), ("c", "1")]),
            pkg("b", "1", &[("d", "1")]),
            pkg("c", "1", &[]),
            pkg("d", "1", &[]),
        ]);
        let idx = ChainIndex::from_graph(&graph);
        // a is direct: empty chain
        assert_eq!(idx.lookup("a", "1"), Some(&[][..]));
        // b is one hop in: chain = [a]
        assert_eq!(
            idx.lookup("b", "1"),
            Some(&[("a".to_string(), "1".to_string())][..])
        );
        // d is two hops in: chain = [a, b]
        assert_eq!(
            idx.lookup("d", "1"),
            Some(
                &[
                    ("a".to_string(), "1".to_string()),
                    ("b".to_string(), "1".to_string())
                ][..]
            )
        );
    }

    #[test]
    fn format_chain_renders_arrow_path() {
        let chain = vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ];
        assert_eq!(
            format_chain(&chain, "leaf", "3"),
            "chain: a@1 > b@2 > leaf@3"
        );
    }

    #[test]
    fn format_chain_empty_returns_empty() {
        assert_eq!(format_chain(&[], "leaf", "3"), "");
    }

    #[test]
    fn aliased_packages_resolve_under_both_alias_and_real_name() {
        // `h3-v2: npm:h3@2.0.0` lands as a `LockedPackage` whose
        // `name` is the alias (`h3-v2`) and whose `alias_of` is the
        // real npm name (`h3`). Install-pipeline error wrappers in
        // `lifecycle.rs` look up by `registry_name` (the real name)
        // and `mod.rs` looks up by `display_name` (the alias) — both
        // must resolve.
        let mut graph = LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "h3-v2".to_string(),
                dep_path: "h3-v2@2.0.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: None,
            }],
        );
        graph.packages.insert(
            "h3-v2@2.0.0".to_string(),
            LockedPackage {
                name: "h3-v2".to_string(),
                version: "2.0.0".to_string(),
                dep_path: "h3-v2@2.0.0".to_string(),
                alias_of: Some("h3".to_string()),
                ..Default::default()
            },
        );
        let idx = ChainIndex::from_graph(&graph);
        assert_eq!(idx.lookup("h3-v2", "2.0.0"), Some(&[][..]));
        assert_eq!(idx.lookup("h3", "2.0.0"), Some(&[][..]));
    }
}
