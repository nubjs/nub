//! Topological sort of selected workspace packages by intra-workspace deps.
//!
//! Used by `aube run -r` and `aube exec -r` so that scripts in
//! dependency packages run before their dependents (e.g. `build` in a
//! shared library before `build` in an app that consumes it).
//!
//! Edges considered: a package `A` depends on `B` when one of `A`'s
//! `dependencies`, `devDependencies`, `optionalDependencies`, or
//! `peerDependencies` names a workspace sibling whose
//! `package.json#name` matches. Dep specifiers are not inspected — any
//! reference to a sibling name pulls in the edge, matching pnpm.
//!
//! Edges are restricted to packages within the matched set. A package
//! that depends on an unselected workspace sibling has the dep ignored;
//! pnpm uses the same projection so `aube run -r --filter=foo build`
//! orders the matched subset without dragging unselected packages in.
//!
//! Cycles fall back to insertion (workspace-listing) order for the
//! cyclic remnant after the acyclic prefix completes. A
//! [`WARN_AUBE_WORKSPACE_TOPO_CYCLE`] warning is emitted with the
//! involved package names.

use crate::selector::SelectedPackage;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Topologically sort `packages` so that workspace deps come before
/// their dependents. Stable: among nodes with equal in-degree, the
/// original input order is preserved.
///
/// Cycle handling: nodes that participate in a cycle (or sit
/// downstream of one) are appended in their original order after the
/// acyclic prefix, with a tracing warning identifying them. The total
/// length of the output always equals `packages.len()`.
pub fn topological_sort(packages: Vec<SelectedPackage>) -> Vec<SelectedPackage> {
    if packages.len() < 2 {
        return packages;
    }

    let by_name: BTreeMap<&str, usize> = packages
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.name.as_deref().map(|n| (n, i)))
        .collect();

    let mut in_degree = vec![0_usize; packages.len()];
    // adj[i] = indices of packages that depend on i (so once i is
    // emitted, those become candidates to emit next).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); packages.len()];

    for (i, pkg) in packages.iter().enumerate() {
        let m = &pkg.manifest;
        let dep_names: BTreeSet<&str> = m
            .dependencies
            .keys()
            .chain(m.dev_dependencies.keys())
            .chain(m.optional_dependencies.keys())
            .chain(m.peer_dependencies.keys())
            .map(String::as_str)
            .collect();
        for dep in dep_names {
            let Some(&j) = by_name.get(dep) else {
                continue;
            };
            if i == j {
                continue;
            }
            adj[j].push(i);
            in_degree[i] += 1;
        }
    }

    // Kahn's algorithm. The initial queue is the acyclic-leaves set in
    // input order; subsequent decrements preserve insertion order
    // because adj[j] was built in input order.
    let mut queue: VecDeque<usize> = (0..packages.len()).filter(|&i| in_degree[i] == 0).collect();
    let mut order: Vec<usize> = Vec::with_capacity(packages.len());
    let mut emitted = vec![false; packages.len()];
    while let Some(i) = queue.pop_front() {
        order.push(i);
        emitted[i] = true;
        for &k in &adj[i] {
            in_degree[k] -= 1;
            if in_degree[k] == 0 {
                queue.push_back(k);
            }
        }
    }

    if order.len() < packages.len() {
        let cycle_names: Vec<&str> = packages
            .iter()
            .enumerate()
            .filter(|(i, _)| !emitted[*i])
            .map(|(_, p)| p.name.as_deref().unwrap_or("<unnamed>"))
            .collect();
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_WORKSPACE_TOPO_CYCLE,
            packages = cycle_names.join(", "),
            "workspace dependency cycle detected; running cycle members in listing order",
        );
        for (i, &done) in emitted.iter().enumerate() {
            if !done {
                order.push(i);
            }
        }
    }

    let mut slots: Vec<Option<SelectedPackage>> = packages.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().expect("each index is visited exactly once"))
        .collect()
}

/// For each package in `packages`, the indices of *other* packages in
/// the same slice it depends on (intra-set edges only). Returned shape:
/// `out[i]` is the list of prerequisite indices for `packages[i]`.
///
/// Used by the bounded-parallel paths to wait for a dependent's
/// workspace deps to finish before claiming its concurrency slot. The
/// edge definition matches [`topological_sort`] for non-cyclic inputs;
/// when the dep graph contains a cycle, back-edges are dropped so the
/// returned graph is acyclic. Without that, a parallel run on a cycle
/// would wedge tasks waiting on each other's `watch::Sender` forever.
pub fn compute_prereq_indices(packages: &[SelectedPackage]) -> Vec<Vec<usize>> {
    let by_name: BTreeMap<&str, usize> = packages
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.name.as_deref().map(|n| (n, i)))
        .collect();
    let raw: Vec<Vec<usize>> = packages
        .iter()
        .enumerate()
        .map(|(i, pkg)| {
            let m = &pkg.manifest;
            let mut prereqs: BTreeSet<usize> = BTreeSet::new();
            for dep in m
                .dependencies
                .keys()
                .chain(m.dev_dependencies.keys())
                .chain(m.optional_dependencies.keys())
                .chain(m.peer_dependencies.keys())
            {
                if let Some(&j) = by_name.get(dep.as_str())
                    && j != i
                {
                    prereqs.insert(j);
                }
            }
            prereqs.into_iter().collect()
        })
        .collect();

    // DFS each component; an edge u → v where v is on the current
    // recursion stack (Gray) is a cycle back-edge and gets dropped.
    // Iterative to avoid blowing the stack on deep workspace graphs.
    let n = packages.len();
    let mut state = vec![NodeState::White; n];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for start in 0..n {
        if state[start] != NodeState::White {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        state[start] = NodeState::Gray;
        while let Some(&mut (u, ref mut i)) = stack.last_mut() {
            if let Some(&v) = raw[u].get(*i) {
                *i += 1;
                match state[v] {
                    NodeState::White => {
                        out[u].push(v);
                        state[v] = NodeState::Gray;
                        stack.push((v, 0));
                    }
                    NodeState::Black => out[u].push(v),
                    NodeState::Gray => {} // back-edge: drop
                }
            } else {
                state[u] = NodeState::Black;
                stack.pop();
            }
        }
    }
    for edges in &mut out {
        edges.sort_unstable();
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum NodeState {
    White,
    Gray,
    Black,
}

/// Transpose a prereq adjacency: if `prereqs[i]` lists `j` (i waits on
/// j), the result has `out[j]` listing `i` (j waits on i). Used by the
/// bounded-parallel path under `--reverse` so dependents finish before
/// their deps — the teardown semantics pnpm advertises. Without this,
/// reversing the slice alone leaves the barrier still enforcing
/// forward dep order.
pub fn transpose_prereqs(prereqs: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); prereqs.len()];
    for (i, deps) in prereqs.iter().enumerate() {
        for &j in deps {
            out[j].push(i);
        }
    }
    for edges in &mut out {
        edges.sort_unstable();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_manifest::PackageJson;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn pkg(name: &str, deps: &[&str]) -> SelectedPackage {
        let manifest = PackageJson {
            name: Some(name.to_string()),
            dependencies: deps
                .iter()
                .map(|d| ((*d).to_string(), "*".to_string()))
                .collect::<BTreeMap<_, _>>(),
            ..PackageJson::default()
        };
        SelectedPackage {
            name: Some(name.to_string()),
            version: None,
            private: false,
            dir: PathBuf::from(name),
            manifest,
        }
    }

    fn names(out: &[SelectedPackage]) -> Vec<&str> {
        out.iter().map(|p| p.name.as_deref().unwrap()).collect()
    }

    #[test]
    fn linear_chain_runs_leaves_first() {
        // app depends on lib; lib depends on core. Order: core, lib, app.
        let out = topological_sort(vec![
            pkg("app", &["lib"]),
            pkg("lib", &["core"]),
            pkg("core", &[]),
        ]);
        assert_eq!(names(&out), vec!["core", "lib", "app"]);
    }

    #[test]
    fn diamond_preserves_input_order_among_independent_nodes() {
        // app depends on left + right; both depend on shared.
        // Output: shared first, then left/right in input order, then app.
        let out = topological_sort(vec![
            pkg("app", &["left", "right"]),
            pkg("left", &["shared"]),
            pkg("right", &["shared"]),
            pkg("shared", &[]),
        ]);
        assert_eq!(names(&out), vec!["shared", "left", "right", "app"]);
    }

    #[test]
    fn unrelated_packages_keep_input_order() {
        let out = topological_sort(vec![pkg("a", &[]), pkg("b", &[]), pkg("c", &[])]);
        assert_eq!(names(&out), vec!["a", "b", "c"]);
    }

    #[test]
    fn ignores_external_deps() {
        // `lodash` is not in the workspace, so it doesn't contribute
        // an edge and `a` is treated as a leaf.
        let out = topological_sort(vec![pkg("a", &["lodash"]), pkg("b", &["a"])]);
        assert_eq!(names(&out), vec!["a", "b"]);
    }

    #[test]
    fn cycle_falls_back_to_input_order_for_cycle_members() {
        // a → b → a forms a cycle. c is independent. Output: c first
        // (acyclic prefix), then a, b in input order.
        let out = topological_sort(vec![pkg("a", &["b"]), pkg("b", &["a"]), pkg("c", &[])]);
        assert_eq!(names(&out), vec!["c", "a", "b"]);
    }

    #[test]
    fn dev_and_peer_deps_count_as_edges() {
        let mut a = pkg("a", &[]);
        a.manifest
            .dev_dependencies
            .insert("b".to_string(), "*".to_string());
        a.manifest
            .peer_dependencies
            .insert("c".to_string(), "*".to_string());
        let out = topological_sort(vec![a, pkg("b", &[]), pkg("c", &[])]);
        // b and c are leaves and come before a; among themselves they
        // keep input order.
        assert_eq!(names(&out), vec!["b", "c", "a"]);
    }

    #[test]
    fn empty_and_single_inputs_are_passthrough() {
        assert!(topological_sort(vec![]).is_empty());
        let single = topological_sort(vec![pkg("only", &[])]);
        assert_eq!(names(&single), vec!["only"]);
    }

    #[test]
    fn compute_prereq_indices_returns_intra_set_dep_indices() {
        // app depends on lib + external; lib depends on core; core has none.
        // Index 0 = app, 1 = lib, 2 = core. External deps don't appear.
        let pkgs = vec![
            pkg("app", &["lib", "lodash"]),
            pkg("lib", &["core"]),
            pkg("core", &[]),
        ];
        let prereqs = compute_prereq_indices(&pkgs);
        assert_eq!(prereqs[0], vec![1]); // app -> lib
        assert_eq!(prereqs[1], vec![2]); // lib -> core
        assert!(prereqs[2].is_empty()); // core has no intra-set deps
    }

    #[test]
    fn compute_prereq_indices_dedupes_when_same_dep_in_multiple_sections() {
        // A package can list the same name in dependencies AND
        // peerDependencies; we want one prereq edge, not two.
        let mut a = pkg("a", &["b"]);
        a.manifest
            .peer_dependencies
            .insert("b".to_string(), "*".to_string());
        let prereqs = compute_prereq_indices(&[a, pkg("b", &[])]);
        assert_eq!(prereqs[0], vec![1]);
    }

    #[test]
    fn compute_prereq_indices_breaks_cycles() {
        // a → b → a forms a cycle. Without cycle-breaking, a parallel
        // run hangs forever waiting on each other's barrier. Exactly
        // one of the two edges must be dropped (DFS visits index 0
        // first, so the b → a back-edge is the one removed).
        let pkgs = vec![pkg("a", &["b"]), pkg("b", &["a"])];
        let prereqs = compute_prereq_indices(&pkgs);
        let total_edges: usize = prereqs.iter().map(Vec::len).sum();
        assert_eq!(total_edges, 1, "cycle back-edge must be dropped");
    }

    #[test]
    fn compute_prereq_indices_breaks_three_node_cycle() {
        // a → b → c → a. Two edges remain after cycle-breaking, no
        // node waits on a node already on its own DFS stack.
        let pkgs = vec![pkg("a", &["b"]), pkg("b", &["c"]), pkg("c", &["a"])];
        let prereqs = compute_prereq_indices(&pkgs);
        let total_edges: usize = prereqs.iter().map(Vec::len).sum();
        assert_eq!(total_edges, 2);
    }

    #[test]
    fn transpose_prereqs_swaps_edge_direction() {
        // Forward chain: app → lib → core (i.e. prereqs[0]=[1], [1]=[2]).
        // Transposed: core → lib → app  (out[2]=[1], out[1]=[0]).
        let forward = vec![vec![1], vec![2], vec![]];
        let reversed = transpose_prereqs(&forward);
        assert_eq!(reversed[0], Vec::<usize>::new());
        assert_eq!(reversed[1], vec![0]);
        assert_eq!(reversed[2], vec![1]);
    }

    #[test]
    fn transpose_prereqs_handles_diamond() {
        // app(0) depends on left(1) and right(2); both depend on shared(3).
        // Forward: [0]=[1,2], [1]=[3], [2]=[3], [3]=[].
        // Transposed (teardown order): [3]=[1,2], [1]=[0], [2]=[0], [0]=[].
        let forward = vec![vec![1, 2], vec![3], vec![3], vec![]];
        let reversed = transpose_prereqs(&forward);
        assert_eq!(reversed[0], Vec::<usize>::new());
        assert_eq!(reversed[1], vec![0]);
        assert_eq!(reversed[2], vec![0]);
        assert_eq!(reversed[3], vec![1, 2]);
    }
}
