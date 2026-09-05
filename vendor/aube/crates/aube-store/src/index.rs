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
    #[serde(with = "stored_path")]
    pub store_path: PathBuf,
    /// Whether the file is executable.
    pub executable: bool,
    /// File size in bytes when the entry was imported.
    #[serde(default)]
    pub size: Option<u64>,
}

mod stored_path {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
    use std::path::{Path, PathBuf};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum NativePath {
        UnixBytes(Vec<u8>),
        WindowsWide(Vec<u16>),
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredPath {
        Utf8(String),
        Native(NativePath),
    }

    pub(super) fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(path) = path.to_str() {
            return serializer.serialize_str(path);
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            NativePath::UnixBytes(path.as_os_str().as_bytes().to_vec()).serialize(serializer)
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            NativePath::WindowsWide(path.as_os_str().encode_wide().collect()).serialize(serializer)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(serde::ser::Error::custom(
                "path contains characters unsupported by this platform",
            ))
        }
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        match StoredPath::deserialize(deserializer)? {
            StoredPath::Utf8(path) => Ok(PathBuf::from(path)),
            StoredPath::Native(NativePath::UnixBytes(bytes)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
                }
                #[cfg(not(unix))]
                {
                    let _ = bytes;
                    Err(de::Error::custom("Unix path cache read on a non-Unix host"))
                }
            }
            StoredPath::Native(NativePath::WindowsWide(wide)) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
                }
                #[cfg(not(windows))]
                {
                    let _ = wide;
                    Err(de::Error::custom(
                        "Windows path cache read on a non-Windows host",
                    ))
                }
            }
        }
    }
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
            // A stale entry served by the read fallback is not ours to
            // delete; the re-fetch this miss triggers saves a fresh index
            // into this store, which then shadows it.
            if index_path.starts_with(self.index_dir()) && self.prepare_for_write().is_ok() {
                let _ = xx::file::remove_file(&index_path);
            }
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
    /// Only this store's own entry is touched: a copy in the read
    /// fallback is left alone and is shadowed by the next `save_index`.
    pub fn invalidate_cached_index(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Result<bool, Error> {
        self.prepare_for_write()?;
        let Some(index_path) = self.index_write_path(name, version, integrity) else {
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
        self.prepare_for_write()?;
        let index_path = self.index_write_path(name, version, integrity).ok_or_else(|| {
            Error::Tar(format!(
                "refusing to cache: invalid coordinate {name:?}@{version:?} or integrity {integrity:?}"
            ))
        })?;
        let json = sonic_rs::to_vec(index).map_err(|e| Error::Tar(format!("serialize: {e}")))?;
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
        let rel = self.index_rel(name, version, integrity)?;
        let primary = self.index_dir().join(&rel);
        Some(self.read_through(primary, &PathBuf::from(crate::INDEX_SUBDIR).join(rel)))
    }

    /// Where `save_index` writes and `invalidate_cached_index` deletes:
    /// this store's own index dir, never the read fallback's.
    fn index_write_path(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<PathBuf> {
        Some(
            self.index_dir()
                .join(self.index_rel(name, version, integrity)?),
        )
    }

    fn index_rel(&self, name: &str, version: &str, integrity: Option<&str>) -> Option<PathBuf> {
        let safe_name = validate_and_encode_name(name)?;
        if !validate_version(version) {
            return None;
        }
        let filename = format!("{safe_name}@{version}.json");
        let rel = match integrity {
            Some(i) => {
                let hex = integrity_to_hex(i)?;
                // 16 hex chars = 64 bits of tarball SHA-512 prefix.
                // Two tarballs whose SHA-512 prefixes collide would
                // both have to be valid registry responses for the
                // same (name, version) *and* survive `verify_integrity`
                // on fetch, so birthday-bound collisions aren't a
                // correctness risk; 16 chars is plenty.
                let short = &hex[..16.min(hex.len())];
                PathBuf::from(short).join(filename)
            }
            None => PathBuf::from(filename),
        };
        Some(rel)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::StoredFile;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn stored_file_round_trips_non_utf8_store_path() {
        let stored = StoredFile {
            hex_hash: "abc123".into(),
            store_path: std::path::PathBuf::from(std::ffi::OsString::from_vec(
                b"/store/path-\xff".to_vec(),
            )),
            executable: false,
            size: Some(3),
        };

        let json = serde_json::to_string(&stored).unwrap();
        let decoded: StoredFile = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.store_path, stored.store_path);
        assert!(json.contains("unixBytes"));
    }
}
