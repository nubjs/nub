#[macro_use]
extern crate log;

pub mod dirs;

mod cas;
mod git;
mod index;
mod integrity;
mod tarball;

pub use git::{
    codeload_cache_integrity, codeload_cache_lookup, extract_codeload_tarball, git_host_in_list,
    git_resolve_ref, git_shallow_clone, git_url_host,
};

#[cfg(test)]
pub(crate) use cas::blake3_hex;
pub(crate) use cas::cas_file_matches_len;
use cas::copy_dir_recursive;
#[cfg(test)]
use git::{
    codeload_cache_paths, extract_codeload_tarball_at, git_commit_matches, validate_git_positional,
};
pub use index::{PackageIndex, StoredFile, index_content_fingerprint};
pub use integrity::{
    SHA512_INTEGRITY_PREFIX, integrity_to_hex, sha512_integrity, validate_and_encode_name,
    validate_pkg_content, validate_version, verify_integrity, verify_precomputed_sha512,
};
#[cfg(test)]
pub(crate) use tarball::normalize_tar_entry_path;
pub(crate) use tarball::{
    CappedReader, MAX_TARBALL_DECOMPRESSED_BYTES, MAX_TARBALL_ENTRIES, MAX_TARBALL_ENTRY_BYTES,
};

#[cfg(test)]
use sha1::Sha1;
#[cfg(test)]
use sha2::{Digest as _, Sha256, Sha384, Sha512};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;

pub const CACHE_DIR_NAME: &str = "aube-cache";
pub const INDEX_SUBDIR: &str = "index";
/// Sibling of `files/` holding per-package extracted trees — the
/// single-`clonefile(2)` clone sources for the macOS whole-dir linker
/// fast path. See [`Store::trees_dir`].
pub const TREES_SUBDIR: &str = "trees";
pub const VIRTUAL_STORE_SUBDIR: &str = "virtual-store";
pub const PACKUMENT_CACHE_SUBDIR: &str = "packuments-v1";
pub const PACKUMENT_FULL_CACHE_SUBDIR: &str = "packuments-full-v1";

/// The global content-addressable store, owned by aube.
///
/// Default location: `$XDG_DATA_HOME/aube/store/v1/files/` (falling
/// back to `~/.local/share/aube/store/v1/files/`).
/// Files are stored by BLAKE3 hash with two-char hex directory sharding.
/// (Tarball-level integrity is still SHA-512 because that's the format the
/// npm registry returns; the per-file CAS key is an internal choice.)
///
/// Layout under the store-version directory (`v1/`):
/// - `v1/files/` — CAS shards, content-addressed by BLAKE3 hex
/// - `v1/index/` — cached package indexes (kept next to `files/` so a
///   single backup/mount captures the whole store; matches pnpm's
///   `~/.pnpm-store/v11/{files,index.db}` grouping)
///
/// `cache_dir` ($XDG_CACHE_HOME/aube) still holds genuinely
/// regenerable caches: the virtual store and packument metadata.
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    cache_dir: PathBuf,
    /// When set, `create_cas_file` writes directly to the final
    /// content-addressed path on non-Linux platforms instead of the
    /// tempfile-then-rename dance. Caller must guarantee no concurrent
    /// installer is writing into this store — typically via an exclusive
    /// file lock taken at install start. Linux is unaffected because the
    /// O_TMPFILE+linkat path is already atomic-by-construction.
    fast_path: Arc<AtomicBool>,
}

impl Store {
    /// Open the store at the default location (see [`dirs::store_dir`]).
    pub fn default_location() -> Result<Self, Error> {
        let root = dirs::store_dir().ok_or(Error::NoHome)?;
        let cache_dir = dirs::cache_dir().ok_or(Error::NoHome)?;
        let store = Self {
            root,
            cache_dir,
            fast_path: Arc::new(AtomicBool::new(false)),
        };
        store.migrate_legacy_index_dir();
        Ok(store)
    }

    /// Open the store with an explicit root, keeping the default
    /// cache dir (`$XDG_CACHE_HOME/aube`). Used when a user overrides
    /// `storeDir` via `.npmrc` / `pnpm-workspace.yaml` — only the CAS
    /// moves; the packument and virtual-store caches stay where the
    /// rest of aube expects them.
    pub fn with_root(root: PathBuf) -> Result<Self, Error> {
        let cache_dir = dirs::cache_dir().ok_or(Error::NoHome)?;
        let store = Self {
            root,
            cache_dir,
            fast_path: Arc::new(AtomicBool::new(false)),
        };
        store.migrate_legacy_index_dir();
        Ok(store)
    }

    /// Open the store at a specific path (cache dir derived from store root).
    /// Used by tests that need a fully isolated layout; production code
    /// should prefer `default_location` or `with_root`.
    pub fn at(root: PathBuf) -> Self {
        let cache_dir = root.parent().unwrap_or(&root).join(CACHE_DIR_NAME);
        Self {
            root,
            cache_dir,
            fast_path: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Enable the macOS direct-write fast path for CAS imports. Bypasses
    /// the tempfile + persist_noclobber pattern and writes straight to
    /// the final content-addressed path, saving ~80µs/file on APFS. The
    /// caller MUST hold an exclusive lock against the store for the
    /// duration any thread might invoke `import_bytes`; otherwise a
    /// concurrent installer can observe a partial file and the
    /// `AlreadyExisted` recovery dance can clobber an in-flight write.
    ///
    /// macOS-gated rather than just declared inert on other platforms.
    /// On Linux the `O_TMPFILE+linkat` path has no inline length-check
    /// recovery — that recovery only lives inside the macOS fast-path
    /// branch — so the outer skip in `import_bytes` (also macOS-gated
    /// via `cfg!`) must never see the flag set on Linux. Removing the
    /// method on non-macOS platforms makes that mismatch a build error
    /// rather than a silent acceptance of torn CAS files.
    #[cfg(target_os = "macos")]
    pub fn enable_fast_path(&self) {
        self.fast_path.store(true, Ordering::Release);
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The store-version directory containing `files/` and `index/`.
    ///
    /// For the default layout this is `<storeDir>/v1/` (parent of
    /// `root`, which is the `files/` subdir). Matches the granularity
    /// of `pnpm store path` — a single cache-mount or backup covering
    /// this directory captures both the CAS shards and the cached
    /// package indexes, so they cannot drift apart.
    ///
    /// Falls back to `root` itself when `root` has no parent (only
    /// possible at the filesystem root, which is never a real store).
    pub fn store_v1_dir(&self) -> PathBuf {
        self.root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone())
    }

    /// Directory for cached package indexes. Lives next to `files/`
    /// at `<v1_dir>/index/` so the whole store is one mount/backup
    /// unit. Public so introspection commands (`aube find-hash`,
    /// `aube store status`, `aube store prune`) can walk it directly.
    pub fn index_dir(&self) -> PathBuf {
        self.store_v1_dir().join(INDEX_SUBDIR)
    }

    /// Legacy index location at `$XDG_CACHE_HOME/aube/index/`, where
    /// aube wrote cached package indexes before they were moved next
    /// to the CAS files. Used only by [`migrate_legacy_index_dir`]; new
    /// code should always go through [`index_dir`].
    fn legacy_index_dir(&self) -> PathBuf {
        self.cache_dir.join(INDEX_SUBDIR)
    }

    /// One-shot migration from the legacy XDG-cache index location to
    /// the in-store `v1/index/` directory. Runs at `Store::open`-time.
    ///
    /// The legacy location was a footgun under Docker BuildKit cache
    /// mounts: users would mount the CAS files dir, the indexes would
    /// silently land on the image layer instead, and the next install
    /// would hit `MissingStoreFile` on every package whose CAS shards
    /// the cache mount dropped. Co-locating index with files matches
    /// pnpm's grouping and removes the drift class entirely.
    ///
    /// Best-effort: a same-filesystem rename is one syscall; on
    /// `EXDEV` (cache dir and store dir on different filesystems, e.g.
    /// tmpfs cache + persistent data) we fall back to a recursive
    /// copy + remove. Either failure logs a warning and proceeds —
    /// the worst-case is re-fetching tarballs on the next install,
    /// which is what would have happened without the migration anyway.
    fn migrate_legacy_index_dir(&self) {
        let legacy = self.legacy_index_dir();
        let new = self.index_dir();
        if !legacy.exists() || new.exists() {
            return;
        }
        if let Some(parent) = new.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!(
                "failed to create {} for index migration: {e}",
                parent.display()
            );
            return;
        }
        if std::fs::rename(&legacy, &new).is_ok() {
            debug!(
                "migrated cached indexes from {} to {}",
                legacy.display(),
                new.display()
            );
            return;
        }
        // Rename lost (cross-FS, or a concurrent process already won
        // the race). If `legacy` is gone, a concurrent process already
        // migrated successfully — leave `new` alone.
        if !legacy.exists() {
            return;
        }
        // Cross-filesystem rename (cache dir on tmpfs, data dir on a
        // persistent FS — common in containers) or any other rename
        // failure: fall back to recursive copy + remove.
        if let Err(e) = copy_dir_recursive(&legacy, &new) {
            warn!(
                "failed to migrate cached indexes from {} to {}: {e}; will be rebuilt on next install",
                legacy.display(),
                new.display()
            );
            // Only roll back `new` if `legacy` is still here — meaning
            // we own the half-copied content and have a recovery
            // path (next install re-fetches). If `legacy` is also gone,
            // a concurrent rename succeeded between our two checks and
            // `new` holds that process's valid data; removing it would
            // silently delete the only good copy.
            if legacy.exists() {
                let _ = std::fs::remove_dir_all(&new);
            }
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&legacy) {
            warn!(
                "migrated indexes to {} but failed to remove old {}: {e}",
                new.display(),
                legacy.display()
            );
        }
    }

    /// Directory for the global virtual store (materialized packages).
    pub fn virtual_store_dir(&self) -> PathBuf {
        self.cache_dir.join(VIRTUAL_STORE_SUBDIR)
    }

    /// Root of the per-package *extracted-tree* tier, a sibling of the
    /// CAS `files/` dir at `<v1_dir>/trees/`. Each entry is a real,
    /// fully-materialized package directory (every file reflinked from
    /// the CAS) keyed by the linker's virtual-store subdir name. It
    /// exists solely as a single-`clonefile(2)` clone *source* for the
    /// macOS whole-dir materialization fast path: the kernel clones the
    /// whole tree in one syscall instead of the linker reflinking each
    /// file individually. Lives next to `files/` so one store
    /// mount/backup/prune unit covers the CAS, the cached indexes, and
    /// the trees together. Empty/absent on platforms that never build
    /// the tier (everything but macOS+APFS), where the linker keeps its
    /// per-file path unchanged.
    pub fn trees_dir(&self) -> PathBuf {
        self.store_v1_dir().join(TREES_SUBDIR)
    }

    /// On-disk path of the extracted tree for `tree_key` (the linker's
    /// virtual-store subdir name — already filesystem-safe, with `/`
    /// flattened to `+`, so it's a single path component). The tree
    /// root *is* the package root: files sit at the same relative
    /// layout they'd have under `<entry>/node_modules/<name>/`.
    pub fn tree_path(&self, tree_key: &str) -> PathBuf {
        self.trees_dir().join(tree_key)
    }

    /// Directory for cached packument metadata (abbreviated/corgi format).
    /// Versioned so we can bump the schema without breaking old caches —
    /// old caches at older versions stay around until manually pruned.
    pub fn packument_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(PACKUMENT_CACHE_SUBDIR)
    }

    /// Directory for cached *full* packument JSON (non-corgi) used by
    /// human-facing commands like `aube view` that need fields the resolver
    /// doesn't parse (`description`, `repository`, `license`, `keywords`,
    /// `maintainers`). Separate from `packument_cache_dir` because the
    /// corgi and full responses have different shapes.
    pub fn packument_full_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(PACKUMENT_FULL_CACHE_SUBDIR)
    }

    /// Check if a file with the given integrity hash exists in the store.
    pub fn has(&self, integrity: &str) -> bool {
        self.file_path_from_integrity(integrity)
            .is_some_and(|p| p.exists())
    }

    /// Get the path to a file in the store by its integrity hash.
    pub fn file_path_from_integrity(&self, integrity: &str) -> Option<PathBuf> {
        let hex_hash = integrity_to_hex(integrity)?;
        Some(self.file_path_from_hex(&hex_hash))
    }

    /// Get the path to a file in the store by its hex hash.
    pub fn file_path_from_hex(&self, hex_hash: &str) -> PathBuf {
        let (shard, rest) = hex_hash.split_at(2);
        self.root.join(shard).join(rest)
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum Error {
    #[error("HOME environment variable not set")]
    #[diagnostic(code(ERR_AUBE_NO_HOME))]
    NoHome,
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("file error: {0}")]
    Xx(String),
    #[error("tarball extraction error: {0}")]
    #[diagnostic(code(ERR_AUBE_TARBALL_EXTRACT))]
    Tar(String),
    #[error("integrity verification failed: {0}")]
    #[diagnostic(code(ERR_AUBE_TARBALL_INTEGRITY))]
    Integrity(String),
    #[error("package.json content mismatch: tarball declares {actual}")]
    #[diagnostic(code(ERR_AUBE_PKG_CONTENT_MISMATCH))]
    PkgContentMismatch { actual: String },
    #[error("git error: {0}")]
    #[diagnostic(code(ERR_AUBE_GIT_ERROR))]
    Git(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::read_codeload_integrity;

    /// Construct a Store with explicit root + cache_dir, bypassing the
    /// XDG resolution path. Test-only so the migration test can drive
    /// `migrate_legacy_index_dir` against a fully isolated layout
    /// without touching env vars or process-global state.
    fn store_for_migration_test(root: PathBuf, cache_dir: PathBuf) -> Store {
        Store {
            root,
            cache_dir,
            fast_path: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn migrate_legacy_index_dir_relocates_files_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data/aube/store/v1/files");
        let cache_dir = tmp.path().join("cache/aube");
        std::fs::create_dir_all(&root).unwrap();
        let legacy_index = cache_dir.join("index");
        let legacy_shard = legacy_index.join("0123456789abcdef");
        std::fs::create_dir_all(&legacy_shard).unwrap();
        std::fs::write(legacy_index.join("foo@1.0.0.json"), b"{\"index\":\"a\"}").unwrap();
        std::fs::write(legacy_shard.join("bar@2.0.0.json"), b"{\"index\":\"b\"}").unwrap();

        let store = store_for_migration_test(root.clone(), cache_dir.clone());
        store.migrate_legacy_index_dir();

        let new_index = store.index_dir();
        assert!(new_index.exists(), "new index dir must exist");
        assert_eq!(
            std::fs::read(new_index.join("foo@1.0.0.json")).unwrap(),
            b"{\"index\":\"a\"}",
            "integrity-less entry must migrate"
        );
        assert_eq!(
            std::fs::read(new_index.join("0123456789abcdef/bar@2.0.0.json")).unwrap(),
            b"{\"index\":\"b\"}",
            "integrity-keyed shard subdir must migrate"
        );
        assert!(
            !legacy_index.exists(),
            "legacy index dir must be removed after a successful migration"
        );
    }

    #[test]
    fn migrate_legacy_index_dir_is_a_noop_when_new_dir_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data/aube/store/v1/files");
        let cache_dir = tmp.path().join("cache/aube");
        std::fs::create_dir_all(&root).unwrap();
        let legacy_index = cache_dir.join("index");
        std::fs::create_dir_all(&legacy_index).unwrap();
        std::fs::write(legacy_index.join("foo@1.0.0.json"), b"old").unwrap();

        let store = store_for_migration_test(root.clone(), cache_dir.clone());
        // Pre-existing new-location entry must not be clobbered.
        std::fs::create_dir_all(store.index_dir()).unwrap();
        std::fs::write(store.index_dir().join("keep.json"), b"new").unwrap();

        store.migrate_legacy_index_dir();

        assert!(
            legacy_index.exists(),
            "legacy dir must stay untouched when new dir already exists"
        );
        assert_eq!(
            std::fs::read(store.index_dir().join("keep.json")).unwrap(),
            b"new",
            "existing new-location content must not be overwritten"
        );
        assert!(
            !store.index_dir().join("foo@1.0.0.json").exists(),
            "no copy must happen — migration only runs when new dir is absent"
        );
    }

    #[test]
    fn migrate_legacy_index_dir_is_a_noop_when_legacy_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data/aube/store/v1/files");
        let cache_dir = tmp.path().join("cache/aube");
        std::fs::create_dir_all(&root).unwrap();

        let store = store_for_migration_test(root, cache_dir);
        store.migrate_legacy_index_dir();

        assert!(
            !store.index_dir().exists(),
            "migration must not create an empty new dir when there's nothing to migrate"
        );
    }

    #[test]
    fn store_v1_dir_is_parent_of_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data/aube/store/v1/files");
        let cache_dir = tmp.path().join("cache/aube");
        let store = store_for_migration_test(root.clone(), cache_dir);
        assert_eq!(store.store_v1_dir(), root.parent().unwrap());
        assert_eq!(store.index_dir(), root.parent().unwrap().join("index"));
    }

    #[test]
    fn git_commit_matches_abbreviated_sha() {
        assert!(git_commit_matches(
            "98e8ff1da1a89f93d1397a24d7413ed15421c139",
            "98e8ff1"
        ));
        assert!(!git_commit_matches(
            "98e8ff1da1a89f93d1397a24d7413ed15421c139",
            "98e8ff2"
        ));
        assert!(!git_commit_matches(
            "98e8ff1da1a89f93d1397a24d7413ed15421c139",
            "main"
        ));
    }

    #[test]
    fn test_integrity_to_hex() {
        let integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let result = integrity_to_hex(integrity);
        assert!(result.is_some());
        let hex = result.unwrap();
        assert_eq!(hex.len(), 128);
        assert!(hex.chars().all(|c| c == '0'));
    }

    #[test]
    fn test_integrity_to_hex_invalid() {
        assert!(integrity_to_hex("md5-abc").is_none());
        assert!(integrity_to_hex("notahash").is_none());
        assert!(integrity_to_hex("").is_none());
    }

    #[test]
    fn test_integrity_to_hex_sha1() {
        // `co@4.6.0`'s real registry integrity.
        let hex = integrity_to_hex("sha1-bqa989hTrlTMuOR7+gvz+QMfsYQ=").unwrap();
        assert_eq!(hex.len(), 40);
        assert_eq!(hex, "6ea6bdf3d853ae54ccb8e47bfa0bf3f9031fb184");
    }

    #[test]
    fn test_integrity_to_hex_sha256() {
        let hex = integrity_to_hex("sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=").unwrap();
        assert_eq!(hex.len(), 64);
    }

    #[test]
    fn test_file_path_from_hex_sharding() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let path = store.file_path_from_hex("abcdef1234567890");
        // First 2 chars are the shard directory. Use the platform's
        // separator so the test works on Windows as well as Unix.
        let sep = std::path::MAIN_SEPARATOR;
        assert!(path.to_string_lossy().contains(&format!("{sep}ab{sep}")));
        assert!(path.to_string_lossy().ends_with("cdef1234567890"));
    }

    #[test]
    fn test_import_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let content = b"hello world";
        let stored = store.import_bytes(content, false).unwrap();

        assert!(stored.store_path.exists());
        assert_eq!(std::fs::read(&stored.store_path).unwrap(), content);
        assert!(!stored.executable);

        // Importing same content returns same hash (idempotent)
        let stored2 = store.import_bytes(content, false).unwrap();
        assert_eq!(stored.hex_hash, stored2.hex_hash);
    }

    #[test]
    fn test_import_bytes_repairs_truncated_existing_cas_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        let content = br#"{"name":"@babel/helper-string-parser","version":"7.27.1"}"#;
        let hex_hash = blake3_hex(content);
        let store_path = store.file_path_from_hex(&hex_hash);
        std::fs::write(&store_path, b"").unwrap();

        let stored = store.import_bytes(content, false).unwrap();

        assert_eq!(stored.hex_hash, hex_hash);
        assert_eq!(stored.size, Some(content.len() as u64));
        assert_eq!(std::fs::read(&stored.store_path).unwrap(), content);
    }

    #[test]
    fn verify_precomputed_sha512_happy_path() {
        let data = b"hello world";
        let mut hasher = Sha512::new();
        hasher.update(data);
        let mut digest = [0u8; 64];
        digest.copy_from_slice(&hasher.finalize()[..]);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
        let integrity = format!("sha512-{b64}");
        assert!(verify_precomputed_sha512(&digest, &integrity).unwrap());
    }

    #[test]
    fn verify_precomputed_sha512_mismatch_errors() {
        // Build a properly-shaped sha512 SRI for all-FF bytes, then
        // verify against an all-zero digest — same length, different
        // content, lands on the byte-compare mismatch arm.
        use base64::Engine;
        let other = [0xFFu8; 64];
        let other_b64 = base64::engine::general_purpose::STANDARD.encode(other);
        let wrong = format!("sha512-{other_b64}");
        let digest = [0u8; 64];
        let err = verify_precomputed_sha512(&digest, &wrong).unwrap_err();
        assert!(err.to_string().contains("integrity mismatch"));
    }

    #[test]
    fn verify_precomputed_sha512_corrupt_b64_errors_distinctly() {
        // Non-base64 characters: decode fails, user gets "malformed
        // base64" instead of the misleading "integrity mismatch" they
        // would see if every failure collapsed into one bucket.
        let digest = [0u8; 64];
        let corrupt = "sha512-not_valid_base64_!!!!!";
        let err = verify_precomputed_sha512(&digest, corrupt).unwrap_err();
        assert!(err.to_string().contains("malformed base64"));
    }

    #[test]
    fn verify_precomputed_sha512_short_b64_errors_distinctly() {
        // Valid base64 but decodes to too few bytes for sha512.
        // Reports actual decoded length rather than mismatch.
        let digest = [0u8; 64];
        let short = "sha512-AAAA";
        let err = verify_precomputed_sha512(&digest, short).unwrap_err();
        assert!(err.to_string().contains("expected 64 for sha512"));
    }

    #[test]
    fn verify_precomputed_sha512_non_sha512_returns_false() {
        // Caller is expected to fall through to the buffered path
        // with the right algo. Function returns Ok(false) so the
        // caller can detect this without matching on a sentinel
        // error variant.
        let digest = [0u8; 64];
        for algo in ["sha1-AAAA", "sha256-AAAA", "sha384-AAAA"] {
            assert!(
                !verify_precomputed_sha512(&digest, algo).unwrap(),
                "{algo} should return Ok(false) for fallback"
            );
        }
    }

    #[test]
    fn verify_precomputed_sha512_malformed_errors() {
        let digest = [0u8; 64];
        for bad in ["", "garbage", "not-an-algo-tag", "sha512", "sha512-"] {
            let result = verify_precomputed_sha512(&digest, bad);
            assert!(result.is_err(), "{bad:?} should not be Ok");
        }
    }

    #[test]
    fn test_verify_integrity_valid() {
        let data = b"hello world";
        // Compute the actual sha512 of "hello world"
        let mut hasher = Sha512::new();
        hasher.update(data);
        let hash = hasher.finalize();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        let integrity = format!("sha512-{b64}");

        assert!(verify_integrity(data, &integrity).is_ok());
    }

    #[test]
    fn test_verify_integrity_mismatch() {
        let data = b"hello world";
        let wrong = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let result = verify_integrity(data, wrong);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("integrity mismatch")
        );
    }

    #[test]
    fn test_verify_integrity_unsupported_format() {
        let result = verify_integrity(b"test", "md5-abc123");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported"));
    }

    #[test]
    fn test_verify_integrity_sha1_valid() {
        // SRI sha1- tarballs exist for legacy packages like co@4.6.0;
        // aube must still install them, so this is a regression guard.
        let data = b"hello world";
        let hash = Sha1::digest(data);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        assert!(verify_integrity(data, &format!("sha1-{b64}")).is_ok());
    }

    #[test]
    fn test_verify_integrity_sha1_mismatch() {
        let result = verify_integrity(b"hello world", "sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("integrity mismatch"));
        assert!(err.contains("sha1-"));
    }

    #[test]
    fn test_verify_integrity_sha256_valid() {
        let data = b"hello world";
        let hash = Sha256::digest(data);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        assert!(verify_integrity(data, &format!("sha256-{b64}")).is_ok());
    }

    #[test]
    fn test_verify_integrity_sha384_valid() {
        let data = b"hello world";
        let hash = Sha384::digest(data);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        assert!(verify_integrity(data, &format!("sha384-{b64}")).is_ok());
    }

    #[test]
    fn test_import_bytes_executable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let content = b"#!/bin/sh\necho hello";
        let stored = store.import_bytes(content, true).unwrap();
        assert!(stored.executable);

        // Check exec marker file exists
        let exec_marker = PathBuf::from(format!("{}-exec", stored.store_path.display()));
        assert!(exec_marker.exists());
    }

    #[test]
    fn test_import_bytes_different_content_different_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored1 = store.import_bytes(b"content a", false).unwrap();
        let stored2 = store.import_bytes(b"content b", false).unwrap();
        assert_ne!(stored1.hex_hash, stored2.hex_hash);
    }

    /// SHA-512 of an arbitrary test payload, encoded as npm's
    /// `sha512-<base64>`. Shared across index-cache tests so every
    /// save/load pair uses the same integrity and the filename is
    /// deterministic.
    const TEST_INTEGRITY: &str = "sha512-7iaw3Ur350mqGo7jwQrpkj9hiYB3Lkc/iBml1JQODbJ6wYX4oOHV+E+IvIh/1ntDcowEzF+prYseb2BRlkqKKw==";
    const OTHER_INTEGRITY: &str = "sha512-n4udRxsOEWaTbNrUjcrNvWAd1/aLvZeC/CwfsBIJZj0kHqyh0h10DmZerKIyp+/YqR09J8rBmdqkIy9SE/6rcQ==";

    #[test]
    fn test_index_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let content = b"test file";
        let stored = store.import_bytes(content, false).unwrap();

        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);

        store
            .save_index("test-pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();

        let loaded = store.load_index("test-pkg", "1.0.0", Some(TEST_INTEGRITY));
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("index.js"));
    }

    #[test]
    fn test_index_cache_scoped_package() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"scoped content", false).unwrap();
        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);

        // Scoped package name should work (slash replaced with __)
        store
            .save_index("@scope/pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        let loaded = store.load_index("@scope/pkg", "1.0.0", Some(TEST_INTEGRITY));
        assert!(loaded.is_some());
    }

    #[test]
    fn test_index_cache_stale_detection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let store_path = stored.store_path.clone();
        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);

        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();

        // Delete the actual store file to simulate staleness
        std::fs::remove_file(&store_path).unwrap();

        // Both variants detect missing store files and return None.
        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
        // save_index wrote the file and load_index just deleted it
        // after detecting the stale store entry, so re-seed before
        // exercising the verified variant.
        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        assert!(
            store
                .load_index_verified("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[test]
    fn load_index_passes_partial_corruption_load_index_verified_catches_it() {
        // The user's BuildKit failure mode: cached index references
        // multiple files; the iterated-first file's CAS shard
        // happens to still exist (or never did — `dist.size` is absent
        // on legacy indexes so the probe defaults to `exists()`), but a
        // later file's shard is gone. The fast `load_index` returns
        // Some(stale_index), which then dies inside the linker with
        // `ERR_AUBE_MISSING_STORE_FILE`. `load_index_verified` stats
        // every file and drops the index so the fetch path re-imports.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        // FxMap iteration is hash-based, so pinning "BBB.txt" as the
        // later-iterated entry the way the BTreeMap test did doesn't
        // hold. Instead, build the index, round-trip it through
        // save+load, and read *that* iteration order — `FxMap`'s
        // FixedState seed is stable, but the incremental-insert map's
        // bucket count can differ from a freshly-deserialized map's,
        // and the cheap probe runs on the deserialized path. Corrupt
        // a non-first-iterated file so both halves of the invariant —
        // cheap probe accepts, verified probe rejects — are
        // deterministic.
        let mut index = PackageIndex::default();
        for i in 0..8 {
            let stored = store
                .import_bytes(format!("content-{i}").as_bytes(), false)
                .unwrap();
            index.insert(format!("file-{i:02}.txt"), stored);
        }
        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        let loaded = store
            .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
            .expect("freshly saved index must load before any corruption");
        let first_path = loaded.values().next().unwrap().store_path.clone();
        let dropped_path = loaded
            .values()
            .find(|f| f.store_path != first_path)
            .unwrap()
            .store_path
            .clone();

        // Remove a non-first file's CAS shard.
        std::fs::remove_file(&dropped_path).unwrap();

        // Cheap probe samples only the iterated-first file (still
        // healthy) and accepts the index — the bug class that motivated
        // the fix.
        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_some(),
            "cheap probe must accept partial corruption (precondition for the fix)"
        );
        // Re-save defensively in case the cheap probe path ever drops
        // the index file on a future tuning. The verified-probe
        // assertion below is the real invariant.
        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();

        // Verified probe walks every file and rejects the stale index.
        assert!(
            store
                .load_index_verified("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none(),
            "verified probe must reject an index whose later files are missing"
        );

        // Side effect: load_index_verified drops the JSON so the next
        // fetch re-imports rather than racing on the same dead reference.
        let path = store.index_path("pkg", "1.0.0", Some(TEST_INTEGRITY));
        assert!(
            !path.unwrap().exists(),
            "verified probe must drop the stale cached index"
        );
    }

    #[test]
    fn test_invalidate_cached_index_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);

        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        // First call removes the entry; second call sees it gone.
        assert!(
            store
                .invalidate_cached_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .unwrap()
        );
        assert!(
            !store
                .invalidate_cached_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .unwrap()
        );
        // load_index now misses, forcing a re-import on the next install.
        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[test]
    fn test_invalidate_cached_index_returns_false_for_invalid_coordinate() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        // Empty name doesn't yield a valid index path; must not error.
        assert!(
            !store
                .invalidate_cached_index("", "1.0.0", Some(TEST_INTEGRITY))
                .unwrap()
        );
    }

    #[test]
    fn test_index_cache_rejects_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let store_path = stored.store_path.clone();
        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);

        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        std::fs::write(&store_path, b"").unwrap();

        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_import_bytes_uses_world_readable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let mode = std::fs::metadata(&stored.store_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o644);
    }

    #[test]
    fn test_index_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        assert!(
            store
                .load_index("nonexistent", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[test]
    fn test_index_cache_integrity_discriminates_sources() {
        // Regression: before this, two tarballs served under the same
        // `(name, version)` from different sources — e.g. a github
        // codeload archive and the npm-published bytes — would share
        // the `<name>@<version>.json` cache file and return each
        // other's file list to the linker.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let registry_bytes = store.import_bytes(b"registry tarball", false).unwrap();
        let mut registry_index = PackageIndex::default();
        registry_index.insert("package.json".to_string(), registry_bytes);

        let github_bytes = store.import_bytes(b"github tarball", false).unwrap();
        let mut github_index = PackageIndex::default();
        github_index.insert("package.json".to_string(), github_bytes);
        github_index.insert("extra-github-only.js".to_string(), {
            store.import_bytes(b"extra", false).unwrap()
        });

        store
            .save_index("node-expat", "2.4.1", Some(TEST_INTEGRITY), &registry_index)
            .unwrap();
        store
            .save_index("node-expat", "2.4.1", Some(OTHER_INTEGRITY), &github_index)
            .unwrap();

        // Each integrity returns its own distinct index.
        let registry = store
            .load_index("node-expat", "2.4.1", Some(TEST_INTEGRITY))
            .unwrap();
        let github = store
            .load_index("node-expat", "2.4.1", Some(OTHER_INTEGRITY))
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(github.len(), 2);
        assert!(github.contains_key("extra-github-only.js"));
    }

    #[test]
    fn test_index_cache_rejects_malformed_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);

        // Not a `sha512-<base64>` string — save returns an error and
        // load returns None rather than falling back to a weaker key.
        assert!(
            store
                .save_index("pkg", "1.0.0", Some("not-an-integrity"), &index)
                .is_err()
        );
        assert!(
            store
                .load_index("pkg", "1.0.0", Some("not-an-integrity"))
                .is_none()
        );
    }

    #[test]
    fn test_index_cache_integrity_none_roundtrip() {
        // Registry proxies that strip `dist.integrity` still need to
        // warm-install. With `integrity = None` the cache falls back
        // to `<name>@<version>.json` (no suffix), matching the
        // pre-integrity-keyed behavior for exactly that narrow case.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"no-integrity content", false).unwrap();
        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);

        store.save_index("pkg", "1.0.0", None, &index).unwrap();
        let loaded = store.load_index("pkg", "1.0.0", None);
        assert!(loaded.is_some());
        assert!(loaded.unwrap().contains_key("index.js"));

        // The integrity-keyed key does *not* see the integrity-less
        // entry — different directory on disk.
        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[test]
    fn test_index_cache_build_metadata_does_not_collide_with_integrity() {
        // Regression: an earlier flat-filename scheme
        // (`<name>@<version>+<16 hex>.json`) could in theory collide
        // with an integrity-less entry for a version whose semver
        // build metadata was exactly 16 lowercase hex chars
        // (`1.0.0+a1b2c3d4e5f6a7b8`). The subdir layout forecloses
        // that: integrity lives in a directory, not in the filename.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let a = store.import_bytes(b"integrity-keyed bytes", false).unwrap();
        let mut integrity_keyed = PackageIndex::default();
        integrity_keyed.insert("integrity-keyed.js".to_string(), a);

        let b = store.import_bytes(b"build-metadata bytes", false).unwrap();
        let mut build_meta = PackageIndex::default();
        build_meta.insert("build-meta.js".to_string(), b);

        // Integrity whose first 16 hex == the version's build metadata.
        // TEST_INTEGRITY hex-decodes to `ee26b0dd4af7e749...`, so the
        // directory name for the integrity-keyed entry is
        // `ee26b0dd4af7e749`. A version with that exact 16-hex build
        // metadata under the plain key must not alias it.
        let colliding_version = "1.0.0+ee26b0dd4af7e749";
        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &integrity_keyed)
            .unwrap();
        store
            .save_index("pkg", colliding_version, None, &build_meta)
            .unwrap();

        let by_integrity = store
            .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
            .unwrap();
        let by_build_meta = store.load_index("pkg", colliding_version, None).unwrap();
        assert!(by_integrity.contains_key("integrity-keyed.js"));
        assert!(by_build_meta.contains_key("build-meta.js"));
        // And neither entry leaks into the other's file list.
        assert!(!by_integrity.contains_key("build-meta.js"));
        assert!(!by_build_meta.contains_key("integrity-keyed.js"));
    }

    fn index_with_manifest(store: &Store, name: &str, version: &str) -> PackageIndex {
        let manifest =
            serde_json::json!({"name": name, "version": version, "main": "index.js"}).to_string();
        let stored = store.import_bytes(manifest.as_bytes(), false).unwrap();
        let mut index = PackageIndex::default();
        index.insert("package.json".to_string(), stored);
        index
    }

    #[test]
    fn test_validate_pkg_content_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "lodash", "4.17.21");
        assert!(validate_pkg_content(&index, "lodash", "4.17.21").is_ok());
    }

    #[test]
    fn test_validate_pkg_content_name_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "evil-pkg", "1.0.0");
        let err = validate_pkg_content(&index, "lodash", "1.0.0").unwrap_err();
        let msg = err.to_string();
        // The variant only carries the *actual* coordinate; the
        // caller's `{name}@{version}: ` prefix supplies the expected
        // half. See the comment on `Error::PkgContentMismatch`.
        assert!(msg.contains("content mismatch"), "{msg}");
        assert!(msg.contains("declares evil-pkg@1.0.0"), "{msg}");
    }

    #[test]
    fn test_validate_pkg_content_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "lodash", "9.9.9");
        let err = validate_pkg_content(&index, "lodash", "4.17.21").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("content mismatch"), "{msg}");
        assert!(msg.contains("declares lodash@9.9.9"), "{msg}");
    }

    #[test]
    fn test_validate_pkg_content_tolerates_leading_v() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "@upstash/ratelimit", "v2.0.8");
        assert!(validate_pkg_content(&index, "@upstash/ratelimit", "2.0.8").is_ok());
    }

    #[test]
    fn test_validate_pkg_content_skips_version_for_url_shaped_expected() {
        // pnpm v9 lockfiles key github-hosted deps by the codeload
        // tarball URL in the version slot; the tarball's real semver
        // will never match it. Skip the version comparison for
        // non-semver expected values, but still enforce the name.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "datejs", "1.0.0-rc3");
        let url = "https://codeload.github.com/abritinthebay/datejs/tar.gz/3675d46ed96d57e30aeddf9b1d1026ac81d37ae3";
        assert!(validate_pkg_content(&index, "datejs", url).is_ok());
        // Name mismatch still rejects.
        let err = validate_pkg_content(&index, "evil", url).unwrap_err();
        assert!(err.to_string().contains("content mismatch"), "{err}");
    }

    #[test]
    fn test_validate_pkg_content_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let stored = store.import_bytes(b"module.exports = 1;", false).unwrap();
        let mut index = PackageIndex::default();
        index.insert("index.js".to_string(), stored);
        let err = validate_pkg_content(&index, "lodash", "4.17.21").unwrap_err();
        assert!(err.to_string().contains("package.json missing"), "{err}",);
    }

    #[test]
    fn test_validate_pkg_content_unparseable_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let stored = store.import_bytes(b"{not json", false).unwrap();
        let mut index = PackageIndex::default();
        index.insert("package.json".to_string(), stored);
        let err = validate_pkg_content(&index, "lodash", "4.17.21").unwrap_err();
        assert!(err.to_string().contains("invalid package.json"), "{err}");
    }

    #[test]
    fn test_import_tarball() {
        // Create a minimal .tar.gz in memory
        let mut builder = tar::Builder::new(Vec::new());

        let content = b"module.exports = 42;\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "package/index.js", &content[..])
            .unwrap();

        let bin_content = b"#!/usr/bin/env node\nconsole.log('hi');\n";
        let mut bin_header = tar::Header::new_gnu();
        bin_header.set_size(bin_content.len() as u64);
        bin_header.set_mode(0o755);
        bin_header.set_cksum();
        builder
            .append_data(&mut bin_header, "package/bin/cli.js", &bin_content[..])
            .unwrap();

        let tar_bytes = builder.into_inner().unwrap();

        // Gzip it
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        let tgz_bytes = encoder.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let index = store.import_tarball(&tgz_bytes).unwrap();
        assert_eq!(index.len(), 2);
        assert!(index.contains_key("index.js"));
        assert!(index.contains_key("bin/cli.js"));

        // Verify file contents
        let idx_stored = &index["index.js"];
        assert!(!idx_stored.executable);
        assert_eq!(std::fs::read(&idx_stored.store_path).unwrap(), content);

        let bin_stored = &index["bin/cli.js"];
        assert!(bin_stored.executable);
        assert_eq!(std::fs::read(&bin_stored.store_path).unwrap(), bin_content);
    }

    #[test]
    fn test_git_url_host_https() {
        assert_eq!(
            git_url_host("https://github.com/user/repo.git"),
            Some("github.com")
        );
        assert_eq!(
            git_url_host("git+https://github.com/user/repo.git#main"),
            Some("github.com")
        );
        assert_eq!(
            git_url_host("git://git.example.com/repo.git"),
            Some("git.example.com")
        );
    }

    #[test]
    fn test_git_url_host_ssh() {
        assert_eq!(
            git_url_host("git+ssh://git@github.com/user/repo.git"),
            Some("github.com")
        );
        assert_eq!(
            git_url_host("ssh://git@gitlab.com:2222/user/repo.git"),
            Some("gitlab.com")
        );
        // scp-style URL (no scheme): git@host:path
        assert_eq!(
            git_url_host("git@github.com:user/repo.git"),
            Some("github.com")
        );
    }

    #[test]
    fn test_git_url_host_ipv6() {
        // IPv6 literals must keep their colons — the port-strip pass
        // has to unwrap the brackets before it even considers `:`.
        assert_eq!(git_url_host("https://[::1]/repo.git"), Some("::1"));
        assert_eq!(git_url_host("https://[::1]:8443/repo.git"), Some("::1"));
        assert_eq!(
            git_url_host("ssh://git@[2001:db8::1]:2222/user/repo.git"),
            Some("2001:db8::1")
        );
    }

    #[test]
    fn test_git_url_host_rejects_garbage() {
        assert_eq!(git_url_host(""), None);
        assert_eq!(git_url_host("not a url"), None);
        assert_eq!(git_url_host("/just/a/path"), None);
    }

    #[test]
    fn test_git_host_in_list_exact_match() {
        let hosts = vec![
            "github.com".to_string(),
            "gitlab.com".to_string(),
            "bitbucket.org".to_string(),
        ];
        assert!(git_host_in_list("https://github.com/user/repo.git", &hosts));
        assert!(git_host_in_list(
            "git+ssh://git@gitlab.com/user/repo.git",
            &hosts
        ));
        // Exact match — no subdomain folding, matching pnpm semantics.
        assert!(!git_host_in_list(
            "https://api.github.com/user/repo.git",
            &hosts
        ));
        assert!(!git_host_in_list(
            "https://self-hosted.example/user/repo.git",
            &hosts
        ));
    }

    #[test]
    fn test_git_host_in_list_empty_list() {
        let hosts: Vec<String> = vec![];
        assert!(!git_host_in_list(
            "https://github.com/user/repo.git",
            &hosts
        ));
    }

    #[test]
    fn test_validate_git_positional_accepts_normal_values() {
        validate_git_positional("https://github.com/u/r.git", "git url").unwrap();
        validate_git_positional("git@github.com:u/r.git", "git url").unwrap();
        validate_git_positional("main", "git commit").unwrap();
        validate_git_positional("0123456789abcdef0123456789abcdef01234567", "git commit").unwrap();
    }

    #[test]
    fn test_validate_git_positional_rejects_dash_prefix() {
        // CVE-2017-1000117 class: git treats a leading `-` as an
        // option. `--upload-pack=...` is the classic payload.
        let err = validate_git_positional("--upload-pack=/tmp/evil", "git url").unwrap_err();
        assert!(matches!(err, Error::Git(_)));
        let err = validate_git_positional("-oX", "git commit").unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_validate_git_positional_rejects_nul() {
        let err = validate_git_positional("normal\0tail", "git url").unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_git_resolve_ref_rejects_dash_prefixed_url() {
        // Must refuse before ever spawning `git ls-remote`. Confirms
        // the validation runs at the public entry point.
        let err = git_resolve_ref("--upload-pack=/tmp/evil", None).unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_git_resolve_ref_full_sha_is_offline() {
        // 40-char hex committish short-circuits `ls-remote`. Confirm
        // by handing a non-existent URL — if the fast path regressed
        // into a network call, the test would fail to spawn git.
        let sha = "0123456789ABCDEF0123456789abcdef01234567";
        let resolved = git_resolve_ref("https://example.invalid/missing.git", Some(sha)).unwrap();
        assert_eq!(resolved, "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn test_git_commit_matches_prefix() {
        let full = "0b6ea539609031977983f0b2393ebe81ee28c8ec";
        assert!(git_commit_matches(full, full));
        assert!(git_commit_matches(full, "0b6ea53"));
        assert!(!git_commit_matches(full, "0b6ea5"));
        assert!(!git_commit_matches(full, "abc1234"));
        assert!(!git_commit_matches(full, "main"));
    }

    /// Build a minimal codeload-style `.tar.gz`: a wrapper directory
    /// `<wrapper>/` followed by a few file entries inside it. Mirrors
    /// the layout `https://codeload.github.com/<owner>/<repo>/tar.gz/<sha>`
    /// produces in the wild.
    fn build_codeload_tarball(wrapper: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut dh = tar::Header::new_gnu();
        dh.set_path(format!("{wrapper}/")).unwrap();
        dh.set_size(0);
        dh.set_mode(0o755);
        dh.set_entry_type(tar::EntryType::Directory);
        dh.set_cksum();
        ar.append(&dh, std::io::empty()).unwrap();
        for (path, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("{wrapper}/{path}")).unwrap();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append(&h, *content).unwrap();
        }
        let gz = ar.into_inner().unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn extract_codeload_tarball_strips_wrapper_and_caches() {
        // Each test gets its own private cache root via `tempfile`,
        // so the three new codeload tests don't race on a process-wide
        // `XDG_CACHE_HOME` mutation under `cargo test`'s default
        // parallel scheduling. Windows surfaces the race as a
        // PermissionDenied on a sibling test's already-dropped temp
        // dir; Linux happens to schedule us out of it.
        let tmp = tempfile::tempdir().unwrap();
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let wrapper = format!("owner-repo-{}", &sha[..7]);
        let bytes = build_codeload_tarball(
            &wrapper,
            &[
                ("package.json", br#"{"name":"x","version":"0.0.1"}"#),
                ("src/index.js", b"module.exports = 1;\n"),
            ],
        );
        let url = "https://github.com/owner/repo.git";
        let (target, head) =
            extract_codeload_tarball_at(tmp.path(), &bytes, url, sha, None).unwrap();
        assert_eq!(head, sha);
        // Wrapper component is stripped — `package.json` lives at the
        // target root, not under `target/<wrapper>/package.json`.
        assert!(target.join("package.json").is_file());
        assert!(target.join("src/index.js").is_file());
        assert!(!target.join(&wrapper).exists());

        // Second call with the same (url, commit) reuses the cached
        // directory rather than re-extracting.
        let (target2, _) = extract_codeload_tarball_at(tmp.path(), &bytes, url, sha, None).unwrap();
        assert_eq!(target, target2);
        assert!(super::git::codeload_integrity_path(&target).is_file());
        assert!(
            super::git::read_codeload_integrity(&target)
                .as_deref()
                .is_some_and(|s| s.starts_with("sha512-"))
        );
    }

    #[test]
    fn codeload_cache_lookup_returns_target_only_after_extract() {
        // Lookup must only report `Some` once the cache directory
        // exists, so callers can use it to skip the HTTPS round-trip
        // on resolver→installer reuse without falsely short-circuiting
        // before the bytes have ever been fetched.
        let tmp = tempfile::tempdir().unwrap();
        let sha = "fedcba9876543210fedcba9876543210fedcba98";
        let wrapper = format!("owner-repo-{}", &sha[..7]);
        let url = "https://github.com/owner/repo.git";
        let bytes = build_codeload_tarball(
            &wrapper,
            &[("package.json", br#"{"name":"x","version":"0.0.1"}"#)],
        );

        // Pre-extract miss.
        let (expected_target, expected_sha) =
            codeload_cache_paths(tmp.path(), url, sha, None).unwrap();
        assert!(!expected_target.exists());
        // The public lookup uses the real `dirs::cache_dir()` so the
        // test path can't drive it directly. Instead, drive the inner
        // helper through the cache_paths function and verify the
        // exists check parallels the `is_dir` filter `codeload_cache_lookup`
        // applies. After extract, the lookup-equivalent must succeed.
        let (target, head) =
            extract_codeload_tarball_at(tmp.path(), &bytes, url, sha, None).unwrap();
        assert_eq!(target, expected_target);
        assert_eq!(head, expected_sha);
        assert!(target.is_dir(), "extract must populate the cache target");
        // A second `extract_codeload_tarball_at` (the equivalent of a
        // post-resolver install-time call) reuses the same dir without
        // re-extracting — same cache path comes back.
        let (target2, _) = extract_codeload_tarball_at(tmp.path(), &bytes, url, sha, None).unwrap();
        assert_eq!(target, target2);
    }

    #[test]
    fn codeload_integrity_sidecar_uses_integrity_keyed_cache_path() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let wrapper = format!("owner-repo-{}", &sha[..7]);
        let bytes = build_codeload_tarball(
            &wrapper,
            &[("package.json", br#"{"name":"x","version":"0.0.1"}"#)],
        );
        let url = "https://github.com/owner/repo.git";
        let expected_integrity = "sha512-expected";

        let (target, _) =
            extract_codeload_tarball_at(tmp.path(), &bytes, url, sha, Some(expected_integrity))
                .unwrap();
        let (keyed_target, _) =
            codeload_cache_paths(tmp.path(), url, sha, Some(expected_integrity)).unwrap();
        let (unkeyed_target, _) = codeload_cache_paths(tmp.path(), url, sha, None).unwrap();

        assert_eq!(target, keyed_target);
        assert_ne!(target, unkeyed_target);
        assert!(read_codeload_integrity(&target).is_some());
        assert!(read_codeload_integrity(&unkeyed_target).is_none());
    }

    #[test]
    fn extract_codeload_tarball_backfills_missing_integrity_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let wrapper = format!("owner-repo-{}", &sha[..7]);
        let bytes = build_codeload_tarball(
            &wrapper,
            &[("package.json", br#"{"name":"x","version":"0.0.1"}"#)],
        );
        let url = "https://github.com/owner/repo.git";
        let (target, _) = extract_codeload_tarball_at(tmp.path(), &bytes, url, sha, None).unwrap();
        let sidecar = super::git::codeload_integrity_path(&target);
        std::fs::remove_file(&sidecar).unwrap();

        let (target2, _) = extract_codeload_tarball_at(tmp.path(), &bytes, url, sha, None).unwrap();
        assert_eq!(target, target2);
        assert!(
            super::git::read_codeload_integrity(&target2)
                .as_deref()
                .is_some_and(|s| s.starts_with("sha512-"))
        );
    }

    #[test]
    fn codeload_cache_paths_rejects_invalid_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        // Abbreviated SHA — codeload extracts can't be verified for
        // non-full-SHA committishes, so the cache key would be ambiguous.
        assert!(
            codeload_cache_paths(tmp.path(), "https://example.com/r.git", "abc1234", None)
                .is_none()
        );
        // Dash-prefixed URL — `validate_git_positional` rejects.
        assert!(
            codeload_cache_paths(
                tmp.path(),
                "--upload-pack=/tmp/evil",
                "abcdef0123456789abcdef0123456789abcdef01",
                None,
            )
            .is_none()
        );
        // Branch name — not a SHA.
        assert!(
            codeload_cache_paths(tmp.path(), "https://example.com/r.git", "main", None).is_none()
        );
    }

    #[test]
    fn codeload_cache_paths_include_integrity_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let url = "https://example.com/r.git";
        let no_integrity = codeload_cache_paths(tmp.path(), url, sha, None).unwrap();
        let with_integrity = codeload_cache_paths(tmp.path(), url, sha, Some("sha512-a")).unwrap();
        let with_other_integrity =
            codeload_cache_paths(tmp.path(), url, sha, Some("sha512-b")).unwrap();

        assert_ne!(no_integrity.0, with_integrity.0);
        assert_ne!(with_integrity.0, with_other_integrity.0);
        assert_eq!(with_integrity.1, sha);
    }

    #[test]
    fn extract_codeload_tarball_rejects_unsafe_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = "1111111111111111111111111111111111111111";
        // The tar crate's safe `set_path` rejects `..` paths up front,
        // so a crafted archive must be assembled with the header name
        // field written directly. This mirrors what a hostile
        // codeload mirror could serve, and verifies our component
        // check catches it before any byte lands on disk.
        let body = b"pwn";
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        // GNU header `name[100]` — write directly to skip set_path's
        // safety filter.
        let raw = b"wrapper/../escape.txt";
        let name = &mut h.as_gnu_mut().unwrap().name;
        name[..raw.len()].copy_from_slice(raw);
        h.set_cksum();
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        ar.append(&h, &body[..]).unwrap();
        let bytes = ar.into_inner().unwrap().finish().unwrap();
        let err =
            extract_codeload_tarball_at(tmp.path(), &bytes, "https://example.com/r.git", sha, None)
                .unwrap_err();
        assert!(
            matches!(err, Error::Tar(ref m) if m.contains("unsafe")),
            "expected Error::Tar with unsafe-path message, got {err:?}",
        );
    }

    #[test]
    fn extract_codeload_tarball_rejects_short_commit() {
        // The cache layout assumes `commit` is the canonical 40-hex
        // SHA, both for the cache key and as the returned head_sha.
        // Branch / tag / abbreviated values must be pinned by an
        // upstream `git ls-remote` before reaching here.
        let tmp = tempfile::tempdir().unwrap();
        let bytes = build_codeload_tarball("wrapper", &[("ok", b"ok")]);
        let err = extract_codeload_tarball_at(
            tmp.path(),
            &bytes,
            "https://example.com/r.git",
            "abc1234",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Git(ref m) if m.contains("40-char")));
    }

    #[test]
    fn test_git_shallow_clone_rejects_dash_prefixed_url() {
        let err = git_shallow_clone("--upload-pack=/tmp/evil", "main", false).unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_git_shallow_clone_rejects_dash_prefixed_commit() {
        // `git checkout -- <commit>` treats <commit> as a pathspec,
        // so the `--` separator is unavailable at that call site.
        // The entry-point check is the only defense.
        let err = git_shallow_clone("https://github.com/u/r.git", "-X-evil", false).unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    /// Build a minimal `.tgz` containing a single entry with the
    /// given path / size / content. `set_path` goes through the
    /// public API so the tar crate's own safety checks run.
    fn build_tarball(path: &str, content: &[u8]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        ar.append(&h, content).unwrap();
        ar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn test_import_tarball_accepts_normal_sized_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_tarball("package/index.js", b"console.log('hi');");
        let index = store.import_tarball(&tarball).unwrap();
        assert_eq!(index.len(), 1);
        assert!(index.contains_key("index.js"));
    }

    #[cfg(not(windows))]
    #[test]
    fn test_import_tarball_accepts_posix_colon_filename() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_tarball(
            "package/dist/__mocks__/package-json:version.d.ts",
            b"export {};",
        );
        let index = store.import_tarball(&tarball).unwrap();
        assert!(index.contains_key("dist/__mocks__/package-json:version.d.ts"));
    }

    #[test]
    fn test_import_tarball_rejects_per_entry_cap_exceeded() {
        // A single entry with a declared size past the per-entry cap
        // must be rejected before we allocate or read its contents.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let oversize = (MAX_TARBALL_ENTRY_BYTES + 1) as usize;
        // Small actual content. The declared size in the header is
        // what matters for the fast-path rejection. We craft the
        // header manually to avoid writing a real 512 MiB payload.
        let mut h = tar::Header::new_gnu();
        h.set_path("package/huge.bin").unwrap();
        h.set_size(oversize as u64);
        h.set_mode(0o644);
        h.set_cksum();
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        // `append` refuses a size/content mismatch, so we manually
        // emit the header + pad to a 512-byte block without actual
        // content. The per-entry-cap check runs on `header.size()`
        // before any read, so the stream shape past the header does
        // not matter for this test.
        ar.append(&h, &[][..]).ok();
        let tarball = ar.into_inner().unwrap().finish().unwrap();
        let err = store.import_tarball(&tarball).unwrap_err();
        let msg = match err {
            Error::Tar(m) => m,
            other => panic!("expected Error::Tar, got {other:?}"),
        };
        assert!(msg.contains("per-entry cap"), "unexpected error: {msg}");
    }

    #[test]
    fn test_import_tarball_rejects_archive_decompression_cap() {
        // Two entries whose combined decompressed size exceeds the
        // archive cap while each stays under the per-entry cap. The
        // cap is enforced by wrapping the gzip decoder in
        // `Read::take(cap)`, so the wrapped reader hits EOF mid-way
        // through the second entry and the archive iteration errors.
        //
        // `MAX_TARBALL_DECOMPRESSED_BYTES` and `MAX_TARBALL_ENTRY_BYTES`
        // are both reduced under `cfg(test)` so this test builds
        // only a couple of MiB of payload and stays CI-fast.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        let half = ((MAX_TARBALL_DECOMPRESSED_BYTES / 2) + 1024) as usize;
        let chunk = vec![0u8; half];
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut ar = tar::Builder::new(gz);
        for i in 0..2 {
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("package/chunk{i}.bin")).unwrap();
            h.set_size(chunk.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append(&h, &chunk[..]).unwrap();
        }
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_entry_count_cap() {
        // `MAX_TARBALL_ENTRIES` is reduced under `cfg(test)` so this
        // test only appends a few dozen empty entries.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut ar = tar::Builder::new(gz);
        for i in 0..=MAX_TARBALL_ENTRIES {
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("package/f{i}.txt")).unwrap();
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append(&h, &[][..]).unwrap();
        }
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let err = store.import_tarball(&tarball).unwrap_err();
        let msg = match err {
            Error::Tar(m) => m,
            other => panic!("expected Error::Tar, got {other:?}"),
        };
        assert!(msg.contains("entry cap"), "unexpected error: {msg}");
    }

    // ---------------------------------------------------------------
    // Path traversal / zip-slip defences.
    //
    // A malicious tarball can try to write files outside the package
    // directory at install time by crafting entry paths with `..`,
    // absolute roots, Windows drive prefixes, or smuggled separators
    // inside a single component. `normalize_tar_entry_path` must
    // refuse every such shape before the key enters the
    // `PackageIndex`, and `import_tarball` must refuse symlink /
    // hardlink / device / fifo entries regardless of their path.
    // ---------------------------------------------------------------

    fn build_raw_named_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        // The `tar` crate's `Builder::append` refuses to write `..`
        // paths and other malformed shapes, which is precisely what
        // this test suite needs to construct. Write the header
        // `name` field raw to bypass the safety check — a real
        // attacker uploading a `.tgz` to a registry has no such
        // guard.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_path("placeholder").unwrap();
            let name = &mut h.as_old_mut().name;
            name.fill(0);
            let bytes = path.as_bytes();
            assert!(bytes.len() < 100, "path too long for ustar name field");
            name[..bytes.len()].copy_from_slice(bytes);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append(&h, *data).unwrap();
        }
        ar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn normalize_tar_entry_path_accepts_plain_keys() {
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/index.js")).unwrap(),
            Some("index.js".to_string())
        );
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/lib/util/a.js")).unwrap(),
            Some("lib/util/a.js".to_string())
        );
    }

    #[test]
    fn normalize_tar_entry_path_skips_wrapper_only_entry() {
        assert_eq!(
            normalize_tar_entry_path(Path::new("package")).unwrap(),
            None
        );
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/")).unwrap(),
            None
        );
    }

    #[test]
    fn normalize_tar_entry_path_collapses_cur_dir() {
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/./foo.js")).unwrap(),
            Some("foo.js".to_string())
        );
    }

    #[test]
    fn normalize_tar_entry_path_rejects_parent_dir() {
        let err = normalize_tar_entry_path(Path::new("package/../etc/passwd")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_parent_dir_after_leading_cur_dir() {
        // `./../file` must be rejected. An earlier version of the
        // validator ran the ParentDir check against the raw first
        // component, so the `.` passed it and the `..` was then
        // silently consumed as the wrapper directory.
        let err = normalize_tar_entry_path(Path::new("./../file")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
        let err = normalize_tar_entry_path(Path::new("././../etc/passwd")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_absolute_path() {
        let err = normalize_tar_entry_path(Path::new("/etc/passwd")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_smuggled_backslash() {
        // On unix `Path::components` leaves `a\b` as one Normal
        // component with a literal backslash inside. Reject.
        let err = normalize_tar_entry_path(Path::new("package/a\\..\\etc")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[cfg(windows)]
    #[test]
    fn normalize_tar_entry_path_rejects_colon_on_windows() {
        let err = normalize_tar_entry_path(Path::new("package/C:evil")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_nul() {
        let err = normalize_tar_entry_path(Path::new("package/a\0b")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_parent_dir_escape() {
        // End-to-end: the crafted tarball that the prior zip-slip
        // reproducer used. `import_tarball` must refuse it and
        // produce no `PackageIndex` entries.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_raw_named_tarball(&[
            ("package/package.json", b"{}"),
            ("package/../../../etc/cron.d/evil", b"* * * * * root id\n"),
        ]);
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_absolute_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_raw_named_tarball(&[
            ("package/package.json", b"{}"),
            ("/etc/passwd", b"root:x:0:0\n"),
        ]);
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_symlink_entry() {
        // Symlink entries let a malicious package place the eventual
        // `pkg_dir.join(key)` file through a symlink that points
        // outside the package root. Refuse the entire class.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_path("package/sneaky").unwrap();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_link_name("/etc/passwd").unwrap();
        h.set_cksum();
        ar.append(&h, &[][..]).unwrap();
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_hardlink_entry() {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_path("package/clobber").unwrap();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Link);
        h.set_link_name("../../../../home/victim/.ssh/authorized_keys")
            .unwrap();
        h.set_cksum();
        ar.append(&h, &[][..]).unwrap();
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_skips_pax_global_header() {
        // GitHub-generated tarballs (e.g. `imap@0.8.19`) start with a
        // PAX global header carrying the source git blob SHA in a
        // `comment=...` record. The entry is metadata-only and has no
        // file content; npm/pnpm/bun skip it silently. The extractor
        // must not reject the tarball on sight.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);

        let pax_body = b"52 comment=867aa88a335a266b904e0b5d1a3b0b5d1a3b0b5d1\n";
        let mut gh = tar::Header::new_ustar();
        gh.set_path("pax_global_header").unwrap();
        gh.set_size(pax_body.len() as u64);
        gh.set_mode(0o644);
        gh.set_entry_type(tar::EntryType::XGlobalHeader);
        gh.set_cksum();
        ar.append(&gh, &pax_body[..]).unwrap();

        let body = b"// ok";
        let mut fh = tar::Header::new_gnu();
        fh.set_path("package/index.js").unwrap();
        fh.set_size(body.len() as u64);
        fh.set_mode(0o644);
        fh.set_cksum();
        ar.append(&fh, &body[..]).unwrap();

        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let index = store.import_tarball(&tarball).unwrap();
        assert!(index.contains_key("index.js"));
        assert!(!index.contains_key("pax_global_header"));
    }

    #[test]
    fn test_import_tarball_still_accepts_normal_nested_paths() {
        // Regression guard: the validator must not refuse legitimate
        // deep paths that top-1000 packages actually ship.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_tarball("package/lib/sub/a.js", b"// hi");
        let index = store.import_tarball(&tarball).unwrap();
        assert!(index.contains_key("lib/sub/a.js"));
    }

    #[test]
    fn test_capped_reader_surfaces_exhaustion_as_error() {
        // Regression: `Read::take(cap)` returns a clean EOF when the
        // limit is reached, which in the tar case can land on a
        // block boundary and let an archive silently truncate into
        // a partial index. `CappedReader` must produce an error
        // instead so `tar::Archive` surfaces it to the caller.
        use std::io::Read;
        let mut r = CappedReader::new(&b"hello world"[..], 5);
        let mut first = [0u8; 5];
        r.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"hello");
        let mut rest = Vec::new();
        let err = r.read_to_end(&mut rest).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_capped_reader_does_not_error_below_cap() {
        // Normal reads under the cap behave identically to the
        // inner reader. Only hitting `remaining == 0` errors.
        use std::io::Read;
        let mut r = CappedReader::new(&b"hi"[..], 10);
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn test_capped_reader_empty_buf_is_ok_past_cap() {
        // `Read::read(&mut [])` is a no-op per contract. Even with
        // the cap exhausted, it must return Ok(0) and not error.
        use std::io::Read;
        let mut r = CappedReader::new(&b"abcd"[..], 4);
        let mut buf = [0u8; 4];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(r.read(&mut []).unwrap(), 0);
    }

    #[test]
    fn test_capped_reader_at_exact_boundary_still_errors() {
        // A read that drains exactly to the cap leaves `remaining`
        // at 0. The next read must error, which is the scenario
        // that motivated dropping `Read::take`. A tar block ending
        // on the cap would otherwise EOF silently.
        use std::io::Read;
        let mut r = CappedReader::new(&b"abcd"[..], 4);
        let mut buf = [0u8; 4];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"abcd");
        let mut rest = Vec::new();
        let err = r.read_to_end(&mut rest).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_import_tarball_declared_size_does_not_overallocate() {
        // A malicious header can declare a size near the per-entry
        // cap while shipping almost no actual content. The per-entry
        // `Vec::with_capacity` is clamped to `VEC_PREALLOC_CEILING`
        // so a lying header cannot force a 512 MiB reservation
        // before any byte has been read.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        // Under `cfg(test)` `MAX_TARBALL_ENTRY_BYTES` is 1 MiB, so
        // we declare an entry right at that cap but with only a few
        // content bytes. Import should succeed with no OOM, and the
        // stored content length should match the actual bytes.
        let declared_near_cap = MAX_TARBALL_ENTRY_BYTES;
        let actual_content = b"tiny";
        let mut h = tar::Header::new_gnu();
        h.set_path("package/lying.bin").unwrap();
        h.set_size(declared_near_cap);
        h.set_mode(0o644);
        h.set_cksum();
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut ar = tar::Builder::new(gz);
        // Size mismatch is intentional. `append` will not refuse
        // when we stamp the header manually via `append`.
        ar.append(&h, &actual_content[..]).ok();
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        // The per-entry cap check rejects `declared == cap` values
        // that exceed it; values at exactly the cap pass. Whichever
        // branch fires, the process must not OOM.
        let _ = store.import_tarball(&tarball);
    }
}
