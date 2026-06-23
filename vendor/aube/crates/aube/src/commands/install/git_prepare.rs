use miette::{Context, IntoDiagnostic, miette};

use super::{FrozenMode, InstallOptions, run};

/// Unique-per-call scratch directory that `rm -rf`s itself on drop.
/// Used to run a git dep's `prepare` script without mutating the
/// shared `git_shallow_clone` cache under `/tmp/aube-git-*`.
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
/// copy an order of magnitude smaller on large repos. Uses `cp -a`
/// so symlinks + file modes survive (matters for repos that ship
/// executable bits their prepare script relies on).
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
    let dst = std::env::temp_dir().join(format!("aube-git-prep-{:x}", hasher.finish()));
    if dst.exists() {
        let _ = std::fs::remove_dir_all(&dst);
    }
    std::fs::create_dir_all(&dst)
        .map_err(|e| miette!("git dep {spec}: create scratch dir {}: {e}", dst.display()))?;

    // Wrap the directory in `ScratchDir` *before* running any of
    // the fallible work below. Handing ownership of cleanup to
    // the Drop impl immediately means a failure to spawn `cp`, a
    // non-zero cp exit, or any panic between here and the `Ok`
    // return still removes the partially-populated temp dir
    // instead of leaking it under `/tmp/aube-git-prep-*`.
    let scratch = ScratchDir(dst);

    // `cp -a src/. dst/` — the trailing `/.` copies src's contents
    // (including dotfiles) into dst rather than creating `dst/<src>`.
    // `-a` preserves perms/symlinks/timestamps. We exclude `.git`
    // manually afterwards rather than with `--exclude` (non-POSIX,
    // GNU-only).
    let out = std::process::Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", src.display()))
        .arg(scratch.path())
        .output()
        .map_err(|e| miette!("git dep {spec}: spawn cp for scratch copy: {e}"))?;
    if !out.status.success() {
        return Err(miette!(
            "git dep {spec}: scratch copy failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = std::fs::remove_dir_all(scratch.path().join(".git"));

    Ok(scratch)
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
