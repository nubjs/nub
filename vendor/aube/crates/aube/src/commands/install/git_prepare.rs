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
/// wrapped in a [`ScratchDir`]. `.git/` is intentionally skipped —
/// prepare scripts never need the history, and dropping it keeps the
/// copy smaller on large repos. The shared native tree copier preserves
/// symlinks and file modes without requiring a Unix `cp` on Windows.
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

    // Own cleanup before any fallible copy work.
    let scratch = ScratchDir(dst);
    super::side_effects_cache::copy_dir(
        src,
        scratch.path(),
        super::side_effects_cache::CopyMode::Copy,
    )
    .wrap_err_with(|| format!("git dep {spec}: scratch copy failed"))?;
    let _ = std::fs::remove_dir_all(scratch.path().join(".git"));

    Ok(scratch)
}

/// Hard cap for nested git dep `prepare` installs. Four levels is more
/// than any real-world chain we've seen and prevents a pathological repo
/// from wedging install in an infinite clone loop.
const GIT_PREPARE_MAX_DEPTH: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_copy_preserves_dotfiles_and_does_not_share_file_writes() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join(".git")).unwrap();
        std::fs::write(
            source.path().join(".npmrc"),
            "registry=https://example.invalid\n",
        )
        .unwrap();
        let script = source.path().join("prepare.js");
        std::fs::write(&script, "original").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let scratch = prepare_scratch_copy(source.path(), "fixture").unwrap();
        let path = scratch.path().to_path_buf();
        assert!(!path.join(".git").exists());
        assert!(path.join(".npmrc").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path.join("prepare.js"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            std::os::unix::fs::symlink("prepare.js", source.path().join("link.js")).unwrap();
            let linked = prepare_scratch_copy(source.path(), "symlink fixture").unwrap();
            assert_eq!(
                std::fs::read_link(linked.path().join("link.js")).unwrap(),
                std::path::Path::new("prepare.js")
            );
        }
        std::fs::write(path.join("prepare.js"), "changed").unwrap();
        assert_eq!(std::fs::read_to_string(script).unwrap(), "original");
        drop(scratch);
        assert!(!path.exists());
    }
}

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
