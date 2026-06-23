//! Whole-directory `clonefile(2)` materialization — the macOS-coupled
//! fast path that replaces the per-file reflink loop with a single
//! in-kernel recursive APFS clone.
//!
//! ## Why
//!
//! The default materializer fills a package directory by reflinking
//! every CAS file one at a time (`materialize_into`'s per-file loop).
//! On a package with dozens-to-thousands of files that's dozens-to-
//! thousands of `clonefile` syscalls plus the `create_dir_all` /
//! `chmod` traffic around them. APFS can clone an *entire directory
//! tree* — files, subdirs, symlinks, mode bits — in **one**
//! `clonefile(2)` call, walking the tree inside the kernel. A
//! microbench on a 60-file package measured ~12x (533µs vs 6396µs per
//! package) for the whole-dir clone over the per-file reflink loop.
//!
//! The catch is that `clonefile(2)` writes the *whole* destination in
//! one shot, so it needs a real extracted directory as its source —
//! the flat CAS (one shard per content hash) isn't one. That's what
//! the store's `trees/` tier provides: each package materialized once
//! into `store/v1/trees/<subdir>/` (itself reflinked from the CAS, so
//! cheap and inode-sharing), then cloned from there on every
//! subsequent materialization.
//!
//! ## Why raw `clonefile(2)` and not `copyfile(3)` / `COPYFILE_CLONE_RECURSIVE`
//!
//! `copyfile`'s recursive-clone mode walks the directory tree in
//! *userspace* and issues a clone per entry — i.e. exactly as slow as
//! the per-file loop this replaces, just behind a libc call. Only the
//! bare `clonefile(2)` syscall (with a directory source) does the
//! recursive clone *in-kernel*, which is where the win comes from.
//!
//! ## Gate
//!
//! All of: macOS, the destination volume is APFS, the tree source and
//! the destination live on the same volume (`clonefile` is
//! single-volume), and the tree exists. Anything else falls through to
//! the unchanged per-file path. On Linux/Windows this module compiles
//! to no-ops because btrfs/xfs `FICLONE` is a per-file ioctl with no
//! recursive-dir form, so there is no equivalent win to gate on there.

#[cfg(target_os = "macos")]
use std::path::Path;

/// Raw `clonefile(2)`: recursively CoW-clones `src` (a file or, for our
/// use, a directory tree) to `dst`, which must not already exist.
/// Preserves mode bits and clones symlinks as symlinks. Same-volume
/// only — cross-volume returns `EXDEV`, which the gate prevents us from
/// ever hitting. `flags = 0` (no `CLONE_NOFOLLOW`/`CLONE_NOOWNERCOPY`):
/// we want symlinks followed-as-cloned and owner preserved, matching a
/// plain per-file reflink's result.
#[cfg(target_os = "macos")]
pub(crate) fn clonefile_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src_c = CString::new(src.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "clonefile src has nul")
    })?;
    let dst_c = CString::new(dst.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "clonefile dst has nul")
    })?;
    // SAFETY: both CStrings outlive the call; clonefile takes two
    // NUL-terminated paths and an int flags word. flags=0 is the
    // documented default (follow symlinks, copy owner).
    let r = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whether a whole-dir `clonefile(2)` from `tree_src` into the package
/// dir under `dst_parent` is safe and worthwhile. The conditions —
/// macOS, both paths on the same APFS volume — are exactly
/// `clonefile`'s own preconditions; we never want to *probe* by
/// attempting a clone and catching the error because a failed clone can
/// leave a partial destination that the per-file fallback would then
/// have to clean up. `dst_parent` is the directory the package dir will
/// be created *inside* (it exists; the package dir itself does not yet).
///
/// Returns `false` on any platform but macOS.
#[cfg(target_os = "macos")]
pub(crate) fn can_clonedir(tree_src: &Path, dst_parent: &Path) -> bool {
    // Same-volume check: clonefile is single-volume. Compare the
    // device id of the tree source against the destination parent.
    // st_dev identifies the mounted volume; APFS volumes in a single
    // container still have distinct st_dev, so this correctly rejects
    // a cross-volume clone even within one APFS container.
    let (Ok(src_meta), Ok(dst_meta)) = (std::fs::metadata(tree_src), std::fs::metadata(dst_parent))
    else {
        return false;
    };
    use std::os::unix::fs::MetadataExt;
    if src_meta.dev() != dst_meta.dev() {
        return false;
    }
    is_apfs(dst_parent)
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
pub(crate) fn can_clonedir(_tree_src: &std::path::Path, _dst_parent: &std::path::Path) -> bool {
    false
}

/// Is `path` on an APFS volume? `clonefile(2)` is supported on APFS
/// (and the deprecated HFS+ does not support directory clones), so we
/// confirm APFS before committing to the whole-dir path. Uses
/// `statfs(2)`'s `f_fstypename`. A non-APFS macOS volume (an external
/// HFS+/exFAT drive, a network mount) returns false and keeps the
/// per-file path.
#[cfg(target_os = "macos")]
fn is_apfs(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: statfs writes into a zeroed struct we own; `c` outlives
    // the call.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::statfs(c.as_ptr(), &mut sfs) };
    if r != 0 {
        return false;
    }
    // f_fstypename is a fixed-size C char array; read up to the first
    // NUL and compare case-insensitively against "apfs".
    let raw = &sfs.f_fstypename;
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    bytes.eq_ignore_ascii_case(b"apfs")
}
