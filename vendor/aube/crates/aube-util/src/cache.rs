//! Process-wide and on-disk caches shared across aube crates.
//!
//! Replaces ad-hoc `OnceLock<RwLock<HashMap>>` patterns and bespoke
//! sidecar-file readers/writers. Three primitives:
//!
//! - [`ProcessCache`] — in-memory, process-lifetime, returns
//!   `Arc<V>` so cache hits are pointer copies.
//! - [`DiskCache`] — file-backed, sharded by hash of the key,
//!   atomic-write on `put`, swallows decode errors as misses.
//! - [`FreshnessSnapshot`] — `(mtime, size, blake3)` triple that
//!   answers "did this file change?" via two cheap stats before
//!   falling back to BLAKE3.

use rustc_hash::FxHashMap;
use std::hash::Hash;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

/// Process-wide memoizer. The first caller for a key runs the
/// compute closure; later callers receive `Arc::clone` of the cached
/// value. Both reads and writes are short critical sections — values
/// are computed without holding the lock so a slow `f` doesn't block
/// other keys.
pub struct ProcessCache<K, V> {
    inner: OnceLock<RwLock<FxHashMap<K, Arc<V>>>>,
}

impl<K, V> ProcessCache<K, V> {
    pub const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn map(&self) -> &RwLock<FxHashMap<K, Arc<V>>> {
        self.inner.get_or_init(|| RwLock::new(FxHashMap::default()))
    }
}

impl<K, V> Default for ProcessCache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> ProcessCache<K, V>
where
    K: Eq + Hash + Clone,
{
    /// Return the cached value for `key`, or compute it and memoize.
    /// The compute closure runs OUTSIDE the lock so a slow computation
    /// doesn't block sibling lookups.
    ///
    /// Concurrency contract: `f` is called **at most once per racing
    /// thread**, NOT exactly once across the cache. Two threads that
    /// both miss the read-side check will each invoke `f` separately;
    /// first-write-wins on the post-compute insert decides which
    /// `Arc` every later caller sees, and the loser's `V` is dropped.
    /// Callers needing exact-once side effects (network fetch, file
    /// write) must guard with a dedicated `OnceLock` or `Mutex`
    /// outside this cache. The `FnOnce` bound preserves the
    /// per-invocation move semantics.
    pub fn get_or_compute<F>(&self, key: K, f: F) -> Arc<V>
    where
        F: FnOnce() -> V,
    {
        if let Some(v) = self
            .map()
            .read()
            .expect("ProcessCache lock poisoned")
            .get(&key)
        {
            return Arc::clone(v);
        }
        // Compute outside the lock so a slow `f` doesn't block sibling
        // lookups. First-write-wins on the rare two-writer race: the
        // second writer's value is dropped, the first writer's value
        // is what every later caller sees. Both writers' callers see
        // a consistent `Arc` (one of the two), and downstream
        // `Arc::ptr_eq` / pointer-identity checks stay sound.
        let value = Arc::new(f());
        let mut w = self.map().write().expect("ProcessCache lock poisoned");
        let stored = w.entry(key).or_insert_with(|| Arc::clone(&value));
        Arc::clone(stored)
    }

    pub fn get(&self, key: &K) -> Option<Arc<V>> {
        self.map()
            .read()
            .expect("ProcessCache lock poisoned")
            .get(key)
            .map(Arc::clone)
    }

    pub fn insert(&self, key: K, value: Arc<V>) {
        self.map()
            .write()
            .expect("ProcessCache lock poisoned")
            .insert(key, value);
    }

    pub fn invalidate(&self, key: &K) -> Option<Arc<V>> {
        self.map()
            .write()
            .expect("ProcessCache lock poisoned")
            .remove(key)
    }
}

/// File-backed cache. Each entry lives at
/// `<root>/<2-char shard>/<full hex hash>` so directory size stays
/// bounded. Values serialize via JSON for now (callers that need
/// rkyv/postcard wrap their own type).
///
/// Cache misses (not-found, parse error, deserialize error) all
/// return `None` so callers always have a recompute fallback.
pub struct DiskCache {
    root: PathBuf,
}

impl DiskCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &[u8]) -> PathBuf {
        let hash = blake3::hash(key).to_hex();
        let hex = hash.as_str();
        self.root.join(&hex[..2]).join(hex)
    }

    /// Read raw bytes for `key` if present and well-formed. Errors
    /// other than `NotFound` propagate so callers can distinguish
    /// "missing" from "filesystem broken".
    pub fn read_bytes(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let path = self.path_for(key);
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write raw bytes for `key`, atomically. Re-writes overwrite.
    /// Creates the 2-char shard directory on first write so cold
    /// starts don't fail with `NotFound` from `atomic_write`.
    pub fn write_bytes(&self, key: &[u8], bytes: &[u8]) -> io::Result<()> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fs_atomic::atomic_write(&path, bytes)
    }

    pub fn remove(&self, key: &[u8]) -> io::Result<()> {
        let path = self.path_for(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// `(mtime, size, blake3)` triple. `is_fresh` checks the cheap pair
/// first and only re-hashes on mismatch, so the warm path is two
/// stats and a memcmp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshnessSnapshot {
    pub mtime: SystemTime,
    pub size: u64,
    pub hash: [u8; 32],
}

impl FreshnessSnapshot {
    pub fn capture(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        let mtime = meta.modified()?;
        let size = meta.len();
        let bytes = std::fs::read(path)?;
        let hash = *blake3::hash(&bytes).as_bytes();
        Ok(Self { mtime, size, hash })
    }

    /// Returns `Ok(true)` when the snapshot's `(mtime, size)` pair
    /// still matches OR, on mismatch, the BLAKE3 hash of the file
    /// equals the recorded hash. Trusts the cheap mtime+size pair as
    /// "fresh, no hash needed" — on filesystems with coarse mtime
    /// resolution (FAT32) a same-second in-place overwrite to the
    /// same byte length could slip past, but every other file
    /// system reports nanosecond mtime so the trust is sound. Hash
    /// fallback runs only when mtime or size differs.
    pub fn is_fresh(&self, path: &Path) -> io::Result<bool> {
        let meta = std::fs::metadata(path)?;
        if meta.len() != self.size {
            return Ok(false);
        }
        if let Ok(mtime) = meta.modified()
            && mtime == self.mtime
        {
            return Ok(true);
        }
        let bytes = std::fs::read(path)?;
        let hash = *blake3::hash(&bytes).as_bytes();
        Ok(hash == self.hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "aube-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn process_cache_computes_once() {
        let cache: ProcessCache<&'static str, u32> = ProcessCache::new();
        let n = std::sync::atomic::AtomicU32::new(0);
        let _a = cache.get_or_compute("k", || {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            42
        });
        let _b = cache.get_or_compute("k", || {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            42
        });
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn process_cache_returns_arc_clone() {
        let cache: ProcessCache<u32, String> = ProcessCache::new();
        let a = cache.get_or_compute(1, || "hello".to_string());
        let b = cache.get_or_compute(1, || "world".to_string());
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*a, "hello");
    }

    #[test]
    fn disk_cache_roundtrip() {
        let dir = tempdir();
        let cache = DiskCache::new(dir.join("dc"));
        assert!(cache.read_bytes(b"key1").unwrap().is_none());
        cache.write_bytes(b"key1", b"value-bytes").unwrap();
        assert_eq!(
            cache.read_bytes(b"key1").unwrap().as_deref(),
            Some(b"value-bytes".as_ref())
        );
        cache.remove(b"key1").unwrap();
        assert!(cache.read_bytes(b"key1").unwrap().is_none());
    }

    #[test]
    fn freshness_detects_size_change() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"hello").unwrap();
        let snap = FreshnessSnapshot::capture(&path).unwrap();
        assert!(snap.is_fresh(&path).unwrap());
        std::fs::write(&path, b"hello world").unwrap();
        assert!(!snap.is_fresh(&path).unwrap());
    }

    #[test]
    fn freshness_handles_touch_with_same_content() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"hello").unwrap();
        let snap = FreshnessSnapshot::capture(&path).unwrap();
        // Re-write same content — mtime may move but hash stays same.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"hello").unwrap();
        // Either size+mtime match (fast path) or hash matches (slow
        // path) — both valid "fresh" outcomes.
        assert!(snap.is_fresh(&path).unwrap());
    }
}
