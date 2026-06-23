//! Per-package staging for `aube deploy`: copy the source tree (or
//! pack's publish-selection) into the target dir, then hand the
//! bundling + manifest-rewrite work off to [`super::injection`] and
//! [`super::rewrite`]. One [`StagedDeploy`] per match is returned to
//! [`super::run`], which drives `aube install` against each in turn.
use super::DeployArgs;
use super::filtering::StripFields;
use super::injection::{materialize_injections, plan_injections};
use super::rewrite::{DeployRoot, rewrite_local_refs};
use crate::commands::CatalogMap;
use crate::commands::pack::collect_package_files;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Staged per-package state: copy and manifest rewrite are complete,
/// but `aube install` hasn't run yet in `target`.
pub(super) struct StagedDeploy {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) target: PathBuf,
    /// Whether staging bundled any local refs (workspace siblings,
    /// `file:` / `link:` targets) into `<target>/.aube-deploy-injected/`.
    /// When set, the source lockfile subset must be skipped — the
    /// rewritten manifest's `file:` pointers don't appear in the source
    /// lockfile, so a frozen install would immediately read as drifted.
    pub(super) bundled_local_refs: bool,
}

/// Copy files into `target` (either pack's publish-selection or the
/// whole source tree, depending on `deploy_all_files`), bundle any
/// local-ref deps the deployed package reaches into
/// `<target>/.aube-deploy-injected/<id>/`, and rewrite each manifest's
/// `workspace:` / `file:` / `link:` specs so install resolves them to
/// the bundled copies. Returns enough state for the caller to drive
/// install.
pub(super) fn stage_one(
    source_pkg_dir: &Path,
    target: &Path,
    ws_index: &BTreeMap<String, (PathBuf, Option<String>)>,
    catalogs: &CatalogMap,
    args: &DeployArgs,
    deploy_all_files: bool,
) -> miette::Result<StagedDeploy> {
    ensure_target_writable(target)?;
    std::fs::create_dir_all(target)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create {}", target.display()))?;

    // Deploy reuses pack's file selection (the same set of files
    // publish would ship) but, unlike pack, has no use for a real
    // tarball or a `version` field — the deployed artifact isn't going
    // to a registry. Loading the manifest directly + calling
    // `collect_package_files` keeps the file selection identical while
    // letting workspace-internal packages without a `version` deploy.
    // Falls back to a placeholder version string purely for the
    // "deployed X@Y to Z" success log.
    let manifest = crate::commands::load_manifest(&source_pkg_dir.join("package.json"))?;
    let name = manifest
        .name
        .clone()
        .ok_or_else(|| miette!("deploy: package.json has no `name` field"))?;
    let version = manifest
        .version
        .clone()
        .unwrap_or_else(|| "0.0.0".to_string());
    let files = if deploy_all_files {
        collect_all_files(source_pkg_dir, target)?
    } else {
        collect_package_files(source_pkg_dir, &manifest)?
    };

    for (src, rel) in &files {
        let dst = target.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::copy(src, &dst)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to copy {} -> {}", src.display(), dst.display()))?;
    }

    // Plan + materialize bundled local refs, then rewrite each manifest
    // (top-level + every bundled sibling) to point at the staged
    // copies. We strip excluded dep fields *before* install runs:
    // install's resolver walks every dep type in the manifest up front
    // and only the linker applies `--prod` / `--no-optional` filtering,
    // so leaving e.g. a devDependency with an unpublished `workspace:`
    // ref in the manifest would make `--prod` deploys fail resolution
    // on a package that would never have been installed.
    let plan = plan_injections(source_pkg_dir, target, ws_index, args)?;
    materialize_injections(&plan, ws_index, deploy_all_files)?;
    let deployed_canonical = super::canonicalize(source_pkg_dir);
    let root = DeployRoot {
        deployed_canonical: &deployed_canonical,
        target_root: target,
    };
    rewrite_local_refs(
        &target.join("package.json"),
        source_pkg_dir,
        target,
        ws_index,
        catalogs,
        &plan,
        StripFields::for_args(args),
        root,
    )?;
    let bundled_strip = StripFields::for_bundled_sibling(args);
    for inj in plan.values() {
        // Tarballs ship as opaque archives — there's no extracted
        // `package.json` under their `target_dir` to rewrite, and no
        // way to recurse into one anyway since the sibling pipeline
        // doesn't unpack archives.
        if inj.is_tarball {
            continue;
        }
        rewrite_local_refs(
            &inj.target_dir.join("package.json"),
            &inj.source_dir,
            &inj.target_dir,
            ws_index,
            catalogs,
            &plan,
            bundled_strip,
            root,
        )?;
    }

    Ok(StagedDeploy {
        name,
        version,
        target: target.to_path_buf(),
        bundled_local_refs: !plan.is_empty(),
    })
}

/// Walk `source` recursively and collect every file path. Skips only
/// the filesystem cruft that could never be part of a package payload
/// (`node_modules/`, `.git/`) and the `target` directory itself when
/// it sits inside `source`. Unlike pack's selection, this path keeps
/// dot-files, test fixtures, and anything the `files` field /
/// `.npmignore` would have filtered — which is the whole point of
/// `deployAllFiles=true`.
pub(super) fn collect_all_files(
    source: &Path,
    target: &Path,
) -> miette::Result<Vec<(PathBuf, String)>> {
    // Canonicalize both sides so the "is this entry the target dir?"
    // check survives `./foo` vs absolute-path spellings. `target`
    // always exists here (ensure_target_writable + create_dir_all
    // already ran), so canonicalize is not expected to fail; fall
    // back to the raw path rather than aborting the deploy.
    let target_canon = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let mut out = Vec::new();
    let mut stack = vec![source.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let iter = std::fs::read_dir(&dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("deploy: read_dir({}) failed", dir.display()))?;
        for entry in iter {
            let entry = entry
                .into_diagnostic()
                .wrap_err_with(|| format!("deploy: failed to read entry in {}", dir.display()))?;
            let name = entry.file_name();
            if matches!(name.to_string_lossy().as_ref(), "node_modules" | ".git") {
                continue;
            }
            let path = entry.path();
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if canon == target_canon {
                continue;
            }
            // `file_type()` is `lstat`, so a symlink-to-file answers
            // `false` to both `is_file()` and `is_dir()`. Follow one
            // level via `metadata()` (which is `stat`) so symlinked
            // files are copied verbatim — packages that ship linked
            // executables or assets would otherwise lose content
            // under `deployAllFiles=true`. Directory symlinks stay
            // excluded: recursing through them risks cycles
            // (e.g. `src/self -> src/`) and pulls in trees outside
            // the package, which is strictly worse than the pack
            // default. `std::fs::copy` follows links, so the
            // destination gets the target's bytes, not another
            // symlink — matches what a user typing `cp -L` expects.
            let ft = entry
                .file_type()
                .into_diagnostic()
                .wrap_err_with(|| format!("deploy: failed to stat {}", path.display()))?;
            let (is_dir, is_file) = if ft.is_symlink() {
                match std::fs::metadata(&path) {
                    Ok(md) => (md.is_dir(), md.is_file()),
                    // Broken link (dangling target). Skip rather
                    // than error — the source package owns it and a
                    // broken link is almost certainly not part of
                    // the intended payload.
                    Err(_) => (false, false),
                }
            } else {
                (ft.is_dir(), ft.is_file())
            };
            if is_dir && !ft.is_symlink() {
                stack.push(path);
            } else if is_file && let Ok(rel) = path.strip_prefix(source) {
                out.push((path.clone(), rel.to_string_lossy().replace('\\', "/")));
            }
        }
    }
    Ok(out)
}

/// Error if the target already holds files. An empty existing directory
/// is fine — useful when CI pre-creates the mount point.
pub(super) fn ensure_target_writable(target: &Path) -> miette::Result<()> {
    match std::fs::read_dir(target) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(miette!(
                    "{}: target directory {} is not empty",
                    aube_util::cmd("deploy"),
                    target.display()
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(miette!(
            "{}: failed to inspect {}: {e}",
            aube_util::cmd("deploy"),
            target.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_target_writable_empty_dir_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_target_writable(tmp.path()).unwrap();
    }

    #[test]
    fn ensure_target_writable_missing_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_target_writable(&tmp.path().join("nope")).unwrap();
    }

    #[test]
    fn ensure_target_writable_nonempty_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("stuff"), "hi").unwrap();
        assert!(ensure_target_writable(tmp.path()).is_err());
    }
}
