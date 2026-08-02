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
//! That argument only closes once the RACY window is handled: mtime resolution
//! is the platform clock's, not the filesystem's, and on Windows that clock
//! ticks coarsely enough that a rewrite microseconds after the cached read can
//! report the very same mtime. `size` catches such a rewrite only when the
//! length also changed — a same-length edit (`pnpm@10.0.0` → `pnpm@11.0.0`, a
//! patch bump inside a `packageManager` pin) is invisible to both fields. So
//! entries also carry the instant they were cached and are trusted only once
//! the file's mtime is strictly OLDER than that instant. This is git's
//! "racily clean" rule: a stamp taken in the same tick as the observation
//! cannot prove the file did not change afterwards, so it is never trusted on
//! its own. See [`Entry::cached_at`].
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
/// still invalidates on length alone. A same-mtime, same-size content edit is
/// not distinguished by these two fields at all — [`Entry::cached_at`] is what
/// covers that case.
#[derive(PartialEq, Eq, Clone, Copy)]
struct Stamp {
    mtime: SystemTime,
    size: u64,
}

struct Entry<V> {
    /// The freshness stamp observed when this value was cached. A later lookup
    /// serves the value only if the file still reports this exact stamp.
    stamp: Stamp,
    /// The instant sampled just BEFORE the value was read, and the reason a
    /// same-mtime same-size rewrite cannot be served stale: an entry is trusted
    /// only while `stamp.mtime < cached_at`, i.e. the file was already at least
    /// one clock tick old when it was read. A file written during or after that
    /// read reports `mtime >= cached_at` and is re-read on every lookup — the
    /// coarse-clock case can no longer masquerade as unchanged.
    ///
    /// Sampled before rather than after the read so the read-then-stat window is
    /// covered too: a write landing mid-read also lands at-or-after this instant.
    ///
    /// Cost is confined to exactly the files this protects. Config already on
    /// disk when the process started — every real `.npmrc` / `package.json` /
    /// `.yarnrc.yml`, and the whole reason the cache exists — has an mtime well
    /// below `cached_at` and still hits. Only a file nub itself just wrote, or
    /// one written within the tick before it read, goes uncached.
    cached_at: SystemTime,
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
        // Sampled before the read so a write landing mid-read is also caught —
        // see `Entry::cached_at`.
        let cached_at = SystemTime::now();
        let value = Arc::new(read()?);
        // Only cache when we can observe a stamp to validate against. The common
        // case (the file exists, so `read` succeeded) has one; re-stat to pin
        // the value to the version we just read.
        if let Some(stamp) = current_stamp(path) {
            self.map()
                .write()
                .expect("MtimeCache lock poisoned")
                .insert(
                    path.to_path_buf(),
                    Entry {
                        stamp,
                        cached_at,
                        value: Arc::clone(&value),
                    },
                );
        }
        Some(value)
    }

    fn lookup_fresh(&self, path: &Path, stamp: Stamp) -> Option<Arc<V>> {
        let guard = self.map().read().expect("MtimeCache lock poisoned");
        let entry = guard.get(path)?;
        (entry.stamp == stamp && stamp.mtime < entry.cached_at).then(|| Arc::clone(&entry.value))
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

    fn set_mtime(path: &Path, mtime: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// Spin until the clock has passed `path`'s mtime, so a subsequent
    /// `get_or_read` caches an entry that is trusted rather than racy-distrusted
    /// (see `Entry::cached_at`). Costs at most one clock tick.
    fn settle(path: &Path) {
        let mtime = mtime_of(path);
        for _ in 0..1_000_000 {
            if SystemTime::now() > mtime {
                return;
            }
            std::hint::spin_loop();
        }
        panic!("clock did not advance past the file's mtime");
    }

    #[test]
    fn second_read_of_unchanged_file_is_a_cache_hit() {
        let dir = tmpdir("hit");
        let path = dir.join("f.txt");
        std::fs::write(&path, "v1").unwrap();
        // Only a file that predates the read is cacheable at all.
        settle(&path);

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

    /// A file still inside its own mtime tick when it is read must never be
    /// cached, because a rewrite moments later can report a byte-identical
    /// `(mtime, size)` and would otherwise be served stale. That is exactly the
    /// `pnpm@10.0.0` → `pnpm@11.0.0` manifest rewrite (32 bytes either way) that
    /// flaked on Windows CI, where two writes separated only by a read collide
    /// on mtime the majority of the time.
    ///
    /// Reproducing it needs the collision forced rather than raced: on an
    /// ns-resolution filesystem the two writes essentially never share an mtime.
    /// Pinning both stamps to an instant the reader's clock has not yet passed
    /// models what a coarse clock hands out — the write lands at or after the
    /// reader's own `now()`, since both quantize to the same tick — and makes
    /// the case deterministic on every platform.
    #[test]
    fn rewrite_inside_the_cached_mtime_tick_is_never_served_stale() {
        let dir = tmpdir("racy");
        let path = dir.join("f.txt");

        let cache: MtimeCache<String> = MtimeCache::new();
        let read = || std::fs::read_to_string(&path).ok();

        for (first, second) in [
            ("pnpm@10.0.0", "pnpm@11.0.0"), // same length — the flake's shape
            ("longer-contents", "short"),   // length also changes
        ] {
            let unpassed = SystemTime::now() + std::time::Duration::from_millis(50);

            std::fs::write(&path, first).unwrap();
            set_mtime(&path, unpassed);
            assert_eq!(&*cache.get_or_read(&path, read).unwrap(), first);

            std::fs::write(&path, second).unwrap();
            set_mtime(&path, unpassed);
            assert_eq!(
                mtime_of(&path),
                unpassed,
                "the two versions must be indistinguishable by stamp for this to test anything"
            );

            assert_eq!(
                &*cache.get_or_read(&path, read).unwrap(),
                second,
                "a rewrite sharing the cached mtime must not serve {first:?}"
            );
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
