//! Baseline benches for the workspace topo-sort hot path.
//!
//! `topological_chunks` (Kahn's algorithm, one wave per level) runs on every
//! `nub run -r` / filtered workspace invocation. Its inner loop rescans the
//! `remaining` set per wave — O(waves * remaining * deps) — so its cost scales
//! with workspace size and dependency depth. This bench fixes a baseline on a
//! synthetic ~200-package graph before any optimization is attempted.
//!
//! Fixture (built inline, no I/O): 200 packages named `pkg-000..pkg-199`. Each
//! package depends on the 3 packages immediately below it (`pkg-N` deps on
//! `pkg-(N-1)`, `pkg-(N-2)`, `pkg-(N-3)`), producing a deep, wide DAG that
//! forces many topo waves — a realistic shape for a layered monorepo.

use rustc_hash::FxHashMap as HashMap;
use std::collections::HashSet;

use criterion::{Criterion, criterion_group, criterion_main};
use nub_core::workspace::filter::{WorkspacePackage, build_dep_graph, topological_chunks};

const N: usize = 200;

/// Build 200 synthetic packages, each depending on its 3 lower-indexed neighbors.
fn synthetic_members() -> Vec<WorkspacePackage> {
    (0..N)
        .map(|i| {
            let mut deps = serde_json::Map::new();
            for d in 1..=3 {
                if i >= d {
                    deps.insert(
                        format!("pkg-{:03}", i - d),
                        serde_json::json!("workspace:*"),
                    );
                }
            }
            let manifest = serde_json::json!({
                "name": format!("pkg-{:03}", i),
                "dependencies": serde_json::Value::Object(deps),
            });
            WorkspacePackage {
                name: format!("pkg-{:03}", i),
                dir: std::path::PathBuf::from(format!("packages/pkg-{:03}", i)),
                manifest,
            }
        })
        .collect()
}

fn bench_workspace_topo(c: &mut Criterion) {
    let members = synthetic_members();
    let name_to_idx: HashMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.as_str(), i))
        .collect();

    // `build_dep_graph` (parse the manifests into an index→deps adjacency) runs
    // once per filter resolution.
    c.bench_function("workspace/build_dep_graph/200", |b| {
        b.iter(|| {
            build_dep_graph(
                std::hint::black_box(&members),
                std::hint::black_box(&name_to_idx),
            )
        });
    });

    let deps = build_dep_graph(&members, &name_to_idx);
    let all_nodes: HashSet<usize> = (0..N).collect();

    // The topo-sort proper — the hot path whose per-wave rescan we want a
    // baseline for.
    c.bench_function("workspace/topological_chunks/200", |b| {
        b.iter(|| {
            topological_chunks(
                std::hint::black_box(&all_nodes),
                std::hint::black_box(&deps),
            )
        });
    });
}

criterion_group!(benches, bench_workspace_topo);
criterion_main!(benches);
