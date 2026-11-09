use miette::{Context, IntoDiagnostic};
use std::path::{Path, PathBuf};

/// The project files an `add` may rewrite, as they were before it started.
///
/// A failed add puts them back, as pnpm leaves them untouched, and `--no-save`
/// puts them back after a successful one once the install pipeline (which reads
/// from disk) has linked `node_modules`. A file that did not exist is recorded
/// as `None`, so restoring removes whatever the add created.
#[derive(Default)]
pub(super) struct Snapshot {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

pub(super) struct Restored {
    pub(super) changed: bool,
    pub(super) errors: Vec<miette::Report>,
}

impl Snapshot {
    /// Capture what an add writes for the project at `dir`: its manifest, its
    /// lockfile in every name, and the workspace yaml that `--allow-build`,
    /// `--deny-build` and `--save-catalog` write to. The pre-rename legacy
    /// lockfiles matter because a write MIGRATES them (writes `nub.lock`, deletes
    /// `lock.yaml`), which would otherwise lose the checked-in file.
    pub(super) fn capture_project(&mut self, dir: &Path) -> miette::Result<()> {
        self.capture_manifest(dir)?;
        let lockfile_path = lockfile_path_for_project(dir)?;
        self.capture(lockfile_path.clone())?;
        for name in aube_util::embedder().lockfile_legacy_basenames {
            let path = dir.join(name);
            if path != lockfile_path {
                self.capture(path)?;
            }
        }
        self.capture(aube_manifest::workspace::workspace_yaml_target(dir))
    }

    pub(super) fn capture_manifest(&mut self, dir: &Path) -> miette::Result<()> {
        self.capture(dir.join("package.json"))
    }

    fn capture(&mut self, path: PathBuf) -> miette::Result<()> {
        let bytes = read_if_present(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to snapshot {}", path.display()))?;
        self.files.push((path, bytes));
        Ok(())
    }

    /// Put every captured file back, attempting each one so a single unwritable
    /// file cannot strand the rest. Files that still match are left alone.
    pub(super) fn restore(&self) -> Restored {
        let mut restored = Restored {
            changed: false,
            errors: Vec::new(),
        };
        for (path, original) in &self.files {
            let result = read_if_present(path).and_then(|current| {
                if current == *original {
                    return Ok(false);
                }
                match original {
                    Some(bytes) => aube_util::fs_atomic::atomic_write(path, bytes)?,
                    None => std::fs::remove_file(path)?,
                }
                Ok(true)
            });
            match result {
                Ok(changed) => restored.changed |= changed,
                Err(e) => restored.errors.push(
                    Err::<(), _>(e)
                        .into_diagnostic()
                        .wrap_err_with(|| format!("failed to restore {}", path.display()))
                        .unwrap_err(),
                ),
            }
        }
        restored
    }
}

/// The add's own error is what the user needs to see, so a file that could not
/// be put back after it is reported beside that error rather than replacing it.
pub(super) fn warn_restore_failures(errors: &[miette::Report]) {
    for error in errors {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_ADD_ROLLBACK_FAILED,
            "{error:#}"
        );
    }
}

fn read_if_present(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Resolve the on-disk lockfile path that a normal `add` would write
/// to in `project_dir`. Mirrors the `LockfileKind` -> filename mapping
/// inside `aube_lockfile::write_lockfile_as` so the snapshot lines up
/// byte-for-byte with the normal install write, including non-aube
/// lockfiles (`pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`,
/// `bun.lock`, `npm-shrinkwrap.json`). When no lockfile exists yet the
/// resolver uses the `package.json`-declared package manager's format,
/// falling back to the configured default. Fallible so a declaration
/// mismatch surfaces here rather than collapsing to the default.
fn lockfile_path_for_project(project_dir: &Path) -> miette::Result<PathBuf> {
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
