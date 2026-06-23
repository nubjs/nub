use crate::Error;
use std::path::Path;

/// Sweep orphan `.tmp-<pid>-*` directories in the virtual store.
///
/// Linker materializes each package into `.tmp-<pid>-<subdir>/`
/// then atomic-renames into `.aube/<subdir>/`. Crash or Ctrl-C
/// between materialize and rename leaves the tmp dir behind.
/// Nothing else cleans these up so they accumulate on every aborted
/// install. Small footprint per entry but a few hundred aborted
/// CI runs pile up gigabytes.
///
/// Called early in link_all so each fresh install reclaims space
/// from prior crashes. Only matches the exact prefix we produce so
/// user files named `.tmp-*` in the virtual store are safe.
pub fn sweep_stale_tmp_dirs(virtual_store: &Path) {
    let Ok(entries) = std::fs::read_dir(virtual_store) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match our exact prefix. Format: `.tmp-<pid>-<subdir>`
        // where pid is numeric.
        if !name.starts_with(".tmp-") {
            continue;
        }
        let rest = &name[".tmp-".len()..];
        let Some((pid_str, _rest)) = rest.split_once('-') else {
            continue;
        };
        if pid_str.chars().any(|c| !c.is_ascii_digit()) {
            continue;
        }
        // Do not touch the dir of our own still-running process.
        // Materialize path creates and removes its tmp dir in the
        // same call and crashes mid-way are the target here, the
        // active pid will not leave ones around that matter.
        if pid_str == std::process::id().to_string() {
            continue;
        }
        let _ = remove_dir_all_with_retry(&entry.path());
    }
}

/// Remove a directory with retry on Windows sharing violations.
///
/// Windows does not let you delete a file while another process holds
/// a handle open. Dev server, vitest watcher, tsc --watch all hold
/// .js / .node files inside node_modules. aube reinstall hits ERROR
/// 32 (SHARING_VIOLATION) or ERROR 5 (ACCESS_DENIED, AV scanner
/// mid-scan) and leaves a half-deleted virtual store. pnpm, npm,
/// rimraf all retry with backoff. Do the same. Unix passthrough.
///
/// Retries 10 times with exponential backoff starting at 50ms. Total
/// worst case around 10 seconds which is tolerable for an install
/// already paying for filesystem work.
pub fn remove_dir_all_with_retry(path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::remove_dir_all(path)
    }
    #[cfg(windows)]
    {
        use std::io::ErrorKind;
        let mut delay_ms = 50u64;
        for attempt in 0..10 {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    // Sharing violation and PermissionDenied both
                    // map to retriable Windows errors. Bail on
                    // attempt 10.
                    let retriable =
                        matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::Other)
                            || e.raw_os_error() == Some(32);
                    if !retriable || attempt == 9 {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    delay_ms = (delay_ms * 2).min(2000);
                }
            }
        }
        // Unreachable, loop always returns by attempt 10.
        Ok(())
    }
}

/// Real workspace importer, not a peer-context bookkeeping entry.
///
/// pnpm v9 lockfiles record the peer-resolution view of each
/// workspace package reached through every nested `node_modules/`
/// traversal. Those virtual importer paths (e.g.
/// `packages/a/node_modules/@scope/b/node_modules/@scope/c`) describe
/// *how* a package looks from a particular context — they are reached
/// via the workspace-to-workspace symlink chain and have no
/// independent `node_modules/` to populate. When the link pipeline
/// treats them as physical importers it queues parallel symlink tasks
/// whose `link_path`s canonicalize to the same inode as a physical
/// importer's task, producing EEXIST races on large monorepos.
pub fn is_physical_importer(importer_path: &str) -> bool {
    importer_path == "." || !importer_path.contains("/node_modules/")
}

/// Wipe `path` when it looks like a linker-managed `.aube/node_modules`
/// tree. If a previously-tampered install (or attacker) replaced the
/// tree with a symlink / junction pointing elsewhere on disk, refuse
/// to recurse into it — modern Rust `remove_dir_all` already declines
/// to follow symlinks, mirroring the invariant at the call site keeps
/// the intent explicit and catches any future regression in the
/// callee.
pub(crate) fn remove_hidden_hoist_tree(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            let _ = std::fs::remove_file(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_dir_all(path);
        }
        Err(_) => {}
    }
}

/// Best-effort unlink of `path` regardless of whether it's a file,
/// symlink, junction, or directory. Errors are intentionally ignored
/// because this is a "clear the slot" operation — the caller is about
/// to place something else here and any residual entry that survives
/// will surface as a downstream error.
pub(crate) fn try_remove_entry(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    let _ = std::fs::remove_file(path);
}

/// `xx::file::mkdirp` wrapped with the linker's `Error::Xx` conversion.
/// Every materialize pass calls this before creating a symlink /
/// junction, so the lossy `.to_string()` wrap lives in exactly one
/// place.
pub fn mkdirp(dir: &Path) -> Result<(), Error> {
    xx::file::mkdirp(dir).map_err(|e| Error::Xx(e.to_string()))
}

/// Classification of a `.aube/<dep_path>` symlink relative to the
/// current hashed global entry the linker wants to point at.
#[derive(Copy, Clone)]
pub(crate) enum EntryState {
    /// The symlink already points at `expected` and the target exists —
    /// nothing to do. Caller can bump a `packages_cached` counter and
    /// move on.
    Fresh,
    /// No entry at `link_path` yet. Caller needs to materialize and
    /// create the symlink, but there's nothing to unlink first.
    Missing,
    /// An entry exists but is stale (different target, dangling link,
    /// or an `Err` read that isn't NotFound). Caller must unlink
    /// before resymlinking.
    Stale,
}

/// Sweep stale entries out of a `node_modules/` directory while
/// preserving everything in `preserve` (bare names like `lodash` and
/// scope prefixes like `@babel`), dotfiles, and — if set — the
/// virtual-store leaf (`aube_dir_leaf`) sitting right under `nm`
/// with a non-dotfile name (the `virtualStoreDir=node_modules/vstore`
/// case). For `@scope` entries we recurse one level and drop any
/// `@scope/<pkg>` whose full `@scope/pkg` name is not in `preserve`;
/// an empty scope directory left behind by the sweep is removed so
/// the next install doesn't trip over a phantom scope tombstone.
pub(crate) fn sweep_stale_top_level_entries(
    nm: &Path,
    preserve: &std::collections::HashSet<&str>,
    aube_dir_leaf: Option<&std::ffi::OsStr>,
) {
    let scope_prefixes: std::collections::HashSet<&str> = preserve
        .iter()
        .filter_map(|n| n.split_once('/').map(|(scope, _)| scope))
        .collect();
    let Ok(entries) = std::fs::read_dir(nm) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        if aube_dir_leaf == Some(name.as_os_str()) {
            continue;
        }
        if preserve.contains(name_str.as_ref()) {
            continue;
        }
        if scope_prefixes.contains(name_str.as_ref()) {
            let scope_dir = entry.path();
            if let Ok(inner) = std::fs::read_dir(&scope_dir) {
                for inner_entry in inner.flatten() {
                    let inner_name = inner_entry.file_name();
                    let full = format!("{}/{}", name_str, inner_name.to_string_lossy());
                    if !preserve.contains(full.as_str()) {
                        try_remove_entry(&inner_entry.path());
                    }
                }
            }
            // If the scope dir is now empty (every member was stale),
            // drop the tombstone directory too.
            if std::fs::read_dir(&scope_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&scope_dir);
            }
            continue;
        }
        try_remove_entry(&entry.path());
    }
}

/// Sweep broken entries from a shared hidden-hoist directory without
/// deleting live links owned by other projects. The GVS hidden hoist is
/// global, so "not in this project's graph" is not stale enough: another
/// project may still need that link. Only entries whose target no longer
/// exists (or non-link junk) are reclaimed here; current-project names are
/// still target-reconciled by `reconcile_top_level_link` below.
pub(crate) fn sweep_dead_hidden_hoist_entries(hidden: &Path) {
    let Ok(entries) = std::fs::read_dir(hidden) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if name_str.starts_with('@') {
            match std::fs::symlink_metadata(&path) {
                Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {
                    sweep_dead_hidden_hoist_scope(&path);
                    if std::fs::read_dir(&path)
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(&path);
                    }
                }
                Ok(_) => sweep_dead_hidden_hoist_entry(&path),
                Err(_) => {}
            }
            continue;
        }
        sweep_dead_hidden_hoist_entry(&path);
    }
}

fn sweep_dead_hidden_hoist_scope(scope_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(scope_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        sweep_dead_hidden_hoist_entry(&entry.path());
    }
}

fn sweep_dead_hidden_hoist_entry(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() && path.exists() => {}
        Ok(md) if md.file_type().is_symlink() => {
            try_remove_entry(path);
        }
        Ok(md) if md.is_dir() => {
            try_remove_entry(path);
        }
        Ok(_) => {
            try_remove_entry(path);
        }
        Err(_) => {}
    }
}

/// Classify `link_path` against `expected` without the double-check
/// (`read_link` then `exists`) that ate ~1.4k ENOENT syscalls per
/// install on the medium fixture. Fresh means "points at expected
/// AND the target still exists"; everything else is Missing or
/// Stale. The fast path returns without touching disk a second time.
#[inline]
pub(crate) fn classify_entry_state(link_path: &Path, expected: &Path) -> EntryState {
    match std::fs::read_link(link_path) {
        Ok(existing) if existing == expected => {
            if link_path.exists() {
                EntryState::Fresh
            } else {
                EntryState::Stale
            }
        }
        Ok(_) => EntryState::Stale,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => EntryState::Missing,
        // Some other error (permission, etc.): treat as Stale and
        // let the removal/recreate path try its best-effort cleanup
        // + surface the real error on symlink creation if unlucky.
        Err(_) => EntryState::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::is_physical_importer;

    #[test]
    fn root_is_physical() {
        assert!(is_physical_importer("."));
    }

    #[test]
    fn workspace_paths_are_physical() {
        assert!(is_physical_importer("packages/dev/core"));
        assert!(is_physical_importer("apps/web"));
        assert!(is_physical_importer("libs/@scope/name"));
    }

    #[test]
    fn nested_peer_context_paths_are_virtual() {
        // pnpm v9 emits these for every peer-resolution view reachable
        // through the workspace symlink chain. They describe the graph,
        // they are not directories to populate.
        assert!(!is_physical_importer(
            "packages/dev/addons/node_modules/@dev/core"
        ));
        assert!(!is_physical_importer(
            "packages/a/node_modules/@s/b/node_modules/@s/c"
        ));
    }
}
