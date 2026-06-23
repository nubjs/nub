use miette::{Context, IntoDiagnostic};

/// Format the resolved `virtualStoreDir` as a display-ready prefix for
/// `aube list --long` and `aube why --long`, ending with a path
/// separator so callers can concatenate an encoded `dep_path`
/// filename. When `aube_dir` is a subdirectory of `ref_dir` the result
/// is relative (`./node_modules/.aube/`), matching the historical
/// output. For overrides that sit above or outside `ref_dir` (custom
/// `virtualStoreDir` like `~/.my-store/project` or `.vstore-out`) the
/// absolute path is returned so users can still find where packages
/// actually live — `../../../...` would be technically correct but
/// hard to paste into a shell.
pub(crate) fn format_virtual_store_display_prefix(
    aube_dir: &std::path::Path,
    ref_dir: &std::path::Path,
) -> String {
    if let Some(rel) = pathdiff::diff_paths(aube_dir, ref_dir)
        && !rel.as_os_str().is_empty()
        && !rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return format!("./{}/", rel.display());
    }
    format!("{}/", aube_dir.display())
}

/// Remove an existing file/dir/symlink at the given path, if present.
///
/// Windows quirk: directory junctions and directory symlinks report as
/// symlinks via `symlink_metadata`, but `std::fs::remove_file` returns
/// `Access is denied (os error 5)` for them — the Win32 `DeleteFile`
/// syscall only works on file-shaped entries. The link entry has to be
/// torn down with `RemoveDirectory` (= `std::fs::remove_dir`), which is
/// non-recursive and so leaves the junction target untouched. Falling
/// back on `remove_file` failure keeps every other platform on the
/// usual single-syscall path.
pub(crate) fn remove_existing(path: &std::path::Path) -> miette::Result<()> {
    let Ok(md) = path.symlink_metadata() else {
        return Ok(());
    };
    let file_type = md.file_type();
    if file_type.is_dir() {
        return std::fs::remove_dir_all(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display()));
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(_) if file_type.is_symlink() => std::fs::remove_dir(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display())),
        Err(e) => Err(e)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display())),
    }
}

/// Create a directory link (symlink on Unix, NTFS junction on
/// Windows). Thin re-export of [`aube_linker::create_dir_link`] —
/// the linker owns the platform-specific implementation so every
/// directory-link call site in the workspace behaves identically,
/// including Windows' "junctions not symlinks" choice that keeps
/// installs working without Developer Mode.
pub(crate) fn symlink_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    aube_linker::create_dir_link(src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_a_symlink_pointing_at_a_populated_directory_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let canary = target.join("keep.txt");
        std::fs::write(&canary, b"keep me").unwrap();

        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        aube_linker::create_dir_link(&target, &link).unwrap();

        remove_existing(&link).unwrap();
        assert!(!link.exists());
        assert!(
            canary.exists(),
            "remove_existing must not recurse into the symlink's target"
        );
    }

    #[test]
    fn missing_path_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        remove_existing(&dir.path().join("does-not-exist")).unwrap();
    }
}
