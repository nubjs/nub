use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

pub(crate) const SHIM_DIR_ENV: &str = "AUBE_SHIM_DIR";

pub(crate) const DISPATCH_ARG: &str = "__aube-shim";

pub(crate) const TOOL_SHIMS: &[&str] = &["node", "npm", "npx", "pnpm", "pnpx", "yarn", "yarnpkg"];

pub(crate) fn shim_file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

pub(crate) fn shim_dir() -> Option<PathBuf> {
    let ns = aube_util::embedder().data_namespace;
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join(ns).join("shims"));
    }
    let data_home = match aube_util::env::xdg_data_home() {
        Some(xdg) => xdg,
        None => aube_util::env::home_dir()?.join(".local/share"),
    };
    Some(data_home.join(ns).join("shims"))
}

pub(crate) fn activated_shim_dir() -> Option<PathBuf> {
    std::env::var_os(SHIM_DIR_ENV)
        .filter(|value| !value.is_empty())
        .and_then(single_path_entry)
}

fn single_path_entry(value: OsString) -> Option<PathBuf> {
    let path = PathBuf::from(&value);
    let mut entries = std::env::split_paths(&value);
    if entries.next().as_deref() != Some(path.as_path()) || entries.next().is_some() {
        return None;
    }
    Some(path)
}

pub(crate) fn sanitize_process_path() {
    let Some(path) = path_without_shim_dir(std::env::var_os("PATH")) else {
        return;
    };
    // SAFETY: called from the synchronous CLI startup path before the
    // tokio runtime spawns worker threads.
    unsafe {
        std::env::set_var("PATH", path);
    }
}

fn path_without_shim_dir(path: Option<OsString>) -> Option<OsString> {
    let shim_dir = activated_shim_dir()?;
    strip_path_entry(path?, &shim_dir)
}

fn strip_path_entry(path: OsString, entry_to_remove: &Path) -> Option<OsString> {
    let original: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let entry_to_remove = comparable_path(entry_to_remove);
    let kept: Vec<PathBuf> = original
        .iter()
        .filter(|entry| comparable_path(entry) != entry_to_remove)
        .cloned()
        .collect();
    if kept.len() == original.len() {
        return None;
    }
    if kept.is_empty() {
        return Some(OsString::new());
    }
    std::env::join_paths(kept).ok()
}

fn comparable_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    std::fs::canonicalize(path)
        .map(|path| normalize_path_lexically(&path))
        .unwrap_or_else(|_| normalize_path_lexically(path))
}

fn normalize_path_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn stem_of_argv0(argv0: &OsStr) -> String {
    std::path::Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("aube")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_matching_path_entry() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path = OsString::from(format!("/a{sep}/b{sep}/c"));
        assert_eq!(
            strip_path_entry(path, Path::new("/b")),
            Some(OsString::from(format!("/a{sep}/c")))
        );
    }

    #[test]
    fn strips_matching_path_entry_with_trailing_separator() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path = OsString::from(format!("/a{sep}/b/{sep}/c"));
        assert_eq!(
            strip_path_entry(path, Path::new("/b")),
            Some(OsString::from(format!("/a{sep}/c")))
        );
    }

    #[test]
    fn strips_only_path_entry_to_empty_path() {
        assert_eq!(
            strip_path_entry(OsString::from("/b"), Path::new("/b")),
            Some(OsString::new())
        );
    }

    #[test]
    fn shim_file_names_use_exe_suffix_on_windows() {
        let node = shim_file_name("node");
        if cfg!(windows) {
            assert_eq!(node, "node.exe");
        } else {
            assert_eq!(node, "node");
        }
    }

    #[test]
    fn rejects_multiple_entries_as_a_shim_dir() {
        let value = std::env::join_paths([Path::new("first"), Path::new("second")])
            .expect("test paths should join");
        assert_eq!(single_path_entry(value), None);
    }
}
