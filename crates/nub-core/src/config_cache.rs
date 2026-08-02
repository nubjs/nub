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
//! That argument only closes once the RACY window is handled. File mtimes are
//! quantized to the platform's timer tick — measured at 0.39-1.4 ms on Windows
//! Server 2022/NTFS — so a rewrite landing in the same tick as the cached read
//! reports the very same mtime. `size` catches that only when the length also
//! changed, and a same-length edit (`pnpm@10.0.0` → `pnpm@11.0.0`, a patch bump
//! inside a `packageManager` pin) is invisible to both fields. Entries therefore
//! also carry the instant they were cached, and are trusted only once a full
//! [`MTIME_GRANULARITY_SLOP`] separates the file's mtime from it — proof the
//! mtime's tick had already closed, so no later write can reuse that value.
//!
//! The slop is the whole trick, and leaving it out makes the check a no-op:
//! `SystemTime::now()` is PRECISE (`GetSystemTimePreciseAsFileTime` on Windows,
//! nanoseconds elsewhere) while the mtime it is compared against is QUANTIZED,
//! so for any just-written file `mtime <= now()` holds by construction. Measured:
//! a bare `mtime < cached_at` rejected 0 of 2000 trials on both macOS and
//! Windows. Git avoids the trap by comparing an entry's mtime against the index
//! FILE's mtime — two values from the same quantized domain. Comparing a precise
//! clock against a quantized timestamp needs the slop to mean anything.
//!
//! Cache MISS semantics match an uncached read exactly: a missing/unreadable
//! file, or a file whose mtime is unavailable, is never cached (the closure
//! re-runs every time), so behavior is byte-for-byte identical to the
//! pre-cache code on those paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

/// How far a file's mtime must precede the read that cached it before the
/// `(mtime, size)` stamp is trusted to detect a later rewrite. Must exceed the
/// platform's mtime quantum, since two writes inside one quantum report the same
/// mtime.
///
/// Two seconds covers every quantum nub can land on with margin: 0.39-1.4 ms
/// measured on Windows Server 2022/NTFS, up to the 15.6 ms Windows timer tick
/// when no process has raised the timer resolution, and FAT32's 2 s. Sized
/// generously because the cost is close to zero — config nub reads is seconds to
/// days old, so it clears the slop and still hits. Only a file written within two
/// seconds of the read goes uncached, and the fallback there is exactly the
/// uncached read this cache replaced.
const MTIME_GRANULARITY_SLOP: Duration = Duration::from_secs(2);

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
    /// same-mtime same-size rewrite cannot be served stale. An entry is trusted
    /// only once `cached_at - stamp.mtime >= MTIME_GRANULARITY_SLOP`, which
    /// proves the mtime's quantum had already closed when the read happened, so
    /// every later write necessarily lands in a later quantum and reports a
    /// different mtime. A file still inside its own quantum is re-read on every
    /// lookup instead.
    ///
    /// Sampled before rather than after the read so the read-then-stat window is
    /// covered too: a write landing mid-read also lands at-or-after this instant.
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
        (entry.stamp == stamp && mtime_quantum_closed(stamp.mtime, entry.cached_at))
            .then(|| Arc::clone(&entry.value))
    }
}

/// Whether `mtime`'s quantum had provably closed by `cached_at`, i.e. whether the
/// `(mtime, size)` stamp can be trusted to notice a rewrite that happened after
/// the caching read. A backwards clock step yields `Err` here and reads as "not
/// closed", which is the safe direction.
pub(crate) fn mtime_quantum_closed(mtime: SystemTime, cached_at: SystemTime) -> bool {
    cached_at
        .duration_since(mtime)
        .is_ok_and(|elapsed| elapsed >= MTIME_GRANULARITY_SLOP)
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

    fn set_mtime(path: &Path, mtime: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// Backdate `path` by `secs`, well clear of [`MTIME_GRANULARITY_SLOP`], making
    /// it cacheable — the state every real on-disk config file is already in.
    /// Tests asserting a cache HIT need this, since a file written moments ago is
    /// deliberately never trusted. Callers that age the same path twice pass
    /// DIFFERENT offsets, so the two mtimes differ by construction rather than by
    /// however much wall-clock drifted between the calls.
    fn age(path: &Path, secs: u64) {
        set_mtime(path, SystemTime::now() - Duration::from_secs(secs));
    }

    #[test]
    fn second_read_of_unchanged_file_is_a_cache_hit() {
        let dir = tmpdir("hit");
        let path = dir.join("f.txt");
        std::fs::write(&path, "v1").unwrap();
        // Only a file whose mtime quantum has closed is cacheable at all.
        age(&path, 60);

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
        age(&path, 60);

        let cache: MtimeCache<String> = MtimeCache::new();
        let read = || std::fs::read_to_string(&path).ok();

        let a = cache.get_or_read(&path, read).unwrap();
        assert_eq!(&*a, "v1");

        // Mutate the file (same byte length, so this exercises the MTIME half of
        // the stamp specifically) and age it again, so it stays cacheable and the
        // MISS is attributable to the changed mtime rather than to the slop.
        std::fs::write(&path, "v2").unwrap();
        age(&path, 30);

        let b = cache.get_or_read(&path, read).unwrap();
        assert_eq!(&*b, "v2", "a changed mtime must yield the fresh contents");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file still inside its own mtime quantum when it is read must never be
    /// cached, because a rewrite moments later reports a byte-identical
    /// `(mtime, size)` and would otherwise be served stale. That is exactly the
    /// `pnpm@10.0.0` → `pnpm@11.0.0` manifest rewrite (32 bytes either way) that
    /// flaked on Windows CI, where two writes separated only by a read collide on
    /// mtime the majority of the time.
    ///
    /// The collision is pinned rather than raced, because an ns-resolution
    /// filesystem essentially never produces it on its own. Both versions are
    /// pinned to `now()` — where a real coarse-clock write lands, and the value
    /// that makes this reproduce identically on every platform. Pinning to a
    /// FUTURE instant would not: no clock hands one out, and a test built that way
    /// passes against a guard that does nothing.
    #[test]
    fn rewrite_inside_the_cached_mtime_quantum_is_never_served_stale() {
        let dir = tmpdir("racy");
        let path = dir.join("f.txt");

        let cache: MtimeCache<String> = MtimeCache::new();
        let read = || std::fs::read_to_string(&path).ok();

        for (first, second) in [
            ("pnpm@10.0.0", "pnpm@11.0.0"), // same length — the flake's shape
            ("longer-contents", "short"),   // length also changes
        ] {
            let collided = SystemTime::now();

            std::fs::write(&path, first).unwrap();
            set_mtime(&path, collided);
            assert_eq!(&*cache.get_or_read(&path, read).unwrap(), first);

            std::fs::write(&path, second).unwrap();
            set_mtime(&path, collided);
            assert_eq!(
                mtime_of(&path),
                collided,
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
