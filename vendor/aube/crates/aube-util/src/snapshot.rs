//! Cross-platform tree snapshot primitives.
//!
//! Portable per-file reflink walker. On CoW filesystems (APFS,
//! btrfs, XFS-with-reflinks, ReFS) produces an O(extent-tree)
//! snapshot via the `reflink-copy` crate; falls through to
//! `std::fs::copy` per file otherwise. `<ENV_PREFIX>_DISABLE_SNAPSHOTS=1`
//! (default profile: `AUBE_DISABLE_SNAPSHOTS=1`) forces the
//! buffered-copy path for byte-identity diffs.

use std::fs;
use std::io;
use std::path::Path;

/// Outcome of a tree snapshot. `Reflinked` means at least one file in
/// the tree took the CoW path; `Copied` means every file fell back to
/// a buffered copy. Useful for tracing-level diagnostics; functionally
/// the result is identical bytes either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotOutcome {
    Reflinked,
    Copied,
}

/// Recursively copy `src` into `dst`, preferring per-file reflinks
/// when the underlying filesystem supports them. `dst` must not exist;
/// the caller owns conflict resolution (rename, overwrite, swap).
///
/// Symlinks are recreated as symlinks pointing at their original
/// target (relative or absolute, copied verbatim). Files use
/// `reflink_or_copy`; directories are walked depth-first.
///
/// `<ENV_PREFIX>_DISABLE_SNAPSHOTS=1` (default profile:
/// `AUBE_DISABLE_SNAPSHOTS=1`) forces every file through `fs::copy`
/// even when reflinks are available, for use as a regression
/// killswitch.
pub fn clone_tree(src: &Path, dst: &Path) -> io::Result<SnapshotOutcome> {
    if dst.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("snapshot destination already exists: {}", dst.display()),
        ));
    }
    let mut state = WalkState::default();
    walk(src, dst, &mut state)?;
    Ok(if state.reflinked > 0 {
        SnapshotOutcome::Reflinked
    } else {
        SnapshotOutcome::Copied
    })
}

#[derive(Default)]
struct WalkState {
    reflinked: u64,
    copied: u64,
}

fn snapshots_disabled() -> bool {
    crate::env::embedder_env("DISABLE_SNAPSHOTS").is_some()
}

fn walk(src: &Path, dst: &Path, state: &mut WalkState) -> io::Result<()> {
    let metadata = fs::symlink_metadata(src)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(src)?;
        symlink(&target, dst)?;
        return Ok(());
    }
    if file_type.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            walk(&entry.path(), &dst.join(entry.file_name()), state)?;
        }
        return Ok(());
    }
    if snapshots_disabled() {
        fs::copy(src, dst)?;
        state.copied += 1;
        return Ok(());
    }
    match reflink_copy::reflink_or_copy(src, dst) {
        Ok(Some(_)) => state.copied += 1,
        Ok(None) => state.reflinked += 1,
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows symlink creation: requires `SeCreateSymbolicLinkPrivilege`
/// (Developer Mode, elevation, or explicit policy grant). Stock dev
/// boxes without Developer Mode will see `ERROR_PRIVILEGE_NOT_HELD`.
/// `aube-linker` sidesteps this by using NTFS junctions for directory
/// links; `clone_tree` callers that point at `node_modules/.aube/`
/// trees on Windows should expect to need junction-aware logic
/// instead. Wire that path before adopting `clone_tree` for the
/// branch-swap snapshot use case (Phase 5 P5-W4).
#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    // The stored `target` may be relative (npm package layouts almost
    // always are: `../react/index.js`). Resolve against `link`'s
    // parent — the directory the symlink will live in — to decide
    // dir-vs-file, since that is the FS-level base from which the
    // OS will resolve the link at read time. Falls back to
    // `symlink_file` on missing target so dangling links don't fail
    // the snapshot outright.
    let resolved = link
        .parent()
        .map(|p| p.join(target))
        .unwrap_or_else(|| target.to_path_buf());
    let is_dir = std::fs::metadata(&resolved)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn clone_tree_reproduces_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(src.join("nested")).unwrap();
        let mut a = fs::File::create(src.join("a.txt")).unwrap();
        a.write_all(b"alpha").unwrap();
        let mut b = fs::File::create(src.join("nested").join("b.txt")).unwrap();
        b.write_all(b"beta").unwrap();
        let outcome = clone_tree(&src, &dst).unwrap();
        assert!(matches!(
            outcome,
            SnapshotOutcome::Reflinked | SnapshotOutcome::Copied
        ));
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(dst.join("nested").join("b.txt")).unwrap(), b"beta");
    }

    #[test]
    fn clone_tree_refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let err = clone_tree(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn clone_tree_handles_empty_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        let outcome = clone_tree(&src, &dst).unwrap();
        // Empty source: no files reflinked or copied; treated as
        // `Copied` (the default arm) which is fine — outcome is a
        // diagnostic, not a contract.
        assert_eq!(outcome, SnapshotOutcome::Copied);
        assert!(dst.exists());
    }
}
