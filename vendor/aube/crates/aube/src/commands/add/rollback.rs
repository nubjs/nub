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
    /// lockfile, and the workspace yaml that `--allow-build`, `--deny-build` and
    /// `--save-catalog` write to.
    pub(super) fn capture_project(&mut self, dir: &Path) -> miette::Result<()> {
        self.capture_manifest(dir)?;
        self.capture_lockfiles(dir)?;
        self.capture(aube_manifest::workspace::workspace_yaml_target(dir))
    }

    /// [`Self::capture_project`] for the directory the install runs in, plus
    /// the lockfiles that install writes elsewhere: a relocated `lockfile-dir`,
    /// and every member's own under `sharedWorkspaceLockfile=false`.
    pub(super) fn capture_install_root(&mut self, root: &Path) -> miette::Result<()> {
        self.capture_project(root)?;
        let (lockfile_dir, shared) = crate::commands::with_settings_ctx(root, |ctx| {
            (
                aube_settings::resolved::lockfile_dir(ctx),
                aube_settings::resolved::shared_workspace_lockfile(ctx),
            )
        });
        if let Some(dir) = lockfile_dir {
            self.capture_lockfiles(&root.join(dir))?;
        }
        if !shared {
            let members = aube_workspace::find_workspace_packages(root)
                .map_err(|e| miette::miette!("failed to discover workspace packages: {e}"))?;
            for member in members {
                self.capture_lockfiles(&member)?;
            }
        }
        Ok(())
    }

    /// Every lockfile name `dir` could hold, so the snapshot does not depend on
    /// predicting which format the install picks. The pre-rename legacy names
    /// matter because a write MIGRATES them (writes `nub.lock`, deletes
    /// `lock.yaml`), which would otherwise lose the checked-in file.
    fn capture_lockfiles(&mut self, dir: &Path) -> miette::Result<()> {
        use aube_lockfile::LockfileKind;
        let branch_aware = [
            aube_lockfile::aube_lock_filename(dir),
            aube_lockfile::pnpm_lock_filename(dir),
        ];
        let fixed = [
            LockfileKind::Npm,
            LockfileKind::NpmShrinkwrap,
            LockfileKind::Yarn,
            LockfileKind::Bun,
        ]
        .map(LockfileKind::filename);
        let legacy = aube_util::embedder().lockfile_legacy_basenames;
        for name in branch_aware
            .iter()
            .map(String::as_str)
            .chain(fixed)
            .chain(legacy.iter().copied())
        {
            self.capture(dir.join(name))?;
        }
        Ok(())
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
