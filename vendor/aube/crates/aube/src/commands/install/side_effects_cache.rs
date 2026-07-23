use miette::{Context, IntoDiagnostic, miette};
use sha2::Digest;

const SIDE_EFFECTS_CACHE_MARKER: &str = ".aube-side-effects-cache";
const SIDE_EFFECTS_CACHE_TMP_PREFIX: &str = ".tmp-side-effects-";
const SIDE_EFFECTS_CACHE_TMP_STALE_AFTER: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

/// Where cache entries live, paired with the Node the lifecycle
/// scripts run under. The two travel together so no call site can name
/// a root without also naming the engine: an entry holds *post-build*
/// artifacts — a native addon's `build/Release/*.node` among them — and
/// those are only loadable by the ABI that compiled them.
///
/// `node_version` is `None` only when the version could not be
/// resolved at all.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SideEffectsCacheLocation<'a> {
    pub(crate) root: &'a std::path::Path,
    pub(crate) node_version: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SideEffectsCacheConfig<'a> {
    Disabled,
    RestoreOnly(SideEffectsCacheLocation<'a>),
    RestoreAndSave(SideEffectsCacheLocation<'a>),
    SaveOnlyOverwrite(SideEffectsCacheLocation<'a>),
}

impl<'a> SideEffectsCacheConfig<'a> {
    pub(super) fn location(self) -> Option<SideEffectsCacheLocation<'a>> {
        match self {
            Self::Disabled => None,
            Self::RestoreOnly(loc) | Self::RestoreAndSave(loc) | Self::SaveOnlyOverwrite(loc) => {
                Some(loc)
            }
        }
    }

    pub(super) fn should_restore(self) -> bool {
        matches!(self, Self::RestoreOnly(_) | Self::RestoreAndSave(_))
    }

    pub(super) fn overwrite_existing(self) -> bool {
        matches!(self, Self::SaveOnlyOverwrite(_))
    }

    pub(super) fn should_save(self) -> bool {
        matches!(self, Self::RestoreAndSave(_) | Self::SaveOnlyOverwrite(_))
    }
}

#[derive(Debug, Clone)]
pub(super) struct SideEffectsCacheEntry {
    engine: String,
    input_hash: String,
    path: std::path::PathBuf,
}

pub(super) enum SideEffectsCacheRestore {
    Miss,
    Restored,
    AlreadyApplied,
}

impl SideEffectsCacheEntry {
    pub(super) fn new(
        location: SideEffectsCacheLocation<'_>,
        name: &str,
        version: &str,
        package_dir: &std::path::Path,
    ) -> miette::Result<Self> {
        // Take only the hash half of the marker: it fingerprints the
        // package *before* its scripts ran, which is what keys this entry
        // no matter which engine last built the directory. Reading it
        // engine-agnostically is also what keeps a marker written before
        // engines were recorded from forcing a rehash of the post-build
        // tree, which would key the entry off the wrong bytes.
        let input_hash = match read_valid_side_effects_marker(package_dir) {
            Some(marker) => marker.input_hash,
            None => hash_dir_for_side_effects_cache(package_dir)?,
        };
        let safe_name = name.replace('/', "__");
        // `input_hash` fingerprints the package *before* its scripts run,
        // so it can never stand in for the engine. Reuse the virtual
        // store's own engine name rather than a second spelling of it, so
        // the two caches segregate on identical axes.
        let engine = match location.node_version {
            Some(v) => aube_lockfile::graph_hash::engine_name_default(v).0,
            None => aube_lockfile::graph_hash::platform_name(),
        };
        Ok(Self {
            path: location
                .root
                .join(format!("{safe_name}@{version}"))
                .join(&engine)
                .join(&input_hash),
            engine,
            input_hash,
        })
    }

    pub(super) fn restore_if_available(
        &self,
        package_dir: &std::path::Path,
    ) -> miette::Result<SideEffectsCacheRestore> {
        if self.marker_matches(package_dir) && self.path.is_dir() {
            tracing::debug!(
                "side-effects-cache: already applied {}",
                self.path.display()
            );
            return Ok(SideEffectsCacheRestore::AlreadyApplied);
        }
        if !self.path.is_dir() {
            return Ok(SideEffectsCacheRestore::Miss);
        }
        copy_dir(&self.path, package_dir, CopyMode::HardlinkOrCopy).wrap_err_with(|| {
            format!(
                "failed to restore side effects cache from {}",
                self.path.display()
            )
        })?;
        // The copy carries the entry's own marker across, so restamping is
        // a no-op except for an entry saved before markers named an engine
        // — that one would fail every future match and re-copy forever.
        write_side_effects_marker(package_dir, &self.engine, &self.input_hash)?;
        tracing::debug!("side-effects-cache: restored {}", self.path.display());
        Ok(SideEffectsCacheRestore::Restored)
    }

    /// True when this package directory's contents were produced by *this*
    /// entry. Both halves are load-bearing: entries segregate by engine, so
    /// several now share one input hash, and matching on the hash alone
    /// would let the skip above fire for a build made under a different
    /// Node ABI — leaving that build's `.node` in place for a runtime
    /// `NODE_MODULE_VERSION` failure. A marker with no engine (written
    /// before this was recorded) never matches, so it degrades to a restore
    /// or a rebuild, never to a silent skip.
    fn marker_matches(&self, package_dir: &std::path::Path) -> bool {
        read_valid_side_effects_marker(package_dir).is_some_and(|marker| {
            marker.engine.as_deref() == Some(self.engine.as_str())
                && marker.input_hash == self.input_hash
        })
    }

    pub(super) fn save(
        &self,
        package_dir: &std::path::Path,
        overwrite_existing: bool,
    ) -> miette::Result<()> {
        if self.path.is_dir() {
            if overwrite_existing {
                std::fs::remove_dir_all(&self.path)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to remove {}", self.path.display()))?;
            } else {
                write_side_effects_marker(package_dir, &self.engine, &self.input_hash)?;
                return Ok(());
            }
        }
        let parent = self.path.parent().ok_or_else(|| {
            miette!(
                "invalid side effects cache path has no parent: {}",
                self.path.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        sweep_stale_side_effects_tmp_dirs(parent);
        write_side_effects_marker(package_dir, &self.engine, &self.input_hash)?;

        let tmp = parent.join(format!(
            "{SIDE_EFFECTS_CACHE_TMP_PREFIX}{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to remove {}", tmp.display()))?;
        }
        copy_dir(package_dir, &tmp, CopyMode::Copy).wrap_err_with(|| {
            format!(
                "failed to write side effects cache into {}",
                self.path.display()
            )
        })?;
        match aube_util::fs_atomic::rename_with_retry(&tmp, &self.path) {
            Ok(()) => {
                tracing::debug!("side-effects-cache: saved {}", self.path.display());
                Ok(())
            }
            Err(e) if self.path.is_dir() => {
                tracing::debug!(
                    "side-effects-cache: cache appeared while saving {}: {e}",
                    self.path.display()
                );
                let _ = std::fs::remove_dir_all(&tmp);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(e)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to publish {}", self.path.display()))
            }
        }
    }
}

fn sweep_stale_side_effects_tmp_dirs(parent: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if should_remove_side_effects_tmp_dir(&entry) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn should_remove_side_effects_tmp_dir(entry: &std::fs::DirEntry) -> bool {
    if !entry
        .file_name()
        .to_string_lossy()
        .starts_with(SIDE_EFFECTS_CACHE_TMP_PREFIX)
    {
        return false;
    }
    entry
        .metadata()
        .and_then(|m| m.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|age| age >= SIDE_EFFECTS_CACHE_TMP_STALE_AFTER)
}

pub(crate) fn side_effects_cache_root(store: &aube_store::Store) -> std::path::PathBuf {
    store
        .virtual_store_dir()
        .parent()
        .unwrap_or_else(|| store.root())
        .join("side-effects-v1")
}

/// Parsed marker contents: `<engine>:<input_hash>`. `engine` is `None` for
/// the bare-hash form written before the engine was recorded.
struct SideEffectsMarker {
    engine: Option<String>,
    input_hash: String,
}

/// Only the hash half is validated, because only the hash is ever joined
/// into a path — the engine a lookup keys on comes from the install's own
/// resolved Node, and the marker's copy is compared, never trusted as a
/// path segment.
fn read_valid_side_effects_marker(package_dir: &std::path::Path) -> Option<SideEffectsMarker> {
    let marker = std::fs::read_to_string(package_dir.join(SIDE_EFFECTS_CACHE_MARKER)).ok()?;
    let marker = marker.trim();
    let (engine, hash) = match marker.rsplit_once(':') {
        Some((engine, hash)) => (Some(engine), hash),
        None => (None, marker),
    };
    is_side_effects_cache_hash(hash).then(|| SideEffectsMarker {
        engine: engine.map(str::to_owned),
        input_hash: hash.to_ascii_lowercase(),
    })
}

fn is_side_effects_cache_hash(value: &str) -> bool {
    value.len() == 128 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn write_side_effects_marker(
    package_dir: &std::path::Path,
    engine: &str,
    input_hash: &str,
) -> miette::Result<()> {
    aube_util::fs_atomic::atomic_write(
        &package_dir.join(SIDE_EFFECTS_CACHE_MARKER),
        format!("{engine}:{input_hash}").as_bytes(),
    )
    .into_diagnostic()
    .wrap_err_with(|| {
        format!(
            "failed to write side effects cache marker in {}",
            package_dir.display()
        )
    })
}

fn hash_dir_for_side_effects_cache(package_dir: &std::path::Path) -> miette::Result<String> {
    let mut hasher = sha2::Sha512::new();
    hash_dir_inner(package_dir, package_dir, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn hash_dir_inner(
    base: &std::path::Path,
    current: &std::path::Path,
    hasher: &mut sha2::Sha512,
) -> miette::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(current)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(SIDE_EFFECTS_CACHE_MARKER) {
            continue;
        }
        let rel = path
            .strip_prefix(base)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to relativize {}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let meta = std::fs::symlink_metadata(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
        hasher.update(rel.as_bytes());
        if meta.file_type().is_symlink() {
            hasher.update(b"\0symlink\0");
            let target = std::fs::read_link(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read symlink {}", path.display()))?;
            hasher.update(target.to_string_lossy().as_bytes());
        } else if meta.is_dir() {
            hasher.update(b"\0dir\0");
            hash_dir_inner(base, &path, hasher)?;
        } else if meta.is_file() {
            hasher.update(b"\0file\0");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hasher.update((meta.permissions().mode() & 0o7777).to_le_bytes());
            }
            let bytes = std::fs::read(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            hasher.update(bytes);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum CopyMode {
    Copy,
    HardlinkOrCopy,
}

pub(super) fn copy_dir(
    src: &std::path::Path,
    dst: &std::path::Path,
    mode: CopyMode,
) -> miette::Result<()> {
    if dst.symlink_metadata().is_ok() {
        remove_path(dst)?;
    }
    std::fs::create_dir_all(dst)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create {}", dst.display()))?;
    copy_dir_inner(src, src, dst, mode)
}

fn copy_dir_inner(
    base: &std::path::Path,
    current: &std::path::Path,
    dst_root: &std::path::Path,
    mode: CopyMode,
) -> miette::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(current)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", current.display()))?;
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to relativize {}", path.display()))?;
        let dst = dst_root.join(rel);
        let meta = std::fs::symlink_metadata(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
        if meta.file_type().is_symlink() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            create_symlink_like(&path, &dst, meta.file_type())?;
        } else if meta.is_dir() {
            std::fs::create_dir_all(&dst)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create {}", dst.display()))?;
            copy_dir_inner(base, &path, dst_root, mode)?;
        } else if meta.is_file() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            match mode {
                CopyMode::Copy => {
                    std::fs::copy(&path, &dst)
                        .into_diagnostic()
                        .wrap_err_with(|| format!("failed to copy {}", dst.display()))?;
                }
                CopyMode::HardlinkOrCopy => {
                    if let Err(e) = std::fs::hard_link(&path, &dst) {
                        tracing::debug!(
                            "side-effects-cache: hardlink failed for {} -> {}: {e}; copying",
                            path.display(),
                            dst.display()
                        );
                        std::fs::copy(&path, &dst)
                            .into_diagnostic()
                            .wrap_err_with(|| format!("failed to copy {}", dst.display()))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn remove_path(path: &std::path::Path) -> miette::Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .into_diagnostic()
    .wrap_err_with(|| format!("failed to remove {}", path.display()))
}

#[cfg(unix)]
fn create_symlink_like(
    src: &std::path::Path,
    dst: &std::path::Path,
    _file_type: std::fs::FileType,
) -> miette::Result<()> {
    let target = std::fs::read_link(src)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read symlink {}", src.display()))?;
    std::os::unix::fs::symlink(&target, dst)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to symlink {}", dst.display()))
}

#[cfg(windows)]
fn create_symlink_like(
    src: &std::path::Path,
    dst: &std::path::Path,
    file_type: std::fs::FileType,
) -> miette::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let target = std::fs::read_link(src)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read symlink {}", src.display()))?;
    if file_type.is_symlink_dir() {
        aube_linker::create_dir_link(&target, dst)
    } else {
        std::os::windows::fs::symlink_file(&target, dst)
    }
    .into_diagnostic()
    .wrap_err_with(|| format!("failed to symlink {}", dst.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        root: &std::path::Path,
        package_dir: &std::path::Path,
        node_version: Option<&str>,
    ) -> SideEffectsCacheEntry {
        SideEffectsCacheEntry::new(
            SideEffectsCacheLocation { root, node_version },
            "p",
            "1.0.0",
            package_dir,
        )
        .unwrap()
    }

    fn entry_path(
        root: &std::path::Path,
        package_dir: &std::path::Path,
        node_version: Option<&str>,
    ) -> std::path::PathBuf {
        entry(root, package_dir, node_version).path
    }

    fn package_fixture(root: &std::path::Path) -> std::path::PathBuf {
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), "{\"name\":\"p\"}\n").unwrap();
        pkg
    }

    #[test]
    fn cache_path_segregates_by_platform() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let s = entry_path(dir.path(), &pkg, Some("22.15.0"))
            .to_string_lossy()
            .into_owned();
        let segment = aube_lockfile::graph_hash::platform_name();
        assert!(
            s.contains(&segment),
            "cache path lacks platform segment {segment}: {s}"
        );
    }

    #[test]
    fn cache_path_segregates_by_node_major() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let node22 = entry_path(dir.path(), &pkg, Some("22.15.0"));
        let node26 = entry_path(dir.path(), &pkg, Some("26.5.0"));
        let unknown = entry_path(dir.path(), &pkg, None);
        assert_ne!(
            node22, node26,
            "a native build under one Node major must not be restorable into another"
        );
        assert_eq!(
            node22,
            entry_path(dir.path(), &pkg, Some("22.16.0")),
            "NODE_MODULE_VERSION tracks the major, so a minor bump must stay a cache hit"
        );
        assert_ne!(
            unknown, node22,
            "an unresolved Node version must not collide with a known engine"
        );
        assert_ne!(unknown, node26);
        assert!(
            unknown
                .to_string_lossy()
                .contains(&aube_lockfile::graph_hash::platform_name()),
            "unresolved Node version should still key on the platform: {}",
            unknown.display()
        );
    }

    #[test]
    fn side_effects_marker_accepts_only_sha512_hex() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = dir.path().join(SIDE_EFFECTS_CACHE_MARKER);

        std::fs::write(&marker_path, "../../evil").unwrap();
        assert!(read_valid_side_effects_marker(dir.path()).is_none());

        std::fs::write(
            &marker_path,
            format!("darwin-arm64-node26:{}", "z".repeat(128)),
        )
        .unwrap();
        assert!(
            read_valid_side_effects_marker(dir.path()).is_none(),
            "a non-hex hash must be rejected however it is prefixed"
        );

        std::fs::write(&marker_path, format!("{}\n", "A".repeat(128))).unwrap();
        let legacy = read_valid_side_effects_marker(dir.path()).unwrap();
        assert_eq!(legacy.engine, None);
        assert_eq!(legacy.input_hash, "a".repeat(128));

        std::fs::write(
            &marker_path,
            format!("darwin-arm64-node26:{}\n", "A".repeat(128)),
        )
        .unwrap();
        let current = read_valid_side_effects_marker(dir.path()).unwrap();
        assert_eq!(current.engine.as_deref(), Some("darwin-arm64-node26"));
        assert_eq!(current.input_hash, "a".repeat(128));
    }

    #[test]
    fn already_applied_requires_the_marker_to_name_the_same_engine() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let root = dir.path().join("cache");

        entry(&root, &pkg, Some("26.5.0"))
            .save(&pkg, false)
            .unwrap();
        assert!(
            matches!(
                entry(&root, &pkg, Some("26.5.0"))
                    .restore_if_available(&pkg)
                    .unwrap(),
                SideEffectsCacheRestore::AlreadyApplied
            ),
            "the same engine that built this directory must still skip the rebuild"
        );

        let node22 = entry(&root, &pkg, Some("22.15.0"));
        assert!(
            matches!(
                node22.restore_if_available(&pkg).unwrap(),
                SideEffectsCacheRestore::Miss
            ),
            "another engine's marker must not stand in for this engine's missing entry"
        );
        node22.save(&pkg, false).unwrap();

        // The directory now holds Node 22's build while Node 26's entry
        // still exists under the same input hash — the case that used to
        // skip and leave a wrong-ABI addon in place.
        assert!(
            matches!(
                entry(&root, &pkg, Some("26.5.0"))
                    .restore_if_available(&pkg)
                    .unwrap(),
                SideEffectsCacheRestore::Restored
            ),
            "a directory built under another engine must be restored, not skipped"
        );
    }

    #[test]
    fn engineless_marker_restores_rather_than_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let root = dir.path().join("cache");

        let saved = entry(&root, &pkg, Some("26.5.0"));
        saved.save(&pkg, false).unwrap();
        std::fs::write(pkg.join(SIDE_EFFECTS_CACHE_MARKER), &saved.input_hash).unwrap();

        let reread = entry(&root, &pkg, Some("26.5.0"));
        assert_eq!(
            reread.input_hash, saved.input_hash,
            "an engineless marker must still supply the input hash, so the entry keeps its key"
        );
        assert!(
            matches!(
                reread.restore_if_available(&pkg).unwrap(),
                SideEffectsCacheRestore::Restored
            ),
            "an engineless marker must degrade to a restore, never to a skip"
        );
        assert!(
            matches!(
                entry(&root, &pkg, Some("26.5.0"))
                    .restore_if_available(&pkg)
                    .unwrap(),
                SideEffectsCacheRestore::AlreadyApplied
            ),
            "the restore restamps the marker, so the skip returns on the next install"
        );
    }
}
