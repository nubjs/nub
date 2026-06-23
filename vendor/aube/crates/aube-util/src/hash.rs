use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;

/// Files at or above this size hash via `update_mmap_rayon`. Below it
/// the streaming reader wins because mmap setup (vma alloc + first
/// page fault) dominates for small files. 4 MiB is the elbow on
/// modern x86 + ARM64; benched against `aube-store` CAS verify on
/// `fixtures/medium`.
pub const BLAKE3_MMAP_THRESHOLD: u64 = 4 * 1024 * 1024;

/// Length-prefixed, tagged BLAKE3 builder. Every field carries a
/// short ASCII tag plus a `u64` length so concatenation collisions
/// are impossible: `("a", "bc")` and `("ab", "c")` produce different
/// digests.
///
/// All hashers in aube that mix multiple typed fields (per-package
/// fingerprints, graph node hashes, dep_path short names) should use
/// this so the encoding stays uniform across crates.
#[derive(Debug, Default, Clone)]
pub struct Blake3Builder(blake3::Hasher);

impl Blake3Builder {
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    /// Mix raw bytes without any tag or length prefix. Use only for
    /// fixed-shape payloads where the position carries the meaning.
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// Mix a tagged, length-prefixed field.
    pub fn field(&mut self, tag: &[u8], bytes: &[u8]) -> &mut Self {
        self.0.update(tag);
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    /// Mix an `Option<&[u8]>`. `None` is encoded as a length of
    /// `u64::MAX` so it cannot collide with any real-length payload.
    pub fn optional(&mut self, tag: &[u8], value: Option<&[u8]>) -> &mut Self {
        match value {
            Some(b) => self.field(tag, b),
            None => {
                self.0.update(tag);
                self.0.update(&u64::MAX.to_le_bytes());
                self
            }
        }
    }

    /// Mix an iterable list of byte items. The list count is
    /// length-prefixed first, then each item is tagged with `i`.
    pub fn list<'a, I>(&mut self, tag: &[u8], items: I) -> &mut Self
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let collected: Vec<&[u8]> = items.into_iter().collect();
        self.0.update(tag);
        self.0.update(&(collected.len() as u64).to_le_bytes());
        for item in collected {
            self.field(b"i", item);
        }
        self
    }

    /// Finalize as a 64-char hex string.
    pub fn finalize_hex(&self) -> String {
        self.0.finalize().to_hex().to_string()
    }

    /// Finalize as raw 32 bytes.
    pub fn finalize_bytes(&self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }

    /// Finalize as a short hex prefix written into a stack buffer.
    /// Returns the borrowed `&str` view. The buffer must be large
    /// enough for the requested prefix length.
    pub fn finalize_short_hex<'a, const N: usize>(&self, buf: &'a mut [u8; N]) -> &'a str {
        let full = self.0.finalize();
        let hex = full.to_hex();
        let bytes = hex.as_bytes();
        let take = N.min(bytes.len());
        buf[..take].copy_from_slice(&bytes[..take]);
        std::str::from_utf8(&buf[..take]).expect("hex is ASCII")
    }
}

/// Trait-object wrapper for any byte-eating hasher. The caller adapts
/// their concrete hasher (e.g. `sha2::Sha512`) into a `&mut dyn
/// ByteHasher` so this crate stays free of the `sha2` dep.
pub trait ByteHasher {
    fn update(&mut self, bytes: &[u8]);
}

impl ByteHasher for blake3::Hasher {
    fn update(&mut self, bytes: &[u8]) {
        blake3::Hasher::update(self, bytes);
    }
}

/// `Read` adapter that updates one or more hashers as bytes flow
/// through. Used by streaming tarball verification (BLAKE3 per CAS
/// entry, SHA-512 for tarball integrity, both updated incrementally
/// while the body downloads).
pub struct TeeReader<'h, R> {
    inner: R,
    hashers: Vec<&'h mut dyn ByteHasher>,
}

impl<'h, R> TeeReader<'h, R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hashers: Vec::new(),
        }
    }

    pub fn with_hasher(mut self, h: &'h mut dyn ByteHasher) -> Self {
        self.hashers.push(h);
        self
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: io::Read> io::Read for TeeReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            for h in self.hashers.iter_mut() {
                h.update(&buf[..n]);
            }
        }
        Ok(n)
    }
}

/// BLAKE3 a file, picking the fastest path for its size.
///
/// Files at or above [`BLAKE3_MMAP_THRESHOLD`] use
/// `Hasher::update_mmap_rayon`, which mmaps the file and hashes
/// chunks across rayon workers (~6-10 GB/s on modern x86 vs ~2 GB/s
/// for single-threaded streaming). Smaller files use a buffered read,
/// because mmap setup cost dominates below the threshold.
///
/// Caller must guarantee the file is not mutated during the call.
/// Aube CAS files are write-once + content-addressed by construction
/// (`O_CREAT|O_EXCL` write semantics in `aube-store`), so this
/// invariant holds for every hash check on a CAS path.
///
/// `AUBE_DISABLE_MMAP_BLAKE3=1` forces the streaming path on every
/// call. Useful as a killswitch if a future kernel + filesystem
/// combination regresses.
pub fn blake3_hash_file<P: AsRef<Path>>(path: P) -> io::Result<[u8; 32]> {
    let path = path.as_ref();
    let mut hasher = blake3::Hasher::new();
    if mmap_disabled() {
        return blake3_streaming(path, hasher);
    }
    let metadata = std::fs::metadata(path)?;
    if metadata.len() >= BLAKE3_MMAP_THRESHOLD {
        // Bubble mmap errors (EINVAL on tmpfs in some sandboxes,
        // EACCES on noexec mounts) up to the streaming fallback so
        // we never poison the hash with a partial mmap. Trace at
        // debug so users on those filesystems can see why the fast
        // path isn't engaging.
        match hasher.update_mmap_rayon(path) {
            Ok(_) => return Ok(*hasher.finalize().as_bytes()),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    path = %path.display(),
                    "blake3 mmap unavailable, falling back to streaming",
                );
                hasher = blake3::Hasher::new();
            }
        }
    }
    blake3_streaming(path, hasher)
}

fn blake3_streaming(path: &Path, mut hasher: blake3::Hasher) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    io::copy(&mut file, &mut hasher)?;
    Ok(*hasher.finalize().as_bytes())
}

fn mmap_disabled() -> bool {
    crate::env::embedder_env("DISABLE_MMAP_BLAKE3").is_some()
}

pub fn ordered_seq_hash<I, T>(iter: I) -> u64
where
    I: IntoIterator<Item = T>,
    T: Hash,
    I::IntoIter: ExactSizeIterator,
{
    let iter = iter.into_iter();
    let mut h = rustc_hash::FxHasher::default();
    iter.len().hash(&mut h);
    for item in iter {
        item.hash(&mut h);
    }
    h.finish()
}

pub fn meta_hash<'a, I, S>(packages: I, scripts: S) -> [u8; 32]
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
    S: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aube-meta-v1\npackages\n");
    for (name, version) in packages {
        hasher.update(name.as_bytes());
        hasher.update(b"@");
        hasher.update(version.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"scripts\n");
    for (name, body) in scripts {
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(body.as_bytes());
        hasher.update(b"\n");
    }
    *hasher.finalize().as_bytes()
}

/// Manifest fields, other than the tool's own namespace, that shape the
/// install. The active embedder's `manifest_namespace` is folded in at the
/// digest site (it sorts ahead of these for standalone aube → `"aube"`).
pub const INSTALL_SHAPE_FIELDS: &[&str] = &[
    "bundleDependencies",
    "bundledDependencies",
    "catalog",
    "catalogs",
    "dependencies",
    "devDependencies",
    "engines",
    "name",
    "optionalDependencies",
    "overrides",
    "peerDependencies",
    "peerDependenciesMeta",
    "pnpm",
    "publishConfig",
    "resolutions",
    "version",
    "workspaces",
];

pub fn manifest_install_shape_digest(manifest: &serde_json::Value) -> [u8; 32] {
    let obj = match manifest.as_object() {
        Some(o) => o,
        None => return *blake3::hash(b"aube-manifest-v1/not-an-object").as_bytes(),
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aube-manifest-v1\n");
    // The tool's own manifest namespace shapes the install too, but it's
    // embedder-derived rather than fixed. Hash it ahead of the static fields
    // so standalone aube (`manifest_namespace == "aube"`) reproduces the
    // historical order; an embedder with no namespace (`""`) skips it, and a
    // switch self-heals — the digest invalidates once, forcing one re-derive.
    let namespace = crate::embedder().manifest_namespace;
    let fields = std::iter::once(namespace)
        .filter(|ns| !ns.is_empty())
        .chain(INSTALL_SHAPE_FIELDS.iter().copied());
    for field in fields {
        if let Some(v) = obj.get(field) {
            hasher.update(field.as_bytes());
            hasher.update(b"=");
            canonical_json(v, &mut hasher);
            hasher.update(b"\n");
        }
    }
    *hasher.finalize().as_bytes()
}

fn canonical_json(v: &serde_json::Value, hasher: &mut blake3::Hasher) {
    use serde_json::Value;
    match v {
        Value::Null => {
            hasher.update(b"null");
        }
        Value::Bool(b) => {
            hasher.update(if *b { b"true" } else { b"false" });
        }
        Value::Number(n) => {
            hasher.update(n.to_string().as_bytes());
        }
        Value::String(s) => {
            hasher.update(b"\"");
            hasher.update(s.as_bytes());
            hasher.update(b"\"");
        }
        Value::Array(items) => {
            hasher.update(b"[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    hasher.update(b",");
                }
                canonical_json(item, hasher);
            }
            hasher.update(b"]");
        }
        Value::Object(obj) => {
            hasher.update(b"{");
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    hasher.update(b",");
                }
                hasher.update(b"\"");
                hasher.update(k.as_bytes());
                hasher.update(b"\":");
                if let Some(val) = obj.get(*k) {
                    canonical_json(val, hasher);
                }
            }
            hasher.update(b"}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_hash_file_matches_in_memory_under_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.bin");
        let data = b"hello aube small file";
        std::fs::write(&path, data).unwrap();
        let from_file = blake3_hash_file(&path).unwrap();
        let from_mem = *blake3::hash(data).as_bytes();
        assert_eq!(from_file, from_mem);
    }

    #[test]
    fn blake3_hash_file_matches_in_memory_over_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // 5 MiB crosses BLAKE3_MMAP_THRESHOLD and exercises the
        // mmap-rayon path. Pseudo-random bytes (deterministic) so
        // the test stays reproducible without a CSPRNG dep.
        let mut data = Vec::with_capacity(5 * 1024 * 1024);
        for i in 0..(5 * 1024 * 1024u32) {
            data.push((i.wrapping_mul(2654435761) >> 24) as u8);
        }
        std::fs::write(&path, &data).unwrap();
        let from_file = blake3_hash_file(&path).unwrap();
        let from_mem = *blake3::hash(&data).as_bytes();
        assert_eq!(from_file, from_mem);
    }

    #[test]
    fn blake3_streaming_path_matches_in_memory() {
        // Direct test of the streaming fallback that bypasses the
        // mmap path entirely — exercises the same code the killswitch
        // would, without touching a process-global env var (which
        // would race with the parallel tests above that also call
        // `blake3_hash_file`).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.bin");
        let data = vec![0xa5u8; (BLAKE3_MMAP_THRESHOLD as usize) + 1024];
        std::fs::write(&path, &data).unwrap();
        let streamed = blake3_streaming(&path, blake3::Hasher::new()).unwrap();
        let from_mem = *blake3::hash(&data).as_bytes();
        assert_eq!(streamed, from_mem);
    }

    #[test]
    fn ordered_seq_hash_is_order_sensitive() {
        let a = ordered_seq_hash(["a", "b", "c"].iter().copied());
        let b = ordered_seq_hash(["c", "b", "a"].iter().copied());
        assert_ne!(a, b);
    }

    #[test]
    fn ordered_seq_hash_detects_count_changes() {
        let short = ordered_seq_hash(["a", "b"].iter().copied());
        let long = ordered_seq_hash(["a", "b", "c"].iter().copied());
        assert_ne!(short, long);
    }

    #[test]
    fn meta_hash_stable_for_same_inputs() {
        let pkgs = [("react", "19.0.0"), ("next", "15.1.3")];
        let scripts: [(&str, &str); 0] = [];
        let a = meta_hash(pkgs.iter().copied(), scripts.iter().copied());
        let b = meta_hash(pkgs.iter().copied(), scripts.iter().copied());
        assert_eq!(a, b);
    }

    #[test]
    fn manifest_digest_ignores_scripts_and_license() {
        let a: serde_json::Value = serde_json::from_str(
            r#"{"name":"x","version":"1.0.0","dependencies":{"react":"19.0.0"},"scripts":{"test":"vitest"},"license":"MIT"}"#,
        )
        .unwrap();
        let b: serde_json::Value = serde_json::from_str(
            r#"{"name":"x","version":"1.0.0","dependencies":{"react":"19.0.0"},"scripts":{"test":"jest --watch"},"license":"Apache-2.0"}"#,
        )
        .unwrap();
        assert_eq!(
            manifest_install_shape_digest(&a),
            manifest_install_shape_digest(&b)
        );
    }

    #[test]
    fn manifest_digest_reacts_to_dep_change() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"dependencies":{"react":"19.0.0"}}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"dependencies":{"react":"19.1.0"}}"#).unwrap();
        assert_ne!(
            manifest_install_shape_digest(&a),
            manifest_install_shape_digest(&b)
        );
    }

    #[test]
    fn manifest_digest_stable_under_key_reorder() {
        let a: serde_json::Value = serde_json::from_str(
            r#"{"name":"x","dependencies":{"b":"1","a":"2"},"devDependencies":{"c":"3"}}"#,
        )
        .unwrap();
        let b: serde_json::Value = serde_json::from_str(
            r#"{"devDependencies":{"c":"3"},"dependencies":{"a":"2","b":"1"},"name":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            manifest_install_shape_digest(&a),
            manifest_install_shape_digest(&b)
        );
    }

    #[test]
    fn meta_hash_reacts_to_script_change() {
        let pkgs = [("react", "19.0.0")];
        let s1 = [("build", "tsc")];
        let s2 = [("build", "tsc --watch")];
        let a = meta_hash(pkgs.iter().copied(), s1.iter().copied());
        let b = meta_hash(pkgs.iter().copied(), s2.iter().copied());
        assert_ne!(a, b);
    }
}
