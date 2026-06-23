use aube_lockfile::dep_path_filename::dep_path_to_filename;

pub(super) fn invalidate_changed_aube_entries(
    aube_dir: &std::path::Path,
    dep_paths: &[String],
    virtual_store_dir_max_length: usize,
) -> usize {
    let mut removed = 0usize;
    for dep_path in dep_paths {
        let path = aube_dir.join(dep_path_to_filename(dep_path, virtual_store_dir_max_length));
        let result = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        match result {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_DELTA_INVALIDATE_FAILED,
                "delta: failed to invalidate {}: {e}",
                path.display()
            ),
        }
    }
    removed
}

/// Remove `node_modules/.aube/<encoded_dep_path>` entries that aren't
/// referenced by the current lockfile graph AND whose last-modified
/// time is older than `max_age`. The `.aube/` directory accumulates
/// orphaned entries as dependencies are upgraded or removed; this
/// pass enforces `modulesCacheMaxAge` (default 7 days) so stale
/// packages don't live forever.
///
/// Runs best-effort: I/O errors are logged and swallowed so a partial
/// sweep never fails an install that otherwise succeeded. Returns the
/// number of entries successfully removed so the caller can decide
/// whether to emit a tracing line.
pub(super) fn sweep_orphaned_aube_entries(
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    max_age: std::time::Duration,
) -> usize {
    let entries = match std::fs::read_dir(aube_dir) {
        Ok(e) => e,
        // No `.aube` directory = nothing to sweep (e.g. fresh CI
        // install). Not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::debug!(
                "modulesCacheMaxAge: cannot read {}: {e}; skipping sweep",
                aube_dir.display()
            );
            return 0;
        }
    };

    let in_use: std::collections::HashSet<String> = graph
        .packages
        .keys()
        .map(|dep_path| dep_path_to_filename(dep_path, virtual_store_dir_max_length))
        .collect();

    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Dotfiles (`.patches`, future sidecars) are always preserved.
        if name_str.starts_with('.') {
            continue;
        }
        // `.aube/node_modules/` is the hidden hoist tree populated
        // by `link_hidden_hoist`, not a `dep_path_to_filename`
        // output, so it never appears in `in_use`. Removing it
        // would break Node's parent-walk resolution for packages
        // inside the virtual store. The hoist is fully managed by
        // the linker (it sweeps stale entries on every run when
        // `hoist=false`), so the modulesCacheMaxAge sweep has no
        // business touching it.
        if name_str == "node_modules" {
            continue;
        }
        if in_use.contains(name_str.as_ref()) {
            continue;
        }
        let metadata = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(
                    "modulesCacheMaxAge: cannot stat {}: {e}",
                    entry.path().display()
                );
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(t) => t,
            Err(_) => continue, // platform doesn't expose mtime; keep.
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age < max_age {
            continue;
        }
        let path = entry.path();
        let file_type = metadata.file_type();
        let result = if file_type.is_symlink() {
            std::fs::remove_file(&path)
        } else {
            std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path))
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) => tracing::debug!(
                "modulesCacheMaxAge: failed to remove {}: {e}",
                path.display()
            ),
        }
    }
    removed
}
