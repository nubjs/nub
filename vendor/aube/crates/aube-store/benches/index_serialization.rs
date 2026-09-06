use aube_store::{PackageIndex, StoredFile};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut index = PackageIndex::default();
    for i in 0..64 {
        index.insert(
            format!("lib/components/component-{i}/index.js"),
            StoredFile {
                hex_hash: format!("{i:064x}"),
                store_path: PathBuf::from(format!("/store/files/{:02x}/{i:062x}", i % 256)),
                executable: i % 17 == 0,
                size: Some(1024 + i),
            },
        );
    }

    const WARMUP_ITERATIONS: usize = 1_000;
    for iteration in 0..WARMUP_ITERATIONS {
        if iteration % 2 == 0 {
            black_box(serde_json::to_vec(black_box(&index)).unwrap());
            black_box(sonic_rs::to_vec(black_box(&index)).unwrap());
        } else {
            black_box(sonic_rs::to_vec(black_box(&index)).unwrap());
            black_box(serde_json::to_vec(black_box(&index)).unwrap());
        }
    }

    const ROUNDS: usize = 20;
    const ITERATIONS_PER_ROUND: usize = 5_000;
    let measure_serde_json = || {
        let started = Instant::now();
        for _ in 0..ITERATIONS_PER_ROUND {
            black_box(serde_json::to_vec(black_box(&index)).unwrap());
        }
        started.elapsed()
    };
    let measure_sonic = || {
        let started = Instant::now();
        for _ in 0..ITERATIONS_PER_ROUND {
            black_box(sonic_rs::to_vec(black_box(&index)).unwrap());
        }
        started.elapsed()
    };

    let mut serde_json = std::time::Duration::ZERO;
    let mut sonic = std::time::Duration::ZERO;
    for round in 0..ROUNDS {
        if round % 2 == 0 {
            serde_json += measure_serde_json();
            sonic += measure_sonic();
        } else {
            sonic += measure_sonic();
            serde_json += measure_serde_json();
        }
    }

    println!("serde_json: {serde_json:?}");
    println!("sonic-rs:   {sonic:?}");
    println!(
        "speedup:    {:.2}x",
        serde_json.as_secs_f64() / sonic.as_secs_f64()
    );
}
