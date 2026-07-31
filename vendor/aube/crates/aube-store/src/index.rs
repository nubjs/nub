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
    /// mtime of the CAS file, in nanoseconds since the epoch, the last
    /// time its bytes were confirmed to hash to `hex_hash`.
    ///
    /// Nanoseconds rather than a coarser unit because the resolution IS
    /// the blind spot: a write landing inside the same tick as the last
    /// verification is invisible. Measured on APFS, truncating to
    /// milliseconds left 1878 of 2000 back-to-back rewrites reporting an
    /// unchanged mtime; at nanosecond precision the timestamps separate.
    ///
    /// The CAS is content-addressed but its entries are *hardlinked* into
    /// every project that installs the package, so any process that can
    /// write a `node_modules` file can rewrite the store entry through
    /// that link — the entry then serves bytes that no longer hash to its
    /// own name, to every future project. This field is what makes the
    /// tamper cheap to detect: an entry whose mtime has not moved since it
    /// was verified needs no read, and only a moved mtime pays a re-hash.
    ///
    /// `None` on entries written before the field existed; those re-hash
    /// once and are restamped.
    #[serde(default)]
    pub checked_at: Option<u64>,
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

fn stored_file_matches_metadata(file: &StoredFile) -> bool {
    file.size
        .map(|size| cas_file_matches_len(&file.store_path, size))
        .unwrap_or_else(|| file.store_path.exists())
}

/// Result of a full-depth warm index load.
pub enum WarmLoad {
    Hit(PackageIndex),
    /// No usable index on disk (absent, unreadable, or unparseable).
    Miss,
    /// The index named a CAS entry that failed its content address. The
    /// bad entries and the index have been removed; derived caches of the
    /// same package are now stale and must be dropped by their owner.
    Corrupt,
}

/// Outcome of checking one CAS entry against its own content address.
enum FileVerdict {
    /// Metadata says the entry is untouched since it was last verified.
    /// No read was performed.
    Unchanged,
    /// The entry was re-hashed and still matches; carries the mtime to
    /// restamp `checked_at` with so the next install takes the fast path.
    Verified(u64),
    /// Gone, truncated, grown, or no longer hashing to its own name.
    Corrupt,
}

/// mtime of a CAS entry that was just published, for stamping into
/// [`StoredFile::checked_at`]. The content is trivially known-good here —
/// it was hashed on the way in — so this only records *when* that was
/// true. `None` on a filesystem with no usable timestamp, which costs one
/// content re-hash on the next warm load and nothing else.
pub(crate) fn published_mtime_ns(store_path: &std::path::Path) -> Option<u64> {
    mtime_ns(&std::fs::metadata(store_path).ok()?)
}

fn mtime_ns(md: &std::fs::Metadata) -> Option<u64> {
    md.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos() as u64)
}

/// Stream the file through BLAKE3 and compare against its content address.
fn content_matches(path: &std::path::Path, expected_hex: &str) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    if std::io::copy(&mut f, &mut hasher).is_err() {
        return false;
    }
    hasher.finalize().to_hex().as_str() == expected_hex
}

/// Check one CAS entry, reading its bytes only when metadata says it moved.
///
/// A size change is decisive on its own and needs no read, so it is
/// checked ahead of the mtime gate rather than behind it: an entry that
/// grew or shrank is corrupt whatever its timestamp claims.
fn verify_stored_file(file: &StoredFile) -> FileVerdict {
    let Ok(md) = std::fs::metadata(&file.store_path) else {
        return FileVerdict::Corrupt;
    };
    if file.size.is_some_and(|size| size != md.len()) {
        return FileVerdict::Corrupt;
    }
    let Some(now_ns) = mtime_ns(&md) else {
        // No usable timestamp (exotic filesystem): fall back to reading.
        return if content_matches(&file.store_path, &file.hex_hash) {
            FileVerdict::Unchanged
        } else {
            FileVerdict::Corrupt
        };
    };
    // Exact equality, with no tolerance window. `checked_at` records the
    // entry's OWN mtime at the moment its bytes were confirmed, not a
    // wall-clock reading taken alongside it, so an untouched entry reports
    // a bit-identical value and any write moves it strictly forward. pnpm
    // needs a 100ms slop here precisely because it stamps a wall clock;
    // stamping the inode instead removes both the slop and the window it
    // leaves open.
    if file.checked_at == Some(now_ns) {
        return FileVerdict::Unchanged;
    }
    if content_matches(&file.store_path, &file.hex_hash) {
        FileVerdict::Verified(now_ns)
    } else {
        FileVerdict::Corrupt
    }
}

/// What a full-depth check concluded about a whole package index.
pub(crate) enum IndexVerdict {
    Intact,
    /// Intact, but at least one entry was re-hashed and its `checked_at`
    /// refreshed — the index should be written back so the next install
    /// does not pay the read again.
    Restamped,
    Corrupt,
}

/// Verify every entry of `index`, restamping the ones that had to be read.
///
/// A corrupt entry is UNLINKED from the CAS before returning. That is
/// load-bearing for repair, not tidiness: the import path dedupes on
/// `(path exists, length matches)`, so an equal-length overwrite left in
/// place would be silently adopted again by the very re-fetch meant to
/// replace it. Unlinking drops only the store's own reference — projects
/// already holding the tampered inode keep it, exactly as pnpm behaves.
fn verify_index_files(index: &mut PackageIndex) -> IndexVerdict {
    let mut restamped = false;
    let mut corrupt = false;
    for file in index.values_mut() {
        match verify_stored_file(file) {
            FileVerdict::Unchanged => {}
            FileVerdict::Verified(now_ns) => {
                file.checked_at = Some(now_ns);
                restamped = true;
            }
            FileVerdict::Corrupt => {
                let _ = xx::file::remove_file(&file.store_path);
                corrupt = true;
            }
        }
    }
    match (corrupt, restamped) {
        (true, _) => IndexVerdict::Corrupt,
        (false, true) => IndexVerdict::Restamped,
        (false, false) => IndexVerdict::Intact,
    }
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

    /// Load a package index, verifying every CAS entry against its own
    /// content address. See [`Store::load_index_checked`]; this is the
    /// `Option` view for callers that do not distinguish a corrupt entry
    /// from a plain cache miss.
    pub fn load_index_verified(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<PackageIndex> {
        match self.load_index_checked(name, version, integrity) {
            WarmLoad::Hit(index) => Some(index),
            WarmLoad::Miss | WarmLoad::Corrupt => None,
        }
    }

    /// Full-depth warm load: parse the index, then check every CAS entry
    /// it names against its own content address, repairing what fails.
    ///
    /// [`WarmLoad::Corrupt`] is reported separately from a miss because a
    /// store entry that failed its content address invalidates every
    /// *derived* copy of the same package too — notably the extracted
    /// `trees/` tier, which is built once from the CAS and then cloned
    /// forever without revalidation. A caller that owns a derived tier
    /// must drop its copy on `Corrupt` or the repaired CAS is bypassed.
    pub fn load_index_checked(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> WarmLoad {
        let Some(index_path) = self.index_path(name, version, integrity) else {
            return WarmLoad::Miss;
        };
        let Ok(buf) = xx::file::read(&index_path) else {
            return WarmLoad::Miss;
        };
        let Ok(mut index) = sonic_rs::from_slice::<PackageIndex>(&buf) else {
            return WarmLoad::Miss;
        };
        match verify_index_files(&mut index) {
            IndexVerdict::Intact => {}
            IndexVerdict::Restamped => {
                if let Ok(json) = serde_json::to_string(&index) {
                    let _ = xx::file::write(&index_path, json);
                }
            }
            IndexVerdict::Corrupt => {
                let _ = xx::file::remove_file(&index_path);
                self.mark_repaired(name, version);
                return WarmLoad::Corrupt;
            }
        }
        trace!("cache hit: {name}@{version}");
        WarmLoad::Hit(index)
    }

    fn mark_repaired(&self, name: &str, version: &str) {
        if let Ok(mut set) = self.repaired.lock() {
            set.insert(format!("{name}@{version}"));
        }
    }

    /// Whether this process discarded a tampered CAS entry for
    /// `name@version`. Callers holding a copy of the package derived from
    /// the store before the repair — the extracted `trees/` tier — must
    /// rebuild it rather than reuse it. See [`Store::load_index_checked`].
    pub fn was_repaired(&self, name: &str, version: &str) -> bool {
        self.repaired
            .lock()
            .map(|set| set.contains(&format!("{name}@{version}")))
            .unwrap_or(false)
    }

    fn load_index_inner(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
        verify_files: bool,
    ) -> Option<PackageIndex> {
        if verify_files {
            return self.load_index_verified(name, version, integrity);
        }
        let index_path = self.index_path(name, version, integrity)?;
        let buf = xx::file::read(&index_path).ok()?;
        let index: PackageIndex = sonic_rs::from_slice(&buf).ok()?;
        // Hot install path: one metadata check catches the common crash
        // residue class (zero-byte/missing CAS files) without turning every
        // warm lockfile install into a full store walk.
        if !index
            .values()
            .next()
            .is_none_or(stored_file_matches_metadata)
        {
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
