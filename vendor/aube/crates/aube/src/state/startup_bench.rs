use super::{read_state_layout, read_state_package_content_hashes, read_state_startup_snapshot};
use std::{hint::black_box, time::Instant};

// Both paths call the real readers. Setup is outside the timed region.
#[test]
#[ignore = "release-mode state-reader benchmark"]
fn bench_startup_state() {
    for packages in [0, 128, 1024] {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("package.json"), "{}").unwrap();
        let directory = super::state_dir(project.path());
        std::fs::create_dir_all(&directory).unwrap();
        let hashes: std::collections::BTreeMap<_, _> = (0..packages)
            .map(|n| (format!("package-{n}@1.0.0(peer@1.0.0)"), "a".repeat(64)))
            .collect();
        let value = serde_json::json!({
            "lockfile_hash": "lock", "package_json_hashes": {}, "aube_version": "0.0.0",
            "layout": {"linker": "isolated", "direct_entries": {}, "packages": {}},
            "package_content_hashes": hashes, "package_subtree_hashes": hashes,
            "package_json_shape_digests": hashes
        });
        std::fs::write(super::install_state_file(&directory), value.to_string()).unwrap();
        for pair in 0..14 {
            for candidate in if pair % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let started = Instant::now();
                for _ in 0..100 {
                    let count = if candidate {
                        let snapshot = read_state_startup_snapshot(project.path()).unwrap();
                        black_box(snapshot.layout);
                        snapshot.package_count
                    } else {
                        black_box(read_state_layout(project.path()).unwrap());
                        read_state_package_content_hashes(project.path()).map(|hashes| hashes.len())
                    };
                    assert_eq!(count, (packages > 0).then_some(packages));
                    black_box(count);
                }
                if pair >= 2 {
                    println!(
                        "{}",
                        serde_json::json!({
                            "packages": packages, "pair": pair - 2, "candidate": candidate,
                            "iterations": 100, "milliseconds": started.elapsed().as_secs_f64() * 1000.0
                        })
                    );
                }
            }
        }
    }
}
