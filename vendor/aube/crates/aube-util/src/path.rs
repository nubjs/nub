//! Cross-crate path normalization helpers.

use std::path::{Component, Path, PathBuf};

/// Collapse `.` and resolvable `..` components without touching the
/// filesystem.
///
/// Unlike `canonicalize`, this does not require the path to exist and
/// does not follow symlinks. Leading `..` components that cannot be
/// collapsed are preserved.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                let prev_is_normal = out
                    .components()
                    .next_back()
                    .is_some_and(|c| matches!(c, Component::Normal(_)));
                if prev_is_normal {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A local dependency path in the form pnpm records it: forward slashes,
/// lexically normalized (`./dep`, `unused/../dep` -> `dep`), and `.` for the
/// project itself. Two spellings of one path give the same string.
pub fn normalize_dep_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    // `PathBuf` joins with `\` on Windows, so the separators are fixed after
    // normalizing as well as before.
    let path = normalize_lexical(Path::new(&path))
        .to_string_lossy()
        .replace('\\', "/");
    if path.is_empty() { ".".to_string() } else { path }
}

/// Strip a Windows `\\?\` verbatim drive prefix from `path` so callers
/// that interpolate it into a path-concatenating template (`%~dp0\{rel}`
/// in the bin-shim generator, `starts_with` ownership checks in
/// `scan_packages` / `remove_package`, `CreateDirectoryW` calls in the
/// linker) all see the plain `C:\…` form.
///
/// `\\?\UNC\server\share\…` identifies a real network share and has no
/// non-verbatim equivalent — callers that strip it would produce an
/// unresolvable drive-rooted path, so leave UNC verbatim prefixes
/// intact.
///
/// No-op on non-Windows.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalization_collapses_internal_parent_dirs() {
        assert_eq!(
            normalize_lexical(Path::new("packages/app/../../vendor")),
            PathBuf::from("vendor")
        );
    }

    #[test]
    fn lexical_normalization_preserves_leading_parent_dirs() {
        assert_eq!(
            normalize_lexical(Path::new("../vendor")),
            PathBuf::from("../vendor")
        );
    }

    #[test]
    fn lexical_normalization_ignores_current_dirs() {
        assert_eq!(
            normalize_lexical(Path::new("./vendor/./pkg")),
            PathBuf::from("vendor/pkg")
        );
    }

    #[cfg(windows)]
    #[test]
    fn strips_verbatim_drive_prefix() {
        let p = Path::new(r"\\?\C:\Users\foo");
        assert_eq!(strip_verbatim_prefix(p), PathBuf::from(r"C:\Users\foo"));
    }

    #[cfg(windows)]
    #[test]
    fn preserves_verbatim_unc_share() {
        let p = Path::new(r"\\?\UNC\server\share\foo");
        assert_eq!(
            strip_verbatim_prefix(p),
            PathBuf::from(r"\\?\UNC\server\share\foo")
        );
    }

    #[cfg(windows)]
    #[test]
    fn leaves_plain_drive_path_unchanged() {
        let p = Path::new(r"C:\Users\foo");
        assert_eq!(strip_verbatim_prefix(p), PathBuf::from(r"C:\Users\foo"));
    }

    #[cfg(not(windows))]
    #[test]
    fn is_noop_on_unix() {
        let p = Path::new("/home/foo");
        assert_eq!(strip_verbatim_prefix(p), PathBuf::from("/home/foo"));
    }
}
