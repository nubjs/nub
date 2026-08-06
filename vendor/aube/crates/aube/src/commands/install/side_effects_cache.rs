use miette::{Context, IntoDiagnostic, miette};
use sha2::Digest;

/// The per-package marker the side-effects cache stamps. Brand-scoped to the
/// active embedder because this file lands inside the global CAS store AND
/// inside a consumer's `node_modules` — a hardcoded `aube` leaf puts the
/// engine's brand in an embedder's user-visible tree. `prog()` is `"aube"`
/// under the default profile, so standalone aube's on-disk name is unchanged.
fn side_effects_cache_marker() -> String {
    format!(".{}-side-effects-cache", aube_util::prog())
}

/// True for this cache's marker under ANY brand — `.<tool>-side-effects-cache`.
/// Used by the directory hash so a marker left by a differently-branded build
/// is excluded from the digest rather than changing it.
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

/// Whether the lifecycle spawn this entry describes runs confined.
///
/// Part of the cache key because it decides WHERE a build's writes outside its own
/// package tree land, and the entry captures only the tree. Confined, `$HOME` is a
/// private per-package directory; unconfined it is the user's own, which is
/// machine-wide and therefore still there on the next install — so an unconfined
/// entry replays soundly and a confined one does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confinement {
    Confined,
    Unconfined,
}

impl Confinement {
    fn id(self) -> &'static str {
        match self {
            Self::Confined => "confined",
            Self::Unconfined => "unconfined",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SideEffectsCacheEntry {
    /// Everything about the build environment an entry is only valid under:
    /// the Node engine that compiled its addons, the shell that ran the
    /// build, and whether it ran confined. One string because all three are
    /// the same kind of fact — "this output is only meaningful under X" — and
    /// all must appear in the path AND the marker or a mismatched directory
    /// gets skipped instead of rebuilt.
    build_env: String,
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
        confinement: Confinement,
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
        // The shell belongs in the key because a build run under a different
        // one can be WRONG, not merely stale: `cmd.exe` exits 0 while writing
        // an unexpanded `${VAR:-default}` literally, so a Windows tree cached
        // under cmd.exe must be rebuilt — not restored — once the lifecycle
        // shell becomes POSIX `sh`. Read from the same `ScriptSettings` the
        // spawn resolves, so a user `script-shell` participates too.
        //
        // Confinement joins them for the same reason one step out: a script
        // that writes OUTSIDE its own package tree — a browser into
        // `$HOME/Library/Caches`, the shape `cypress`/`puppeteer`/`*driver`
        // all have — puts that artifact somewhere the entry does not capture,
        // and confinement decides where. Under a jail it is a private
        // per-package HOME; unconfined it is the user's own. Restoring a
        // jail-built entry into an unconfined install therefore SKIPS the
        // script and lands the artifact NOWHERE, which made the per-package
        // opt-out (`dependenciesMeta.<pkg>.sandbox: false`) non-functional on
        // any machine that had once installed the package jailed — the miss
        // that healed it was purging this directory by hand. Naming it here
        // makes the two modes different entries instead of one poisoned one,
        // and busts every entry written before this existed, which is what
        // heals an already-poisoned machine without the user knowing to.
        let build_env = format!(
            "{engine}-{}-{}",
            aube_scripts::resolved_shell_id(),
            confinement.id()
        );
        Ok(Self {
            path: location
                .root
                .join(format!("{safe_name}@{version}"))
                .join(&build_env)
                .join(&input_hash),
            build_env,
            input_hash,
        })
    }

    /// `mode` is the caller's, not this type's: a hardlinked restore shares the cache
    /// entry's inode, which a Windows build jail cannot read through (see
    /// `lifecycle::jail_forces_copy`), so the confinement-aware caller picks.
    pub(super) fn restore_if_available(
        &self,
        package_dir: &std::path::Path,
        mode: CopyMode,
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
        copy_dir(&self.path, package_dir, mode).wrap_err_with(|| {
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
        // The copy carries the entry's own marker across, so restamping is
        // a no-op except for an entry saved before markers named an engine
        // — that one would fail every future match and re-copy forever.
        write_side_effects_marker(package_dir, &self.build_env, &self.input_hash)?;
        tracing::debug!("side-effects-cache: restored {}", self.path.display());
        Ok(SideEffectsCacheRestore::Restored)
    }

    /// True when this package directory's contents were produced by *this*
    /// entry. Both halves are load-bearing: entries segregate by build
    /// environment, so several now share one input hash, and matching on the
    /// hash alone would let the skip above fire for a build made under a
    /// different Node ABI (a runtime `NODE_MODULE_VERSION` failure), a
    /// different shell (bytes the shell mis-expanded), or a different
    /// confinement (out-of-tree writes that went to a private HOME). A marker naming a
    /// different — or no — build environment never matches, so it degrades to
    /// a restore or a rebuild, never to a silent skip.
    fn marker_matches(&self, package_dir: &std::path::Path) -> bool {
        read_valid_side_effects_marker(package_dir).is_some_and(|marker| {
            marker.build_env.as_deref() == Some(self.build_env.as_str())
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
                write_side_effects_marker(package_dir, &self.build_env, &self.input_hash)?;
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
        write_side_effects_marker(package_dir, &self.build_env, &self.input_hash)?;

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

/// Parsed marker contents: `<build_env>:<input_hash>`. `build_env` is `None`
/// for the bare-hash form written before the build environment was recorded;
/// a marker from an older build that recorded only the engine simply names a
/// different build environment, which is already a non-match.
struct SideEffectsMarker {
    build_env: Option<String>,
    input_hash: String,
}

/// Only the hash half is validated, because only the hash is ever joined
/// into a path — the build environment a lookup keys on comes from the
/// install's own resolved Node and shell, and the marker's copy is compared,
/// never trusted as a path segment.
fn read_valid_side_effects_marker(package_dir: &std::path::Path) -> Option<SideEffectsMarker> {
    let marker = std::fs::read_to_string(package_dir.join(side_effects_cache_marker())).ok()?;
    let marker = marker.trim();
    let (build_env, hash) = match marker.rsplit_once(':') {
        Some((build_env, hash)) => (Some(build_env), hash),
        None => (None, marker),
    };
    is_side_effects_cache_hash(hash).then(|| SideEffectsMarker {
        build_env: build_env.map(str::to_owned),
        input_hash: hash.to_ascii_lowercase(),
    })
}

fn is_side_effects_cache_hash(value: &str) -> bool {
    value.len() == 128 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn write_side_effects_marker(
    package_dir: &std::path::Path,
    build_env: &str,
    input_hash: &str,
) -> miette::Result<()> {
    aube_util::fs_atomic::atomic_write(
        &package_dir.join(side_effects_cache_marker()),
        format!("{build_env}:{input_hash}").as_bytes(),
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
        // Skip ANY brand's marker, not just the active one. A tree installed by
        // an earlier build carries the previous spelling, and folding that file
        // into the hash would invalidate every cached entry exactly once, for a
        // file this cache wrote itself.
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
        entry_confined(root, package_dir, node_version, Confinement::Unconfined)
    }

    fn entry_confined(
        root: &std::path::Path,
        package_dir: &std::path::Path,
        node_version: Option<&str>,
        confinement: Confinement,
    ) -> SideEffectsCacheEntry {
        SideEffectsCacheEntry::new(
            SideEffectsCacheLocation { root, node_version },
            "p",
            "1.0.0",
            package_dir,
            confinement,
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

    /// Output built under one shell can be WRONG under another, not merely
    /// stale — `cmd.exe` exits 0 having written `${VAR:-default}` literally —
    /// so an entry must not be restorable across a shell change. Asserted
    /// against the resolved id rather than by flipping shells: `ScriptSettings`
    /// is process-global outside an install scope, and mutating it here would
    /// leak into the sibling tests. `aube-scripts` owns the id's own coverage.
    #[test]
    fn cache_path_segregates_by_shell() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let path = entry_path(dir.path(), &pkg, Some("22.15.0"));
        let build_env = path
            .parent()
            .and_then(|p| p.file_name())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let shell = aube_scripts::resolved_shell_id();
        assert!(
            build_env.contains(&format!("-{shell}-")),
            "build-env segment {build_env} does not name the resolved shell {shell}"
        );
    }

    /// A build's writes OUTSIDE its own package tree — the `cypress`/`puppeteer` shape,
    /// a browser into `$HOME` — are not in the entry, and confinement decides where they
    /// went: a private per-package HOME, or the user's own. So a jail-built entry must
    /// not be restorable into an unconfined install, which would skip the script and
    /// land that artifact nowhere at all, making
    /// `dependenciesMeta.<pkg>.sandbox: false` non-functional.
    #[test]
    fn cache_path_segregates_by_confinement() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = package_fixture(dir.path());
        let root = dir.path().join("cache");

        let confined = entry_confined(&root, &pkg, Some("26.5.0"), Confinement::Confined);
        let unconfined = entry_confined(&root, &pkg, Some("26.5.0"), Confinement::Unconfined);
        assert_ne!(
            confined.path, unconfined.path,
            "a jail-built entry must not be reachable from an unconfined install"
        );

        confined.save(&pkg, false).unwrap();
        assert!(
            matches!(
                unconfined
                    .restore_if_available(&pkg, CopyMode::HardlinkOrCopy)
                    .unwrap(),
                SideEffectsCacheRestore::Miss
            ),
            "the opt-out must miss the jail-built entry and rebuild"
        );
        // The marker the confined save stamped names the confined build env, so the skip
        // cannot fire off it either — the path alone would still let `AlreadyApplied`
        // return for a directory the other mode built.
        assert!(
            !unconfined.marker_matches(&pkg),
            "a confined marker must not satisfy an unconfined entry"
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
        let marker_path = dir.path().join(side_effects_cache_marker());

        std::fs::write(&marker_path, "../../evil").unwrap();
        assert!(read_valid_side_effects_marker(dir.path()).is_none());

        std::fs::write(
            &marker_path,
            format!("darwin-arm64-node26-sh-confined:{}", "z".repeat(128)),
        )
        .unwrap();
        assert!(
            read_valid_side_effects_marker(dir.path()).is_none(),
            "a non-hex hash must be rejected however it is prefixed"
        );

        std::fs::write(&marker_path, format!("{}\n", "A".repeat(128))).unwrap();
        let legacy = read_valid_side_effects_marker(dir.path()).unwrap();
        assert_eq!(legacy.build_env, None);
        assert_eq!(legacy.input_hash, "a".repeat(128));

        std::fs::write(
            &marker_path,
            format!("darwin-arm64-node26-sh-confined:{}\n", "A".repeat(128)),
        )
        .unwrap();
        let current = read_valid_side_effects_marker(dir.path()).unwrap();
        assert_eq!(
            current.build_env.as_deref(),
            Some("darwin-arm64-node26-sh-confined")
        );
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
                    .restore_if_available(&pkg, CopyMode::HardlinkOrCopy)
                    .unwrap(),
                SideEffectsCacheRestore::AlreadyApplied
            ),
            "the same engine that built this directory must still skip the rebuild"
        );

        let node22 = entry(&root, &pkg, Some("22.15.0"));
        assert!(
            matches!(
                node22
                    .restore_if_available(&pkg, CopyMode::HardlinkOrCopy)
                    .unwrap(),
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
                    .restore_if_available(&pkg, CopyMode::HardlinkOrCopy)
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
        std::fs::write(pkg.join(side_effects_cache_marker()), &saved.input_hash).unwrap();

        let reread = entry(&root, &pkg, Some("26.5.0"));
        assert_eq!(
            reread.input_hash, saved.input_hash,
            "an engineless marker must still supply the input hash, so the entry keeps its key"
        );
        assert!(
            matches!(
                reread
                    .restore_if_available(&pkg, CopyMode::HardlinkOrCopy)
                    .unwrap(),
                SideEffectsCacheRestore::Restored
            ),
            "an engineless marker must degrade to a restore, never to a skip"
        );
        assert!(
            matches!(
                entry(&root, &pkg, Some("26.5.0"))
                    .restore_if_available(&pkg, CopyMode::HardlinkOrCopy)
                    .unwrap(),
                SideEffectsCacheRestore::AlreadyApplied
            ),
            "the restore restamps the marker, so the skip returns on the next install"
        );
    }
}
