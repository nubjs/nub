//! Per-process, mtime-validated cache for config-file reads.
//!
//! The PM engine re-reads and re-parses the same handful of files (the root
//! `package.json`, `.yarnrc.yml`, the `.npmrc` ancestor chain) several times
//! per command — once during `engine_session` construction, again from the
//! later `install_config_signals` / `session_role_root` reads. Each read is a
//! `stat` + `read` + parse the previous read already did.
//!
//! This is a memoizer for the READ/parse phase only. The single thing that
//! makes it correct rather than a stale-value hazard: every lookup re-stats the
//! file and serves the cached value ONLY when the file's modification time is
//! unchanged. A mutation (the in-process aube engine rewriting `package.json`
//! mid-command) bumps the mtime, the next lookup misses, and the file is
//! re-read. So the no-stale-read property is STRUCTURAL — it does not depend on
//! call-ordering analysis: a cache validated on mtime can never serve a value
//! older than the file on disk.
//!
//! Cache MISS semantics match an uncached read exactly: a missing/unreadable
//! file, or a file whose mtime is unavailable, is never cached (the closure
//! re-runs every time), so behavior is byte-for-byte identical to the
//! pre-cache code on those paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

/// A path-keyed cache whose entries are invalidated when the underlying file's
/// freshness stamp — its `(mtime, size)` pair — changes. Stores `Arc<V>` so a
/// hit is a pointer clone.
///
/// `V` is the already-parsed value (e.g. the parsed manifest, or the raw file
/// contents) — the read + parse runs once per `(path, stamp)` pair.
pub struct MtimeCache<V> {
    inner: OnceLock<RwLock<std::collections::HashMap<PathBuf, Entry<V>>>>,
}

/// The cheap two-field freshness signal compared on every lookup. Both fields
/// come from one `stat`. `size` is belt-and-suspenders alongside `mtime`: a
/// rewrite that lands within the same mtime tick but changes the file length
/// (common on coarse-resolution filesystems, and in tests that rewrite-then-
/// reread the same path) still invalidates. A same-mtime, same-size content
/// edit is not distinguished — that case does not arise in nub's one-command-
/// per-process flow (the only mutator runs after all config reads), and the
/// cache is per-process, so there is no cross-run staleness to chase.
#[derive(PartialEq, Eq, Clone, Copy)]
struct Stamp {
    mtime: SystemTime,
    size: u64,
}

struct Entry<V> {
    /// The freshness stamp observed when this value was cached. A later lookup
    /// serves the value only if the file still reports this exact stamp.
    stamp: Stamp,
    value: Arc<V>,
}

impl<V> MtimeCache<V> {
    pub const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn map(&self) -> &RwLock<std::collections::HashMap<PathBuf, Entry<V>>> {
        self.inner
            .get_or_init(|| RwLock::new(std::collections::HashMap::new()))
    }
}

impl<V> Default for MtimeCache<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> MtimeCache<V> {
    /// Return the cached parse for `path` when the file's current freshness
    /// stamp matches the cached one; otherwise run `read` to produce a fresh
    /// value and cache it under the current stamp. A `None` from `read`
    /// (missing/unparseable file) is NOT cached — the next caller retries — so
    /// the result is identical to calling `read` directly every time.
    ///
    /// The current stamp is read with a single `stat`. If the file has no
    /// obtainable stamp, the value is computed but not cached (a `stat` failure
    /// means the file is gone or inaccessible, which the uncached path would
    /// also surface on its own `read`).
    pub fn get_or_read<F>(&self, path: &Path, read: F) -> Option<Arc<V>>
    where
        F: FnOnce() -> Option<V>,
    {
        if let Some(stamp) = current_stamp(path)
            && let Some(hit) = self.lookup_fresh(path, stamp)
        {
            return Some(hit);
        }
        let value = Arc::new(read()?);
        // Only cache when we can observe a stamp to validate against. The common
        // case (the file exists, so `read` succeeded) has one; re-stat to pin
        // the value to the version we just read. A read-then-mutate race within
        // the same command is impossible (the only mutator is the engine call
        // that runs after all config reads), but the post-read re-stat keeps the
        // cached stamp honest regardless.
        if let Some(stamp) = current_stamp(path) {
            self.map()
                .write()
                .expect("MtimeCache lock poisoned")
                .insert(
                    path.to_path_buf(),
                    Entry {
                        stamp,
                        value: Arc::clone(&value),
                    },
                );
        }
        Some(value)
    }

    fn lookup_fresh(&self, path: &Path, stamp: Stamp) -> Option<Arc<V>> {
        let guard = self.map().read().expect("MtimeCache lock poisoned");
        let entry = guard.get(path)?;
        (entry.stamp == stamp).then(|| Arc::clone(&entry.value))
    }
}

/// The file's freshness stamp `(mtime, size)`, or `None` when it can't be
/// stat'd (missing / inaccessible) or the platform reports no mtime.
///
/// Cache keys are the literal, un-canonicalized paths the callers pass in, and
/// `fs::metadata` follows symlinks to stamp the TARGET. So two distinct keys
/// that alias one file (e.g. a symlinked config path) cache independently but
/// each stamps the real target — at worst a redundant read, never a stale one.
fn current_stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        mtime: meta.modified().ok()?,
        size: meta.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// A unique temp dir (no tempfile dev-dep — matching this crate's
    /// convention, see `node::discovery`'s `resolution_tmpdir`).
    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-cfgcache-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mtime_of(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    /// Rewrite `path` with `contents`, busy-rewriting until the file's reported
    /// mtime advances past `prev` — so the test forces a genuine mtime change
    /// regardless of the filesystem's mtime granularity, without a fixed sleep
    /// or a `filetime` dev-dep. Bounded to avoid hanging.
    fn write_until_mtime_advances(path: &Path, contents: &str, prev: SystemTime) {
        for _ in 0..10_000 {
            std::fs::write(path, contents).unwrap();
            if mtime_of(path) > prev {
                return;
            }
        }
        panic!("filesystem mtime did not advance after repeated writes");
    }

    #[test]
    fn second_read_of_unchanged_file_is_a_cache_hit() {
        let dir = tmpdir("hit");
        let path = dir.join("f.txt");
        std::fs::write(&path, "v1").unwrap();

        let cache: MtimeCache<String> = MtimeCache::new();
        let reads = AtomicUsize::new(0);
        let read = |c: &AtomicUsize| {
            c.fetch_add(1, Ordering::SeqCst);
            std::fs::read_to_string(&path).ok()
        };

        let a = cache.get_or_read(&path, || read(&reads)).unwrap();
        let b = cache.get_or_read(&path, || read(&reads)).unwrap();
        assert_eq!(&*a, "v1");
        assert_eq!(&*b, "v1");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "second read must hit cache"
        );
        // Same Arc => zero-copy hit.
        assert!(Arc::ptr_eq(&a, &b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_change_invalidates_and_rereads() {
        let dir = tmpdir("inval");
        let path = dir.join("f.txt");
        std::fs::write(&path, "v1").unwrap();

        let cache: MtimeCache<String> = MtimeCache::new();
        let read = || std::fs::read_to_string(&path).ok();

        let a = cache.get_or_read(&path, read).unwrap();
        assert_eq!(&*a, "v1");
        let cached_mtime = mtime_of(&path);

        // Mutate the file (same byte length, so this exercises the MTIME half of
        // the stamp specifically) and force a later mtime so the cache must miss
        // and re-read — the same protection a mid-command engine write gets.
        write_until_mtime_advances(&path, "v2", cached_mtime);

        let b = cache.get_or_read(&path, read).unwrap();
        assert_eq!(&*b, "v2", "a changed mtime must yield the fresh contents");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rewrite that lands within the SAME mtime tick but changes the file
    /// length must still invalidate — the `size` half of the stamp. This is the
    /// rapid rewrite-then-reread pattern some config tests use, and without the
    /// size field it served a stale value on coarse-mtime filesystems. The loop
    /// hammers the rewrite to make a same-tick collision likely; the assertion
    /// must hold whether or not the tick actually collided.
    #[test]
    fn same_mtime_different_size_invalidates() {
        let dir = tmpdir("size");
        let path = dir.join("f.txt");

        let cache: MtimeCache<String> = MtimeCache::new();
        let read = || std::fs::read_to_string(&path).ok();

        for _ in 0..200 {
            std::fs::write(&path, "longer-contents").unwrap();
            let long = cache.get_or_read(&path, read).unwrap();
            assert_eq!(&*long, "longer-contents");
            // Different length — a same-mtime-tick collision here must NOT serve
            // the stale "longer-contents".
            std::fs::write(&path, "short").unwrap();
            let short = cache.get_or_read(&path, read).unwrap();
            assert_eq!(&*short, "short", "size change must invalidate the cache");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_not_cached() {
        let dir = tmpdir("absent");
        let path = dir.join("absent.txt");

        let cache: MtimeCache<String> = MtimeCache::new();
        let calls = AtomicUsize::new(0);
        let read = |c: &AtomicUsize| {
            c.fetch_add(1, Ordering::SeqCst);
            std::fs::read_to_string(&path).ok()
        };

        assert!(cache.get_or_read(&path, || read(&calls)).is_none());
        assert!(cache.get_or_read(&path, || read(&calls)).is_none());
        // A None result is never cached: a file that appears later must be seen.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
