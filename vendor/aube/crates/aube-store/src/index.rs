use crate::{
    Error, Store, cas_file_matches_len, integrity_to_hex, validate_and_encode_name,
    validate_version,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Metadata about a file stored in the CAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFile {
    /// The hex hash of the file content.
    pub hex_hash: String,
    /// The path within the store.
    pub store_path: PathBuf,
    /// Whether the file is executable.
    pub executable: bool,
    /// File size in bytes when the entry was imported.
    #[serde(default)]
    pub size: Option<u64>,
}

/// Index of all files in a package, keyed by relative path within the package.
///
/// Backed by `FxMap` (foldhash) rather than `BTreeMap`: the linker
/// iterates this map per package and only two non-hot call sites do
/// keyed lookups (`ignored_builds` checks for `"package.json"` and
/// `"binding.gyp"`). Hash-based lookup is O(1) for those, and the
/// flat-bucket layout deserializes/clones with one allocation
/// instead of one per entry. Iteration order is no longer
/// lexicographic — cache JSON files now ship in hash order, which
/// doesn't affect any caller (caches are keyed by tarball path, not
/// file content).
pub type PackageIndex = aube_util::collections::FxMap<String, StoredFile>;

/// Deterministic content fingerprint of a materialized package.
///
/// Hashes the package's full file set — every relative path plus its
/// CAS content hash and executable bit — in sorted order, so two
/// imports with byte-identical trees produce the same fingerprint and
/// two imports that differ in any file (presence, contents, or mode)
/// produce different fingerprints.
///
/// Used by the global virtual store to disambiguate source-backed
/// dependencies (git / remote tarball) whose lockfile coordinate is
/// identical but whose materialized bytes are not — e.g. the same git
/// commit installed once normally (its `prepare` script built `dist/`)
/// and once under `--ignore-scripts` (raw checkout, no `dist/`). The
/// graph hash folds this in so the two land at distinct GVS paths
/// instead of the first writer's tree leaking into the second project.
///
/// `PackageIndex` is an `FxMap` with non-deterministic iteration order,
/// so the entries are collected and sorted by path before hashing.
pub fn index_content_fingerprint(index: &PackageIndex) -> String {
    let mut entries: Vec<(&str, &str, bool)> = index
        .iter()
        .map(|(path, file)| (path.as_str(), file.hex_hash.as_str(), file.executable))
        .collect();
    entries.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for (path, hex_hash, executable) in entries {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(hex_hash.as_bytes());
        hasher.update(if executable { b"\x01" } else { b"\x00" });
    }
    hasher.finalize().to_hex().to_string()
}

fn index_files_match_metadata(index: &PackageIndex, verify_all: bool) -> bool {
    let mut files = index.values();
    if verify_all {
        return files.all(stored_file_matches_metadata);
    }
    // Hot install path: one metadata check catches the common crash
    // residue class (zero-byte/missing CAS files) without turning every
    // warm lockfile install into a full store walk.
    files.next().is_none_or(stored_file_matches_metadata)
}

fn stored_file_matches_metadata(file: &StoredFile) -> bool {
    file.size
        .map(|size| cas_file_matches_len(&file.store_path, size))
        .unwrap_or_else(|| file.store_path.exists())
}

impl Store {
    /// Load a cached package index, if it exists.
    ///
    /// `integrity`, when `Some`, is the registry-advertised SRI
    /// digest (`sha512-`, or legacy `sha1-` / `sha256-` / `sha384-`)
    /// of the tarball these cache files came from —
    /// part of the cache key so the same `(name, version)` resolved
    /// from different sources (npm registry vs. github codeload vs. a
    /// proxy that served different bytes) can't alias on disk and
    /// return each other's file lists to the linker. `None` falls
    /// back to an unsuffixed `<name>@<version>.json` key so packages
    /// fetched through a registry proxy that strips `dist.integrity`
    /// can still warm-install — an integrity-less setup is already a
    /// degraded mode the user opted into via `strict-store-integrity=false`.
    pub fn load_index(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<PackageIndex> {
        self.load_index_inner(name, version, integrity, false)
    }

    /// Load a package index, optionally verifying that all store files still exist.
    /// The verified variant is slower (stat per file) but detects a corrupted store.
    pub fn load_index_verified(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<PackageIndex> {
        self.load_index_inner(name, version, integrity, true)
    }

    fn load_index_inner(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
        verify_files: bool,
    ) -> Option<PackageIndex> {
        let index_path = self.index_path(name, version, integrity)?;
        let buf = xx::file::read(&index_path).ok()?;
        let index: PackageIndex = sonic_rs::from_slice(&buf).ok()?;
        if !index_files_match_metadata(&index, verify_files) {
            trace!("cache stale: {name}@{version}");
            let _ = xx::file::remove_file(&index_path);
            return None;
        }
        trace!("cache hit: {name}@{version}");
        Some(index)
    }

    /// Delete the cached package index for `(name, version, integrity)` if
    /// it exists. Used as a recovery hatch when the linker discovers a
    /// CAS shard referenced by the index has gone missing — the cached
    /// JSON points at a dead `store_path`, so the next install must
    /// re-derive the index by re-importing the tarball.
    ///
    /// `Ok(true)` when an entry was removed; `Ok(false)` when there
    /// was nothing to remove (or the coordinate was invalid). Errors
    /// surface only on real I/O failure, not on the missing-file case.
    pub fn invalidate_cached_index(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Result<bool, Error> {
        let Some(index_path) = self.index_path(name, version, integrity) else {
            return Ok(false);
        };
        match std::fs::remove_file(&index_path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(index_path, e)),
        }
    }

    /// Save a package index to the cache.
    ///
    /// See [`load_index`](Self::load_index) for the semantics of
    /// `integrity` and the integrity-less fallback.
    pub fn save_index(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
        index: &PackageIndex,
    ) -> Result<(), Error> {
        let index_path = self.index_path(name, version, integrity).ok_or_else(|| {
            Error::Tar(format!(
                "refusing to cache: invalid coordinate {name:?}@{version:?} or integrity {integrity:?}"
            ))
        })?;
        let json =
            serde_json::to_string(index).map_err(|e| Error::Tar(format!("serialize: {e}")))?;
        xx::file::write(&index_path, json).map_err(|e| Error::Xx(e.to_string()))?;
        trace!("cached index: {name}@{version}");
        Ok(())
    }

    /// Build the on-disk path for a cached index.
    ///
    /// Layout:
    /// - With integrity: `index/<16 hex>/<name>@<version>.json`. The
    ///   integrity hex lives in a subdirectory (not as part of the
    ///   filename) so a version whose semver build metadata happens
    ///   to be 16 lowercase hex chars (e.g. `1.0.0+a1b2c3d4e5f6a7b8`)
    ///   can never collide with an integrity-keyed entry for
    ///   `1.0.0` — they land in distinct directories by construction.
    /// - Without integrity: `index/<name>@<version>.json` at the
    ///   index dir root. Used for registry proxies that strip
    ///   `dist.integrity`; the user has already opted out of
    ///   cross-source integrity enforcement.
    ///
    /// Returns `None` when any component is invalid (including an
    /// integrity string we can't hex-decode).
    pub(crate) fn index_path(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<PathBuf> {
        let safe_name = validate_and_encode_name(name)?;
        if !validate_version(version) {
            return None;
        }
        let filename = format!("{safe_name}@{version}.json");
        let dir = self.index_dir();
        match integrity {
            Some(i) => {
                let hex = integrity_to_hex(i)?;
                // 16 hex chars = 64 bits of tarball SHA-512 prefix.
                // Two tarballs whose SHA-512 prefixes collide would
                // both have to be valid registry responses for the
                // same (name, version) *and* survive `verify_integrity`
                // on fetch, so birthday-bound collisions aren't a
                // correctness risk; 16 chars is plenty.
                let short = &hex[..16.min(hex.len())];
                Some(dir.join(short).join(filename))
            }
            None => Some(dir.join(filename)),
        }
    }
}
