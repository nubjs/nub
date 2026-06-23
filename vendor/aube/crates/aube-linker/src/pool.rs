use tracing::warn;

pub fn default_linker_parallelism() -> usize {
    // The macOS cap was 4 because the old `auto` strategy hardlinked
    // every file, and concurrent `hard_link` into the same directory
    // serializes on HFS+/APFS directory-mutation locks past ~4 threads.
    // `auto` now reflinks (clonefile), which writes a fresh inode per
    // file and doesn't take that lock, so the link pass scales with
    // cores. A sweep-line over the per-file diag spans showed the
    // 4-thread cap pinning a 10-core machine at ~3.9 effective
    // concurrency with ~6 idle cores; lifting the cap lets the
    // clonefile pass use them. 16 matches the non-macOS default and is
    // itself bounded by `available_parallelism` below.
    let default_limit = 16;

    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(default_limit)
}

type LinkPoolCache = std::sync::Mutex<Vec<(usize, std::sync::Arc<rayon::ThreadPool>)>>;
static LINK_POOL_CACHE: std::sync::OnceLock<LinkPoolCache> = std::sync::OnceLock::new();

fn link_pool(threads: usize) -> Option<std::sync::Arc<rayon::ThreadPool>> {
    let cache = LINK_POOL_CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = cache.lock().ok()?;
    if let Some((_, pool)) = guard.iter().find(|(t, _)| *t == threads) {
        return Some(pool.clone());
    }
    match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("aube-linker-{i}"))
        .build()
    {
        Ok(pool) => {
            let pool = std::sync::Arc::new(pool);
            guard.push((threads, pool.clone()));
            Some(pool)
        }
        Err(err) => {
            warn!("failed to build aube linker thread pool: {err}; falling back to caller thread");
            None
        }
    }
}

pub(crate) fn with_link_pool<R: Send>(threads: usize, f: impl FnOnce() -> R + Send) -> R {
    match link_pool(threads) {
        Some(pool) => pool.install(f),
        None => f(),
    }
}
