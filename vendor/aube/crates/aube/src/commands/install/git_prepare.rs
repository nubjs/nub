use miette::{Context, IntoDiagnostic, miette};

use super::{FrozenMode, InstallOptions, run};

/// Unique-per-call scratch directory that `rm -rf`s itself on drop.
/// Used to run a git dep's `prepare` script without mutating the
/// shared `git_shallow_clone` cache under `<cache>/git/<tool>-git-*`.
pub(super) struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    pub(super) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Recursively copy `src` into a fresh temp directory and return it
/// wrapped in a [`ScratchDir`]. A top-level `.git` is skipped —
/// prepare scripts never need the history, and dropping it keeps the
/// copy an order of magnitude smaller on large repos.
///
/// The copy is in-process rather than a `cp -a` subprocess, which
/// stock Windows does not have. It keeps what `cp -a` kept that a
/// prepare script can observe: symlinks stay symlinks, permission
/// bits (the exec bit above all) survive through `fs::copy`, and file
/// modification times are carried over best-effort.
pub(super) fn prepare_scratch_copy(
    src: &std::path::Path,
    spec: &str,
) -> miette::Result<ScratchDir> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    src.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    let dst = std::env::temp_dir().join(format!(
        "{}-git-prep-{:x}",
        aube_util::prog(),
        hasher.finish()
    ));
    if dst.exists() {
        let _ = std::fs::remove_dir_all(&dst);
    }
    std::fs::create_dir_all(&dst)
        .map_err(|e| miette!("git dep {spec}: create scratch dir {}: {e}", dst.display()))?;

    // Owning the directory before the fallible copy means a failure
    // part-way through still removes the partial tree on drop instead
    // of leaking it under `<tmp>/<tool>-git-prep-*`.
    let scratch = ScratchDir(dst);
    copy_checkout(src, scratch.path(), true)
        .map_err(|e| miette!("git dep {spec}: scratch copy failed: {e}"))?;
    Ok(scratch)
}

fn copy_checkout(
    src: &std::path::Path,
    dst: &std::path::Path,
    top_level: bool,
) -> std::io::Result<()> {
    let with_path = |path: &std::path::Path, e: std::io::Error| {
        std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    };
    for entry in std::fs::read_dir(src).map_err(|e| with_path(src, e))? {
        let entry = entry.map_err(|e| with_path(src, e))?;
        let name = entry.file_name();
        if top_level && name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry.file_type().map_err(|e| with_path(&from, e))?;
        if file_type.is_symlink() {
            copy_symlink(&from, &to, file_type).map_err(|e| with_path(&from, e))?;
        } else if file_type.is_dir() {
            std::fs::create_dir(&to).map_err(|e| with_path(&to, e))?;
            copy_checkout(&from, &to, false)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|e| with_path(&from, e))?;
            // Times are fidelity, not correctness: a build that compares
            // them still works from fresh ones, so a refusal (a read-only
            // file, a filesystem without the call) does not fail the copy.
            if let Ok(meta) = entry.metadata()
                && let Ok(file) = std::fs::OpenOptions::new().write(true).open(&to)
            {
                let mut times = std::fs::FileTimes::new();
                if let Ok(modified) = meta.modified() {
                    times = times.set_modified(modified);
                }
                if let Ok(accessed) = meta.accessed() {
                    times = times.set_accessed(accessed);
                }
                let _ = file.set_times(times);
            }
        }
        // Git stores no other file type, so a FIFO or socket here is
        // stray local state; skipping it also keeps `fs::copy` from
        // blocking on a FIFO's read end.
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(
    src: &std::path::Path,
    dst: &std::path::Path,
    _file_type: std::fs::FileType,
) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
}

#[cfg(windows)]
fn copy_symlink(
    src: &std::path::Path,
    dst: &std::path::Path,
    file_type: std::fs::FileType,
) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    let target = std::fs::read_link(src)?;
    if file_type.is_symlink_dir() {
        return aube_linker::create_dir_link(&target, dst);
    }
    // A file symlink needs Developer Mode or an elevated shell. Without
    // either, copy what it points at — what Git for Windows' `cp`
    // produced, so the users who had one on PATH see no change.
    match std::os::windows::fs::symlink_file(&target, dst) {
        Ok(()) => Ok(()),
        Err(e) if std::fs::metadata(src).is_ok_and(|m| m.is_file()) => {
            tracing::debug!(
                "git prepare: symlink {} unavailable ({e}); copying its target",
                dst.display()
            );
            std::fs::copy(src, dst).map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// Hard cap for nested git dep `prepare` installs. Four levels is more
/// than any real-world chain we've seen and prevents a pathological repo
/// from wedging install in an infinite clone loop.
const GIT_PREPARE_MAX_DEPTH: u32 = 4;

/// Run a nested `aube install` inside a git-dep checkout so its
/// devDependencies are linked and its root `prepare` script runs
/// before the caller snapshots the tree via `aube pack`.
///
/// `ignore_scripts` is forwarded from the outer install so a user
/// who passed `--ignore-scripts` for security/reproducibility
/// reasons doesn't have the git dep's full root lifecycle sequence
/// execute regardless — the caller is expected to *skip* calling
/// this function entirely under `--ignore-scripts`, but we still
/// forward the flag as a belt-and-suspenders defense in case a
/// nested install reaches this path through some other code path.
pub(super) async fn run_git_dep_prepare(
    clone_dir: &std::path::Path,
    spec: &str,
    ignore_scripts: bool,
    depth: u32,
    inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
) -> miette::Result<()> {
    if depth >= GIT_PREPARE_MAX_DEPTH {
        return Err(miette!(
            "git dep {spec}: `prepare` nesting exceeded {GIT_PREPARE_MAX_DEPTH} levels"
        ));
    }
    let mut opts = InstallOptions::with_mode(super::super::chained_frozen_mode(FrozenMode::Prefer));
    opts.project_dir = Some(clone_dir.to_path_buf());
    opts.ignore_scripts = ignore_scripts;
    opts.git_prepare_depth = depth + 1;
    opts.inherited_build_policy = inherited_build_policy;
    // Override the chained-call default: this nested install's "root" IS
    // the git dep itself, and running its `prepare` (plus
    // pre/post-install) is the entire point of git-dep preparation.
    // Treat this as if it were an argumentless `aube install` against the
    // dep's clone directory.
    opts.skip_root_lifecycle = false;
    opts.run_dev_preinstall = true;
    let spec = spec.to_string();
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .into_diagnostic()
            .wrap_err("failed to build nested git prepare runtime")?;
        runtime.block_on(run(opts))
    })
    .await
    .into_diagnostic()
    .wrap_err_with(|| format!("git dep {spec}: nested install task failed"))?
    .wrap_err_with(|| format!("git dep {spec}: nested install for `prepare` failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_copy_keeps_the_checkout_but_not_its_history() {
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".npmrc"), "x=1\n").unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/nested/a.js"), "a\n").unwrap();
        std::fs::write(root.join("build.sh"), "#!/bin/sh\n").unwrap();
        let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(root.join("src/nested/a.js"))
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("build.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::os::unix::fs::symlink("build.sh", root.join("alias.sh")).unwrap();
        }
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("build.sh", root.join("alias.sh")).unwrap();

        let scratch = prepare_scratch_copy(root, "test").unwrap();
        let dst = scratch.path();

        assert!(
            !dst.join(".git").exists(),
            "the clone's history is not copied"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".npmrc")).unwrap(),
            "x=1\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("src/nested/a.js")).unwrap(),
            "a\n"
        );
        assert_eq!(
            std::fs::metadata(dst.join("src/nested/a.js"))
                .unwrap()
                .modified()
                .unwrap(),
            mtime,
            "modification time carries over"
        );
        assert_eq!(
            std::fs::read_link(dst.join("alias.sh")).unwrap(),
            std::path::Path::new("build.sh"),
            "a symlink is recreated as a symlink, target unchanged"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dst.join("build.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "the exec bit survives");
        }

        let copied = dst.to_path_buf();
        drop(scratch);
        assert!(!copied.exists(), "the scratch dir removes itself on drop");
    }
}
