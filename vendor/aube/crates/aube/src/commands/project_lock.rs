use std::any::Any;

use miette::miette;

/// Whether the project-level advisory lock is disabled. Resolves the
/// `aubeNoLock` setting through the full file-source chain so
/// `.npmrc`, `~/.config/aube/config.toml`, project
/// `.config/aube/config.toml`, and `aube-workspace.yaml` entries
/// participate alongside the canonical `AUBE_NO_LOCK` env var.
fn aube_no_lock_enabled(cwd: &std::path::Path) -> bool {
    super::with_settings_ctx(cwd, aube_settings::resolved::aube_no_lock)
}

/// Opaque guard holding a project-level advisory lock. Commands bind this to a
/// local variable for the duration of the operation. Commands that chain into
/// install pass a reference to the guard explicitly, so unrelated concurrent
/// operations never mistake another project's lock for their own.
///
/// The `_inner` field holds an erased `fslock::LockFile` (via `dyn Any`)
/// so callers don't have to take a direct dep on `fslock` to name the
/// type — the lock is released on drop regardless.
pub(crate) struct ProjectLock {
    project_dir: std::path::PathBuf,
    _inner: Option<Box<dyn Any + Send + Sync>>,
}

impl ProjectLock {
    pub(crate) fn project_dir(&self) -> &std::path::Path {
        &self.project_dir
    }
}

/// Take an advisory lock on the current project's `node_modules/`.
///
/// The lock is keyed off the canonical path of `node_modules` (hashed into
/// `$TMPDIR/fslock/`), so multiple `aube` invocations against the same
/// project — even via different relative paths or symlinks — serialize
/// correctly.
///
/// Returns a no-op guard when `AUBE_NO_LOCK` is active. Nested commands must
/// pass the returned guard into the inner operation rather than attempting to
/// acquire the same filesystem lock again.
pub(crate) fn take_project_lock(cwd: &std::path::Path) -> miette::Result<ProjectLock> {
    let project_dir = cwd
        .canonicalize()
        .or_else(|_| std::path::absolute(cwd))
        .unwrap_or_else(|_| cwd.to_path_buf());
    if aube_no_lock_enabled(cwd) {
        return Ok(ProjectLock {
            project_dir,
            _inner: None,
        });
    }

    take_project_lock_enabled(cwd, project_dir)
}

/// Lock the directory that a chained install will operate on. Outer commands
/// may mutate a workspace member manifest, but the install itself owns the
/// workspace-root lockfile and virtual store.
pub(crate) fn take_install_project_lock(
    project_dir: &std::path::Path,
) -> miette::Result<ProjectLock> {
    let install_dir =
        crate::dirs::find_workspace_root(project_dir).unwrap_or_else(|| project_dir.to_path_buf());
    take_project_lock(&install_dir)
}

fn take_project_lock_enabled(
    cwd: &std::path::Path,
    project_dir: std::path::PathBuf,
) -> miette::Result<ProjectLock> {
    let nm_path = super::project_modules_dir(cwd);
    let lock = xx::fslock::FSLock::new(&nm_path)
        .with_callback(|_| {
            // Raw, uncaptured stderr write fired by `xx::fslock` when the
            // lock is contended, so route the process name through the
            // embedder profile. Standalone aube → "aube".
            eprintln!(
                "Waiting for another {} process to finish in this project...",
                aube_util::embedder().name
            );
        })
        .lock()
        .map_err(|e| miette!("failed to acquire project lock: {e}"))?;

    Ok(ProjectLock {
        project_dir,
        _inner: Some(Box::new(lock)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_can_cross_async_worker_threads_by_reference() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProjectLock>();
    }

    #[test]
    fn different_projects_hold_independent_process_locks() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let first_lock =
            take_project_lock_enabled(first.path(), first.path().to_path_buf()).unwrap();
        let second_lock =
            take_project_lock_enabled(second.path(), second.path().to_path_buf()).unwrap();

        assert!(first_lock._inner.is_some());
        assert!(second_lock._inner.is_some());
        assert_eq!(first_lock.project_dir(), first.path());
        assert_eq!(second_lock.project_dir(), second.path());
    }

    #[test]
    fn chained_install_lock_targets_workspace_root() {
        let workspace = tempfile::tempdir().unwrap();
        let member = workspace.path().join("packages/app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            workspace.path().join("package.json"),
            "{\"workspaces\":[\"packages/*\"]}\n",
        )
        .unwrap();
        std::fs::write(member.join("package.json"), "{}\n").unwrap();

        let lock = take_install_project_lock(&member).unwrap();

        assert_eq!(lock.project_dir(), workspace.path().canonicalize().unwrap());
    }
}
