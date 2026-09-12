use miette::{Context, IntoDiagnostic, miette};
use sha2::Digest;

/// Filename prefix of the installer-owned marker the side-effects cache
/// stamps beside each package. Brand-scoped to the active embedder because
/// the marker lands inside a consumer's `node_modules` — a hardcoded `aube`
/// leaf puts the engine's brand in an embedder's user-visible tree. `prog()`
/// is `"aube"` under the default profile, so standalone aube's on-disk name
/// is unchanged.
fn side_effects_cache_marker_prefix() -> String {
    format!(".{}-side-effects-cache", aube_util::prog())
}

/// True for a LEGACY in-package marker under ANY brand —
/// `.<tool>-side-effects-cache`. Markers now live beside the package rather
/// than inside it; the directory hash still skips the old spelling so a tree
/// stamped by an earlier build is not re-hashed over a file this cache wrote.
fn is_side_effects_marker_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with("-side-effects-cache")
}
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
    already_applied: bool,
    engine: String,
    input_hash: String,
    marker_path: std::path::PathBuf,
    path: std::path::PathBuf,
}

struct SideEffectsMarker {
    engine: String,
    input_hash: String,
    output_hash: String,
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
        let marker_path = side_effects_marker_path(package_dir, name)?;
        // `input_hash` fingerprints the package *before* its scripts run,
        // so it can never stand in for the engine. Reuse the virtual
        // store's own engine name rather than a second spelling of it, so
        // the two caches segregate on identical axes.
        let engine = match location.node_version {
            Some(v) => aube_lockfile::graph_hash::engine_name_default(v).0,
            None => aube_lockfile::graph_hash::platform_name(),
        };
        let current_hash = hash_dir_for_side_effects_cache(package_dir)?;
        // A marker naming a different engine cannot authorize the
        // already-applied skip: the tree it describes was built against
        // another Node ABI, so its `.node` files would fail at load time with
        // a `NODE_MODULE_VERSION` mismatch. Dropping it degrades to a restore
        // or a rebuild, never to a silent skip.
        let marker =
            read_valid_side_effects_marker(&marker_path).filter(|marker| marker.engine == engine);
        let already_applied = marker
            .as_ref()
            .is_some_and(|marker| marker.output_hash == current_hash);
        let input_hash = match marker {
            Some(marker) if already_applied || marker.input_hash == current_hash => {
                marker.input_hash
            }
            _ => current_hash,
        };
        let safe_name = name.replace('/', "__");
        Ok(Self {
            already_applied,
            path: location
                .root
                .join(format!("{safe_name}@{version}"))
                .join(&engine)
                .join(&input_hash),
            engine,
            input_hash,
            marker_path,
        })
    }

    pub(super) fn restore_if_available(
        &self,
        package_dir: &std::path::Path,
    ) -> miette::Result<SideEffectsCacheRestore> {
        // The installer-owned marker sits outside package content and records
        // both the pre-build input and post-build output hashes. A swept
        // reusable cache therefore does not invalidate an intact build, while
        // missing or modified generated output still forces restore/rebuild.
        if self.already_applied {
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
        // Build output is locally compiled, so on macOS it is ad-hoc
        // signed at best and Gatekeeper refuses it outright once
        // quarantined. Both restore modes carry the attribute in: a
        // hardlink shares the cache entry's inode and therefore its
        // xattrs, and the `fs::copy` fallback is `fcopyfile` with
        // `COPYFILE_ALL`. The copy also mints a new inode, which a
        // quarantine-enabled process has stamped whatever the source
        // was — so this belongs after the restore rather than once when
        // the entry is written. Walk-driven because this output was
        // never in a package index. No-op off macOS.
        aube_linker::strip_quarantine_from_tree(package_dir);
        self.write_marker(package_dir)?;
        tracing::debug!("side-effects-cache: restored {}", self.path.display());
        Ok(SideEffectsCacheRestore::Restored)
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
                self.write_marker(package_dir)?;
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
        self.write_marker(package_dir)?;

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

    fn write_marker(&self, package_dir: &std::path::Path) -> miette::Result<()> {
        let output_hash = hash_dir_for_side_effects_cache(package_dir)?;
        write_side_effects_marker(
            &self.marker_path,
            &self.engine,
            &self.input_hash,
            &output_hash,
        )
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
    let virtual_store_dir = store.virtual_store_dir();
    let virtual_store_root = if virtual_store_dir.file_name()
        == Some(std::ffi::OsStr::new(
            crate::commands::settings_context::GVS_REGISTRY_NAMESPACE_VERSION,
        )) {
        virtual_store_dir
            .parent()
            .unwrap_or(virtual_store_dir.as_path())
    } else {
        virtual_store_dir.as_path()
    };
    virtual_store_root
        .parent()
        .unwrap_or_else(|| store.root())
        .join("side-effects-v1")
}

fn side_effects_marker_path(
    package_dir: &std::path::Path,
    name: &str,
) -> miette::Result<std::path::PathBuf> {
    let parent = package_dir.parent().ok_or_else(|| {
        miette!(
            "package directory has no parent for side effects marker: {}",
            package_dir.display()
        )
    })?;
    let name_hash = sha2::Sha256::digest(name.as_bytes());
    Ok(parent.join(format!(
        "{}-{}",
        side_effects_cache_marker_prefix(),
        hex::encode(name_hash)
    )))
}

/// Parsed marker contents. `engine` is compared, never joined into a path —
/// the engine a lookup keys on comes from the install's own resolved Node.
/// Only `v2` is accepted: the engine-less `v1` form cannot rule out an
/// ABI mismatch, so it degrades to a restore or a rebuild.
fn read_valid_side_effects_marker(marker_path: &std::path::Path) -> Option<SideEffectsMarker> {
    let marker = std::fs::read_to_string(marker_path).ok()?;
    let mut lines = marker.lines();
    let version = lines.next()?;
    let engine = lines.next()?;
    let input_hash = lines.next()?;
    let output_hash = lines.next()?;
    if version != "v2"
        || engine.is_empty()
        || lines.next().is_some()
        || !is_side_effects_cache_hash(input_hash)
        || !is_side_effects_cache_hash(output_hash)
    {
        return None;
    }
    Some(SideEffectsMarker {
        engine: engine.to_owned(),
        input_hash: input_hash.to_ascii_lowercase(),
        output_hash: output_hash.to_ascii_lowercase(),
    })
}

fn is_side_effects_cache_hash(value: &str) -> bool {
    value.len() == 128 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn write_side_effects_marker(
    marker_path: &std::path::Path,
    engine: &str,
    input_hash: &str,
    output_hash: &str,
) -> miette::Result<()> {
    aube_util::fs_atomic::atomic_write(
        marker_path,
        format!("v2\n{engine}\n{input_hash}\n{output_hash}\n").as_bytes(),
    )
    .into_diagnostic()
    .wrap_err_with(|| {
        format!(
            "failed to write side effects cache marker {}",
            marker_path.display()
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
        // Skip a LEGACY in-package marker under ANY brand. A tree installed
        // by an earlier build carries one, and folding that file into the hash
        // would invalidate every cached entry exactly once, for a file this
        // cache wrote itself.
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_side_effects_marker_name)
        {
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
    fn cache_root_is_a_sibling_of_the_versioned_virtual_store_root() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let store = aube_store::Store::with_dirs(dir.path().join("store/files"), cache_dir.clone())
            .with_virtual_store_dir(cache_dir.join("virtual-store/v1"));

        assert_eq!(
            side_effects_cache_root(&store),
            cache_dir.join("side-effects-v1")
        );
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
        let marker_path = dir.path().join("marker");

        std::fs::write(&marker_path, "../../evil").unwrap();
        assert!(read_valid_side_effects_marker(&marker_path).is_none());

        std::fs::write(
            &marker_path,
            format!(
                "v2\ndarwin-arm64-node26\n{}\n{}\n",
                "z".repeat(128),
                "B".repeat(128)
            ),
        )
        .unwrap();
        assert!(
            read_valid_side_effects_marker(&marker_path).is_none(),
            "a non-hex hash must be rejected however it is prefixed"
        );

        std::fs::write(
            &marker_path,
            format!("v1\n{}\n{}\n", "A".repeat(128), "B".repeat(128)),
        )
        .unwrap();
        assert!(
            read_valid_side_effects_marker(&marker_path).is_none(),
            "an engine-less v1 marker cannot rule out an ABI mismatch, so it is not accepted"
        );

        std::fs::write(
            &marker_path,
            format!(
                "v2\ndarwin-arm64-node26\n{}\n{}\n",
                "A".repeat(128),
                "B".repeat(128)
            ),
        )
        .unwrap();
        let current = read_valid_side_effects_marker(&marker_path).unwrap();
        assert_eq!(current.engine, "darwin-arm64-node26");
        assert_eq!(current.input_hash, "a".repeat(128));
        assert_eq!(current.output_hash, "b".repeat(128));
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
        // Downgrade the marker to the engine-less form an older build wrote.
        std::fs::write(
            side_effects_marker_path(&pkg, "p").unwrap(),
            format!("v1\n{}\n{}\n", saved.input_hash, saved.input_hash),
        )
        .unwrap();

        let reread = entry(&root, &pkg, Some("26.5.0"));
        assert_eq!(
            reread.input_hash, saved.input_hash,
            "an engineless marker leaves the entry keyed on the directory's own hash"
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

    #[test]
    fn applied_marker_survives_reusable_cache_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let cache = dir.path().join("cache");

        let saved = entry(&cache, &pkg, Some("26.5.0"));
        std::fs::write(pkg.join("built.node"), "built").unwrap();
        saved.save(&pkg, false).unwrap();
        std::fs::remove_dir_all(&cache).unwrap();

        let reread = entry(&cache, &pkg, Some("26.5.0"));
        assert!(!reread.path.exists());
        assert!(matches!(
            reread.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::AlreadyApplied
        ));
    }

    #[test]
    fn stale_marker_does_not_skip_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let cache = dir.path().join("cache");

        let saved = entry(&cache, &pkg, Some("26.5.0"));
        std::fs::write(pkg.join("built.node"), "built").unwrap();
        saved.save(&pkg, false).unwrap();
        std::fs::remove_dir_all(&cache).unwrap();
        std::fs::remove_file(pkg.join("built.node")).unwrap();

        assert!(matches!(
            entry(&cache, &pkg, Some("26.5.0"))
                .restore_if_available(&pkg)
                .unwrap(),
            SideEffectsCacheRestore::Miss
        ));
    }

    #[test]
    fn changed_package_does_not_restore_stale_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let cache = dir.path().join("cache");

        let original = entry(&cache, &pkg, Some("26.5.0"));
        std::fs::write(pkg.join("built.node"), "old build").unwrap();
        original.save(&pkg, false).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            "{\"name\":\"p\",\"revision\":2}\n",
        )
        .unwrap();
        std::fs::remove_file(pkg.join("built.node")).unwrap();

        let changed = entry(&cache, &pkg, Some("26.5.0"));
        assert_ne!(changed.path, original.path);
        assert!(matches!(
            changed.restore_if_available(&pkg).unwrap(),
            SideEffectsCacheRestore::Miss
        ));
    }

    #[test]
    fn package_supplied_marker_is_not_installer_state() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        // A tarball that ships the LEGACY in-package marker name must not be
        // able to claim its own build is already applied.
        std::fs::write(
            pkg.join(side_effects_cache_marker_prefix()),
            format!("v2\nany-engine\n{}\n{}\n", "a".repeat(128), "b".repeat(128)),
        )
        .unwrap();

        assert!(matches!(
            entry(dir.path(), &pkg, Some("26.5.0"))
                .restore_if_available(&pkg)
                .unwrap(),
            SideEffectsCacheRestore::Miss
        ));
    }
}
