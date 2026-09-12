use crate::Error;
use std::path::Path;

/// Sweep orphan `.tmp-<pid>-*` directories in the virtual store.
///
/// Linker materializes each package into `.tmp-<pid>-<id>/`
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
        // Match our exact prefix. Format: `.tmp-<pid>-<id>`
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

/// Whether a Windows filesystem error is the transient
/// somebody-else-holds-a-handle kind that backing off will clear:
/// `ERROR_SHARING_VIOLATION` (32) or `ERROR_ACCESS_DENIED` (5), raised
/// whenever another process has the file open — Defender mid-scan, the
/// search indexer, a dev server, a `--watch` task, an editor.
///
/// Test the RAW code, never `ErrorKind` alone. Rust maps
/// `ERROR_ACCESS_DENIED` to `PermissionDenied` but has NO mapping for
/// `ERROR_SHARING_VIOLATION` — it falls through `decode_error_kind` into
/// `Uncategorized`, which no `matches!` arm can name. An `ErrorKind`-only
/// predicate therefore silently never fires for the single most common
/// transient on Windows, which is how the rename retry in
/// `place_materialized_entry` came to be dead code for os 32 (#552).
#[cfg(windows)]
pub(crate) fn is_transient_fs_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(5 | 32))
}

/// Full ladder for an operation whose failure is FATAL to the install:
/// 10 attempts, 50ms doubling to a 2s cap, ~10s worst case. Long enough to
/// outlast an AV scan, short enough that a genuinely stuck file fails the
/// install rather than hanging it.
pub(crate) fn with_transient_retry<T>(
    op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    with_transient_retry_bounded(10, op)
}

/// Short ladder for a BEST-EFFORT operation the caller proceeds past either
/// way: 4 attempts, ~750ms worst case.
///
/// The distinction is about where the call sits, not how much we want it to
/// succeed. Best-effort removals run once per entry inside sweep loops, so a
/// ~10s ladder does not cost 10s — it costs 10s PER LOCKED ENTRY, and a branch
/// switch with a dev server running turns that into a multi-minute stall on
/// what is supposed to be a cleanup pass. The transient being waited out (an
/// AV scan window) clears well inside a second, so the short ladder catches
/// essentially all of what the long one would.
pub(crate) fn with_transient_retry_bounded<T>(
    #[cfg_attr(not(windows), allow(unused_variables))] attempts: u32,
    mut op: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    // Unix is a straight passthrough — the failure mode does not exist there
    // (an open file can be replaced or unlinked freely), so behavior off
    // Windows is byte-for-byte unchanged.
    #[cfg(not(windows))]
    {
        op()
    }
    #[cfg(windows)]
    {
        for attempt in 0..attempts.max(1) {
            match op() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !is_transient_fs_error(&e) || attempt == attempts.max(1) - 1 {
                        return Err(e);
                    }
                    let delay = (50u64 << attempt.min(6)).min(2000);
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
            }
        }
        unreachable!("the loop returns on the final attempt")
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
pub fn remove_dir_all_with_retry(path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::remove_dir_all(path)
    }
    #[cfg(windows)]
    {
        // Already gone — including a concurrent remover winning the race —
        // is success, not failure.
        match with_transient_retry(|| std::fs::remove_dir_all(path)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
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
/// symlink, junction, or populated directory. Errors are intentionally
/// ignored because this is a "clear the slot" operation — the caller is
/// about to place something else here and any residual entry that
/// survives will surface as a downstream error.
///
/// Goes through the Windows retry (a plain passthrough on Unix): the
/// slots this clears live inside `node_modules`, where a dev server,
/// watcher, or AV scanner holding a handle turns the delete into a
/// transient os-5/32 failure. Swallowed here, that failure re-emerges
/// as an `ERROR_ALREADY_EXISTS` from whatever the caller places next.
pub(crate) fn try_remove_entry(path: &Path) {
    // A partial wipe is not just a re-emerging `ERROR_ALREADY_EXISTS` (above):
    // the refill that follows only writes paths present in the NEW index, so
    // anything the old version had and the new one does not survives and stays
    // resolvable, silently blending two package versions.
    //
    // SHORT ladder, not the full one: this runs once per entry inside three
    // sweep loops, so the ~10s ladder would not cost 10s — it would cost 10s
    // PER LOCKED ENTRY, turning a cleanup pass into a multi-minute stall when
    // a dev server holds several. The transient here is an AV scan window,
    // which clears well inside a second.
    let _ = with_transient_retry_bounded(4, || std::fs::remove_dir_all(path));
    let _ = std::fs::remove_file(path);
}

/// Recursive directory creation, retried through Windows' transient
/// sharing errors. Every materialize pass calls this before creating a
/// symlink / junction, so the retry and the path-carrying error
/// conversion live in exactly one place.
pub fn mkdirp(dir: &Path) -> Result<(), Error> {
    // Calls `std::fs::create_dir_all` rather than `xx::file::mkdirp` so the
    // raw OS error survives: `xx` wraps it in its own error type, and the
    // retry predicate below matches on `raw_os_error()`, so a wrapped error
    // would silently never be retriable. `xx::file::mkdirp` is an `exists()`
    // check plus this same call, and `create_dir_all` is already a no-op on an
    // existing directory, so dropping the check changes nothing.
    //
    // Retried because creating a directory whose slot is in Windows'
    // pending-delete state returns ACCESS_DENIED — a state reachable precisely
    // because a wipe just ran against a path something else holds open.
    // Failure here is fatal to the install.
    with_transient_retry(|| std::fs::create_dir_all(dir))
        .map_err(|e| Error::Io(dir.to_path_buf(), e))
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

/// Classify a project-local virtual-store entry. A real directory is
/// already materialized and reusable; symlinks and other file types are
/// stale shapes left by a different layout mode.
#[inline]
pub(crate) fn classify_local_entry_state(path: &Path) -> EntryState {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return EntryState::Stale;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return EntryState::Stale;
                }
            }
            if metadata.file_type().is_dir() {
                EntryState::Fresh
            } else {
                EntryState::Stale
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EntryState::Missing,
        Err(_) => EntryState::Stale,
    }
}

/// Reconcile a directory link against its expected target.
///
/// Returns `Ok(true)` when the existing link stores the expected target.
/// Missing, incorrectly-targeted, and non-link entries are removed and return
/// `Ok(false)` so the caller can recreate the link. The target is not probed,
/// so a dangling link that stores the expected target is considered current.
pub(crate) fn reconcile_dir_link(link_path: &Path, expected_target: &Path) -> Result<bool, Error> {
    #[cfg(windows)]
    {
        // NTFS junctions store normalized absolute targets, sometimes with a
        // `\\?\` prefix, so compare canonical destinations rather than the
        // relative target passed to `create_dir_link`.
        let expected_abs = if expected_target.is_absolute() {
            expected_target.to_path_buf()
        } else {
            link_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(expected_target)
        };
        // The link destination is mutable during reconciliation and shared
        // installs can repair it concurrently, so it must never be cached.
        if let Ok(link_canon) = link_path.canonicalize()
            && let Ok(exp_canon) = expected_abs.canonicalize()
            && link_canon == exp_canon
        {
            return Ok(true);
        }
        if link_path.symlink_metadata().is_err() {
            return Ok(false);
        }
        match std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path)) {
            Ok(()) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(link_path.to_path_buf(), e)),
        }
    }
    #[cfg(not(windows))]
    {
        match std::fs::read_link(link_path) {
            Ok(existing) if existing == expected_target => Ok(true),
            Ok(_) => {
                let _ = std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => {
                let _ =
                    std::fs::remove_dir_all(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
        }
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
