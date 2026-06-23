use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};

use miette::miette;

/// Process-wide guard: `true` while a project lock is held by this process.
/// Nested commands (e.g. `add` calling `install`) observe this and skip
/// re-acquiring so they don't deadlock against themselves.
static LOCK_HELD: AtomicBool = AtomicBool::new(false);

/// Whether the project-level advisory lock is disabled. Resolves the
/// `aubeNoLock` setting through the full file-source chain so
/// `.npmrc`, `~/.config/aube/config.toml`, project
/// `.config/aube/config.toml`, and `aube-workspace.yaml` entries
/// participate alongside the canonical `AUBE_NO_LOCK` env var.
fn aube_no_lock_enabled(cwd: &std::path::Path) -> bool {
    super::with_settings_ctx(cwd, aube_settings::resolved::aube_no_lock)
}

/// Opaque guard holding a project-level advisory lock. Dropping it releases
/// the lock and clears the process-wide `LOCK_HELD` flag. Commands bind
/// this to a `_lock` variable at the top of `run` so the lock is held for
/// the duration of the command.
///
/// The `_inner` field holds an erased `fslock::LockFile` (via `dyn Any`)
/// so callers don't have to take a direct dep on `fslock` to name the
/// type — the lock is released on drop regardless.
pub(crate) struct ProjectLock {
    _inner: Option<Box<dyn Any + Send>>,
    owns_flag: bool,
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        if self.owns_flag {
            LOCK_HELD.store(false, Ordering::Release);
        }
    }
}

/// Take an advisory lock on the current project's `node_modules/`.
///
/// The lock is keyed off the canonical path of `node_modules` (hashed into
/// `$TMPDIR/fslock/`), so multiple `aube` invocations against the same
/// project — even via different relative paths or symlinks — serialize
/// correctly.
///
/// Returns a no-op guard when `AUBE_NO_LOCK` is active or when this
/// process already holds the project lock (re-entrant case for
/// `add` → `install`), so callers don't need to special-case.
pub(crate) fn take_project_lock(cwd: &std::path::Path) -> miette::Result<ProjectLock> {
    if aube_no_lock_enabled(cwd) {
        return Ok(ProjectLock {
            _inner: None,
            owns_flag: false,
        });
    }

    // Re-entrant: if this process already holds the lock (outer command
    // chained into an inner one like add → install), skip re-acquisition.
    if LOCK_HELD.load(Ordering::Acquire) {
        return Ok(ProjectLock {
            _inner: None,
            owns_flag: false,
        });
    }

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

    // Only mark the flag as held AFTER the OS lock is in hand, so a nested
    // call can't observe `LOCK_HELD = true` and get a no-op guard before
    // this process actually owns the underlying advisory lock.
    LOCK_HELD.store(true, Ordering::Release);

    Ok(ProjectLock {
        _inner: Some(Box::new(lock)),
        owns_flag: true,
    })
}
