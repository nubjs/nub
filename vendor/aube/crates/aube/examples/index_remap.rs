//! Release-mode remapping benchmark; setup is excluded from each sample.
#[path = "../src/commands/install/index_remap.rs"]
mod index_remap;
use aube_lockfile::{LockedPackage, LockfileGraph};
use aube_store::{PackageIndex, StoredFile};
use std::{collections::BTreeMap, hint::black_box, time::Instant};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let files: usize = args.get(1).map_or(1024, |v| v.parse().unwrap());
    let iterations: usize = args.get(2).map_or(100, |v| v.parse().unwrap());
    let contexts: usize = args.get(3).map_or(1, |v| v.parse().unwrap());
    let index: PackageIndex = (0..files)
        .map(|i| {
            (
                format!("lib/component-{i}/index.js"),
                StoredFile {
                    hex_hash: format!("{i:064x}"),
                    store_path: format!("/private-store/files/{:02x}/{i:062x}", i % 256).into(),
                    executable: i % 17 == 0,
                    size: Some(1024),
                },
            )
        })
        .collect();
    let mut graph = LockfileGraph::default();
    for n in 0..contexts {
        let dep_path = if contexts == 1 {
            "fixture@1.0.0".to_owned()
        } else {
            format!("fixture@1.0.0(peer@{n})")
        };
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: "fixture".into(),
                version: "1.0.0".into(),
                dep_path,
                ..Default::default()
            },
        );
    }
    let mut elapsed = std::time::Duration::ZERO;
    for _ in 0..iterations {
        let input: BTreeMap<_, _> = [("fixture@1.0.0".to_owned(), index.clone())].into();
        let start = Instant::now();
        let result = index_remap::remap_indices_to_contextualized(input, black_box(&graph));
        elapsed += start.elapsed();
        assert_eq!(result.len(), contexts);
        for copy in result.values() {
            assert_eq!(copy.len(), files);
            if files > 0 {
                assert_eq!(copy["lib/component-0/index.js"].size, Some(1024));
            }
        }
        black_box(result);
    }
    println!("{:.6}", elapsed.as_secs_f64() * 1000.0);
}
