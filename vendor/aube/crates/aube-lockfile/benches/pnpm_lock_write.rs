//! Lockfile write-overlap economics benchmark.
//!
//! The lockfile-write-overlap optimization moves the serialize, reformat,
//! and atomic-write work off the install's critical path onto a background
//! `spawn_blocking` task, overlapping it with the link tail. The spawned
//! task operates on a clone of the graph, so the net win is the recovered
//! write time minus the clone cost.
//!
//! This bench measures both sides on a large real `pnpm-lock.yaml` so the
//! decision to ship is grounded in numbers, not intuition. Point it at a
//! big lockfile with `AUBE_BENCH_LOCKFILE=/path/to/pnpm-lock.yaml`.

use aube_lockfile::pnpm;
use aube_manifest::PackageJson;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn load_path() -> String {
    std::env::var("AUBE_BENCH_LOCKFILE").expect("set AUBE_BENCH_LOCKFILE to a large pnpm-lock.yaml")
}

fn bench(c: &mut Criterion) {
    let path = load_path();
    let graph = pnpm::parse(std::path::Path::new(&path)).expect("parse bench lockfile");
    let manifest = PackageJson::default();
    eprintln!(
        "benchmarking write/clone on {}: {} packages, {} importers",
        path,
        graph.packages.len(),
        graph.importers.len()
    );

    let tmp = tempfile::tempdir().expect("tmpdir");
    // Use the pnpm-lock.yaml name so the writer takes the native-alias path.
    let out = tmp.path().join("pnpm-lock.yaml");

    let mut g = c.benchmark_group("pnpm-lock-write");
    // The overlappable work: serialize + reformat + atomic fs write.
    g.bench_function("write_full", |b| {
        b.iter(|| pnpm::__bench_write_to(black_box(&out), black_box(&graph), black_box(&manifest)))
    });
    // The offsetting cost the spawned task pays (a deep clone of the graph).
    g.bench_function("graph_clone", |b| b.iter(|| black_box(graph.clone())));
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
