use aube_store::Store;
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::at(tmp.path().join("store/v1/files"));
    store.prepare_for_write().unwrap();

    const ITERATIONS: usize = 5_000_000;
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(&store).prepare_for_write().unwrap();
    }
    let elapsed = started.elapsed();
    println!("{ITERATIONS} prepared writes: {elapsed:?}");
    println!(
        "per prepared write: {:.2}ns",
        elapsed.as_nanos() as f64 / ITERATIONS as f64
    );
}
