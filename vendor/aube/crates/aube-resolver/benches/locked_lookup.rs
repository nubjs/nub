//! Benchmarks the lockfile-reuse scan over an existing graph: a
//! per-task linear `graph.packages.values().find(..)` vs the
//! `LockedIndex` name-bucket lookup.
//!
//! Run with:
//!   cargo bench -p aube-resolver --bench locked_lookup
//!
//! The fixture mimics a real warm-path install: a large existing
//! lockfile with a handful of high-version-count names (`@types/node`,
//! `typescript`, `react`, `@babel/*`) and a long single-version tail,
//! plus a task list scaling with the tree (1k and 5k brackets). The
//! benched work is the whole warm path's lockfile-reuse lookup for
//! every task — exactly the `O(tasks × lockfile)` cost the index
//! removes.

use std::collections::BTreeMap;
use std::hint::black_box;

use aube_lockfile::{LockedPackage, LockfileGraph};
use aube_resolver::locked_index::LockedIndex;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

/// Versions per "hot" multi-version name, mirroring how many releases
/// of `@types/node` / `typescript` accumulate in a long-lived lockfile.
const HOT_VERSIONS: usize = 40;

fn locked(name: &str, version: &str) -> LockedPackage {
    LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        ..Default::default()
    }
}

/// Build an existing lockfile graph with `tail_len` single-version
/// packages plus several multi-version "hot" names.
fn build_graph(tail_len: usize) -> LockfileGraph {
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    // Hot names with many locked versions (the index's win case).
    let hot = ["@types/node", "typescript", "react", "@babel/core"];
    for name in hot {
        for minor in 0..HOT_VERSIONS {
            let version = format!("18.{minor}.0");
            packages.insert(format!("{name}@{version}"), locked(name, &version));
        }
    }

    // Single-version tail.
    for i in 0..tail_len {
        let name = format!("pkg-{i:05}");
        let version = "1.0.0";
        packages.insert(format!("{name}@{version}"), locked(&name, version));
    }

    LockfileGraph {
        packages,
        ..Default::default()
    }
}

/// The task list a resolve would iterate: every package name in the
/// graph, paired with a range that matches. Hot names get a wide range
/// (`>=18`) so the lookup must scan their whole version set; the tail
/// gets an exact range.
fn build_tasks(graph: &LockfileGraph) -> Vec<(String, String)> {
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    let mut tasks = Vec::new();
    for pkg in graph.packages.values() {
        if seen.insert(pkg.name.as_str(), ()).is_none() {
            let range = if pkg.name.starts_with("pkg-") {
                "1.0.0".to_string()
            } else {
                ">=18".to_string()
            };
            tasks.push((pkg.name.clone(), range));
        }
    }
    tasks
}

/// The per-task scan baseline: a fresh linear `.values().find(..)` over
/// the whole graph for every task.
fn linear_scan(
    graph: &LockfileGraph,
    tasks: &[(String, String)],
    vuln: &BTreeMap<String, Vec<String>>,
) -> usize {
    let mut hits = 0;
    for (name, range) in tasks {
        let found = graph.packages.values().find(|p| {
            p.name == *name
                && version_satisfies(&p.version, range)
                && !is_vulnerable(name, &p.version, vuln)
        });
        if found.is_some() {
            hits += 1;
        }
    }
    hits
}

/// The indexed path: build the index once, then a bucket lookup per
/// task.
fn indexed(
    graph: &LockfileGraph,
    tasks: &[(String, String)],
    vuln: &BTreeMap<String, Vec<String>>,
) -> usize {
    let index = LockedIndex::new(Some(graph));
    let mut hits = 0;
    for (name, range) in tasks {
        if index
            .find_satisfying(name, range, name, false, vuln)
            .is_some()
        {
            hits += 1;
        }
    }
    hits
}

// Local copies of the resolver's predicates so the bench links without
// reaching into the crate's private modules. `version_satisfies` mirrors
// `semver_util::version_satisfies` (node-semver Range::satisfies);
// `is_vulnerable` mirrors `resolve::vulnerable::is_vulnerable`.
fn version_satisfies(version: &str, range_str: &str) -> bool {
    let (Ok(v), Ok(r)) = (
        node_semver::Version::parse(version),
        node_semver::Range::parse(range_str),
    ) else {
        return false;
    };
    v.satisfies(&r)
}

fn is_vulnerable(name: &str, version: &str, vuln: &BTreeMap<String, Vec<String>>) -> bool {
    let Some(ranges) = vuln.get(name) else {
        return false;
    };
    let Ok(v) = node_semver::Version::parse(version) else {
        return false;
    };
    ranges
        .iter()
        .filter_map(|r| node_semver::Range::parse(r).ok())
        .any(|r| v.satisfies(&r))
}

fn bench_locked_lookup(c: &mut Criterion) {
    let vuln = BTreeMap::new();
    let mut group = c.benchmark_group("locked_lookup");

    for tail_len in [1000usize, 5000] {
        let graph = build_graph(tail_len);
        let tasks = build_tasks(&graph);
        // Confirm both paths agree before timing them.
        assert_eq!(
            linear_scan(&graph, &tasks, &vuln),
            indexed(&graph, &tasks, &vuln),
            "linear and indexed disagree at tail_len={tail_len}",
        );

        group.bench_with_input(BenchmarkId::new("linear", tail_len), &tail_len, |b, _| {
            b.iter(|| black_box(linear_scan(&graph, &tasks, &vuln)))
        });
        group.bench_with_input(BenchmarkId::new("indexed", tail_len), &tail_len, |b, _| {
            b.iter(|| black_box(indexed(&graph, &tasks, &vuln)))
        });
    }

    group.finish();
}

criterion_group!(benches, bench_locked_lookup);
criterion_main!(benches);
