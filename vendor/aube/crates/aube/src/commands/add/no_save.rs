use miette::{Context, IntoDiagnostic};
use std::path::{Path, PathBuf};

/// Bytes captured from disk before `aube add --no-save` mutated the
/// manifest and lockfile, used to put both back exactly as the user had
/// them once the install pipeline (which insists on reading from disk)
/// has finished linking `node_modules`.
pub(super) struct Snapshot {
    manifest_bytes: Vec<u8>,
    /// `None` means the lockfile didn't exist before the add — in that
    /// case the restore step deletes whatever the resolver wrote.
    lockfile_bytes: Option<Vec<u8>>,
}

/// Resolve the on-disk lockfile path that a normal `add` would write
/// to in `project_dir`. Mirrors the `LockfileKind` -> filename mapping
/// inside `aube_lockfile::write_lockfile_as` so the snapshot/restore
/// path under `--no-save` lines up byte-for-byte with whatever
/// `write_lockfile_preserving_existing` produces, including non-aube
/// lockfiles (`pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`,
/// `bun.lock`, `npm-shrinkwrap.json`). When no lockfile exists yet the
/// resolver uses the `package.json`-declared package manager's format,
/// falling back to aube's own.
pub(super) fn lockfile_path_for_project(project_dir: &Path) -> miette::Result<PathBuf> {
    use aube_lockfile::LockfileKind;
    let kind = crate::commands::resolve_lockfile_kind_for_write(project_dir)?
        .unwrap_or_else(|| crate::commands::default_lockfile_kind_for_cwd(project_dir));
    let filename = match kind {
        LockfileKind::Aube => aube_lockfile::aube_lock_filename(project_dir),
        LockfileKind::Pnpm => aube_lockfile::pnpm_lock_filename(project_dir),
        other => other.filename().to_string(),
    };
    Ok(project_dir.join(filename))
}

pub(super) fn snapshot_manifest_and_lockfile(
    manifest_path: &Path,
    lockfile_path: &Path,
) -> miette::Result<Snapshot> {
    let manifest_bytes = std::fs::read(manifest_path)
        .into_diagnostic()
        .wrap_err("failed to snapshot package.json for --no-save")?;
    let lockfile_bytes = snapshot_lockfile(lockfile_path)?;
    Ok(Snapshot {
        manifest_bytes,
        lockfile_bytes,
    })
}

pub(super) fn snapshot_lockfile(lockfile_path: &Path) -> miette::Result<Option<Vec<u8>>> {
    match std::fs::read(lockfile_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e)
            .into_diagnostic()
            .wrap_err("failed to snapshot lockfile for --no-save"),
    }
}

pub(super) fn restore_manifest_and_lockfile(
    snapshot: Snapshot,
    manifest_path: &Path,
    lockfile_path: &Path,
) -> Vec<miette::Report> {
    let mut errors = Vec::new();
    if let Err(e) = aube_util::fs_atomic::atomic_write(manifest_path, &snapshot.manifest_bytes) {
        errors.push(
            Result::<(), _>::Err(e)
                .into_diagnostic()
                .wrap_err("failed to restore original package.json after --no-save")
                .unwrap_err(),
        );
    }
    if let Err(e) = restore_lockfile(lockfile_path, &snapshot.lockfile_bytes) {
        errors.push(e);
    }
    errors
}

pub(super) fn restore_lockfile(
    lockfile_path: &Path,
    snapshot: &Option<Vec<u8>>,
) -> Result<(), miette::Report> {
    let result = match snapshot {
        Some(bytes) => aube_util::fs_atomic::atomic_write(lockfile_path, bytes),
        None => match std::fs::remove_file(lockfile_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
    };
    result
        .into_diagnostic()
        .wrap_err("failed to restore original lockfile after --no-save")
}
